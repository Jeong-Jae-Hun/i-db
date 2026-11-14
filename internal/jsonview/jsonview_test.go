package jsonview

import (
	"strings"
	"testing"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/x/ansi"
)

const sample = `{"zeta": 1, "alpha": {"b": [10, 20], "a b": "x"}, "items": [{"name": "kim", "big": 12345678901234567890123, "f": 1.10}], "ok": true, "none": null}`

func mustNew(t *testing.T, raw string) Model {
	t.Helper()
	m, err := New(raw)
	if err != nil {
		t.Fatalf("New: %v", err)
	}
	m.SetSize(80, 40)
	return m
}

func lines(m Model) []string {
	return strings.Split(ansi.Strip(m.View()), "\n")
}

func press(m Model, keys ...string) (Model, Action) {
	var a Action
	for _, k := range keys {
		msg := tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune(k)}
		switch k {
		case "enter":
			msg = tea.KeyMsg{Type: tea.KeyEnter}
		case "space":
			msg = tea.KeyMsg{Type: tea.KeySpace, Runes: []rune(" ")}
		case "down":
			msg = tea.KeyMsg{Type: tea.KeyDown}
		case "left":
			msg = tea.KeyMsg{Type: tea.KeyLeft}
		case "right":
			msg = tea.KeyMsg{Type: tea.KeyRight}
		case "pgdown":
			msg = tea.KeyMsg{Type: tea.KeyPgDown}
		case "esc":
			msg = tea.KeyMsg{Type: tea.KeyEsc}
		}
		m, a = m.Update(msg)
	}
	return m, a
}

// find 는 경로가 p 인 보이는 줄까지 커서를 내린다.
func find(t *testing.T, m Model, p string) Model {
	t.Helper()
	for i, n := range m.rows {
		if path(n) == p {
			m.cur = i
			m.follow()
			return m
		}
	}
	t.Fatalf("보이는 줄에 %s 없음", p)
	return m
}

func TestNew_키_순서를_원문대로_지킨다(t *testing.T) {
	m := mustNew(t, sample)
	var keys []string
	for _, c := range m.root.children {
		keys = append(keys, c.key)
	}
	if strings.Join(keys, ",") != "zeta,alpha,items,ok,none" {
		t.Fatalf("키 순서: %v", keys)
	}
	dup := mustNew(t, `{"b":1,"a":2,"b":3}`)
	if len(dup.root.children) != 3 || dup.root.children[2].text != "3" {
		t.Fatalf("같은 키도 원문대로 남긴다: %+v", dup.root.children)
	}
}

func TestNew_처음에는_두_단계까지_펼친다(t *testing.T) {
	got := lines(mustNew(t, sample))
	want := []string{
		"▾ $: {5}",
		"    zeta: 1",
		"  ▾ alpha: {2}",
		"    ▸ b: [2]",
		`      a b: "x"`,
		"  ▾ items: [1]",
		"    ▸ [0]: {3}",
		"    ok: true",
		"    none: null",
	}
	if strings.Join(got, "\n") != strings.Join(want, "\n") {
		t.Fatalf("got\n%s\nwant\n%s", strings.Join(got, "\n"), strings.Join(want, "\n"))
	}
}

func TestUpdate_enter_space는_접고_편다(t *testing.T) {
	m := mustNew(t, sample)
	m = find(t, m, "$.items[0]")
	m, _ = press(m, "enter")
	if !strings.Contains(strings.Join(lines(m), "\n"), `name: "kim"`) {
		t.Fatalf("enter 로 편다: %q", lines(m))
	}
	m, _ = press(m, "space")
	if strings.Contains(strings.Join(lines(m), "\n"), "name") {
		t.Fatalf("space 로 접는다: %q", lines(m))
	}
	m, _ = press(m, "g", "enter")
	if got := lines(m); len(got) != 1 || got[0] != "▸ $: {5}" {
		t.Fatalf("루트를 접으면 한 줄: %q", got)
	}
}

func TestUpdate_h_l은_접고_펴고_부모와_자식으로_간다(t *testing.T) {
	m := mustNew(t, sample)
	m = find(t, m, "$.alpha.b")
	m, _ = press(m, "l")
	if !strings.Contains(strings.Join(lines(m), "\n"), "[1]: 20") {
		t.Fatalf("l 로 편다: %q", lines(m))
	}
	m, _ = press(m, "right")
	if p := path(m.rows[m.cur]); p != "$.alpha.b[0]" {
		t.Fatalf("펼친 묶음에서 l 은 첫 자식: %s", p)
	}
	m, _ = press(m, "h")
	if p := path(m.rows[m.cur]); p != "$.alpha.b" {
		t.Fatalf("스칼라에서 h 는 부모: %s", p)
	}
	m, _ = press(m, "left")
	if !m.rows[m.cur].collapsed {
		t.Fatal("펼친 묶음에서 h 는 접는다")
	}
	m, _ = press(m, "h")
	if p := path(m.rows[m.cur]); p != "$.alpha" {
		t.Fatalf("접힌 묶음에서 h 는 부모: %s", p)
	}
}

