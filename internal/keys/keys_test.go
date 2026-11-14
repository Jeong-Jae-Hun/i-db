package keys

import (
	"bytes"
	"io"
	"os"
	"runtime"
	"strings"
	"testing"
	"testing/iotest"
	"time"

	tea "github.com/charmbracelet/bubbletea"
)

type keyRecorder struct {
	want int
	got  []string
}

func (k *keyRecorder) Init() tea.Cmd { return nil }

func (k *keyRecorder) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	key, ok := msg.(tea.KeyMsg)
	if !ok {
		return k, nil
	}
	k.got = append(k.got, key.String())
	if len(k.got) == k.want {
		return k, tea.Quit
	}
	return k, nil
}

func (k *keyRecorder) View() string { return "" }

func readAll(t *testing.T, r io.Reader) string {
	t.Helper()
	out, err := io.ReadAll(newReader(r))
	if err != nil {
		t.Fatalf("read: %v", err)
	}
	return string(out)
}

func TestConvert_kitty_키를_레거시와_단축키로_바꾼다(t *testing.T) {
	cases := []struct {
		name, in, want string
	}{
		// ⌘ 표
		{"⌘R", "\x1b[114;9u", "\x12"},
		{"⌘↩", "\x1b[13;9u", "\x12"},
		{"⌘E", "\x1b[101;9u", "\x05"},
		{"⌥⌘E", "\x1b[101;11u", "\x1be"},
		{"⌘S", "\x1b[115;9u", "\x13"},
		{"⌘O", "\x1b[111;9u", "\x0f"},
		{"⌘/", "\x1b[47;9u", "\x1f"},
		{"⌘Z", "\x1b[122;9u", "\x1a"},
		{"⌘⇧Z", "\x1b[122;10u", "\x19"},
		{"⌘⇧Z 대문자 코드포인트", "\x1b[90;9u", "\x19"},
		{"⌘D", "\x1b[100;9u", "\x04"},
		{"⌘⇧F", "\x1b[102;10u", "\x1b[19~"},
		{"⌘K", "\x1b[107;9u", "\x0b"},
		{"⌘A", "\x1b[97;9u", "\x01"},
		{"⌘B", "\x1b[98;9u", "\x02"},
		{"⌘P", "\x1b[112;9u", "\x10"},
		{"⌘R 캡스락", "\x1b[114;73u", "\x12"},
		{"⌘R 반복", "\x1b[114;9:2u", "\x12"},
		{"⌘R 뗌은 버림", "\x1b[114;9:3u", ""},
		// ⌘ 표 R47.2
		{"⌘T", "\x1b[116;9u", "\x14"},
		{"⌘W", "\x1b[119;9u", "\x17"},
		{"⌘1", "\x1b[49;9u", "\x1b1"},
		{"⌘9", "\x1b[57;9u", "\x1b9"},
		{"⌘N", "\x1b[110;9u", "\x0e"},
		// ⌘ 표 v7
		{"⌘U", "\x1b[117;9u", "\x15"},
		{"⌘L", "\x1b[108;9u", "\x0c"},
		{"⌘⇧C", "\x1b[99;10u", "\x1bc"},
		{"⌘⇧R", "\x1b[114;10u", "\x1br"},
		{"⌘⇧P 대문자 코드포인트", "\x1b[80;9u", "\x1bp"},
		{"⌘H", "\x1b[104;9u", "\x08"},
		{"⌘⇧/", "\x1b[47;10u", "\x1bOP"},
		{"⌘? 바뀐 글자에 shift", "\x1b[63;10u", "\x1bOP"},
		{"⌘? 바뀐 글자만", "\x1b[63;9u", "\x1bOP"},
		{"⌘⇧O", "\x1b[111;10u", "\x1bOR"},
		{"⌘⇧O 대문자 코드포인트", "\x1b[79;9u", "\x1bOR"},
		{"⌘⇧S", "\x1b[115;10u", "\x1bOS"},
		{"⌘⇧D", "\x1b[100;10u", "\x1b[17~"},
		{"⌘⇧E", "\x1b[101;10u", "\x1b[18~"},
		{"⌥⌘L", "\x1b[108;11u", "\x1b[19~"},
		{"⌘⇧M", "\x1b[109;10u", "\x1b[20~"},
		{"⌘⇧↩", "\x1b[13;10u", "\x1b[21~"},
		{"⌘⇧⌫", "\x1b[127;10u", "\x1b[23~"},
		{"⌘⇧S 캡스락", "\x1b[115;74u", "\x1bOS"},
		{"⌘0 은 버림", "\x1b[48;9u", ""},
		{"⌘⌫ 은 버림", "\x1b[127;9u", ""},
		{"⌘⇧X 은 버림", "\x1b[120;10u", ""},
		{"⌘F 는 버림", "\x1b[102;9u", ""},
		{"⌘J 은 버림", "\x1b[106;9u", ""},
		{"⌃⌘R 은 버림", "\x1b[114;13u", ""},
		{"⌘←", "\x1b[1;9D", "\x1b[H"},
		{"⌘→", "\x1b[1;9C", "\x1b[F"},
		{"⌘↑", "\x1b[1;9A", "\x1b[1;5H"},
		{"⌘↓", "\x1b[1;9B", "\x1b[1;5F"},
		{"⌘⇧← 은 버림", "\x1b[1;10D", ""},
		{"⌘Delete 는 버림", "\x1b[3;9~", ""},
		// 레거시 되돌리기
		{"ctrl+r", "\x1b[114;5u", "\x12"},
		{"ctrl+c", "\x1b[99;5u", "\x03"},
		{"ctrl+shift+a", "\x1b[97;6u", "\x01"},
		{"ctrl+대문자", "\x1b[65;5u", "\x01"},
		{"ctrl+space", "\x1b[32;5u", "\x00"},
		{"ctrl+[", "\x1b[91;5u", "\x1b"},
		{"ctrl+/", "\x1b[47;5u", "\x1f"},
		{"ctrl+_", "\x1b[95;5u", "\x1f"},
		{"ctrl+1 은 레거시가 없어 그대로", "\x1b[49;5u", "\x1b[49;5u"},
		{"esc", "\x1b[27u", "\x1b"},
		{"alt+esc", "\x1b[27;3u", "\x1b\x1b"},
		{"alt+x", "\x1b[120;3u", "\x1bx"},
		{"alt+shift+x", "\x1b[120;4u", "\x1bX"},
		{"ctrl+alt+r", "\x1b[114;7u", "\x1b\x12"},
		{"alt+[", "\x1b[91;3u", "\x1b["},
		{"alt+한글", "\x1b[54620;3u", "\x1b한"},
		{"enter", "\x1b[13u", "\r"},
		{"shift+enter", "\x1b[13;2u", "\r"},
		{"alt+enter", "\x1b[13;3u", "\x1b\r"},
		{"키패드 enter", "\x1b[57414u", "\r"},
		{"tab", "\x1b[9u", "\t"},
		{"shift+tab", "\x1b[9;2u", "\x1b[Z"},
		{"ctrl+tab", "\x1b[9;5u", "\t"},
		{"backspace", "\x1b[127u", "\x7f"},
		{"alt+backspace", "\x1b[127;3u", "\x1b\x7f"},
		{"ctrl+backspace", "\x1b[127;5u", "\x08"},
		{"alt+shift+1 은 자판을 몰라 그대로", "\x1b[49;4u", "\x1b[49;4u"},
		{"키패드 PUA 는 그대로", "\x1b[57399u", "\x1b[57399u"},
		{"hyper 는 그대로", "\x1b[114;17u", "\x1b[114;17u"},
		{"대체 키 필드는 무시", "\x1b[97:65;6u", "\x01"},
		// 수정자 붙은 특수 키
		{"ctrl+up 은 그대로", "\x1b[1;5A", "\x1b[1;5A"},
		{"shift+delete 는 그대로", "\x1b[3;2~", "\x1b[3;2~"},
		{"넘버락 비트만 지운다", "\x1b[1;133A", "\x1b[1;5A"},
		{"넘버락만 있으면 수정자 없는 꼴", "\x1b[1;129H", "\x1b[H"},
		{"~ 넘버락", "\x1b[3;129~", "\x1b[3~"},
		// 모르는 것
		{"수정자 없는 방향키", "\x1b[A", "\x1b[A"},
		{"SGR 마우스", "\x1b[<0;10;20M", "\x1b[<0;10;20M"},
		{"붙여넣기 시작", "\x1b[200~", "\x1b[200~"},
		{"커서 보고", "\x1b[12;40R", "\x1b[12;40R"},
		{"큰 숫자", "\x1b[99999999999999999999;9u", "\x1b[99999999999999999999;9u"},
		{"범위 밖 코드포인트", "\x1b[1114112u", "\x1b[1114112u"},
		{"수정자 0", "\x1b[114;0u", "\x1b[114;0u"},
		{"빈 코드포인트", "\x1b[;5u", "\x1b[;5u"},
		{"제어 코드포인트", "\x1b[1u", "\x1b[1u"},
	}
	for _, c := range cases {
		if got := string(convert(nil, []byte(c.in))); got != c.want {
			t.Errorf("%s: convert(%q) = %q, want %q", c.name, c.in, got, c.want)
		}
	}
}

