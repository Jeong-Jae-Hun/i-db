package obs

import (
	"context"
	"errors"
	"fmt"
	"net/http"
	"net/http/httptest"
	"net/url"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
)

// 2026-11-15 12:00:00 KST
var fixedNow = time.Date(2026, 11, 15, 3, 0, 0, 0, time.UTC)

type seen struct {
	path   string
	query  url.Values
	header http.Header
}

// fake 는 경로별 응답을 돌려주고 받은 요청을 적어 둔다. 등록 안 된 경로는 404 다.
type fake struct {
	mu     sync.Mutex
	reqs   []seen
	routes map[string]func(url.Values) (int, string)
	srv    *httptest.Server
}

func newFake(t *testing.T, routes map[string]func(url.Values) (int, string)) *fake {
	t.Helper()
	f := &fake{routes: routes}
	f.srv = httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		f.mu.Lock()
		f.reqs = append(f.reqs, seen{path: r.URL.Path, query: r.URL.Query(), header: r.Header.Clone()})
		f.mu.Unlock()
		if r.Method != http.MethodGet {
			http.Error(w, "method", http.StatusMethodNotAllowed)
			return
		}
		h, ok := f.routes[r.URL.Path]
		if !ok {
			http.NotFound(w, r)
			return
		}
		code, body := h(r.URL.Query())
		w.WriteHeader(code)
		fmt.Fprint(w, body)
	}))
	t.Cleanup(f.srv.Close)
	return f
}

func (f *fake) last(t *testing.T) seen {
	t.Helper()
	f.mu.Lock()
	defer f.mu.Unlock()
	if len(f.reqs) == 0 {
		t.Fatal("no request")
	}
	return f.reqs[len(f.reqs)-1]
}

func (f *fake) count() int {
	f.mu.Lock()
	defer f.mu.Unlock()
	return len(f.reqs)
}

func ok(body string) func(url.Values) (int, string) {
	return func(url.Values) (int, string) { return 200, body }
}

func success(data string) func(url.Values) (int, string) {
	return ok(`{"status":"success","data":` + data + `}`)
}

func newLokiAt(t *testing.T, base string, c db.Conn) *loki {
	t.Helper()
	c.URL = base
	b, err := NewLoki(c)
	if err != nil {
		t.Fatal(err)
	}
	l := b.(*loki)
	l.now = func() time.Time { return fixedNow }
	return l
}

func newPromAt(t *testing.T, base string, c db.Conn) *prom {
	t.Helper()
	c.URL = base
	b, err := NewPrometheus(c)
	if err != nil {
		t.Fatal(err)
	}
	p := b.(*prom)
	p.now = func() time.Time { return fixedNow }
	return p
}

func ns(t time.Time) string { return fmt.Sprint(t.UnixNano()) }

func eq[T comparable](t *testing.T, name string, got, want T) {
	t.Helper()
	if got != want {
		t.Errorf("%s = %v, want %v", name, got, want)
	}
}

func rowsText(r *db.Result) string {
	var b strings.Builder
	b.WriteString(strings.Join(r.Cols, "|") + "\n")
	for _, row := range r.Rows {
		b.WriteString(strings.Join(row, "|") + "\n")
	}
	return b.String()
}

// ─── Loki ───

func TestLoki_트리는_최근_1시간_라벨_이름과_값이고_basic_인증을_보낸다(t *testing.T) {
	f := newFake(t, map[string]func(url.Values) (int, string){
		"/loki/api/v1/labels":           success(`["app","env"]`),
		"/loki/api/v1/label/app/values": success(`["api","web/v2"]`),
	})
	l := newLokiAt(t, f.srv.URL, db.Conn{Auth: "basic", User: "mon", Password: "pw"})

	nodes, err := l.Children(context.Background(), "")
	if err != nil {
		t.Fatal(err)
	}
	if len(nodes) != 2 || nodes[0].Path != "app" || !nodes[0].HasChildren || nodes[0].Openable {
		t.Fatalf("labels = %+v", nodes)
	}
	r := f.last(t)
	eq(t, "start", r.query.Get("start"), ns(fixedNow.Add(-time.Hour)))
	eq(t, "end", r.query.Get("end"), ns(fixedNow))
	u, p, _ := (&http.Request{Header: r.header}).BasicAuth()
	eq(t, "basic", u+":"+p, "mon:pw")

	vals, err := l.Children(context.Background(), "app")
	if err != nil {
		t.Fatal(err)
	}
	if len(vals) != 2 || vals[1].Path != "app/web/v2" || !vals[1].Openable || vals[1].HasChildren {
		t.Fatalf("values = %+v", vals)
	}
	if got, _ := l.Children(context.Background(), "app/api"); got != nil {
		t.Errorf("value node children = %+v", got)
	}
}

