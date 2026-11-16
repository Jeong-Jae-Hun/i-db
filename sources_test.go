package main

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/x/ansi"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/history"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
)

// ─── 가짜 저장소 ───

// fakeSource 는 받은 요청을 적어 두고 정한 값을 돌려주는 Backend 다. 화면 명령 고루틴이 같이 써서 잠근다.
type fakeSource struct {
	mu       sync.Mutex
	conn     db.Conn
	children map[string][]source.Node
	childErr error
	runs     []source.Query
	opens    []string
	limits   []int
	run      func(q source.Query) (*db.Result, error)
	open     func(path string) (*db.Result, error)
	news     int
	closed   int
}

func (f *fakeSource) Children(_ context.Context, path string) ([]source.Node, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	return f.children[path], f.childErr
}

func (f *fakeSource) Run(_ context.Context, q source.Query) (*db.Result, error) {
	f.mu.Lock()
	f.runs = append(f.runs, q)
	run := f.run
	f.mu.Unlock()
	if run != nil {
		return run(q)
	}
	return &db.Result{Cols: []string{"time", "value"}, Rows: [][]string{{"2026-11-15 10:00:00", "1"}, {"2026-11-15 10:01:00", "3"}}}, nil
}

func (f *fakeSource) Open(_ context.Context, path string, limit int) (*db.Result, error) {
	f.mu.Lock()
	f.opens, f.limits = append(f.opens, path), append(f.limits, limit)
	open := f.open
	f.mu.Unlock()
	if open != nil {
		return open(path)
	}
	return &db.Result{Cols: []string{"_id"}, Rows: [][]string{{"doc-1"}, {"doc-2"}}}, nil
}

func (f *fakeSource) Close() error {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.closed++
	return nil
}

func (f *fakeSource) snapshot() (runs []source.Query, opens []string, limits []int) {
	f.mu.Lock()
	defer f.mu.Unlock()
	return append([]source.Query(nil), f.runs...), append([]string(nil), f.opens...), append([]int(nil), f.limits...)
}

// registerFake 는 모든 저장소 종류를 f 로 연다. 시험이 끝나면 등록을 되돌린다.
func registerFake(t *testing.T, f *fakeSource) {
	t.Helper()
	old := map[db.Kind]func(context.Context, db.Conn) (source.Backend, error){}
	for _, k := range db.SourceKinds {
		old[k] = sourceOpeners[k]
		sourceOpeners[k] = func(_ context.Context, c db.Conn) (source.Backend, error) {
			f.mu.Lock()
			f.news++
			f.conn = c
			f.mu.Unlock()
			return f, nil
		}
	}
	t.Cleanup(func() {
		for k, fn := range old {
			if fn == nil {
				delete(sourceOpeners, k)
			} else {
				sourceOpeners[k] = fn
			}
		}
	})
}

func newFake() *fakeSource {
	return &fakeSource{children: map[string][]source.Node{
		"": {
			{Name: "logs-2026", Path: "logs-2026", Info: "1,234 docs", HasChildren: true, Openable: true},
			{Name: "app", Path: "app", Info: "→ logs-2026", Openable: true},
		},
		"logs-2026": {{Name: "status long", Path: "logs-2026/status", Info: "long"}},
	}}
}

// sourceModel 은 저장소 접속 넷(OpenSearch·Loki·prod Prometheus·DynamoDB)을 가진 화면이다.
func sourceModel(t *testing.T) *model {
	t.Helper()
	conns := []db.Conn{
		{Name: "search-stg", UUID: "s1", Kind: db.KindOpenSearch, URL: "https://search.stg.internal", Source: db.SourceDB},
		{Name: "logs-stg", UUID: "s2", Kind: db.KindLoki, URL: "http://loki:3100", Source: db.SourceDB},
		{Name: "metrics", UUID: "s3", Kind: db.KindPrometheus, URL: "https://prom.prod.example.internal", Source: db.SourceDB},
		{Name: "ddb-stg", UUID: "s4", Kind: db.KindDynamoDB, Region: "ap-northeast-2", Source: db.SourceDB},
	}
	m := newModel(&db.Project{Dir: t.TempDir(), Mapping: map[string]string{}, Conns: conns})
	m.Update(tea.WindowSizeMsg{Width: 140, Height: 40})
	h, err := history.Open(filepath.Join(t.TempDir(), "history.jsonl"))
	must(t, err)
	m.hist = h
	t.Cleanup(func() { m.stop(); m.sources.close() })
	m.setFocus(fTree)
	return m
}

// expandConn 은 접속 i 를 트리에서 고르고 Enter 로 펼친다.
func expandConn(t *testing.T, m *model, i int) *node {
	t.Helper()
	cn := m.connNode(i)
	m.rebuild()
	m.selectNode(cn)
	drive(m, m.activate(cn, true))
	return cn
}

func childNamed(n *node, label string) *node {
	for _, c := range n.children {
		if c.label == label {
			return c
		}
	}
	return nil
}

func editorRun(m *model, text string) {
	m.cur().editor.SetValue(text)
	m.setFocus(fEditor)
	_, cmd := m.Update(tea.KeyMsg{Type: tea.KeyCtrlR})
	drive(m, cmd)
}

// ─── 트리 ───

func TestSource_트리_접속을_펼치면_Children_을_읽고_Info_를_흐리게_보이고_l_로_더_펼친다(t *testing.T) {
	f := newFake()
	registerFake(t, f)
	m := sourceModel(t)
	cn := expandConn(t, m, 0)
	idx := childNamed(cn, "logs-2026")
	if idx == nil || idx.kind != nSource || idx.key != "logs-2026" || !idx.openable || !idx.expandable() || idx.loaded {
		t.Fatalf("인덱스 노드: %+v", idx)
	}
	if alias := childNamed(cn, "app"); alias == nil || alias.expandable() {
		t.Fatal("자식 없는 노드는 펼치지 않는다")
	}
	view := ansi.Strip(m.View())
	if !strings.Contains(view, "logs-2026 1,234 docs") || !strings.Contains(view, "app → logs-2026") {
		t.Fatalf("Info 를 이름 뒤에 보인다:\n%s", view)
	}
	m.selectNode(idx)
	_, cmd := m.Update(key("l"))
	drive(m, cmd)
	if len(idx.children) != 1 || idx.children[0].label != "status long" || idx.children[0].key != "logs-2026/status" {
		t.Fatalf("Children(path) 로 펼친다: %+v", idx.children)
	}
	if _, opens, _ := f.snapshot(); len(opens) != 0 {
		t.Fatal("l 은 펼치기만 하고 Open 하지 않는다")
	}
}

func TestSource_트리_r_은_다시_읽고_읽기_오류는_노드에_보인다(t *testing.T) {
	f := newFake()
	registerFake(t, f)
	m := sourceModel(t)
	cn := expandConn(t, m, 0)
	f.mu.Lock()
	f.childErr = errors.New("connection refused")
	f.mu.Unlock()
	m.selectNode(cn)
	_, cmd := m.Update(key("r"))
	drive(m, cmd)
	if len(cn.children) != 1 || !strings.Contains(cn.children[0].label, "connection refused") {
		t.Fatalf("r 은 Children 을 다시 부른다: %+v", cn.children)
	}
}