func TestScanCSI_완결_덜옴_아님을_가른다(t *testing.T) {
	cases := []struct {
		in   string
		want int
	}{
		{"\x1b", 0},
		{"\x1b[", 0},
		{"\x1b[114;9", 0},
		{"\x1bO", 0},
		{"\x1b[114;9u", 8},
		{"\x1b[Arest", 3},
		{"\x1b[<0;1;1M", 9},
		{"\x1bx", -1},
		{"\x1bOP", -1},
		{"\x1b[1\x07", -1},
		{"\x1b[1\xff", -1},
		{"\x1b[" + strings.Repeat("1", maxSeq), -1},
	}
	for _, c := range cases {
		if got := scanCSI([]byte(c.in)); got != c.want {
			t.Errorf("scanCSI(%q) = %d, want %d", c.in, got, c.want)
		}
	}
}

func TestReader_스트림에서_시퀀스를_찾아_바꾸고_나머지는_그대로_둔다(t *testing.T) {
	cases := []struct {
		name, in, want string
	}{
		{"섞인 입력", "ab\x1b[114;9ucd\x1b[1;9De", "ab\x12cd\x1b[He"},
		{"레거시 입력", "\x1bx\x12\x1b[A\x1bOP한글", "\x1bx\x12\x1b[A\x1bOP한글"},
		{"끝의 ESC 는 EOF 에 내보낸다", "a\x1b", "a\x1b"},
		{"끝의 덜 온 CSI 도 EOF 에 그대로", "a\x1b[114;9", "a\x1b[114;9"},
		{"끝의 ESC O", "\x1bO", "\x1bO"},
		{"CSI 안의 깨진 바이트", "\x1b[11\x07u", "\x1b[11\x07u"},
		{"CSI 안의 ESC", "\x1b[11\x1b[27u", "\x1b[11\x1b"},
		{"너무 긴 CSI", "\x1b[" + strings.Repeat("1", 80) + "u", "\x1b[" + strings.Repeat("1", 80) + "u"},
		{"깨진 UTF-8", "\xff\xfe\x1b[27u", "\xff\xfe\x1b"},
		{
			"붙여넣기 본문은 바꾸지 않는다",
			"\x1b[200~x\x1b[114;9u\x1b[20\x1b[27u\x1b[201~\x1b[27u",
			"\x1b[200~x\x1b[114;9u\x1b[20\x1b[27u\x1b[201~\x1b",
		},
	}
	for _, c := range cases {
		if got := readAll(t, strings.NewReader(c.in)); got != c.want {
			t.Errorf("%s: whole = %q, want %q", c.name, got, c.want)
		}
		if got := readAll(t, iotest.OneByteReader(strings.NewReader(c.in))); got != c.want {
			t.Errorf("%s: one byte = %q, want %q", c.name, got, c.want)
		}
		if got := readAll(t, iotest.DataErrReader(iotest.HalfReader(strings.NewReader(c.in)))); got != c.want {
			t.Errorf("%s: half = %q, want %q", c.name, got, c.want)
		}
	}
}

