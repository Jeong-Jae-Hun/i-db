package grid

import (
	"reflect"
	"strings"
	"testing"
	"time"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
)

// typedResult 는 bool·날짜·enum·json 열이 있는 tasks 테이블 결과다.
func typedResult() *db.Result {
	return &db.Result{
		Cols:  []string{"id", "done", "due", "at", "atz", "state", "meta"},
		Types: []string{"int4", "bool", "date", "timestamp", "timestamptz", "16401", "jsonb"},
		Rows: [][]string{
			{"1", "t", "2026-01-01", "2026-01-01 00:00:00", "2026-01-01 00:00:00+09", "open", `{"a":1}`},
			{"2", "NULL", "NULL", "NULL", "NULL", "NULL", "NULL"},
		},
		Origin: &db.Origin{Table: db.TableRef{Schema: "public", Table: "tasks"}, PK: []string{"id"},
			Cols: []string{"id", "done", "due", "at", "atz", "state", "meta"}},
	}
}

func pasted(t *testing.T, m *Model, text string) PasteResult {
	t.Helper()
	p, err := m.Paste(text)
	if err != nil {
		t.Fatalf("붙여넣기 실패: %v", err)
	}
	return p
}

func TestParseTSV_시트가_만든_따옴표_칸과_줄바꿈을_읽는다(t *testing.T) {
	cases := []struct {
		in   string
		want [][]string
	}{
		{"a\tb\r\nc\td\r\n", [][]string{{"a", "b"}, {"c", "d"}}},
		{"a\n\nb\n", [][]string{{"a"}, {""}, {"b"}}},
		{"\"x\ty\"\t\"줄\r\n바꿈\"\n", [][]string{{"x\ty", "줄\n바꿈"}}},
		{"\"줄\r바꿈\r\n둘\"\tz\n", [][]string{{"줄\n바꿈\n둘", "z"}}},
		{"\"say \"\"hi\"\"\"\tz", [][]string{{`say "hi"`, "z"}}},
		{"\"열린\tq", [][]string{{`"열린`, "q"}}},
		{"5\" 화면\t\t", [][]string{{`5" 화면`, "", ""}}},
		{"\n", [][]string{{""}}},
	}
	for _, c := range cases {
		got, ok := parseTSV(c.in, maxPasteCells)
		if !ok || !reflect.DeepEqual(got, c.want) {
			t.Errorf("%q → %q (ok=%v), 기대 %q", c.in, got, ok, c.want)
		}
	}
	if _, ok := parseTSV("a\tb\tc", 2); ok {
		t.Fatal("한도를 넘으면 거짓이어야 한다")
	}
}

func TestPaste_커서부터_덮어쓰고_넘친_줄은_추가_행이_되어_제출된다(t *testing.T) {
	m := newGrid(editableResult(), [2]int{80, 10})
	m, _ = press(m, "j", "l") // bob 행의 name
	p := pasted(t, &m, "Bo\tSeoul\r\nCa\tNULL\r\nDan\t\r\n")
	if p != (PasteResult{Cells: 6, Added: 1}) {
		t.Fatalf("결과 %+v", p)
	}
	if !strings.Contains(p.Status(), "6칸") || !strings.Contains(p.Status(), "새 행 1개") {
		t.Fatalf("상태 글 %q", p.Status())
	}
	if got := m.current(1); got[1] != "Bo" || got[2] != "Seoul" {
		t.Fatalf("bob 행 %v", got)
	}
	// carol 의 city 는 원래 NULL 이라 NULL 붙여넣기는 바꿈이 아니다
	if e := m.edits[2]; len(e.vals) != 1 || *e.vals[1] != "Ca" {
		t.Fatalf("carol 바꿈 %+v", e)
	}
	cs := submit(t, m)
	ops := []db.ChangeOp{}
	for _, c := range cs.Changes {
		ops = append(ops, c.Op)
	}
	if !reflect.DeepEqual(ops, []db.ChangeOp{db.ChangeUpdate, db.ChangeUpdate, db.ChangeInsert}) {
		t.Fatalf("묶음 %v", ops)
	}
	ins := cs.Changes[2].Values
	if len(ins) != 2 || *ins["name"] != "Dan" || *ins["city"] != "" {
		t.Fatalf("추가 행 값 %v", ins)
	}
	// 붙인 행도 u 로 되돌린다
	m, _ = press(m, "u") // 커서는 붙여넣기를 시작한 bob 행에 남아 있다
	if m.Pending() != 2 || m.current(1)[1] != "bob" {
		t.Fatalf("되돌리기 뒤 pending=%d %v", m.Pending(), m.current(1))
	}
}

