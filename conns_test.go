package main

import (
	"os"
	"path/filepath"
	"strings"
	"testing"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/x/ansi"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
)

// connTestModel 은 DataGrip 접속(PostgreSQL·Redis) 둘과 db 전용 접속(PostgreSQL·Redis) 둘을 가진 프로젝트의 화면이다.
// 모두 닿지 않는 로컬 포트라 시험은 곧바로 실패한다.
func connTestModel(t *testing.T) *model {
	t.Helper()
	dir := writeCLIProject(t, []cliSource{
		{"dg-stg", "grp", "postgresql", "127.0.0.1", "1", "app", "u"},
		{"dg-stg-redis", "grp", "redis", "127.0.0.1", "1", "0", ""},
	})
	for _, c := range []db.Conn{
		{Name: "mine-stg", Group: "grp", Kind: db.KindPG, Host: "127.0.0.1", Port: 1, DB: "app", Password: "old-pw"},
		{Name: "mine-redis", Group: "grp", Kind: db.KindRedis, Host: "127.0.0.1", Port: 1},
	} {
		if _, err := db.AddConn(dir, c); err != nil {
			t.Fatal(err)
		}
	}
	proj, err := loadProject(dir)
	must(t, err)
	m := newModel(proj)
	m.Update(tea.WindowSizeMsg{Width: 120, Height: 40})
	m.setFocus(fTree)
	return m
}

func (m *model) selectConn(t *testing.T, name string) int {
	t.Helper()
	i := connByName(m.proj, name)
	cn := m.connNode(i)
	if cn == nil {
		t.Fatalf("트리에 %s 가 없다", name)
	}
	m.rebuild()
	m.selectNode(cn)
	return i
}

// runTest 는 Ctrl+T·Enter 가 돌려준 시험 명령을 실제로 돌려 결과를 화면에 넣는다.
func runTest(t *testing.T, m *model, cmd tea.Cmd) {
	t.Helper()
	if cmd == nil {
		t.Fatalf("시험 명령이 없다: %q", m.connForm.status)
	}
	msg, ok := cmd().(connTestMsg)
	if !ok {
		t.Fatal("connTestMsg 가 아니다")
	}
	m.Update(msg)
}

func TestConnForm_트리_N_은_그룹을_채운_창을_열고_URL_을_붙이면_칸을_채운다(t *testing.T) {
	m := connTestModel(t)
	m.selectConn(t, "dg-stg")
	m.Update(key("N"))
	f := m.connForm
	if m.overlay != oConn || f == nil || f.inputs[cfGroup].Value() != "grp" {
		t.Fatalf("창: overlay=%v", m.overlay)
	}
	m.Update(key("app2"))
	m.Update(paste("redis://:p%40ss@cache.internal:6380/3"))
	want := map[int]string{cfName: "app2", cfHost: "cache.internal", cfPort: "6380", cfDB: "3", cfUser: "", cfPassword: "p@ss"}
	for i, v := range want {
		if got := f.inputs[i].Value(); got != v {
			t.Errorf("%s: %q, want %q", connFieldLabels[i], got, v)
		}
	}
	if f.kind != db.KindRedis {
		t.Fatalf("종류: %s", f.kind)
	}
	view := ansi.Strip(m.View())
	if !strings.Contains(view, "DB 번호") || !strings.Contains(view, "••••") || strings.Contains(view, "p@ss") {
		t.Fatalf("비밀번호는 • 로만 보인다:\n%s", view)
	}
	m.Update(tea.KeyMsg{Type: tea.KeyEsc})
	if m.overlay != oNone || m.connForm != nil {
		t.Fatal("esc 로 닫는다")
	}
}

