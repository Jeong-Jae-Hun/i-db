package redisview

import (
	"strings"
	"testing"
	"time"

	"github.com/charmbracelet/x/ansi"
)

// checkBounds 는 View 가 크기 안에 들어오는지 본다.
func checkBounds(t *testing.T, m Model) {
	t.Helper()
	out := m.View()
	if out == "" {
		return
	}
	lines := strings.Split(out, "\n")
	if len(lines) > m.h {
		t.Fatalf("%dx%d 에 %d 줄", m.w, m.h, len(lines))
	}
	for _, l := range lines {
		if w := ansi.StringWidth(l); w > m.w {
			t.Fatalf("%dx%d 에 폭 %d 줄: %q", m.w, m.h, w, l)
		}
	}
}

func TestView_타입마다_머리줄과_값을_그리고_제어문자는_보이게_한다(t *testing.T) {
	v := sample("hash")
	v.Key = "키\x1b]52;c;x\x07"
	v.TTL = 90 * time.Second
	v.Fields = [][2]string{{"이름", "김\n철수"}, {"evil", "\x1b[2J"}}
	out := NewEditor(v).SetSize(50, 8).View()
	if strings.ContainsRune(out, 0x1b) || strings.ContainsRune(out, 0x07) {
		t.Fatalf("제어 문자가 그대로다: %q", out)
	}
	plain := ansi.Strip(out)
	for _, want := range []string{`키\x1b]52`, "hash", "TTL 1m30s", "2개", "이름", "김↵철수", `\x1b[2J`, "e 편집"} {
		if !strings.Contains(plain, want) {
			t.Fatalf("%q 가 없다:\n%s", want, plain)
		}
	}
	for typ, want := range map[string]string{"string": "hello", "list": "2 │ x", "set": "m2", "zset": "1.5 │ z1", "stream": "편집 미지원"} {
		if out := NewEditor(sample(typ)).SetSize(40, 8).View(); !strings.Contains(out, want) {
			t.Fatalf("%s 에 %q 가 없다:\n%s", typ, want, out)
		}
	}
	empty := sample("set")
	empty.Members = nil
	if out := NewEditor(empty).SetSize(40, 5).View(); !strings.Contains(out, "비어 있음") {
		t.Fatalf("빈 set:\n%s", out)
	}
}

func TestView_한글_폭과_아주_작은_크기에서도_패닉없이_크기를_지킨다(t *testing.T) {
	long := sample("hash")
	long.Fields = [][2]string{{strings.Repeat("한글필드", 30), strings.Repeat("값", 500)}}
	multi := sample("string")
	multi.String = strings.Repeat("줄\n", 100)
	for _, v := range []Value{sample("string"), long, multi, sample("zset"), sample("none"), {}} {
		for _, size := range [][2]int{{0, 0}, {1, 1}, {2, 1}, {1, 3}, {5, 2}, {80, 24}} {
			m := NewEditor(v).SetSize(size[0], size[1])
			checkBounds(t, m)
			for _, k := range []string{"G", "e", "가나다라마바사", "left"} {
				m, _ = press(m, k)
				checkBounds(t, m)
			}
		}
	}
}

func TestView_큰_값_입력은_커서_주변만_그려_빨리_그린다(t *testing.T) {
	if raceOn {
		t.Skip("속도 측정은 -race 빌드에서 의미가 없다")
	}
	v := sample("string")
	v.String = strings.Repeat("가", MaxStringEdit/3)
	m, _ := press(NewEditor(v).SetSize(60, 6), "e")
	start := time.Now()
	for range 50 {
		m, _ = press(m, "나")
		checkBounds(t, m)
	}
	if el := time.Since(start); el > 500*time.Millisecond {
		t.Fatalf("너무 느리다: %v", el)
	}
	if foot := ansi.Strip(m.View()); !strings.Contains(foot, "나나▏") || !strings.Contains(foot, "…") {
		t.Fatalf("입력 줄:\n%s", foot)
	}
}
