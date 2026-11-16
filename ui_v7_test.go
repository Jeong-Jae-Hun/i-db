package main

import (
	"context"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"testing"
	"time"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/Jeong-Jae-Hun/i-db/internal/complete"
	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/history"
	"github.com/Jeong-Jae-Hun/i-db/internal/paramstore"
	"github.com/Jeong-Jae-Hun/i-db/internal/statcrypt"
	"github.com/Jeong-Jae-Hun/i-db/internal/tableview"
)

const (
	v7Key = "0123456789abcdef"
	v7IV  = "000102030405060708090a0b0c0d0e0f"
)

// collect 는 명령을 한 번 실행해 나온 메시지를 모은다(Batch 는 펼친다). 시계 메시지는 버린다.
func collect(cmd tea.Cmd) []tea.Msg {
	var out []tea.Msg
	queue := []tea.Cmd{cmd}
	for len(queue) > 0 {
		c := queue[0]
		queue = queue[1:]
		if c == nil {
			continue
		}
		switch msg := c().(type) {
		case nil, tickMsg, lockTickMsg:
		case tea.BatchMsg:
			queue = append(queue, msg...)
		default:
			out = append(out, msg)
		}
	}
	return out
}

// resultOf 는 명령이 낸 실행 결과 메시지다. 없으면 시험을 멈춘다.
func resultOf(t *testing.T, cmd tea.Cmd) resultMsg {
	t.Helper()
	for _, msg := range collect(cmd) {
		if r, ok := msg.(resultMsg); ok {
			return r
		}
	}
	t.Fatal("실행 결과 메시지가 없다")
	return resultMsg{}
}

func paste(s string) tea.KeyMsg {
	return tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune(s), Paste: true}
}

// fakeRuns 는 readOnlyRun 을 가짜로 바꿔 접속 이름 → 결과를 돌려주고 보낸 SQL 을 남긴다.
type fakeRuns struct {
	mu   sync.Mutex
	sqls map[string]string
}

func fakeReadOnly(t *testing.T, results map[string]*db.Result) *fakeRuns {
	t.Helper()
	f := &fakeRuns{sqls: map[string]string{}}
	old := readOnlyRun
	readOnlyRun = func(_ context.Context, _ *db.Clients, c db.Conn, sql string) (*db.Result, error) {
		f.mu.Lock()
		f.sqls[c.Name] = sql
		f.mu.Unlock()
		if r, ok := results[c.Name]; ok {
			return r, nil
		}
		return nil, errors.New("boom " + c.Name)
	}
	t.Cleanup(func() { readOnlyRun = old })
	return f
}

func openHistory(t *testing.T, m *model) {
	t.Helper()
	h, err := history.Open(filepath.Join(t.TempDir(), "history.jsonl"))
	must(t, err)
	m.hist = h
}

// ─── R48 붙여넣기 ───

func TestPaste_결과_포커스의_붙여넣기는_커서부터_덮고_넘치면_새_행이다(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 1
	show(m, editable())
	m.setFocus(fResult)
	m.Update(key("l"))
	m.Update(paste("q\nw\ne\n"))
	if m.cur().grid.Pending() != 3 || m.status != "붙여넣기 3칸 · 새 행 1개 · ctrl+s 로 미리 보기" {
		t.Fatalf("pending=%d status=%q", m.cur().grid.Pending(), m.status)
	}
	m.Update(key("ctrl+s"))
	if m.overlay != oConfirm || !strings.Contains(strings.Join(m.pending.preview, "\n"), `SET "name" = 'q'`) {
		t.Fatalf("붙인 값도 제출 미리 보기를 탄다: %+v", m.pending)
	}
	m.Update(key("n"))

	// 편집기 포커스의 붙여넣기는 편집기에 들어간다
	m.setFocus(fEditor)
	m.Update(paste("select 1"))
	if m.cur().editor.Value() != "select 1" {
		t.Fatalf("편집기: %q", m.cur().editor.Value())
	}
}

func TestPaste_편집할_수_없는_결과는_이유만_알린다(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 1
	show(m, &db.Result{Cols: []string{"a"}, Rows: [][]string{{"1"}}})
	m.setFocus(fResult)
	m.Update(paste("x"))
	if m.cur().grid.Pending() != 0 || m.status == "" || strings.HasPrefix(m.status, "붙여넣기") {
		t.Fatalf("status=%q", m.status)
	}
}

// ─── R49 enum·긴 글 ───

func typedTable() *db.Result {
	return &db.Result{Cols: []string{"id", "state", "meta"}, Types: []string{"int4", "16401", "jsonb"},
		Rows:   [][]string{{"1", "open", `{"a":1}`}, {"2", "NULL", "NULL"}},
		Origin: &db.Origin{Table: db.TableRef{Schema: "public", Table: "tasks"}, PK: []string{"id"}, Cols: []string{"id", "state", "meta"}}}
}