func TestReader_작은_버퍼로_읽어도_잃지_않는다(t *testing.T) {
	r := newReader(iotest.OneByteReader(strings.NewReader("a\x1b[114;9ub\x1b[200~\x1b[27u\x1b[201~")))
	var got []byte
	p := make([]byte, 3)
	for {
		n, err := r.Read(p)
		got = append(got, p[:n]...)
		if err == io.EOF {
			break
		}
		if err != nil {
			t.Fatalf("read: %v", err)
		}
	}
	if want := "a\x12b\x1b[200~\x1b[27u\x1b[201~"; string(got) != want {
		t.Fatalf("got %q, want %q", got, want)
	}
}

func TestReader_원본_오류는_남은_출력_뒤에_전한다(t *testing.T) {
	r := newReader(iotest.TimeoutReader(iotest.OneByteReader(strings.NewReader("a"))))
	p := make([]byte, 8)
	n, err := r.Read(p)
	if n != 1 || err != nil || p[0] != 'a' {
		t.Fatalf("first read = %d %v", n, err)
	}
	if _, err := r.Read(p); err != iotest.ErrTimeout {
		t.Fatalf("err = %v, want ErrTimeout", err)
	}
}

func TestTTY_잘린_꼬리를_기다리되_막히지_않는다(t *testing.T) {
	pr, pw, err := os.Pipe()
	if err != nil {
		t.Fatal(err)
	}
	defer pr.Close()
	defer pw.Close()
	r := NewTTY(pr)
	p := make([]byte, 256)

	// 혼자 누른 ESC 는 escWait 뒤에 그대로 나간다
	if _, err := pw.WriteString("\x1b"); err != nil {
		t.Fatal(err)
	}
	start := time.Now()
	n, err := r.Read(p)
	if err != nil || string(p[:n]) != "\x1b" {
		t.Fatalf("ESC = %q %v", p[:n], err)
	}
	if d := time.Since(start); d > time.Second {
		t.Fatalf("lone ESC blocked for %v", d)
	}

	// 버린 ⌘ 조합만 있으면 다음 키를 기다리지 않고 0 바이트로 돌아온다
	if _, err := pw.WriteString("\x1b[106;9u"); err != nil {
		t.Fatal(err)
	}
	if n, err := r.Read(p); n != 0 || err != nil {
		t.Fatalf("dropped key = %d %v", n, err)
	}

	// escWait 안에 나머지가 오면 이어 붙여 번역한다
	if _, err := pw.WriteString("x\x1b[11"); err != nil {
		t.Fatal(err)
	}
	done := make(chan struct{})
	go func() {
		defer close(done)
		time.Sleep(escWait / 5)
		if _, err := pw.WriteString("4;9u"); err != nil {
			t.Error(err)
		}
	}()
	n, err = r.Read(p)
	<-done
	if err != nil || string(p[:n]) != "x\x12" {
		t.Fatalf("split super+r = %q %v", p[:n], err)
	}
}