func TestConnForm_종류_칸은_space_로_바꾸고_기본_포트를_따라간다(t *testing.T) {
	m := connTestModel(t)
	m.openConnForm(nil, false)
	f := m.connForm
	f.setFocus(cfKind)
	m.Update(key(" "))
	if f.kind != db.KindRedis || !strings.Contains(ansi.Strip(m.View()), "6379") {
		t.Fatalf("Redis 로 바뀌고 포트 자리 글은 6379: %s", f.kind)
	}
	m.Update(tea.KeyMsg{Type: tea.KeyRight})
	if f.kind != db.KindPG {
		t.Fatal("→ 로 다시 PostgreSQL")
	}
}

func TestConnForm_CtrlT_는_저장하지_않고_비동기로_시험해_상태를_보인다(t *testing.T) {
	m := connTestModel(t)
	m.openConnForm(nil, false)
	m.Update(paste("postgres://u:Sekrit-77@127.0.0.1:1/d"))
	_, cmd := m.Update(tea.KeyMsg{Type: tea.KeyCtrlT})
	f := m.connForm
	if cmd == nil || !f.testing || f.status != "접속 시험 중…" {
		t.Fatalf("시험 시작: %q", f.status)
	}
	// 시험 중 결과가 오기 전에는 다시 시험하지 않는다
	if _, again := m.Update(tea.KeyMsg{Type: tea.KeyCtrlT}); again != nil {
		t.Fatal("시험 중 두 번째 Ctrl+T 는 무시")
	}
	m.Update(connTestMsg{seq: f.testSeq, version: "PostgreSQL 16.4"})
	if f.testing || f.status != "접속됨 (PostgreSQL 16.4)" || !strings.Contains(ansi.Strip(m.View()), "접속됨 (PostgreSQL 16.4)") {
		t.Fatalf("성공 상태: %q", f.status)
	}
	if _, err := os.Stat(db.ConnsPath(m.proj.Dir)); err != nil {
		t.Fatal(err)
	}
	if len(m.proj.Conns) != 4 {
		t.Fatal("시험은 저장하지 않는다")
	}

	_, cmd = m.Update(tea.KeyMsg{Type: tea.KeyCtrlT})
	runTest(t, m, cmd)
	if !f.failed || !strings.HasPrefix(f.status, "접속 실패: ") || strings.Contains(f.status, "Sekrit-77") || f.askSave {
		t.Fatalf("실패 상태(비밀번호 없이, 저장 묻지 않음): %q ask=%v", f.status, f.askSave)
	}
	// 늦게 온 이전 시험 결과는 버린다
	m.Update(connTestMsg{seq: f.testSeq - 1, version: "PostgreSQL 9"})
	if strings.Contains(f.status, "9") && !strings.Contains(f.status, "실패") {
		t.Fatalf("늦은 결과를 버려야 한다: %q", f.status)
	}
}

func TestConnForm_Enter_시험이_실패하면_저장할지_묻고_y_면_저장해_트리에_db_로_보인다(t *testing.T) {
	m := connTestModel(t)
	m.cur().conn = connByName(m.proj, "dg-stg")
	m.openConnForm(nil, false)
	m.Update(key("new-one"))
	m.Update(paste("postgres://ro:Pw-Secret-1@127.0.0.1:1/app"))
	_, cmd := m.Update(key("enter"))
	runTest(t, m, cmd)
	if !m.connForm.askSave || !strings.Contains(ansi.Strip(m.View()), "저장할까요? y/N") {
		t.Fatalf("저장을 묻는다: %q", m.connForm.status)
	}
	m.Update(key("y"))
	if m.overlay != oNone {
		t.Fatalf("저장하면 닫는다: %q", m.connForm.status)
	}
	i := connByName(m.proj, "new-one")
	if i != 4 || m.conn(i).Source != db.SourceDB || m.conn(i).Password != "Pw-Secret-1" || m.conn(i).User != "ro" {
		t.Fatalf("끝에 붙는다: i=%d %+v", i, m.conn(i))
	}
	if m.current() != m.connNode(i) || !strings.Contains(ansi.Strip(m.View()), "new-one (db)") {
		t.Fatal("트리에서 새 접속을 고르고 (db) 를 붙인다")
	}
	if m.cur().conn != connByName(m.proj, "dg-stg") {
		t.Fatal("다른 탭 접속은 그대로")
	}
	env, err := os.ReadFile(filepath.Join(m.proj.Dir, ".env"))
	must(t, err)
	if !strings.Contains(string(env), "PG_NEW_ONE_PASSWORD=Pw-Secret-1\n") {
		t.Fatalf(".env: %s", env)
	}

	// n 이면 저장하지 않는다
	m.openConnForm(nil, false)
	m.Update(key("other"))
	m.Update(paste("postgres://127.0.0.1:1/app"))
	_, cmd = m.Update(key("enter"))
	runTest(t, m, cmd)
	m.Update(key("n"))
	if m.overlay != oConn || connByName(m.proj, "other") >= 0 || m.connForm.status != "저장하지 않음" {
		t.Fatalf("n 이면 창에 남고 저장하지 않는다: %q", m.connForm.status)
	}
}