func TestSource_트리_Enter_는_Openable_노드를_새_결과로_열고_Loki_는_100줄이다(t *testing.T) {
	f := newFake()
	registerFake(t, f)
	m := sourceModel(t)
	m.cur().editor.SetValue("쓰던 글")
	cn := expandConn(t, m, 0)
	m.selectNode(childNamed(cn, "logs-2026"))
	_, cmd := m.Update(key("enter"))
	drive(m, cmd)
	_, opens, limits := f.snapshot()
	if len(opens) != 1 || opens[0] != "logs-2026" || limits[0] != 50 {
		t.Fatalf("Open: %v %v", opens, limits)
	}
	tab := m.cur()
	if len(m.tabs) != 2 || tab.conn != 0 || tab.res == nil || len(tab.res.Rows) != 2 {
		t.Fatalf("쓰던 글을 덮지 않고 새 탭 결과로 연다: tabs=%d conn=%d", len(m.tabs), tab.conn)
	}
	if m.tabs[0].editor.Value() != "쓰던 글" {
		t.Fatal("첫 탭 글은 그대로")
	}
	if childNamed(cn, "logs-2026").expanded {
		t.Fatal("Openable 노드의 Enter 는 펼치지 않고 연다")
	}

	f.children[""] = []source.Node{{Name: "app", Path: "app", Openable: true}}
	ln := expandConn(t, m, 1)
	m.selectNode(childNamed(ln, "app"))
	_, cmd = m.Update(key("enter"))
	drive(m, cmd)
	if _, _, limits := f.snapshot(); limits[len(limits)-1] != 100 {
		t.Fatalf("Loki 미리 보기는 100줄: %v", limits)
	}
}

func TestSource_DynamoDB_테이블_Enter_는_비싼_질의를_묻고_y_면_전체_스캔을_Force_로_보낸다(t *testing.T) {
	f := newFake()
	f.children[""] = []source.Node{{Name: "orders", Path: "orders", Info: "항목 ≈1,000", HasChildren: true, Openable: true}}
	f.open = func(string) (*db.Result, error) { return nil, source.ErrNeedsConfirm }
	f.run = func(q source.Query) (*db.Result, error) {
		return &db.Result{Cols: []string{"pk"}, Rows: [][]string{{"u#1"}}}, nil
	}
	registerFake(t, f)
	m := sourceModel(t)
	cn := expandConn(t, m, 3)
	m.selectNode(childNamed(cn, "orders"))
	_, cmd := m.Update(key("enter"))
	drive(m, cmd)
	if m.overlay != oConfirm || m.yes == nil || !strings.Contains(ansi.Strip(m.View()), "비싼 질의: y/N") {
		t.Fatalf("확인 창: overlay=%v status=%q", m.overlay, m.status)
	}
	// 거절하면 보내지 않는다
	m.Update(key("n"))
	if runs, _, _ := f.snapshot(); len(runs) != 0 || m.status != "취소함" {
		t.Fatalf("N: %v %q", runs, m.status)
	}
	_, cmd = m.Update(key("enter"))
	drive(m, cmd)
	_, cmd = m.Update(key("y"))
	drive(m, cmd)
	runs, _, _ := f.snapshot()
	if len(runs) != 1 || runs[0].Text != `SELECT * FROM "orders"` || !runs[0].Force || runs[0].Limit != 50 {
		t.Fatalf("y 뒤 Force 스캔: %+v", runs)
	}
	if res := m.cur().res; res == nil || res.Rows[0][0] != "u#1" {
		t.Fatal("스캔 결과를 표에 붙인다")
	}
}

func TestSource_트리_운영_조회는_안_되고_X_는_db_접속_지우기다(t *testing.T) {
	registerFake(t, newFake())
	m := sourceModel(t)
	cn := m.connNode(0)
	m.rebuild()
	m.selectNode(cn)
	m.Update(key("A"))
	if !strings.Contains(m.status, msgUnsupported) {
		t.Fatalf("A: %q", m.status)
	}
	m.Update(key("X"))
	if m.overlay != oConfirm || m.yes == nil || !strings.Contains(m.yes.q, "search-stg 접속을 지울까요") {
		t.Fatalf("X 는 접속 지우기 확인: %q", m.status)
	}
}

// ─── 편집기 ───

func TestSource_편집기_CtrlR_은_Backend_Run_으로_보내고_prod_도_묻지_않으며_기록에_질의를_남긴다(t *testing.T) {
	f := newFake()
	registerFake(t, f)
	m := sourceModel(t)
	m.setConn(m.cur(), 2) // prod Prometheus
	editorRun(m, "sum by (status) (rate(http_requests_total[5m]))")
	runs, _, _ := f.snapshot()
	if len(runs) != 1 || runs[0].Text != "sum by (status) (rate(http_requests_total[5m]))" || runs[0].Since != time.Hour || runs[0].Force {
		t.Fatalf("Run: %+v", runs)
	}
	if m.overlay != oNone {
		t.Fatal("읽기 전용 저장소는 prod 확인(R6)을 하지 않는다")
	}
	if res := m.cur().res; res == nil || len(res.Rows) != 2 {
		t.Fatal("결과를 표에 붙인다")
	}
	if es := m.hist.List(history.Filter{}); len(es) != 1 || es[0].Conn != "metrics" || !strings.Contains(es[0].SQL, "http_requests_total") {
		t.Fatalf("기록: %+v", es)
	}
	if m.cur().exportSQL != "" {
		t.Fatal("F7 전체 내보내기는 PostgreSQL 문장만 남긴다")
	}
	// 지표 결과는 차트(R28)로 볼 수 있다
	m.setFocus(fResult)
	m.Update(key("c"))
	if strings.Contains(m.status, "숫자 열") {
		t.Fatalf("차트: %q", m.status)
	}
}

func TestSource_편집기_쓰기로_보이는_질의는_보내지_않음을_알리고_기록하지_않는다(t *testing.T) {
	f := newFake()
	f.run = func(source.Query) (*db.Result, error) { return nil, source.ErrWrite }
	registerFake(t, f)
	m := sourceModel(t)
	m.setConn(m.cur(), 0)
	editorRun(m, "DELETE logs-*/_doc/1")
	if m.status != msgReadOnlySource || m.overlay != oNone || m.cur().running != nil {
		t.Fatalf("status=%q overlay=%v", m.status, m.overlay)
	}
	if es := m.hist.List(history.Filter{}); len(es) != 0 {
		t.Fatalf("보내지 않은 질의는 기록하지 않는다: %+v", es)
	}
}

