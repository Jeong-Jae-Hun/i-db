package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/x/ansi"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/history"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
)

// historyModel 은 기록·사본 폴더를 임시 상태 폴더에 둔 화면이다. 접속 0 은 x-prod, 1 은 x-stg 다.
func historyModel(t *testing.T) (*model, string) {
	t.Helper()
	state := t.TempDir()
	t.Setenv("XDG_STATE_HOME", state)
	m := testModel(t)
	attachHistory(t, m)
	return m, state
}

func attachHistory(t *testing.T, m *model) {
	t.Helper()
	hp, err := history.DefaultPath()
	must(t, err)
	m.hist, err = history.Open(hp)
	must(t, err)
	m.results, err = openResults(hp)
	must(t, err)
}

// finishRun 은 접속 conn 에서 sql 을 실행한 것처럼 결과를 넣고 기록 명령을 돌린다. 돌려준 메시지(알림)를 같이 준다.
func finishRun(t *testing.T, m *model, conn int, sql string, res *db.Result) tea.Msg {
	t.Helper()
	m.cur().conn = conn
	m.cur().running = func() {}
	_, cmd := m.Update(resultMsg{tab: m.cur().id, seq: m.cur().seq, conn: conn, sql: sql, stmts: 1, res: res})
	if cmd == nil {
		t.Fatal("기록 명령이 있어야 한다")
	}
	return cmd()
}

func snapFiles(t *testing.T, state string) []string {
	t.Helper()
	ents, err := os.ReadDir(filepath.Join(state, "db", "results"))
	if errors.Is(err, os.ErrNotExist) {
		return nil
	}
	must(t, err)
	var out []string
	for _, e := range ents {
		out = append(out, e.Name())
	}
	return out
}

func lastEntry(t *testing.T, m *model) history.Entry {
	t.Helper()
	es := m.hist.List(history.Filter{Limit: 1})
	if len(es) != 1 {
		t.Fatal("기록이 없다")
	}
	return es[0]
}

func table(n int) *db.Result {
	res := &db.Result{Cols: []string{"id", "name"}, Types: []string{"int4", "text"}, Elapsed: 120 * time.Millisecond}
	for i := range n {
		res.Rows = append(res.Rows, []string{string(rune('0' + i%10)), "n"})
	}
	return res
}

// ─── 사본 남기기 ───

func TestSnapshot_stg_실행_결과는_사본을_0600_으로_남기고_기록에_id_와_result_를_단다(t *testing.T) {
	m, state := historyModel(t)
	if msg := finishRun(t, m, 1, "select id, name from t", table(3)); msg != nil {
		t.Fatalf("알림 없이 끝나야 한다: %v", msg)
	}
	e := lastEntry(t, m)
	if e.ID == "" || !e.Result || e.Rows != 3 {
		t.Fatalf("기록: %+v", e)
	}
	path := filepath.Join(state, "db", "results", e.ID+".json.gz")
	st, err := os.Stat(path)
	if err != nil || st.Mode().Perm() != 0o600 {
		t.Fatalf("사본 파일: %v %v", st, err)
	}
	s, err := m.results.Read(e.ID)
	if err != nil || s.Conn != "x-stg" || s.SQL != "select id, name from t" || len(s.Rows) != 3 || s.Types[1] != "text" || s.Elapsed != 120*time.Millisecond {
		t.Fatalf("사본: %+v %v", s, err)
	}
}