func TestEnum_결과의_사용자_타입을_읽어_enum_칸은_고르기_창으로_고친다(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 1
	show(m, typedTable())
	if labels, ok := m.enums[1]["16401"]; !ok || labels != nil {
		t.Fatalf("모르는 사용자 타입은 읽는 중으로 표시한다: %v %v", labels, ok)
	}
	m.Update(enumMsg{conn: 1, db: m.conn(1).DB, labels: map[string][]string{"16401": {"open", "closed"}}})
	m.setFocus(fResult)
	m.Update(key("l"))
	m.Update(key("enter"))
	if m.overlay != oPicker || !slices.Equal(m.pickerItems, []string{"open", "closed", "NULL"}) || m.pickerCursor != 0 {
		t.Fatalf("고르기 창: overlay=%v items=%v cursor=%d", m.overlay, m.pickerItems, m.pickerCursor)
	}
	m.Update(key("down"))
	m.Update(key("enter"))
	if m.cur().grid.Pending() != 1 || gridCell(t, m) != "closed" {
		t.Fatalf("고른 값은 보류 중인 바꿈: pending=%d cell=%q", m.cur().grid.Pending(), gridCell(t, m))
	}

	// NULL 칸은 NULL 에 커서가 있고, esc 면 바꾸지 않는다
	m.Update(key("j"))
	m.Update(key("enter"))
	if m.pickerCursor != 2 {
		t.Fatalf("지금 값 NULL 에 커서: %d", m.pickerCursor)
	}
	m.Update(key("esc"))
	if m.cur().grid.Pending() != 1 {
		t.Fatal("esc 면 바꾸지 않는다")
	}

	// 같은 타입은 다시 묻지 않는다
	if cmd := m.loadEnums(m.cur(), 1, typedTable()); cmd != nil {
		t.Fatal("아는 타입은 다시 읽지 않는다")
	}
}

func TestLongEdit_저장은_칸에_두고_취소와_바꾸지_않은_저장은_두지_않으며_JSON을_검사한다(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 1
	show(m, typedTable())
	m.setFocus(fResult)
	m.Update(key("l"))
	m.Update(key("l"))
	open := func() {
		m.Update(tea.KeyMsg{Type: tea.KeyF2})
		m.Update(key("ctrl+o"))
		if m.overlay != oLong || !m.long.json {
			t.Fatalf("전체 화면 편집기: overlay=%v", m.overlay)
		}
	}
	open()
	if !strings.Contains(m.View(), "긴 값 편집 (JSON)") {
		t.Fatal("편집기 화면")
	}
	m.Update(key("esc"))
	if m.overlay != oNone || m.cur().grid.Pending() != 0 {
		t.Fatal("esc 면 버린다")
	}

	open()
	m.Update(key("ctrl+s"))
	if m.overlay != oNone || m.cur().grid.Pending() != 0 || m.status != "바뀐 것이 없어 두지 않음" {
		t.Fatalf("바꾸지 않은 저장: %q", m.status)
	}

	open()
	m.long.ed.SetValue(`{"b": `)
	m.Update(key("ctrl+s"))
	if m.overlay != oLong || !strings.HasPrefix(m.status, "JSON 이 아님: ") {
		t.Fatalf("잘못된 JSON 은 남는다: %q", m.status)
	}
	m.long.ed.SetValue(`{"b":2}`)
	m.Update(tea.KeyMsg{Type: tea.KeyF8})
	if m.long.ed.Value() != "{\n  \"b\": 2\n}" {
		t.Fatalf("F8 정렬: %q", m.long.ed.Value())
	}
	m.Update(key("ctrl+s"))
	if m.overlay != oNone || m.cur().grid.Pending() != 1 || gridCell(t, m) != "{\n  \"b\": 2\n}" {
		t.Fatalf("저장: pending=%d status=%q", m.cur().grid.Pending(), m.status)
	}
}

// ─── R50 되돌리기 ───

func submitted() (db.ChangeSet, *db.Applied) {
	q, a := "q", "a"
	one := "1"
	cs := db.ChangeSet{Table: db.TableRef{Schema: "public", Table: "t"}, PK: []string{"id"},
		Changes: []db.Change{{Op: db.ChangeUpdate, Key: map[string]string{"id": "1"}, Values: map[string]*string{"name": &q}, Old: map[string]*string{"name": &a}}}}
	return cs, &db.Applied{Rows: []map[string]*string{{"id": &one, "name": &q}}}
}

