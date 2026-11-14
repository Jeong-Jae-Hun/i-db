package grid

import (
	"reflect"
	"strings"
	"testing"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
	"github.com/charmbracelet/x/ansi"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
)

var peopleTable = db.TableRef{Schema: "public", Table: "people"}

// editableResult 는 people 테이블 결과다. 마지막 열 label 은 식 열이다.
func editableResult() *db.Result {
	r := peopleResult()
	r.Cols = append(r.Cols, "label")
	r.Types = append(r.Types, "text")
	for i := range r.Rows {
		r.Rows[i] = append(r.Rows[i], "L"+r.Rows[i][0])
	}
	r.Origin = &db.Origin{Table: peopleTable, PK: []string{"id"}, Cols: []string{"id", "name", "city", ""}}
	return r
}

func ptr(s string) *string {
	return &s
}

// submit 은 ctrl+s 로 만든 묶음을 돌려준다.
func submit(t *testing.T, m Model) db.ChangeSet {
	t.Helper()
	_, a := press(m, "ctrl+s")
	if a.Submit == nil {
		t.Fatalf("제출 묶음이 없다: %+v", a)
	}
	return *a.Submit
}

func TestEdit_enter로_고치고_확정하면_원래_PK로_update를_만든다(t *testing.T) {
	m := newGrid(editableResult(), [2]int{80, 10})
	m, a := press(m, "l", "enter")
	if !m.Typing() || !strings.Contains(a.Status, "편집") {
		t.Fatalf("편집이 시작되지 않았다: %v %q", m.Typing(), a.Status)
	}
	if string(m.edit.buf) != "Alice" || m.edit.cur != 5 {
		t.Fatalf("원래 값으로 채워야 한다: %q %d", string(m.edit.buf), m.edit.cur)
	}
	m, _ = press(m, "ctrl+a", "delete", "delete", "delete", "delete", "delete", "Z", "e", "d", "enter")
	if m.Typing() || m.Pending() != 1 || m.current(0)[1] != "Zed" {
		t.Fatalf("확정 결과: typing=%v pending=%d %v", m.Typing(), m.Pending(), m.current(0))
	}
	if m.res.Rows[0][1] != "Alice" {
		t.Fatal("결과 원본을 바꾸면 안 된다")
	}
	want := db.ChangeSet{Table: peopleTable, PK: []string{"id"}, Changes: []db.Change{
		{Op: db.ChangeUpdate, Key: map[string]string{"id": "1"}, Values: map[string]*string{"name": ptr("Zed")}, Old: map[string]*string{"name": ptr("Alice")}},
	}}
	if got := submit(t, m); !reflect.DeepEqual(got, want) {
		t.Fatalf("묶음: %+v", got)
	}
}

func TestEdit_esc는_취소하고_바꾸지_않은_확정과_원래_값_복귀는_변경이_아니다(t *testing.T) {
	m := newGrid(editableResult(), [2]int{80, 10})
	m, _ = press(m, "l", "enter", "x", "esc")
	if m.Pending() != 0 || m.Typing() {
		t.Fatalf("esc 뒤 pending=%d", m.Pending())
	}
	m, _ = press(m, "f2", "enter")
	if m.Pending() != 0 {
		t.Fatal("바꾸지 않은 확정을 기록했다")
	}
	m, _ = press(m, "f2", "x", "enter")
	m, _ = press(m, "f2", "backspace", "enter")
	if m.Pending() != 0 {
		t.Fatalf("원래 값으로 돌아왔는데 pending=%d", m.Pending())
	}
}