func TestSnapshot_prod_결과는_기본으로_남기지_않고_DB_HISTORY_PROD_1_이면_남긴다(t *testing.T) {
	m, state := historyModel(t)
	finishRun(t, m, 0, "select 1", table(1))
	if e := lastEntry(t, m); e.Result || e.SQL != "select 1" || e.Rows != 1 {
		t.Fatalf("prod 는 SQL·행 수만 기록: %+v", e)
	}
	if files := snapFiles(t, state); len(files) != 0 {
		t.Fatalf("prod 사본이 남았다: %v", files)
	}
	for _, v := range []string{"true", "yes"} {
		t.Setenv("DB_HISTORY_PROD", v)
		finishRun(t, m, 0, "select 1", table(1))
		if len(snapFiles(t, state)) != 0 {
			t.Fatalf("DB_HISTORY_PROD=%s 은 켜는 값이 아니다", v)
		}
	}
	t.Setenv("DB_HISTORY_PROD", "1")
	finishRun(t, m, 0, "select 2", table(1))
	if e := lastEntry(t, m); !e.Result || len(snapFiles(t, state)) != 1 {
		t.Fatalf("DB_HISTORY_PROD=1 이면 남긴다: %+v", e)
	}
}

func TestSnapshot_비밀값을_가린_문장은_사본을_남기지_않는다(t *testing.T) {
	m, state := historyModel(t)
	finishRun(t, m, 1, "ALTER ROLE app PASSWORD 'x'", &db.Result{Text: "ALTER ROLE"})
	if e := lastEntry(t, m); e.Result || e.SQL != db.MaskedSQL {
		t.Fatalf("기록: %+v", e)
	}
	if files := snapFiles(t, state); len(files) != 0 {
		t.Fatalf("사본이 남았다: %v", files)
	}
}

func TestSnapshot_DB_HISTORY_RESULTS_0_이면_사본_폴더를_열지_않고_기록만_한다(t *testing.T) {
	state := t.TempDir()
	t.Setenv("XDG_STATE_HOME", state)
	t.Setenv("DB_HISTORY_RESULTS", "0")
	m := testModel(t)
	attachHistory(t, m)
	if m.results != nil {
		t.Fatal("사본을 열지 않는다")
	}
	finishRun(t, m, 1, "select 1", table(1))
	if e := lastEntry(t, m); e.Result || e.SQL != "select 1" {
		t.Fatalf("기록은 남긴다: %+v", e)
	}
	if _, err := os.Stat(filepath.Join(state, "db", "results")); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("사본 폴더를 만들지 않는다: %v", err)
	}
}

func TestSnapshot_쓰기는_화면_밖에서_돌아_Update_가_기다리지_않는다(t *testing.T) {
	m, _ := historyModel(t)
	started, release := make(chan history.Snapshot, 1), make(chan struct{})
	saved := writeSnapshot
	writeSnapshot = func(r *history.Results, s history.Snapshot) error {
		started <- s
		<-release
		return saved(r, s)
	}
	t.Cleanup(func() { writeSnapshot = saved })
	m.cur().conn = 1
	_, cmd := m.Update(resultMsg{tab: m.cur().id, seq: m.cur().seq, conn: 1, sql: "select 1", stmts: 1, res: table(2)})
	select {
	case <-started:
		t.Fatal("Update 안에서 사본을 쓰면 안 된다")
	default:
	}
	if m.cur().res == nil || len(m.cur().res.Rows) != 2 {
		t.Fatal("결과는 쓰기를 기다리지 않고 붙는다")
	}
	done := make(chan tea.Msg)
	go func() { done <- cmd() }()
	s := <-started
	if s.Conn != "x-stg" || len(s.Rows) != 2 {
		t.Fatalf("넘긴 사본: %+v", s)
	}
	close(release)
	if msg := <-done; msg != nil {
		t.Fatalf("알림 없이 끝나야 한다: %v", msg)
	}
	if !lastEntry(t, m).Result {
		t.Fatal("쓰기가 끝나면 result 를 단다")
	}
}

