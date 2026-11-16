package grid

import (
	"slices"
	"strings"
	"testing"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
)

// ─── 복사 꼴 고르기 (R71) ───

func copyWith(t *testing.T, m Model, target CopyTarget, f CopyFormat) string {
	t.Helper()
	return copied(t, m.CopyWith(CopyChoice{Target: target, Format: f}))
}

func TestCopyMenu_CtrlK는_표에서_창을_청하고_행이_없으면_청하지_않는다(t *testing.T) {
	m := newGrid(peopleResult(), [2]int{80, 10})
	if _, a := press(m, "ctrl+k"); !a.CopyMenu || a.Copy != nil {
		t.Fatalf("Ctrl+K: %+v", a)
	}
	m, _ = press(m, "V", "j")
	if _, a := press(m, "ctrl+k"); !a.CopyMenu {
		t.Fatalf("선택 중 Ctrl+K: %+v", a)
	}
	empty := newGrid(&db.Result{Cols: []string{"id"}}, [2]int{80, 10})
	if _, a := press(empty, "ctrl+k"); a.CopyMenu {
		t.Fatal("행 없는 결과에서 창을 청했다")
	}
}

func TestCopyMenu_항목은_선택_중에만_선택_원본_테이블이면_INSERT_열이면_IN이다(t *testing.T) {
	m := newGrid(peopleResult(), [2]int{80, 10})
	choices := m.CopyChoices()
	if len(choices) != 4+4+5 {
		t.Fatalf("항목 수 %d: %v", len(choices), choices)
	}
	for _, c := range choices {
		if c.Target == TargetSelection || c.Format == FormatInsert || (c.Format == FormatIN && c.Target != TargetColumn) {
			t.Fatalf("있으면 안 되는 항목: %v", c)
		}
	}
	if choices[0].String() != "현재 행 · TSV (헤더)" {
		t.Fatalf("이름: %q", choices[0])
	}
	m = newGrid(editableResult(), [2]int{80, 10})
	m, _ = press(m, "V")
	choices = m.CopyChoices()
	if !slices.Contains(choices, CopyChoice{TargetSelection, FormatInsert}) || slices.Contains(choices, CopyChoice{TargetColumn, FormatInsert}) {
		t.Fatalf("선택·INSERT: %v", choices)
	}
}

func TestCopyMenu_TSV_헤더는_첫_줄에_열_이름이고_Y와_V_y는_그대로다(t *testing.T) {
	m := newGrid(peopleResult(), [2]int{80, 10})
	if got := copyWith(t, m, TargetRow, FormatTSV); got != "id\tname\tcity\n1\tAlice\tseoul" {
		t.Fatalf("행 TSV: %q", got)
	}
	if got := copyWith(t, m, TargetVisible, FormatTSV); got != "id\tname\tcity\n1\tAlice\tseoul\n2\tbob\tBusan\n3\tcarol\tNULL" {
		t.Fatalf("보이는 행 TSV: %q", got)
	}
	if _, a := press(m, "Y"); copied(t, a) != "1\tAlice\tseoul" {
		t.Fatalf("Y 가 바뀌었다: %q", *a.Copy)
	}
	m, _ = press(m, "l", "V", "j", "l")
	if got := copyWith(t, m, TargetSelection, FormatTSV); got != "name\tcity\nAlice\tseoul\nbob\tBusan" {
		t.Fatalf("선택 TSV: %q", got)
	}
	if _, a := press(m, "y"); copied(t, a) != "Alice\tseoul\nbob\tBusan" {
		t.Fatalf("V→y 가 바뀌었다: %q", *a.Copy)
	}
}

func TestCopyMenu_CSV는_헤더를_붙이고_수식_주입을_막는다(t *testing.T) {
	r := &db.Result{Cols: []string{"id", "f"}, Types: []string{"int4", "text"}, Rows: [][]string{{"-1", "=1+1"}, {"2", "a,b"}}}
	m := newGrid(r, [2]int{80, 10})
	if got := copyWith(t, m, TargetVisible, FormatCSV); got != "id,f\n-1,'=1+1\n2,\"a,b\"\n" {
		t.Fatalf("CSV: %q", got)
	}
}

func TestCopyMenu_JSON은_내보내기처럼_숫자_bool_null을_JSON_값으로_쓴다(t *testing.T) {
	r := &db.Result{Cols: []string{"id", "ok", "name"}, Types: []string{"int4", "bool", "text"},
		Rows: [][]string{{"1", "t", "007"}, {"2", "f", "NULL"}}}
	m := newGrid(r, [2]int{80, 10})
	want := "[\n  {\"id\": 1, \"ok\": true, \"name\": \"007\"},\n  {\"id\": 2, \"ok\": false, \"name\": null}\n]\n"
	if got := copyWith(t, m, TargetVisible, FormatJSON); got != want {
		t.Fatalf("JSON: %q", got)
	}
	if got := copyWith(t, m, TargetColumn, FormatJSON); got != "[\n  {\"id\": 1},\n  {\"id\": 2}\n]\n" {
		t.Fatalf("열 JSON: %q", got)
	}
}