func TestSource_편집기_비싼_질의는_확인_뒤_Force_로_다시_보낸다(t *testing.T) {
	f := newFake()
	f.run = func(q source.Query) (*db.Result, error) {
		if !q.Force {
			return nil, source.ErrNeedsConfirm
		}
		return &db.Result{Cols: []string{"pk"}, Rows: [][]string{{"x"}}}, nil
	}
	registerFake(t, f)
	m := sourceModel(t)
	m.setConn(m.cur(), 3)
	editorRun(m, `SELECT * FROM "orders"`)
	if m.overlay != oConfirm || m.yes == nil || m.yes.q != msgExpensive {
		t.Fatalf("확인: overlay=%v status=%q", m.overlay, m.status)
	}
	_, cmd := m.Update(key("y"))
	drive(m, cmd)
	runs, _, _ := f.snapshot()
	if len(runs) != 2 || runs[0].Force || !runs[1].Force || runs[1].Text != `SELECT * FROM "orders"` {
		t.Fatalf("Run: %+v", runs)
	}
	if res := m.cur().res; res == nil || res.Rows[0][0] != "x" {
		t.Fatal("Force 결과를 붙인다")
	}
	if es := m.hist.List(history.Filter{}); len(es) != 1 {
		t.Fatalf("보낸 질의만 기록한다: %+v", es)
	}
}

func TestSource_Loki_탭은_편집기_위에_범위를_보이고_CtrlR_은_탭마다_돈다(t *testing.T) {
	f := newFake()
	registerFake(t, f)
	m := sourceModel(t)
	m.setConn(m.cur(), 1)
	if !strings.Contains(ansi.Strip(m.View()), "최근 1h") {
		t.Fatal("범위 줄 기본 1h")
	}
	ctrlBracket := tea.KeyMsg{Type: tea.KeyCtrlCloseBracket}
	var got []string
	for range 5 {
		m.Update(ctrlBracket)
		got = append(got, rangeLabel(m.cur().since))
	}
	if strings.Join(got, ",") != "6h,24h,7d,15m,1h" {
		t.Fatalf("순환: %v", got)
	}
	m.Update(ctrlBracket) // 6h
	if !strings.Contains(ansi.Strip(m.View()), "최근 6h") || m.status != "범위: 최근 6h" {
		t.Fatalf("상태: %q", m.status)
	}
	editorRun(m, `{app="api"} |= "error"`)
	if runs, _, _ := f.snapshot(); runs[0].Since != 6*time.Hour {
		t.Fatalf("Since: %v", runs[0].Since)
	}
	// 새 탭은 기본 범위, OpenSearch 탭에서는 범위가 없다
	nt := m.newTab()
	if nt.since != 0 || !strings.Contains(ansi.Strip(m.View()), "최근 1h") {
		t.Fatal("범위는 탭마다")
	}
	m.setConn(nt, 0)
	m.Update(ctrlBracket)
	if strings.Contains(ansi.Strip(m.View()), "최근 ") || !strings.Contains(m.status, "Loki·Prometheus") {
		t.Fatalf("OpenSearch 탭: %q", m.status)
	}
	editorRun(m, "GET logs-*/_search")
	if runs, _, _ := f.snapshot(); runs[1].Since != 0 {
		t.Fatal("OpenSearch 는 범위를 싣지 않는다")
	}
}

func TestSource_편집기_PostgreSQL_전용_기능은_안내만_한다(t *testing.T) {
	registerFake(t, newFake())
	m := sourceModel(t)
	m.setConn(m.cur(), 0)
	m.cur().editor.SetValue("SELECT * FROM logs")
	m.setFocus(fEditor)
	for k, want := range map[string]string{"f8": msgUnsupported, "ctrl+e": "PostgreSQL", "f9": "PostgreSQL"} {
		m.Update(keyMsgOf(k))
		if !strings.Contains(m.status, want) {
			t.Errorf("%s: %q", k, m.status)
		}
	}
	if m.savableNew(m.cur()) {
		t.Fatal("저장소 질의는 queries/*.sql 로 자동 저장하지 않는다")
	}
}

func TestSource_접속을_고치면_열린_Backend_를_닫는다(t *testing.T) {
	f := newFake()
	registerFake(t, f)
	m := sourceModel(t)
	expandConn(t, m, 0)
	if err := m.sources.forget("s1"); err != nil || f.closed != 1 {
		t.Fatalf("forget: %v closed=%d", err, f.closed)
	}
	cn := m.connNode(0)
	cn.loaded, cn.expanded = false, false
	expandConn(t, m, 0)
	if f.news != 2 {
		t.Fatalf("닫은 뒤 다시 연다: %d", f.news)
	}
}

// ─── 접속 시험 ───

func TestTestConn_저장소는_Children_로_시험하고_오류에서_비밀값을_지운다(t *testing.T) {
	c := db.Conn{Name: "graf", Kind: db.KindGrafana, URL: "https://g", Auth: "token", Password: "glsa_secret"}
	// 등록 안 된 종류는 실제 Grafana 생성기를 잠시 빼서 본다(네트워크에 나가지 않게)
	saved := sourceOpeners[db.KindGrafana]
	delete(sourceOpeners, db.KindGrafana)
	_, err0 := testConn(context.Background(), c)
	sourceOpeners[db.KindGrafana] = saved
	if err0 == nil || !strings.Contains(err0.Error(), "not built in") {
		t.Fatalf("등록 안 된 종류: %v", err0)
	}
	f := newFake()
	registerFake(t, f)
	v, err := testConn(context.Background(), c)
	if err != nil || v != "Grafana · 2개" || f.closed != 1 {
		t.Fatalf("%q %v closed=%d", v, err, f.closed)
	}
	f.childErr = errors.New("401 token glsa_secret rejected")
	if _, err := testConn(context.Background(), c); err == nil || strings.Contains(err.Error(), "glsa_secret") {
		t.Fatalf("비밀값을 지운다: %v", err)
	}
}

// ─── 접속 창 ───

func TestConnForm_종류는_7개를_돌고_종류마다_칸이_다르다(t *testing.T) {
	m := connTestModel(t)
	m.openConnForm(nil, false)
	f := m.connForm
	f.setFocus(cfKind)
	var kinds []db.Kind
	for range len(formKinds) {
		kinds = append(kinds, f.kind)
		m.Update(key(" "))
	}
	if f.kind != db.KindPG || len(kinds) != 7 || kinds[2] != db.KindOpenSearch || kinds[6] != db.KindDynamoDB {
		t.Fatalf("순환: %v", kinds)
	}
	labels := func() string {
		var out []string
		for i := range cfCount {
			if f.visible(i) {
				out = append(out, connFieldLabels[i])
			}
		}
		return strings.Join(out, ",")
	}
	cases := []struct {
		kind db.Kind
		auth string
		want string
	}{
		{db.KindPG, "", "이름,그룹,종류,호스트,포트,DB,사용자,비밀번호"},
		{db.KindOpenSearch, "", "이름,그룹,종류,URL,인증"},
		{db.KindOpenSearch, "basic", "이름,그룹,종류,URL,인증,사용자,비밀번호"},
		{db.KindOpenSearch, "sigv4", "이름,그룹,종류,URL,인증,리전,프로필"},
		{db.KindLoki, "basic", "이름,그룹,종류,URL,인증,사용자,비밀번호"},
		{db.KindGrafana, "token", "이름,그룹,종류,URL,인증,비밀번호"},
		{db.KindDynamoDB, "", "이름,그룹,종류,URL,리전,프로필"},
	}
	for _, c := range cases {
		f.kind, f.auth = c.kind, c.auth
		if got := labels(); got != c.want {
			t.Errorf("%s/%s: %s", c.kind, c.auth, got)
		}
	}
	f.setKind(db.KindGrafana)
	view := ansi.Strip(m.View())
	if f.auth != "token" || !strings.Contains(view, "토큰") || strings.Contains(view, "호스트") {
		t.Fatalf("Grafana 는 토큰:\n%s", view)
	}
	f.setKind(db.KindDynamoDB)
	if view := ansi.Strip(m.View()); !strings.Contains(view, "엔드포인트") || strings.Contains(view, "인증") {
		t.Fatalf("DynamoDB:\n%s", view)
	}
	// 인증 칸은 space 로 그 종류의 값만 돈다. Tab 은 숨은 칸을 건너뛴다
	f.setKind(db.KindOpenSearch)
	f.setFocus(cfAuth)
	m.Update(key(" "))
	m.Update(key(" "))
	if f.auth != "sigv4" {
		t.Fatalf("auth: %q", f.auth)
	}
	m.Update(tea.KeyMsg{Type: tea.KeyTab})
	if f.focus != cfRegion {
		t.Fatalf("sigv4 다음 칸은 리전: %d", f.focus)
	}
}