func TestWriter_대체_화면에_맞춰_kitty_플래그를_넣고_뺀다(t *testing.T) {
	pr, pw, err := os.Pipe()
	if err != nil {
		t.Fatal(err)
	}
	defer pr.Close()
	w := NewWriter(pw)
	writes := []string{"plain", "\x1b[?1049h\x1b[2J", "\x1b[?1049l\x1b[?25h"}
	for _, s := range writes {
		n, err := w.Write([]byte(s))
		if err != nil || n != len(s) {
			t.Fatalf("Write(%q) = %d %v", s, n, err)
		}
	}
	if err := pw.Close(); err != nil {
		t.Fatal(err)
	}
	got, err := io.ReadAll(pr)
	if err != nil {
		t.Fatal(err)
	}
	if want := "plain\x1b[?1049h\x1b[>1u\x1b[2J\x1b[<u\x1b[?1049l\x1b[?25h"; string(got) != want {
		t.Fatalf("got %q, want %q", got, want)
	}
}

func TestReader_bubbletea_가_바꾼_바이트를_원래_키로_읽는다(t *testing.T) {
	// esc 는 바로 뒤 바이트와 붙으면 alt 로 읽혀 맨 끝에 둔다
	in := "\x1b[114;9u\x1b[101;11u\x1b[1;9D\x1b[122;10u\x1b[102;10u\x1b[98;9u" +
		"\x1b[116;9u\x1b[119;9u\x1b[51;9u\x1b[110;9u\x1b[104;9u\x1b[47;10u\x1b[111;10u\x1b[115;10u" +
		"\x1b[100;10u\x1b[101;10u\x1b[108;11u\x1b[109;10u\x1b[13;10u\x1b[127;10u\x1b[99;5u\x1b[27u"
	want := []string{"ctrl+r", "alt+e", "home", "ctrl+y", "f8", "ctrl+b",
		"ctrl+t", "ctrl+w", "alt+3", "ctrl+n", "ctrl+h", "f1", "f3", "f4",
		"f6", "f7", "f8", "f9", "f10", "f11", "ctrl+c", "esc"}
	srcs := []io.Reader{strings.NewReader(in), iotest.OneByteReader(strings.NewReader(in))}
	for _, src := range srcs {
		m := &keyRecorder{want: len(want)}
		_, err := tea.NewProgram(m,
			tea.WithInput(newReader(src)),
			tea.WithOutput(io.Discard),
			tea.WithoutRenderer(),
			tea.WithoutSignalHandler(),
		).Run()
		if err != nil {
			t.Fatalf("run: %v", err)
		}
		if strings.Join(m.got, " ") != strings.Join(want, " ") {
			t.Fatalf("got %v, want %v", m.got, want)
		}
	}
}