func TestSnapshot_쓰기에_실패하면_한_번만_알리고_사본을_끈다(t *testing.T) {
	m, state := historyModel(t)
	saved := writeSnapshot
	writeSnapshot = func(*history.Results, history.Snapshot) error { return errors.New("disk full") }
	t.Cleanup(func() { writeSnapshot = saved })
	if msg := finishRun(t, m, 1, "select 1", table(1)); msg != statusMsg("결과 사본을 끔: disk full") {
		t.Fatalf("첫 실패는 알린다: %v", msg)
	}
	if e := lastEntry(t, m); e.Result || e.SQL != "select 1" {
		t.Fatalf("기록은 남기고 result 는 달지 않는다: %+v", e)
	}
	writeSnapshot = saved
	if msg := finishRun(t, m, 1, "select 2", table(1)); msg != nil || len(snapFiles(t, state)) != 0 {
		t.Fatalf("끈 뒤에는 쓰지도 알리지도 않는다: %v", msg)
	}
}

func TestSnapshot_큰_결과는_앞_1000행만_남기고_잘림을_표시한다(t *testing.T) {
	m, _ := historyModel(t)
	finishRun(t, m, 1, "select * from big", table(1500))
	s, err := m.results.Read(lastEntry(t, m).ID)
	if err != nil || len(s.Rows) != 1000 || !s.Truncated || s.RowCount != 1500 {
		t.Fatalf("사본: %d행 truncated=%v count=%d %v", len(s.Rows), s.Truncated, s.RowCount, err)
	}
}

// ─── 기록 창 ───

// seedHistory 는 사본 있는 stg 기록, 사본 없는 기록, 실패 기록을 남긴다. 최근 것부터 fail·plain·snap 이다.
func seedHistory(t *testing.T, m *model) {
	t.Helper()
	finishRun(t, m, 1, "select id, name from orders", table(2))
	must(t, m.hist.Append(history.Entry{ID: history.NewID(time.Now()), Time: time.Now(), Conn: "x-stg", SQL: "select 2 from plain", Rows: 1}))
	must(t, m.hist.Append(history.Entry{ID: history.NewID(time.Now()), Time: time.Now(), Conn: "x-stg", SQL: "select nope", Err: "bad"}))
}

func openHistoryWindow(m *model, filter string) {
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlH})
	typeText(m, filter)
}

func TestHistoryWindow_줄은_시각_접속_행수_시간_SQL_이고_사본이_있으면_표시한다(t *testing.T) {
	m, _ := historyModel(t)
	seedHistory(t, m)
	openHistoryWindow(m, "")
	view := ansi.Strip(m.View())
	snap := time.Now().Format("01-02 15:04") + " · x-stg · 2행 · 0.12s · select id, name from orders"
	if !strings.Contains(view, "◆ "+snap) {
		t.Fatalf("사본 표시:\n%s", view)
	}
	for _, ln := range strings.Split(view, "\n") {
		if strings.Contains(ln, "select 2 from plain") && (strings.Contains(ln, "◆") || !strings.Contains(ln, " · x-stg · 1행 · 0.00s · ")) {
			t.Fatalf("사본 없는 줄: %q", ln)
		}
	}
	if !strings.Contains(view, "✗ ") || !strings.Contains(view, "e 편집기에 넣기") {
		t.Fatalf("실패 표시·안내:\n%s", view)
	}
}

func TestHistoryWindow_거르기는_접속_이름과_오늘_어제_낱말을_본다(t *testing.T) {
	m, _ := historyModel(t)
	seedHistory(t, m)
	yesterday := time.Now().AddDate(0, 0, -1)
	must(t, m.hist.Append(history.Entry{ID: history.NewID(yesterday), Time: yesterday, Conn: "x-prod", SQL: "select old"}))
	openHistoryWindow(m, "x-prod")
	if got := m.historyMatches(); len(got) != 1 || got[0].SQL != "select old" {
		t.Fatalf("접속 이름: %+v", got)
	}
	m.input.SetValue("어제")
	if got := m.historyMatches(); len(got) != 1 || got[0].SQL != "select old" {
		t.Fatalf("어제: %+v", got)
	}
	m.input.SetValue("today orders")
	if got := m.historyMatches(); len(got) != 1 || got[0].SQL != "select id, name from orders" {
		t.Fatalf("오늘 + 글: %+v", got)
	}
}

