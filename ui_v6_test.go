package main

import (
	"errors"
	"os"
	"path/filepath"
	"regexp"
	"runtime"
	"slices"
	"strings"
	"testing"
	"time"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/Jeong-Jae-Hun/i-db/internal/session"
)

// writeQuery 는 프로젝트에 queries 파일을 쓰고 수정 시각을 mod 로 맞춘다.
func writeQuery(t *testing.T, m *model, rel, text string, mod time.Time) string {
	t.Helper()
	p := filepath.Join(m.proj.Dir, filepath.FromSlash(rel))
	must(t, os.MkdirAll(filepath.Dir(p), 0o755))
	must(t, os.WriteFile(p, []byte(text), 0o644))
	must(t, os.Chtimes(p, mod, mod))
	return p
}

func readFile(t *testing.T, p string) string {
	t.Helper()
	b, err := os.ReadFile(p)
	must(t, err)
	return string(b)
}

func labels(ns []*node) []string {
	out := make([]string, len(ns))
	for i, n := range ns {
		out[i] = n.label
	}
	return out
}

func motion(x, y int) tea.MouseMsg {
	return tea.MouseMsg{X: x, Y: y, Action: tea.MouseActionMotion, Button: tea.MouseButtonLeft}
}

func release(x, y int) tea.MouseMsg {
	return tea.MouseMsg{X: x, Y: y, Action: tea.MouseActionRelease, Button: tea.MouseButtonNone}
}

// latest 는 접속 uuid 의 최근 파일 맨 앞이다(R44).
func latest(m *model, uuid string) string {
	if rs := m.sess.Recent[uuid]; len(rs) > 0 {
		return rs[0].File
	}
	return ""
}

func savedState(t *testing.T, m *model) session.State {
	t.Helper()
	path, err := session.Path()
	must(t, err)
	st, ok, err := session.Load(path, m.proj.Dir)
	if err != nil || !ok {
		t.Fatalf("세션 읽기: %v %v", ok, err)
	}
	return st
}

// ─── R43 트리의 쿼리 파일 ───

func TestTreeQueries_접속_아래_최근_수정_순으로_보이고_안_붙은_파일은_맨_아래_묶음이다(t *testing.T) {
	t.Setenv("DB_AUTO_OPEN", "0")
	m := testModel(t)
	now := time.Now()
	writeQuery(t, m, "queries/old.sql", "select 1", now.Add(-2*time.Hour))
	writeQuery(t, m, "queries/sub/new.sql", "select 2", now.Add(-time.Hour))
	writeQuery(t, m, "queries/loose.sql", "select 3", now)
	m.proj.Mapping["queries/old.sql"] = "s"
	m.proj.Mapping["queries/sub/new.sql"] = "s"
	m.proj.Mapping["queries/gone.sql"] = "s" // 매핑만 있고 파일이 없으면 보이지 않는다
	m.refreshQueries()

	cn := m.connNode(1)
	m.setFocus(fTree)
	m.selectNode(cn)
	m.Update(key("enter"))
	m.Update(childrenMsg{n: cn, gen: cn.gen, children: []*node{{kind: nSchema, label: "public", conn: 1, parent: cn}}})
	q := cn.children[0]
	if q.kind != nQueries || q.label != "쿼리 (2)" || !slices.Equal(labels(q.children), []string{"sub/new.sql", "old.sql"}) {
		t.Fatalf("접속 아래 첫 줄은 최근 수정 순 쿼리 묶음: %q %q", q.label, labels(q.children))
	}
	if cn.children[1].label != "public" {
		t.Fatalf("스키마는 쿼리 묶음 뒤: %q", labels(cn.children))
	}
	if last := m.roots[len(m.roots)-1]; last.label != "쿼리 (접속 없음)" || !slices.Equal(labels(last.children), []string{"loose.sql"}) {
		t.Fatalf("안 붙은 파일은 맨 아래 묶음: %q %q", last.label, labels(last.children))
	}
	if pg := m.connNode(0); len(pg.children) != 0 {
		t.Fatalf("파일이 없는 접속(읽기 전)에는 묶음을 만들지 않는다: %q", labels(pg.children))
	}

	// 파일 줄 Enter 는 Ctrl+O 로 고른 것과 같다: 빈 탭에 열고 매핑 접속으로 바꾼다
	m.selectNode(q)
	m.Update(key("enter"))
	m.selectNode(q.children[1])
	m.Update(key("enter"))
	if m.cur().file != "queries/old.sql" || m.cur().conn != 1 || m.cur().editor.Value() != "select 1" || m.focus != fEditor || len(m.tabs) != 1 {
		t.Fatalf("빈 탭에 연다: file=%q conn=%d tabs=%d", m.cur().file, m.cur().conn, len(m.tabs))
	}
	m.setFocus(fTree)
	m.selectNode(q.children[0])
	m.Update(key("enter"))
	if len(m.tabs) != 2 || m.cur().file != "queries/sub/new.sql" {
		t.Fatalf("파일이 있는 탭이면 새 탭에 연다: tabs=%d file=%q", len(m.tabs), m.cur().file)
	}
	m.setFocus(fTree)
	m.selectNode(q.children[1])
	m.Update(key("enter"))
	if len(m.tabs) != 2 || m.tab != 0 || !strings.HasPrefix(m.status, "이미 열려 있음") {
		t.Fatalf("이미 열린 파일은 그 탭으로 간다: tabs=%d tab=%d status=%q", len(m.tabs), m.tab, m.status)
	}

	// r 은 목록을 다시 읽는다. 펼침 상태와 커서 줄은 이어받는다
	writeQuery(t, m, "queries/newest.sql", "select 4", now.Add(time.Minute))
	m.proj.Mapping["queries/newest.sql"] = "s"
	m.setFocus(fTree)
	m.selectNode(q.children[1])
	m.Update(key("r"))
	q = cn.children[0]
	if q.label != "쿼리 (3)" || !q.expanded || q.children[0].label != "newest.sql" || m.current().key != "queries/old.sql" {
		t.Fatalf("r 로 다시 읽기: %q %v %q cur=%q", q.label, q.expanded, labels(q.children), m.current().key)
	}
	m.filter = "newest"
	m.rebuild()
	if m.current() == nil || m.rows[len(m.rows)-1].n.key != "queries/newest.sql" {
		t.Fatal("트리 거르기는 파일 이름에도 걸린다")
	}
}

// ─── R44 최근 파일 자동 열기 ───

func TestAutoOpen_접속을_고르면_최근_기록_없으면_최근_수정_파일을_열고_쓰는_탭이_있으면_열지_않는다(t *testing.T) {
	m := testModel(t)
	now := time.Now()
	writeQuery(t, m, "queries/s-old.sql", "select 1", now.Add(-time.Hour))
	writeQuery(t, m, "queries/s-new.sql", "select 2", now)
	writeQuery(t, m, "queries/p1.sql", "select 3", now.Add(-time.Hour))
	writeQuery(t, m, "queries/p2.sql", "select 4", now)
	for f, c := range map[string]string{"queries/s-old.sql": "s", "queries/s-new.sql": "s", "queries/p1.sql": "p", "queries/p2.sql": "p"} {
		m.proj.Mapping[f] = c
	}
	m.refreshQueries()
	m.setFocus(fTree)

	// 기록이 없으면 붙은 파일 중 가장 늦게 고친 것을 빈 탭에 연다. 포커스는 트리에 둔다
	m.selectNode(m.connNode(1))
	m.Update(key("l"))
	if len(m.tabs) != 1 || m.cur().file != "queries/s-new.sql" || m.focus != fTree {
		t.Fatalf("빈 탭에 최근 수정 파일: tabs=%d file=%q focus=%v", len(m.tabs), m.cur().file, m.focus)
	}

	// 기록이 있으면 기록 순이다. 지워진 파일 기록은 건너뛴다. 글이 있는 탭이면 새 탭에 연다
	m.sess.Touch("p", "queries/p1.sql", now.Add(-time.Minute))
	m.sess.Touch("p", "queries/gone.sql", now)
	m.selectNode(m.connNode(0))
	m.Update(key("l"))
	if len(m.tabs) != 2 || m.cur().file != "queries/p1.sql" || m.cur().conn != 0 {
		t.Fatalf("기록 순으로 새 탭에: tabs=%d file=%q", len(m.tabs), m.cur().file)
	}

	// 그 접속을 쓰는 탭이 있으면 열지 않는다
	m.selectNode(m.connNode(0))
	m.Update(key("l"))
	if len(m.tabs) != 2 {
		t.Fatalf("쓰는 탭이 있으면 열지 않는다: tabs=%d", len(m.tabs))
	}

	// 붙은 파일이 없으면 빈 탭 그대로다
	m.Update(keyMsgOf("ctrl+t"))
	rn := addRedis(m)
	m.setFocus(fTree)
	m.selectNode(rn)
	m.Update(key("l"))
	if len(m.tabs) != 3 || m.cur().file != "" || m.cur().conn != rn.conn {
		t.Fatalf("붙은 파일이 없으면 빈 탭: tabs=%d file=%q", len(m.tabs), m.cur().file)
	}
}