func TestConnForm_http_URL_을_붙이면_url_칸과_사용자_비밀번호를_채운다(t *testing.T) {
	m := connTestModel(t)
	m.openConnForm(nil, false)
	f := m.connForm
	m.Update(key("search2 "))
	m.Update(paste("https://ro:p%40ss@search.internal:9200/"))
	if f.kind != db.KindOpenSearch || f.auth != "basic" || f.inputs[cfName].Value() != "search2" ||
		f.inputs[cfURL].Value() != "https://search.internal:9200" || f.inputs[cfUser].Value() != "ro" || f.inputs[cfPassword].Value() != "p@ss" {
		t.Fatalf("kind=%s auth=%q url=%q", f.kind, f.auth, f.inputs[cfURL].Value())
	}
	if strings.Contains(ansi.Strip(m.View()), "p@ss") {
		t.Fatal("비밀번호는 • 로만")
	}
	m.openConnForm(nil, false)
	f = m.connForm
	m.Update(paste("http://loki.internal:3100"))
	if f.kind != db.KindLoki || f.auth != "" || f.inputs[cfURL].Value() != "http://loki.internal:3100" {
		t.Fatalf("포트로 Loki 짐작: %s %q", f.kind, f.inputs[cfURL].Value())
	}
	// URL 칸에 사용자 정보째 붙여도 나눈다
	f.setFocus(cfURL)
	f.inputs[cfURL].SetValue("")
	m.Update(paste("http://u:pw@loki.internal:3100"))
	if f.inputs[cfURL].Value() != "http://loki.internal:3100" || f.inputs[cfUser].Value() != "u" || f.auth != "basic" {
		t.Fatalf("URL 칸: %q %q", f.inputs[cfURL].Value(), f.auth)
	}
}

func TestConnForm_Grafana_를_저장하면_토큰을_GRAFANA_TOKEN_키에_쓴다(t *testing.T) {
	registerFake(t, newFake())
	m := connTestModel(t)
	m.openConnForm(nil, false)
	f := m.connForm
	f.inputs[cfName].SetValue("graf")
	f.setKind(db.KindGrafana)
	f.inputs[cfURL].SetValue("https://grafana.internal")
	f.inputs[cfPassword].SetValue("glsa_tok")
	f.inputs[cfHost].SetValue("숨은 칸 값") // 보이지 않는 칸은 저장하지 않는다
	_, cmd := m.Update(key("enter"))
	runTest(t, m, cmd)
	if m.connForm != nil {
		t.Fatalf("시험 성공 뒤 저장하고 닫는다: %q", f.status)
	}
	env, _ := os.ReadFile(filepath.Join(m.proj.Dir, ".env"))
	if !strings.Contains(string(env), "GRAFANA_GRAF_TOKEN=glsa_tok") {
		t.Fatalf(".env: %s", env)
	}
	raw, _ := os.ReadFile(filepath.Join(m.proj.Dir, ".dbtui", "connections.json"))
	if !strings.Contains(string(raw), `"kind": "grafana"`) || !strings.Contains(string(raw), `"auth": "token"`) || strings.Contains(string(raw), "숨은") {
		t.Fatalf("connections.json: %s", raw)
	}
	i := connByName(m.proj, "graf")
	if i < 0 || m.conn(i).Password != "glsa_tok" || m.connNode(i) == nil {
		t.Fatal("다시 읽어 트리에 붙인다")
	}
	// 고치기 창은 저장된 저장소 칸을 채운다
	m.rebuild()
	m.selectNode(m.connNode(i))
	m.Update(key("E"))
	if f := m.connForm; f == nil || f.kind != db.KindGrafana || f.auth != "token" || f.inputs[cfURL].Value() != "https://grafana.internal" {
		t.Fatal("고치기 창")
	}
}

func TestConnForm_저장소_칸이_비면_시험하지_않는다(t *testing.T) {
	m := connTestModel(t)
	m.openConnForm(nil, false)
	f := m.connForm
	f.setKind(db.KindDynamoDB)
	if _, cmd := m.Update(tea.KeyMsg{Type: tea.KeyCtrlT}); cmd != nil || f.status != "리전이 필요함" {
		t.Fatalf("리전: %q", f.status)
	}
	f.setKind(db.KindPrometheus)
	if _, cmd := m.Update(tea.KeyMsg{Type: tea.KeyCtrlT}); cmd != nil || f.status != "URL 이 필요함" {
		t.Fatalf("URL: %q", f.status)
	}
}

// ─── CLI ───

// sourceCLIProject 는 fakeCLIProject 에 저장소 db 전용 접속을 더한다. logs-prod 는 prod 다.
func sourceCLIProject(t *testing.T) string {
	t.Helper()
	dir := fakeCLIProject(t)
	for _, c := range []db.Conn{
		{Name: "search-stg", Kind: db.KindOpenSearch, URL: "https://search.stg.internal"},
		{Name: "logs-stg", Kind: db.KindLoki, URL: "http://loki:3100"},
		{Name: "logs-prod", Kind: db.KindLoki, URL: "http://loki:3100"},
		{Name: "prom", Kind: db.KindPrometheus, URL: "http://prom:9090"},
		{Name: "graf", Kind: db.KindGrafana, URL: "https://grafana", Auth: "token"},
		{Name: "ddb", Kind: db.KindDynamoDB, Region: "ap-northeast-2"},
	} {
		if _, err := db.AddConn(dir, c); err != nil {
			t.Fatal(err)
		}
	}
	return dir
}

