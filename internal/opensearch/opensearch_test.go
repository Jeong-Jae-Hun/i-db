package opensearch

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"sync/atomic"
	"testing"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
)

type seen struct {
	method, path, query, body, auth string
}

// fakeServer 는 경로별로 정한 응답을 돌려주고 받은 요청을 기록한다.
func fakeServer(t *testing.T, routes map[string]string) (*httptest.Server, func() []seen) {
	t.Helper()
	var (
		mu  sync.Mutex
		got []seen
	)
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, _ := io.ReadAll(r.Body)
		mu.Lock()
		got = append(got, seen{r.Method, r.URL.Path, r.URL.RawQuery, string(body), r.Header.Get("Authorization")})
		mu.Unlock()
		resp, ok := routes[r.Method+" "+r.URL.Path]
		if !ok {
			w.WriteHeader(http.StatusNotFound)
			_, _ = io.WriteString(w, `{"error":{"type":"index_not_found_exception","reason":"no such index"},"status":404}`)
			return
		}
		_, _ = io.WriteString(w, resp)
	}))
	t.Cleanup(srv.Close)
	return srv, func() []seen {
		mu.Lock()
		defer mu.Unlock()
		return slices.Clone(got)
	}
}

func newBackend(t *testing.T, c db.Conn) source.Backend {
	t.Helper()
	b, err := New(c)
	if err != nil {
		t.Fatal(err)
	}
	return b
}

const searchResp = `{"hits":{"total":{"value":2},"hits":[
 {"_index":"logs","_id":"1","_score":1.5,"_source":{"msg":"hi","n":3,"tags":["a","b"]}},
 {"_index":"logs","_id":"2","_score":null,"_source":{"user":{"id":7},"msg":null,"ok":true}}]}}`

func TestAllowedPath_읽기_경로만_허용한다(t *testing.T) {
	cases := []struct {
		method, path, endpoint string
		ok                     bool
	}{
		{"GET", "logs/_search", "_search", true},
		{"GET", "logs-*/_count", "_count", true},
		{"GET", "logs/_mapping", "_mapping", true},
		{"GET", "_search", "_search", true},
		{"GET", "_cat/indices", "_cat", true},
		{"GET", "_cat/aliases/foo", "_cat", true},
		{"GET", "_all/_search", "_search", true},
		{"POST", "logs/_search", "_search", true},
		{"POST", "_search", "_search", true},
		{"POST", "logs/_count", "", false},
		{"POST", "logs/_doc", "", false},
		{"PUT", "logs", "", false},
		{"DELETE", "logs", "", false},
		{"DELETE", "logs/_doc/1", "", false},
		{"POST", "_bulk", "", false},
		{"POST", "logs/_delete_by_query", "", false},
		{"POST", "logs/_update/1", "", false},
		{"GET", "logs/_doc/1", "", false},
		{"GET", "_cluster/settings", "", false},
		{"GET", "_cat", "", false},
		{"GET", "_doc/_search", "", false},
		{"GET", "logs", "", false},
		{"GET", "/_search", "", false},
		// 경로 우회: 빈 마디·./..·인코딩·제어 문자·시스템 인덱스
		{"GET", "_cat/a/../..", "", false},
		{"GET", "_cat/indices/../../_cluster", "", false},
		{"GET", "_cat/indices/foo/bar", "", false},
		{"GET", "_cat/in-dices", "", false},
		{"GET", "../_search", "", false},
		{"GET", "./_search", "", false},
		{"GET", "logs//_search", "", false},
		{"GET", ".opendistro_security/_search", "", false},
		{"GET", "logs,.opendistro_security/_search", "", false},
		{"GET", "_cat/indices/.kibana", "", false},
		{"GET", "%2e%2e/_search", "", false},
		{"GET", "logs%2F_doc/_search", "", false},
		{"GET", "lo\tgs/_search", "", false},
		{"GET", "lo\x00gs/_search", "", false},
		{"GET", "lo\\gs/_search", "", false},
		{"GET", "logs,-old/_search", "_search", true},
	}
	for _, c := range cases {
		endpoint, ok := allowedPath(c.method, c.path)
		if ok != c.ok || (ok && endpoint != c.endpoint) {
			t.Errorf("%s %s = %q,%v, want %q,%v", c.method, c.path, endpoint, ok, c.endpoint, c.ok)
		}
	}
}