func TestPaste_식_열과_범위_밖은_건너뛰고_삭제_표시_행은_덮지_않는다(t *testing.T) {
	m := newGrid(editableResult(), [2]int{80, 10})
	m, _ = press(m, "j", "d", "k", "l", "l") // bob 삭제 표시, Alice 행 city
	p := pasted(t, &m, "x\tL\tover\ny\tL\n")
	if p != (PasteResult{Cells: 1, Skipped: 4}) {
		t.Fatalf("결과 %+v", p)
	}
	if m.current(0)[2] != "x" || m.current(0)[3] != "L1" || !m.edits[1].deleted || len(m.edits[1].vals) != 0 {
		t.Fatalf("Alice %v bob %+v", m.current(0), m.edits[1])
	}
	// 끝 행 아래로 식 열만 넘치면 빈 추가 행을 만들지 않는다
	m, _ = press(m, "G", "l")
	if p := pasted(t, &m, "a\nb\n"); p.Added != 0 || p.Skipped != 2 || m.inserts != 0 {
		t.Fatalf("식 열만 넘침 %+v inserts=%d", p, m.inserts)
	}
}

func TestPaste_편집할_수_없는_결과와_한도_초과는_막고_아무것도_바꾸지_않는다(t *testing.T) {
	noPK := newGrid(peopleResult(), [2]int{80, 10})
	if _, err := noPK.Paste("a"); err == nil || !strings.Contains(err.Error(), "PK") {
		t.Fatalf("PK 없는 결과: %v", err)
	}
	m := newGrid(editableResult(), [2]int{80, 10})
	if _, err := m.Paste(strings.Repeat("a\t", maxPasteCells)); err == nil || !strings.Contains(err.Error(), "10,000") {
		t.Fatalf("한도: %v", err)
	}
	m.SetDecrypted(func(string) bool { return true }, func(v string) (string, error) { return v, nil })
	if _, err := m.Paste("a"); err == nil || !strings.Contains(err.Error(), "복호화") {
		t.Fatalf("복호화 보기: %v", err)
	}
	m.ClearDecrypted()
	m, _ = press(m, "/")
	if _, err := m.Paste("a"); err == nil {
		t.Fatal("거르기 입력 중에는 막아야 한다")
	}
	if m.Pending() != 0 {
		t.Fatalf("막힌 붙여넣기가 바꿈을 남겼다: %d", m.Pending())
	}
}

func TestPaste_한도_딱_맞는_칸과_따옴표_여러_줄_칸을_새_행으로_넣는다(t *testing.T) {
	m := newGrid(editableResult(), [2]int{80, 10})
	m, _ = press(m, "l")
	p := pasted(t, &m, strings.Repeat("n\n", 3)+"\"첫 줄\n둘째\tcol\"\tc\n")
	if p.Cells != 5 || p.Added != 1 {
		t.Fatalf("결과 %+v", p)
	}
	row := m.current(3)
	if row[1] != "첫 줄\n둘째\tcol" || row[2] != "c" {
		t.Fatalf("새 행 %q", row)
	}
	big := strings.TrimSuffix(strings.Repeat("v\n", maxPasteCells), "\n")
	m = newGrid(editableResult(), [2]int{80, 10})
	if p := pasted(t, &m, big); p.Cells != maxPasteCells || p.Added != maxPasteCells-3 {
		t.Fatalf("한도 딱: %+v", p)
	}
	if cs := submit(t, m); len(cs.Changes) != maxPasteCells {
		t.Fatalf("묶음 %d건", len(cs.Changes))
	}
}

