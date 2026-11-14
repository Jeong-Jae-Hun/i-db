package textutil

import "testing"

func TestPrintable_제어문자는_보이는_글자로_줄바꿈은_고를_수_있다(t *testing.T) {
	cases := []struct {
		in   string
		keep bool
		want string
	}{
		{"a\x1b[31mb", false, `a\x1b[31mb`},
		{"a\nb", false, `a\x0ab`},
		{"a\nb", true, "a\nb"},
		{"탭\t끝\x7f\u0085", false, `탭  끝\x7f\x85`},
		{"\xff", false, "�"},
	}
	for _, c := range cases {
		if got := Printable(c.in, c.keep); got != c.want {
			t.Errorf("Printable(%q, %v) = %q, want %q", c.in, c.keep, got, c.want)
		}
	}
}
