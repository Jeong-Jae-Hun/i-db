package grid

import (
	"encoding/json"
	"fmt"
	"strconv"
	"strings"
	"testing"
	"time"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/x/ansi"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
)

var specialKeys = map[string]tea.KeyType{
	"down": tea.KeyDown, "up": tea.KeyUp, "left": tea.KeyLeft, "right": tea.KeyRight,
	"enter": tea.KeyEnter, "esc": tea.KeyEsc, "backspace": tea.KeyBackspace,
	"pgdown": tea.KeyPgDown, "pgup": tea.KeyPgUp, "ctrl+d": tea.KeyCtrlD, "ctrl+u": tea.KeyCtrlU,
	"home": tea.KeyHome, "end": tea.KeyEnd, " ": tea.KeySpace, "tab": tea.KeyTab, "ctrl+c": tea.KeyCtrlC,
	"f2": tea.KeyF2, "ctrl+n": tea.KeyCtrlN, "ctrl+s": tea.KeyCtrlS, "delete": tea.KeyDelete,
	"ctrl+a": tea.KeyCtrlA, "ctrl+e": tea.KeyCtrlE, "ctrl+o": tea.KeyCtrlO, "ctrl+k": tea.KeyCtrlK,
}

// press 는 키를 차례로 누르고 마지막 Action 을 돌려준다.
func press(m Model, keys ...string) (Model, Action) {
	var a Action
	for _, k := range keys {
		msg := tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune(k)}
		if t, ok := specialKeys[k]; ok {
			msg = tea.KeyMsg{Type: t}
		}
		m, a = m.Update(msg)
	}
	return m, a
}

func newGrid(r *db.Result, size [2]int) Model {
	m := New()
	m.SetSize(size[0], size[1])
	m.SetResult(r)
	return m
}

// viewed 는 현재 view 순서의 한 열 값이다.
func viewed(m Model, col int) []string {
	out := make([]string, 0, len(m.view))
	for _, i := range m.view {
		out = append(out, m.res.Rows[i][col])
	}
	return out
}

func copied(t *testing.T, a Action) string {
	t.Helper()
	if a.Copy == nil {
		t.Fatalf("복사 요청이 없다: %+v", a)
	}
	return *a.Copy
}

func numbersResult(n, cols int) *db.Result {
	r := &db.Result{}
	for c := 0; c < cols; c++ {
		r.Cols = append(r.Cols, fmt.Sprintf("column_%02d", c))
		r.Types = append(r.Types, "int4")
	}
	for i := 0; i < n; i++ {
		row := make([]string, cols)
		for c := range row {
			row[c] = strconv.Itoa(i*100 + c)
		}
		r.Rows = append(r.Rows, row)
	}
	return r
}

func peopleResult() *db.Result {
	return &db.Result{
		Cols:  []string{"id", "name", "city"},
		Types: []string{"int4", "text", "text"},
		Rows:  [][]string{{"1", "Alice", "seoul"}, {"2", "bob", "Busan"}, {"3", "carol", "NULL"}},
	}
}

// checkBounds 는 View 가 주어진 크기를 넘지 않는지 본다.
func checkBounds(t *testing.T, m Model) {
	t.Helper()
	for _, focused := range []bool{true, false} {
		out := m.View(focused)
		if out == "" {
			continue
		}
		lines := strings.Split(out, "\n")
		if len(lines) > m.h {
			t.Fatalf("%dx%d 에 %d줄", m.w, m.h, len(lines))
		}
		for _, l := range lines {
			if w := ansi.StringWidth(l); w > m.w {
				t.Fatalf("%dx%d 에 폭 %d 줄: %q", m.w, m.h, w, ansi.Strip(l))
			}
		}
	}
}

func TestColumnKinds_타입이_있으면_타입으로_없으면_값으로_고른다(t *testing.T) {
	typed := columnKinds(&db.Result{Cols: []string{"a", "b", "c", "d"}, Types: []string{"numeric", "bool", "text", "1234"}})
	if typed[0] != kindNumber || typed[1] != kindBool || typed[2] != kindText || typed[3] != kindText {
		t.Fatalf("typed: %v", typed)
	}
	r := &db.Result{Cols: []string{"n", "s", "allnull"}, Rows: [][]string{{"1.5", "x", "NULL"}, {"NULL", "2", "NULL"}, {"-3e2", "3", "NULL"}}}
	inferred := columnKinds(r)
	if inferred[0] != kindNumber || inferred[1] != kindText || inferred[2] != kindText {
		t.Fatalf("inferred: %v", inferred)
	}
}