func TestPaste_제출_뒤에_고친_붙여넣기만_남는다(t *testing.T) {
	m := newGrid(editableResult(), [2]int{80, 10})
	m, _ = press(m, "l")
	pasted(t, &m, "A2\n")
	rev := m.Revision()
	m, _ = press(m, "j")
	pasted(t, &m, "B2\n")
	if n := m.ClearSubmitted(rev); n != 0 {
		t.Fatalf("버린 행 %d", n)
	}
	if m.Pending() != 1 || m.res.Rows[0][1] != "A2" || m.current(1)[1] != "B2" {
		t.Fatalf("pending=%d rows=%v", m.Pending(), m.res.Rows)
	}
}

func TestBoolSpace_손대지_않은_칸은_space로_true_false_NULL을_돌린다(t *testing.T) {
	m := newGrid(typedResult(), [2]int{100, 10})
	m, _ = press(m, "l", "enter")
	want := []struct {
		buf  string
		null bool
	}{{"f", false}, {"", true}, {"t", false}, {"f", false}}
	for _, w := range want {
		m, _ = press(m, " ")
		if string(m.edit.buf) != w.buf || m.edit.null != w.null {
			t.Fatalf("space 뒤 %q null=%v, 기대 %q", string(m.edit.buf), m.edit.null, w.buf)
		}
	}
	m, _ = press(m, "enter")
	if m.current(0)[1] != "f" || m.Pending() != 1 {
		t.Fatalf("확정 %v", m.current(0))
	}
	// 글자를 친 뒤의 space 는 그대로 글자다
	m, _ = press(m, "j", "enter", "x", " ")
	if string(m.edit.buf) != "x " {
		t.Fatalf("친 뒤 space %q", string(m.edit.buf))
	}
	// bool 이 아닌 열의 space 는 글자다
	m, _ = press(m, "esc", "k", "h", "enter", " ")
	if string(m.edit.buf) != "1 " {
		t.Fatalf("숫자 열 space %q", string(m.edit.buf))
	}
}

func TestNowToday_날짜_열_타입마다_지역_시각_리터럴로_바꾼다(t *testing.T) {
	loc := time.FixedZone("KST", 9*3600)
	defer func(c func() time.Time) { clock = c }(clock)
	clock = func() time.Time { return time.Date(2026, 11, 15, 14, 30, 5, 123, loc) }
	cases := []struct {
		col        int
		word, want string
	}{
		{2, "today", "2026-11-15"},
		{2, "NOW", "2026-11-15"},
		{3, "now", "2026-11-15 14:30:05"},
		{3, " today ", "2026-11-15 00:00:00"},
		{4, "now", "2026-11-15 14:30:05+09:00"},
		{4, "today", "2026-11-15 00:00:00+09:00"},
		{6, "now", "now"}, // jsonb 는 바꾸지 않는다
	}
	for _, c := range cases {
		m := newGrid(typedResult(), [2]int{120, 10})
		m.col = c.col
		m, _ = press(m, "j", "enter")
		m, _ = press(m, strings.Split(c.word, "")...)
		m, _ = press(m, "enter")
		if got := m.current(1)[c.col]; got != c.want {
			t.Errorf("열 %d %q → %q, 기대 %q", c.col, c.word, got, c.want)
		}
	}
}