func TestUndo_제출이_성공하면_쌓고_기록하며_CtrlZ는_미리_보기_뒤_내고_실패하면_남긴다(t *testing.T) {
	t.Setenv("XDG_STATE_HOME", t.TempDir())
	m := testModel(t)
	tab := m.cur()
	tab.conn = 1
	show(m, editable())
	m.setFocus(fResult)
	m.Update(key("ctrl+z"))
	if m.status != "되돌릴 제출이 없음" {
		t.Fatalf("status=%q", m.status)
	}

	cs, applied := submitted()
	_, cmd := m.Update(resultMsg{tab: tab.id, seq: tab.seq, conn: 1, kind: rkSubmit, changes: &cs,
		res: &db.Result{Text: "UPDATE 1", Applied: applied}, rev: tab.grid.Revision()})
	if len(tab.undo) != 1 {
		t.Fatalf("쌓인 묶음: %d", len(tab.undo))
	}
	collect(cmd)
	path, _ := undoPath()
	st, err := os.Stat(path)
	must(t, err)
	if st.Mode().Perm() != 0o600 {
		t.Fatalf("권한: %v", st.Mode())
	}
	var line undoLine
	must(t, json.Unmarshal([]byte(readFile(t, path)), &line))
	if line.Conn != "s" || line.Table != `"public"."t"` || len(line.Undo) != 1 || !strings.Contains(line.Undo[0], `SET "name" = 'a'`) {
		t.Fatalf("기록: %+v", line)
	}

	m.Update(key("ctrl+z"))
	if m.overlay != oConfirm || m.pending.confirm != "제출 되돌리기 1건" || !strings.Contains(m.pending.preview[0], `SET "name" = 'a' WHERE "id" = '1' AND`) || !strings.HasSuffix(m.pending.preview[0], `= 'q' RETURNING *;`) {
		t.Fatalf("미리 보기: %+v", m.pending)
	}
	if v := m.View(); !strings.Contains(v, "제출 되돌리기 1건: y/N") {
		t.Fatal("확인 창")
	}
	_, cmd = m.Update(key("y"))
	r := resultOf(t, cmd)
	if r.undo == nil || r.kind != rkSubmit {
		t.Fatalf("되돌리기는 제출 길을 탄다: %+v", r)
	}
	// 옛값 확인 실패(다른 사람이 고침)면 묶음을 남긴다
	r.err, r.res = errors.New("update affected 0 rows"), nil
	m.Update(r)
	if len(tab.undo) != 1 || !strings.HasPrefix(m.status, "되돌리기 실패: ") || !strings.HasSuffix(m.status, "묶음은 남김") {
		t.Fatalf("실패: undo=%d status=%q", len(tab.undo), m.status)
	}
	m.Update(key("ctrl+z"))
	_, cmd = m.Update(key("y"))
	r = resultOf(t, cmd)
	r.err, r.res = nil, &db.Result{Text: "UPDATE 1"}
	m.Update(r)
	if len(tab.undo) != 0 || !strings.HasPrefix(m.status, "되돌림: ") {
		t.Fatalf("성공하면 뺀다: undo=%d status=%q", len(tab.undo), m.status)
	}
}

func TestUndo_탭마다_20개까지_쌓는다(t *testing.T) {
	m := testModel(t)
	tab := m.cur()
	tab.conn = 1
	for range maxUndo + 3 {
		cs, applied := submitted()
		m.pushUndo(tab, 1, cs, applied)
	}
	if len(tab.undo) != maxUndo {
		t.Fatalf("%d개", len(tab.undo))
	}
	if note, _ := m.pushUndo(tab, 1, db.ChangeSet{PK: []string{"id"}}, nil); !strings.HasPrefix(note, "되돌리기 묶음을 만들지 못함") {
		t.Fatalf("만들 수 없으면 알린다: %q", note)
	}
}

func TestUndo_실제_서버에서_되돌리고_다른_사람이_고친_행은_덮지_않는다(t *testing.T) {
	t.Setenv("XDG_STATE_HOME", t.TempDir())
	m, it := editTable(t, "v7_undo")
	m.Update(key("l"))
	editCell(m, "q")
	m.Update(key("ctrl+s"))
	_, cmd := m.Update(key("y"))
	drive(m, cmd)
	if got := it.sql("select name from public.v7_undo where id = 1").Rows[0][0]; got != "q" || len(m.cur().undo) != 1 {
		t.Fatalf("제출: %q undo=%d status=%q", got, len(m.cur().undo), m.status)
	}
	m.Update(key("ctrl+z"))
	_, cmd = m.Update(key("y"))
	drive(m, cmd)
	if got := it.sql("select name from public.v7_undo where id = 1").Rows[0][0]; got != "a" || len(m.cur().undo) != 0 {
		t.Fatalf("되돌림: %q undo=%d status=%q", got, len(m.cur().undo), m.status)
	}

	// 다시 제출한 뒤 다른 세션이 고치면 되돌리지 않는다
	m.Update(key("g"))
	m.Update(key("l")) // 다시 읽은 표는 첫 열로 돌아간다
	editCell(m, "w")
	m.Update(key("ctrl+s"))
	_, cmd = m.Update(key("y"))
	drive(m, cmd)
	it.sql("update public.v7_undo set name = 'other' where id = 1")
	m.Update(key("ctrl+z"))
	_, cmd = m.Update(key("y"))
	drive(m, cmd)
	if got := it.sql("select name from public.v7_undo where id = 1").Rows[0][0]; got != "other" || len(m.cur().undo) != 1 || !strings.Contains(m.status, "되돌리기 실패") {
		t.Fatalf("덮지 않는다: %q undo=%d status=%q", got, len(m.cur().undo), m.status)
	}
}

// ─── R51 사용자 한눈에 보기 ───