func TestSort_숫자_열은_크기순이고_NULL은_오름차순_내림차순_모두_맨_뒤다(t *testing.T) {
	r := &db.Result{Cols: []string{"n"}, Types: []string{"int4"}, Rows: [][]string{{"10"}, {"2"}, {"NULL"}, {"1"}, {"-3"}}}
	m := newGrid(r, [2]int{40, 10})
	m, _ = press(m, "s")
	if got := strings.Join(viewed(m, 0), ","); got != "-3,1,2,10,NULL" {
		t.Fatalf("asc: %s", got)
	}
	m, _ = press(m, "s")
	if got := strings.Join(viewed(m, 0), ","); got != "10,2,1,-3,NULL" {
		t.Fatalf("desc: %s", got)
	}
	m, _ = press(m, "s")
	if got := strings.Join(viewed(m, 0), ","); got != "10,2,NULL,1,-3" {
		t.Fatalf("none: %s", got)
	}
}

func TestSort_타입_없는_숫자_열도_숫자로_정렬하고_같은_값은_원래_순서를_지킨다(t *testing.T) {
	r := &db.Result{Cols: []string{"score", "member"}, Rows: [][]string{{"10", "a"}, {"9", "b"}, {"1.5", "c"}, {"9", "d"}}}
	m := newGrid(r, [2]int{40, 10})
	m, _ = press(m, "s")
	if got := strings.Join(viewed(m, 1), ","); got != "c,b,d,a" {
		t.Fatalf("got %s", got)
	}
	m, _ = press(m, "l", "s", "s")
	if got := strings.Join(viewed(m, 1), ","); got != "d,c,b,a" {
		t.Fatalf("다른 열 내림차순: %s", got)
	}
}

func TestSort_정렬해도_커서는_같은_행을_가리키고_헤더에_표시가_붙는다(t *testing.T) {
	m := newGrid(peopleResult(), [2]int{60, 10})
	m, _ = press(m, "j", "l") // bob
	m, _ = press(m, "s")
	m, a := press(m, "y")
	if copied(t, a) != "bob" {
		t.Fatalf("커서 행이 바뀌었다: %q", *a.Copy)
	}
	if out := ansi.Strip(m.View(true)); !strings.Contains(out, "name ▲") || !strings.Contains(out, "▲ name") {
		t.Fatalf("정렬 표시 없음:\n%s", out)
	}
}

func TestFilter_대소문자_무시_전체_열과_열이름_지정을_거른다(t *testing.T) {
	m := newGrid(peopleResult(), [2]int{60, 10})
	cases := []struct{ q, want string }{
		{"ALI", "1"},
		{"b", "2"},          // bob·Busan 같은 행
		{"city:bus", "2"},   // 열 지정
		{"name:seoul", ""},  // 다른 열 값은 안 찾는다
		{"CITY: Null", "3"}, // 열 이름 대소문자·값 앞 공백 무시
		{"nocol:x", ""},     // 없는 열 이름이면 전체 문자열로 찾는다
		{"", "1,2,3"},
	}
	for _, c := range cases {
		g, _ := press(m, "/", c.q, "enter")
		if got := strings.Join(viewed(g, 0), ","); got != c.want {
			t.Fatalf("%q: got %q want %q", c.q, got, c.want)
		}
	}
	r := peopleResult()
	r.Rows = append(r.Rows, []string{"4", "dave", "http://x"})
	g, _ := press(newGrid(r, [2]int{60, 10}), "/", "http://", "enter")
	if got := strings.Join(viewed(g, 0), ","); got != "4" {
		t.Fatalf("콜론이 든 값: %q", got)
	}
}