func TestAutoOpen_DB_AUTO_OPEN이_0이면_열지_않는다(t *testing.T) {
	t.Setenv("DB_AUTO_OPEN", "0")
	m := testModel(t)
	writeQuery(t, m, "queries/a.sql", "select 1", time.Now())
	m.proj.Mapping["queries/a.sql"] = "s"
	m.refreshQueries()
	m.setFocus(fTree)
	m.selectNode(m.connNode(1))
	m.Update(key("l"))
	if m.cur().file != "" || m.cur().conn != 1 {
		t.Fatalf("열지 않고 접속만 바꾼다: file=%q conn=%d", m.cur().file, m.cur().conn)
	}
}

func TestAutoOpen_열고_실행하고_저장한_파일을_접속별_최근으로_세션에_남긴다(t *testing.T) {
	t.Setenv("XDG_STATE_HOME", t.TempDir())
	m := testModel(t)
	writeQuery(t, m, "queries/a.sql", "select 1", time.Now())
	writeQuery(t, m, "queries/b.sql", "select 2", time.Now())
	m.proj.Mapping["queries/a.sql"] = "s"
	m.proj.Mapping["queries/b.sql"] = "s"
	openQuery(m, "a.sql")
	openQuery(m, "b.sql")
	if f := latest(m, "s"); f != "queries/b.sql" {
		t.Fatalf("연 파일: %q", f)
	}
	m.switchTab(0)
	m.Update(keyMsgOf("ctrl+r"))
	if f := latest(m, "s"); f != "queries/a.sql" {
		t.Fatalf("실행한 파일: %q", f)
	}
	m.switchTab(1)
	m.Update(key("ctrl+s"))
	if f := latest(m, "s"); f != "queries/b.sql" {
		t.Fatalf("저장한 파일: %q", f)
	}
	t.Setenv("DB_RESTORE", "0") // 탭을 저장하지 않아도 최근 파일은 남긴다
	must(t, m.saveSession())
	if rs := savedState(t, m).Recent["s"]; len(rs) != 2 || rs[0].File != "queries/b.sql" {
		t.Fatalf("세션에 남긴다: %+v", rs)
	}
}

// ─── R45 새 쿼리 자동 저장 ───

func TestAutoSave_파일_없는_탭의_첫_실행은_queries에_저장하고_매핑을_붙이고_탭을_잇는다(t *testing.T) {
	m := testModel(t)
	m.proj.Conns[1].Group = "my grp"
	tab := m.cur()
	m.setConn(tab, 1)
	tab.editor.SetValue("select 1")
	m.setFocus(fEditor)
	m.Update(keyMsgOf("ctrl+r"))
	if !regexp.MustCompile(`^queries/my-grp/x-stg-\d{8}-\d{6}\.sql$`).MatchString(tab.file) {
		t.Fatalf("저장 경로: %q", tab.file)
	}
	if got := readFile(t, filepath.Join(m.proj.Dir, tab.file)); got != "select 1" {
		t.Fatalf("저장 내용: %q", got)
	}
	xml := readFile(t, filepath.Join(m.proj.Dir, ".idea", "data_source_mapping.xml"))
	if !strings.Contains(xml, `url="file://$PROJECT_DIR$/`+tab.file+`" value="s"`) || m.proj.Mapping[tab.file] != "s" {
		t.Fatalf("매핑: %s", xml)
	}
	if !strings.HasPrefix(m.status, "저장함: "+tab.file+" · 실행 중") {
		t.Fatalf("상태 줄에 경로: %q", m.status)
	}
	if !slices.Contains(m.qfiles[1], tab.file) {
		t.Fatalf("트리 목록도 다시 읽는다: %v", m.qfiles)
	}
	first := tab.file
	m.Update(keyMsgOf("ctrl+r"))
	if tab.file != first {
		t.Fatalf("두 번째 실행은 새 파일을 만들지 않는다: %q", tab.file)
	}

	// 접속이 없으면 실행도 저장도 하지 않는다
	nt := m.newTab()
	m.setConn(nt, -1)
	nt.editor.SetValue("select 2")
	m.Update(keyMsgOf("ctrl+r"))
	if nt.file != "" {
		t.Fatalf("접속 없는 탭은 저장하지 않는다: %q", nt.file)
	}
}

func TestAutoSave_끝낼_때_사용자가_친_접속_있는_SQL_탭만_새_쿼리로_저장한다(t *testing.T) {
	t.Setenv("XDG_STATE_HOME", t.TempDir())
	t.Setenv("DB_AUTO_OPEN", "0")
	m := testModel(t)
	rn := addRedis(m)
	m.setFocus(fEditor)

	typed := m.cur() // 친 글 · PostgreSQL 접속 → 저장
	m.setConn(typed, 1)
	typeText(m, "select 9")
	blank := m.newTab() // 공백만 친 탭 → 빼기
	typeText(m, " ")
	m.Update(ddlMsg{conn: 1, table: "users", ddl: "CREATE TABLE users ();"}) // DDL 로 넣은 글 → 빼기
	ddl := m.cur()
	noConn := m.newTab() // 접속 없는 탭 → 빼기
	m.setConn(noConn, -1)
	typeText(m, "select 8")
	redis := m.newTab() // Redis 명령은 SQL 파일이 아니다 → 빼기
	m.setConn(redis, rn.conn)
	typeText(m, "GET a")

	if _, cmd := m.Update(tea.KeyMsg{Type: tea.KeyCtrlC}); !isQuit(cmd) {
		t.Fatal("Ctrl+C 로 끝낸다")
	}
	if !regexp.MustCompile(`^queries/etc/x-stg-\d{8}-\d{6}\.sql$`).MatchString(typed.file) {
		t.Fatalf("친 글은 저장: %q", typed.file)
	}
	if got := readFile(t, filepath.Join(m.proj.Dir, typed.file)); got != "select 9" {
		t.Fatalf("저장 내용: %q", got)
	}
	for name, tab := range map[string]*queryTab{"공백": blank, "DDL": ddl, "접속 없음": noConn, "Redis": redis} {
		if tab.file != "" {
			t.Errorf("%s 탭은 저장하지 않는다: %q", name, tab.file)
		}
	}
	if tabs := savedTabs(t, m); tabs[0].File != typed.file || tabs[0].Text != "" || tabs[2].Text != "CREATE TABLE users ();" {
		t.Fatalf("세션은 파일을 가리키고 저장하지 않은 탭 글은 세션에 둔다: %+v", tabs)
	}
}