func TestHistoryWindow_Enter_는_사본을_읽기_전용_새_탭에_열고_r_은_그_접속에서_다시_실행한다(t *testing.T) {
	m, _ := historyModel(t)
	seedHistory(t, m)
	m.cur().editor.SetValue("쓰던 글")
	m.cur().conn = 0
	openHistoryWindow(m, "orders")
	m.Update(key("enter"))
	tab := m.cur()
	title := "기록 · " + time.Now().Format("01-02 15:04")
	if len(m.tabs) != 2 || tab.title != title || tab.snap == nil || tab.res == nil || len(tab.res.Rows) != 2 || m.focus != fResult {
		t.Fatalf("사본 탭: tabs=%d title=%q snap=%v", len(m.tabs), tab.title, tab.snap)
	}
	if tab.conn != 1 || tab.editor.Value() != "select id, name from orders" || m.tabs[0].editor.Value() != "쓰던 글" {
		t.Fatalf("탭 접속·편집기: conn=%d %q", tab.conn, tab.editor.Value())
	}
	if !strings.Contains(m.status, "기록 사본") || !strings.Contains(m.status, "x-stg") || !strings.Contains(m.status, "읽기 전용") {
		t.Fatalf("상태 줄: %q", m.status)
	}
	if tab.grid.Editable() {
		t.Fatal("사본은 고칠 수 없다")
	}
	m.Update(key("a"))
	if tab.grid.Pending() != 0 {
		t.Fatal("행 추가도 안 된다")
	}
	// 정렬은 받은 행 안에서 한다. 서버로 보내지 않는다
	m.Update(key("s"))
	if tab.running != nil {
		t.Fatal("사본 정렬은 다시 읽지 않는다")
	}
	if hints := strings.Join(m.hints(), " · "); !strings.HasPrefix(hints, "r 다시 실행") {
		t.Fatalf("키 안내: %q", hints)
	}
	_, cmd := m.Update(key("r"))
	if cmd == nil || tab.running == nil {
		t.Fatalf("r 은 다시 실행한다: %q", m.status)
	}
	m.Update(resultMsg{tab: tab.id, seq: tab.seq, conn: 1, sql: tab.snap.sql, stmts: 1, res: table(5)})
	if tab.snap != nil || len(tab.res.Rows) != 5 || strings.HasPrefix(tab.title, "기록") {
		t.Fatalf("다시 실행한 결과가 사본을 갈음한다: title=%q", tab.title)
	}
}

func TestHistoryWindow_사본_탭의_r_은_prod_쓰기면_확인을_거친다(t *testing.T) {
	m, _ := historyModel(t)
	t.Setenv("DB_HISTORY_PROD", "1")
	finishRun(t, m, 0, "delete from t where id = 1", &db.Result{Text: "DELETE 1"})
	m.cur().editor.SetValue("x")
	openHistoryWindow(m, "")
	m.Update(key("enter"))
	tab := m.cur()
	if tab.snap == nil || tab.conn != 0 {
		t.Fatal("prod 사본 탭")
	}
	m.Update(key("r"))
	if m.overlay != oConfirm || tab.running != nil {
		t.Fatalf("prod 쓰기는 확인: overlay=%v", m.overlay)
	}
}