func TestUser360_여러_접속을_읽기_전용으로_조회하고_입력값은_기록에_남기지_않는다(t *testing.T) {
	m := testModel(t)
	openHistory(t, m)
	m.proj.Conns[1].Group = "auth"
	m.proj.AESEnv = map[string]string{"AUTH_AES_KEY": v7Key, "AUTH_AES_IV": v7IV}
	cfg := `[{"title":"auth users","conn":"x-stg","by":["email"],"sql":"select * from users where email = :email_enc"},
		{"title":"없는 곳","conn":"nope","by":["email"],"sql":"select 1 where :email is not null"},
		{"title":"id 만","conn":"x-stg","by":["id"],"sql":"select :id"}]`
	must(t, os.MkdirAll(filepath.Join(m.proj.Dir, ".dbtui"), 0o755))
	must(t, os.WriteFile(filepath.Join(m.proj.Dir, ".dbtui", "user360.json"), []byte(cfg), 0o644))
	f := fakeReadOnly(t, map[string]*db.Result{"x-stg": {Cols: []string{"uid"}, Rows: [][]string{{"u1"}}}})

	m.Update(keyMsgOf("ctrl+u"))
	if m.overlay != oPrompt {
		t.Fatal("입력 줄을 연다")
	}
	typeText(m, "a@b.com")
	_, cmd := m.Update(key("enter"))
	drive(m, cmd)
	tab := m.cur()
	enc, err := statcrypt.Encrypt("a@b.com", v7Key, v7IV)
	must(t, err)
	if tab.title != "사용자 찾기" || len(tab.subs) != 2 || tab.subs[0].title != "auth users" || tab.subs[1].title != "없는 곳" {
		t.Fatalf("탭: %q %+v", tab.title, tab.subs)
	}
	if want := "select * from users where email = " + db.QuoteLiteral(enc); f.sqls["x-stg"] != want {
		t.Fatalf("이메일은 그룹 키로 암호화한다: %q", f.sqls["x-stg"])
	}
	if tab.subs[0].res.Rows[0][0] != "u1" || !strings.Contains(tab.subs[1].res.Text, "접속 없음: nope") || tab.summary != "조회 2곳 · 실패 1" {
		t.Fatalf("결과: %+v / %q", tab.subs[1].res, tab.summary)
	}
	if es := m.hist.List(history.Filter{Limit: 10}); len(es) != 0 {
		t.Fatalf("기록에 남기지 않는다: %+v", es)
	}
	if strings.Contains(tab.title+tab.name+strings.Join(m.tabLabels(), ""), "a@b.com") {
		t.Fatal("탭 이름(세션에 남는다)에 입력값을 넣지 않는다")
	}

	// 키·IV 가 없으면 서버 함수로 암호화한다
	delete(m.proj.AESEnv, "AUTH_AES_IV")
	m.Update(keyMsgOf("ctrl+u"))
	typeText(m, "a@b.com")
	_, cmd = m.Update(key("enter"))
	drive(m, cmd)
	if f.sqls["x-stg"] != "select * from users where email = staticEncrypt('a@b.com')" {
		t.Fatalf("서버 대체: %q", f.sqls["x-stg"])
	}

	m.Update(keyMsgOf("ctrl+u"))
	typeText(m, "zz")
	m.Update(key("enter"))
	if m.status != "맞는 조회가 없음" {
		t.Fatalf("status=%q", m.status)
	}
}

func TestInlineArgs_자리를_리터럴로_바꾸고_문자열_안은_두며_NULL을_넣는다(t *testing.T) {
	got := inlineArgs("select '$1', $1, $2 -- $1", []any{"o'k", nil})
	if got != "select '$1', 'o''k', NULL -- $1" {
		t.Fatalf("%q", got)
	}
}

// ─── R52 enc() ───

func TestEnc_실행_전에_암호문으로_바꾸고_기록에는_확장_전_글을_남긴다(t *testing.T) {
	m := testModel(t)
	openHistory(t, m)
	m.proj.Conns[1].Group = "app"
	m.proj.AESEnv = map[string]string{"APP_AES_KEY": v7Key, "APP_AES_IV": v7IV}
	m.cur().conn = 1
	text := "select * from u where e = enc('a@b.com')"
	cmd := m.run(text)
	if !strings.HasPrefix(m.status, "enc() 1개 암호화 · 실행 중") {
		t.Fatalf("status=%q", m.status)
	}
	drive(m, cmd) // 접속이 없어 실패하지만 기록은 남는다
	es := m.hist.List(history.Filter{Limit: 1})
	if len(es) != 1 || es[0].SQL != text {
		t.Fatalf("기록은 확장 전 글: %+v", es)
	}

	sent, note, err := m.expandEnc(1, text)
	enc, _ := statcrypt.Encrypt("a@b.com", v7Key, v7IV)
	if err != nil || note != "enc() 1개 암호화" || sent != "select * from u where e = "+db.QuoteLiteral(enc) {
		t.Fatalf("확장: %q %q %v", sent, note, err)
	}

	delete(m.proj.AESEnv, "APP_AES_IV")
	m.run(text)
	if !strings.HasPrefix(m.status, "키·IV 가 없어 서버 staticEncrypt 로 실행 · ") {
		t.Fatalf("서버 대체: %q", m.status)
	}
	m.cur().running = nil
	if cmd := m.run("select enc(email) from u"); cmd != nil || !strings.HasPrefix(m.status, "enc() at 1:8:") || strings.Contains(m.status, v7Key) {
		t.Fatalf("리터럴이 아니면 실행하지 않는다: %q", m.status)
	}
}