func TestFilter_입력_중에는_글자키를_받고_esc는_해제_위치줄에_개수를_보인다(t *testing.T) {
	m := newGrid(peopleResult(), [2]int{60, 10})
	m, _ = press(m, "/")
	if !m.Typing() {
		t.Fatal("입력 중이어야 한다")
	}
	m, _ = press(m, "q", "x", "backspace", "backspace", "c", "a", "r", "o")
	if m.filter != "caro" || len(m.view) != 1 {
		t.Fatalf("filter=%q view=%v", m.filter, m.view)
	}
	if out := ansi.Strip(m.View(true)); !strings.Contains(out, "filtered 1/3") || !strings.Contains(out, "/caro▏") {
		t.Fatalf("위치 줄:\n%s", out)
	}
	m, _ = press(m, "enter")
	if m.Typing() || len(m.view) != 1 {
		t.Fatal("enter 는 입력만 끝내고 거르기는 남긴다")
	}
	m, _ = press(m, "esc")
	if m.filter != "" || len(m.view) != 3 {
		t.Fatalf("esc 해제 실패: %q %v", m.filter, m.view)
	}
	m, _ = press(m, "/", "zzz", "esc")
	if m.Typing() || m.filter != "" || len(m.view) != 3 {
		t.Fatal("입력 중 esc 는 거르기를 지우고 입력을 끝낸다")
	}
	m, _ = press(m, "/", "없는값", "enter")
	if out := ansi.Strip(m.View(true)); !strings.Contains(out, "일치하는 행 없음") {
		t.Fatalf("빈 결과 안내 없음:\n%s", out)
	}
}

func TestScroll_커서를_따라_세로로_밀고_위치를_알린다(t *testing.T) {
	m := newGrid(numbersResult(100, 3), [2]int{60, 10})
	body := m.layout().body
	if body != 6 { // 10 - 이름·타입·구분선 3 - 위치 줄 1
		t.Fatalf("body=%d", body)
	}
	keys := make([]string, 10)
	for i := range keys {
		keys[i] = "j"
	}
	m, _ = press(m, keys...)
	if m.row != 10 || m.rowTop != 5 {
		t.Fatalf("row=%d top=%d", m.row, m.rowTop)
	}
	if out := ansi.Strip(m.View(true)); !strings.Contains(out, "rows 6–11 of 100") {
		t.Fatalf("위치 줄:\n%s", out)
	}
	m, _ = press(m, "G")
	if m.row != 99 || m.rowTop != 94 {
		t.Fatalf("G: row=%d top=%d", m.row, m.rowTop)
	}
	m, _ = press(m, "pgup")
	if m.row != 93 {
		t.Fatalf("pgup: %d", m.row)
	}
	m, _ = press(m, "g")
	if m.row != 0 || m.rowTop != 0 {
		t.Fatalf("g: row=%d top=%d", m.row, m.rowTop)
	}
	m, _ = press(m, "ctrl+d")
	if m.row != 3 {
		t.Fatalf("ctrl+d 는 반 쪽: %d", m.row)
	}
	m.SetSize(60, 30)
	if m.rowTop != 0 {
		t.Fatalf("커지면 빈 줄 없이 위로 당긴다: %d", m.rowTop)
	}
}

func TestScroll_가로로_커서_열이_보이도록_민다(t *testing.T) {
	m := newGrid(numbersResult(5, 20), [2]int{50, 10})
	m, _ = press(m, "$")
	if m.col != 19 {
		t.Fatalf("col=%d", m.col)
	}
	l := m.layout()
	last := l.cols[len(l.cols)-1]
	if last.col != 19 || last.w != m.widths[19] {
		t.Fatalf("마지막 열이 다 보여야 한다: %+v", l.cols)
	}
	out := ansi.Strip(m.View(true))
	if !strings.Contains(out, "column_19") || !strings.Contains(out, "of 20") {
		t.Fatalf("화면:\n%s", out)
	}
	m, _ = press(m, "0")
	if m.colTop != 0 {
		t.Fatalf("0: colTop=%d", m.colTop)
	}
	if l := m.layout(); l.cols[len(l.cols)-1].w >= m.widths[l.cols[len(l.cols)-1].col] {
		t.Fatalf("남는 자리에 잘린 열이 보여야 한다: %+v", l.cols)
	}
}

func TestResize_열_폭을_상하한_안에서_바꾼다(t *testing.T) {
	m := newGrid(peopleResult(), [2]int{60, 10})
	start := m.widths[0]
	m, a := press(m, ">")
	if m.widths[0] != start+resizeStep || a.Status == "" {
		t.Fatalf("> : %d %q", m.widths[0], a.Status)
	}
	for i := 0; i < 200; i++ {
		m, _ = press(m, ">")
	}
	if m.widths[0] != maxColWidth {
		t.Fatalf("상한: %d", m.widths[0])
	}
	for i := 0; i < 200; i++ {
		m, _ = press(m, "<")
	}
	if m.widths[0] != minColWidth {
		t.Fatalf("하한: %d", m.widths[0])
	}
	checkBounds(t, m)
}