func TestHistoryWindow_사본이_없으면_Enter_는_편집기에_e_는_늘_편집기에_d_는_사본만_지운다(t *testing.T) {
	m, state := historyModel(t)
	seedHistory(t, m)
	openHistoryWindow(m, "plain")
	m.Update(key("enter"))
	if len(m.tabs) != 1 || m.cur().snap != nil || m.cur().editor.Value() != "select 2 from plain" || m.focus != fEditor {
		t.Fatalf("사본 없으면 편집기: tabs=%d %q", len(m.tabs), m.cur().editor.Value())
	}
	// 거르기 글이 없을 때 e 는 사본이 있어도 편집기에 넣는다(목록은 최근 것부터 fail·plain·snap)
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlH})
	m.Update(tea.KeyMsg{Type: tea.KeyDown})
	m.Update(tea.KeyMsg{Type: tea.KeyDown})
	m.Update(key("e"))
	if m.overlay != oNone || len(m.tabs) != 2 || m.cur().snap != nil || m.cur().editor.Value() != "select id, name from orders" {
		t.Fatalf("e: tabs=%d %q", len(m.tabs), m.cur().editor.Value())
	}
	snap := m.hist.List(history.Filter{Query: "orders"})[0]
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlH})
	m.Update(tea.KeyMsg{Type: tea.KeyDown})
	m.Update(tea.KeyMsg{Type: tea.KeyDown})
	m.Update(key("d"))
	if m.overlay != oHistory || m.results.Has(snap.ID) || len(snapFiles(t, state)) != 0 || !strings.Contains(m.status, "결과 사본을 지움") {
		t.Fatalf("d: overlay=%v status=%q", m.overlay, m.status)
	}
	if len(m.hist.List(history.Filter{})) != 3 {
		t.Fatal("기록 줄은 지우지 않는다")
	}
	if strings.Contains(ansi.Strip(m.View()), "◆ "+time.Now().Format("01-02")) {
		t.Fatal("지운 사본은 표시하지 않는다")
	}
	// 거르기 글이 있으면 e·d 는 글자다
	typeText(m, "xed")
	if m.input.Value() != "xed" || m.overlay != oHistory {
		t.Fatalf("글자: %q", m.input.Value())
	}
}

func TestSession_기록_사본_탭은_되살리지_않는다(t *testing.T) {
	m, _ := historyModel(t)
	seedHistory(t, m)
	m.cur().editor.SetValue("쓰던 글")
	openHistoryWindow(m, "orders")
	m.Update(key("enter"))
	if m.cur().snap == nil {
		t.Fatal("사본 탭")
	}
	must(t, m.saveSession())
	if tabs := savedTabs(t, m); len(tabs) != 1 || tabs[0].Text != "쓰던 글" {
		t.Fatalf("사본 탭은 저장하지 않는다: %+v", tabs)
	}
}

// ─── 저장소 기록 ───

func TestSource_트리_미리_보기도_질의_글로_기록하고_사본을_남긴다(t *testing.T) {
	f := newFake()
	f.children[""] = []source.Node{{Name: "app", Path: "app/api", Openable: true}}
	registerFake(t, f)
	t.Setenv("XDG_STATE_HOME", t.TempDir())
	m := sourceModel(t)
	attachHistory(t, m)
	ln := expandConn(t, m, 1) // Loki
	m.selectNode(childNamed(ln, "app"))
	_, cmd := m.Update(key("enter"))
	drive(m, cmd)
	e := lastEntry(t, m)
	if e.Conn != "logs-stg" || e.SQL != `{app="api"}` || e.Rows != 2 || !e.Result {
		t.Fatalf("Loki 미리 보기 기록: %+v", e)
	}

	// prod Prometheus 지표 미리 보기는 기록하되 사본은 남기지 않는다
	f.children[""] = []source.Node{{Name: "up", Path: "up", Openable: true}}
	pn := expandConn(t, m, 2)
	m.selectNode(childNamed(pn, "up"))
	_, cmd = m.Update(key("enter"))
	drive(m, cmd)
	e = lastEntry(t, m)
	if e.Conn != "metrics" || e.SQL != "up" || e.Result {
		t.Fatalf("Prometheus 미리 보기 기록: %+v", e)
	}
}

// ─── db history ───