func TestLoki_로그_질의는_범위_한도_방향을_보내고_최신_먼저_라벨_합집합으로_편다(t *testing.T) {
	t1 := time.Date(2026, 11, 15, 2, 0, 0, 123_000_000, time.UTC) // 11:00:00.123 KST
	t2 := t1.Add(time.Minute)
	t3 := t1.Add(2 * time.Minute)
	f := newFake(t, map[string]func(url.Values) (int, string){
		"/loki/api/v1/query_range": success(fmt.Sprintf(`{"resultType":"streams","result":[
			{"stream":{"app":"api","pod":"a"},"values":[["%s","third"],["%s","first"]]},
			{"stream":{"app":"web","env":"stg"},"values":[["%s","second",{"trace_id":"x"}]]}]}`, ns(t3), ns(t1), ns(t2))),
	})
	l := newLokiAt(t, f.srv.URL, db.Conn{})

	res, err := l.Run(context.Background(), source.Query{Text: `{app=~".+"} |= "x"`, Since: 2 * time.Hour, Until: 10 * time.Minute})
	if err != nil {
		t.Fatal(err)
	}
	q := f.last(t).query
	eq(t, "query", q.Get("query"), `{app=~".+"} |= "x"`)
	eq(t, "start", q.Get("start"), ns(fixedNow.Add(-2*time.Hour)))
	eq(t, "end", q.Get("end"), ns(fixedNow.Add(-10*time.Minute)))
	eq(t, "limit", q.Get("limit"), "1000")
	eq(t, "direction", q.Get("direction"), "backward")
	eq(t, "auth", f.last(t).header.Get("Authorization"), "")

	want := "time|app|env|pod|line\n" +
		"2026-11-15 11:02:00.123|api||a|third\n" +
		"2026-11-15 11:01:00.123|web|stg||second\n" +
		"2026-11-15 11:00:00.123|api||a|first\n"
	eq(t, "rows", rowsText(res), want)
	eq(t, "truncated", res.Truncated, false)
}

func TestLoki_한도는_5000에서_막고_서버가_한도만큼_주면_잘림으로_표시한다(t *testing.T) {
	f := newFake(t, map[string]func(url.Values) (int, string){
		"/loki/api/v1/query_range": success(`{"resultType":"streams","result":[{"stream":{"a":"1"},"values":[["2","b"],["1","a"]]}]}`),
	})
	l := newLokiAt(t, f.srv.URL, db.Conn{})

	if _, err := l.Run(context.Background(), source.Query{Text: `{a="1"}`, Limit: 9000}); err != nil {
		t.Fatal(err)
	}
	eq(t, "limit", f.last(t).query.Get("limit"), "5000")

	res, err := l.Run(context.Background(), source.Query{Text: `{a="1"}`, Limit: 2})
	if err != nil {
		t.Fatal(err)
	}
	eq(t, "truncated", res.Truncated, true)
}

