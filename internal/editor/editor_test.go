package editor

import (
	"bytes"
	"encoding/base64"
	"os"
	"regexp"
	"slices"
	"strings"
	"testing"
	"time"
	"unicode/utf8"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
)

// TestMain 은 복사 테스트가 실제 시스템 클립보드 명령(clip.exe 등)을 부르지 않게 OSC 52 만 쓰게 한다.
func TestMain(m *testing.M) {
	os.Setenv("DB_CLIPBOARD", "osc52")
	os.Exit(m.Run())
}

var (
	ansiRe = regexp.MustCompile(`\x1b\[[0-9;]*m`)

	namedKeys = map[string]tea.KeyType{
		"left": tea.KeyLeft, "right": tea.KeyRight, "up": tea.KeyUp, "down": tea.KeyDown,
		"shift+left": tea.KeyShiftLeft, "shift+right": tea.KeyShiftRight,
		"shift+up": tea.KeyShiftUp, "shift+down": tea.KeyShiftDown,
		"home": tea.KeyHome, "end": tea.KeyEnd, "shift+home": tea.KeyShiftHome, "shift+end": tea.KeyShiftEnd,
		"ctrl+left": tea.KeyCtrlLeft, "ctrl+right": tea.KeyCtrlRight,
		"ctrl+shift+left": tea.KeyCtrlShiftLeft, "ctrl+shift+right": tea.KeyCtrlShiftRight,
		"ctrl+home": tea.KeyCtrlHome, "ctrl+end": tea.KeyCtrlEnd, "pgup": tea.KeyPgUp, "pgdown": tea.KeyPgDown,
		"enter": tea.KeyEnter, "backspace": tea.KeyBackspace, "delete": tea.KeyDelete, "tab": tea.KeyTab,
		"esc": tea.KeyEsc, "ctrl+a": tea.KeyCtrlA, "ctrl+_": tea.KeyCtrlUnderscore, "ctrl+d": tea.KeyCtrlD,
		"ctrl+z": tea.KeyCtrlZ, "ctrl+y": tea.KeyCtrlY, "ctrl+x": tea.KeyCtrlX, "ctrl+k": tea.KeyCtrlK,
		"ctrl+v": tea.KeyCtrlV, "ctrl+@": tea.KeyCtrlAt, "ctrl+c": tea.KeyCtrlC, "ctrl+r": tea.KeyCtrlR,
		"space": tea.KeySpace, "ctrl+p": tea.KeyCtrlP, "ctrl+n": tea.KeyCtrlN,
	}
)

func newEd(text string) Model {
	m := New()
	m.SetTemplates(DefaultTemplates())
	m.SetSize(40, 8)
	m.Focus()
	m.SetValue(text)
	return m
}

func keyMsg(k string) tea.KeyMsg {
	if t, ok := namedKeys[k]; ok {
		if t == tea.KeySpace {
			return tea.KeyMsg{Type: t, Runes: []rune{' '}}
		}
		return tea.KeyMsg{Type: t}
	}
	return tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune(k)}
}

// press 는 키 이름이면 그 키를, 아니면 글자 하나씩 타이핑한다.
func press(m Model, keys ...string) Model {
	for _, k := range keys {
		if _, ok := namedKeys[k]; ok {
			m, _ = m.Update(keyMsg(k))
			continue
		}
		for _, r := range k {
			if r == ' ' {
				m, _ = m.Update(keyMsg("space"))
				continue
			}
			m, _ = m.Update(keyMsg(string(r)))
		}
	}
	return m
}

func plain(m Model) []string {
	return strings.Split(ansiRe.ReplaceAllString(m.View(), ""), "\n")
}

func TestSelection_한글은_바이트_위치로_돌려준다(t *testing.T) {
	m := newEd("")
	m = press(m, "가나다", "shift+left")
	s, e, ok := m.Selection()
	if !ok || s != 6 || e != 9 || m.CursorOffset() != 6 {
		t.Fatalf("got %d %d %v cur %d", s, e, ok, m.CursorOffset())
	}
	m = press(m, "shift+home")
	if s, e, _ = m.Selection(); s != 0 || e != 9 {
		t.Fatalf("shift+home got %d %d", s, e)
	}
	m = press(m, "라")
	if m.Value() != "라" || m.CursorOffset() != 3 {
		t.Fatalf("typing over selection got %q %d", m.Value(), m.CursorOffset())
	}
}

func TestSetCursorOffset_범위_밖은_끝으로_글자_가운데는_글자_앞으로(t *testing.T) {
	m := newEd("가나")
	m.SetCursorOffset(4)
	if m.CursorOffset() != 3 {
		t.Fatalf("글자 가운데: %d", m.CursorOffset())
	}
	m.SetCursorOffset(99)
	if m.CursorOffset() != 6 {
		t.Fatalf("범위 밖: %d", m.CursorOffset())
	}
	m.SetCursorOffset(-1)
	if m.CursorOffset() != 0 {
		t.Fatalf("음수: %d", m.CursorOffset())
	}
}