func TestEdit_ctrl_n은_NULL이고_NULL_칸은_빈_입력에서_시작한다(t *testing.T) {
	m := newGrid(editableResult(), [2]int{80, 10})
	m, _ = press(m, "l", "enter", "ctrl+n", "enter")
	if m.current(0)[1] != nullText {
		t.Fatalf("NULL 이 아니다: %v", m.current(0))
	}
	cs := submit(t, m)
	if v, ok := cs.Changes[0].Values["name"]; !ok || v != nil {
		t.Fatalf("NULL 은 nil 값이어야 한다: %+v", cs.Changes[0].Values)
	}
	// 3번 행 city 는 원래 NULL 이다. NULL 로 다시 정하면 바꿈이 아니다
	m, _ = press(m, "G", "l", "enter")
	if !m.edit.null || len(m.edit.buf) != 0 {
		t.Fatalf("NULL 칸 편집 시작: null=%v %q", m.edit.null, string(m.edit.buf))
	}
	m, _ = press(m, "ctrl+n", "enter")
	if m.Pending() != 1 {
		t.Fatalf("NULL→NULL 을 기록했다: %d", m.Pending())
	}
	m, _ = press(m, "enter", "x", "enter")
	if m.current(2)[2] != "x" || m.Pending() != 2 {
		t.Fatalf("NULL 칸에 글자: %v", m.current(2))
	}
}

func TestEdit_정렬_거르기_뒤에도_바꿈은_원래_행을_따라간다(t *testing.T) {
	m := newGrid(editableResult(), [2]int{80, 10})
	// name 내림차순: carol, bob, Alice → 첫 행은 carol(id 3)
	m, _ = press(m, "l", "s", "s")
	m, _ = press(m, "g", "enter", "!", "enter")
	m, _ = press(m, "/", "b", "o", "b", "enter", "l", "enter", "?", "enter", "esc")
	m, _ = press(m, "h", "s")
	cs := submit(t, m)
	want := []db.Change{
		{Op: db.ChangeUpdate, Key: map[string]string{"id": "2"}, Values: map[string]*string{"city": ptr("Busan?")}, Old: map[string]*string{"city": ptr("Busan")}},
		{Op: db.ChangeUpdate, Key: map[string]string{"id": "3"}, Values: map[string]*string{"name": ptr("carol!")}, Old: map[string]*string{"name": ptr("carol")}},
	}
	if !reflect.DeepEqual(cs.Changes, want) {
		t.Fatalf("묶음: %+v", cs.Changes)
	}
	if m.current(2)[1] != "carol!" || m.current(1)[2] != "Busan?" {
		t.Fatal("정렬을 풀어도 바꾼 값이 원래 행에 있어야 한다")
	}
}

func TestEdit_a는_끝에_행을_더하고_정한_값만_insert한다(t *testing.T) {
	m := newGrid(editableResult(), [2]int{80, 10})
	m, _ = press(m, "/", "z", "z", "enter")
	m, a := press(m, "a")
	if len(m.view) != 1 || m.view[0] != 3 || m.row != 0 || !strings.Contains(a.Status, "행 추가") {
		t.Fatalf("거른 뒤에도 추가 행이 보여야 한다: %v %q", m.view, a.Status)
	}
	if out := ansi.Strip(m.View(true)); !strings.Contains(out, defaultText) || !strings.Contains(out, "+") {
		t.Fatalf("추가 행 표시가 없다:\n%s", out)
	}
	m, _ = press(m, "l", "enter", "d", "a", "n", "enter", "l", "enter", "ctrl+n", "enter")
	cs := submit(t, m)
	want := []db.Change{{Op: db.ChangeInsert, Values: map[string]*string{"name": ptr("dan"), "city": nil}}}
	if !reflect.DeepEqual(cs.Changes, want) {
		t.Fatalf("insert: %+v", cs.Changes)
	}
}

func TestEdit_ctrl_d는_PK와_식_열을_빼고_행을_복제한다(t *testing.T) {
	m := newGrid(editableResult(), [2]int{80, 10})
	m, _ = press(m, "G", "ctrl+d")
	if m.row != 3 || m.Pending() != 1 {
		t.Fatalf("복제 행으로 가야 한다: row=%d pending=%d", m.row, m.Pending())
	}
	cs := submit(t, m)
	want := []db.Change{{Op: db.ChangeInsert, Values: map[string]*string{"name": ptr("carol"), "city": nil}}}
	if !reflect.DeepEqual(cs.Changes, want) {
		t.Fatalf("복제: %+v", cs.Changes)
	}
}