func TestHistoryCLI_목록은_접속_기간으로_거르고_show_는_사본을_찍는다(t *testing.T) {
	m, _ := historyModel(t)
	finishRun(t, m, 1, "select id, name from orders", table(2))
	old := time.Now().Add(-48 * time.Hour)
	must(t, m.hist.Append(history.Entry{ID: history.NewID(old), Time: old, Conn: "x-prod", SQL: "select old"}))
	must(t, m.hist.Append(history.Entry{ID: history.NewID(time.Now()), Time: time.Now(), Conn: "x-prod", SQL: db.MaskedSQL}))

	run := func(args ...string) (int, string, string) {
		var out, errb bytes.Buffer
		code := runCLI(context.Background(), append([]string{"history"}, args...), nil, true, &out, &errb, false)
		return code, out.String(), errb.String()
	}
	code, out, _ := run("--format", "json")
	var list []jsonHistory
	if code != 0 || json.Unmarshal([]byte(out), &list) != nil || len(list) != 3 {
		t.Fatalf("json: %d %s", code, out)
	}
	if list[2].SQL != "select id, name from orders" || !list[2].Result || list[2].Conn != "x-stg" || list[2].ElapsedMS != 120 || list[0].SQL != db.MaskedSQL {
		t.Fatalf("목록: %+v", list)
	}
	if code, out, _ = run("--conn", "x-prod", "--since", "24h"); code != 0 || strings.Count(out, "\n") != 2 || !strings.Contains(out, db.MaskedSQL) {
		t.Fatalf("tsv 거르기: %d %q", code, out)
	}
	if code, out, _ = run("--format", "table"); code != 0 || !strings.Contains(out, "◆") || !strings.Contains(out, "(3행)") {
		t.Fatalf("table: %d %s", code, out)
	}

	code, out, _ = run("show", list[2].ID, "--format", "json")
	var shown struct {
		Conn string     `json:"conn"`
		Cols []string   `json:"cols"`
		Rows [][]string `json:"rows"`
	}
	if code != 0 || json.Unmarshal([]byte(out), &shown) != nil || shown.Conn != "x-stg" || len(shown.Rows) != 2 || shown.Cols[1] != "name" {
		t.Fatalf("show: %d %s", code, out)
	}
	if code, _, errText := run("show", list[1].ID); code != 1 || !strings.Contains(errText, "결과 사본이 없음") {
		t.Fatalf("사본 없는 기록: %d %q", code, errText)
	}
	if code, _, _ := run("show", "../history"); code != 1 {
		t.Fatal("이름 꼴이 아니면 읽지 않는다")
	}
	if code, _, _ := run("show"); code != 2 {
		t.Fatal("id 가 없으면 사용법")
	}
	if code, _, _ := run("--format", "csv"); code != 2 {
		t.Fatal("csv 는 받지 않는다")
	}
}

// ─── 통합 (DBTUI_TEST_PG_URL 이 있을 때만) ───

func TestSnapshot_PG_실행_결과를_사본으로_남기고_기록_창에서_연다(t *testing.T) {
	state := t.TempDir()
	t.Setenv("XDG_STATE_HOME", state)
	m := pgModel(t, "hist-stg")
	attachHistory(t, m)
	m.cur().conn = 0
	drive(m, m.run("select g as n, 'v' || g as label from generate_series(1, 1500) g"))
	e := lastEntry(t, m)
	if !e.Result || e.Rows != 1000 {
		t.Fatalf("기록: %+v status=%q", e, m.status)
	}
	s, err := m.results.Read(e.ID)
	if err != nil || len(s.Rows) != 1000 || !s.ResultTruncated || s.Types[0] != "int4" || s.Rows[999][1] != "v1000" {
		t.Fatalf("사본: %d행 %+v %v", len(s.Rows), s.Types, err)
	}
	m.cur().editor.SetValue("x")
	openHistoryWindow(m, "generate_series")
	m.Update(key("enter"))
	if tab := m.cur(); tab.snap == nil || len(tab.res.Rows) != 1000 || !tab.res.Truncated {
		t.Fatal("사본 탭")
	}
}