func TestAutoSave_되살릴_때_갈라_둔_저장_안_한_글은_끝낼_때_충돌_사본에_저장한다(t *testing.T) {
	t.Setenv("XDG_STATE_HOME", t.TempDir())
	m := testModel(t)
	writeQuery(t, m, "queries/a.sql", "select 2 -- git pull", time.Now())
	m.proj.Mapping["queries/a.sql"] = "s"
	path, err := session.Path()
	must(t, err)
	must(t, session.Save(path, m.proj.Dir, session.State{Tabs: []session.Tab{
		{Title: "a", File: "queries/a.sql", Conn: "s", Text: "select 1 -- 고침", Disk: session.DiskHash([]byte("select 1"))}}}))
	r := newModel(m.proj)
	r.restoreSession()
	u := r.tabs[1]
	if u.file != "" || u.editor.Value() != "select 1 -- 고침" {
		t.Fatalf("갈라 둔 탭: %q %q", u.file, u.editor.Value())
	}
	t.Setenv("DB_RESTORE", "0") // 세션에 남기지 않아도 글을 잃지 않는다
	must(t, r.saveSession())
	if !regexp.MustCompile(`^queries/a-conflict-\d{8}-\d{6}\.sql$`).MatchString(u.file) {
		t.Fatalf("충돌 사본 이름: %q", u.file)
	}
	if got := readFile(t, filepath.Join(m.proj.Dir, u.file)); got != "select 1 -- 고침" || r.proj.Mapping[u.file] != "s" {
		t.Fatalf("사본 내용·매핑: %q %q", got, r.proj.Mapping[u.file])
	}
	if got := readFile(t, filepath.Join(m.proj.Dir, "queries/a.sql")); got != "select 2 -- git pull" {
		t.Fatalf("원래 파일은 그대로: %q", got)
	}
}

func TestAutoSave_자동_저장을_멈춘_탭과_지워진_파일은_끝낼_때_충돌_사본에_남기고_알린다(t *testing.T) {
	t.Setenv("XDG_STATE_HOME", t.TempDir())
	t.Setenv("DB_RESTORE", "0") // 세션이 글을 들고 있지 않아도 잃지 않는다
	m := testModel(t)
	pa := writeQuery(t, m, "queries/a.sql", "select 1", time.Now().Add(-time.Hour))
	pb := writeQuery(t, m, "queries/b.sql", "select 2", time.Now().Add(-time.Hour))
	m.proj.Mapping["queries/a.sql"] = "s"
	openQuery(m, "a.sql")
	a := m.cur()
	a.editor.SetCursorOffset(len("select 1"))
	m.Update(key("0"))
	must(t, os.WriteFile(pa, []byte("select 1 -- 다른 편집기"), 0o644))
	m.Update(autoSaveMsg{tab: a.id, rev: a.editRev})
	if !a.autoStop {
		t.Fatal("밖에서 바뀌어 멈춘다")
	}
	openQuery(m, "b.sql")
	b := m.cur()
	b.editor.SetCursorOffset(len("select 2"))
	m.Update(key("0"))
	must(t, os.Remove(pb))

	must(t, m.saveSession())
	if got := readFile(t, pa); got != "select 1 -- 다른 편집기" {
		t.Fatalf("바뀐 파일은 덮지 않는다: %q", got)
	}
	if !strings.HasPrefix(a.file, "queries/a-conflict-") || readFile(t, filepath.Join(m.proj.Dir, a.file)) != "select 10" || m.proj.Mapping[a.file] != "s" {
		t.Fatalf("멈춘 탭은 충돌 사본: %q", a.file)
	}
	if !strings.HasPrefix(b.file, "queries/b-conflict-") || readFile(t, filepath.Join(m.proj.Dir, b.file)) != "select 20" {
		t.Fatalf("지워진 파일도 충돌 사본: %q", b.file)
	}
	if len(m.quitNotes) != 2 || !strings.Contains(m.quitNotes[0], a.file) || !strings.Contains(m.quitNotes[1], b.file) {
		t.Fatalf("화면을 닫은 뒤 알린다: %q", m.quitNotes)
	}
}

func TestAutoSave_파일_탭은_마지막_고침_뒤에_저장하고_이전_판의_알림은_버린다(t *testing.T) {
	m := testModel(t)
	p := writeQuery(t, m, "queries/a.sql", "select 1", time.Now().Add(-time.Hour))
	openQuery(m, "a.sql")
	tab := m.cur()
	tab.editor.SetCursorOffset(len("select 1"))
	if _, cmd := m.Update(key("0")); cmd == nil {
		t.Fatal("고치면 자동 저장 시각을 건다")
	}
	stale := tab.editRev
	m.Update(key("0"))
	m.Update(autoSaveMsg{tab: tab.id, rev: stale})
	if got := readFile(t, p); got != "select 1" {
		t.Fatalf("그사이 또 고쳤으면 이전 판 알림은 쓰지 않는다: %q", got)
	}
	m.Update(autoSaveMsg{tab: tab.id, rev: tab.editRev})
	if got := readFile(t, p); got != "select 100" {
		t.Fatalf("마지막 고침 뒤 저장: %q", got)
	}
	// 커서만 움직이면 판을 올리지 않는다
	rev := tab.editRev
	m.Update(tea.KeyMsg{Type: tea.KeyLeft})
	if tab.editRev != rev {
		t.Fatal("글이 그대로면 저장을 걸지 않는다")
	}
	// 탭을 옮길 때(새 탭 포함) 저장한다
	m.Update(key("1"))
	m.Update(keyMsgOf("ctrl+t"))
	if got := readFile(t, p); got != "select 1010" {
		t.Fatalf("새 탭으로 옮길 때 저장: %q", got)
	}
	m.Update(keyMsgOf("alt+1"))
	m.setFocus(fEditor)
	m.Update(key("2"))
	m.Update(keyMsgOf("ctrl+pgdown"))
	if got := readFile(t, p); got != "select 10120" {
		t.Fatalf("탭을 옮길 때 저장: %q", got)
	}
}

func TestAutoSave_밖에서_바뀐_파일은_자동_저장을_멈추고_CtrlS는_덮어쓴다(t *testing.T) {
	t.Setenv("XDG_STATE_HOME", t.TempDir())
	m := testModel(t)
	p := writeQuery(t, m, "queries/a.sql", "select 1", time.Now().Add(-time.Hour))
	openQuery(m, "a.sql")
	tab := m.cur()
	tab.editor.SetCursorOffset(len("select 1"))
	m.Update(key("0"))
	must(t, os.WriteFile(p, []byte("select 1 -- 다른 편집기"), 0o644))
	m.Update(autoSaveMsg{tab: tab.id, rev: tab.editRev})
	if got := readFile(t, p); got != "select 1 -- 다른 편집기" || m.status != autoSaveStopped || !tab.autoStop {
		t.Fatalf("덮지 않고 멈춘다: %q status=%q", got, m.status)
	}
	m.Update(keyMsgOf("ctrl+t"))
	m.Update(keyMsgOf("alt+1"))
	if got := readFile(t, p); got != "select 1 -- 다른 편집기" {
		t.Fatalf("탭을 옮겨도 덮지 않는다: %q", got)
	}
	m.Update(key("ctrl+s"))
	if got := readFile(t, p); got != "select 10" || tab.autoStop || m.status != "저장함: queries/a.sql" {
		t.Fatalf("Ctrl+S 는 덮어쓰고 자동 저장을 다시 켠다: %q status=%q", got, m.status)
	}
	m.setFocus(fEditor)
	m.Update(key("0"))
	m.Update(autoSaveMsg{tab: tab.id, rev: tab.editRev})
	if got := readFile(t, p); got != "select 100" {
		t.Fatalf("다시 자동 저장: %q", got)
	}
}