func TestSelection_단어_선택과_전체_선택(t *testing.T) {
	m := newEd("select  users.name")
	m = press(m, "end", "ctrl+shift+left")
	if s, e, _ := m.Selection(); m.Value()[s:e] != "name" {
		t.Fatalf("got %q", m.Value()[s:e])
	}
	m = press(m, "ctrl+a", "x")
	if m.Value() != "x" {
		t.Fatalf("got %q", m.Value())
	}
}

func TestSelection_선택_없이_좌우를_누르면_선택_끝으로_접힌다(t *testing.T) {
	m := newEd("abcdef")
	m = press(m, "right", "shift+right", "shift+right", "left")
	if _, _, ok := m.Selection(); ok || m.CursorOffset() != 1 {
		t.Fatalf("cur %d sel %v", m.CursorOffset(), ok)
	}
}

func TestWordMove_단어와_공백_기호_줄바꿈에서_멈춘다(t *testing.T) {
	s := "select  foo.bar\nx"
	var stops []int
	for off := 0; off < len(s); {
		off = wordRight(s, off)
		stops = append(stops, off)
	}
	want := []int{6, 11, 12, 15, 16, 17}
	if !slices.Equal(stops, want) {
		t.Fatalf("right %v want %v", stops, want)
	}
	stops = stops[:0]
	for off := len(s); off > 0; {
		off = wordLeft(s, off)
		stops = append(stops, off)
	}
	want = []int{16, 15, 12, 11, 8, 0}
	if !slices.Equal(stops, want) {
		t.Fatalf("left %v want %v", stops, want)
	}
}

func TestMove_위아래는_넓은_글자에서도_화면_열을_지킨다(t *testing.T) {
	m := newEd("가나다라\nabcdefgh\n가")
	m = press(m, "right", "right", "down")
	if m.CursorOffset() != len("가나다라\n")+4 {
		t.Fatalf("down got %d", m.CursorOffset())
	}
	m = press(m, "down")
	if m.CursorOffset() != len(m.Value()) {
		t.Fatalf("short line got %d", m.CursorOffset())
	}
	m = press(m, "up")
	if m.CursorOffset() != len("가나다라\n")+4 {
		t.Fatalf("goal column lost: %d", m.CursorOffset())
	}
}

func TestMove_홈은_들여쓰기_끝과_줄_맨_앞을_오간다(t *testing.T) {
	m := newEd("    select")
	m = press(m, "end", "home")
	if m.CursorOffset() != 4 {
		t.Fatalf("got %d", m.CursorOffset())
	}
	if m = press(m, "home"); m.CursorOffset() != 0 {
		t.Fatalf("got %d", m.CursorOffset())
	}
}

func TestMove_CRLF_에서는_줄_사이를_한_번에_넘는다(t *testing.T) {
	m := newEd("ab\r\ncd")
	m = press(m, "end")
	if m.CursorOffset() != 2 {
		t.Fatalf("end got %d", m.CursorOffset())
	}
	if m = press(m, "right"); m.CursorOffset() != 4 {
		t.Fatalf("right got %d", m.CursorOffset())
	}
	if m = press(m, "backspace"); m.Value() != "abcd" {
		t.Fatalf("backspace got %q", m.Value())
	}
	m = press(m, "enter")
	if m.Value() != "ab\ncd" {
		t.Fatalf("enter got %q", m.Value())
	}
}

func TestEdit_엔터는_들여쓰기를_잇고_여는_괄호_뒤에서_한_단계_더_들인다(t *testing.T) {
	m := newEd("  select (")
	m = press(m, "end", "enter")
	if m.Value() != "  select (\n      " {
		t.Fatalf("got %q", m.Value())
	}
}

func TestEdit_지우기는_선택을_먼저_지운다(t *testing.T) {
	m := newEd("가나다")
	m = press(m, "shift+right", "delete")
	if m.Value() != "나다" {
		t.Fatalf("got %q", m.Value())
	}
	m = press(m, "end", "backspace")
	if m.Value() != "나" {
		t.Fatalf("got %q", m.Value())
	}
}

func TestToggleComment_한_줄은_붙이고_다시_누르면_벗긴다(t *testing.T) {
	m := newEd("  select 1")
	m = press(m, "end")
	m.toggleComment()
	if m.Value() != "--   select 1" || m.CursorOffset() != len(m.Value()) {
		t.Fatalf("got %q cur %d", m.Value(), m.CursorOffset())
	}
	m = press(m, "ctrl+_")
	if m.Value() != "  select 1" || m.CursorOffset() != len(m.Value()) {
		t.Fatalf("got %q cur %d", m.Value(), m.CursorOffset())
	}
}

func TestToggleComment_선택한_줄_모두에_적용하고_빈_줄은_건너뛴다(t *testing.T) {
	m := newEd("a\n\nb\nc")
	m = press(m, "shift+down", "shift+down", "shift+down")
	m.toggleComment()
	if m.Value() != "-- a\n\n-- b\nc" {
		t.Fatalf("got %q", m.Value())
	}
	m.toggleComment()
	if m.Value() != "a\n\nb\nc" {
		t.Fatalf("got %q", m.Value())
	}
}

func TestToggleComment_Redis_에서는_아무것도_하지_않는다(t *testing.T) {
	m := newEd("GET k")
	m.SetLanguage(LangRedis)
	if m = press(m, "ctrl+_"); m.Value() != "GET k" {
		t.Fatalf("got %q", m.Value())
	}
}