func TestEdit_d는_삭제_표시를_토글하고_추가_행은_지우며_u는_행을_되돌린다(t *testing.T) {
	m := newGrid(editableResult(), [2]int{80, 10})
	m, _ = press(m, "j", "l", "enter", "!", "enter", "d")
	if out := ansi.Strip(m.View(true)); !strings.Contains(out, "−") {
		t.Fatalf("삭제 표시가 없다:\n%s", out)
	}
	m2, a := press(m, "f2")
	if m2.Typing() || !strings.Contains(a.Status, "삭제 표시") {
		t.Fatalf("이유: %q", a.Status)
	}
	cs := submit(t, m)
	// 삭제의 옛값은 PK·식 열을 뺀 테이블 열의 편집 전 값이다
	want := []db.Change{{Op: db.ChangeDelete, Key: map[string]string{"id": "2"}, Old: map[string]*string{"name": ptr("bob"), "city": ptr("Busan")}}}
	if !reflect.DeepEqual(cs.Changes, want) {
		t.Fatalf("delete: %+v", cs.Changes)
	}
	// 해제하면 삭제 전 고친 값이 남는다
	m, _ = press(m, "d")
	if m.Pending() != 1 || m.current(1)[1] != "bob!" {
		t.Fatalf("해제 뒤: %d %v", m.Pending(), m.current(1))
	}
	m, _ = press(m, "u")
	if m.Pending() != 0 || m.current(1)[1] != "bob" {
		t.Fatal("u 가 되돌리지 않았다")
	}
	if _, a = press(m, "u"); a.Status != "되돌릴 변경 없음" {
		t.Fatalf("u 상태: %q", a.Status)
	}
	m, _ = press(m, "a", "d")
	if m.Pending() != 0 || len(m.view) != 3 {
		t.Fatalf("추가 행 d: %d %v", m.Pending(), m.view)
	}
	m, _ = press(m, "a", "u")
	if m.Pending() != 0 || len(m.view) != 3 {
		t.Fatalf("추가 행 u: %d %v", m.Pending(), m.view)
	}
}

func TestEdit_제출_순서는_삭제_수정_추가다(t *testing.T) {
	m := newGrid(editableResult(), [2]int{80, 10})
	m, _ = press(m, "a", "l", "enter", "n", "enter")
	m, _ = press(m, "g", "enter", "x", "enter")
	m, _ = press(m, "G", "k", "d")
	var ops []db.ChangeOp
	for _, c := range submit(t, m).Changes {
		ops = append(ops, c.Op)
	}
	if !reflect.DeepEqual(ops, []db.ChangeOp{db.ChangeDelete, db.ChangeUpdate, db.ChangeInsert}) {
		t.Fatalf("순서: %v", ops)
	}
}

func TestEdit_읽기_전용_결과는_편집_키에_이유만_알린다(t *testing.T) {
	m := newGrid(peopleResult(), [2]int{80, 10})
	for _, k := range []string{"f2", "a", "d", "u", "i", "U"} {
		g, a := press(m, k)
		if !strings.Contains(a.Status, "PK 를 모르는") || g.Typing() || g.Pending() != 0 || a.CopyAs != nil {
			t.Fatalf("%s: %+v", k, a)
		}
	}
	if g, _ := press(m, "enter"); g.mode != modeCell {
		t.Fatal("읽기 전용 결과의 enter 는 전체 값 보기여야 한다")
	}
	if g, _ := press(m, "ctrl+d"); g.row == 0 {
		t.Fatal("읽기 전용 결과의 ctrl+d 는 스크롤이어야 한다")
	}
	if g, a := press(m, "ctrl+s"); !reflect.DeepEqual(a, Action{}) || g.row != m.row {
		t.Fatalf("바꿈이 없으면 ctrl+s 를 처리하지 않아야 한다: %+v", a)
	}

	r := editableResult()
	r.Origin.PK = []string{"uuid"}
	if why := newGrid(r, [2]int{80, 10}).cellBlock(1); !strings.Contains(why, "PK 열") {
		t.Fatalf("PK 열이 빠진 결과: %q", why)
	}
	r.Origin = &db.Origin{Table: peopleTable, PK: []string{"id"}, Cols: []string{"id"}}
	if why := newGrid(r, [2]int{80, 10}).cellBlock(1); !strings.Contains(why, "PK 열") {
		t.Fatalf("열 수가 다른 Origin: %q", why)
	}
}