func TestAutoSave_같은_초_같은_크기로_밖에서_바뀐_것도_잡고_내용이_같은_touch는_멈추지_않는다(t *testing.T) {
	m := testModel(t)
	mod := time.Now().Add(-time.Hour).Truncate(time.Second)
	p := writeQuery(t, m, "queries/a.sql", "select 1", mod)
	openQuery(m, "a.sql")
	tab := m.cur()
	tab.editor.SetCursorOffset(len("select 1"))

	// 내용은 그대로 두고 수정 시각만 바꾼다(touch·git checkout)
	must(t, os.Chtimes(p, time.Now(), time.Now()))
	m.Update(key("0"))
	m.Update(autoSaveMsg{tab: tab.id, rev: tab.editRev})
	if got := readFile(t, p); got != "select 10" || tab.autoStop {
		t.Fatalf("내용이 같으면 자동 저장한다: %q stop=%v", got, tab.autoStop)
	}

	// 같은 크기·같은 수정 시각으로 내용만 바뀌어도 잡는다
	fi, err := os.Stat(p)
	must(t, err)
	must(t, os.WriteFile(p, []byte("select 99"), 0o644))
	must(t, os.Chtimes(p, fi.ModTime(), fi.ModTime()))
	m.Update(key("0"))
	m.Update(autoSaveMsg{tab: tab.id, rev: tab.editRev})
	if got := readFile(t, p); got != "select 99" || !tab.autoStop {
		t.Fatalf("같은 초·같은 크기 변경도 멈춘다: %q stop=%v", got, tab.autoStop)
	}
}

func TestAutoSave_고친_지_1초가_안_돼_탭을_닫아도_저장한다(t *testing.T) {
	m := testModel(t)
	p := writeQuery(t, m, "queries/a.sql", "select 1", time.Now().Add(-time.Hour))
	m.Update(keyMsgOf("ctrl+t"))
	openQuery(m, "a.sql")
	tab := m.cur()
	tab.editor.SetCursorOffset(len("select 1"))
	m.Update(key("0"))
	m.Update(keyMsgOf("ctrl+w"))
	if got := readFile(t, p); got != "select 10" || len(m.tabs) != 1 || m.tab != 0 {
		t.Fatalf("닫기 전에 저장: %q tabs=%d tab=%d", got, len(m.tabs), m.tab)
	}
	m.Update(autoSaveMsg{tab: tab.id, rev: tab.editRev}) // 닫은 탭의 늦은 알림은 버린다
}

func TestAutoSave_F8_정렬도_자동_저장을_건다(t *testing.T) {
	m := testModel(t)
	p := writeQuery(t, m, "queries/a.sql", "select 1 from t", time.Now().Add(-time.Hour))
	openQuery(m, "a.sql")
	tab := m.cur()
	_, cmd := m.Update(tea.KeyMsg{Type: tea.KeyF8})
	if cmd == nil || tab.editRev == 0 {
		t.Fatal("정렬로 글이 바뀌면 판을 올리고 저장 시각을 건다")
	}
	m.Update(autoSaveMsg{tab: tab.id, rev: tab.editRev})
	if got := readFile(t, p); got != tab.editor.Value() || got == "select 1 from t" {
		t.Fatalf("정렬한 글을 저장: %q", got)
	}
}

func TestAutoSave_되살리는_중에는_탭을_옮겨도_저장하지_않는다(t *testing.T) {
	t.Setenv("XDG_STATE_HOME", t.TempDir())
	m := testModel(t)
	p := writeQuery(t, m, "queries/a.sql", "select 1", time.Now().Add(-time.Hour))
	path, err := session.Path()
	must(t, err)
	must(t, session.Save(path, m.proj.Dir, session.State{Active: 1, Tabs: []session.Tab{
		{Title: "a", File: "queries/a.sql", Text: "select 1 -- 고침", Disk: session.DiskHash([]byte("select 1"))},
		{Title: "b", Text: "select 2"}}}))
	r := newModel(m.proj)
	r.restoreSession()
	if got := readFile(t, p); got != "select 1" || r.tab != 1 {
		t.Fatalf("되살리기만으로는 쓰지 않는다: %q tab=%d", got, r.tab)
	}
	r.switchTab(0)
	r.switchTab(1)
	if got := readFile(t, p); got != "select 1 -- 고침" {
		t.Fatalf("되살린 뒤 탭을 옮기면 쓴다: %q", got)
	}
}

func TestAutoSave_심볼릭_링크가_프로젝트_밖이면_쓰지_않고_목록에서도_뺀다(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("심볼릭 링크 권한이 필요하다")
	}
	m := testModel(t)
	outside := filepath.Join(t.TempDir(), "outside.sql")
	must(t, os.WriteFile(outside, []byte("select 1"), 0o644))
	writeQuery(t, m, "queries/in.sql", "select 2", time.Now())
	must(t, os.Symlink(outside, filepath.Join(m.proj.Dir, "queries", "out.sql")))
	must(t, os.Symlink(filepath.Join(m.proj.Dir, "queries", "in.sql"), filepath.Join(m.proj.Dir, "queries", "link.sql")))
	qs, err := listQueries(m.proj.Dir)
	must(t, err)
	var rels []string
	for _, q := range qs {
		rels = append(rels, q.rel)
	}
	slices.Sort(rels)
	if !slices.Equal(rels, []string{"queries/in.sql", "queries/link.sql"}) {
		t.Fatalf("프로젝트 밖 링크는 뺀다: %q", rels)
	}

	// 열어 둔 뒤 링크가 밖을 가리키게 바뀌어도 쓰지 않는다
	openQuery(m, "link.sql")
	tab := m.cur()
	link := filepath.Join(m.proj.Dir, "queries", "link.sql")
	must(t, os.Remove(link))
	must(t, os.Symlink(outside, link))
	m.stamp(tab, "select 1")
	tab.editor.SetCursorOffset(0)
	m.Update(key("-"))
	m.Update(autoSaveMsg{tab: tab.id, rev: tab.editRev})
	if got := readFile(t, outside); got != "select 1" || !tab.autoStop || m.status != autoSaveOutside {
		t.Fatalf("밖으로는 쓰지 않고 멈춘다: %q stop=%v status=%q", got, tab.autoStop, m.status)
	}

	// 안을 가리키는 링크는 링크를 일반 파일로 바꾸지 않고 실제 파일에 쓴다
	must(t, os.Remove(link))
	must(t, os.Symlink(filepath.Join(m.proj.Dir, "queries", "in.sql"), link))
	m.stamp(tab, "select 2")
	m.Update(key("-"))
	m.Update(autoSaveMsg{tab: tab.id, rev: tab.editRev})
	if fi, err := os.Lstat(link); err != nil || fi.Mode()&os.ModeSymlink == 0 || readFile(t, filepath.Join(m.proj.Dir, "queries", "in.sql")) != tab.editor.Value() {
		t.Fatalf("링크는 그대로, 실제 파일에 쓴다: %v", err)
	}
}

func TestQueries_읽지_못하는_폴더는_건너뛰고_나머지는_보인다(t *testing.T) {
	if runtime.GOOS == "windows" || os.Geteuid() == 0 {
		t.Skip("권한으로 읽기를 막을 수 없다")
	}
	m := testModel(t)
	writeQuery(t, m, "queries/a.sql", "select 1", time.Now())
	writeQuery(t, m, "queries/locked/b.sql", "select 2", time.Now())
	locked := filepath.Join(m.proj.Dir, "queries", "locked")
	must(t, os.Chmod(locked, 0))
	t.Cleanup(func() { os.Chmod(locked, 0o755) })
	qs, err := listQueries(m.proj.Dir)
	if err != nil || len(qs) != 1 || qs[0].rel != "queries/a.sql" {
		t.Fatalf("읽지 못하는 폴더만 건너뛴다: %+v %v", qs, err)
	}
}