func TestPickEnum_enum_칸의_enter는_목록을_청하고_SetCell이_바꿈으로_둔다(t *testing.T) {
	m := newGrid(typedResult(), [2]int{120, 10})
	m.SetEnumTypes(map[string]bool{"16401": true})
	m.SetResult(typedResult()) // 새 결과에도 이어진다
	m.col = 5
	m, a := press(m, "enter")
	if a.PickEnum == nil || *a.PickEnum != (PickEnum{Col: 5, Type: "16401", Current: "open"}) || m.Typing() {
		t.Fatalf("요청 %+v typing=%v", a.PickEnum, m.Typing())
	}
	m, _ = press(m, "j") // 목록이 열린 사이 커서가 움직여도 연 칸에 둔다
	if err := m.SetCell(ptr("closed")); err != nil {
		t.Fatal(err)
	}
	if m.current(0)[5] != "closed" || m.Pending() != 1 {
		t.Fatalf("값 %v", m.current(0))
	}
	if err := m.SetCell(ptr("x")); err == nil {
		t.Fatal("한 요청에 한 번만 받아야 한다")
	}
	cs := submit(t, m)
	if c := cs.Changes[0]; *c.Values["state"] != "closed" || *c.Old["state"] != "open" {
		t.Fatalf("묶음 %+v", c)
	}
	// f2 도 같고, NULL 칸은 Current 가 NULL 이다
	m, a = press(m, "f2")
	if a.PickEnum == nil || a.PickEnum.Current != nullText {
		t.Fatalf("f2 %+v", a.PickEnum)
	}
	// 그사이 삭제 표시하면 막는다
	m, _ = press(m, "d")
	if err := m.SetCell(nil); err == nil || !strings.Contains(err.Error(), "삭제") {
		t.Fatalf("삭제 표시 행: %v", err)
	}
}

func TestEditLong_ctrl_o는_편집기를_청하고_SetCell로_값을_둔다(t *testing.T) {
	m := newGrid(typedResult(), [2]int{120, 10})
	m.col = 6
	m, _ = press(m, "enter", "end", "x")
	m, a := m.Update(tea.KeyMsg{Type: tea.KeyCtrlO})
	if a.EditLong == nil || *a.EditLong != (EditLong{Value: `{"a":1}x`, JSON: true}) || m.Typing() {
		t.Fatalf("요청 %+v typing=%v", a.EditLong, m.Typing())
	}
	if m.Pending() != 0 {
		t.Fatal("편집기를 연 것만으로 바꿈을 두면 안 된다")
	}
	if err := m.SetCell(ptr("{\n  \"a\": 2\n}")); err != nil {
		t.Fatal(err)
	}
	if m.current(0)[6] != "{\n  \"a\": 2\n}" || m.Pending() != 1 {
		t.Fatalf("값 %q", m.current(0)[6])
	}
	// NULL 칸은 Null 로 알리고, 새 결과를 받으면 SetCell 은 실패한다
	m, _ = press(m, "j", "enter")
	m, a = m.Update(tea.KeyMsg{Type: tea.KeyCtrlO})
	if a.EditLong == nil || !a.EditLong.Null || a.EditLong.Value != "" {
		t.Fatalf("NULL 칸 %+v", a.EditLong)
	}
	m.SetResult(typedResult())
	if err := m.SetCell(ptr("x")); err == nil {
		t.Fatal("새 결과 뒤에는 실패해야 한다")
	}
}

func TestEditLong_추가_행을_지운_뒤의_SetCell은_실패한다(t *testing.T) {
	m := newGrid(typedResult(), [2]int{120, 10})
	m, _ = press(m, "a", "l", "enter")
	m, _ = m.Update(tea.KeyMsg{Type: tea.KeyCtrlO})
	m, _ = press(m, "d")
	if err := m.SetCell(ptr("t")); err == nil || m.Pending() != 0 {
		t.Fatalf("err=%v pending=%d", err, m.Pending())
	}
}

func TestEditHint_열_타입에_맞는_도우미_키를_보인다(t *testing.T) {
	m := newGrid(typedResult(), [2]int{200, 10})
	m, a := press(m, "l", "enter")
	if !strings.Contains(a.Status, "space 참/거짓/NULL") || !strings.Contains(a.Status, "ctrl+o") {
		t.Fatalf("bool 안내 %q", a.Status)
	}
	m, _ = press(m, "esc", "l")
	if _, a = press(m, "enter"); !strings.Contains(a.Status, "now·today") {
		t.Fatalf("날짜 안내 %q", a.Status)
	}
}