func TestEnc_실제_서버에서_편집기와_테이블_보기_WHERE에_적용한다(t *testing.T) {
	m := pgModel(t, "it-local")
	openHistory(t, m)
	m.proj.Conns[0].Group = "app"
	m.proj.AESEnv = map[string]string{"APP_AES_KEY": v7Key, "APP_AES_IV": v7IV}
	it := pgTest{t, m}
	it.sql("drop table if exists public.v7_enc; create table public.v7_enc (id int primary key, email text)")
	enc, _ := statcrypt.Encrypt("a@b.com", v7Key, v7IV)
	it.sql("insert into public.v7_enc values (1, " + db.QuoteLiteral(enc) + "), (2, 'x')")
	m.cur().conn = 0
	drive(m, m.run("select id from public.v7_enc where email = enc('a@b.com')"))
	if r := m.cur().res; r == nil || len(r.Rows) != 1 || r.Rows[0][0] != "1" {
		t.Fatalf("편집기: %+v %q", r, m.cur().resErr)
	}
	tab := m.cur()
	drive(m, m.openTable(tab, 0, db.TableRef{Schema: "public", Table: "v7_enc"}, "email = enc('a@b.com')", tableview.Options{}))
	if r := tab.res; r == nil || len(r.Rows) != 1 || r.Rows[0][0] != "1" {
		t.Fatalf("테이블 보기 WHERE: %+v %q", r, tab.resErr)
	}
	for _, e := range m.hist.List(history.Filter{Limit: 10}) {
		if strings.Contains(e.SQL, enc) {
			t.Fatalf("기록에 암호문이 아니라 확장 전 글: %q", e.SQL)
		}
	}
}

// ─── R53 비교 ───

func TestCompare_기본_대상은_stg_prod를_바꾼_접속이고_새_탭에_비교를_보인다(t *testing.T) {
	m := testModel(t)
	m.proj.Conns = append(m.proj.Conns, db.Conn{Name: "z-stg", UUID: "zs", Kind: db.KindPG}, db.Conn{Name: "z-prod", UUID: "zp", Kind: db.KindPG})
	m.cur().conn = 2
	m.setFocus(fEditor)
	m.cur().editor.SetValue("delete from t")
	m.Update(keyMsgOf("alt+c"))
	if m.status != "읽기 문장만 비교" {
		t.Fatalf("status=%q", m.status)
	}
	f := fakeReadOnly(t, map[string]*db.Result{
		"z-stg":  {Cols: []string{"id"}, Rows: [][]string{{"1"}, {"2"}}},
		"z-prod": {Cols: []string{"id"}, Rows: [][]string{{"1"}, {"3"}}},
	})
	m.cur().editor.SetValue("select id from t")
	m.Update(keyMsgOf("alt+c"))
	if m.overlay != oPicker || !slices.Equal(m.pickerItems, []string{"x-prod", "x-stg", "z-prod"}) || m.pickerCursor != 2 {
		t.Fatalf("고르기: %v cursor=%d", m.pickerItems, m.pickerCursor)
	}
	_, cmd := m.Update(key("enter"))
	drive(m, cmd)
	tab := m.cur()
	if tab.title != "비교 z-stg ↔ z-prod" || tab.view != 1 || !strings.Contains(tab.summary, "z-stg 기준 z-prod 비교") || f.sqls["z-prod"] != "select id from t" {
		t.Fatalf("탭: %q view=%d summary=%q", tab.title, tab.view, tab.summary)
	}
	if v := tab.grid.View(true); !strings.Contains(v, "+1") || !strings.Contains(v, "−1") {
		t.Fatalf("비교 표: %s", v)
	}

	// 한쪽이 실패하면 알린다
	m.switchTab(0)
	m.cur().conn = 2
	m.Update(keyMsgOf("alt+c"))
	m.Update(key("up"))
	_, cmd = m.Update(key("enter")) // x-stg 는 가짜에 없어 실패한다
	drive(m, cmd)
	if !strings.HasPrefix(m.cur().summary, "비교 실패 x-stg: boom") {
		t.Fatalf("실패: %q", m.cur().summary)
	}
}

func TestSchemaDiff_트리_스키마_줄의_등호는_고른_접속의_같은_스키마와_비교한다(t *testing.T) {
	m := testModel(t)
	addTable(m, 1)
	m.selectNode(m.connNode(1).children[0])
	m.setFocus(fTree)
	old := schemaSnap
	var asked []string
	var mu sync.Mutex
	schemaSnap = func(_ context.Context, _ *db.Clients, c db.Conn, schema string) (db.Snapshot, error) {
		mu.Lock()
		asked = append(asked, c.Name+"."+schema)
		mu.Unlock()
		s := db.Snapshot{Schema: schema, Tables: map[string]*db.SnapTable{"users": {Kind: "TABLE"}}}
		if c.Name == "x-prod" {
			s.Tables["only_prod"] = &db.SnapTable{Kind: "TABLE"}
		}
		return s, nil
	}
	t.Cleanup(func() { schemaSnap = old })
	m.Update(key("="))
	if m.overlay != oPicker || !slices.Equal(m.pickerItems, []string{"x-prod"}) {
		t.Fatalf("고르기: %v", m.pickerItems)
	}
	m.Update(key("enter"))
	if m.overlay != oPrompt || m.input.Value() != "public" {
		t.Fatalf("스키마 기본값: %q", m.input.Value())
	}
	_, cmd := m.Update(key("enter"))
	drive(m, cmd)
	slices.Sort(asked)
	tab := m.cur()
	if !slices.Equal(asked, []string{"x-prod.public", "x-stg.public"}) || tab.res == nil || len(tab.res.Rows) != 1 || tab.summary != "다른 곳 1" {
		t.Fatalf("asked=%v res=%+v summary=%q", asked, tab.res, tab.summary)
	}
}