func TestAutoSave_DB_AUTO_SAVE가_0이면_첫_실행_저장도_자동_저장도_하지_않는다(t *testing.T) {
	t.Setenv("XDG_STATE_HOME", t.TempDir())
	t.Setenv("DB_AUTO_SAVE", "0")
	m := testModel(t)
	m.setConn(m.cur(), 1)
	m.cur().editor.SetValue("select 1")
	m.Update(keyMsgOf("ctrl+r"))
	if m.cur().file != "" {
		t.Fatalf("첫 실행에 저장하지 않는다: %q", m.cur().file)
	}
	p := writeQuery(t, m, "queries/a.sql", "select 1", time.Now().Add(-time.Hour))
	openQuery(m, "a.sql")
	tab := m.cur()
	tab.editor.SetCursorOffset(len("select 1"))
	if m.Update(key("0")); tab.editRev != 0 {
		t.Fatal("자동 저장 시각을 걸지 않는다")
	}
	m.Update(autoSaveMsg{tab: tab.id, rev: 0})
	m.Update(keyMsgOf("alt+1"))
	if got := readFile(t, p); got != "select 1" {
		t.Fatalf("자동 저장하지 않는다: %q", got)
	}
	m.switchTab(1)
	m.Update(key("ctrl+s"))
	if got := readFile(t, p); got != "select 10" {
		t.Fatalf("Ctrl+S 는 쓴다: %q", got)
	}
	m.switchTab(0)
	must(t, m.saveSession())
	if m.tabs[0].file != "" {
		t.Fatalf("끝낼 때도 새 쿼리로 저장하지 않는다: %q", m.tabs[0].file)
	}
}

func TestAutoSave_되살린_저장_안_한_글은_디스크가_그대로일_때만_쓴다(t *testing.T) {
	t.Setenv("XDG_STATE_HOME", t.TempDir())
	m := testModel(t)
	p := writeQuery(t, m, "queries/a.sql", "select 1", time.Now().Add(-time.Hour))
	path, err := session.Path()
	must(t, err)
	must(t, session.Save(path, m.proj.Dir, session.State{Tabs: []session.Tab{
		{Title: "a", File: "queries/a.sql", Text: "select 1 -- 고침", Disk: session.DiskHash([]byte("select 1"))}}}))
	r := newModel(m.proj)
	r.restoreSession()
	r.Update(keyMsgOf("ctrl+t"))
	if got := readFile(t, p); got != "select 1 -- 고침" {
		t.Fatalf("디스크가 그대로면 되살린 글을 쓴다: %q", got)
	}

	// 디스크가 바뀌었으면 파일 탭은 디스크 내용, 저장 안 한 글은 새 탭(R41)이고 덮지 않는다
	must(t, session.Save(path, m.proj.Dir, session.State{Tabs: []session.Tab{
		{Title: "a", File: "queries/a.sql", Text: "select 2", Disk: session.DiskHash([]byte("select 1"))}}}))
	r = newModel(m.proj)
	r.restoreSession()
	r.switchTab(0)
	r.Update(keyMsgOf("ctrl+t"))
	if got := readFile(t, p); got != "select 1 -- 고침" || len(r.tabs) != 3 || r.tabs[1].editor.Value() != "select 2" {
		t.Fatalf("바뀐 디스크는 덮지 않는다: %q tabs=%d", got, len(r.tabs))
	}
}

// ─── R46 마우스로 창 크기 조절 ───

func TestMouseResize_경계를_끌면_범위_안에서_크기를_바꾸고_놓으면_저장한다(t *testing.T) {
	t.Setenv("XDG_STATE_HOME", t.TempDir())
	m := testModel(t) // 120x40
	l := m.layout()
	before := m.View()

	// 세로 경계: 트리 오른쪽 테두리를 잡고 끈다. 20칸~폭의 60%(72칸). 끄는 동안 경계를 강조한다
	m.Update(press(l.treeW+1, 5))
	if m.drag != bTree || m.View() == before {
		t.Fatalf("잡으면 경계를 강조한다: drag=%v", m.drag)
	}
	for _, c := range []struct{ x, want int }{{41, 40}, {1, minTreeWidth}, {119, 72}, {41, 40}} {
		m.Update(motion(c.x, 5))
		if m.treeW != c.want || m.layout().treeW != c.want {
			t.Fatalf("x=%d: treeW=%d want %d", c.x, m.treeW, c.want)
		}
	}
	m.Update(release(41, 5))
	if p := savedState(t, m).Panes; p.Tree != 40 {
		t.Fatalf("놓으면 저장: %+v", p)
	}
	m.Update(motion(50, 5))
	if m.treeW != 40 {
		t.Fatal("놓은 뒤 움직임은 크기를 바꾸지 않는다")
	}

	// 가로 경계: 편집기 아래 테두리(edH+3)를 끈다. 3줄~편집기·결과 몫(33) - 3줄
	l = m.layout()
	rightX := l.treeW + 2
	m.Update(press(rightX+5, l.edH+3))
	for _, c := range []struct{ y, want int }{{17, 14}, {0, minEditorLines}, {100, 30}, {17, 14}} {
		m.Update(motion(rightX+5, c.y))
		if m.edH != c.want || m.layout().edH != c.want {
			t.Fatalf("y=%d: edH=%d want %d", c.y, m.edH, c.want)
		}
	}
	m.Update(release(rightX+5, 17))
	if p := savedState(t, m).Panes; p.Tree != 40 || p.Editor != 14 {
		t.Fatalf("두 크기 모두 저장: %+v", p)
	}
	if got := strings.Count(m.cur().editor.View(), "\n") + 1; got != 14 {
		t.Fatalf("편집기도 따라간다: %d", got)
	}

	// 경계 밖 클릭은 그대로다
	m.setFocus(fEditor)
	m.Update(press(10, 3))
	if m.focus != fTree || m.drag != bNone {
		t.Fatalf("트리 클릭은 포커스: focus=%v drag=%v", m.focus, m.drag)
	}
}

func TestMouseResize_키보드는_반복_키_뒤_한_번만_저장한다(t *testing.T) {
	t.Setenv("XDG_STATE_HOME", t.TempDir())
	m := testModel(t)
	m.setFocus(fTree)
	var msgs []tea.Msg
	for range 3 {
		_, cmd := m.Update(tea.KeyMsg{Type: tea.KeyCtrlRight})
		if cmd == nil {
			t.Fatal("저장 시각을 건다")
		}
		msgs = append(msgs, cmd())
	}
	path, err := session.Path()
	must(t, err)
	if _, err := os.Stat(path); !os.IsNotExist(err) {
		t.Fatalf("키마다 쓰지 않는다: %v", err)
	}
	// 앞 판의 알림은 버리고 마지막 판의 알림만 쓴다
	for _, msg := range msgs[:2] {
		m.Update(msg)
	}
	if _, err := os.Stat(path); !os.IsNotExist(err) {
		t.Fatalf("앞 판 알림은 쓰지 않는다: %v", err)
	}
	m.Update(msgs[2])
	if p := savedState(t, m).Panes; p.Tree != defaultTreeWidth+3*treeWidthStep {
		t.Fatalf("마지막 판을 저장: %+v", p)
	}
}