func TestCLI_저장소_query_logs_metrics_는_Backend_Run_으로_범위와_한도를_싣는다(t *testing.T) {
	f := newFake()
	registerFake(t, f)
	dir := sourceCLIProject(t)
	code, out, errw := runCLITest(t, "", "query", "-p", dir, "-c", "search-stg", "GET logs-*/_search")
	if code != exitOK || !strings.HasPrefix(out, "time\tvalue\n") {
		t.Fatalf("query: %d %q %q", code, out, errw)
	}
	runs, _, _ := f.snapshot()
	if runs[0].Text != "GET logs-*/_search" || runs[0].Limit != sourceDefaultRows || runs[0].Force || runs[0].Since != 0 {
		t.Fatalf("query 는 기본 1,000행: %+v", runs[0])
	}
	if code, _, errw := runCLITest(t, "", "logs", "-p", dir, "-c", "logs-stg", `{app="api"}`, "--since", "6h", "--until", "1h", "--max-rows", "5"); code != exitOK {
		t.Fatalf("logs: %d %q", code, errw)
	}
	if code, out, errw := runCLITest(t, "", "metrics", "-p", dir, "-c", "graf", "up", "--since", "7d", "--format", "json"); code != exitOK ||
		!strings.Contains(out, `"conn":"graf"`) || !strings.Contains(out, `"cols":["time","value"]`) {
		t.Fatalf("metrics: %d %q %q", code, out, errw)
	}
	runs, _, _ = f.snapshot()
	if r := runs[1]; r.Since != 6*time.Hour || r.Until != time.Hour || r.Limit != 5 {
		t.Fatalf("logs 범위: %+v", r)
	}
	if r := runs[2]; r.Since != 7*24*time.Hour {
		t.Fatalf("metrics 7d: %+v", r)
	}
	if f.closed != 3 {
		t.Fatalf("명령마다 닫는다: %d", f.closed)
	}
}

func TestCLI_저장소_쓰기와_비싼_질의는_3이고_force_면_보낸다(t *testing.T) {
	f := newFake()
	registerFake(t, f)
	dir := sourceCLIProject(t)
	f.run = func(q source.Query) (*db.Result, error) {
		switch {
		case strings.HasPrefix(q.Text, "DELETE"):
			return nil, source.ErrWrite
		case !q.Force:
			return nil, source.ErrNeedsConfirm
		}
		return &db.Result{Cols: []string{"pk"}, Rows: [][]string{{"1"}}}, nil
	}
	if code, _, errw := runCLITest(t, "", "query", "-p", dir, "-c", "ddb", `DELETE FROM "t"`); code != exitRefused || !strings.Contains(errw, msgReadOnlySource) {
		t.Fatalf("ErrWrite: %d %q", code, errw)
	}
	if code, _, errw := runCLITest(t, "", "query", "-p", dir, "-c", "ddb", `SELECT * FROM "t"`); code != exitRefused || !strings.Contains(errw, "비싼 질의 (--force)") {
		t.Fatalf("ErrNeedsConfirm: %d %q", code, errw)
	}
	if code, out, errw := runCLITest(t, "", "query", "-p", dir, "-c", "ddb", "--force", `SELECT * FROM "t"`); code != exitOK || !strings.Contains(out, "pk\n1\n") {
		t.Fatalf("--force: %d %q %q", code, out, errw)
	}
}

func TestCLI_저장소_prod_는_prod_플래그가_필요하고_열기_전에_거절한다(t *testing.T) {
	f := newFake()
	registerFake(t, f)
	dir := sourceCLIProject(t)
	if code, _, errw := runCLITest(t, "", "logs", "-p", dir, "-c", "logs-prod", "{a=\"b\"}"); code != exitRefused || !strings.Contains(errw, "--prod") {
		t.Fatalf("prod: %d %q", code, errw)
	}
	if f.news != 0 {
		t.Fatal("prod 거절은 접속을 열기 전이다")
	}
	if code, _, errw := runCLITest(t, "", "logs", "-p", dir, "-c", "logs-prod", "--prod", "{a=\"b\"}"); code != exitOK {
		t.Fatalf("--prod: %d %q", code, errw)
	}
}

func TestCLI_저장소_tables_는_맨_위_목록이다(t *testing.T) {
	registerFake(t, newFake())
	dir := sourceCLIProject(t)
	code, out, errw := runCLITest(t, "", "tables", "-p", dir, "-c", "search-stg")
	if code != exitOK || out != "name\tinfo\nlogs-2026\t1,234 docs\napp\t→ logs-2026\n" {
		t.Fatalf("%d %q %q", code, out, errw)
	}
}

func TestCLI_저장소에서_안_되는_명령과_플래그는_사용법_오류다(t *testing.T) {
	f := newFake()
	registerFake(t, f)
	dir := sourceCLIProject(t)
	for _, args := range [][]string{
		{"describe", "-c", "search-stg", "t"},
		{"ddl", "-c", "search-stg", "t"},
		{"explain", "-c", "ddb", "select 1"},
		{"redis", "-c", "logs-stg", "GET", "k"},
		{"search", "-c", "search-stg", "x"},
		{"schemas", "-c", "prom"},
		{"schema-diff", "-c", "search-stg", "--to", "app-stg"},
		{"schema-diff", "-c", "app-stg", "--to", "search-stg"},
	} {
		code, _, errw := runCLITest(t, "", append(args, "-p", dir)...)
		if code != exitUsage || !strings.Contains(errw, msgUnsupported) {
			t.Errorf("%v: %d %q", args, code, errw)
		}
	}
	for _, args := range [][]string{
		{"query", "-c", "logs-stg", "--write", "x"},
		{"query", "-c", "logs-stg", "--param", "a=1", "x"},
		{"logs", "-c", "prom", "up"},
		{"metrics", "-c", "logs-stg", "x"},
		{"logs", "-c", "app-stg", "x"},
		{"logs", "-c", "logs-stg", "--since", "x", "y"},
	} {
		if code, _, errw := runCLITest(t, "", append(args, "-p", dir)...); code != exitUsage {
			t.Errorf("%v: 사용법 오류(2)여야 한다: %d %q", args, code, errw)
		}
	}
	if f.news != 0 {
		t.Fatalf("사용법 오류는 접속을 열지 않는다: %d", f.news)
	}
}

func TestCLI_도움말에_logs_metrics_가_있다(t *testing.T) {
	_, out, _ := runCLITest(t, "", "help")
	if !strings.Contains(out, "logs") || !strings.Contains(out, "metrics") {
		t.Fatalf("%q", out)
	}
	_, out, _ = runCLITest(t, "", "help", "logs")
	if !strings.Contains(out, "-since") || !strings.Contains(out, "-until") || !strings.Contains(out, "-force") {
		t.Fatalf("logs 도움말: %q", out)
	}
}

func TestConnCLI_저장소_접속을_더하고_시험한다(t *testing.T) {
	f := newFake()
	registerFake(t, f)
	dir := fakeCLIProject(t)
	var out, errw strings.Builder
	code := runConnCLI(context.Background(), []string{"add", "graf", "-p", dir, "--kind", "grafana", "--url", "https://grafana.internal/", "--password-stdin"},
		strings.NewReader("glsa_x\n"), false, &out, &errw)
	if code != exitOK || !strings.Contains(out.String(), "Grafana https://grafana.internal token") {
		t.Fatalf("add: %d %q %q", code, out.String(), errw.String())
	}
	env, _ := os.ReadFile(filepath.Join(dir, ".env"))
	if !strings.Contains(string(env), "GRAFANA_GRAF_TOKEN=glsa_x") {
		t.Fatalf(".env: %s", env)
	}
	out.Reset()
	code = runConnCLI(context.Background(), []string{"add", "search", "-p", dir, "--kind", "opensearch", "--url", "https://ro:pw@search.internal"},
		nil, false, &out, &errw)
	if code != exitOK || !strings.Contains(out.String(), "OpenSearch https://search.internal basic") {
		t.Fatalf("사용자 정보가 든 URL 은 basic: %d %q %q", code, out.String(), errw.String())
	}
	if f.conn.Password != "pw" || f.conn.User != "ro" {
		t.Fatalf("시험은 URL 의 사용자·비밀번호로: %+v", f.conn)
	}
	out.Reset()
	code = runConnCLI(context.Background(), []string{"add", "ddb", "-p", dir, "--kind", "dynamodb", "--region", "ap-northeast-2"}, nil, false, &out, &errw)
	if code != exitOK {
		t.Fatalf("dynamodb 는 --url 없이: %d %q", code, errw.String())
	}
	out.Reset()
	if code := runConnCLI(context.Background(), []string{"test", "graf", "-p", dir}, nil, false, &out, &errw); code != exitOK || !strings.Contains(out.String(), "접속됨 (Grafana · 2개)") {
		t.Fatalf("test: %d %q", code, out.String())
	}
}