func TestToCSV_따옴표_쉼표_줄바꿈과_NULL_빈문자열을_가른다(t *testing.T) {
	r := &db.Result{
		Cols: []string{"a", "b,c", "d"},
		Rows: [][]string{{`x"y`, "", "NULL"}, {"line\nbreak", " sp", "1"}},
	}
	want := "a,\"b,c\",d\n\"x\"\"y\",\"\",\n\"line\nbreak\",\" sp\",1\n"
	if got := ToCSV(r); got != want {
		t.Fatalf("got %q\nwant %q", got, want)
	}
}

func TestToJSON_열_순서와_같은_이름을_지키고_타입별_값을_쓴다(t *testing.T) {
	r := &db.Result{
		Cols:  []string{"id", "name", "ok", "doc", "id", "f"},
		Types: []string{"int4", "text", "bool", "jsonb", "int8", "float8"},
		Rows: [][]string{
			{"1", `a"b<`, "t", `{"k": [1, 2]}`, "NULL", "NaN"},
			{"2", "NULL", "f", "not json", "9", "1.5"},
		},
	}
	got := ToJSON(r, columnKinds(r))
	want := `[
  {"id": 1, "name": "a\"b<", "ok": true, "doc": {"k":[1,2]}, "id": null, "f": "NaN"},
  {"id": 2, "name": null, "ok": false, "doc": "not json", "id": 9, "f": 1.5}
]
`
	if got != want {
		t.Fatalf("got\n%s\nwant\n%s", got, want)
	}
	if !json.Valid([]byte(got)) {
		t.Fatal("JSON 이 아니다")
	}
	if got := ToJSON(&db.Result{Cols: []string{"a"}}, []kind{kindText}); got != "[]\n" || !json.Valid([]byte(got)) {
		t.Fatalf("빈 결과: %q", got)
	}
}

func TestRowTSV_탭과_줄바꿈을_이스케이프한다(t *testing.T) {
	if got := RowTSV([]string{"a\tb", "c\nd\r", `e\f`, "NULL"}); got != `a\tb`+"\t"+`c\nd\r`+"\t"+`e\\f`+"\tNULL" {
		t.Fatalf("got %q", got)
	}
}

func TestCopy_셀과_행을_복사하고_빈_값도_복사한다(t *testing.T) {
	r := &db.Result{Cols: []string{"a", "b"}, Rows: [][]string{{"", "x\ty"}}}
	m := newGrid(r, [2]int{40, 10})
	_, a := press(m, "y")
	if copied(t, a) != "" {
		t.Fatalf("빈 셀: %q", *a.Copy)
	}
	_, a = press(m, "Y")
	if copied(t, a) != "\t"+`x\ty` {
		t.Fatalf("행: %q", *a.Copy)
	}
	_, a = press(newGrid(&db.Result{Cols: []string{"a"}}, [2]int{40, 10}), "y")
	if a.Copy != nil {
		t.Fatal("행이 없으면 복사하지 않는다")
	}
}

func TestExport_거른_행을_CSV_JSON으로_내보낸다(t *testing.T) {
	m, _ := press(newGrid(peopleResult(), [2]int{60, 10}), "/", "bob", "enter")
	_, a := press(m, "e")
	if a.Export == nil || a.Export.Format != "csv" || a.Export.Rows != 1 || a.Export.Content != "id,name,city\n2,bob,Busan\n" {
		t.Fatalf("csv: %+v", a.Export)
	}
	_, a = press(m, "E")
	if a.Export == nil || a.Export.Format != "json" || !strings.Contains(a.Export.Content, `"id": 2`) {
		t.Fatalf("json: %+v", a.Export)
	}
}

func TestWidth_한글은_두_칸으로_재고_줄_폭이_맞는다(t *testing.T) {
	r := &db.Result{Cols: []string{"이름", "n"}, Types: []string{"text", "int4"}, Rows: [][]string{{"홍길동", "1"}, {"a", "22"}}}
	if ws := autoWidths(r, columnKinds(r)); ws[0] != 6 || ws[1] != 4 { // "홍길동" 6칸, "이름"+정렬 표시 6칸, "int4" 4칸
		t.Fatalf("widths=%v", ws)
	}
	m := newGrid(r, [2]int{40, 10})
	lines := strings.Split(ansi.Strip(m.View(true)), "\n")
	want := ansi.StringWidth(lines[0])
	for _, l := range lines[1:5] { // 타입·구분선·두 행
		if ansi.StringWidth(l) != want {
			t.Fatalf("줄 폭이 다르다 %d vs %d: %q", ansi.StringWidth(l), want, l)
		}
	}
	if !strings.Contains(lines[3], "홍길동") || !strings.Contains(lines[4], "│   22 ") || !strings.Contains(lines[3], "│    1 ") {
		t.Fatalf("숫자는 오른쪽 정렬:\n%s", strings.Join(lines, "\n"))
	}
}

