package main

import (
	"bytes"
	"context"
	"encoding/base64"
	"errors"
	"fmt"
	"image"
	"io"
	"net/url"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"testing"
	"time"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"

	"github.com/Jeong-Jae-Hun/i-db/internal/complete"
	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/editor"
	"github.com/Jeong-Jae-Hun/i-db/internal/history"
)

func testModel(t *testing.T) *model {
	t.Helper()
	p := &db.Project{Dir: t.TempDir(), Mapping: map[string]string{}, Conns: []db.Conn{
		{Name: "x-prod", UUID: "p", Kind: db.KindPG, Host: "127.0.0.1", Port: 1},
		{Name: "x-stg", UUID: "s", Kind: db.KindPG, Host: "127.0.0.1", Port: 1},
	}}
	m := newModel(p)
	m.prodLock = false // v6 까지의 prod 확인 시험은 잠금(R54) 없이 본다. 잠금 시험은 다시 켠다
	m.Update(tea.WindowSizeMsg{Width: 120, Height: 40})
	t.Cleanup(func() { _ = m.recentSaver.flush(5 * time.Second) }) // 화면 밖 기억 쓰기가 임시 폴더 정리와 겹치지 않게
	return m
}

func TestGuard_prod_쓰기는_확인을_기다리고_N이면_실행하지_않는다(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 0
	// 돌려주는 명령은 영향 행 수 추정(R39)이고 실행은 아니다
	if m.run("delete from t"); m.overlay != oConfirm || m.pending == nil || m.cur().running != nil {
		t.Fatalf("prod 쓰기는 확인 단계여야 한다: overlay=%v", m.overlay)
	}
	m.Update(tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("n")})
	if m.overlay != oNone || m.pending != nil || m.cur().running != nil || m.status != "취소함" {
		t.Fatalf("N 이면 실행하지 않아야 한다: status=%q", m.status)
	}

	m.run("update t set a=1")
	_, cmd := m.Update(tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("y")})
	if cmd == nil || m.cur().running == nil {
		t.Fatal("y 면 실행 명령을 돌려줘야 한다")
	}
}

func TestGuard_prod_읽기와_stg_쓰기는_바로_실행한다(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 0
	if cmd := m.run("select 1"); cmd == nil || m.overlay != oNone {
		t.Fatal("prod 읽기는 바로 실행")
	}
	m.cur().conn = 1
	if cmd := m.run("delete from t where id = 1"); cmd == nil || m.overlay != oNone {
		t.Fatal("stg 쓰기는 바로 실행")
	}
}

func TestCtrlC_쓰기_취소는_반영_가능성을_알린다(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 1
	m.run("delete from t where id = 1")
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlC})
	if m.cur().running != nil || !strings.Contains(m.status, "이미 반영됐을 수 있음") {
		t.Fatalf("status=%q", m.status)
	}
}

func key(s string) tea.KeyMsg {
	switch s {
	case "enter":
		return tea.KeyMsg{Type: tea.KeyEnter}
	case "ctrl+o":
		return tea.KeyMsg{Type: tea.KeyCtrlO}
	case "ctrl+s":
		return tea.KeyMsg{Type: tea.KeyCtrlS}
	}
	return tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune(s)}
}

func TestTree_테이블_Enter_는_테이블_보기를_열고_추정치부터_읽는다(t *testing.T) {
	m := testModel(t)
	stg := m.connNode(1)
	schema := &node{kind: nSchema, label: "pub\"lic", conn: 1, parent: stg, loaded: true, expanded: true}
	schema.children = []*node{{kind: nTable, label: "users", schema: "pub\"lic", conn: 1, parent: schema}}
	stg.children, stg.loaded, stg.expanded = []*node{schema}, true, true
	m.rebuild()
	m.selectNode(schema.children[0])
	_, cmd := m.Update(key("enter"))
	tab := m.cur()
	if tab.tv == nil || cmd == nil || tab.conn != 1 || tab.title != "users" {
		t.Fatalf("테이블 보기: tv=%v active=%d cmd=%v", tab.tv != nil, tab.conn, cmd != nil)
	}
	if want := `SELECT * FROM "pub""lic"."users" LIMIT 200 OFFSET 0`; tab.tv.SQL() != want {
		t.Fatalf("SQL=%q", tab.tv.SQL())
	}
}

func TestTree_거르기는_이름으로_Redis_키_단계는_SCAN_패턴으로(t *testing.T) {
	m := testModel(t)
	m.Update(key("/"))
	for _, r := range "stg" {
		m.Update(key(string(r)))
	}
	m.Update(key("enter"))
	labels := []string{}
	for _, r := range m.rows {
		labels = append(labels, r.n.label)
	}
	if strings.Join(labels, ",") != "기타,x-stg" {
		t.Fatalf("거르기 결과: %v", labels)
	}

	m.proj.Conns = append(m.proj.Conns, db.Conn{Name: "r-stg", UUID: "r", Kind: db.KindRedis, Host: "127.0.0.1", Port: 1})
	rn := &node{kind: nConn, label: "r-stg", conn: 2, parent: m.roots[0]}
	m.roots[0].children = append(m.roots[0].children, rn)
	m.filter = ""
	m.rebuild()
	m.selectNode(rn)
	m.Update(key("/"))
	for _, r := range "session:*" {
		m.Update(key(string(r)))
	}
	_, cmd := m.Update(key("enter"))
	if m.patterns[2] != "session:*" || cmd == nil || rn.gen != 1 {
		t.Fatalf("pattern=%q gen=%d", m.patterns[2], rn.gen)
	}
}

func TestFile_열면_매핑된_접속이_활성이_되고_저장은_연_파일에만(t *testing.T) {
	m := testModel(t)
	dir := m.proj.Dir
	must(t, os.MkdirAll(filepath.Join(dir, "queries", "x"), 0o755))
	f := filepath.Join(dir, "queries", "x", "q.sql")
	must(t, os.WriteFile(f, []byte("select 1;"), 0o600))
	m.proj.Mapping["queries/x/q.sql"] = "s"

	m.Update(key("ctrl+s"))
	if !strings.Contains(m.status, "저장하지 않음") {
		t.Fatalf("연 파일 없이 저장하면 안 된다: %q", m.status)
	}
	m.Update(key("ctrl+o"))
	for _, r := range "q.sql" {
		m.Update(key(string(r)))
	}
	m.Update(key("enter"))
	if m.cur().file != "queries/x/q.sql" || m.cur().conn != 1 || m.cur().editor.Value() != "select 1;" || m.focus != fEditor {
		t.Fatalf("file=%q active=%d value=%q", m.cur().file, m.cur().conn, m.cur().editor.Value())
	}
	m.cur().editor.SetValue("select 2;")
	m.Update(key("ctrl+s"))
	b, err := os.ReadFile(f)
	must(t, err)
	st, err := os.Stat(f)
	must(t, err)
	if string(b) != "select 2;" || st.Mode().Perm() != 0o600 {
		t.Fatalf("저장 내용·권한: %q %v", b, st.Mode().Perm())
	}
}