func TestEdit_식_열은_편집하지_않고_enter는_전체_값을_연다(t *testing.T) {
	m := newGrid(editableResult(), [2]int{80, 10})
	m, _ = press(m, "$")
	if g, a := press(m, "f2"); g.Typing() || !strings.Contains(a.Status, "식") {
		t.Fatalf("식 열 f2: %q", a.Status)
	}
	if g, _ := press(m, "enter"); g.mode != modeCell || g.Typing() {
		t.Fatal("식 열 enter 는 전체 값 보기여야 한다")
	}
	if g, _ := press(m, "h", "o"); g.mode != modeCell {
		t.Fatal("o 는 편집할 수 있는 칸에서도 전체 값 보기다")
	}
}

func TestEdit_i_U는_원래_행_인덱스로_SQL_복사를_요청한다(t *testing.T) {
	m := newGrid(editableResult(), [2]int{80, 10})
	m, _ = press(m, "l", "s", "s", "g")
	if _, a := press(m, "i"); a.CopyAs == nil || !reflect.DeepEqual(*a.CopyAs, CopyAs{Kind: "insert", Rows: []int{2}}) {
		t.Fatalf("i: %+v", a.CopyAs)
	}
	if _, a := press(m, "j", "U"); a.CopyAs == nil || !reflect.DeepEqual(*a.CopyAs, CopyAs{Kind: "update", Rows: []int{1}}) {
		t.Fatalf("U: %+v", a.CopyAs)
	}
	if _, a := press(m, "a", "i"); a.CopyAs != nil || !strings.Contains(a.Status, "추가한 행") {
		t.Fatalf("추가 행 i: %+v", a)
	}
}

func TestEdit_ClearChanges와_SetResult는_바꿈을_버린다(t *testing.T) {
	m := newGrid(editableResult(), [2]int{80, 10})
	m, _ = press(m, "l", "enter", "x", "enter", "a")
	m.ClearChanges()
	if m.Pending() != 0 || len(m.view) != 3 || m.current(0)[1] != "Alice" {
		t.Fatalf("ClearChanges: %d %v", m.Pending(), m.view)
	}
	if _, a := press(m, "ctrl+s"); a.Submit != nil {
		t.Fatal("비운 뒤 제출했다")
	}
	m, _ = press(m, "enter", "x", "enter")
	m.SetResult(editableResult())
	if m.Pending() != 0 {
		t.Fatal("SetResult 가 바꿈을 남겼다")
	}
}

func TestEdit_편집_중에는_글자키를_받고_StopTyping과_클릭은_값을_확정한다(t *testing.T) {
	m := newGrid(editableResult(), [2]int{80, 10})
	m, _ = press(m, "l", "enter", "q", "j", "tab")
	if !m.Typing() || string(m.edit.buf) != "Aliceqj" {
		t.Fatalf("글자를 넣어야 한다: %q", string(m.edit.buf))
	}
	g := m
	g.StopTyping()
	if g.Typing() || g.current(0)[1] != "Aliceqj" {
		t.Fatalf("StopTyping: %v", g.current(0))
	}
	m.HandleMouse(tea.MouseMsg{X: 1, Y: 5, Button: tea.MouseButtonLeft, Action: tea.MouseActionPress})
	if m.Typing() || m.current(0)[1] != "Aliceqj" {
		t.Fatalf("클릭: %v", m.current(0))
	}
}

func TestEdit_커서_이동과_여러_줄_값을_지킨다(t *testing.T) {
	r := editableResult()
	r.Rows[0][1] = "a\nb"
	m := newGrid(r, [2]int{80, 10})
	m, _ = press(m, "l", "enter", "home", "right", "X", "end", "left", "Y", "ctrl+e", "가", "enter")
	if got := m.current(0)[1]; got != "aX\nYb가" {
		t.Fatalf("값: %q", got)
	}
}