func TestView_NULL_bool_캡션과_제어문자를_보이는_글자로_그린다(t *testing.T) {
	r := &db.Result{
		Text:  "key · hash · TTL -1",
		Cols:  []string{"v\x1b]0;x\x07", "ok"},
		Types: []string{"text", "bool"},
		Rows:  [][]string{{"a\x1b[31mred", "t"}, {"NULL", "f"}},
	}
	m := newGrid(r, [2]int{80, 10})
	raw := m.View(true)
	out := ansi.Strip(raw)
	for _, want := range []string{"key · hash · TTL -1", `a\x1b[31mred`, `v\x1b]0;x\x07`, "NULL", "true", "false", "text", "bool"} {
		if !strings.Contains(out, want) {
			t.Fatalf("%q 없음:\n%s", want, out)
		}
	}
	if strings.Contains(raw, "\x1b[31mred") || strings.Contains(raw, "\x1b]0;") {
		t.Fatal("DB 값의 제어 문자가 그대로 나갔다")
	}
	g, _ := press(m, "/", "true", "enter")
	if len(g.view) != 1 {
		t.Fatalf("bool 은 보이는 글자로 거른다: %v", g.view)
	}
}

func TestView_64KiB_칸은_앞부분만_재서_빨리_그린다(t *testing.T) {
	big := strings.Repeat("가", db.MaxCellBytes/3)
	r := &db.Result{Cols: []string{"big"}}
	for i := 0; i < db.MaxRows; i++ {
		r.Rows = append(r.Rows, []string{big})
	}
	start := time.Now()
	m := newGrid(r, [2]int{100, 40})
	for i := 0; i < 20; i++ {
		m, _ = press(m, "j")
		_ = m.View(true)
	}
	if m.widths[0] != autoMaxWidth {
		t.Fatalf("폭 상한: %d", m.widths[0])
	}
	// 통째로 재면 64MiB 를 매번 훑어 수 초가 걸린다
	if d := time.Since(start); d > 2*time.Second {
		t.Fatalf("너무 느리다: %v", d)
	}
	checkBounds(t, m)
}

func TestMouse_클릭은_칸을_고르고_휠은_세로_shift휠은_가로로_민다(t *testing.T) {
	m := newGrid(numbersResult(50, 10), [2]int{60, 12})
	l := m.layout()
	target := l.cols[2]
	m.HandleMouse(tea.MouseMsg{X: target.x + 2, Y: l.headLines + 3, Button: tea.MouseButtonLeft, Action: tea.MouseActionPress})
	if m.row != 3 || m.col != target.col {
		t.Fatalf("클릭: row=%d col=%d", m.row, m.col)
	}
	m.HandleMouse(tea.MouseMsg{X: l.cols[1].x + 1, Y: 0, Button: tea.MouseButtonLeft, Action: tea.MouseActionPress})
	if m.row != 3 || m.col != l.cols[1].col {
		t.Fatalf("헤더 클릭은 열만 고른다: row=%d col=%d", m.row, m.col)
	}
	m.HandleMouse(tea.MouseMsg{X: 1, Y: 11, Button: tea.MouseButtonLeft, Action: tea.MouseActionPress})
	if m.row != 3 {
		t.Fatal("위치 줄 클릭은 무시한다")
	}
	m.HandleMouse(tea.MouseMsg{Button: tea.MouseButtonLeft, Action: tea.MouseActionRelease, Y: l.headLines})
	if m.row != 3 {
		t.Fatal("놓기는 무시한다")
	}

	for i := 0; i < 3; i++ {
		m.HandleMouse(tea.MouseMsg{Button: tea.MouseButtonWheelDown, Action: tea.MouseActionPress})
	}
	if m.rowTop != 9 || m.row < 9 || m.row >= 9+m.layout().body {
		t.Fatalf("휠: top=%d row=%d", m.rowTop, m.row)
	}
	m.HandleMouse(tea.MouseMsg{Button: tea.MouseButtonWheelDown, Shift: true, Action: tea.MouseActionPress})
	if m.colTop != 1 || m.col < 1 {
		t.Fatalf("shift+휠: colTop=%d col=%d", m.colTop, m.col)
	}
	m.HandleMouse(tea.MouseMsg{Button: tea.MouseButtonWheelLeft, Action: tea.MouseActionPress})
	if m.colTop != 0 {
		t.Fatalf("가로 휠: colTop=%d", m.colTop)
	}
	checkBounds(t, m)
}