func TestLoki_지표_질의는_시계열별로_묶은_time_라벨_value_표다(t *testing.T) {
	f := newFake(t, map[string]func(url.Values) (int, string){
		"/loki/api/v1/query_range": success(`{"resultType":"matrix","result":[
			{"metric":{"status":"500"},"values":[[1794708000,"3"],[1794708060.5,"4"]]},
			{"metric":{"status":"200"},"values":[[1794708000,"10"]]}]}`),
	})
	l := newLokiAt(t, f.srv.URL, db.Conn{})

	res, err := l.Run(context.Background(), source.Query{Text: `sum by (status) (count_over_time({a="1"}[1m]))`})
	if err != nil {
		t.Fatal(err)
	}
	want := "time|status|value\n" +
		"2026-11-15 11:00:00.000|200|10\n" +
		"2026-11-15 11:00:00.000|500|3\n" +
		"2026-11-15 11:01:00.500|500|4\n"
	eq(t, "rows", rowsText(res), want)
	eq(t, "step", f.last(t).query.Get("step"), "15")
}

func TestLoki_범위가_7일을_넘으면_Force_없이는_보내지_않는다(t *testing.T) {
	f := newFake(t, map[string]func(url.Values) (int, string){
		"/loki/api/v1/query_range": success(`{"resultType":"streams","result":[]}`),
	})
	l := newLokiAt(t, f.srv.URL, db.Conn{})
	q := source.Query{Text: `{a="1"}`, Since: 8 * 24 * time.Hour}

	if _, err := l.Run(context.Background(), q); !errors.Is(err, source.ErrNeedsConfirm) {
		t.Fatalf("err = %v, want ErrNeedsConfirm", err)
	}
	eq(t, "requests", f.count(), 0)

	// 경계: until 로 줄여 정확히 7일이면 묻지 않는다
	if _, err := l.Run(context.Background(), source.Query{Text: q.Text, Since: q.Since, Until: 24 * time.Hour}); err != nil {
		t.Fatal(err)
	}
	q.Force = true
	res, err := l.Run(context.Background(), q)
	if err != nil {
		t.Fatal(err)
	}
	eq(t, "requests", f.count(), 2)
	eq(t, "cols", strings.Join(res.Cols, "|"), "time|line")
}

func TestLoki_빈_범위와_빈_질의는_거절한다(t *testing.T) {
	l := newLokiAt(t, "http://127.0.0.1:1", db.Conn{})
	if _, err := l.Run(context.Background(), source.Query{Text: `{a="1"}`, Since: time.Hour, Until: time.Hour}); err == nil {
		t.Error("until == since must fail")
	}
	if _, err := l.Run(context.Background(), source.Query{Text: "  \n"}); !errors.Is(err, errEmptyQuery) {
		t.Errorf("err = %v", err)
	}
}

func TestLoki_값_Enter는_라벨_선택자_최근_100줄이다(t *testing.T) {
	f := newFake(t, map[string]func(url.Values) (int, string){
		"/loki/api/v1/query_range": success(`{"resultType":"streams","result":[]}`),
	})
	l := newLokiAt(t, f.srv.URL, db.Conn{})

	if _, err := l.Open(context.Background(), `app/we"b/v2`, 0); err != nil {
		t.Fatal(err)
	}
	q := f.last(t).query
	eq(t, "query", q.Get("query"), `{app="we\"b/v2"}`)
	eq(t, "limit", q.Get("limit"), "100")
	eq(t, "start", q.Get("start"), ns(fixedNow.Add(-time.Hour)))
	if _, err := l.Open(context.Background(), "app", 0); err == nil {
		t.Error("label without value must fail")
	}
}

// ─── Prometheus ───

func TestPrometheus_트리는_지표_이름_앞_1000개다(t *testing.T) {
	names := make([]string, 1200)
	for i := range names {
		names[i] = fmt.Sprintf(`"m%04d"`, i)
	}
	f := newFake(t, map[string]func(url.Values) (int, string){
		"/api/v1/label/__name__/values": success("[" + strings.Join(names, ",") + "]"),
	})
	p := newPromAt(t, f.srv.URL, db.Conn{})

	nodes, err := p.Children(context.Background(), "")
	if err != nil {
		t.Fatal(err)
	}
	eq(t, "len", len(nodes), 1000)
	eq(t, "last", nodes[999].Path, "m0999")
	eq(t, "openable", nodes[0].Openable && !nodes[0].HasChildren, true)
	eq(t, "limit param", f.last(t).query.Get("limit"), "1000")
	if got, _ := p.Children(context.Background(), "m0001"); got != nil {
		t.Errorf("metric children = %+v", got)
	}
}

