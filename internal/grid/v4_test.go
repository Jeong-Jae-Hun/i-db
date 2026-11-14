package grid

import (
	"reflect"
	"strings"
	"testing"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/x/ansi"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
)

// editedGrid 는 첫 행 name 을 AliceZ 로 고친 편집 가능한 표다.
func editedGrid(t *testing.T, size [2]int) Model {
	t.Helper()
	m := newGrid(editableResult(), size)
	m, _ = press(m, "l", "enter", "Z", "enter", "h")
	if m.Pending() != 1 {
		t.Fatalf("준비한 바꿈 수 %d", m.Pending())
	}
	return m
}

func TestChart_c는_차트로_바꾸고_c_esc로_돌아오며_숫자_열이_없으면_표에_남는다(t *testing.T) {
	m := newGrid(peopleResult(), [2]int{50, 12})
	m, a := press(m, "c")
	if m.mode != modeChart || a.Status != "" {
		t.Fatalf("차트 모드가 아니다: %v %q", m.mode, a.Status)
	}
	out := ansi.Strip(m.View(true))
	if !strings.Contains(out, "값: id · 이름표: name") || !strings.Contains(out, "c·esc 표로") {
		t.Fatalf("차트 화면:\n%s", out)
	}
	checkBounds(t, m)
	for _, k := range []string{"c", "esc"} {
		g, _ := press(m, k)
		if g.mode != modeTable {
			t.Fatalf("%s 로 표에 돌아가야 한다", k)
		}
	}

	text := &db.Result{Cols: []string{"name"}, Types: []string{"text"}, Rows: [][]string{{"a"}}}
	g, a := press(newGrid(text, [2]int{50, 12}), "c")
	if g.mode != modeTable || a.Status != "차트로 그릴 숫자 열이 없음" {
		t.Fatalf("숫자 열이 없으면 표에 남고 이유를 알려야 한다: %v %q", g.mode, a.Status)
	}
}

func TestChart_거르기_순서를_따르고_크기가_바뀌면_다시_그린다(t *testing.T) {
	m := newGrid(peopleResult(), [2]int{50, 12})
	m, _ = press(m, "/", "b", "o", "b", "enter", "c")
	out := ansi.Strip(m.View(true))
	if !strings.Contains(out, "bob") || strings.Contains(out, "Alice") {
		t.Fatalf("거른 행만 그려야 한다:\n%s", out)
	}
	m.SetSize(20, 4)
	checkBounds(t, m)
	m.SetSize(8, 2)
	checkBounds(t, m)
}

func TestSelect_V는_사각형을_넓히며_합계를_보이고_y는_TSV로_복사하고_esc로_끝난다(t *testing.T) {
	m := newGrid(numbersResult(5, 3), [2]int{60, 12})
	m, a := press(m, "V")
	if !m.sel || a.Status != "칸 1 · 합 0 · 평균 0 · 최소 0 · 최대 0" {
		t.Fatalf("선택 시작: %v %q", m.sel, a.Status)
	}
	m, a = press(m, "j", "l")
	if a.Status != "칸 4 · 합 202 · 평균 50.5 · 최소 0 · 최대 101" {
		t.Fatalf("집계: %q", a.Status)
	}
	if out := ansi.Strip(m.View(true)); !strings.Contains(out, "선택 2×2 · y 복사 · C IN 목록 · esc 끝") {
		t.Fatalf("안내 줄:\n%s", out)
	}
	checkBounds(t, m)
	_, a = press(m, "y")
	if got := copied(t, a); got != "0\t1\n100\t101" {
		t.Fatalf("TSV: %q", got)
	}
	// 시작점 위·왼쪽으로 넘어가도 사각형이다
	m, _ = press(m, "k", "k", "h", "h")
	if r0, r1, c0, c1 := m.selRect(); r0 != 0 || r1 != 0 || c0 != 0 || c1 != 0 {
		t.Fatalf("범위: %d %d %d %d", r0, r1, c0, c1)
	}
	for _, k := range []string{"esc", "V"} {
		g, a := press(m, k)
		if g.sel || a.Status != "선택 끝" {
			t.Fatalf("%s 로 선택을 끝내야 한다: %q", k, a.Status)
		}
	}
}

func TestSelect_글자_칸은_칸_수만_세고_복사는_탭_줄바꿈을_이스케이프한다(t *testing.T) {
	r := peopleResult()
	r.Rows[0][1] = "A\tB"
	m := newGrid(r, [2]int{60, 12})
	m, a := press(m, "l", "V", "l")
	if a.Status != "칸 2" {
		t.Fatalf("집계: %q", a.Status)
	}
	_, a = press(m, "y")
	if got := copied(t, a); got != `A\tB`+"\tseoul" {
		t.Fatalf("TSV: %q", got)
	}
}