func TestCellView_enter는_JSON을_펼치고_esc는_돌아간다(t *testing.T) {
	r := &db.Result{Cols: []string{"id", "doc"}, Types: []string{"int4", "jsonb"}, Rows: [][]string{{"1", `{"k":[1,2],"s":"x"}`}}}
	m := newGrid(r, [2]int{40, 12})
	m, _ = press(m, "l", "enter", "t") // JSON 은 나무로 먼저 열려 t 로 원문을 본다(R32)
	out := ansi.Strip(m.View(true))
	if !strings.Contains(out, `  "k": [`) || !strings.Contains(out, "doc · jsonb") {
		t.Fatalf("전체 값:\n%s", out)
	}
	_, a := press(m, "y")
	if copied(t, a) != `{"k":[1,2],"s":"x"}` {
		t.Fatalf("원문 복사: %q", *a.Copy)
	}
	m, _ = press(m, "esc")
	if m.mode != modeTable {
		t.Fatal("esc 로 표에 돌아가야 한다")
	}
	checkBounds(t, m)
}

func TestDetail_v는_세로_레코드를_보이고_enter_esc는_레코드로_돌아온다(t *testing.T) {
	m := newGrid(peopleResult(), [2]int{60, 10})
	m, _ = press(m, "j", "v")
	out := ansi.Strip(m.View(true))
	if !strings.Contains(out, "레코드 2 / 3") || !strings.Contains(out, "bob") || !strings.Contains(out, "Busan") {
		t.Fatalf("레코드:\n%s", out)
	}
	m, a := press(m, "j", "j", "y")
	if copied(t, a) != "Busan" {
		t.Fatalf("필드 복사: %q", *a.Copy)
	}
	m, _ = press(m, "enter")
	if m.mode != modeCell {
		t.Fatal("enter 는 전체 값")
	}
	m, _ = press(m, "esc")
	if m.mode != modeDetail {
		t.Fatal("전체 값을 닫으면 레코드로 돌아온다")
	}
	m, _ = press(m, "esc")
	if m.mode != modeTable {
		t.Fatal("esc 로 표")
	}
}

func TestText_스크롤과_줄바꿈_토글(t *testing.T) {
	lines := make([]string, 30)
	for i := range lines {
		lines[i] = fmt.Sprintf("line %02d", i)
	}
	lines[0] = strings.Repeat("abcdefghij ", 5)
	m := newGrid(&db.Result{Text: strings.Join(lines, "\n") + "\x1b[2J"}, [2]int{20, 5})
	if len(m.textLines) <= 30 {
		t.Fatalf("긴 줄은 접혀야 한다: %d", len(m.textLines))
	}
	m, _ = press(m, "G")
	out := ansi.Strip(m.View(true))
	if !strings.Contains(out, `line 29\x1b[2J`) {
		t.Fatalf("끝:\n%s", out)
	}
	m, a := press(m, "g", "w")
	if m.wrap || len(m.textLines) != 30 || a.Status == "" {
		t.Fatalf("줄바꿈 끔: wrap=%v lines=%d", m.wrap, len(m.textLines))
	}
	m, _ = press(m, "l")
	if m.textLeft != textStep || !strings.HasPrefix(ansi.Strip(m.View(true)), "ij abcdef") {
		t.Fatalf("가로 이동: %d\n%s", m.textLeft, ansi.Strip(m.View(true)))
	}
	for i := 0; i < 20; i++ {
		m, _ = press(m, "l")
	}
	if m.textLeft != ansi.StringWidth(lines[0])-20 {
		t.Fatalf("가로 끝: %d", m.textLeft)
	}
	checkBounds(t, m)
}

func TestKeys_전역_키는_처리하지_않는다(t *testing.T) {
	m := newGrid(peopleResult(), [2]int{60, 10})
	for _, k := range []string{"tab", "ctrl+c", "q"} {
		g, a := press(m, k)
		if a.Status != "" || a.Copy != nil || a.Export != nil || g.row != m.row || g.col != m.col || g.mode != m.mode {
			t.Fatalf("%s 를 처리했다", k)
		}
	}
}