func TestDuplicateLines_줄을_아래에_복제하고_커서를_따라_옮긴다(t *testing.T) {
	m := newEd("ab\ncd")
	m = press(m, "right")
	m.duplicateLines()
	if m.Value() != "ab\nab\ncd" || m.CursorOffset() != 4 {
		t.Fatalf("got %q cur %d", m.Value(), m.CursorOffset())
	}
}

func TestUndo_연속_타이핑은_한_번에_되돌리고_다시_할_수_있다(t *testing.T) {
	m := newEd("")
	m = press(m, "abc", "enter", "가나")
	if m = press(m, "ctrl+z"); m.Value() != "abc\n" {
		t.Fatalf("undo 1 got %q", m.Value())
	}
	if m = press(m, "ctrl+z", "ctrl+z"); m.Value() != "" {
		t.Fatalf("undo all got %q", m.Value())
	}
	if m = press(m, "ctrl+y"); m.Value() != "abc" || m.CursorOffset() != 3 {
		t.Fatalf("redo got %q cur %d", m.Value(), m.CursorOffset())
	}
	m = press(m, "x")
	if m = press(m, "ctrl+y"); m.Value() != "abcx" {
		t.Fatalf("redo after new edit got %q", m.Value())
	}
}

func TestUndo_커서를_옮기면_타이핑_묶음이_끊긴다(t *testing.T) {
	m := newEd("")
	m = press(m, "ab", "left", "cd")
	if m = press(m, "ctrl+z"); m.Value() != "ab" {
		t.Fatalf("got %q", m.Value())
	}
}

func TestUndo_기록은_200개까지만_남는다(t *testing.T) {
	m := newEd("")
	for i := 0; i < 250; i++ {
		m = press(m, "enter")
	}
	if len(m.undo) != undoLimit {
		t.Fatalf("got %d", len(m.undo))
	}
}

func TestClipboard_복사는_OSC52_를_쓰고_붙여넣기는_내부_클립보드를_쓴다(t *testing.T) {
	var buf bytes.Buffer
	old := clipboardOut
	clipboardOut = &buf
	t.Cleanup(func() { clipboardOut = old })

	m := newEd("select 한글")
	m = press(m, "end", "ctrl+shift+left")
	m, cmd := m.Update(keyMsg("ctrl+k"))
	if cmd == nil || cmd() != nil {
		t.Fatal("copy cmd failed")
	}
	if want := "\x1b]52;c;" + base64.StdEncoding.EncodeToString([]byte("한글")) + "\a"; buf.String() != want {
		t.Fatalf("osc52 got %q", buf.String())
	}
	m = press(m, "end", "ctrl+v")
	if m.Value() != "select 한글한글" {
		t.Fatalf("paste got %q", m.Value())
	}
}

func TestClipboard_선택_없이_잘라내면_현재_줄을_지운다(t *testing.T) {
	m := newEd("a\nb\nc")
	m = press(m, "down")
	m, _ = m.Update(keyMsg("ctrl+x"))
	if m.Value() != "a\nc" || m.clip != "b\n" {
		t.Fatalf("got %q clip %q", m.Value(), m.clip)
	}
	m = press(m, "ctrl+end")
	m, _ = m.Update(keyMsg("ctrl+x"))
	if m.Value() != "a" {
		t.Fatalf("last line got %q", m.Value())
	}
}

func TestPaste_괄호_붙여넣기는_줄바꿈만_맞추고_그대로_넣는다(t *testing.T) {
	m := newEd("  (")
	m.SetCompleter(func(string, int) []Item { return []Item{{Label: "x"}} })
	m = press(m, "end")
	m, _ = m.Update(tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("a\r\nb\rc"), Paste: true})
	if m.Value() != "  (a\nb\nc" || m.PopupOpen() {
		t.Fatalf("got %q popup %v", m.Value(), m.PopupOpen())
	}
}

func TestComplete_타이핑하면_열리고_엔터로_단어_조각을_바꾼다(t *testing.T) {
	var gotOff int
	m := newEd("select ")
	m.SetCompleter(func(text string, off int) []Item {
		gotOff = off
		return []Item{{Label: "users", Kind: "table"}, {Label: "user_id", Kind: "column", Insert: "user_id"}}
	})
	m = press(m, "end", "us")
	if !m.PopupOpen() || gotOff != len("select us") {
		t.Fatalf("popup %v off %d", m.PopupOpen(), gotOff)
	}
	view := strings.Join(plain(m), "\n")
	if !strings.Contains(view, "users") || !strings.Contains(view, "user_id") {
		t.Fatalf("popup not drawn:\n%s", view)
	}
	m = press(m, "down", "enter")
	if m.Value() != "select user_id" || m.PopupOpen() {
		t.Fatalf("got %q popup %v", m.Value(), m.PopupOpen())
	}
	if m = press(m, "end", "enter"); m.Value() != "select user_id\n" {
		t.Fatalf("closed enter got %q", m.Value())
	}
}