func TestPanes_다음에_켜면_저장한_크기로_열고_DB_RESTORE가_0이어도_그렇다(t *testing.T) {
	for _, off := range []string{"", "0"} {
		t.Run("DB_RESTORE="+off, func(t *testing.T) {
			t.Setenv("XDG_STATE_HOME", t.TempDir())
			t.Setenv("DB_RESTORE", off)
			m := testModel(t)
			path, err := session.Path()
			must(t, err)
			must(t, session.Save(path, m.proj.Dir, session.State{Panes: session.Panes{Tree: 40, Editor: 14},
				Tabs: []session.Tab{{Title: "a", Text: "select 1"}}}))
			r := newModel(m.proj)
			r.restoreSession()
			r.Update(tea.WindowSizeMsg{Width: 120, Height: 40})
			if l := r.layout(); l.treeW != 40 || l.edH != 14 {
				t.Fatalf("저장한 크기: %+v", l)
			}
			if restored := r.cur().editor.Value() == "select 1"; restored != (off == "") {
				t.Fatalf("탭은 DB_RESTORE 를 따른다: %q", r.cur().editor.Value())
			}
			// 창이 작아졌으면 범위로 줄여 쓰고, 다시 키우면 원래 크기다
			r.Update(tea.WindowSizeMsg{Width: minWidth, Height: minHeight})
			if l := r.layout(); l.treeW != 36 || l.edH != 4 {
				t.Fatalf("작은 창: %+v", l)
			}
			r.Update(tea.WindowSizeMsg{Width: 120, Height: 40})
			if l := r.layout(); l.treeW != 40 || l.edH != 14 {
				t.Fatalf("다시 키운 창: %+v", l)
			}
		})
	}
}

// ─── R47 맥은 ⌘ 키가 기본 ───

func TestKeys_맥이면_키_안내_도움말_팔레트가_커맨드_글자이고_아니면_그대로다(t *testing.T) {
	m := testModel(t)
	m.Update(tea.WindowSizeMsg{Width: 200, Height: 120})
	m.setFocus(fEditor)
	if v := m.View(); !strings.Contains(v, "Ctrl+R 실행 · Ctrl+E 계획") {
		t.Fatal("맥이 아니면 Ctrl 그대로")
	}
	m.Update(keyMsgOf("ctrl+p"))
	if !slices.Contains(m.pickerItems, "새 탭 — Ctrl+T · 쿼리 탭") {
		t.Fatalf("팔레트: %q", m.pickerItems[:3])
	}
	m.Update(key("esc"))

	m.mac = true
	if v := m.View(); !strings.Contains(v, "⌘R 실행 · ⌘E 계획 · ⌘B 정의 · ⌥⌘L 정렬 · ⌘⇧M Auto/Manual · ⌘⇧O/⌘⇧S 즐겨찾기 · ⌘P 팔레트") {
		t.Fatal("상태 줄 키 안내가 ⌘")
	}
	m.Update(keyMsgOf("f1"))
	v := m.View()
	for _, want := range []string{"⌘R ", "⌘O / ⌘S", "⌘Z / ⌘⇧Z", "⌃← / ⌃→", "⌘1..9", "⌘⇧↩ / ⌘⇧⌫"} {
		if !strings.Contains(v, want) {
			t.Errorf("도움말에 %q 가 없다", want)
		}
	}
	if strings.Contains(v, "⌘R / ⌘R") || strings.Contains(v, "Ctrl+R") {
		t.Error("같은 ⌘ 키가 되는 대안은 하나로 줄이고 Ctrl 은 남기지 않는다")
	}
	m.Update(key("x"))
	m.Update(keyMsgOf("ctrl+p"))
	if !slices.Contains(m.pickerItems, "새 탭 — ⌘T · 쿼리 탭") || !slices.Contains(m.pickerItems, "실행: 선택한 글 전부, 없으면 커서 문장 — ⌘R · 전역") {
		t.Fatalf("팔레트도 ⌘: %q", m.pickerItems[:3])
	}
	m.Update(key("esc"))
	m.Update(key("ctrl+s"))
	if m.status != "연 파일이 없어 저장하지 않음 (⌘O 로 열기)" {
		t.Fatalf("키를 말하는 상태 줄도 ⌘: %q", m.status)
	}

	t.Setenv("DB_KEYS", "cmd")
	if !newModel(m.proj).mac {
		t.Fatal("DB_KEYS=cmd 면 ⌘")
	}
}

func TestKeys_맥_팔레트도_원래_키를_다시_누른다(t *testing.T) {
	m := testModel(t)
	m.mac = true
	m.setFocus(fEditor)
	palette(m, "새 탭 — ⌘T")
	if len(m.tabs) != 2 || m.overlay != oNone {
		t.Fatalf("⌘T 로 보여도 Ctrl+T 를 누른다: tabs=%d", len(m.tabs))
	}
	m.cur().conn = 1
	palette(m, "Auto / Manual (Manual")
	if !m.manual[1] {
		t.Fatalf("F9 를 누른다: status=%q", m.status)
	}
}

// ─── v6 리뷰 수정 ───

func TestMouseResize_버튼_없는_움직임이나_창이_뜨면_끌기를_끝내고_저장한다(t *testing.T) {
	t.Setenv("XDG_STATE_HOME", t.TempDir())
	m := testModel(t)
	l := m.layout()
	m.Update(press(l.treeW+1, 5))
	m.Update(motion(41, 5))
	// 창 밖에서 놓아 놓기가 오지 않고 버튼 없는 움직임이 온다
	m.Update(tea.MouseMsg{X: 50, Y: 5, Action: tea.MouseActionMotion, Button: tea.MouseButtonNone})
	if m.drag != bNone || savedState(t, m).Panes.Tree != 40 {
		t.Fatalf("끌기를 끝내고 저장: drag=%v", m.drag)
	}
	m.Update(motion(60, 5))
	if m.treeW != 40 {
		t.Fatalf("끝난 뒤 움직임은 크기를 바꾸지 않는다: %d", m.treeW)
	}

	// 끄는 중에 창(도움말)이 뜨면 다음 마우스에서 끌기를 끝낸다
	m.Update(press(m.layout().treeW+1, 5))
	m.Update(motion(45, 5))
	m.overlay = oHelp
	m.Update(release(45, 5))
	if m.drag != bNone || savedState(t, m).Panes.Tree != 44 {
		t.Fatalf("창이 뜨면 끌기를 끝낸다: drag=%v panes=%+v", m.drag, savedState(t, m).Panes)
	}
}

func TestMouseResize_작은_창에서_줄여_그린_크기로_저장한_크기를_덮지_않는다(t *testing.T) {
	t.Setenv("XDG_STATE_HOME", t.TempDir())
	m := testModel(t)
	m.treeW, m.edH = 40, 14
	m.Update(tea.WindowSizeMsg{Width: minWidth, Height: minHeight})
	l := m.layout()
	if l.treeW != 36 || l.edH != 4 {
		t.Fatalf("작은 창: %+v", l)
	}
	// 범위 끝이라 그려진 크기가 그대로인 키는 저장한 크기를 건드리지 않는다
	m.setFocus(fTree)
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlRight})
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlDown})
	if m.treeW != 40 || m.edH != 14 {
		t.Fatalf("키: treeW=%d edH=%d", m.treeW, m.edH)
	}
	// 끌다가 처음 그려진 자리로 돌아와 놓으면 저장한 크기 그대로다
	m.Update(press(l.treeW+1, 5))
	m.Update(motion(l.treeW-1, 5))
	m.Update(motion(l.treeW+1, 5))
	m.Update(release(l.treeW+1, 5))
	if p := savedState(t, m).Panes; p.Tree != 40 || p.Editor != 14 {
		t.Fatalf("저장한 크기: %+v", p)
	}
	m.Update(tea.WindowSizeMsg{Width: 120, Height: 40})
	if l := m.layout(); l.treeW != 40 || l.edH != 14 {
		t.Fatalf("다시 키운 창: %+v", l)
	}
}