func TestRun_쓰기_요청은_보내지_않고_ErrWrite(t *testing.T) {
	srv, got := fakeServer(t, nil)
	b := newBackend(t, db.Conn{URL: srv.URL})
	for _, text := range []string{
		"DELETE logs",
		"PUT logs/_doc/1\n{\"a\":1}",
		"POST logs/_doc\n{}",
		"POST _bulk",
		"GET logs/_doc/1",
		"UPDATE logs SET a = 1",
		"DELETE FROM logs",
		"",
		"-- 주석만",
	} {
		_, err := b.Run(context.Background(), source.Query{Text: text})
		if !errors.Is(err, source.ErrWrite) {
			t.Errorf("%q: err = %v, want ErrWrite", text, err)
		}
	}
	if n := len(got()); n != 0 {
		t.Fatalf("거절한 질의가 %d번 나갔다", n)
	}
}

func TestRun_SELECT_는_SQL_플러그인으로_REST_는_경로로_보낸다(t *testing.T) {
	srv, got := fakeServer(t, map[string]string{
		"POST /_plugins/_sql": `{"schema":[{"name":"msg","type":"text"},{"name":"count(*)","alias":"c","type":"integer"}],"datarows":[["hi",2],[null,{"x":1}]],"total":2,"size":2,"status":200}`,
		"GET /logs/_search":   searchResp,
	})
	b := newBackend(t, db.Conn{URL: srv.URL + "/"})
	res, err := b.Run(context.Background(), source.Query{Text: "-- 첫 줄 주석\n  select msg, count(*) as c from logs group by msg;"})
	if err != nil {
		t.Fatal(err)
	}
	if !slices.Equal(res.Cols, []string{"msg", "c"}) {
		t.Fatalf("cols = %v", res.Cols)
	}
	if !slices.Equal(res.Rows[0], []string{"hi", "2"}) || !slices.Equal(res.Rows[1], []string{"NULL", `{"x":1}`}) {
		t.Fatalf("rows = %v", res.Rows)
	}
	if _, err := b.Run(context.Background(), source.Query{Text: "# 주석\nget logs/_search\n{\"query\":{\"match_all\":{}}}"}); err != nil {
		t.Fatal(err)
	}
	reqs := got()
	if reqs[0].path != "/_plugins/_sql" || reqs[0].query != "format=jdbc" {
		t.Fatalf("sql 요청 = %+v", reqs[0])
	}
	var sql struct{ Query string }
	if err := json.Unmarshal([]byte(reqs[0].body), &sql); err != nil || strings.HasSuffix(sql.Query, ";") || !strings.Contains(sql.Query, "select msg") {
		t.Fatalf("sql 본문 = %s", reqs[0].body)
	}
	if reqs[1].method != "GET" || reqs[1].path != "/logs/_search" || reqs[1].body != `{"query":{"match_all":{}}}` {
		t.Fatalf("rest 요청 = %+v", reqs[1])
	}
}

func TestRun_search_결과는_필드_합집합과_JSON_칸이다(t *testing.T) {
	srv, _ := fakeServer(t, map[string]string{"POST /logs/_search": searchResp})
	b := newBackend(t, db.Conn{URL: srv.URL})
	res, err := b.Run(context.Background(), source.Query{Text: "POST logs/_search"})
	if err != nil {
		t.Fatal(err)
	}
	want := []string{"_index", "_id", "_score", "msg", "n", "tags", "user", "ok"}
	if !slices.Equal(res.Cols, want) {
		t.Fatalf("cols = %v, want %v", res.Cols, want)
	}
	if !slices.Equal(res.Rows[0], []string{"logs", "1", "1.5", "hi", "3", `["a","b"]`, "NULL", "NULL"}) {
		t.Fatalf("row0 = %v", res.Rows[0])
	}
	if !slices.Equal(res.Rows[1], []string{"logs", "2", "NULL", "NULL", "NULL", "NULL", `{"id":7}`, "true"}) {
		t.Fatalf("row1 = %v", res.Rows[1])
	}
	if res.Truncated {
		t.Fatal("잘리지 않았는데 Truncated")
	}

	res, err = b.Run(context.Background(), source.Query{Text: "POST logs/_search", Limit: 1})
	if err != nil {
		t.Fatal(err)
	}
	if len(res.Rows) != 1 || !res.Truncated || slices.Contains(res.Cols, "user") {
		t.Fatalf("Limit 1: rows=%d truncated=%v cols=%v", len(res.Rows), res.Truncated, res.Cols)
	}
}