func TestComplete_닫혀_있으면_탭을_먹지_않고_esc_로_닫는다(t *testing.T) {
	m := newEd("")
	m.SetCompleter(func(text string, off int) []Item { return []Item{{Label: "SELECT"}} })
	m = press(m, "tab")
	if m.Value() != "" || m.PopupOpen() {
		t.Fatalf("tab got %q", m.Value())
	}
	m = press(m, "ctrl+@")
	if !m.PopupOpen() {
		t.Fatal("ctrl+space did not open")
	}
	if m = press(m, "esc"); m.PopupOpen() {
		t.Fatal("esc did not close")
	}
	m = press(m, "s", "tab")
	if m.Value() != "SELECT" {
		t.Fatalf("tab accept got %q", m.Value())
	}
}

func TestComplete_후보가_없으면_열지_않는다(t *testing.T) {
	m := newEd("")
	m.SetCompleter(func(string, int) []Item { return nil })
	if m = press(m, "a"); m.PopupOpen() {
		t.Fatal("popup opened without items")
	}
}

func TestComplete_따옴표를_연_채_고르면_따옴표가_겹치지_않는다(t *testing.T) {
	m := newEd("select \"Us")
	m.SetCompleter(func(string, int) []Item { return []Item{{Label: "User", Insert: `"User"`}} })
	m = press(m, "end", "ctrl+@", "enter")
	if m.Value() != `select "User"` {
		t.Fatalf("got %q", m.Value())
	}
}

func TestMouse_클릭은_거터와_넓은_글자를_고려해_커서를_옮긴다(t *testing.T) {
	m := newEd("가나다\nabc")
	gw := m.gutterWidth()
	m.HandleMouse(tea.MouseMsg{X: gw + 3, Y: 0, Button: tea.MouseButtonLeft, Action: tea.MouseActionPress})
	if m.CursorOffset() != 3 {
		t.Fatalf("wide click got %d", m.CursorOffset())
	}
	m.HandleMouse(tea.MouseMsg{Action: tea.MouseActionRelease})
	m.HandleMouse(tea.MouseMsg{X: 0, Y: 1, Button: tea.MouseButtonLeft, Action: tea.MouseActionPress})
	if m.CursorOffset() != len("가나다\n") {
		t.Fatalf("gutter click got %d", m.CursorOffset())
	}
	m.HandleMouse(tea.MouseMsg{X: 30, Y: 5, Button: tea.MouseButtonLeft, Action: tea.MouseActionMotion})
	m.HandleMouse(tea.MouseMsg{Action: tea.MouseActionRelease})
	if s, e, ok := m.Selection(); !ok || m.Value()[s:e] != "abc" {
		t.Fatalf("drag got %d %d %v", s, e, ok)
	}
}

func TestMouse_더블클릭은_단어를_고른다(t *testing.T) {
	clock := time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC)
	old := now
	now = func() time.Time { return clock }
	t.Cleanup(func() { now = old })

	m := newEd("select 사용자_id from t")
	gw := m.gutterWidth()
	click := tea.MouseMsg{X: gw + 9, Y: 0, Button: tea.MouseButtonLeft, Action: tea.MouseActionPress}
	m.HandleMouse(click)
	m.HandleMouse(tea.MouseMsg{Action: tea.MouseActionRelease})
	clock = clock.Add(100 * time.Millisecond)
	m.HandleMouse(click)
	m.HandleMouse(tea.MouseMsg{Action: tea.MouseActionRelease})
	if s, e, ok := m.Selection(); !ok || m.Value()[s:e] != "사용자_id" {
		t.Fatalf("got %d %d %v", s, e, ok)
	}
}

func TestMouse_휠은_커서를_두고_화면만_굴린다(t *testing.T) {
	m := newEd(strings.Repeat("x\n", 30))
	m.HandleMouse(tea.MouseMsg{Button: tea.MouseButtonWheelDown, Action: tea.MouseActionPress})
	if m.top != wheelStep || m.CursorOffset() != 0 {
		t.Fatalf("top %d cur %d", m.top, m.CursorOffset())
	}
	if got := plain(m)[0]; !strings.HasPrefix(strings.TrimLeft(got, " "), "4 ") {
		t.Fatalf("first row %q", got)
	}
	m.HandleMouse(tea.MouseMsg{Button: tea.MouseButtonWheelUp, Action: tea.MouseActionPress})
	m.HandleMouse(tea.MouseMsg{Button: tea.MouseButtonWheelUp, Action: tea.MouseActionPress})
	if m.top != 0 {
		t.Fatalf("top %d", m.top)
	}
}

func TestView_가로_스크롤은_커서를_보이게_하고_넓은_글자_반쪽은_공백이다(t *testing.T) {
	m := newEd(strings.Repeat("a", 50) + "가")
	m.SetSize(20, 3)
	m = press(m, "end")
	if m.left == 0 {
		t.Fatal("no horizontal scroll")
	}
	rows := plain(m)
	if !strings.HasSuffix(strings.TrimRight(rows[0], " "), "가") {
		t.Fatalf("row %q", rows[0])
	}
	m = press(m, "left", "left")
	m.left++
	for _, r := range plain(m) {
		if w := lipgloss.Width(r); w != 20 {
			t.Fatalf("width %d row %q", w, r)
		}
	}
}

