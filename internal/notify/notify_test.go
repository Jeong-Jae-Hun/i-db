package notify

import (
	"bytes"
	"testing"
	"time"
)

func TestDone_OSC9_바이트는_접속_시간_행수만_싣는다(t *testing.T) {
	var b bytes.Buffer
	if err := Done(&b, "운영 DB", 12345*time.Millisecond, 1001, false); err != nil {
		t.Fatal(err)
	}
	if want := "\x1b]9;db: 운영 DB 끝 12.3s · 1,001행\a"; b.String() != want {
		t.Fatalf("got %q, want %q", b.String(), want)
	}
	b.Reset()
	if err := Done(&b, "pg", 10*time.Second, 5, true); err != nil {
		t.Fatal(err)
	}
	if want := "\x1b]9;db: pg 실패 10.0s\a"; b.String() != want {
		t.Fatalf("실패: got %q, want %q", b.String(), want)
	}
}

func TestDone_접속_이름의_ESC_BEL_은_알림을_끊지_못한다(t *testing.T) {
	var b bytes.Buffer
	if err := Done(&b, "x\a\x1b]52;c;aGk=\x1b\\\u009c", time.Second, 0, false); err != nil {
		t.Fatal(err)
	}
	out := b.Bytes()
	// 여는 ESC 하나와 닫는 BEL 하나만 있어야 한다
	if bytes.Count(out, []byte{0x1b}) != 1 || bytes.Count(out, []byte{0x07}) != 1 || !bytes.HasSuffix(out, []byte{0x07}) {
		t.Fatalf("제어 문자가 남았다: %q", out)
	}
	if bytes.Contains(out, []byte("\u009c")) {
		t.Fatalf("C1 ST 가 남았다: %q", out)
	}
}

func TestThreshold_DB_NOTIFY_SECS_를_읽고_잘못되거나_24시간_넘는_값은_기본값이다(t *testing.T) {
	cases := []struct {
		env  string
		want time.Duration
	}{
		{"30", 30 * time.Second},
		{"0", 0},
		{"abc", 10 * time.Second},
		{"-5", 10 * time.Second},
		{"", 10 * time.Second},
		{"1.5", 10 * time.Second},
		{"86400", 24 * time.Hour},
		// 24시간을 넘으면 잘못된 값이다. 큰 수를 time.Second 에 곱하면 넘쳐 음수·엉뚱한 값이 된다
		{"86401", 10 * time.Second},
		{"9999999999999", 10 * time.Second},
	}
	for _, c := range cases {
		t.Setenv("DB_NOTIFY_SECS", c.env)
		if got := threshold(); got != c.want {
			t.Errorf("DB_NOTIFY_SECS=%q: got %v, want %v", c.env, got, c.want)
		}
	}
}

func TestShouldNotify_기준_이상이고_취소가_아니고_터미널일_때만(t *testing.T) {
	t.Setenv("DB_NOTIFY_SECS", "10")
	cases := []struct {
		elapsed       time.Duration
		canceled, tty bool
		want          bool
	}{
		{10 * time.Second, false, true, true},
		{9999 * time.Millisecond, false, true, false},
		{time.Minute, true, true, false},
		{time.Minute, false, false, false},
	}
	for _, c := range cases {
		if got := ShouldNotify(c.elapsed, c.canceled, c.tty); got != c.want {
			t.Errorf("ShouldNotify(%v, %v, %v) = %v", c.elapsed, c.canceled, c.tty, got)
		}
	}
	t.Setenv("DB_NOTIFY_SECS", "0")
	if ShouldNotify(time.Hour, false, true) {
		t.Fatal("0 이면 끈다")
	}
}