func TestCopyMenu_Markdown은_파이프와_줄바꿈을_이스케이프하고_숫자_열은_오른쪽_정렬이다(t *testing.T) {
	r := &db.Result{Cols: []string{"id", "note"}, Types: []string{"int4", "text"}, Rows: [][]string{{"1", "a|b\nc\r\nd"}}}
	m := newGrid(r, [2]int{80, 10})
	want := "| id | note |\n| ---: | --- |\n| 1 | a\\|b<br>c<br>d |\n"
	if got := copyWith(t, m, TargetRow, FormatMarkdown); got != want {
		t.Fatalf("Markdown: %q", got)
	}
}

func TestCopyMenu_열_대상은_보이는_행의_커서_열_값을_줄마다_쓴다(t *testing.T) {
	m := newGrid(peopleResult(), [2]int{80, 10})
	m, _ = press(m, "/", "n", "a", "m", "e", ":", "o", "enter", "l")
	if got := copyWith(t, m, TargetColumn, FormatTSV); got != "name\nbob\ncarol" {
		t.Fatalf("열 TSV(거른 행만): %q", got)
	}
}

func TestCopyMenu_IN_목록은_C와_같다(t *testing.T) {
	m := newGrid(peopleResult(), [2]int{80, 10})
	_, c := press(m, "C")
	a := m.CopyWith(CopyChoice{TargetColumn, FormatIN})
	if copied(t, a) != *c.Copy || copied(t, a) != "(1, 2, 3)" || a.Status != "복사함 (1줄) · 현재 열 · IN 목록 · "+c.Status {
		t.Fatalf("IN: %q %q / C: %q %q", *a.Copy, a.Status, *c.Copy, c.Status)
	}
}

func TestCopyMenu_INSERT는_i와_같은_문이고_보이는_행_전체를_한_문으로_넣는다(t *testing.T) {
	m := newGrid(editableResult(), [2]int{80, 10})
	if got := copyWith(t, m, TargetRow, FormatInsert); got != db.InsertSQL(m.res, []int{0}) {
		t.Fatalf("행 INSERT: %q", got)
	}
	got := copyWith(t, m, TargetVisible, FormatInsert)
	if got != db.InsertSQL(m.res, []int{0, 1, 2}) || !strings.Contains(got, `"people"`) {
		t.Fatalf("전체 INSERT: %q", got)
	}
	if a := m.CopyWith(CopyChoice{TargetVisible, FormatInsert}); !strings.HasPrefix(a.Status, "복사함 (") {
		t.Fatalf("상태: %q", a.Status)
	}
}

func TestCopyMenu_상태_줄은_복사한_줄_수와_고른_항목이다(t *testing.T) {
	m := newGrid(peopleResult(), [2]int{80, 10})
	if a := m.CopyWith(CopyChoice{TargetVisible, FormatCSV}); a.Status != "복사함 (4줄) · 보이는 행 전체 · CSV (헤더)" {
		t.Fatalf("상태: %q", a.Status)
	}
}

func TestCopyMenu_오른쪽_클릭은_그_칸으로_옮기고_본문_칸인지_알려_준다(t *testing.T) {
	m := newGrid(numbersResult(50, 10), [2]int{60, 12})
	l := m.layout()
	target := l.cols[2]
	x, y := target.x+2, l.headLines+3
	if !m.CellAt(x, y) || m.CellAt(x, 0) {
		t.Fatal("본문 칸만 참이어야 한다")
	}
	m.HandleMouse(tea.MouseMsg{X: x, Y: y, Button: tea.MouseButtonRight, Action: tea.MouseActionPress})
	if m.row != 3 || m.col != target.col {
		t.Fatalf("오른쪽 클릭: row=%d col=%d", m.row, m.col)
	}
}

func TestCopyMenu_10MiB_를_넘을_결과는_만들기_전에_멈춘다(t *testing.T) {
	big := strings.Repeat("x", 1<<20)
	r := &db.Result{Cols: []string{"v"}}
	for range 11 {
		r.Rows = append(r.Rows, []string{big})
	}
	m := newGrid(r, [2]int{80, 10})
	a := m.CopyWith(CopyChoice{TargetVisible, FormatJSON})
	if a.Copy != nil || !strings.HasPrefix(a.Status, "너무 커서 복사하지 않음") {
		t.Fatalf("큰 결과: copy=%v %q", a.Copy != nil, a.Status)
	}
	if a := m.CopyWith(CopyChoice{TargetRow, FormatTSV}); a.Copy == nil {
		t.Fatal("한 행은 그대로 복사한다")
	}
}