func TestCompare_등호는_비교를_청하고_ShowDiff는_요약과_바뀐_행을_보인다(t *testing.T) {
	m := newGrid(editableResult(), [2]int{70, 12})
	_, a := press(m, "=")
	if !a.Compare {
		t.Fatal("= 는 Compare 를 청해야 한다")
	}
	cur := editableResult()
	cur.Rows[1][2] = "Daegu"
	cur.Rows[2] = []string{"4", "dan", "x", "L4"}
	m = newGrid(cur, [2]int{70, 12})
	if err := m.ShowDiff(editableResult()); err != nil {
		t.Fatal(err)
	}
	out := ansi.Strip(m.View(true))
	for _, want := range []string{"+1 −1 ~1", "~ │ 2", "Daegu", "+ │ 4", "− │ 3", "esc 닫기"} {
		if !strings.Contains(out, want) {
			t.Fatalf("%q 가 없다:\n%s", want, out)
		}
	}
	checkBounds(t, m)
	m, _ = press(m, "j", "j", "j")
	if m.diff.row != 2 {
		t.Fatalf("j 는 비교 줄을 옮긴다: %d", m.diff.row)
	}
	m, _ = press(m, "esc")
	if m.mode != modeTable {
		t.Fatal("esc 로 표에 돌아가야 한다")
	}

	other := &db.Result{Cols: []string{"id", "nick"}, Types: []string{"int4", "text"}}
	err := m.ShowDiff(other)
	if err == nil || !strings.Contains(err.Error(), "열이 달라 비교할 수 없음") || m.mode != modeTable {
		t.Fatalf("열이 다르면 오류이고 표에 남아야 한다: %v %v", err, m.mode)
	}
}

func TestCompare_비교_화면은_작은_크기와_가로_이동에서도_크기를_지킨다(t *testing.T) {
	m := newGrid(numbersResult(30, 12), [2]int{30, 6})
	if err := m.ShowDiff(numbersResult(3, 12)); err != nil {
		t.Fatal(err)
	}
	m, _ = press(m, "l", "l", "G")
	checkBounds(t, m)
	if m.diff.left != 2 || m.diff.row != 26 {
		t.Fatalf("이동: left %d row %d", m.diff.left, m.diff.row)
	}
	m.HandleMouse(tea.MouseMsg{Action: tea.MouseActionPress, Button: tea.MouseButtonWheelUp})
	if m.diff.row != 23 {
		t.Fatalf("휠: %d", m.diff.row)
	}
	m.SetSize(5, 2)
	checkBounds(t, m)
}

func TestCellView_JSON은_나무로_열고_t로_원문과_오가며_y_Y를_복사한다(t *testing.T) {
	r := &db.Result{Cols: []string{"id", "doc"}, Types: []string{"int4", "text"}, Rows: [][]string{{"1", `{"a":{"b":[1,2]}}`}, {"2", `{not json`}}}
	m := newGrid(r, [2]int{40, 12})
	m, _ = press(m, "l", "o")
	if !m.cell.treeOn {
		t.Fatal("{ 로 시작해 해석되는 값은 나무로 열어야 한다")
	}
	if out := ansi.Strip(m.View(true)); !strings.Contains(out, "▾ $: {1}") || !strings.Contains(out, "t 원문") {
		t.Fatalf("나무:\n%s", out)
	}
	checkBounds(t, m)
	m, _ = press(m, "j")
	_, a := press(m, "Y")
	if copied(t, a) != "$.a" {
		t.Fatalf("경로: %q", *a.Copy)
	}
	_, a = press(m, "y")
	if copied(t, a) != `{"b":[1,2]}` || !strings.Contains(a.Status, "값 복사") {
		t.Fatalf("값: %q %q", *a.Copy, a.Status)
	}
	if g, _ := press(m, "q"); g.mode != modeCell {
		t.Fatal("q 는 전역 키라 나무를 닫지 않는다")
	}
	m, _ = press(m, "t")
	if out := ansi.Strip(m.View(true)); m.cell.treeOn || !strings.Contains(out, `"a": {`) || !strings.Contains(out, "t 나무") {
		t.Fatalf("원문:\n%s", out)
	}
	m, _ = press(m, "t")
	if !m.cell.treeOn {
		t.Fatal("t 로 나무에 돌아가야 한다")
	}
	m, _ = press(m, "esc")
	if m.mode != modeTable {
		t.Fatal("esc 로 표에 돌아가야 한다")
	}

	m, _ = press(m, "j", "o")
	if m.mode != modeCell || m.cell.hasTree || m.cell.treeOn {
		t.Fatal("해석되지 않는 값은 원문 보기다")
	}
	if g, _ := press(m, "t"); g.cell.treeOn {
		t.Fatal("나무가 없으면 t 는 아무것도 하지 않는다")
	}
}

func TestCellView_jsonb_열은_나무로_열고_작은_크기에서도_크기를_지킨다(t *testing.T) {
	r := &db.Result{Cols: []string{"doc"}, Types: []string{"jsonb"}, Rows: [][]string{{`"scalar"`}, {"NULL"}}}
	m := newGrid(r, [2]int{6, 3})
	m, _ = press(m, "o")
	if !m.cell.treeOn {
		t.Fatal("jsonb 스칼라도 나무로 연다")
	}
	checkBounds(t, m)
	m, _ = press(m, "esc", "j", "o")
	if m.cell.hasTree {
		t.Fatal("NULL 은 원문 보기다")
	}
}