func TestView_아주_작은_크기와_빈_결과에서도_패닉없이_크기를_지킨다(t *testing.T) {
	results := []*db.Result{
		nil,
		{},
		{Cols: []string{"a"}},
		{Text: "UPDATE 3"},
		{Text: "caption", Cols: []string{"한글열"}, Rows: [][]string{{"가나다라마바사"}, {"x"}}},
		numbersResult(3, 4),
	}
	for _, r := range results {
		for w := 0; w <= 12; w++ {
			for h := 0; h <= 8; h++ {
				m := newGrid(r, [2]int{w, h})
				checkBounds(t, m)
				for _, k := range []string{"j", "l", "$", "G", "s", "v", "enter", "esc", "esc", ">", "e"} {
					m, _ = press(m, k)
					checkBounds(t, m)
				}
				m.HandleMouse(tea.MouseMsg{X: w / 2, Y: h / 2, Button: tea.MouseButtonLeft, Action: tea.MouseActionPress})
				m.HandleMouse(tea.MouseMsg{Button: tea.MouseButtonWheelDown, Action: tea.MouseActionPress})
				checkBounds(t, m)
			}
		}
	}
}

func FuzzView_어떤_값과_키에도_패닉없이_크기를_지킨다(f *testing.F) {
	f.Add("a|b\nNULL|가나\n1|\x1b[31m", "jls/a\nGv", uint8(30), uint8(8))
	f.Add("", "", uint8(0), uint8(0))
	f.Add("x", "$<<<<>e", uint8(5), uint8(3))
	f.Add("a|b\n{\"x\":[1]}|2\n3|4", "cc\x1bVjly\x1b=lotj y\x1b\x00", uint8(30), uint8(8))
	f.Add("a|b\nNULL|x\n1|1", "CFRD+-VjlC\x1bjC", uint8(30), uint8(8))
	f.Add("x", "CFRD+-", uint8(20), uint8(5))
	f.Fuzz(func(t *testing.T, data, keys string, w, h uint8) {
		var r *db.Result
		if head, rest, ok := strings.Cut(data, "\n"); ok {
			r = &db.Result{Cols: strings.Split(head, "|")}
			for _, line := range strings.Split(rest, "\n") {
				r.Rows = append(r.Rows, strings.Split(line, "|"))
			}
		} else {
			r = &db.Result{Text: data}
		}
		m := newGrid(r, [2]int{int(w % 120), int(h % 40)})
		for _, k := range keys {
			switch k {
			case '\n':
				m, _ = press(m, "enter")
			case '\x1b':
				m, _ = press(m, "esc")
			case '\x7f':
				m, _ = press(m, "backspace")
			case '\x00':
				m.HandleMouse(tea.MouseMsg{X: int(w) / 3, Y: int(h) / 2, Button: tea.MouseButtonLeft, Action: tea.MouseActionPress})
			default:
				m, _ = press(m, string(k))
			}
		}
		checkBounds(t, m)
	})
}

func TestFilter_거르기가_없으면_빈_행도_보인다(t *testing.T) {
	m := newGrid(&db.Result{Cols: []string{"a"}, Rows: [][]string{{}, {"x"}}}, [2]int{40, 10})
	if len(m.view) != 2 {
		t.Fatalf("view=%v", m.view)
	}
	if out := ansi.Strip(m.View(true)); !strings.Contains(out, "rows 1–2 of 2") {
		t.Fatalf("위치 줄:\n%s", out)
	}
	m, _ = press(m, "/", "x", "backspace", "enter")
	if len(m.view) != 2 {
		t.Fatalf("거르기를 지운 뒤 view=%v", m.view)
	}
}

func TestFilter_키마다_칸을_소문자로_복사하지_않는다(t *testing.T) {
	big := strings.Repeat("X", db.MaxCellBytes)
	r := &db.Result{Cols: []string{"big"}}
	for i := 0; i < 200; i++ {
		r.Rows = append(r.Rows, []string{big})
	}
	m, _ := press(newGrid(r, [2]int{60, 10}), "/", "x")
	if len(m.view) != 200 {
		t.Fatalf("view=%d", len(m.view))
	}
	m.filter = "xy"
	if allocs := testing.AllocsPerRun(3, func() { m.rebuild() }); allocs > 20 {
		t.Fatalf("키 하나에 할당 %v번", allocs)
	}
	if len(m.view) != 0 {
		t.Fatalf("xy view=%d", len(m.view))
	}
}