func TestUI_빈_목록에서_움직여도_패닉이_없다(t *testing.T) {
	m := testModel(t)
	m.filter = "없는이름"
	m.rebuild()
	m.Update(key("j"))
	m.Update(key("enter"))
	show(m, &db.Result{Cols: []string{"a"}})
	m.setFocus(fResult)
	m.Update(key("j"))
	m.Update(key("enter"))
	m.Update(key("ctrl+o"))
	for _, r := range "zzz" {
		m.Update(key(string(r)))
	}
	m.Update(tea.KeyMsg{Type: tea.KeyDown})
	m.Update(key("enter"))
	_ = m.View()
}

func TestUI_작은_터미널은_안내만_보여_준다(t *testing.T) {
	m := testModel(t)
	for _, sz := range [][2]int{{10, 5}, {59, 40}, {120, 13}, {120, 14}} {
		m.Update(tea.WindowSizeMsg{Width: sz[0], Height: sz[1]})
		show(m, &db.Result{Text: "x\ny"})
		m.overlay = oHistory
		_ = m.View()
		m.overlay = oNone
		_ = m.View()
	}
}

func TestUI_취소_뒤_늦게_온_결과가_경고를_덮지_않는다(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 1
	m.run("delete from t where id = 1")
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlC})
	m.Update(resultMsg{tab: m.cur().id, seq: m.seq, conn: 1, err: context.Canceled})
	if !strings.Contains(m.status, "취소됨") || !strings.Contains(m.status, "이미 반영됐을 수 있음") {
		t.Fatalf("status=%q", m.status)
	}
}

func TestUI_오류의_비밀번호와_제어문자는_화면에_나오지_않는다(t *testing.T) {
	m := testModel(t)
	m.proj.Conns[1].Password = "p@ss/word"
	m.cur().conn = 1
	m.run("select 1")
	m.Update(resultMsg{tab: m.cur().id, seq: m.seq, conn: 1, err: errors.New("bad p@ss/word p%40ss%2Fword \x1b]52;c;evil\x07")})
	out := m.View()
	if strings.Contains(out, "p@ss/word") || strings.Contains(out, "p%40ss%2Fword") || strings.Contains(out, "\x1b]52") {
		t.Fatalf("노출: %q", m.cur().resErr)
	}
	if got := fmt.Sprintf("%+v", m.proj.Conns[1]); strings.Contains(got, "p@ss") {
		t.Fatalf("db.Conn 출력에 비밀번호: %s", got)
	}
}

func TestUI_패턴을_바꾸면_이전_SCAN_응답은_버린다(t *testing.T) {
	m := testModel(t)
	m.proj.Conns = append(m.proj.Conns, db.Conn{Name: "r-stg", UUID: "r", Kind: db.KindRedis, Host: "127.0.0.1", Port: 1})
	rn := &node{kind: nConn, label: "r-stg", conn: 2, parent: m.roots[0]}
	m.roots[0].children = append(m.roots[0].children, rn)
	m.expand(rn)
	oldGen := rn.gen
	m.patterns[2] = "new:*"
	rn.loaded = false
	m.expand(rn)
	m.Update(childrenMsg{n: rn, gen: oldGen, children: []*node{{kind: nKey, label: "old:1"}}})
	if rn.loaded || rn.children[0].label == "old:1" {
		t.Fatal("이전 세대 응답을 붙이면 안 된다")
	}
	m.Update(childrenMsg{n: rn, gen: rn.gen, children: []*node{{kind: nKey, label: "new:1"}}})
	if !rn.loaded || rn.children[0].label != "new:1" {
		t.Fatal("최신 세대 응답은 붙여야 한다")
	}
}

func TestUI_실패해도_트랜잭션이_열려_있으면_표시한다(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 1
	m.run("select * from nope")
	m.Update(resultMsg{tab: m.cur().id, seq: m.seq, conn: 1, res: &db.Result{InTx: true}, err: errors.New("relation does not exist")})
	if !strings.Contains(m.status, "실패") || !strings.Contains(m.status, "트랜잭션 열림") {
		t.Fatalf("status=%q", m.status)
	}
}

func TestUI_쓰기_실행_중_새로_실행하면_이전_쓰기_반영_가능성을_알린다(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 1
	m.run("delete from t where id = 1")
	m.run("select 1")
	if !strings.Contains(m.status, "이전 쓰기를 취소 요청함") {
		t.Fatalf("status=%q", m.status)
	}
}

func must(t *testing.T, err error) {
	t.Helper()
	if err != nil {
		t.Fatal(err)
	}
}

// openQuery 는 Ctrl+O 선택 창에서 이름으로 골라 연다.
func openQuery(m *model, name string) {
	m.Update(key("ctrl+o"))
	for _, r := range name {
		m.Update(key(string(r)))
	}
	m.Update(key("enter"))
}

func press(x, y int) tea.MouseMsg {
	return tea.MouseMsg{X: x, Y: y, Action: tea.MouseActionPress, Button: tea.MouseButtonLeft}
}

func wheel(at image.Point, b tea.MouseButton) tea.MouseMsg {
	return tea.MouseMsg{X: at.X, Y: at.Y, Action: tea.MouseActionPress, Button: b}
}

func TestMouse_트리_행을_클릭하면_고르고_포커스를_옮긴다(t *testing.T) {
	m := testModel(t)
	m.setFocus(fEditor)
	// y=0 헤더, y=1 트리 위 테두리, y=2 첫 행(기타), y=3 x-stg(R66: stg 가 prod 앞)
	m.Update(press(10, 3))
	if m.focus != fTree || m.cursor != 1 || m.current().label != "x-stg" {
		t.Fatalf("focus=%v cursor=%d", m.focus, m.cursor)
	}
	m.Update(press(10, 30)) // 행이 없는 자리
	if m.cursor != 1 {
		t.Fatalf("빈 자리 클릭은 커서를 옮기지 않는다: %d", m.cursor)
	}
}

func TestMouse_펼침표시_클릭과_더블클릭은_노드를_연다(t *testing.T) {
	m := testModel(t)
	group := m.roots[0]
	m.Update(press(1, 2)) // 그룹의 ▾
	if group.expanded || len(m.rows) != 1 {
		t.Fatalf("▾ 클릭은 접어야 한다: expanded=%v rows=%d", group.expanded, len(m.rows))
	}
	m.Update(press(1, 2))
	if !group.expanded {
		t.Fatal("▸ 클릭은 펼쳐야 한다")
	}

	stg := m.connNode(1)
	m.lastClick = click{row: -1}
	m.Update(press(12, 3)) // 이름 위 한 번 클릭은 고르기만
	if stg.expanded || m.cur().conn != -1 {
		t.Fatal("한 번 클릭은 펼치지 않는다")
	}
	_, cmd := m.Update(press(12, 3))
	if !stg.expanded || cmd == nil || m.cur().conn != 1 {
		t.Fatalf("더블클릭은 접속을 펼치고 활성으로: expanded=%v conn=%d", stg.expanded, m.cur().conn)
	}
	m.Update(press(12, 3))
	if !stg.expanded {
		t.Fatal("더블클릭 다음 세 번째 클릭은 다시 더블클릭이 아니다")
	}
}