// ─── R54 prod 잠금 ───

func TestProdLock_잠긴_prod는_쓰기_경로마다_거절하고_읽기는_그대로다(t *testing.T) {
	m := testModel(t)
	m.prodLock = true
	tab := m.cur()
	tab.conn = 0
	const locked = "prod(x-prod) 쓰기 잠김 — Ctrl+L 로 10분 열기"
	refused := func(what string) {
		t.Helper()
		if m.status != locked || m.overlay == oConfirm || tab.running != nil {
			t.Fatalf("%s: status=%q overlay=%v", what, m.status, m.overlay)
		}
		m.status = ""
	}
	m.run("update t set a = 1 where id = 1")
	refused("편집기 쓰기")
	m.cur().editor.SetValue("delete from t where id = 1")
	m.setFocus(fEditor)
	m.Update(keyMsgOf("alt+e"))
	refused("EXPLAIN ANALYZE")
	for _, p := range []pendingRun{{kind: rkSubmit}, {kind: rkImport}, {kind: rkSignal}, {kind: rkRedisWrite}} {
		p.tab, p.conn = tab.id, 0
		m.guard(p)
		refused("guard")
	}
	// 결과 편집 제출·되돌리기·K
	m.Update(resultMsg{tab: tab.id, seq: tab.seq, conn: 0, res: editable(), rev: tab.grid.Revision()})
	m.setFocus(fResult)
	m.Update(key("l"))
	editCell(m, "z")
	m.Update(key("ctrl+s"))
	refused("제출")
	cs, applied := submitted()
	m.pushUndo(tab, 0, cs, applied)
	m.Update(key("ctrl+z"))
	refused("되돌리기")
	m.Update(resultMsg{tab: tab.id, seq: tab.seq, conn: 0, res: &db.Result{Cols: []string{"pid"}, Rows: [][]string{{"9"}}}, rev: tab.grid.Revision(), kind: rkGrid})
	m.Update(key("K"))
	refused("K")
	// Redis 패턴 일괄 삭제
	m.proj.Conns = append(m.proj.Conns, db.Conn{Name: "r-prod", UUID: "rp", Kind: db.KindRedis})
	rn := &node{kind: nConn, label: "r-prod", conn: len(m.proj.Conns) - 1, parent: m.roots[0], loaded: true}
	m.roots[0].children = append(m.roots[0].children, rn)
	m.rebuild()
	m.selectNode(rn)
	m.setFocus(fTree)
	m.Update(key("X"))
	if m.status != "prod(r-prod) 쓰기 잠김 — Ctrl+L 로 10분 열기" || m.overlay == oPrompt {
		t.Fatalf("X: %q", m.status)
	}

	// 읽기·COMMIT 은 그대로
	if cmd := m.run("select 1"); cmd == nil || tab.running == nil {
		t.Fatalf("읽기: %q", m.status)
	}
	tab.running = nil
	m.guard(pendingRun{tab: tab.id, conn: 0, kind: rkTx, sql: "COMMIT"})
	if m.overlay != oConfirm {
		t.Fatal("COMMIT 은 잠금이 막지 않고 R6 확인을 받는다")
	}
}

func TestProdLock_CtrlL은_탭_접속만_N분_열고_남은_시간을_보이며_다_되면_다시_잠근다(t *testing.T) {
	m := testModel(t)
	m.prodLock = true
	now := time.Date(2026, 11, 15, 9, 0, 0, 0, time.UTC)
	old := lockNow
	lockNow = func() time.Time { return now }
	t.Cleanup(func() { lockNow = old })
	t.Setenv("DB_PROD_UNLOCK_MIN", "3")
	tab := m.cur()
	tab.conn = 1
	m.Update(keyMsgOf("ctrl+l"))
	if m.status != "prod 접속이 아니라 잠그지 않음" {
		t.Fatalf("status=%q", m.status)
	}
	tab.conn = 0
	_, cmd := m.Update(keyMsgOf("ctrl+l"))
	if cmd == nil || !strings.HasPrefix(m.status, "prod(x-prod) 쓰기 3분 열림") || !strings.Contains(m.View(), "🔓 prod 3:00") {
		t.Fatalf("열기: cmd=%v status=%q", cmd != nil, m.status)
	}
	m.run("update t set a = 1 where id = 1")
	if m.overlay != oConfirm {
		t.Fatal("열려도 R6 확인은 그대로")
	}
	m.Update(key("n"))
	// 다른 prod 는 잠긴 채다
	m.proj.Conns = append(m.proj.Conns, db.Conn{Name: "y-prod", UUID: "yp", Kind: db.KindPG})
	if !m.prodLocked(2) || m.prodLocked(0) {
		t.Fatal("접속마다 따로 연다")
	}

	now = now.Add(2*time.Minute + 30*time.Second)
	if !strings.Contains(m.View(), "🔓 prod 0:30") {
		t.Fatal("남은 시간")
	}
	now = now.Add(31 * time.Second)
	_, cmd = m.Update(lockTickMsg{})
	if cmd != nil || m.status != "prod(x-prod) 쓰기 다시 잠김" || strings.Contains(m.View(), "🔓") {
		t.Fatalf("다 되면 다시 잠근다: %q", m.status)
	}
	m.run("update t set a = 1 where id = 1")
	if m.status != "prod(x-prod) 쓰기 잠김 — Ctrl+L 로 3분 열기" {
		t.Fatalf("status=%q", m.status)
	}

	// 열린 접속에서 다시 Ctrl+L 은 바로 잠근다
	m.Update(keyMsgOf("ctrl+l"))
	m.Update(keyMsgOf("ctrl+l"))
	if !m.prodLocked(0) || m.status != "prod(x-prod) 쓰기 잠금" {
		t.Fatalf("다시 잠금: %q", m.status)
	}
	if r := newModel(m.proj); !r.prodLock || len(r.unlocked) != 0 {
		t.Fatal("켤 때마다 잠긴다")
	}
}