func TestExport_거른_뒤에도_JSON_열_종류는_전체_결과를_따른다(t *testing.T) {
	r := &db.Result{Cols: []string{"v"}, Rows: [][]string{{"1"}, {"2"}, {"abc"}}}
	m := newGrid(r, [2]int{40, 10})
	_, a := press(m, "E")
	if !strings.Contains(a.Export.Content, `"v": "1"`) {
		t.Fatalf("전체: %s", a.Export.Content)
	}
	m, _ = press(m, "/", "1", "enter")
	_, a = press(m, "E")
	if a.Export.Content != "[\n  {\"v\": \"1\"}\n]\n" {
		t.Fatalf("거른 뒤: %q", a.Export.Content)
	}
}

func TestToCSV_열이_하나면_NULL도_빈_줄이_아니다(t *testing.T) {
	r := &db.Result{Cols: []string{"a"}, Rows: [][]string{{"NULL"}, {"x"}}}
	if got := ToCSV(r); got != "a\n\"\"\nx\n" {
		t.Fatalf("got %q", got)
	}
}

func TestKeys_묶여_온_글자는_키_이름으로_읽지_않는다(t *testing.T) {
	m := newGrid(numbersResult(30, 3), [2]int{60, 10})
	for _, s := range []string{"end", "down", "pgdown", "right"} {
		m, _ = m.Update(tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune(s)})
	}
	if m.row != 0 || m.col != 0 {
		t.Fatalf("row=%d col=%d", m.row, m.col)
	}
	if m, _ = press(m, "G"); m.row != 29 {
		t.Fatalf("G: row=%d", m.row)
	}
}

func TestView_타입_이름의_제어_문자를_보이는_글자로_그린다(t *testing.T) {
	r := &db.Result{Cols: []string{"a"}, Types: []string{"t\x1b]0;x\x07"}, Rows: [][]string{{"1"}}}
	m := newGrid(r, [2]int{60, 10})
	for _, keys := range [][]string{nil, {"v"}, {"enter"}} {
		g, _ := press(m, keys...)
		if out := g.View(true); strings.Contains(out, "\x1b]0;") || strings.Contains(out, "\x07") {
			t.Fatalf("%v: 제어 문자가 그대로 나갔다: %q", keys, out)
		}
	}
}

func TestToCSV_수식으로_실행될_글은_작은따옴표를_붙이고_숫자는_그대로_둔다(t *testing.T) {
	r := &db.Result{
		Cols: []string{"=cmd"},
		Rows: [][]string{{"=1+1"}, {"+A1"}, {"-2+3"}, {"@SUM(A1)"}, {"\tx"}, {"\rx"}, {"-1.5"}, {"+3"}, {"a=b"}},
	}
	want := "'=cmd\n'=1+1\n'+A1\n'-2+3\n'@SUM(A1)\n'\tx\n\"'\rx\"\n-1.5\n+3\na=b\n"
	if got := ToCSV(r); got != want {
		t.Fatalf("got %q\nwant %q", got, want)
	}
}

func TestStopTyping_거르기_입력을_끝내고_거른_글은_남긴다(t *testing.T) {
	m := newGrid(peopleResult(), [2]int{60, 10})
	m, _ = press(m, "/", "c", "a", "r", "o")
	m.StopTyping()
	if m.Typing() || m.filter != "caro" || len(m.view) != 1 {
		t.Fatalf("typing=%v filter=%q", m.Typing(), m.filter)
	}
}

func TestHeaderAt_머리_줄의_열_이름만_알리고_CurrentRow는_커서_행이다(t *testing.T) {
	m := newGrid(peopleResult(), [2]int{60, 12})
	l := m.layout()
	if col, ok := m.HeaderAt(l.cols[1].x+1, 0); !ok || col != "name" {
		t.Fatalf("열 이름 줄: %q %v", col, ok)
	}
	if col, ok := m.HeaderAt(l.cols[2].x+1, l.headLines-1); !ok || col != "city" {
		t.Fatalf("구분선 줄도 머리다: %q %v", col, ok)
	}
	if _, ok := m.HeaderAt(l.cols[1].x+1, l.headLines); ok {
		t.Fatal("데이터 줄은 머리가 아니다")
	}
	m, _ = press(m, "j")
	if row := m.CurrentRow(); len(row) != 3 || row[1] != "bob" {
		t.Fatalf("커서 행: %v", row)
	}
	if New().CurrentRow() != nil {
		t.Fatal("결과가 없으면 nil")
	}
}