func TestView_모든_줄은_정확히_폭과_높이를_채운다(t *testing.T) {
	m := newEd("select '한글'\tx\n\x01ctl")
	rows := plain(m)
	if len(rows) != 8 {
		t.Fatalf("rows %d", len(rows))
	}
	for _, r := range rows {
		if w := lipgloss.Width(r); w != 40 {
			t.Fatalf("width %d row %q", w, r)
		}
	}
	if !strings.Contains(rows[1], "?ctl") {
		t.Fatalf("control char not replaced: %q", rows[1])
	}
}

func TestView_아주_작은_크기에서도_패닉하지_않는다(t *testing.T) {
	m := newEd("가나다\nselect")
	m.SetCompleter(func(string, int) []Item { return []Item{{Label: "아주긴후보이름", Detail: "text"}} })
	for _, sz := range [][2]int{{0, 0}, {1, 1}, {2, 1}, {3, 2}, {5, 1}, {-1, -1}} {
		m.SetSize(sz[0], sz[1])
		m = press(m, "ctrl+end", "a", "가")
		v := m.View()
		if sz[0] <= 0 || sz[1] <= 0 {
			if v != "" {
				t.Fatalf("size %v view %q", sz, v)
			}
			continue
		}
		rows := plain(m)
		if len(rows) != sz[1] {
			t.Fatalf("size %v rows %d", sz, len(rows))
		}
		for _, r := range rows {
			if lipgloss.Width(r) != sz[0] {
				t.Fatalf("size %v row %q", sz, r)
			}
		}
	}
}

func TestUpdate_포커스가_없거나_전역_키면_건드리지_않는다(t *testing.T) {
	m := newEd("abc")
	m.Blur()
	if m = press(m, "x"); m.Value() != "abc" {
		t.Fatalf("blurred got %q", m.Value())
	}
	m.Focus()
	for _, k := range []string{"ctrl+c", "ctrl+r", "tab"} {
		if m = press(m, k); m.Value() != "abc" || m.CursorOffset() != 0 {
			t.Fatalf("%s changed %q", k, m.Value())
		}
	}
}

func FuzzEdits_패닉없이_커서와_선택은_글자_경계에_있다(f *testing.F) {
	f.Add("select 1;\n-- 한글 '문자열'\r\n$$x$$", []byte{0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12})
	f.Add("", []byte{20, 21, 22, 23, 24, 25, 26})
	f.Add(strings.Repeat("x\n", 12), []byte{24, 24, 24, 24, 30, 0xf0, 0xf0, 0xf0, 0xf0, 30, 0xf1})
	ops := []string{
		"left", "right", "up", "down", "shift+left", "shift+right", "shift+up", "shift+down", "home", "end",
		"ctrl+left", "ctrl+shift+right", "enter", "backspace", "delete", "ctrl+a", "ctrl+_", "ctrl+d",
		"ctrl+z", "ctrl+y", "ctrl+x", "ctrl+k", "ctrl+v", "pgup", "pgdown", "가", "x", "(", "\t", "space",
		"ctrl+@", "tab", "esc", "sel",
	}
	f.Fuzz(func(t *testing.T, text string, prog []byte) {
		if !utf8.ValidString(text) {
			return
		}
		m := newEd(text)
		m.SetSize(12, 4)
		m.SetCompleter(func(string, int) []Item { return []Item{{Label: "가나다라마바사"}} })
		for i, b := range prog {
			if b >= 0xf0 {
				m.HandleMouse(tea.MouseMsg{Button: tea.MouseButtonWheelUp + tea.MouseButton(b%4), Action: tea.MouseActionPress})
				continue
			}
			if b >= byte(len(ops)) {
				m.HandleMouse(tea.MouseMsg{X: int(b % 13), Y: i % 5, Button: tea.MouseButtonLeft, Action: tea.MouseActionPress})
				continue
			}
			m = press(m, ops[b])
			// 전체 선택 뒤 줄 복제·붙여넣기를 반복하면 내용이 두 배씩 늘어 퍼저가 멈춘다
			if len(m.Value()) > 4096 {
				return
			}
			if !utf8.ValidString(m.Value()) {
				t.Fatalf("invalid utf8 after %q: %q", ops[b], m.Value())
			}
			cur := m.CursorOffset()
			if cur < 0 || cur > len(m.Value()) || cur < len(m.Value()) && !utf8.RuneStart(m.Value()[cur]) {
				t.Fatalf("bad cursor %d in %q", cur, m.Value())
			}
			if s, e, ok := m.Selection(); ok && (s < 0 || e > len(m.Value()) || s >= e) {
				t.Fatalf("bad selection %d %d", s, e)
			}
			for _, st := range m.stops {
				if st.start < 0 || st.start > st.end || st.end > len(m.Value()) {
					t.Fatalf("bad stop %v in %q", st, m.Value())
				}
			}
			// 폭은 글자 단위로 잰다. 자모 조합·이모지 ZWJ 같은 다중 글자 클러스터는 lipgloss(클러스터 단위)와 다를 수 있다
			if rows := plain(m); len(rows) != 4 {
				t.Fatalf("rows %d", len(rows))
			}
			for y := 0; y < 4; y++ {
				if row := m.renderRow(m.top + y); len(row) != 12 {
					t.Fatalf("row %d has %d cells", y, len(row))
				}
			}
		}
	})
}