func TestPrometheus_Enter는_즉시값_vector_표다(t *testing.T) {
	f := newFake(t, map[string]func(url.Values) (int, string){
		"/api/v1/query": success(`{"resultType":"vector","result":[
			{"metric":{"__name__":"up","job":"node"},"value":[1794711600.25,"0"]},
			{"metric":{"__name__":"up","instance":"a:9090","job":"api"},"value":[1794711600.25,"1"]}]}`),
	})
	p := newPromAt(t, f.srv.URL, db.Conn{Auth: "token", Password: "tok"})

	res, err := p.Open(context.Background(), "up", 0)
	if err != nil {
		t.Fatal(err)
	}
	r := f.last(t)
	eq(t, "query", r.query.Get("query"), "up")
	eq(t, "time", r.query.Get("time"), "1794711600.000")
	eq(t, "bearer", r.header.Get("Authorization"), "Bearer tok")
	want := "time|__name__|instance|job|value\n" +
		"2026-11-15 12:00:00.250|up|a:9090|api|1\n" +
		"2026-11-15 12:00:00.250|up||node|0\n"
	eq(t, "rows", rowsText(res), want)
}

func TestPrometheus_실행은_범위_질의이고_step은_범위의_250분의_1_최소_15초다(t *testing.T) {
	f := newFake(t, map[string]func(url.Values) (int, string){
		"/api/v1/query_range": success(`{"resultType":"matrix","result":[
			{"metric":{"status":"200"},"values":[[1794708000,"1"],[1794708015,"2"],[1794708030,"3"]]}]}`),
	})
	p := newPromAt(t, f.srv.URL, db.Conn{})

	res, err := p.Run(context.Background(), source.Query{Text: "sum by (status) (rate(x[5m]))", Limit: 2})
	if err != nil {
		t.Fatal(err)
	}
	q := f.last(t).query
	eq(t, "start", q.Get("start"), "1794708000.000")
	eq(t, "end", q.Get("end"), "1794711600.000")
	eq(t, "step", q.Get("step"), "15")
	eq(t, "rows", len(res.Rows), 2)
	eq(t, "truncated", res.Truncated, true)

	if _, err := p.Run(context.Background(), source.Query{Text: "x", Since: 24 * time.Hour}); err != nil {
		t.Fatal(err)
	}
	eq(t, "step 24h", f.last(t).query.Get("step"), "345")
}

func TestPrometheus_scalar_결과는_time_value_한_줄이다(t *testing.T) {
	f := newFake(t, map[string]func(url.Values) (int, string){
		"/api/v1/query_range": success(`{"resultType":"scalar","result":[1794711600,"2"]}`),
	})
	p := newPromAt(t, f.srv.URL, db.Conn{})
	res, err := p.Run(context.Background(), source.Query{Text: "1+1"})
	if err != nil {
		t.Fatal(err)
	}
	eq(t, "rows", rowsText(res), "time|value\n2026-11-15 12:00:00.000|2\n")
}

// ─── 오류·인증 ───

func TestErrors_서버_메시지를_담고_비밀값은_지운다(t *testing.T) {
	f := newFake(t, map[string]func(url.Values) (int, string){
		"/api/v1/query_range": func(url.Values) (int, string) {
			return 400, `{"status":"error","errorType":"bad_data","error":"parse error near s3cr3t!"}`
		},
		"/loki/api/v1/query_range": func(url.Values) (int, string) { return 400, "parse error at line 1, col 2: syntax error\n" },
	})
	p := newPromAt(t, f.srv.URL, db.Conn{Auth: "basic", User: "u", Password: "s3cr3t!"})
	_, err := p.Run(context.Background(), source.Query{Text: "x{"})
	if err == nil || !strings.Contains(err.Error(), "400") || !strings.Contains(err.Error(), "parse error near ***") || strings.Contains(err.Error(), "s3cr3t") {
		t.Errorf("prom err = %v", err)
	}

	l := newLokiAt(t, f.srv.URL, db.Conn{})
	_, err = l.Run(context.Background(), source.Query{Text: "{"})
	if err == nil || !strings.Contains(err.Error(), "loki 400 Bad Request: parse error at line 1") {
		t.Errorf("loki err = %v", err)
	}
}