func TestUpdate_y는_값을_Y는_경로를_복사한다(t *testing.T) {
	m := mustNew(t, sample)
	cases := []struct{ path, value string }{
		{"$", `{"zeta":1,"alpha":{"b":[10,20],"a b":"x"},"items":[{"name":"kim","big":12345678901234567890123,"f":1.10}],"ok":true,"none":null}`},
		{"$.alpha", `{"b":[10,20],"a b":"x"}`},
		{`$.alpha["a b"]`, "x"},
		{"$.items[0]", `{"name":"kim","big":12345678901234567890123,"f":1.10}`},
		{"$.ok", "true"},
		{"$.none", "null"},
	}
	for _, c := range cases {
		m = find(t, m, c.path)
		_, a := press(m, "y")
		if a.Copy == nil || *a.Copy != c.value {
			t.Errorf("%s y = %v, want %q", c.path, a.Copy, c.value)
		}
		_, a = press(m, "Y")
		if a.Copy == nil || *a.Copy != c.path {
			t.Errorf("Y = %v, want %q", a.Copy, c.path)
		}
	}
	m = find(t, m, "$.items[0]")
	m, _ = press(m, "l", "down")
	if _, a := press(m, "Y"); *a.Copy != "$.items[0].name" {
		t.Errorf("깊은 경로: %q", *a.Copy)
	}
}

func TestPath_식별자가_아닌_키는_따옴표로_감싼다(t *testing.T) {
	m := mustNew(t, `{"a b":{"1x":{"한글":{"q\"<>":{"_ok9":0}}}}}`)
	// 맨 끝 줄을 펴고 내려가기를 잎까지 되풀이한다
	for range 5 {
		m, _ = press(m, "G", "l")
	}
	leaf := m.rows[len(m.rows)-1]
	if p := path(leaf); p != `$["a b"]["1x"]["한글"]["q\"<>"]._ok9` {
		t.Fatalf("경로: %s", p)
	}
}

func TestNew_큰_수와_소수는_원문_글자를_지킨다(t *testing.T) {
	m := mustNew(t, `[12345678901234567890123, 1.10, -0.0, 1e400, 9007199254740993]`)
	var got []string
	for _, c := range m.root.children {
		got = append(got, c.text)
	}
	if strings.Join(got, " ") != "12345678901234567890123 1.10 -0.0 1e400 9007199254740993" {
		t.Fatalf("숫자 원문: %v", got)
	}
}

func TestNew_JSON이_아니거나_1MiB를_넘으면_오류다(t *testing.T) {
	for _, raw := range []string{"", "   ", "{", `{"a":}`, "[1,]", "abc", `{"a":1} {"b":2}`, `[1] x`, `{"a" 1}`} {
		if _, err := New(raw); err == nil {
			t.Errorf("%q: 오류가 나야 한다", raw)
		}
	}
	big := `["` + strings.Repeat("a", maxBytes) + `"]`
	if _, err := New(big); err == nil {
		t.Error("1MiB 초과는 오류")
	}
	fits := `"` + strings.Repeat("a", maxBytes-2) + `"`
	if _, err := New(fits); err != nil {
		t.Errorf("딱 1MiB 는 받는다: %v", err)
	}
	if m := mustNew(t, `"abc"`); lines(m)[0] != `  $: "abc"` {
		t.Errorf("스칼라 하나: %q", lines(m))
	}
}

func TestNew_깊게_중첩돼도_재귀_없이_읽는다(t *testing.T) {
	const depth = 100000
	m := mustNew(t, strings.Repeat("[", depth)+strings.Repeat("]", depth))
	if got := lines(m); len(got) != 3 {
		t.Fatalf("두 단계까지만 펼친다: %q", got)
	}
}

func TestView_폭과_높이를_넘지_않고_커서를_따라_스크롤한다(t *testing.T) {
	raw := `{"긴한글키이름입니다정말로아주길어요": "값도 아주 길어서 화면을 넘기는 한글 문자열입니다 정말로요", "제어": "a\u001b[31mb\tc\nd", "arr": [` + strings.Repeat(`1,`, 99) + `1]}`
	m := mustNew(t, raw)
	for w := 1; w <= 40; w++ {
		for h := 1; h <= 8; h++ {
			m.SetSize(w, h)
			out := m.View()
			ls := strings.Split(out, "\n")
			if len(ls) > h {
				t.Fatalf("%dx%d 줄 %d", w, h, len(ls))
			}
			for _, l := range ls {
				if ansi.StringWidth(l) > w {
					t.Fatalf("%dx%d 폭 %d: %q", w, h, ansi.StringWidth(l), l)
				}
			}
		}
	}
	m.SetSize(80, 5)
	if out := m.View(); strings.Contains(out, "\x1b[31m") || strings.Contains(out, "\t") {
		t.Fatalf("제어 문자는 보이는 글자로: %q", out)
	}
	m = find(t, m, "$.arr")
	m, _ = press(m, "pgdown", "pgdown")
	if p := path(m.rows[m.cur]); p != "$.arr[9]" {
		t.Fatalf("pgdown 은 화면 높이만큼: %s", p)
	}
	if got := lines(m); got[len(got)-1] != "      [9]: 1" {
		t.Fatalf("커서 줄이 보여야 한다: %q", got)
	}
	m, _ = press(m, "G")
	if got := lines(m); !strings.Contains(got[len(got)-1], "[99]: 1") {
		t.Fatalf("G 는 끝 줄: %q", got)
	}
	if _, a := press(m, "esc"); !a.Close {
		t.Fatal("esc 는 닫기")
	}
}