func TestRun_집계는_aggregations_JSON_칸이다(t *testing.T) {
	srv, _ := fakeServer(t, map[string]string{
		"GET /a/_search": `{"hits":{"hits":[]},"aggregations":{"by":{"buckets":[{"key":"x","doc_count":2}]}}}`,
		"GET /b/_search": `{"hits":{"hits":[{"_index":"b","_id":"1","_score":1,"_source":{"k":1}}]},"aggregations":{"m":{"value":1}}}`,
	})
	b := newBackend(t, db.Conn{URL: srv.URL})
	res, err := b.Run(context.Background(), source.Query{Text: "GET a/_search\n{\"size\":0,\"aggs\":{\"by\":{\"terms\":{\"field\":\"k\"}}}}"})
	if err != nil {
		t.Fatal(err)
	}
	if !slices.Equal(res.Cols, []string{"aggregations"}) || res.Rows[0][0] != `{"by":{"buckets":[{"key":"x","doc_count":2}]}}` {
		t.Fatalf("문서 없는 집계 = %v %v", res.Cols, res.Rows)
	}
	res, err = b.Run(context.Background(), source.Query{Text: "GET b/_search"})
	if err != nil {
		t.Fatal(err)
	}
	if !slices.Equal(res.Cols, []string{"_index", "_id", "_score", "k", "aggregations"}) || len(res.Rows) != 2 {
		t.Fatalf("문서+집계 = %v %v", res.Cols, res.Rows)
	}
	if res.Rows[0][4] != "NULL" || !slices.Equal(res.Rows[1], []string{"NULL", "NULL", "NULL", "NULL", `{"m":{"value":1}}`}) {
		t.Fatalf("집계 행 = %v", res.Rows)
	}
}

func TestRun_count_와_cat_과_mapping_은_표다(t *testing.T) {
	srv, got := fakeServer(t, map[string]string{
		"GET /logs/_count":   `{"count":42,"_shards":{}}`,
		"GET /_cat/indices":  `[{"index":"a","health":"green"},{"index":"b","docs.count":"3"}]`,
		"GET /logs/_mapping": `{"logs":{"mappings":{"properties":{"msg":{"type":"text"},"user":{"properties":{"id":{"type":"long"}}}}}}}`,
	})
	b := newBackend(t, db.Conn{URL: srv.URL})
	res, err := b.Run(context.Background(), source.Query{Text: "GET logs/_count\n{\"query\":{\"term\":{\"a\":1}}}"})
	if err != nil {
		t.Fatal(err)
	}
	if !slices.Equal(res.Cols, []string{"count"}) || res.Rows[0][0] != "42" {
		t.Fatalf("count = %v %v", res.Cols, res.Rows)
	}
	res, err = b.Run(context.Background(), source.Query{Text: "GET _cat/indices?v&format=text"})
	if err != nil {
		t.Fatal(err)
	}
	if !slices.Equal(res.Cols, []string{"index", "health", "docs.count"}) || !slices.Equal(res.Rows[1], []string{"b", "NULL", "3"}) {
		t.Fatalf("cat = %v %v", res.Cols, res.Rows)
	}
	if q := got()[1].query; !strings.Contains(q, "format=json") || strings.Contains(q, "format=text") {
		t.Fatalf("_cat 질의 문자열 = %q", q)
	}
	res, err = b.Run(context.Background(), source.Query{Text: "GET logs/_mapping"})
	if err != nil {
		t.Fatal(err)
	}
	if len(res.Rows) != 2 || !slices.Equal(res.Rows[1], []string{"logs", "user.id", "long"}) {
		t.Fatalf("mapping = %v", res.Rows)
	}
}