func TestAutoOpen_스키마_줄을_고르면_열지_않고_접속_줄만_연다(t *testing.T) {
	m := testModel(t)
	writeQuery(t, m, "queries/a.sql", "select 1", time.Now())
	m.proj.Mapping["queries/a.sql"] = "s"
	m.refreshQueries()
	addTable(m, 1)
	schema := m.connNode(1).children[len(m.connNode(1).children)-1]
	if schema.kind != nSchema {
		t.Fatalf("스키마 줄: %v", schema.kind)
	}
	schema.expanded = false // 접힌 스키마를 펼치는 것도 접속을 고른 게 아니다
	m.setFocus(fTree)
	m.selectNode(schema)
	m.Update(key("enter"))
	if m.cur().file != "" || m.cur().conn != 1 {
		t.Fatalf("스키마 줄은 접속만 바꾼다: file=%q conn=%d", m.cur().file, m.cur().conn)
	}
	m.Update(keyMsgOf("ctrl+t"))
	m.setConn(m.tabs[0], -1)
	m.setConn(m.cur(), -1)
	m.setFocus(fTree)
	m.selectNode(m.connNode(1))
	m.Update(key("l"))
	if m.cur().file != "queries/a.sql" {
		t.Fatalf("접속 줄은 연다: %q", m.cur().file)
	}
}

func TestOpenFile_결과가_있는_탭은_빈_탭이_아니라_새_탭에_연다(t *testing.T) {
	m := testModel(t)
	writeQuery(t, m, "queries/a.sql", "select 1", time.Now())
	m.cur().conn = 1
	show(m, editable())
	first := m.cur()
	openQuery(m, "a.sql")
	if len(m.tabs) != 2 || first.file != "" || first.res == nil || m.cur().file != "queries/a.sql" {
		t.Fatalf("결과를 덮지 않는다: tabs=%d first=%q", len(m.tabs), first.file)
	}
	// 공백만 있는 탭은 빈 탭이다
	m.Update(keyMsgOf("ctrl+t"))
	m.cur().editor.SetValue(" \n")
	writeQuery(t, m, "queries/b.sql", "select 2", time.Now())
	openQuery(m, "b.sql")
	if len(m.tabs) != 3 || m.cur().file != "queries/b.sql" {
		t.Fatalf("공백 탭에 연다: tabs=%d", len(m.tabs))
	}
}

func TestKeys_맥_안내는_탭_제목을_키로_바꾸지_않는다(t *testing.T) {
	m := testModel(t)
	m.mac = true
	m.cur().conn = 1
	m.cur().title = "Ctrl+R"
	show(m, editable())
	m.setFocus(fResult)
	m.Update(key("l"))
	editCell(m, "q")
	got := m.dbSwitchBlock(1)
	if !strings.HasPrefix(got, "Ctrl+R 탭의 변경 1건") || !strings.Contains(got, "⌘S") {
		t.Fatalf("제목은 그대로, 안내 키만 ⌘: %q", got)
	}
}

func TestQueries_CtrlO_목록과_트리는_같은_최근_수정_순이고_자동_저장한_뒤_탭을_옮기면_순서가_바뀐다(t *testing.T) {
	m := testModel(t)
	now := time.Now()
	writeQuery(t, m, "queries/a.sql", "select 1", now.Add(-time.Hour))
	writeQuery(t, m, "queries/b.sql", "select 2", now)
	m.refreshQueries()
	if got := m.qfiles[-1]; !slices.Equal(got, []string{"queries/b.sql", "queries/a.sql"}) {
		t.Fatalf("트리: %q", got)
	}
	m.Update(key("ctrl+o"))
	if !slices.Equal(m.pickerItems, []string{"queries/b.sql", "queries/a.sql"}) {
		t.Fatalf("Ctrl+O: %q", m.pickerItems)
	}
	m.Update(tea.KeyMsg{Type: tea.KeyEsc})
	openQuery(m, "a.sql")
	tab := m.cur()
	m.Update(key("-"))
	m.Update(autoSaveMsg{tab: tab.id, rev: tab.editRev})
	if got := readFile(t, filepath.Join(m.proj.Dir, "queries/a.sql")); got != "-select 1" {
		t.Fatalf("자동 저장: %q", got)
	}
	if got := m.qfiles[-1]; !slices.Equal(got, []string{"queries/b.sql", "queries/a.sql"}) {
		t.Fatalf("자동 저장마다 queries 를 훑지 않는다: %q", got)
	}
	m.Update(keyMsgOf("ctrl+t"))
	if got := m.qfiles[-1]; !slices.Equal(got, []string{"queries/a.sql", "queries/b.sql"}) {
		t.Fatalf("탭을 옮기면 다시 읽어 자동 저장한 파일이 맨 앞: %q", got)
	}
}

// ─── 재리뷰 지적: 닫기·끝내기·Ctrl+S ───

func TestCloseTab_자동_저장을_멈춘_탭과_지워진_파일은_닫을_때_충돌_사본에_남기고_경로를_보인다(t *testing.T) {
	m := testModel(t)
	pa := writeQuery(t, m, "queries/a.sql", "select 1", time.Now().Add(-time.Hour))
	pb := writeQuery(t, m, "queries/b.sql", "select 2", time.Now().Add(-time.Hour))
	openQuery(m, "a.sql")
	a := m.cur()
	a.editor.SetCursorOffset(len("select 1"))
	m.Update(key("0"))
	must(t, os.WriteFile(pa, []byte("select 1 -- 다른 편집기"), 0o644))
	m.Update(keyMsgOf("ctrl+w"))
	if got := readFile(t, pa); got != "select 1 -- 다른 편집기" {
		t.Fatalf("바뀐 파일은 덮지 않는다: %q", got)
	}
	if !strings.HasPrefix(a.file, "queries/a-conflict-") || readFile(t, filepath.Join(m.proj.Dir, a.file)) != "select 10" || !strings.Contains(m.status, a.file) {
		t.Fatalf("충돌 사본에 남기고 알린다: %q status=%q", a.file, m.status)
	}
	if slices.Contains(m.tabs, a) {
		t.Fatal("탭은 닫는다")
	}

	openQuery(m, "b.sql")
	b := m.cur()
	b.editor.SetCursorOffset(len("select 2"))
	m.Update(key("0"))
	must(t, os.Remove(pb))
	m.Update(keyMsgOf("ctrl+w"))
	if !strings.HasPrefix(b.file, "queries/b-conflict-") || readFile(t, filepath.Join(m.proj.Dir, b.file)) != "select 20" || !strings.Contains(m.status, b.file) {
		t.Fatalf("지워진 파일도 충돌 사본: %q status=%q", b.file, m.status)
	}
}

func TestCloseTab_파일_없는_탭에_친_글은_닫을_때_저장하고_저장할_수_없으면_묻는다(t *testing.T) {
	m := testModel(t)
	rn := addRedis(m)
	m.setFocus(fEditor)
	typed := m.cur()
	m.setConn(typed, 1)
	m.Update(keyMsgOf("ctrl+t")) // 마지막 탭을 닫으면 새 빈 탭이 생기므로 하나 더 둔다
	m.switchTab(0)
	typeText(m, "select 9")
	m.Update(keyMsgOf("ctrl+w"))
	if !regexp.MustCompile(`^queries/etc/x-stg-\d{8}-\d{6}\.sql$`).MatchString(typed.file) || slices.Contains(m.tabs, typed) {
		t.Fatalf("친 글은 새 쿼리로 저장하고 닫는다: %q", typed.file)
	}
	if got := readFile(t, filepath.Join(m.proj.Dir, typed.file)); got != "select 9" || m.status != "저장함: "+typed.file {
		t.Fatalf("저장 내용·상태: %q status=%q", got, m.status)
	}

	redis := m.cur()
	m.setConn(redis, rn.conn)
	typeText(m, "GET a")
	m.Update(keyMsgOf("ctrl+w"))
	if m.overlay != oConfirm || m.yes.q != "저장하지 않은 글이 있는 탭을 닫을까요" || !slices.Contains(m.tabs, redis) {
		t.Fatalf("Redis 탭은 저장하지 않으니 묻는다: overlay=%v", m.overlay)
	}
	m.Update(key("n"))
	if !slices.Contains(m.tabs, redis) {
		t.Fatal("n 이면 닫지 않는다")
	}
	m.Update(keyMsgOf("ctrl+w"))
	m.Update(key("y"))
	if slices.Contains(m.tabs, redis) || redis.file != "" {
		t.Fatalf("y 면 저장하지 않고 닫는다: %q", redis.file)
	}

	// 친 글이 없거나 공백뿐이면 묻지 않는다
	m.Update(keyMsgOf("ctrl+t"))
	m.setFocus(fEditor)
	blank := m.cur()
	typeText(m, " ")
	m.Update(keyMsgOf("ctrl+w"))
	if m.overlay != oNone || slices.Contains(m.tabs, blank) {
		t.Fatalf("공백뿐이면 바로 닫는다: overlay=%v", m.overlay)
	}
}