// ─── R55 파라미터 기억 ───

func TestParams_성공한_실행의_값을_기억해_다음에_채우고_실패와_번호_자리는_남기지_않는다(t *testing.T) {
	t.Setenv("XDG_STATE_HOME", t.TempDir())
	m := testModel(t)
	tab := m.cur()
	tab.conn = 1
	m.run("select :uid")
	if m.overlay != oParams || m.input.Value() != "" {
		t.Fatalf("처음은 빈 칸: %q", m.input.Value())
	}
	typeText(m, "1315")
	_, cmd := m.Update(key("enter"))
	r := resultOf(t, cmd)
	if r.params["uid"] != "1315" {
		t.Fatalf("실행이 값을 싣는다: %+v", r.params)
	}
	r.err, r.res = nil, &db.Result{Cols: []string{"uid"}, Rows: [][]string{{"1315"}}}
	_, cmd = m.Update(r)
	collect(cmd)
	path, _ := paramstore.Path()
	vals, err := paramstore.Load(path, m.proj.Dir)
	if err != nil || vals["uid"] != "1315" {
		t.Fatalf("저장: %v %v", vals, err)
	}

	// 번호 자리($1)가 먼저 묻고 :uid 는 그다음 번호다
	m.run("select :uid, $1")
	if m.input.Value() != "" {
		t.Fatalf("번호 자리는 채우지 않는다: %q", m.input.Value())
	}
	typeText(m, "7")
	m.Update(key("enter"))
	if m.input.Value() != "1315" {
		t.Fatalf("기억한 값을 채운다: %q", m.input.Value())
	}
	m.input.SetValue("8")
	_, cmd = m.Update(key("enter"))
	r = resultOf(t, cmd) // 접속이 없어 실패한다
	_, cmd = m.Update(r)
	collect(cmd)
	if vals, _ := paramstore.Load(path, m.proj.Dir); vals["uid"] != "1315" || len(vals) != 1 {
		t.Fatalf("실패한 실행은 저장하지 않는다: %v", vals)
	}
}

// ─── R56 찾기 ───

func TestFinder_열과_함수도_찾고_열은_그_테이블을_연다(t *testing.T) {
	m := testModel(t)
	addTable(m, 1)
	m.catalogs[1] = &complete.Catalog{Schemas: []complete.Schema{{Name: "public", Tables: []complete.Table{
		{Name: "users", Columns: []complete.Column{{Name: "email", Type: "text"}}}}}}, Functions: []string{"fn_x"}}
	m.Update(keyMsgOf("ctrl+n"))
	if m.overlay != oFinder || !strings.Contains(m.View(), "public.users.email") || !strings.Contains(m.View(), "fn_x") {
		t.Fatal("열·함수 후보")
	}
	typeText(m, "email")
	_, cmd := m.Update(key("enter"))
	if cmd == nil || m.cur().tv == nil || m.cur().title != "users" {
		t.Fatalf("열은 테이블 보기: tv=%v title=%q status=%q", m.cur().tv != nil, m.cur().title, m.status)
	}

	m.Update(keyMsgOf("ctrl+n"))
	typeText(m, "fn_x")
	_, cmd = m.Update(key("enter"))
	msgs := collect(cmd)
	if len(msgs) != 1 {
		t.Fatalf("함수는 정의로 이동처럼 이름을 찾는다: %v", msgs)
	}
	if d, ok := msgs[0].(defMsg); !ok || d.name != "fn_x" || d.conn != 1 {
		t.Fatalf("defMsg: %+v", msgs[0])
	}
}

// ─── R57 탭 이름·고정 ───