func TestRun_size_1만_초과는_확인을_받는다(t *testing.T) {
	srv, got := fakeServer(t, map[string]string{"GET /logs/_search": searchResp})
	b := newBackend(t, db.Conn{URL: srv.URL})
	for _, text := range []string{
		"GET logs/_search\n{\"size\":10001}",
		"GET logs/_search\n{\"from\":9990,\"size\":20}",
		"GET logs/_search?size=20000",
	} {
		_, err := b.Run(context.Background(), source.Query{Text: text})
		if !errors.Is(err, source.ErrNeedsConfirm) {
			t.Errorf("%q: err = %v, want ErrNeedsConfirm", text, err)
		}
	}
	if n := len(got()); n != 0 {
		t.Fatalf("확인 전 %d번 나갔다", n)
	}
	if _, err := b.Run(context.Background(), source.Query{Text: "GET logs/_search\n{\"size\":10000}"}); err != nil {
		t.Fatalf("경계 1만 = %v", err)
	}
	if _, err := b.Run(context.Background(), source.Query{Text: "GET logs/_search\n{\"size\":20000}", Force: true}); err != nil {
		t.Fatalf("Force = %v", err)
	}
	if _, err := b.Run(context.Background(), source.Query{Text: "GET logs/_search\n{\"size\":"}); err == nil || errors.Is(err, source.ErrWrite) {
		t.Fatalf("깨진 JSON 본문 = %v", err)
	}
}

func TestChildren_인덱스와_별칭과_매핑_필드(t *testing.T) {
	srv, _ := fakeServer(t, map[string]string{
		"GET /_cat/indices": `[{"index":"logs-2","docs.count":"5","health":"yellow"},{"index":".kibana","docs.count":"1","health":"green"},{"index":"logs-1","docs.count":"3","health":"green"}]`,
		"GET /_cat/aliases": `[{"alias":"logs","index":"logs-2"},{"alias":"logs","index":"logs-1"},{"alias":".sys","index":".kibana"}]`,
		"GET /logs/_mapping": `{"logs-1":{"mappings":{"properties":{"msg":{"type":"text","fields":{"keyword":{"type":"keyword"}}},"user":{"properties":{"id":{"type":"long"},"geo":{"properties":{"lat":{"type":"float"}}}}}}}},
		  "logs-2":{"mappings":{"properties":{"extra":{"type":"keyword"},"msg":{"type":"keyword"}}}}}`,
	})
	b := newBackend(t, db.Conn{URL: srv.URL})
	nodes, err := b.Children(context.Background(), "")
	if err != nil {
		t.Fatal(err)
	}
	var names []string
	for _, n := range nodes {
		names = append(names, n.Name+"|"+n.Path+"|"+n.Info)
	}
	want := []string{"logs → logs-1, logs-2|logs|alias", "logs-1|logs-1|3 green", "logs-2|logs-2|5 yellow"}
	if !slices.Equal(names, want) {
		t.Fatalf("nodes = %v, want %v", names, want)
	}
	if !nodes[0].HasChildren || !nodes[0].Openable {
		t.Fatal("인덱스·별칭은 펼치고 열 수 있어야 한다")
	}
	fields, err := b.Children(context.Background(), "logs")
	if err != nil {
		t.Fatal(err)
	}
	names = nil
	for _, n := range fields {
		names = append(names, n.Name+" "+n.Info)
	}
	if !slices.Equal(names, []string{"extra keyword", "msg text", "user.geo.lat float", "user.id long"}) {
		t.Fatalf("fields = %v", names)
	}
	if fields[0].Path != "logs/extra" || fields[0].HasChildren {
		t.Fatalf("field node = %+v", fields[0])
	}
	if leaf, err := b.Children(context.Background(), "logs/extra"); err != nil || len(leaf) != 0 {
		t.Fatalf("필드 아래 = %v %v", leaf, err)
	}
}

func TestOpen_match_all_size_로_연다(t *testing.T) {
	srv, got := fakeServer(t, map[string]string{"POST /logs/_search": searchResp})
	b := newBackend(t, db.Conn{URL: srv.URL})
	res, err := b.Open(context.Background(), "logs", 0)
	if err != nil {
		t.Fatal(err)
	}
	if len(res.Rows) != 2 {
		t.Fatalf("rows = %d", len(res.Rows))
	}
	if body := got()[0].body; body != `{"query":{"match_all":{}},"size":50}` {
		t.Fatalf("body = %s", body)
	}
}

