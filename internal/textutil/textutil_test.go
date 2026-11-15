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

func TestKoreanCount_만_억_조_단위로_줄이고_경계는_버린다(t *testing.T) {
	cases := []struct {
		in   int64
		want string
	}{
		{0, "0"},
		{37, "37"},
		{9999, "9,999"},
		{10000, "1만"},
		{12000, "1.2만"},
		{99999, "9.9만"},
		{100000, "10만"},
		{1200000, "120만"},
		{99999999, "9,999만"},
		{100000000, "1억"},
		{150000000, "1.5억"},
		{1234567890123, "1.2조"},
		{-12000, "-1.2만"},
	}
	for _, c := range cases {
		if got := KoreanCount(c.in); got != c.want {
			t.Errorf("KoreanCount(%d) = %q, want %q", c.in, got, c.want)
		}
	}
}