func TestMouse_휠은_트리와_결과를_스크롤한다(t *testing.T) {
	m := testModel(t)
	g := m.roots[0]
	for i := 0; i < 100; i++ {
		g.children = append(g.children, &node{kind: nInfo, label: fmt.Sprintf("n%d", i), conn: 1, parent: g})
	}
	m.rebuild()
	m.Update(wheel(image.Pt(5, 5), tea.MouseButtonWheelDown))
	m.Update(wheel(image.Pt(5, 5), tea.MouseButtonWheelDown))
	if m.treeTop != 2*wheelStep || m.cursor < m.treeTop {
		t.Fatalf("treeTop=%d cursor=%d", m.treeTop, m.cursor)
	}
	_ = m.View()
	if m.treeTop != 2*wheelStep {
		t.Fatalf("View 가 휠 위치를 되돌리면 안 된다: %d", m.treeTop)
	}
	for i := 0; i < 100; i++ {
		m.Update(wheel(image.Pt(5, 5), tea.MouseButtonWheelDown))
	}
	if l := m.layout(); m.treeTop != len(m.rows)-l.treeH {
		t.Fatalf("끝에서 멈춰야 한다: %d", m.treeTop)
	}

	rows := make([][]string, 100)
	for i := range rows {
		rows[i] = []string{fmt.Sprint(i)}
	}
	show(m, &db.Result{Cols: []string{"a"}, Rows: rows})
	m.Update(wheel(image.Pt(80, 35), tea.MouseButtonWheelDown))
	if got := gridCell(t, m); got != "3" {
		t.Fatalf("결과 휠은 표를 3행 내린다: %q", got)
	}
	for i := 0; i < 40; i++ {
		m.Update(wheel(image.Pt(80, 35), tea.MouseButtonWheelDown))
	}
	if got := gridCell(t, m); got == "3" {
		t.Fatalf("계속 내리면 커서도 따라 내려온다: %q", got)
	}
}

func TestMouse_영역을_클릭하면_포커스가_옮겨진다(t *testing.T) {
	m := testModel(t)
	l := m.layout()
	m.Update(press(l.treeW+10, 4))
	if m.focus != fEditor {
		t.Fatalf("편집기 클릭: %v", m.focus)
	}
	m.Update(press(l.treeW+10, l.edH+6))
	if m.focus != fResult {
		t.Fatalf("결과 클릭: %v", m.focus)
	}
	m.Update(press(5, 0))
	m.Update(press(5, m.h-1))
	if m.focus != fResult {
		t.Fatalf("헤더·상태 줄 클릭은 포커스를 바꾸지 않는다: %v", m.focus)
	}
	m.overlay = oConfirm
	m.Update(press(5, 3))
	if m.focus != fResult {
		t.Fatal("오버레이 중 클릭은 무시한다")
	}
}

func TestMouse_탭_줄_클릭은_탭을_바꾸고_더하기는_새_탭을_만든다(t *testing.T) {
	m := testModel(t)
	m.newTab()
	x := m.layout().treeW + 2
	m.Update(press(x+1, 1))
	if m.tab != 0 {
		t.Fatalf("첫 탭 클릭: %d", m.tab)
	}
	labels := m.tabLabels()
	plusX := x + len(labels[0]) + 1 + len(labels[1]) + 1
	m.Update(press(plusX+1, 1))
	if len(m.tabs) != 3 || m.tab != 2 || m.focus != fEditor {
		t.Fatalf("+ 클릭: tabs=%d tab=%d", len(m.tabs), m.tab)
	}
}

func TestTabs_만들기_닫기_전환(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 1
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlT})
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlT})
	if len(m.tabs) != 3 || m.tab != 2 || m.cur().conn != 1 || m.cur().title != "Query 3" {
		t.Fatalf("tabs=%d tab=%d conn=%d", len(m.tabs), m.tab, m.cur().conn)
	}
	m.Update(tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("1"), Alt: true})
	if m.tab != 0 {
		t.Fatalf("Alt+1: %d", m.tab)
	}
	m.Update(tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("9"), Alt: true})
	if m.tab != 0 {
		t.Fatalf("없는 탭 번호는 무시: %d", m.tab)
	}
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlPgUp})
	if m.tab != 2 {
		t.Fatalf("처음에서 이전은 마지막으로: %d", m.tab)
	}
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlPgDown})
	if m.tab != 0 {
		t.Fatalf("마지막에서 다음은 처음으로: %d", m.tab)
	}
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlW})
	if len(m.tabs) != 2 || m.cur().title != "Query 2" {
		t.Fatalf("닫은 뒤 tabs=%d title=%q", len(m.tabs), m.cur().title)
	}
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlW})
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlW})
	if len(m.tabs) != 1 || m.cur().title != "Query 4" {
		t.Fatalf("마지막 탭을 닫으면 빈 탭 하나가 남는다: tabs=%d title=%q", len(m.tabs), m.cur().title)
	}
}

func TestTabs_결과는_실행한_탭에만_붙고_늦은_결과는_버린다(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 1
	first := m.cur()
	m.run("select 1")
	oldSeq := first.seq
	m.run("select 2")
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlT})
	second := m.cur()

	m.Update(resultMsg{tab: first.id, seq: oldSeq, conn: 1, res: &db.Result{Text: "old"}})
	if first.res != nil || first.running == nil {
		t.Fatal("다시 실행하기 전 결과는 버려야 한다")
	}
	m.Update(resultMsg{tab: first.id, seq: first.seq, conn: 1, session: true, res: &db.Result{Text: "SELECT 1", InTx: true}})
	if first.res == nil || first.res.Text != "SELECT 1" || first.running != nil || second.res != nil {
		t.Fatalf("결과는 실행한 탭에만: first=%v second=%v", first.res, second.res)
	}
	if !strings.HasPrefix(m.status, "Query 1: ") || !m.inTx[1] {
		t.Fatalf("다른 탭 결과는 탭 이름을 붙여 알린다: %q", m.status)
	}

	m.run("select 3")
	id, seq := second.id, second.seq
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlW})
	m.Update(resultMsg{tab: id, seq: seq, conn: 1, res: &db.Result{Text: "late"}})
	if len(m.tabs) != 1 || first.res.Text != "SELECT 1" {
		t.Fatal("닫힌 탭의 결과는 어디에도 붙지 않는다")
	}
}

func TestTabs_prod_확인_중_탭을_바꿔도_실행은_원래_탭에서_한다(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 0
	first := m.cur()
	m.run("delete from t")
	m.newTab()
	m.Update(key("y"))
	if first.running == nil || m.cur().running != nil {
		t.Fatal("확인한 실행은 요청한 탭에서 돌아야 한다")
	}
}