// ─── 리뷰 반영 ───

func TestSource_편집기_비싼_질의_확인은_까닭을_보인다(t *testing.T) {
	f := newFake()
	f.run = func(q source.Query) (*db.Result, error) {
		if !q.Force {
			return nil, source.NeedsConfirm("전체 스캔")
		}
		return &db.Result{Cols: []string{"pk"}, Rows: [][]string{{"x"}}}, nil
	}
	registerFake(t, f)
	m := sourceModel(t)
	m.setConn(m.cur(), 3)
	editorRun(m, `SELECT * FROM "orders"`)
	if m.overlay != oConfirm || m.yes == nil || m.yes.q != "비싼 질의: 전체 스캔" {
		t.Fatalf("확인 글: overlay=%v yes=%+v", m.overlay, m.yes)
	}
}

func TestCLI_저장소_비싼_질의_거절은_까닭을_싣는다(t *testing.T) {
	f := newFake()
	f.run = func(source.Query) (*db.Result, error) { return nil, source.NeedsConfirm("범위 7일 초과") }
	registerFake(t, f)
	dir := sourceCLIProject(t)
	if code, _, errw := runCLITest(t, "", "logs", "-p", dir, "-c", "logs-stg", `{a="b"}`, "--since", "8d"); code != exitRefused ||
		!strings.Contains(errw, "비싼 질의: 범위 7일 초과 (--force)") {
		t.Fatalf("%d %q", code, errw)
	}
}

func TestCLI_since_날짜는_3650일까지만_받는다(t *testing.T) {
	var d durFlag
	if err := d.Set("3650d"); err != nil || time.Duration(d) != 3650*24*time.Hour {
		t.Fatalf("3650d: %v %v", err, time.Duration(d))
	}
	for _, s := range []string{"3651d", "200000d", "-1d"} {
		if err := d.Set(s); err == nil {
			t.Errorf("%s 는 오류여야 한다 (%v)", s, time.Duration(d))
		}
	}
}

func TestCLI_AWS_프로필이_prod_면_prod_플래그가_필요하다(t *testing.T) {
	f := newFake()
	registerFake(t, f)
	dir := sourceCLIProject(t)
	if _, err := db.AddConn(dir, db.Conn{Name: "orders", Kind: db.KindDynamoDB, Region: "ap-northeast-2", Profile: "prod-admin"}); err != nil {
		t.Fatal(err)
	}
	if code, _, errw := runCLITest(t, "", "query", "-p", dir, "-c", "orders", `SELECT * FROM "t" WHERE pk = 'a'`); code != exitRefused || !strings.Contains(errw, "--prod") {
		t.Fatalf("prod 프로필: %d %q", code, errw)
	}
	if f.news != 0 {
		t.Fatal("prod 거절은 접속을 열기 전이다")
	}
}

func TestSourcePool_접속_값이_바뀌면_닫고_다시_연다(t *testing.T) {
	f := newFake()
	registerFake(t, f)
	p := newSourcePool()
	defer p.close()
	c := db.Conn{Name: "search", UUID: "u1", Kind: db.KindOpenSearch, URL: "https://s", Auth: "basic", User: "ro", Password: "old"}
	for range 2 {
		if _, err := p.get(context.Background(), c); err != nil {
			t.Fatal(err)
		}
	}
	if f.news != 1 || f.closed != 0 {
		t.Fatalf("같은 값은 다시 쓴다: news=%d closed=%d", f.news, f.closed)
	}
	c.Password = "new"
	if _, err := p.get(context.Background(), c); err != nil {
		t.Fatal(err)
	}
	if f.news != 2 || f.closed != 1 || f.conn.Password != "new" {
		t.Fatalf("비밀값이 바뀌면 새로 연다: news=%d closed=%d pw=%q", f.news, f.closed, f.conn.Password)
	}
}

func TestConnWarning_평문_http_비밀번호와_AWS_밖_서명을_알린다(t *testing.T) {
	cases := []struct {
		c    db.Conn
		want string
	}{
		{db.Conn{Kind: db.KindOpenSearch, URL: "http://search:9200", Auth: "basic"}, msgPlainHTTP},
		{db.Conn{Kind: db.KindLoki, URL: "http://loki:3100", Auth: "token"}, msgPlainHTTP},
		{db.Conn{Kind: db.KindGrafana, URL: "http://grafana"}, msgPlainHTTP},
		{db.Conn{Kind: db.KindOpenSearch, URL: "https://search", Auth: "basic"}, ""},
		{db.Conn{Kind: db.KindOpenSearch, URL: "http://search", Auth: "sigv4"}, ""},
		{db.Conn{Kind: db.KindLoki, URL: "http://loki:3100"}, ""},
		{db.Conn{Kind: db.KindDynamoDB, URL: "https://ddb.example.com"}, msgForeignSigV4},
		{db.Conn{Kind: db.KindDynamoDB, URL: "https://dynamodb.evil-amazonaws.com"}, msgForeignSigV4},
		{db.Conn{Kind: db.KindDynamoDB, URL: "http://localhost:8000"}, ""},
		{db.Conn{Kind: db.KindDynamoDB, URL: "https://dynamodb.ap-northeast-2.amazonaws.com"}, ""},
		{db.Conn{Kind: db.KindDynamoDB}, ""},
		{db.Conn{Kind: db.KindPG, Host: "db.internal", SSLMode: "prefer"}, msgPlainPG},
		{db.Conn{Kind: db.KindPG, Host: "db.internal", SSLMode: "require"}, ""},
		{db.Conn{Kind: db.KindPG, Host: "localhost", SSLMode: "disable"}, ""},
	}
	for _, c := range cases {
		if got := connWarning(c.c); got != c.want {
			t.Errorf("%s %s %s: %q, want %q", c.c.Kind, c.c.URL, c.c.Auth, got, c.want)
		}
	}
}

func TestConnCLI_평문_http_basic_은_경고를_찍는다(t *testing.T) {
	registerFake(t, newFake())
	dir := fakeCLIProject(t)
	var out, errw strings.Builder
	code := runConnCLI(context.Background(), []string{"add", "search", "-p", dir, "--kind", "opensearch", "--url", "http://ro:pw@search.internal"},
		nil, false, &out, &errw)
	if code != exitOK || !strings.Contains(errw.String(), "경고: "+msgPlainHTTP) {
		t.Fatalf("add: %d %q %q", code, out.String(), errw.String())
	}
	errw.Reset()
	if code := runConnCLI(context.Background(), []string{"test", "search", "-p", dir}, nil, false, &out, &errw); code != exitOK ||
		!strings.Contains(errw.String(), msgPlainHTTP) {
		t.Fatalf("test: %d %q", code, errw.String())
	}
}