func TestQuit_DB_RESTORE가_0이면_파일에도_세션에도_남지_않은_탭_수를_알린다(t *testing.T) {
	t.Setenv("XDG_STATE_HOME", t.TempDir())
	t.Setenv("DB_RESTORE", "0")
	m := testModel(t)
	m.setFocus(fEditor)
	m.setConn(m.cur(), 1)
	typeText(m, "select 9")                                                  // 친 글 · 접속 있음 → 파일에 저장
	m.Update(ddlMsg{conn: 1, table: "users", ddl: "CREATE TABLE users ();"}) // DDL 로 넣은 글 → 남지 않음
	m.newTab()
	m.setConn(m.cur(), -1)
	typeText(m, "select 8") // 접속 없음 → 남지 않음
	m.newTab()              // 빈 탭 → 셀 것이 없음

	must(t, m.saveSession())
	if !slices.Contains(m.quitNotes, "2 tabs' text was not saved (DB_RESTORE=0)") {
		t.Fatalf("남지 않은 탭 2개를 알린다: %q", m.quitNotes)
	}
}

func TestCtrlS_지워진_파일은_다시_만들고_알린다(t *testing.T) {
	m := testModel(t)
	p := writeQuery(t, m, "queries/sub/a.sql", "select 1", time.Now().Add(-time.Hour))
	openQuery(m, "a.sql")
	tab := m.cur()
	tab.editor.SetCursorOffset(len("select 1"))
	m.Update(key("0"))
	must(t, os.RemoveAll(filepath.Dir(p)))
	m.Update(key("ctrl+s"))
	if got := readFile(t, p); got != "select 10" || m.status != "지워진 파일을 다시 만듦: queries/sub/a.sql" || tab.autoStop {
		t.Fatalf("다시 만든다: %q status=%q", got, m.status)
	}
	if fi, err := os.Stat(p); err != nil || fi.Mode().Perm() != 0o644 {
		t.Fatalf("권한 0644: %v", err)
	}
}

func TestWriteNew_부모가_프로젝트_밖을_가리키는_링크면_밖에_폴더를_만들지_않는다(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("심볼릭 링크 권한이 필요하다")
	}
	m := testModel(t)
	outside := t.TempDir()
	must(t, os.MkdirAll(filepath.Join(m.proj.Dir, "queries"), 0o755))
	must(t, os.Symlink(outside, filepath.Join(m.proj.Dir, "queries", "out")))
	tab := m.cur()
	tab.editor.SetValue("select 1")
	if err := m.writeNew(tab, "queries/out/sub/a.sql", ""); !errors.Is(err, errOutside) {
		t.Fatalf("밖이면 쓰지 않는다: %v", err)
	}
	if _, err := os.Stat(filepath.Join(outside, "sub")); !os.IsNotExist(err) {
		t.Fatalf("밖에 폴더를 만들지 않는다: %v", err)
	}
}

func TestQuit_충돌_사본을_쓰지_못하면_세션_저장_실패와_따로_알린다(t *testing.T) {
	if runtime.GOOS == "windows" || os.Geteuid() == 0 {
		t.Skip("폴더 쓰기 권한으로 실패를 만든다")
	}
	t.Setenv("XDG_STATE_HOME", t.TempDir())
	m := testModel(t)
	p := writeQuery(t, m, "queries/a.sql", "select 1", time.Now().Add(-time.Hour))
	openQuery(m, "a.sql")
	tab := m.cur()
	tab.editor.SetCursorOffset(len("select 1"))
	m.Update(key("0"))
	must(t, os.Remove(p))
	dir := filepath.Dir(p)
	must(t, os.Chmod(dir, 0o555))
	t.Cleanup(func() { os.Chmod(dir, 0o755) })

	if err := m.saveSession(); err != nil {
		t.Fatalf("세션 저장은 성공이다: %v", err)
	}
	if len(m.quitNotes) != 1 || !strings.HasPrefix(m.quitNotes[0], "could not save "+tab.title+": ") {
		t.Fatalf("쓰지 못한 탭을 따로 알린다: %q", m.quitNotes)
	}
}

func TestCtrlS_실제_경로가_프로젝트_밖이면_쓰지_않고_끝낼_때_충돌_사본으로_남긴다고_알린다(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("심볼릭 링크 권한이 필요하다")
	}
	m := testModel(t)
	outside := filepath.Join(t.TempDir(), "outside.sql")
	must(t, os.WriteFile(outside, []byte("select 1"), 0o644))
	writeQuery(t, m, "queries/in.sql", "select 2", time.Now())
	link := filepath.Join(m.proj.Dir, "queries", "link.sql")
	must(t, os.Symlink(filepath.Join(m.proj.Dir, "queries", "in.sql"), link))
	openQuery(m, "link.sql")
	tab := m.cur()
	must(t, os.Remove(link))
	must(t, os.Symlink(outside, link))
	m.Update(key("-"))
	m.Update(key("ctrl+s"))
	if got := readFile(t, outside); got != "select 1" || !tab.autoStop || m.status != autoSaveOutside+" (끝낼 때 충돌 사본으로 남김)" {
		t.Fatalf("쓰지 않고 알린다: %q stop=%v status=%q", got, tab.autoStop, m.status)
	}
}

func TestMouseLeak_글자로_새어_든_마우스_보고는_편집기에_넣지_않는다(t *testing.T) {
	m := testModel(t)
	m.setFocus(fEditor)
	tab := m.cur()
	tab.editor.SetValue("select 1")
	m.Update(tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("<64;84;23M<65;63;31M")})
	m.Update(tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("[<64;84;23M")})
	if got := tab.editor.Value(); got != "select 1" {
		t.Fatalf("마우스 조각이 들어감: %q", got)
	}
	m.Update(tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("<64;84;23M"), Paste: true})
	if got := tab.editor.Value(); !strings.Contains(got, "<64;84;23M") {
		t.Fatalf("붙여넣기는 그대로 넣는다: %q", got)
	}
}

func TestKittyTerminal_감쌀_터미널만_고른다(t *testing.T) {
	env := func(kv map[string]string) func(string) string { return func(k string) string { return kv[k] } }
	cases := []struct {
		kv   map[string]string
		want bool
	}{
		{map[string]string{"DB_KITTY_KEYS": "1", "WT_SESSION": "x"}, true},
		{map[string]string{"DB_KITTY_KEYS": "0", "TERM_PROGRAM": "ghostty"}, false},
		{map[string]string{"WT_SESSION": "abc", "TERM_PROGRAM": "ghostty"}, false},
		{map[string]string{"TERM_PROGRAM": "ghostty"}, true},
		{map[string]string{"TERM": "xterm-kitty"}, true},
	}
	for _, c := range cases {
		if got := kittyTerminal(env(c.kv)); got != c.want {
			t.Errorf("%v = %v, want %v", c.kv, got, c.want)
		}
	}
	if runtime.GOOS != "darwin" && kittyTerminal(env(map[string]string{"TERM": "xterm-256color"})) {
		t.Error("맥이 아닌 보통 터미널은 감싸지 않는다")
	}
}