func TestConnForm_Enter_는_이름이_틀리면_시험하지_않는다(t *testing.T) {
	m := connTestModel(t)
	m.openConnForm(nil, false)
	m.Update(key("bad name"))
	m.connForm.setFocus(cfHost)
	m.Update(key("h"))
	if _, cmd := m.Update(key("enter")); cmd != nil || !m.connForm.failed {
		t.Fatalf("이름 검사: %q", m.connForm.status)
	}
}

func TestConnForm_E_는_db_접속을_채워_열고_비밀번호를_비우면_그대로_둔다(t *testing.T) {
	m := connTestModel(t)
	i := m.selectConn(t, "mine-stg")
	m.cur().conn = i
	m.Update(key("E"))
	f := m.connForm
	if m.overlay != oConn || f.edit != i || f.inputs[cfName].Value() != "mine-stg" || f.inputs[cfHost].Value() != "127.0.0.1" ||
		f.inputs[cfPort].Value() != "1" || f.inputs[cfDB].Value() != "app" || f.inputs[cfPassword].Value() != "" {
		t.Fatalf("채운 칸: edit=%d", f.edit)
	}
	f.setFocus(cfName)
	f.inputs[cfName].SetValue("mine-stg2")
	f.inputs[cfDB].SetValue("app2")
	_, cmd := m.Update(key("enter"))
	runTest(t, m, cmd)
	m.Update(key("y"))
	if m.overlay != oNone {
		t.Fatalf("저장: %q", f.status)
	}
	c := m.conn(i)
	if c.Name != "mine-stg2" || c.DB != "app2" || c.Password != "old-pw" || connByName(m.proj, "mine-stg") >= 0 {
		t.Fatalf("같은 자리에서 고친다: %+v", c)
	}
	if m.cur().conn != i {
		t.Fatal("고친 접속을 쓰던 탭은 접속을 지킨다")
	}
	env, err := os.ReadFile(filepath.Join(m.proj.Dir, ".env"))
	must(t, err)
	if !strings.Contains(string(env), "PG_MINE_STG2_PASSWORD=old-pw") || strings.Contains(string(env), "PG_MINE_STG_PASSWORD") {
		t.Fatalf(".env 키를 옮긴다: %s", env)
	}
}

func TestConnForm_DataGrip_접속의_E_X_는_안내만_하고_Redis_X_는_패턴_삭제다(t *testing.T) {
	m := connTestModel(t)
	m.selectConn(t, "dg-stg")
	for _, k := range []string{"E", "X"} {
		m.status = ""
		m.Update(key(k))
		if m.overlay != oNone || m.status != dataGripRefusal {
			t.Fatalf("%s: overlay=%v status=%q", k, m.overlay, m.status)
		}
	}
	m.selectConn(t, "dg-stg-redis")
	m.Update(key("X"))
	if m.overlay != oPrompt || !strings.Contains(m.input.Prompt, "지울 키 패턴") {
		t.Fatalf("Redis 접속 X 는 R42: overlay=%v", m.overlay)
	}
}