func TestEdit_편집_칸과_바뀐_칸을_폭_안에_그린다(t *testing.T) {
	r := editableResult()
	r.Rows[0][1] = "가나다라마바사아자차카타파하\x1b[31m"
	m := newGrid(r, [2]int{60, 8})
	m.widths[1] = 7
	m, _ = press(m, "l", "enter")
	out := ansi.Strip(m.View(true))
	// 커서가 끝에 있어 앞을 잘라 내고, ESC 는 보이는 글자(\x1b)로 그린다
	if !strings.Contains(out, "편집 중") || !strings.Contains(out, `1b[31m`) || strings.Contains(m.View(true), "\x1b[31m") {
		t.Fatalf("편집 칸·안내:\n%s", out)
	}
	checkBounds(t, m)
	m, _ = press(m, "home", "right", "right")
	checkBounds(t, m)
	if out = ansi.Strip(m.View(true)); !strings.Contains(out, "가나다") {
		t.Fatalf("앞쪽 커서:\n%s", out)
	}
	m, _ = press(m, "x", "enter", "j", "d")
	out = ansi.Strip(m.View(true))
	if !strings.Contains(out, "*") || !strings.Contains(out, "변경 2건 · ctrl+s 제출") {
		t.Fatalf("표시:\n%s", out)
	}
	checkBounds(t, m)
}

func TestEdit_아주_작은_크기에서도_편집_키가_크기를_지킨다(t *testing.T) {
	keys := []string{"a", "v", "esc", "o", "esc", "enter", "가", "enter", "l", "f2", "ctrl+n", "enter", "ctrl+d", "d", "k", "d", "enter", "x", "u", "G", "$", "enter"}
	for w := 0; w <= 12; w++ {
		for h := 0; h <= 8; h++ {
			m := newGrid(editableResult(), [2]int{w, h})
			for _, k := range keys {
				m, _ = press(m, k)
				checkBounds(t, m)
			}
		}
	}
}

func FuzzEdit_어떤_키_순서에도_패닉없이_묶음이_바꿈_수와_맞는다(f *testing.F) {
	f.Add("lx\nax\n\x04d\x15", uint8(40), uint8(8))
	f.Add("\x02\x0e\n/b\nl\x02z\x13", uint8(10), uint8(3))
	f.Add("lx\nVjdua\x04\x1bc\x00c=", uint8(40), uint8(8))
	f.Add("lDx\n\x02lDCVjC\x1bFR+-ax\nD\x13", uint8(40), uint8(8))
	// v7: \x10 은 키 글 전체를 붙여넣기, \x11 은 text 열을 enum 으로, \x0f 는 ctrl+o
	f.Add("jl\x10a\tb\n\"c\td\n\"\tNULL\nx\x13", uint8(40), uint8(8))
	f.Add("\x11l\nGl\n\x0f\x13d\x10", uint8(40), uint8(8))
	f.Add("ljd\x10\"\"\"\t\r\n\r\nDx\x10u\x13", uint8(30), uint8(6))
	f.Fuzz(func(t *testing.T, keys string, w, h uint8) {
		names := map[rune]string{
			'\n': "enter", '\x1b': "esc", '\x7f': "backspace", '\x02': "f2", '\x0e': "ctrl+n",
			'\x13': "ctrl+s", '\x04': "ctrl+d", '\x01': "home", '\x05': "end", '\x15': "left", '\x16': "right",
			'\x0f': "ctrl+o",
		}
		m := newGrid(editableResult(), [2]int{int(w % 100), int(h % 30)})
		for _, k := range keys {
			name, ok := names[k]
			if !ok {
				name = string(k)
			}
			switch k {
			case '\x00':
				m.HandleMouse(tea.MouseMsg{X: int(w) / 3, Y: int(h) / 2, Button: tea.MouseButtonLeft, Action: tea.MouseActionPress})
				continue
			case '\x10':
				before := m.Pending()
				if _, err := m.Paste(keys); err != nil && m.Pending() != before {
					t.Fatalf("실패한 붙여넣기가 바꿈을 남겼다: %d → %d", before, m.Pending())
				}
				continue
			case '\x11':
				m.SetEnumTypes(map[string]bool{"text": true})
				continue
			}
			var a Action
			m, a = press(m, name)
			// 통합처럼 고르기 목록·편집기에서 값을 골라 넘긴다
			if a.PickEnum != nil || a.EditLong != nil {
				_ = m.SetCell(ptr("v"))
			}
			// 통합처럼 D 로 복호화 보기를 켜고 끈다. 앞이 소문자인 칸을 대문자로 "푼다"
			if a.Decrypt && m.Decrypted() {
				m.ClearDecrypted()
			} else if a.Decrypt {
				m.SetDecrypted(func(v string) bool { return v != "" && v[0] >= 'a' && v[0] <= 'z' }, func(v string) (string, error) { return strings.ToUpper(v), nil })
			}
			if a.Submit != nil && len(a.Submit.Changes) != m.Pending() {
				t.Fatalf("묶음 %d건, pending %d", len(a.Submit.Changes), m.Pending())
			}
		}
		checkBounds(t, m)
		for _, c := range m.changeSet().Changes {
			if c.Op != db.ChangeInsert && c.Key["id"] == "" {
				t.Fatalf("PK 없는 바꿈: %+v", c)
			}
		}
	})
}