func TestRenderRow_선택_칸과_커서_칸을_표시한다(t *testing.T) {
	m := newEd("ab\ncd")
	m = press(m, "right", "shift+down")
	row := m.renderRow(0)[m.gutterWidth():]
	if row[0].bg != bgNone || row[1].bg != bgSel || row[2].bg != bgSel {
		t.Fatalf("row0 bg %v %v %v", row[0].bg, row[1].bg, row[2].bg)
	}
	row = m.renderRow(1)[m.gutterWidth():]
	if row[0].bg != bgSel || !row[1].cursor || row[1].bg != bgLine {
		t.Fatalf("row1 %+v %+v", row[0], row[1])
	}
}

func TestView_팝업은_아래_자리가_없으면_커서_위에_그린다(t *testing.T) {
	m := newEd(strings.Repeat("\n", 7))
	m.SetCompleter(func(string, int) []Item { return []Item{{Label: "alpha", Detail: "table"}, {Label: "beta"}} })
	m = press(m, "ctrl+end", "a")
	rows := plain(m)
	if !strings.Contains(rows[5], "alpha") || !strings.Contains(rows[5], "table") || !strings.Contains(rows[6], "beta") {
		t.Fatalf("popup rows:\n%s", strings.Join(rows, "\n"))
	}
}

func TestComplete_팝업이_열린_채_휠로_굴려도_패닉하지_않고_팝업을_닫는다(t *testing.T) {
	m := newEd(strings.Repeat("x\n", 29) + "x")
	m.SetCompleter(func(string, int) []Item { return []Item{{Label: "xa"}, {Label: "xb"}} })
	m = press(m, "ctrl+end", "ctrl+@")
	if !m.PopupOpen() {
		t.Fatal("popup did not open")
	}
	for i := 0; i < 12; i++ {
		m.HandleMouse(tea.MouseMsg{Button: tea.MouseButtonWheelUp, Action: tea.MouseActionPress})
		_ = m.View()
	}
	if m.PopupOpen() {
		t.Fatal("wheel did not close popup")
	}
	m = press(m, "ctrl+@")
	if !m.PopupOpen() || m.rowOf(m.cur) < m.top || m.rowOf(m.cur) >= m.top+m.h {
		t.Fatalf("ctrl+@ popup %v top %d", m.PopupOpen(), m.top)
	}
	_ = m.View()
}

func TestView_커서가_화면_밖이면_팝업을_그리지_않는다(t *testing.T) {
	m := newEd(strings.Repeat("x\n", 29) + "x")
	m.SetCompleter(func(string, int) []Item { return []Item{{Label: "zz"}} })
	m = press(m, "ctrl+end", "ctrl+@")
	m.top = 0
	if view := strings.Join(plain(m), "\n"); strings.Contains(view, "zz") {
		t.Fatalf("popup drawn off-screen:\n%s", view)
	}
}

func TestComplete_PrefixLen_만큼_커서_앞을_바꾼다(t *testing.T) {
	cases := []struct {
		text string
		it   Item
		want string
	}{
		{`select * from "My Ta`, Item{Label: "My Table", Insert: `"My Table"`, PrefixLen: 6}, `select * from "My Table"`},
		{"GET user:1", Item{Label: "user:123", PrefixLen: 6}, "GET user:123"},
		{"가나", Item{Label: "다", PrefixLen: 4}, "가다"},        // 글자 중간이면 다음 글자 경계로 줄인다
		{"ab", Item{Label: "xyz", PrefixLen: 99}, "xyz"},    // 커서 앞보다 길면 맨 앞까지
		{"select us", Item{Label: "users"}, "select users"}, // 0 이면 단어 규칙
	}
	for _, c := range cases {
		m := newEd(c.text)
		m.SetCompleter(func(string, int) []Item { return []Item{c.it} })
		m = press(m, "ctrl+end", "ctrl+@", "enter")
		if m.Value() != c.want {
			t.Fatalf("%q: got %q want %q", c.text, m.Value(), c.want)
		}
	}
}

func TestUpdate_묶여_온_글자는_키_이름이어도_그대로_넣는다(t *testing.T) {
	m := newEd("ab")
	for _, s := range []string{"end", "left", "home", "up", "enter"} {
		m, _ = m.Update(tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune(s)})
	}
	if m.Value() != "endlefthomeupenterab" {
		t.Fatalf("got %q", m.Value())
	}
	m.SetCompleter(func(string, int) []Item { return []Item{{Label: "zzz"}} })
	m = press(m, "ctrl+@")
	m, _ = m.Update(tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("tab")})
	if !strings.HasPrefix(m.Value(), "endlefthomeupentertab") {
		t.Fatalf("popup got %q", m.Value())
	}
}