// ─── R61.3 OpenSearch 정렬 다시 읽기 ───

func TestSource_OpenSearch_정렬과_거르기는_본문을_바꿔_서버에서_다시_읽는다(t *testing.T) {
	f := newFake()
	f.run = func(q source.Query) (*db.Result, error) {
		rows := [][]string{{"a", "1"}, {"b", "2"}}
		switch {
		case strings.Contains(q.Text, `"filter"`):
			rows = [][]string{{"y", "98"}} // 서버가 거른 문서
		case strings.Contains(q.Text, `"order":"desc"`):
			rows = [][]string{{"z", "99"}, {"y", "98"}} // 받은 행 밖의 문서
		}
		return &db.Result{Cols: []string{"_id", "n"}, Rows: rows}, nil
	}
	registerFake(t, f)
	m := sourceModel(t)
	m.cur().conn = 0
	editorRun(m, "GET logs/_search\n{\"size\":2}")
	tab := m.cur()
	m.setFocus(fResult)
	tab.grid.FocusCol("n")
	_, cmd := m.Update(key("s")) // 오름
	drive(m, cmd)
	_, cmd = m.Update(key("s")) // 내림
	drive(m, cmd)
	runs, _, _ := f.snapshot()
	if len(runs) != 3 || !strings.Contains(runs[2].Text, `{"size":2,"sort":[{"n":{"missing":"_last","order":"desc"}}]}`) {
		t.Fatalf("정렬마다 sort 를 바꿔 보낸다: %+v", runs)
	}
	if tab.res.Rows[0][0] != "z" || tab.grid.Summary() != "정렬 n ↓" || tab.baseSQL != "GET logs/_search\n{\"size\":2}" {
		t.Fatalf("서버에서 다시 읽은 행: %q %q %q", tab.res.Rows[0][0], tab.grid.Summary(), tab.baseSQL)
	}

	m.Update(key("/"))
	m.Update(key("n"))
	m.Update(key("="))
	m.Update(key("98"))
	if runs, _, _ := f.snapshot(); len(runs) != 3 {
		t.Fatal("치는 동안은 보내지 않는다")
	}
	_, cmd = m.Update(key("enter"))
	drive(m, cmd)
	runs, _, _ = f.snapshot()
	want := `{"query":{"bool":{"filter":[{"term":{"n":"98"}}],"must":[{"match_all":{}}]}},"size":2,"sort":[{"n":{"missing":"_last","order":"desc"}}]}`
	if len(runs) != 4 || !strings.HasSuffix(runs[3].Text, want) || len(tab.res.Rows) != 1 || !strings.Contains(tab.grid.Summary(), "1/1행") {
		t.Fatalf("거르기 확정은 bool.filter 로 다시 읽는다: %+v %q", runs, tab.grid.Summary())
	}
	if h := m.hist.List(history.Filter{}); h[0].SQL != runs[3].Text {
		t.Fatalf("기록에는 보낸 글: %q", h[0].SQL)
	}

	_, cmd = m.Update(tea.KeyMsg{Type: tea.KeyEsc}) // 거르기 해제 → 정렬만
	drive(m, cmd)
	_, cmd = m.Update(key("s")) // 정렬 없음 → 원래 요청
	drive(m, cmd)
	if runs, _, _ := f.snapshot(); len(runs) != 6 || runs[5].Text != "GET logs/_search\n{\"size\":2}" || strings.Contains(runs[4].Text, "filter") {
		t.Fatalf("둘 다 풀면 원래 요청을 보낸다: %+v", runs)
	}
}

func TestSource_OpenSearch_SELECT_결과는_받은_행_안에서만_정렬한다(t *testing.T) {
	f := newFake()
	registerFake(t, f)
	m := sourceModel(t)
	m.cur().conn = 0
	editorRun(m, "SELECT * FROM logs")
	tab := m.cur()
	m.setFocus(fResult)
	tab.grid.FocusCol("value")
	if _, cmd := m.Update(key("s")); cmd != nil || tab.running != nil {
		t.Fatal("SQL 플러그인 결과는 다시 읽지 않는다")
	}
}

func TestSource_OpenSearch_트리_미리_보기도_정렬을_서버로_다시_읽는다(t *testing.T) {
	f := newFake()
	registerFake(t, f)
	m := sourceModel(t)
	cn := expandConn(t, m, 0)
	m.selectNode(childNamed(cn, "logs-2026"))
	_, cmd := m.Update(key("enter"))
	drive(m, cmd)
	tab := m.cur()
	m.setFocus(fResult)
	tab.grid.FocusCol("_id")
	_, cmd = m.Update(key("s"))
	drive(m, cmd)
	runs, _, _ := f.snapshot()
	want := "POST logs-2026/_search\n" + `{"query":{"match_all":{}},"size":50,"sort":[{"_id":{"missing":"_last","order":"asc"}}]}`
	if len(runs) != 1 || runs[0].Text != want {
		t.Fatalf("미리 보기 요청에 sort 를 붙인다: %+v", runs)
	}
}

// ─── R14.6 받은 행만 정렬·거르기 까닭 ───

func TestSource_서버로_다시_읽지_못하면_까닭을_한_번만_알린다(t *testing.T) {
	f := newFake()
	registerFake(t, f)
	m := sourceModel(t)
	m.cur().conn = 0
	editorRun(m, "SELECT * FROM logs")
	tab := m.cur()
	m.setFocus(fResult)
	tab.grid.FocusCol("value")
	m.Update(key("s"))
	if m.status != "받은 행만 정렬: SELECT 결과" || tab.grid.Summary() != "정렬 value ↑" {
		t.Fatalf("까닭: %q %q", m.status, tab.grid.Summary())
	}
	m.status = ""
	m.Update(key("j"))
	if m.status != "" {
		t.Fatalf("같은 정렬로는 다시 알리지 않는다: %q", m.status)
	}

	m.setConn(tab, 2) // Prometheus
	editorRun(m, "up")
	m.setFocus(fResult)
	m.Update(key("/"))
	m.Update(key("3"))
	if strings.HasPrefix(m.status, "받은 행만") {
		t.Fatalf("치는 동안은 알리지 않는다: %q", m.status)
	}
	m.Update(key("enter"))
	if m.status != "받은 행만 거르기: 이 저장소는 서버 거르기 없음" {
		t.Fatalf("거르기 까닭: %q", m.status)
	}
	tab.grid.FocusCol("time")
	m.Update(key("s"))
	if m.status != "받은 행만 정렬: 이 저장소는 서버 정렬 없음" {
		t.Fatalf("정렬 까닭: %q", m.status)
	}
	if runs, _, _ := f.snapshot(); len(runs) != 2 {
		t.Fatalf("다시 보내지 않는다: %d", len(runs))
	}
}

// ─── R62.3 Loki time 정렬 ───