func TestCellEditRender_한글_값도_커서_자리와_상관없이_폭을_지키고_커서가_보인다(t *testing.T) {
	buf := []rune("가나다라마바사\n아x")
	for w := 0; w <= 12; w++ {
		for cur := 0; cur <= len(buf); cur++ {
			out := cellEdit{buf: buf, cur: cur}.render(w, lipgloss.NewStyle())
			if got := ansi.StringWidth(out); got != w+2 {
				t.Fatalf("w=%d cur=%d 폭 %d: %q", w, cur, got, ansi.Strip(out))
			}
			if cur < len(buf) && w >= 2 && !strings.Contains(ansi.Strip(out), oneLine(string(buf[cur]))) {
				t.Fatalf("w=%d cur=%d 커서 글자가 안 보인다: %q", w, cur, ansi.Strip(out))
			}
		}
	}
	if out := ansi.Strip(cellEdit{null: true}.render(6, lipgloss.NewStyle())); !strings.Contains(out, nullText) {
		t.Fatalf("NULL 자리 표시: %q", out)
	}
}

func TestEdit_옛값은_NULL_칸을_nil로_싣고_잘린_칸은_뺀다(t *testing.T) {
	r := editableResult()
	r.Rows[1][1] = strings.Repeat("x", db.MaxCellBytes+20) // 잘려서 실린 칸
	m := newGrid(r, [2]int{80, 10})
	// 3번 행 city(NULL)를 고치고 2번 행을 지운다
	m, _ = press(m, "G", "l", "l", "enter", "x", "enter")
	m, _ = press(m, "k", "d")
	want := []db.Change{
		{Op: db.ChangeDelete, Key: map[string]string{"id": "2"}, Old: map[string]*string{"city": ptr("Busan")}},
		{Op: db.ChangeUpdate, Key: map[string]string{"id": "3"}, Values: map[string]*string{"city": ptr("x")}, Old: map[string]*string{"city": nil}},
	}
	if got := submit(t, m).Changes; !reflect.DeepEqual(got, want) {
		t.Fatalf("묶음: %+v", got)
	}
}

func TestEdit_추가_행을_복제하면_정하지_않은_칸은_DEFAULT_로_남는다(t *testing.T) {
	m := newGrid(editableResult(), [2]int{80, 10})
	// 추가 행에 id 와 name 만 정하고 복제한다. PK 는 비우고 city 는 정하지 않은 채로 둔다
	m, _ = press(m, "a", "enter", "9", "enter", "l", "enter", "z", "enter", "ctrl+d")
	cs := submit(t, m)
	want := []db.Change{
		{Op: db.ChangeInsert, Values: map[string]*string{"id": ptr("9"), "name": ptr("z")}},
		{Op: db.ChangeInsert, Values: map[string]*string{"name": ptr("z")}},
	}
	if !reflect.DeepEqual(cs.Changes, want) {
		t.Fatalf("묶음: %+v", cs.Changes)
	}
	// 복사·내보내기는 정하지 않은 칸을 NULL 이 아니라 빈 값으로 싣는다
	if _, a := press(m, "Y"); a.Copy == nil || *a.Copy != "\tz\t\t" {
		t.Fatalf("행 복사: %q", *a.Copy)
	}
	_, a := press(m, "e")
	if a.Export == nil || strings.Contains(a.Export.Content, "NULL") || !strings.Contains(a.Export.Content, ",z,") {
		t.Fatalf("내보내기: %+v", a.Export)
	}
	// 레코드 보기도 표처럼 DEFAULT 로 그린다
	m, _ = press(m, "v")
	if out := ansi.Strip(m.View(true)); !strings.Contains(out, defaultText) || strings.Contains(out, nullText) {
		t.Fatalf("레코드 보기:\n%s", out)
	}
}