func TestBasic_인증_헤더를_싣고_오류에서_비밀번호를_지운다(t *testing.T) {
	const pw = "s3cr:et@pw"
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		u, p, ok := r.BasicAuth()
		if !ok || u != "reader" || p != pw {
			w.WriteHeader(http.StatusUnauthorized)
			return
		}
		w.WriteHeader(http.StatusBadRequest)
		_, _ = io.WriteString(w, `{"error":{"root_cause":[],"type":"parsing_exception","reason":"bad token `+pw+`"},"status":400}`)
	}))
	defer srv.Close()
	b := newBackend(t, db.Conn{URL: srv.URL, Auth: "basic", User: "reader", Password: pw})
	_, err := b.Run(context.Background(), source.Query{Text: "GET logs/_search"})
	if err == nil {
		t.Fatal("오류가 나야 한다")
	}
	msg := err.Error()
	if !strings.Contains(msg, "400") || !strings.Contains(msg, "parsing_exception: bad token") || strings.Contains(msg, pw) {
		t.Fatalf("err = %q", msg)
	}
}

func TestSigV4_서명한_Authorization_을_싣는다(t *testing.T) {
	dir := t.TempDir()
	t.Setenv("AWS_CONFIG_FILE", filepath.Join(dir, "config"))
	t.Setenv("AWS_SHARED_CREDENTIALS_FILE", filepath.Join(dir, "credentials"))
	t.Setenv("AWS_PROFILE", "")
	t.Setenv("AWS_ACCESS_KEY_ID", "AKIDEXAMPLE")
	t.Setenv("AWS_SECRET_ACCESS_KEY", "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY")
	t.Setenv("AWS_SESSION_TOKEN", "")
	srv, got := fakeServer(t, map[string]string{"GET /logs/_count": `{"count":1}`})
	b := newBackend(t, db.Conn{URL: srv.URL, Auth: "sigv4", Region: "ap-northeast-2"})
	if _, err := b.Run(context.Background(), source.Query{Text: "GET logs/_count\n{}"}); err != nil {
		t.Fatal(err)
	}
	auth := got()[0].auth
	if !strings.HasPrefix(auth, "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/") || !strings.Contains(auth, "/ap-northeast-2/es/aws4_request") {
		t.Fatalf("Authorization = %q", auth)
	}
}

func TestNew_잘못된_설정은_거절한다(t *testing.T) {
	for _, c := range []db.Conn{
		{URL: ""},
		{URL: "localhost:9200"},
		{URL: "ftp://x"},
		{URL: "http://x", Auth: "token"},
		{URL: "http://x", Auth: "basic"},
	} {
		if _, err := New(c); err == nil {
			t.Errorf("%+v: 오류가 나야 한다", c)
		}
	}
}

func TestCell_큰_칸은_IsCappedCell_꼴로_자른다(t *testing.T) {
	raw, _ := json.Marshal(strings.Repeat("가", db.MaxCellBytes))
	if s := cell(raw); !db.IsCappedCell(s) {
		t.Fatalf("잘린 칸으로 알아보지 못한다: len=%d", len(s))
	}
}