func TestTabs_파일_열기는_빈_탭이면_그_탭에_아니면_새_탭에(t *testing.T) {
	m := testModel(t)
	dir := m.proj.Dir
	must(t, os.MkdirAll(filepath.Join(dir, "queries"), 0o755))
	must(t, os.WriteFile(filepath.Join(dir, "queries", "a.sql"), []byte("select 'a';"), 0o600))
	must(t, os.WriteFile(filepath.Join(dir, "queries", "b.sql"), []byte("select 'b';"), 0o600))
	openQuery(m, "a.sql")
	if len(m.tabs) != 1 || m.cur().file != "queries/a.sql" {
		t.Fatalf("빈 탭에 연다: tabs=%d file=%q", len(m.tabs), m.cur().file)
	}
	openQuery(m, "b.sql")
	if len(m.tabs) != 2 || m.cur().file != "queries/b.sql" || m.tabs[0].editor.Value() != "select 'a';" {
		t.Fatalf("내용이 있으면 새 탭에: tabs=%d", len(m.tabs))
	}
	openQuery(m, "a.sql")
	if len(m.tabs) != 2 || m.tab != 0 {
		t.Fatalf("이미 연 파일은 그 탭으로 간다: tabs=%d tab=%d", len(m.tabs), m.tab)
	}
}

func TestHelp_F1_은_도움말_트리의_물음표는_이_화면의_키다(t *testing.T) {
	m := testModel(t)
	m.Update(tea.KeyMsg{Type: tea.KeyF1})
	if m.overlay != oHelp || !strings.Contains(m.View(), "Ctrl+O / Ctrl+S") {
		t.Fatal("F1 은 도움말을 연다")
	}
	m.Update(key("x"))
	if m.overlay != oNone {
		t.Fatal("아무 키나 닫는다")
	}
	m.Update(key("?"))
	if m.overlay != oKeys {
		t.Fatal("트리에서 ? 는 이 화면의 키(R74.2)")
	}
	m.Update(tea.KeyMsg{Type: tea.KeyEsc})
	m.setFocus(fEditor)
	m.Update(key("?"))
	if m.overlay != oNone || m.cur().editor.Value() != "?" {
		t.Fatalf("편집기에서 ? 는 글자다: %q", m.cur().editor.Value())
	}
}

func TestResize_트리_폭과_편집기_높이는_범위_안에서만_바뀐다(t *testing.T) {
	t.Setenv("XDG_STATE_HOME", t.TempDir())
	m := testModel(t)
	for i := 0; i < 50; i++ {
		m.Update(tea.KeyMsg{Type: tea.KeyCtrlLeft})
		m.Update(tea.KeyMsg{Type: tea.KeyCtrlUp})
	}
	if m.treeW != minTreeWidth || m.edH != minEditorLines {
		t.Fatalf("하한: treeW=%d edH=%d", m.treeW, m.edH)
	}
	for i := 0; i < 50; i++ {
		m.Update(tea.KeyMsg{Type: tea.KeyCtrlRight})
		m.Update(tea.KeyMsg{Type: tea.KeyCtrlDown})
	}
	// 120x40: 트리는 폭의 60%(72칸), 편집기는 편집기·결과 몫 33줄 - 3줄
	if m.treeW != 72 || m.edH != 30 {
		t.Fatalf("상한: treeW=%d edH=%d", m.treeW, m.edH)
	}
	if got := strings.Count(m.cur().editor.View(), "\n") + 1; got != m.layout().edH {
		t.Fatalf("편집기 크기도 따라가야 한다: %d != %d", got, m.layout().edH)
	}
	m.setFocus(fEditor)
	before := m.treeW
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlLeft})
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlUp})
	if m.treeW != before || m.edH != 30 {
		t.Fatalf("편집기 포커스의 Ctrl+방향키는 편집기 몫이다: treeW=%d edH=%d", m.treeW, m.edH)
	}
	m.Update(tea.WindowSizeMsg{Width: minWidth, Height: minHeight})
	l := m.layout()
	if l.rightW < minRightWidth || l.resH < 1 || l.treeW+l.rightW+4 != minWidth {
		t.Fatalf("좁은 화면에서도 결과 영역이 남아야 한다: %+v", l)
	}
}

func TestView_여러_크기와_오버레이에서_화면을_넘치지_않는다(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 0
	show(m, &db.Result{Cols: []string{"id", "긴 이름의 열"}, Rows: [][]string{{"1", strings.Repeat("값", 80)}}, InTx: true})
	m.cur().conn = 0
	m.inTx[0] = true
	m.newTab()
	m.cur().resErr = "boom\nline2"
	m.status = strings.Repeat("상태 ", 60)
	for _, ratio := range []int{0, minEditorLines, 200} {
		for _, tw := range []int{minTreeWidth, defaultTreeWidth, 200} {
			m.edH, m.treeW = ratio, tw
			for w := minWidth; w <= 220; w += 23 {
				for h := minHeight; h <= 60; h += 7 {
					m.Update(tea.WindowSizeMsg{Width: w, Height: h})
					for _, tab := range []int{0, 1} {
						m.switchTab(tab)
						for _, f := range []focus{fTree, fEditor, fResult} {
							m.setFocus(f)
							lines := strings.Split(m.View(), "\n")
							if len(lines) != h {
								t.Fatalf("%dx%d ratio=%d tree=%d: 줄 수 %d", w, h, ratio, tw, len(lines))
							}
							for i, ln := range lines {
								if lipgloss.Width(ln) > w {
									t.Fatalf("%dx%d ratio=%d tree=%d: %d번 줄 폭 %d", w, h, ratio, tw, i, lipgloss.Width(ln))
								}
							}
						}
					}
					m.overlay = oHelp
					if lines := strings.Split(m.View(), "\n"); len(lines) > h {
						t.Fatalf("%dx%d 도움말 줄 수 %d", w, h, len(lines))
					}
					m.overlay = oNone
				}
			}
		}
	}
}

// show 는 결과를 실제 경로(resultMsg)로 현재 탭에 붙인다. 접속 1(x-stg)에서 지금 편집 판으로 시작한 결과다.
func show(m *model, r *db.Result) {
	m.Update(resultMsg{tab: m.cur().id, seq: m.cur().seq, conn: 1, res: r, rev: m.cur().grid.Revision()})
}

// gridCell 은 결과 표 커서 칸의 값을 y(복사) 요청으로 읽는다. 표 상태는 바꾸지 않는다.
func gridCell(t *testing.T, m *model) string {
	t.Helper()
	_, a := m.cur().grid.Update(key("y"))
	if a.Copy == nil {
		t.Fatal("복사 요청이 없다")
	}
	return *a.Copy
}

func typeText(m *model, s string) {
	for _, r := range s {
		m.Update(key(string(r)))
	}
}

func isQuit(cmd tea.Cmd) bool {
	if cmd == nil {
		return false
	}
	_, ok := cmd().(tea.QuitMsg)
	return ok
}