func TestTabs_이름을_바꾸고_고정하면_앞에_모이며_닫을_때_묻고_세션에_남는다(t *testing.T) {
	t.Setenv("XDG_STATE_HOME", t.TempDir())
	m := testModel(t)
	m.newTab()
	third := m.newTab()
	m.setFocus(fEditor)
	m.Update(keyMsgOf("alt+r"))
	typeText(m, "내 탭")
	m.Update(key("enter"))
	m.Update(keyMsgOf("alt+p"))
	if m.tabs[0] != third || m.tab != 0 || !strings.Contains(m.tabLabels()[0], "📌내 탭") {
		t.Fatalf("고정한 탭이 앞으로: %v", m.tabLabels())
	}
	m.Update(keyMsgOf("alt+2"))
	m.Update(keyMsgOf("alt+1"))
	if m.cur() != third {
		t.Fatal("Alt+1 이 고정한 탭")
	}
	m.Update(keyMsgOf("ctrl+w"))
	if m.overlay != oConfirm || !strings.Contains(m.View(), "고정한 탭 내 탭 닫기: y/N") {
		t.Fatal("고정한 탭은 닫을 때 묻는다")
	}
	m.Update(key("n"))
	if len(m.tabs) != 3 {
		t.Fatal("N 이면 닫지 않는다")
	}

	must(t, m.saveSession())
	r := newModel(m.proj)
	r.restoreSession()
	if r.tabs[0].name != "내 탭" || !r.tabs[0].pinned || r.tabs[1].pinned {
		t.Fatalf("세션: %+v", r.tabLabels())
	}

	m.Update(keyMsgOf("ctrl+w"))
	m.Update(key("y"))
	if len(m.tabs) != 2 || slices.Contains(m.tabs, third) {
		t.Fatal("y 면 닫는다")
	}
	// 비우면 원래 이름
	m.Update(keyMsgOf("alt+r"))
	m.input.SetValue("")
	m.Update(key("enter"))
	if m.cur().name != "" {
		t.Fatal("비우면 원래 이름")
	}
}

// ─── ⌘ 안내 ───

func TestKeys_v7_동작도_팔레트와_도움말에_있고_맥은_커맨드_글자다(t *testing.T) {
	m := testModel(t)
	m.mac = true
	m.Update(keyMsgOf("ctrl+p"))
	for _, want := range []string{
		"사용자 한눈에 보기 (이메일·uid·id·닉네임으로 여러 DB 조회) — ⌘U · 전역",
		"같은 쿼리 다른 접속에서 비교 (기본 stg↔prod) — ⌘⇧C · 전역",
		"탭 이름 바꾸기 — ⌘⇧R · 쿼리 탭",
		"탭 고정 (앞에 모으고 닫을 때 묻기) — ⌘⇧P · 쿼리 탭",
		"마지막 제출 되돌리기 (미리 보기 뒤 확인) — ⌘Z · 결과",
		"다른 접속과 스키마 비교 (PostgreSQL 접속·스키마) — = · 트리",
	} {
		if !slices.Contains(m.pickerItems, want) {
			t.Errorf("팔레트에 %q 가 없다", want)
		}
	}
	if !slices.ContainsFunc(m.pickerItems, func(s string) bool {
		return strings.HasPrefix(s, "prod 쓰기 잠금 열기·닫기") && strings.Contains(s, "— ⌘L ·")
	}) {
		t.Error("잠금 항목")
	}
}

func TestEnum_실제_서버에서_사용자_타입_OID로_값을_읽어_고르기_창을_연다(t *testing.T) {
	m := pgModel(t, "it-local")
	it := pgTest{t, m}
	it.sql("drop table if exists public.v7_enum; drop type if exists public.v7_state; create type public.v7_state as enum ('open', 'closed')")
	it.sql("create table public.v7_enum (id int primary key, state public.v7_state)")
	it.sql("insert into public.v7_enum values (1, 'open')")
	m.cur().conn = 0
	drive(m, m.run("select * from public.v7_enum"))
	m.setFocus(fResult)
	m.Update(key("l"))
	m.Update(key("enter"))
	if m.overlay != oPicker || !slices.Equal(m.pickerItems, []string{"open", "closed", "NULL"}) {
		t.Fatalf("고르기: overlay=%v items=%v status=%q", m.overlay, m.pickerItems, m.status)
	}
	m.Update(key("down"))
	m.Update(key("enter"))
	m.Update(key("ctrl+s"))
	_, cmd := m.Update(key("y"))
	drive(m, cmd)
	if got := it.sql("select state::text from public.v7_enum").Rows[0][0]; got != "closed" {
		t.Fatalf("DB: %q status=%q", got, m.status)
	}
}

func TestCompare_실제_서버에서_두_접속을_읽기_전용으로_비교한다(t *testing.T) {
	m := pgModel(t, "it-stg")
	other := m.conn(0)
	other.Name, other.UUID = "it-prod", "it-prod"
	m.proj.Conns = append(m.proj.Conns, other)
	m.cur().conn = 0
	m.cur().editor.SetValue("select 1 as n union all select 2")
	m.Update(keyMsgOf("alt+c"))
	_, cmd := m.Update(key("enter"))
	drive(m, cmd)
	tab := m.cur()
	if len(tab.subs) != 2 || tab.subs[1].res == nil || len(tab.subs[1].res.Rows) != 2 || !strings.Contains(tab.summary, "it-stg 기준 it-prod 비교") {
		t.Fatalf("비교: %+v %q", tab.subs, tab.summary)
	}
}