func TestRun_size_from_은_문자열_실수_지수_중복까지_읽어_확인을_받는다(t *testing.T) {
	srv, got := fakeServer(t, map[string]string{"GET /logs/_search": searchResp})
	b := newBackend(t, db.Conn{URL: srv.URL})
	need := map[string]string{
		"GET logs/_search\n{\"size\":\"20000\"}":           "size 1만 초과",
		"GET logs/_search\n{\"size\":1e5}":                 "size 1만 초과",
		"GET logs/_search\n{\"size\":10000.5}":             "size 1만 초과",
		"GET logs/_search\n{\"from\":\"9999\",\"size\":2}": "size 1만 초과",
		"GET logs/_search?size=20&size=20000":              "읽을 수 없음",
		"GET logs/_search?size=abc":                        "읽을 수 없음",
		"GET logs/_search\n{\"size\":10,\"size\":20000}":   "읽을 수 없음",
		"GET logs/_search?size=20\n{\"size\":20}":          "읽을 수 없음",
		"GET logs/_search\n{\"size\":\"many\"}":            "읽을 수 없음",
		"GET logs/_search\n{\"size\":\"NaN\"}":             "읽을 수 없음",
		"GET logs/_search\n[1]":                            "읽을 수 없음",
	}
	for text, reason := range need {
		_, err := b.Run(context.Background(), source.Query{Text: text})
		if !errors.Is(err, source.ErrNeedsConfirm) || !strings.Contains(source.ConfirmReason(err), reason) {
			t.Errorf("%q: err = %v, want ErrNeedsConfirm(%s)", text, err, reason)
		}
	}
	if n := len(got()); n != 0 {
		t.Fatalf("확인 전 %d번 나갔다", n)
	}
	for _, text := range []string{"GET logs/_search\n{\"size\":\"20\"}", "GET logs/_search\n{\"size\":2e3}", "GET logs/_search?size=100&from=10"} {
		if _, err := b.Run(context.Background(), source.Query{Text: text}); err != nil {
			t.Errorf("%q: %v, want 바로 보냄", text, err)
		}
	}
}

func TestRun_SQL_은_끝_세미콜론_뒤_다른_문장이나_쓰기_낱말을_거절한다(t *testing.T) {
	srv, got := fakeServer(t, map[string]string{"POST /_plugins/_sql": `{"schema":[{"name":"a"}],"datarows":[[1]]}`})
	b := newBackend(t, db.Conn{URL: srv.URL})
	for _, text := range []string{
		"SELECT * FROM logs; DELETE FROM logs",
		"SELECT * FROM logs;; SELECT 1",
		"SELECT * FROM (DELETE FROM logs)",
		"select a from logs where b in (select c from x); drop table x",
		"SELECT * FROM logs WHERE 1=1 /* x */ ; update logs set a=1",
	} {
		if _, err := b.Run(context.Background(), source.Query{Text: text}); !errors.Is(err, source.ErrWrite) {
			t.Errorf("%q: err = %v, want ErrWrite", text, err)
		}
	}
	if n := len(got()); n != 0 {
		t.Fatalf("거절한 SQL 이 %d번 나갔다", n)
	}
	for _, text := range []string{
		"SELECT * FROM logs WHERE msg = 'a; delete from x';",
		"SELECT * FROM logs -- ; drop table x\n;",
		"SELECT updated_at FROM logs ;  ",
	} {
		if _, err := b.Run(context.Background(), source.Query{Text: text}); err != nil {
			t.Errorf("%q: %v, want 보냄", text, err)
		}
	}
	if q := got()[1].body; strings.Contains(q, ";") {
		t.Errorf("끝 ;·주석은 떼고 보내야 한다: %s", q)
	}
}

func TestRedirect_를_따라가지_않고_오류로_본다(t *testing.T) {
	var second, leaked int32
	target := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		atomic.AddInt32(&second, 1)
		if r.Header.Get("Authorization") != "" {
			atomic.AddInt32(&leaked, 1)
		}
	}))
	defer target.Close()
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/logs/_search" {
			http.Redirect(w, r, "/_all/_delete_by_query", http.StatusFound) // 같은 호스트 다른 경로
			return
		}
		http.Redirect(w, r, target.URL+"/x", http.StatusFound)
	}))
	defer srv.Close()
	b := newBackend(t, db.Conn{URL: srv.URL, Auth: "basic", User: "reader", Password: "pw"})
	for _, text := range []string{"GET logs/_search", "GET logs/_count"} {
		_, err := b.Run(context.Background(), source.Query{Text: text})
		if err == nil || !strings.Contains(err.Error(), "redirect not followed: 302") {
			t.Errorf("%q: err = %v, want redirect not followed", text, err)
		}
	}
	if atomic.LoadInt32(&second) != 0 || atomic.LoadInt32(&leaked) != 0 {
		t.Fatalf("리다이렉트를 따라갔다: 두 번째 요청 %d, 인증 헤더 %d", second, leaked)
	}
}