func lokiRows(q source.Query) (*db.Result, error) {
	rows := [][]string{{"2026-11-15 10:02:00.000", "api", "new"}, {"2026-11-15 10:01:00.000", "api", "mid"}}
	if q.Direction == "forward" {
		rows = [][]string{{"2026-11-15 09:00:00.000", "api", "old"}, {"2026-11-15 09:01:00.000", "api", "older2"}}
	}
	return &db.Result{Cols: []string{"time", "app", "line"}, Rows: rows}, nil
}

func TestSource_Loki_time_정렬은_방향을_바꿔_같은_범위로_다시_읽는다(t *testing.T) {
	f := newFake()
	f.run = lokiRows
	registerFake(t, f)
	m := sourceModel(t)
	tab := m.cur()
	m.setConn(tab, 1)
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlCloseBracket}) // 6h
	editorRun(m, `{app="api"}`)
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlCloseBracket}) // 24h: 다시 읽기는 원래 실행의 범위를 쓴다
	m.setFocus(fResult)
	tab.grid.FocusCol("time")
	_, cmd := m.Update(key("s")) // 오름 → forward
	drive(m, cmd)
	runs, _, _ := f.snapshot()
	if len(runs) != 2 || runs[1].Direction != "forward" || runs[1].Since != 6*time.Hour || runs[1].Text != `{app="api"}` {
		t.Fatalf("forward 로 다시 읽는다: %+v", runs)
	}
	if tab.res.Rows[0][2] != "old" || tab.grid.Summary() != "정렬 time ↑" {
		t.Fatalf("범위의 오래된 줄부터: %q %q", tab.res.Rows[0][2], tab.grid.Summary())
	}
	_, cmd = m.Update(key("s")) // 내림 → backward
	drive(m, cmd)
	_, cmd = m.Update(key("s")) // 없음 → 이미 backward 라 보내지 않는다
	drive(m, cmd)
	runs, _, _ = f.snapshot()
	if len(runs) != 3 || runs[2].Direction != "backward" || tab.res.Rows[0][2] != "new" {
		t.Fatalf("내림은 backward, 정렬 없음은 그대로: %+v", runs)
	}
	tab.grid.FocusCol("app")
	m.Update(key("s"))
	if m.status != "받은 행만 정렬: time 열만 서버 정렬" {
		t.Fatalf("다른 열: %q", m.status)
	}
}

func TestSource_Loki_트리_미리_보기도_time_정렬을_서버로_다시_읽는다(t *testing.T) {
	f := newFake()
	f.run = lokiRows
	f.open = func(string) (*db.Result, error) { return lokiRows(source.Query{}) }
	f.children[""] = []source.Node{{Name: "app", Path: "app", HasChildren: true}}
	f.children["app"] = []source.Node{{Name: "api", Path: "app/api", Openable: true}}
	registerFake(t, f)
	m := sourceModel(t)
	ln := expandConn(t, m, 1)
	app := childNamed(ln, "app")
	m.selectNode(app)
	drive(m, m.activate(app, true))
	m.selectNode(childNamed(app, "api"))
	_, cmd := m.Update(key("enter"))
	drive(m, cmd)
	tab := m.cur()
	m.setFocus(fResult)
	tab.grid.FocusCol("time")
	_, cmd = m.Update(key("s"))
	drive(m, cmd)
	runs, _, _ := f.snapshot()
	if len(runs) != 1 || runs[0].Text != `{app="api"}` || runs[0].Limit != source.LokiOpenLimit || runs[0].Direction != "forward" {
		t.Fatalf("미리 보기 질의를 forward 로: %+v", runs)
	}
}

// ─── R63.3 DynamoDB 정렬 키 ───

// fakeDynamo 는 정렬 키를 아는 가짜 DynamoDB 다(dynamo.Backend.SortKey 와 같은 꼴).
type fakeDynamo struct {
	*fakeSource
	sk, reason string
}

func (f fakeDynamo) SortKey(string) (string, string) { return f.sk, f.reason }

func dynamoModel(t *testing.T, sk, reason string) (*model, *fakeSource) {
	t.Helper()
	f := newFake()
	f.run = func(source.Query) (*db.Result, error) {
		return &db.Result{Cols: []string{"pk", "sk", "name"}, Rows: [][]string{{"a", "1", "x"}, {"a", "2", "y"}}}, nil
	}
	registerFake(t, f)
	sourceOpeners[db.KindDynamoDB] = func(context.Context, db.Conn) (source.Backend, error) {
		return fakeDynamo{f, sk, reason}, nil
	}
	m := sourceModel(t)
	m.setConn(m.cur(), 3)
	editorRun(m, `SELECT * FROM "orders" WHERE pk = 'a' -- 메모`)
	m.setFocus(fResult)
	return m, f
}

func TestSource_DynamoDB_정렬_키는_ORDER_BY_를_붙여_다시_읽는다(t *testing.T) {
	m, f := dynamoModel(t, "sk", "")
	tab := m.cur()
	tab.grid.FocusCol("sk")
	_, cmd := m.Update(key("s"))
	drive(m, cmd)
	_, cmd = m.Update(key("s"))
	drive(m, cmd)
	runs, _, _ := f.snapshot()
	if len(runs) != 3 || runs[1].Text != `SELECT * FROM "orders" WHERE pk = 'a' ORDER BY "sk" ASC` ||
		runs[2].Text != `SELECT * FROM "orders" WHERE pk = 'a' ORDER BY "sk" DESC` {
		t.Fatalf("ORDER BY 를 붙인다: %+v", runs)
	}
	_, cmd = m.Update(key("s"))
	drive(m, cmd)
	if runs, _, _ := f.snapshot(); len(runs) != 4 || runs[3].Text != `SELECT * FROM "orders" WHERE pk = 'a' -- 메모` {
		t.Fatalf("정렬을 풀면 원래 문장: %+v", runs)
	}
	tab.grid.FocusCol("name")
	m.Update(key("s"))
	if m.status != "받은 행만 정렬: 정렬 키(sk)가 아님" {
		t.Fatalf("다른 열: %q", m.status)
	}
}

func TestSource_DynamoDB_파티션_키_조건이_없으면_받은_행만_정렬한다(t *testing.T) {
	m, f := dynamoModel(t, "", "파티션 키 = 조건 없음")
	m.cur().grid.FocusCol("sk")
	if _, cmd := m.Update(key("s")); cmd != nil || m.status != "받은 행만 정렬: 파티션 키 = 조건 없음" {
		t.Fatalf("까닭: %q", m.status)
	}
	if runs, _, _ := f.snapshot(); len(runs) != 1 {
		t.Fatal("다시 보내지 않는다")
	}
}

// ─── R14.7 더 읽기 ───

func TestSource_더_읽기는_PostgreSQL_결과만이다(t *testing.T) {
	f := newFake()
	registerFake(t, f)
	m := sourceModel(t)
	m.cur().conn = 0
	editorRun(m, "GET logs/_search")
	m.setFocus(fResult)
	if _, cmd := m.Update(key("]")); cmd != nil || m.status != "더 읽지 않음: PostgreSQL 결과만 더 읽음" {
		t.Fatalf("저장소 결과: %q", m.status)
	}
}