func addRedis(m *model) *node {
	m.proj.Conns = append(m.proj.Conns, db.Conn{Name: "r-stg", UUID: "r", Kind: db.KindRedis, Host: "127.0.0.1", Port: 1})
	rn := &node{kind: nConn, label: "r-stg", conn: len(m.proj.Conns) - 1, parent: m.roots[0], loaded: true, expanded: true}
	rn.children = []*node{{kind: nKey, label: "session:1", key: "session:1", conn: rn.conn, parent: rn}, {kind: nInfo, label: "… 더 있음", conn: rn.conn, parent: rn}}
	m.roots[0].children = append(m.roots[0].children, rn)
	m.rebuild()
	return rn
}

func addTable(m *model, conn int) *node {
	cn := m.connNode(conn)
	schema := &node{kind: nSchema, label: "public", conn: conn, parent: cn, loaded: true, expanded: true}
	schema.children = []*node{{kind: nTable, label: "users", schema: "public", conn: conn, parent: schema}}
	cn.children, cn.loaded, cn.expanded = []*node{schema}, true, true
	m.rebuild()
	m.selectNode(schema.children[0])
	return schema.children[0]
}

func TestRun_선택한_여러_문장은_prod에서_하나라도_쓰기면_한_번만_묻는다(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 0
	m.cur().editor.SetValue("select 1;\ndelete from t;\nselect 2")
	m.setFocus(fEditor)
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlA})
	if m.Update(tea.KeyMsg{Type: tea.KeyCtrlR}); m.overlay != oConfirm || m.pending.stmts != 3 || m.cur().running != nil {
		t.Fatalf("prod 쓰기가 섞이면 확인 한 번: overlay=%v pending=%+v", m.overlay, m.pending)
	}
	_, cmd := m.Update(key("y"))
	if cmd == nil || m.overlay != oNone || m.cur().running == nil {
		t.Fatal("y 한 번이면 세 문장을 실행한다")
	}

	m.cur().editor.SetValue("select 1;\nselect 2")
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlA})
	if _, cmd := m.Update(tea.KeyMsg{Type: tea.KeyCtrlR}); cmd == nil || m.overlay != oNone {
		t.Fatal("읽기만 있으면 prod 도 바로 실행")
	}
	m.Update(resultMsg{tab: m.cur().id, seq: m.cur().seq, conn: 0, stmts: 2, res: &db.Result{Cols: []string{"a"}, Rows: [][]string{{"2"}}}})
	if !strings.HasPrefix(m.status, "2 statements · 1 rows") {
		t.Fatalf("여러 문장은 개수를 알린다: %q", m.status)
	}
}

func TestRun_선택이_없으면_커서_문장_하나만_실행한다(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 0
	m.cur().editor.SetValue("delete from t; select 2")
	m.setFocus(fEditor)
	if m.Update(tea.KeyMsg{Type: tea.KeyCtrlR}); m.pending == nil || m.pending.sql != "delete from t" || m.pending.stmts != 1 {
		t.Fatalf("커서(맨 앞)의 문장만: %+v", m.pending)
	}
}

func testPGConn(t *testing.T) db.Conn {
	t.Helper()
	raw := os.Getenv("DBTUI_TEST_PG_URL")
	if raw == "" {
		t.Skip("DBTUI_TEST_PG_URL not set")
	}
	u, err := url.Parse(raw)
	must(t, err)
	pass, _ := u.User.Password()
	port, err := strconv.Atoi(u.Port())
	must(t, err)
	return db.Conn{Name: "it-local", UUID: "it", Kind: db.KindPG, Host: u.Hostname(), Port: port,
		DB: strings.TrimPrefix(u.Path, "/"), User: u.User.Username(), Password: pass}
}

func TestExecScript_한_세션에서_차례로_돌고_첫_오류에서_멈춘다(t *testing.T) {
	c := testPGConn(t)
	clients := db.NewClients()
	defer clients.Close()
	ctx := context.Background()
	res, err := execScript(clients, script{conn: c, stmts: []string{"set application_name = 'dbtui_it'", "select current_setting('application_name') as a"}})(ctx)
	if err != nil || len(res.Rows) != 1 || res.Rows[0][0] != "dbtui_it" {
		t.Fatalf("SET 이 다음 문장까지 이어져야 한다: %v %+v", err, res)
	}
	_, err = execScript(clients, script{conn: c, stmts: []string{"select 1", "select * from no_such_table_x", "select 3"}})(ctx)
	if err == nil || !strings.Contains(err.Error(), "statement 2 of 3") {
		t.Fatalf("두 번째에서 멈추고 몇 번째인지 알린다: %v", err)
	}
}

func TestExplain_CtrlE는_실행하지_않는_EXPLAIN을_묻지_않고_돌린다(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 0
	m.cur().editor.SetValue("delete from t")
	m.setFocus(fEditor)
	_, cmd := m.Update(tea.KeyMsg{Type: tea.KeyCtrlE})
	if cmd == nil || m.overlay != oNone || m.cur().running == nil {
		t.Fatalf("EXPLAIN 은 prod 쓰기 문장도 묻지 않는다: overlay=%v", m.overlay)
	}
	msg, ok := cmd().(resultMsg) // 127.0.0.1:1 은 바로 거부되어 결과 메시지가 돌아온다
	if !ok || msg.sql != "EXPLAIN (FORMAT JSON) delete from t" {
		t.Fatalf("sql=%q", msg.sql)
	}

	m.cur().editor.SetValue("explain analyze delete from t")
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlE})
	if m.overlay != oConfirm || !m.pending.analyze {
		t.Fatal("직접 쓴 EXPLAIN ANALYZE 쓰기는 묻는다")
	}
	m.Update(key("n"))
}

func TestExplain_AltE는_쓰기_문장이면_모든_환경에서_묻고_prod는_한_번_더_묻는다(t *testing.T) {
	altE := tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("e"), Alt: true}
	m := testModel(t)
	m.cur().conn = 1
	m.cur().editor.SetValue("update t set a = 1 where id = 1")
	m.setFocus(fEditor)
	m.Update(altE)
	if m.overlay != oConfirm || !strings.Contains(m.View(), "EXPLAIN ANALYZE 는 문장을 실제로 실행합니다") {
		t.Fatal("stg 에서도 ANALYZE 쓰기는 묻는다")
	}
	m.Update(key("n"))
	if m.cur().running != nil || m.status != "취소함" {
		t.Fatal("N 이면 실행하지 않는다")
	}
	m.Update(altE)
	if _, cmd := m.Update(key("y")); cmd == nil || m.cur().running == nil {
		t.Fatal("stg 는 ANALYZE 확인 한 번이면 실행")
	}

	m.cur().conn = 0
	m.Update(altE)
	m.Update(key("y"))
	if m.overlay != oConfirm || m.pending == nil || m.pending.analyze || !strings.Contains(m.View(), "prod(x-prod)에 쓰기 실행") {
		t.Fatal("prod 는 ANALYZE 확인 뒤 prod 확인을 한 번 더")
	}
	m.Update(key("y"))
	if m.overlay != oNone || m.cur().running == nil {
		t.Fatal("두 번 확인하면 실행")
	}

	m.cur().editor.SetValue("select 1")
	_, cmd := m.Update(altE)
	if cmd == nil || m.overlay != oNone {
		t.Fatal("읽기 문장의 ANALYZE 는 prod 에서도 바로")
	}
	if msg := cmd().(resultMsg); msg.sql != "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) select 1" {
		t.Fatalf("sql=%q", msg.sql)
	}

	addRedis(m)
	m.cur().conn = 2
	if _, cmd := m.Update(altE); cmd != nil || !strings.Contains(m.status, "PostgreSQL") {
		t.Fatalf("Redis 는 EXPLAIN 이 없다: %q", m.status)
	}
}