func TestEdit_레코드_보기에서_추가_행만_있을_때_바꿈을_버려도_패닉하지_않는다(t *testing.T) {
	m := newGrid(editableResult(), [2]int{80, 10})
	m, _ = press(m, "/", "z", "z", "enter", "a", "v")
	if m.mode != modeDetail || len(m.view) != 1 {
		t.Fatalf("전제: mode=%v view=%v", m.mode, m.view)
	}
	m.ClearChanges()
	if m.mode != modeTable || m.row != 0 {
		t.Fatalf("빈 표에서는 표 보기로 돌아가야 한다: mode=%v row=%d", m.mode, m.row)
	}
	checkBounds(t, m)
	m, _ = press(m, "o")
	checkBounds(t, m)
}

func TestEdit_ClearSubmitted는_낸_바꿈만_지우고_뒤에_고친_칸은_낸_값을_원래_값으로_삼는다(t *testing.T) {
	r := editableResult()
	m := newGrid(r, [2]int{80, 10})
	// 제출: 행 0 name=Alicex, 행 1 삭제 표시, 행 2 city 수정
	m, _ = press(m, "l", "f2", "x", "enter", "j", "d", "j", "l", "f2", "x", "enter")
	if m.Pending() != 3 {
		t.Fatalf("전제: pending=%d", m.Pending())
	}
	rev := m.Revision()
	// 제출하는 사이: 행 0 name 을 다시, 행 0 city 도 고친다
	m, _ = press(m, "g", "h", "f2", "x", "enter", "l", "f2", "y", "enter")
	if n := m.ClearSubmitted(rev); n != 0 {
		t.Fatalf("버린 행 없음: %d", n)
	}
	want := db.ChangeSet{Table: peopleTable, PK: []string{"id"}, Changes: []db.Change{
		{Op: db.ChangeUpdate, Key: map[string]string{"id": "1"},
			Values: map[string]*string{"name": ptr("Alicexx"), "city": ptr("seouly")},
			Old:    map[string]*string{"name": ptr("Alicex"), "city": ptr("seoul")}},
	}}
	if got := submit(t, m); !reflect.DeepEqual(got, want) {
		t.Fatalf("남은 묶음: %+v", got)
	}
	if !reflect.DeepEqual(m.view, []int{0, 2}) || m.res.Rows[2][2] != "x" {
		t.Fatalf("낸 삭제 행은 감추고 낸 수정은 원래 값이 된다: view=%v row2=%v", m.view, m.res.Rows[2])
	}
	if r.Rows[0][1] != "Alice" {
		t.Fatal("통합과 함께 쓰는 결과 원본을 바꾸면 안 된다")
	}
}

func TestEdit_ClearSubmitted는_낸_뒤에_고친_추가_행을_버리고_수를_돌려준다(t *testing.T) {
	m := newGrid(editableResult(), [2]int{80, 10})
	m, _ = press(m, "a", "l", "f2", "D", "a", "n", "enter")
	rev := m.Revision()
	m, _ = press(m, "f2", "x", "enter", "a")
	if n := m.ClearSubmitted(rev); n != 1 {
		t.Fatalf("낸 뒤 고친 추가 행 1개를 버린다: %d", n)
	}
	// 낸 뒤에 만든 추가 행은 남는다
	if m.Pending() != 1 || len(m.view) != 4 {
		t.Fatalf("pending=%d view=%v", m.Pending(), m.view)
	}
	cs := submit(t, m)
	if len(cs.Changes) != 1 || cs.Changes[0].Op != db.ChangeInsert || len(cs.Changes[0].Values) != 0 {
		t.Fatalf("새 추가 행만 간다: %+v", cs)
	}
}