func TestView_C1_제어_문자는_물음표로_그린다(t *testing.T) {
	m := newEd("a\u009d52;c;eA==\u0007")
	view := m.View()
	if strings.ContainsRune(view, '\u009d') || strings.ContainsRune(view, '\u0007') {
		t.Fatalf("control char leaked: %q", view)
	}
	if !strings.Contains(plain(m)[0], "a?52;c;eA==?") {
		t.Fatalf("got %q", plain(m)[0])
	}
}

func TestClipboard_빈_줄에서_ctrl_k_는_클립보드를_건드리지_않는다(t *testing.T) {
	m := newEd("abc\n")
	m = press(m, "end", "shift+left", "ctrl+k")
	m = press(m, "ctrl+end")
	m, cmd := m.Update(keyMsg("ctrl+k"))
	if cmd != nil || m.clip != "c" {
		t.Fatalf("cmd %v clip %q", cmd != nil, m.clip)
	}
}

func TestEdit_선택을_엔터로_바꾸면_선택_시작_줄의_들여쓰기를_잇는다(t *testing.T) {
	m := newEd("  ab\ncd")
	m = press(m, "end", "shift+down", "enter")
	if m.Value() != "  ab\n  " {
		t.Fatalf("got %q", m.Value())
	}
}

func TestTemplate_탭으로_펼치고_자리를_돌아_END_에서_끝난다(t *testing.T) {
	m := newEd("")
	m = press(m, "ins")
	if !m.WantsTab() {
		t.Fatal("WantsTab false after template key")
	}
	m = press(m, "tab")
	if m.Value() != "INSERT INTO table (cols) VALUES ();" {
		t.Fatalf("expand got %q", m.Value())
	}
	if s, e, ok := m.Selection(); !ok || m.Value()[s:e] != "table" {
		t.Fatalf("first stop not selected: %d %d %v", s, e, ok)
	}
	m = press(m, "users", "tab")
	if s, e, ok := m.Selection(); !ok || m.Value()[s:e] != "cols" {
		t.Fatalf("second stop got %q", m.Value())
	}
	m = press(m, "id, n", "tab")
	want := "INSERT INTO users (id, n) VALUES ();"
	if m.Value() != want || m.CursorOffset() != strings.Index(want, ")")+len(") VALUES (") {
		t.Fatalf("end got %q cur %d", m.Value(), m.CursorOffset())
	}
	if m.WantsTab() {
		t.Fatal("template mode did not end at $END$")
	}
}

func TestTemplate_자동완성_창에서_고르면_템플릿을_펼친다(t *testing.T) {
	m := newEd("")
	m.SetCompleter(func(text string, off int) []Item {
		if strings.HasSuffix(text[:off], "sel") {
			return []Item{{Label: "SELECT"}}
		}
		return nil
	})
	m = press(m, "sel")
	if !m.PopupOpen() || m.pop.items[0].Label != "sel" || m.pop.items[1].Label != "SELECT" {
		t.Fatalf("popup items %+v", m.pop.items)
	}
	m = press(m, "tab")
	if m.Value() != "SELECT * FROM table ;" {
		t.Fatalf("got %q", m.Value())
	}
	m = press(m, "t", "tab")
	if m.Value() != "SELECT * FROM t ;" || m.CursorOffset() != len("SELECT * FROM t ") {
		t.Fatalf("got %q cur %d", m.Value(), m.CursorOffset())
	}
}

func TestPopup_Ctrl_P는_위로_Ctrl_N은_아래로_옮기고_끝에서_돈다(t *testing.T) {
	m := newEd("")
	m.SetCompleter(func(string, int) []Item { return []Item{{Label: "users"}, {Label: "user_id"}, {Label: "username"}} })
	m = press(m, "us")
	if !m.PopupOpen() || m.pop.sel != 0 {
		t.Fatalf("popup %v sel %d", m.PopupOpen(), m.pop.sel)
	}
	if m = press(m, "ctrl+n", "ctrl+n"); m.pop.sel != 2 {
		t.Fatalf("Ctrl+N 두 번: %d", m.pop.sel)
	}
	if m = press(m, "ctrl+p"); m.pop.sel != 1 || m.Value() != "us" {
		t.Fatalf("Ctrl+P: %d %q", m.pop.sel, m.Value())
	}
	if m = press(m, "ctrl+p", "ctrl+p"); m.pop.sel != len(m.pop.items)-1 || !m.PopupOpen() {
		t.Fatalf("맨 위에서 Ctrl+P 는 맨 아래로: %d", m.pop.sel)
	}
}

func TestTemplate_자리_안에서는_템플릿을_내밀지_않고_탭은_다음_자리로_간다(t *testing.T) {
	m := newEd("")
	m = press(m, "ins", "tab", "u")
	if m.PopupOpen() {
		t.Fatalf("template offered inside stop: %+v", m.pop.items)
	}
	m = press(m, "tab")
	if s, e, ok := m.Selection(); m.Value() != "INSERT INTO u (cols) VALUES ();" || !ok || m.Value()[s:e] != "cols" {
		t.Fatalf("got %q", m.Value())
	}
	// 완성기 창이 열려 있어도 Tab 은 고르지 않고 다음 자리로 간다
	m.SetCompleter(func(string, int) []Item { return []Item{{Label: "users_id"}} })
	m = press(m, "id")
	if !m.PopupOpen() || !m.WantsTab() {
		t.Fatalf("popup %v wants %v", m.PopupOpen(), m.WantsTab())
	}
	m = press(m, "tab")
	if m.Value() != "INSERT INTO u (id) VALUES ();" || m.PopupOpen() || m.WantsTab() {
		t.Fatalf("got %q popup %v", m.Value(), m.PopupOpen())
	}
}