func TestTree_d는_구조를_현재_탭_결과로_D는_DDL을_새_탭에(t *testing.T) {
	m := testModel(t)
	addTable(m, 1)
	_, cmd := m.Update(key("d"))
	if cmd == nil || m.cur().running == nil || m.cur().conn != 1 || len(m.tabs) != 1 {
		t.Fatalf("d 는 현재 탭에서 구조를 읽는다: conn=%d tabs=%d", m.cur().conn, len(m.tabs))
	}
	_, cmd = m.Update(key("D"))
	if cmd == nil || len(m.tabs) != 1 {
		t.Fatal("D 는 DDL 을 읽는 명령")
	}
	m.Update(ddlMsg{conn: 1, table: "users", ddl: "CREATE TABLE users ();"})
	if len(m.tabs) != 2 || m.cur().editor.Value() != "CREATE TABLE users ();" || m.cur().conn != 1 || m.cur().title != "DDL users" || m.focus != fEditor {
		t.Fatalf("DDL 은 새 탭 편집기에: tabs=%d title=%q", len(m.tabs), m.cur().title)
	}
	m.proj.Conns[1].Password = "s3cret"
	m.Update(ddlMsg{conn: 1, err: errors.New("auth s3cret failed")})
	if len(m.tabs) != 2 || strings.Contains(m.status, "s3cret") || !strings.Contains(m.status, "DDL 실패") {
		t.Fatalf("실패는 상태 줄에, 비밀번호 없이: %q", m.status)
	}

	m.setFocus(fTree)
	m.selectNode(m.roots[0])
	if _, cmd := m.Update(key("d")); cmd != nil || !strings.Contains(m.status, "테이블에서") {
		t.Fatalf("테이블이 아니면 안내만: %q", m.status)
	}
}

func TestHistory_실행이_끝나면_기록하고_Ctrl_H로_골라_편집기에_넣는다(t *testing.T) {
	m := testModel(t)
	st, err := history.Open(filepath.Join(t.TempDir(), "h.jsonl"))
	must(t, err)
	m.hist = st
	m.proj.Conns[1].Password = "pw!"
	m.cur().conn = 1
	m.run("select 1")
	_, cmd := m.Update(resultMsg{tab: m.cur().id, seq: m.seq, conn: 1, sql: "select 1", stmts: 1, res: &db.Result{Cols: []string{"a"}, Rows: [][]string{{"1"}}}})
	if cmd == nil || cmd() != nil {
		t.Fatal("기록 명령이 오류 없이 돌아야 한다")
	}
	m.run("select nope")
	_, cmd = m.Update(resultMsg{tab: m.cur().id, seq: m.seq, conn: 1, sql: "select nope", stmts: 1, err: errors.New("bad pw!")})
	must(t, nilMsg(cmd))
	got := st.List(history.Filter{})
	if len(got) != 2 || got[0].SQL != "select nope" || got[0].Err != "bad ***" || got[1].Rows != 1 || got[1].Conn != "x-stg" {
		t.Fatalf("기록: %+v", got)
	}

	m.cur().editor.SetValue("")
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlH})
	if m.overlay != oHistory || len(m.historyMatches()) != 2 {
		t.Fatal("Ctrl+H 는 기록 창")
	}
	typeText(m, "select 1")
	m.Update(key("enter"))
	if m.overlay != oNone || len(m.tabs) != 1 || m.cur().editor.Value() != "select 1" || m.focus != fEditor {
		t.Fatalf("빈 탭이면 그 탭에 넣는다: tabs=%d value=%q", len(m.tabs), m.cur().editor.Value())
	}
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlH})
	m.Update(tea.KeyMsg{Type: tea.KeyDown})
	m.Update(key("enter"))
	if len(m.tabs) != 2 || m.cur().editor.Value() != "select 1" || m.cur().conn != 1 {
		t.Fatalf("내용이 있으면 새 탭에: tabs=%d value=%q", len(m.tabs), m.cur().editor.Value())
	}
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlH})
	m.Update(tea.KeyMsg{Type: tea.KeyEsc})
	if m.overlay != oNone || len(m.tabs) != 2 {
		t.Fatal("esc 는 닫기만")
	}

	m.hist = nil
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlH})
	if m.overlay != oNone {
		t.Fatal("기록을 못 열었으면 창을 열지 않는다")
	}
}

func nilMsg(cmd tea.Cmd) error {
	if cmd == nil {
		return errors.New("no command")
	}
	if msg := cmd(); msg != nil {
		return fmt.Errorf("unexpected message: %v", msg)
	}
	return nil
}

func TestComplete_목록은_접속별로_한_번_읽고_없어도_키워드는_나온다(t *testing.T) {
	m := testModel(t)
	tab := m.cur()
	sug := completer{m: m, t: tab}.suggest
	if items := sug("sel", 3); len(items) == 0 || items[0].Label != "SELECT" {
		t.Fatalf("접속이 없어도 키워드: %+v", items)
	}
	if cmd := m.setConn(tab, 1); cmd == nil {
		t.Fatal("처음 쓰는 PostgreSQL 접속은 목록을 읽는다")
	}
	if cmd := m.ensureCatalog(1); cmd != nil {
		t.Fatal("읽는 중이면 다시 읽지 않는다")
	}
	if items := sug("select * from us", 16); len(items) != 0 {
		t.Fatalf("목록이 오기 전엔 테이블이 없다: %+v", items)
	}
	m.Update(catalogMsg{conn: 1, gen: m.catGen[1], cat: &complete.Catalog{Schemas: []complete.Schema{{Name: "public", Tables: []complete.Table{{Name: "users"}}}}}})
	if items := sug("select * from us", 16); len(items) == 0 || items[0].Label != "users" {
		t.Fatalf("목록이 오면 테이블: %+v", items)
	}

	m.selectNode(m.connNode(1))
	if _, cmd := m.Update(key("r")); cmd == nil || m.catalogs[1] != nil {
		t.Fatal("접속에서 r 은 목록도 다시 읽는다")
	}

	addRedis(m)
	if cmd := m.setConn(tab, 2); cmd != nil {
		t.Fatal("Redis 는 목록을 읽지 않는다")
	}
	items := sug("GET ses", 7)
	found := false
	for _, it := range items {
		found = found || it.Label == "session:1"
	}
	if !found {
		t.Fatalf("Redis 는 트리에 읽힌 키: %+v", items)
	}
}