func TestPending_차트_선택_비교_JSON_보기는_보류_중인_바꿈을_지킨다(t *testing.T) {
	m := editedGrid(t, [2]int{70, 12})
	want := submit(t, m)

	m, _ = press(m, "c")
	m.HandleMouse(tea.MouseMsg{Action: tea.MouseActionPress, Button: tea.MouseButtonLeft, X: 10, Y: 5})
	m, _ = press(m, "d", "u", "a", "c")

	m, _ = press(m, "V", "j", "d", "u", "a", "ctrl+d", "enter", "f2", "s", "/", "y")
	if m.sel != true || m.editing || m.typing {
		t.Fatal("선택 중 편집·거르기 키는 무시해야 한다")
	}
	m, _ = press(m, "esc")

	m, a := press(m, "=")
	if !a.Compare {
		t.Fatal("= 는 Compare 를 청해야 한다")
	}
	if err := m.ShowDiff(editableResult()); err != nil {
		t.Fatal(err)
	}
	m, _ = press(m, "d", "u", "a", "esc")

	m, _ = press(m, "o", "t", "t", "y", "esc")

	if m.Pending() != 1 || !reflect.DeepEqual(submit(t, m), want) {
		t.Fatalf("바꿈이 달라졌다: %d %+v", m.Pending(), submit(t, m))
	}
}

func TestPending_추가_행만_고른_선택은_바꿈을_버리면_끝난다(t *testing.T) {
	m := newGrid(&db.Result{Cols: []string{"id"}, Types: []string{"int4"}, Origin: &db.Origin{Table: peopleTable, PK: []string{"id"}, Cols: []string{"id"}}}, [2]int{40, 8})
	m, _ = press(m, "a", "V")
	m.ClearChanges()
	if m.sel {
		t.Fatal("빈 표에서 선택이 남으면 안 된다")
	}
	_, _ = press(m, "j", "y")
	checkBounds(t, m)
}

func TestSelect_선택이_그대로면_집계를_다시_세지_않는다(t *testing.T) {
	m := newGrid(numbersResult(5, 3), [2]int{60, 12})
	m, _ = press(m, "V", "j")
	// 캐시를 바꿔 두면 다시 세지 않았는지 상태 글로 알 수 있다
	m.agg.text = "캐시"
	m, a := press(m, "x")
	if a.Status != "캐시" {
		t.Fatalf("움직이지 않는 키에 다시 셌다: %q", a.Status)
	}
	_, a = press(m, "l")
	if a.Status != "칸 4 · 합 202 · 평균 50.5 · 최소 0 · 최대 101" {
		t.Fatalf("사각형이 바뀌면 다시 세야 한다: %q", a.Status)
	}
}

func TestPending_표가_아닌_화면의_ctrl_s는_표로_돌아가라고_알린다(t *testing.T) {
	m := editedGrid(t, [2]int{70, 12})
	const want = "표로 돌아가 Ctrl+S 로 제출 (변경 1건)"
	for name, keys := range map[string][]string{"차트": {"c"}, "선택": {"V"}, "값": {"o"}, "레코드": {"v"}} {
		g, _ := press(m, keys...)
		g, a := press(g, "ctrl+s")
		if a.Status != want || a.Submit != nil || g.Pending() != 1 {
			t.Errorf("%s: %q submit=%v", name, a.Status, a.Submit != nil)
		}
	}
	if err := m.ShowDiff(editableResult()); err != nil {
		t.Fatal(err)
	}
	if _, a := press(m, "ctrl+s"); a.Status != want {
		t.Errorf("비교: %q", a.Status)
	}
}

func TestCellView_JSON_나무에서_휠은_나무를_움직인다(t *testing.T) {
	r := &db.Result{Cols: []string{"doc"}, Types: []string{"jsonb"}, Rows: [][]string{{`{"a":1,"b":2,"c":3,"d":4,"e":5}`}}}
	m := newGrid(r, [2]int{40, 12})
	m, _ = press(m, "o")
	m.HandleMouse(tea.MouseMsg{Action: tea.MouseActionPress, Button: tea.MouseButtonWheelDown})
	if _, a := press(m, "Y"); copied(t, a) != "$.c" {
		t.Fatalf("휠 아래 세 줄: %q", *a.Copy)
	}
	m.HandleMouse(tea.MouseMsg{Action: tea.MouseActionPress, Button: tea.MouseButtonWheelUp})
	if _, a := press(m, "Y"); copied(t, a) != "$" {
		t.Fatalf("휠 위: %q", *a.Copy)
	}
}

func TestViewing_사각형_선택_중에도_참이다(t *testing.T) {
	m := newGrid(numbersResult(3, 2), [2]int{40, 8})
	if m.Viewing() {
		t.Fatal("표는 Viewing 이 아니다")
	}
	m, _ = press(m, "V")
	if !m.Viewing() {
		t.Fatal("선택 중에는 Viewing 이다")
	}
	m, _ = press(m, "esc")
	if m.Viewing() {
		t.Fatal("선택을 끝내면 Viewing 이 아니다")
	}
}