func TestAuth_주소의_userinfo는_떼어_basic으로_쓰고_오류에_나오지_않는다(t *testing.T) {
	f := newFake(t, map[string]func(url.Values) (int, string){
		"/api/v1/label/__name__/values": success(`["up"]`),
	})
	u, _ := url.Parse(f.srv.URL)
	u.User = url.UserPassword("mon", "p@ss")
	p := newPromAt(t, u.String(), db.Conn{Auth: "basic"})
	if strings.Contains(p.c.base, "p%40ss") || strings.Contains(p.c.base, "mon") {
		t.Fatalf("base kept userinfo: %s", p.c.base)
	}
	if _, err := p.Children(context.Background(), ""); err != nil {
		t.Fatal(err)
	}
	user, pass, _ := (&http.Request{Header: f.last(t).header}).BasicAuth()
	eq(t, "basic", user+":"+pass, "mon:p@ss")
}

func TestAuth_지원하지_않는_인증과_잘못된_주소는_만들지_않는다(t *testing.T) {
	for _, c := range []db.Conn{
		{URL: "http://x", Auth: "sigv4"},
		{URL: "localhost:3100"},
		{URL: ""},
		{URL: "http://mon:p@ss@x:3100"}, // 인증 없음인데 주소에 user:pass 를 조용히 버리지 않는다
	} {
		if _, err := NewLoki(c); err == nil {
			t.Errorf("NewLoki(%q, %q) succeeded", c.URL, c.Auth)
		}
	}
}

// ─── Grafana ───

const grafanaDS = `[
	{"uid":"lk1","name":"Loki (Legacy)","type":"loki"},
	{"uid":"my1","name":"MySQL","type":"mysql"},
	{"uid":"pr1","name":"Prom","type":"prometheus"},
	{"uid":"lk2","name":"Loki B","type":"loki"}]`

func newGrafanaFake(t *testing.T) (*fake, *grafana) {
	t.Helper()
	f := newFake(t, map[string]func(url.Values) (int, string){
		"/grafana/api/datasources":                                       ok(grafanaDS),
		"/grafana/api/datasources/proxy/uid/lk1/loki/api/v1/labels":      success(`["app"]`),
		"/grafana/api/datasources/proxy/uid/lk1/loki/api/v1/query_range": success(`{"resultType":"streams","result":[]}`),
		"/grafana/api/datasources/proxy/uid/lk2/loki/api/v1/query_range": success(`{"resultType":"streams","result":[]}`),
		"/grafana/api/datasources/proxy/uid/pr1/api/v1/query":            success(`{"resultType":"vector","result":[]}`),
		"/grafana/api/datasources/proxy/uid/pr1/api/v1/query_range":      success(`{"resultType":"matrix","result":[]}`),
	})
	b, err := NewGrafana(db.Conn{URL: f.srv.URL + "/grafana/", Auth: "token", Password: "glsa_x"})
	if err != nil {
		t.Fatal(err)
	}
	g := b.(*grafana)
	g.now = func() time.Time { return fixedNow }
	return f, g
}