func FuzzReader(f *testing.F) {
	f.Add([]byte("\x1b[114;9u"), []byte("x"), uint8(1))
	f.Add([]byte("a\x1b[1;9D\x1b"), []byte("\x1b[27u\x1b[20"), uint8(3))
	f.Add([]byte("\x1b[<0;1;1M\x1b[200~"), []byte("\x1b[201"), uint8(7))
	f.Fuzz(func(t *testing.T, data, paste []byte, chunk uint8) {
		size := int(chunk%16) + 1
		out, err := io.ReadAll(newReader(iotest.HalfReader(bytes.NewReader(data))))
		if err != nil {
			t.Fatal(err)
		}
		if len(out) > len(data) {
			t.Fatalf("output %d bytes longer than input %d bytes", len(out), len(data))
		}
		// 붙여넣기 본문은 앞 입력과 상관없이 한 바이트도 바뀌지 않는다
		body := paste
		for bytes.Contains(body, pasteEnd) {
			body = bytes.ReplaceAll(body, pasteEnd, nil)
		}
		in := append(append(append([]byte("\x1b[200~"), body...), pasteEnd...), '.')
		r := newReader(iotest.OneByteReader(bytes.NewReader(in)))
		got := make([]byte, 0, len(in))
		p := make([]byte, size)
		for {
			n, err := r.Read(p)
			got = append(got, p[:n]...)
			if err == io.EOF {
				break
			}
			if err != nil {
				t.Fatal(err)
			}
		}
		if !bytes.Equal(got, in) {
			t.Fatalf("paste changed: %q -> %q", in, got)
		}
	})
}

func TestMacStyle_DB_KEYS_가_먼저고_없으면_맥이나_kitty(t *testing.T) {
	t.Setenv("DB_KEYS", "cmd")
	if !MacStyle(false) {
		t.Fatal("DB_KEYS=cmd 는 ⌘ 안내")
	}
	t.Setenv("DB_KEYS", "ctrl")
	if MacStyle(true) {
		t.Fatal("DB_KEYS=ctrl 은 kitty 가 켜져도 Ctrl 안내")
	}
	t.Setenv("DB_KEYS", "")
	if !MacStyle(true) {
		t.Fatal("kitty 프로토콜이 켜졌으면 ⌘ 안내")
	}
	if got, want := MacStyle(false), runtime.GOOS == "darwin"; got != want {
		t.Fatalf("kitty 가 꺼지면 macOS 만 ⌘ 안내: got %v, want %v", got, want)
	}
}