func TestTemplate_이름이_아니면_탭을_먹지_않는다(t *testing.T) {
	for _, c := range []struct {
		name, text string
		cur        int
	}{
		{"이름 아님", "select", 6},
		{"단어 가운데", "select", 3},
		{"점 뒤", "t.sel", 5},
		{"더 긴 단어", "xsel", 4},
	} {
		m := newEd(c.text)
		m.cur = c.cur
		if m.WantsTab() {
			t.Errorf("%s: WantsTab true", c.name)
		}
		if m = press(m, "tab"); m.Value() != c.text {
			t.Errorf("%s: tab changed %q", c.name, m.Value())
		}
	}
	m := newEd("sel")
	m.SetLanguage(LangRedis)
	m.cur = 3
	if m.WantsTab() {
		t.Error("redis expanded template")
	}
	m = newEd("sel")
	m.SetTemplates(nil)
	m.cur = 3
	if m.WantsTab() {
		t.Error("nil templates still expand")
	}
}

func TestTemplate_자리_밖을_고치거나_esc_면_템플릿_모드를_끝낸다(t *testing.T) {
	m := newEd("")
	m = press(m, "upd", "esc", "tab", "esc")
	if m.WantsTab() {
		t.Fatal("esc did not leave template mode")
	}
	m = newEd("")
	m = press(m, "del", "esc", "tab", "left", "tab")
	if m.Value() != "DELETE FROM table WHERE ;" {
		t.Fatalf("tab after move got %q", m.Value())
	}
	m = newEd("x ")
	m.cur = 2
	m = press(m, "cnt", "esc", "tab")
	m = press(m, "ctrl+a", "z")
	if m.Value() != "z" || len(m.stops) != 0 {
		t.Fatalf("edit outside stop kept template: %q %v", m.Value(), m.stops)
	}
}

func TestExpand_자리와_END_위치를_계산한다(t *testing.T) {
	text, stops := expand("UPDATE $TABLE$ SET $END$ WHERE $COL$ $$x$$")
	if text != "UPDATE table SET  WHERE col $$x$$" {
		t.Fatalf("text %q", text)
	}
	want := []stop{{7, 12}, {24, 27}, {17, 17}}
	if !slices.Equal(stops, want) {
		t.Fatalf("stops %v want %v", stops, want)
	}
	if text, stops = expand("LIMIT 1"); text != "LIMIT 1" || !slices.Equal(stops, []stop{{7, 7}}) {
		t.Fatalf("no placeholder %q %v", text, stops)
	}
}

func TestIdentAt_커서_위나_바로_앞_이름을_한정자까지_돌려준다(t *testing.T) {
	// | 는 커서 자리다
	cases := []struct{ text, want string }{
		{"select * from |orders c", "orders"},
		{"select * from ord|ers c", "orders"},
		{"select * from orders| c", "orders"},
		{"select * from public.ord|ers", "public.orders"},
		{"select * from pub|lic.orders", "public.orders"},
		{"select * from public|.orders", "public.orders"},
		{"select * from public.|orders", "public.orders"},
		{"select * from public.|", "public"},
		{"select * from public.| where", "public"},
		{`select * from "Big |Name"`, `"Big Name"`},
		{`select * from "Big Name"|`, `"Big Name"`},
		{`select * from s."T|"`, `s."T"`},
		{`select * from "My S"|."a.b"`, `"My S"."a.b"`},
		{"select * from db.s.|t", "db.s.t"},
		{"select c.na|me from t c", "c.name"},
		{"select 이름 from 사용|자 where 나이 > 1", "사용자"},
		{"select 이름 from 스키마.사용자|", "스키마.사용자"},
		{"select * from comments|", "comments"},
		{"select now|()", "now"},
		{"select a$b|", "a$b"},
		{"|select", "select"},
		{"select * from t |", ""},
		{"select | from t", ""},
		{"|", ""},
		{"select 'abc|def'", ""},
		{"select 'abc'|", ""},
		{"select E'it\\'s |x'", ""},
		{"select 1 -- ord|ers", ""},
		{"select 1 -- orders|", ""},
		{"/* public.|t */ select", ""},
		{"select $$ body |$$", ""},
		{"limit 10|0", ""},
		{"where id = $1|", ""},
	}
	for _, c := range cases {
		m := newEd(strings.Replace(c.text, "|", "", 1))
		m.cur = strings.Index(c.text, "|")
		if got := m.IdentAt(); got != c.want {
			t.Errorf("%q: %q, want %q", c.text, got, c.want)
		}
	}
}

func TestIdentAt_Redis_편집기는_빈_글이다(t *testing.T) {
	m := newEd("GET user:1")
	m.SetLanguage(LangRedis)
	m.cur = 5
	if got := m.IdentAt(); got != "" {
		t.Fatalf("redis: %q", got)
	}
}