func TestEditor_자동완성_창이_열려_있으면_Tab은_고르기다(t *testing.T) {
	m := testModel(t)
	m.setFocus(fEditor)
	typeText(m, "selec") // sel 은 라이브 템플릿 이름이라 Tab 이 템플릿을 펼친다
	if !m.cur().editor.PopupOpen() {
		t.Fatal("글자를 치면 자동완성 창이 열린다")
	}
	m.Update(tea.KeyMsg{Type: tea.KeyTab})
	if m.focus != fEditor || m.cur().editor.Value() != "select" {
		t.Fatalf("Tab 은 후보를 넣는다: focus=%v value=%q", m.focus, m.cur().editor.Value())
	}
	m.Update(tea.KeyMsg{Type: tea.KeyTab})
	if m.focus != fResult {
		t.Fatalf("창이 닫혀 있으면 Tab 은 포커스 이동: %v", m.focus)
	}
}

func TestEditor_자동완성_창이_열려_있으면_Ctrl_N_Ctrl_P는_찾기_팔레트가_아니라_후보_이동이다(t *testing.T) {
	m := testModel(t)
	m.setFocus(fEditor)
	typeText(m, "selec")
	for _, k := range []tea.KeyType{tea.KeyCtrlN, tea.KeyCtrlP} {
		m.Update(tea.KeyMsg{Type: k})
		if m.overlay != oNone || strings.Contains(m.status, "찾을 객체") || !m.cur().editor.PopupOpen() || m.cur().editor.Value() != "selec" {
			t.Fatalf("%v: overlay=%v status=%q popup=%v value=%q", k, m.overlay, m.status, m.cur().editor.PopupOpen(), m.cur().editor.Value())
		}
	}
}

// quitModel 은 아무 일도 하지 않는 화면이다. 신호로 끝나는지만 본다.
type quitModel struct{}

func (quitModel) Init() tea.Cmd                       { return nil }
func (quitModel) Update(tea.Msg) (tea.Model, tea.Cmd) { return quitModel{}, nil }
func (quitModel) View() string                        { return "" }

func TestQuitOnHangup_SIGHUP을_받으면_Run이_정상으로_돌아와_세션을_저장할_수_있다(t *testing.T) {
	p := tea.NewProgram(quitModel{}, tea.WithInput(nil), tea.WithOutput(io.Discard), tea.WithoutRenderer(), tea.WithoutSignalHandler())
	stop := quitOnHangup(p)
	defer stop()
	done := make(chan error, 1)
	go func() {
		_, err := p.Run()
		done <- err
	}()
	// Quit 은 Run 의 메시지 고리가 받을 때까지 기다리므로 Run 이 들어가기 전에 신호가 와도 된다
	must(t, syscall.Kill(os.Getpid(), syscall.SIGHUP))
	select {
	case err := <-done:
		if err != nil {
			t.Fatalf("Run 오류: %v", err)
		}
	case <-time.After(3 * time.Second):
		p.Kill()
		t.Fatal("SIGHUP 으로 끝나지 않았다")
	}
}

func TestEditor_빈_편집기에는_안내를_그리고_클릭_좌표는_편집기_기준으로_넘긴다(t *testing.T) {
	m := testModel(t)
	if !strings.Contains(m.View(), "Ctrl+Space 자동완성") {
		t.Fatal("빈 편집기 안내")
	}
	m.cur().editor.SetValue("abc\ndef")
	l := m.layout()
	gutter := 4
	m.Update(press(l.treeW+3+gutter+2, 4)) // 둘째 줄 셋째 글자 앞
	if m.focus != fEditor || m.cur().editor.CursorOffset() != 6 {
		t.Fatalf("focus=%v offset=%d", m.focus, m.cur().editor.CursorOffset())
	}
	m.Update(tea.MouseMsg{X: l.treeW + 3 + gutter + 3, Y: 4, Action: tea.MouseActionMotion, Button: tea.MouseButtonLeft})
	m.Update(tea.MouseMsg{X: l.treeW + 3 + gutter + 3, Y: 4, Action: tea.MouseActionRelease})
	if s, e, ok := m.cur().editor.Selection(); !ok || s != 6 || e != 7 {
		t.Fatalf("끌기는 선택: %d %d %v", s, e, ok)
	}
	m.cur().editor.SetValue("x")
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlA})
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlK})
	m.Update(editor.ClipboardErrMsg{Err: errors.New("closed")})
	if !strings.Contains(m.status, "클립보드 복사 실패") {
		t.Fatalf("status=%q", m.status)
	}
}

func TestGrid_거르기_입력_중에는_q와_esc가_표로_간다(t *testing.T) {
	m := testModel(t)
	show(m, &db.Result{Cols: []string{"name"}, Rows: [][]string{{"aq"}, {"b"}}})
	m.setFocus(fResult)
	m.Update(key("/"))
	if !m.cur().grid.Typing() {
		t.Fatal("/ 는 표 거르기")
	}
	if _, cmd := m.Update(key("q")); isQuit(cmd) {
		t.Fatal("입력 중 q 는 종료가 아니다")
	}
	m.Update(tea.KeyMsg{Type: tea.KeyEnter})
	if m.cur().grid.Typing() || gridCell(t, m) != "aq" {
		t.Fatal("q 로 걸러 aq 만 남는다")
	}
	if _, cmd := m.Update(key("q")); !isQuit(cmd) {
		t.Fatal("입력이 끝나면 q 는 종료")
	}
}

func TestGrid_복사는_OSC52로_내보내기는_Downloads에_0600으로(t *testing.T) {
	var buf bytes.Buffer
	old := clipboardOut
	clipboardOut = &buf
	t.Cleanup(func() { clipboardOut = old })
	home := t.TempDir()
	t.Setenv("HOME", home)
	must(t, os.Mkdir(filepath.Join(home, "Downloads"), 0o700))

	m := testModel(t)
	show(m, &db.Result{Cols: []string{"a", "b"}, Rows: [][]string{{"1", "x,y"}}})
	m.setFocus(fResult)
	_, cmd := m.Update(key("y"))
	must(t, nilMsg(cmd))
	if !strings.Contains(buf.String(), "\x1b]52;c;"+base64.StdEncoding.EncodeToString([]byte("1"))) || !strings.Contains(m.status, "복사") {
		t.Fatalf("OSC 52: %q status=%q", buf.String(), m.status)
	}

	_, cmd = m.Update(key("e"))
	m.Update(cmd())
	p := strings.TrimSpace(m.status[strings.Index(m.status, ": ")+2:])
	if filepath.Dir(p) != filepath.Join(home, "Downloads") || !strings.HasPrefix(filepath.Base(p), "db-Query-1-") || !strings.HasSuffix(p, ".csv") {
		t.Fatalf("경로: %q", m.status)
	}
	st, err := os.Stat(p)
	must(t, err)
	b, err := os.ReadFile(p)
	must(t, err)
	if st.Mode().Perm() != 0o600 || !strings.Contains(string(b), `"x,y"`) {
		t.Fatalf("권한 %v 내용 %q", st.Mode().Perm(), b)
	}

	must(t, os.RemoveAll(filepath.Join(home, "Downloads")))
	t.Setenv("TMPDIR", t.TempDir())
	_, cmd = m.Update(key("E"))
	m.Update(cmd())
	if !strings.Contains(m.status, os.Getenv("TMPDIR")) || !strings.HasSuffix(m.status, ".json") {
		t.Fatalf("Downloads 가 없으면 임시 폴더: %q", m.status)
	}
}