func TestLabel_키_안내를_맥_글자로_바꾼다(t *testing.T) {
	cases := []struct{ in, want string }{
		{"Ctrl+R 실행 · Ctrl+S 저장", "⌘R 실행 · ⌘S 저장"},
		{"Ctrl+R / F5", "⌘R / ⌘R"},
		{"Ctrl+B / F12", "⌘B / ⌘B"},
		{"Alt+E", "⌥⌘E"},
		{"Alt+1..9", "⌘1..9"},
		{"Alt+1:", "⌘1:"},
		{"Ctrl+Z / Y", "⌘Z / Y"},
		{"Ctrl+Y", "⌘⇧Z"},
		{"Ctrl+/", "⌘/"},
		{"Ctrl+H · Ctrl+N · Ctrl+T · Ctrl+W", "⌘H · ⌘N · ⌘T · ⌘W"},
		{"F1 도움말", "⌘? 도움말"},
		{"F3/F4 즐겨찾기", "⌘⇧O/⌘⇧S 즐겨찾기"},
		{"F6 · F7 · F8 · F9", "⌘⇧D · ⌘⇧E · ⌥⌘L · ⌘⇧M"},
		{"F10 / F11", "⌘⇧↩ / ⌘⇧⌫"},
		{"Ctrl+U · Ctrl+L · Alt+C · Alt+R / Alt+P", "⌘U · ⌘L · ⌘⇧C · ⌘⇧R / ⌘⇧P"},
		{"F2 고치기", "F2 고치기"},
		{"Ctrl+S:", "⌘S:"},
		{"(Ctrl+S)하거나", "(⌘S)하거나"},
		{"Ctrl+R로 실행", "⌘R로 실행"},
		// ⌘ 표에 없으면 수정자 기호만
		{"Ctrl+←→↑↓", "⌃←→↑↓"},
		{"Ctrl+← / →", "⌃← / →"},
		{"Ctrl+Space 자동완성", "⌃Space 자동완성"},
		{"Ctrl+C 취소", "⌃C 취소"},
		{"Ctrl+PgUp / PgDn", "⌃PgUp / PgDn"},
		{"Shift+Tab", "⇧Tab"},
		{"Shift+방향키", "⇧방향키"},
		{"Ctrl+방향키는", "⌃방향키는"},
		{"Alt+X", "⌥X"},
		// 키가 아닌 글자
		{"UTF8 · F/R 외래 키 · F 검색 · F13", "UTF8 · F/R 외래 키 · F 검색 · F13"},
		{"xF5 · F55", "xF5 · F55"},
		{"", ""},
	}
	for _, c := range cases {
		if got := Label(c.in, true); got != c.want {
			t.Errorf("Label(%q) = %q, want %q", c.in, got, c.want)
		}
		if got := Label(c.in, false); got != c.in {
			t.Errorf("mac 이 아니면 그대로: Label(%q) = %q", c.in, got)
		}
	}
}

func TestBindings_기존_키_글자마다_맥_글자가_있고_별칭만_글자가_없다(t *testing.T) {
	for _, b := range bindings {
		for _, l := range b.labels {
			if macLabels[l] == "" {
				t.Errorf("%s: 안내 글자가 없음", l)
			}
		}
		if (b.mac == "") != (len(b.labels) == 0) {
			t.Errorf("%q: 별칭이 아니면 mac 글자가 있어야 함", b.mac)
		}
	}
}

func TestTTY_escWait_보다_늦게_온_마우스_보고도_쪼개지_않는다(t *testing.T) {
	pr, pw, err := os.Pipe()
	if err != nil {
		t.Fatal(err)
	}
	defer pr.Close()
	defer pw.Close()
	r := NewTTY(pr)
	p := make([]byte, 256)

	// ConPTY·느린 터미널은 SGR 마우스 보고를 나눠 보낸다. 앞 조각만 내보내면 나머지가 글자로 들어갔다
	if _, err := pw.WriteString("\x1b[<64;84"); err != nil {
		t.Fatal(err)
	}
	if n, err := r.Read(p); n != 0 || err != nil {
		t.Fatalf("덜 온 CSI 는 들고 있는다: %q %v", p[:n], err)
	}
	time.Sleep(escWait * 2)
	if _, err := pw.WriteString(";23M"); err != nil {
		t.Fatal(err)
	}
	n, err := r.Read(p)
	if err != nil || string(p[:n]) != "\x1b[<64;84;23M" {
		t.Fatalf("이어 붙인 보고 = %q %v", p[:n], err)
	}
}