func TestConnForm_X_는_확인한_뒤_db_접속을_지우고_쓰던_탭은_접속을_잃는다(t *testing.T) {
	m := connTestModel(t)
	i := m.selectConn(t, "mine-stg")
	m.cur().conn = i
	m.Update(key("X"))
	if m.overlay != oConfirm || m.yes == nil || m.yes.q != "mine-stg 접속을 지울까요(.env 비밀번호도)" {
		t.Fatalf("확인: overlay=%v", m.overlay)
	}
	m.Update(key("y"))
	if connByName(m.proj, "mine-stg") >= 0 || m.cur().conn != -1 || m.connNode(i) != nil {
		t.Fatalf("지움: tab conn=%d", m.cur().conn)
	}
	if len(m.proj.Conns) != 4 || m.proj.Conns[i].Kind != "" {
		t.Fatal("자리는 비워 두고 다른 접속 인덱스는 그대로")
	}
	if j := connByName(m.proj, "mine-redis"); j != 3 {
		t.Fatalf("뒤 접속 인덱스: %d", j)
	}
	for _, r := range m.rows {
		if r.n.label == "mine-stg" {
			t.Fatal("트리에서 빠진다")
		}
	}
	env, err := os.ReadFile(filepath.Join(m.proj.Dir, ".env"))
	must(t, err)
	if strings.Contains(string(env), "PG_MINE_STG_PASSWORD") {
		t.Fatal(".env 비밀번호도 지운다")
	}

	// n 이면 지우지 않는다
	m.selectConn(t, "mine-redis")
	m.Update(tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("X"), Alt: true})
	m.Update(key("n"))
	if connByName(m.proj, "mine-redis") < 0 {
		t.Fatal("n 이면 그대로")
	}
}

func TestConnForm_팔레트_새_접속과_접속_지우기(t *testing.T) {
	m := connTestModel(t)
	m.setFocus(fEditor)
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlP})
	m.input.SetValue("새 접속")
	list := m.pickerMatches()
	if len(list) == 0 {
		t.Fatal("팔레트에 새 접속이 있다")
	}
	m.Update(key("enter"))
	if m.overlay != oConn {
		t.Fatalf("새 접속 창: overlay=%v", m.overlay)
	}
	m.Update(tea.KeyMsg{Type: tea.KeyEsc})

	// 접속 줄이 아니면 db 전용 접속을 (db) 를 붙여 고르게 한다
	m.setFocus(fEditor)
	m.cursor = 0 // 그룹 줄
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlP})
	m.input.SetValue("접속 지우기")
	if len(m.pickerMatches()) == 0 {
		t.Fatal("팔레트에 접속 지우기가 있다")
	}
	m.Update(key("enter"))
	if m.overlay != oPicker || strings.Join(m.pickerItems, ",") != "mine-stg (db),mine-redis (db)" {
		t.Fatalf("고르기: overlay=%v %v", m.overlay, m.pickerItems)
	}
	m.pickerCursor = 1
	m.Update(key("enter"))
	if m.overlay != oConfirm || !strings.HasPrefix(m.yes.q, "mine-redis ") {
		t.Fatalf("Redis db 접속도 지울 수 있다: overlay=%v", m.overlay)
	}
	m.Update(key("y"))
	if connByName(m.proj, "mine-redis") >= 0 {
		t.Fatal("지움")
	}
}

func TestConnForm_실행_중인_접속은_고치거나_지우지_않는다(t *testing.T) {
	m := connTestModel(t)
	i := m.selectConn(t, "mine-stg")
	m.inTx[i] = true
	m.Update(key("X"))
	if m.overlay != oNone || !strings.Contains(m.status, "트랜잭션") {
		t.Fatalf("열린 트랜잭션: %q", m.status)
	}
}