func TestParseMCPArgs_플래그와_기본값(t *testing.T) {
	t.Setenv("DBTUI_PROJECT", "/proj")
	a, err := parseMCPArgs(nil)
	if err != nil || a.dir != "/proj" || a.opt.AllowProd || a.opt.MaxRows != 0 {
		t.Fatalf("기본값: %+v %v", a, err)
	}
	a, err = parseMCPArgs([]string{"-p", "/x", "--allow-prod", "--max-rows", "50"})
	if err != nil || a.dir != "/x" || !a.opt.AllowProd || a.opt.MaxRows != 50 {
		t.Fatalf("플래그: %+v %v", a, err)
	}
	for _, bad := range [][]string{{"--max-rows", "-1"}, {"extra"}, {"--nope"}} {
		if _, err := parseMCPArgs(bad); err == nil {
			t.Errorf("%v 는 오류여야 한다", bad)
		}
	}
}

func TestExplain_CtrlE는_속_문장에_analyze_글자가_있어도_묻지_않는다(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 0
	m.setFocus(fEditor)
	for _, s := range []string{"select 'analyze'", "update t set analyze = 1"} {
		m.cur().editor.SetValue(s)
		if _, cmd := m.Update(tea.KeyMsg{Type: tea.KeyCtrlE}); cmd == nil || m.overlay != oNone {
			t.Fatalf("%q: overlay=%v", s, m.overlay)
		}
	}
}

func TestHistory_밀려나거나_닫힌_탭의_결과도_기록하고_탭에는_붙이지_않는다(t *testing.T) {
	m := testModel(t)
	st, err := history.Open(filepath.Join(t.TempDir(), "h.jsonl"))
	must(t, err)
	m.hist = st
	m.proj.Conns[1].Password = "pw!"
	m.cur().conn = 1
	first := m.cur()
	m.run("delete from t")
	oldSeq := first.seq
	m.run("select 1")
	_, cmd := m.Update(resultMsg{tab: first.id, seq: oldSeq, conn: 1, sql: "delete from t", stmts: 1, err: errors.New("canceled pw!")})
	must(t, nilMsg(cmd))
	if first.res != nil || first.resErr != "" || first.running == nil {
		t.Fatal("밀려난 결과는 탭에 붙지 않는다")
	}
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlW})
	_, cmd = m.Update(resultMsg{tab: first.id, seq: first.seq, conn: 1, sql: "select 1", stmts: 1, res: &db.Result{Text: "SELECT 1"}})
	must(t, nilMsg(cmd))
	got := st.List(history.Filter{})
	if len(got) != 2 || got[0].SQL != "select 1" || got[1].SQL != "delete from t" || got[1].Err != "canceled ***" {
		t.Fatalf("기록: %+v", got)
	}
}

func TestHistory_비밀값이_든_문장은_가려서_기록하고_기록_창에도_가려_보인다(t *testing.T) {
	m := testModel(t)
	st, err := history.Open(filepath.Join(t.TempDir(), "h.jsonl"))
	must(t, err)
	m.hist = st
	m.proj.Conns[1].Password = "pw!"
	m.cur().conn = 1
	for _, sql := range []string{"ALTER ROLE app PASSWORD 'hunter2'", "select 'pw!'"} {
		m.run(sql)
		_, cmd := m.Update(resultMsg{tab: m.cur().id, seq: m.seq, conn: 1, sql: sql, stmts: 1, res: &db.Result{Text: "OK"}})
		must(t, nilMsg(cmd))
	}
	got := st.List(history.Filter{})
	if len(got) != 2 || got[0].SQL != "select '***'" || got[1].SQL != db.MaskedSQL {
		t.Fatalf("기록: %+v", got)
	}

	// 가리기 전에 남은 옛 기록도 기록 창에는 가려 보인다
	must(t, st.Append(history.Entry{Time: time.Now(), Conn: "x-stg", SQL: "AUTH hunter2"}))
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlH})
	if v := m.View(); strings.Contains(v, "hunter2") || !strings.Contains(v, "비밀값이 있어 기록하지 않음") {
		t.Fatalf("기록 창:\n%s", v)
	}
}

func TestRun_실행이_끝나면_실행_컨텍스트를_해제한다(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 1
	var got context.Context
	cmd := m.start(pendingRun{tab: m.cur().id, conn: 1, readOnly: true, exec: func(ctx context.Context) (*db.Result, error) {
		got = ctx
		return &db.Result{Text: "OK"}, nil
	}})
	cmd()
	if got == nil || got.Err() == nil {
		t.Fatal("끝난 실행의 컨텍스트는 취소되어야 한다")
	}
}

func TestTree_테이블_Enter_는_편집기에_글이_있으면_새_탭에_연다(t *testing.T) {
	m := testModel(t)
	addTable(m, 1)
	m.cur().editor.SetValue("select keep")
	m.cur().file = "queries/a.sql"
	m.setFocus(fTree)
	_, cmd := m.Update(key("enter"))
	if cmd == nil || len(m.tabs) != 2 || m.cur().tv == nil || m.cur().conn != 1 {
		t.Fatalf("새 탭: tabs=%d tv=%v", len(m.tabs), m.cur().tv != nil)
	}
	if m.tabs[0].editor.Value() != "select keep" || m.tabs[0].file != "queries/a.sql" || m.tabs[0].conn != -1 {
		t.Fatalf("원래 탭은 그대로: value=%q file=%q conn=%d", m.tabs[0].editor.Value(), m.tabs[0].file, m.tabs[0].conn)
	}
}

func TestGrid_결과_영역을_떠나면_거르기_입력이_끝난다(t *testing.T) {
	m := testModel(t)
	show(m, &db.Result{Cols: []string{"name"}, Rows: [][]string{{"aq"}, {"b"}}})
	m.setFocus(fResult)
	m.Update(key("/"))
	l := m.layout()
	m.Update(press(l.treeW+10, 4))
	if m.focus != fEditor || m.cur().grid.Typing() {
		t.Fatalf("focus=%v typing=%v", m.focus, m.cur().grid.Typing())
	}
	m.setFocus(fTree)
	if _, cmd := m.Update(key("q")); !isQuit(cmd) {
		t.Fatal("거르기가 끝났으니 q 는 종료")
	}
}