func TestGrafana_트리는_Loki_Prometheus_데이터_소스이고_아래는_프록시로_보낸다(t *testing.T) {
	f, g := newGrafanaFake(t)
	ctx := context.Background()

	nodes, err := g.Children(ctx, "")
	if err != nil {
		t.Fatal(err)
	}
	var got []string
	for _, n := range nodes {
		got = append(got, n.Name+"="+n.Path+":"+n.Info)
	}
	eq(t, "datasources", strings.Join(got, ","), "Loki (Legacy)=lk1:loki,Prom=pr1:prometheus,Loki B=lk2:loki")
	eq(t, "bearer", f.last(t).header.Get("Authorization"), "Bearer glsa_x")

	labels, err := g.Children(ctx, "lk1")
	if err != nil {
		t.Fatal(err)
	}
	if len(labels) != 1 || labels[0].Path != "lk1/app" {
		t.Fatalf("labels = %+v", labels)
	}
	r := f.last(t)
	eq(t, "proxy path", r.path, "/grafana/api/datasources/proxy/uid/lk1/loki/api/v1/labels")
	eq(t, "proxy bearer", r.header.Get("Authorization"), "Bearer glsa_x")

	if _, err := g.Open(ctx, "pr1/up", 0); err != nil {
		t.Fatal(err)
	}
	eq(t, "open path", f.last(t).path, "/grafana/api/datasources/proxy/uid/pr1/api/v1/query")

	if _, err := g.Children(ctx, "my1"); err == nil {
		t.Error("non loki/prometheus datasource must fail")
	}
}

func TestGrafana_실행은_지시줄의_데이터_소스로_없으면_첫_Loki로_보낸다(t *testing.T) {
	f, g := newGrafanaFake(t)
	ctx := context.Background()

	if _, err := g.Run(ctx, source.Query{Text: `{app="api"}`}); err != nil {
		t.Fatal(err)
	}
	eq(t, "default", f.last(t).path, "/grafana/api/datasources/proxy/uid/lk1/loki/api/v1/query_range")

	if _, err := g.Run(ctx, source.Query{Text: "\n# datasource: Prom\nsum(up)"}); err != nil {
		t.Fatal(err)
	}
	r := f.last(t)
	eq(t, "by name", r.path, "/grafana/api/datasources/proxy/uid/pr1/api/v1/query_range")
	eq(t, "directive stripped", r.query.Get("query"), "sum(up)")

	if _, err := g.Run(ctx, source.Query{Text: "#DataSource:lk2\n{a=\"1\"}"}); err != nil {
		t.Fatal(err)
	}
	eq(t, "by uid", f.last(t).path, "/grafana/api/datasources/proxy/uid/lk2/loki/api/v1/query_range")

	if _, err := g.Run(ctx, source.Query{Text: "# datasource: MySQL\nselect 1"}); err == nil || !strings.Contains(err.Error(), `"MySQL"`) {
		t.Errorf("err = %v", err)
	}
}

func TestSplitDirective_첫_비어_있지_않은_줄만_지시로_본다(t *testing.T) {
	for _, tc := range []struct{ in, key, rest string }{
		{"# datasource: Loki B\n{a=\"1\"}", "Loki B", `{a="1"}`},
		{"  #datasource:pr1  \nup", "pr1", "up"},
		{"up\n# datasource: Prom", "", "up\n# datasource: Prom"},
		{"# 주석\nup", "", "# 주석\nup"},
		{"", "", ""},
	} {
		key, rest := splitDirective(tc.in)
		if key != tc.key || rest != tc.rest {
			t.Errorf("splitDirective(%q) = %q, %q; want %q, %q", tc.in, key, rest, tc.key, tc.rest)
		}
	}
}

func TestRedirect_를_따라가지_않고_토큰을_다시_보내지_않는다(t *testing.T) {
	var second atomic.Int32
	target := httptest.NewServer(http.HandlerFunc(func(http.ResponseWriter, *http.Request) { second.Add(1) }))
	defer target.Close()
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		http.Redirect(w, r, target.URL+r.URL.Path, http.StatusFound)
	}))
	defer srv.Close()
	p := newPromAt(t, srv.URL, db.Conn{Auth: "token", Password: "glsa_secret"})
	_, err := p.Children(context.Background(), "")
	if err == nil || !strings.Contains(err.Error(), "redirect not followed: 302") || strings.Contains(err.Error(), "glsa_secret") {
		t.Fatalf("err = %v, want redirect not followed", err)
	}
	if n := second.Load(); n != 0 {
		t.Fatalf("리다이렉트를 따라 %d번 더 보냈다", n)
	}
}
