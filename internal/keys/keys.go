// Package keys 는 kitty 키보드 프로토콜 입력을 bubbletea v1 이 아는 레거시 바이트로 바꾼다.
//
// 맥 터미널(Ghostty·iTerm2·kitty·WezTerm)은 앱이 kitty 프로토콜을 켜야 ⌘ 조합을 넘겨 준다.
// bubbletea v1 은 이 프로토콜을 모르므로, 출력 쪽 Writer 가 대체 화면에 들어갈 때 프로토콜을 켜고
// 입력 쪽 Reader 가 CSI u 시퀀스를 레거시 키로 되돌린다. ⌘ 조합은 같은 동작의 기존 단축키로 옮긴다.
package keys

import (
	"bytes"
	"io"
	"os"
	"runtime"
	"strconv"
	"strings"
	"time"
	"unicode"
	"unicode/utf8"
)

// kitty 수정자 비트 (수정자 값 - 1). 잠금 비트는 키 의미와 무관해 지운다
const (
	modShift = 1
	modAlt   = 2
	modCtrl  = 4
	modSuper = 8
	modLocks = 64 | 128
)

const (
	esc = 0x1b
	// kpEnter 는 키패드 Enter 의 kitty 전용 코드포인트다. 일반 Enter 와 같게 다룬다
	kpEnter = 57414
	// maxSeq 보다 긴 CSI 는 키가 아니라고 보고 그대로 흘린다. 끝없이 쌓이지 않게 하는 상한이다
	maxSeq = 64
	// escWait 는 tty 에서 잘린 ESC 시퀀스의 나머지를 기다리는 시간이다. 넘으면 있는 그대로 내보낸다
	escWait = 25 * time.Millisecond
)

var (
	// 대체 화면과 kitty 키 스택은 화면마다 따로라서 대체 화면에 들어간 뒤 켜고 나오기 전에 끈다
	altOn  = []byte("\x1b[?1049h")
	altOff = []byte("\x1b[?1049l")
	// 1 = disambiguate escape codes. 지원하지 않는 터미널은 무시한다
	pushFlags = []byte("\x1b[>1u")
	popFlags  = []byte("\x1b[<u")

	pasteStart = []byte("\x1b[200~")
	pasteEnd   = []byte("\x1b[201~")

	// 수정자 붙은 특수 키(CSI 1;mods X)의 끝 글자. 방향키·Home·End·F1~F4 다
	letterFinals = []byte("ABCDEFHPQS")

	ctrlBytes = map[rune]byte{
		' ': 0x00, '@': 0x00, '2': 0x00,
		'[': 0x1b, '3': 0x1b,
		'\\': 0x1c, '4': 0x1c,
		']': 0x1d, '5': 0x1d,
		'^': 0x1e, '6': 0x1e,
		'_': 0x1f, '/': 0x1f, '7': 0x1f,
		'?': 0x7f, '8': 0x7f,
	}

	// superKeys 는 ⌘ 조합을 db 가 이미 아는 단축키 바이트로, macLabels 는 키 안내 글자(Ctrl+R)를 ⌘ 글자로 옮긴다.
	// 둘 다 bindings 한 표에서 만들어 받는 키와 보이는 키가 어긋나지 않는다
	superKeys, macLabels = buildTables()

	// superArrows 는 ⌘ 방향키를 줄 처음·끝(←→), 문서 처음·끝(↑↓)으로 옮긴다
	superArrows = map[byte]string{
		'D': "\x1b[H",
		'C': "\x1b[F",
		'A': "\x1b[1;5H",
		'B': "\x1b[1;5F",
	}
)

type superKey struct {
	cp   rune
	mods int
}

// binding 은 ⌘ 조합 하나다. labels 는 같은 동작의 기존 키 안내 글자이고 mac 은 맥에서 대신 보일 글자다.
// labels 가 비면 받기만 하는 별칭이다(⌘↩ 는 ⌘R 과 같다). 같은 label 이 여럿이면 앞 줄의 mac 을 보인다.
type binding struct {
	key    superKey
	seq    string
	labels []string
	mac    string
}

// F 키 레거시 바이트(bubbletea v1 이 아는 xterm 꼴)
const (
	f1  = "\x1bOP"
	f3  = "\x1bOR"
	f4  = "\x1bOS"
	f6  = "\x1b[17~"
	f7  = "\x1b[18~"
	f8  = "\x1b[19~"
	f9  = "\x1b[20~"
	f10 = "\x1b[21~"
	f11 = "\x1b[23~"
)

// bindings 는 표에 없는 ⌘ 조합을 버린다. F 키는 맥에서 fn 을 같이 눌러야 해서 ⌘ 대안을 둔다(R47.2).
// kitty 는 ⇧ 를 붙여도 기본 키 코드(/ · o)에 shift 수정자를 싣지만, 바뀐 글자(? · O)를 보내는 터미널도 있어
// 대문자는 convert 가 소문자+shift 로 고치고 ? 는 별칭으로 받는다.
var bindings = []binding{
	{superKey{'r', 0}, "\x12", []string{"Ctrl+R", "F5"}, "⌘R"},
	{superKey{'\r', 0}, "\x12", nil, ""},
	{superKey{'e', 0}, "\x05", []string{"Ctrl+E"}, "⌘E"},
	{superKey{'e', modAlt}, "\x1be", []string{"Alt+E"}, "⌥⌘E"},
	{superKey{'s', 0}, "\x13", []string{"Ctrl+S"}, "⌘S"},
	{superKey{'o', 0}, "\x0f", []string{"Ctrl+O"}, "⌘O"},
	{superKey{'/', 0}, "\x1f", []string{"Ctrl+/"}, "⌘/"},
	{superKey{'z', 0}, "\x1a", []string{"Ctrl+Z"}, "⌘Z"},
	{superKey{'z', modShift}, "\x19", []string{"Ctrl+Y"}, "⌘⇧Z"},
	{superKey{'d', 0}, "\x04", []string{"Ctrl+D"}, "⌘D"},
	{superKey{'k', 0}, "\x0b", []string{"Ctrl+K"}, "⌘K"},
	{superKey{'a', 0}, "\x01", []string{"Ctrl+A"}, "⌘A"},
	{superKey{'b', 0}, "\x02", []string{"Ctrl+B", "F12"}, "⌘B"},
	{superKey{'p', 0}, "\x10", []string{"Ctrl+P"}, "⌘P"},
	{superKey{'t', 0}, "\x14", []string{"Ctrl+T"}, "⌘T"},
	{superKey{'w', 0}, "\x17", []string{"Ctrl+W"}, "⌘W"},
	{superKey{'n', 0}, "\x0e", []string{"Ctrl+N"}, "⌘N"},
	{superKey{'h', 0}, "\x08", []string{"Ctrl+H"}, "⌘H"},
	{superKey{'1', 0}, "\x1b1", []string{"Alt+1"}, "⌘1"},
	{superKey{'2', 0}, "\x1b2", []string{"Alt+2"}, "⌘2"},
	{superKey{'3', 0}, "\x1b3", []string{"Alt+3"}, "⌘3"},
	{superKey{'4', 0}, "\x1b4", []string{"Alt+4"}, "⌘4"},
	{superKey{'5', 0}, "\x1b5", []string{"Alt+5"}, "⌘5"},
	{superKey{'6', 0}, "\x1b6", []string{"Alt+6"}, "⌘6"},
	{superKey{'7', 0}, "\x1b7", []string{"Alt+7"}, "⌘7"},
	{superKey{'8', 0}, "\x1b8", []string{"Alt+8"}, "⌘8"},
	{superKey{'9', 0}, "\x1b9", []string{"Alt+9"}, "⌘9"},
	{superKey{'/', modShift}, f1, []string{"F1"}, "⌘?"},
	{superKey{'?', modShift}, f1, nil, ""},
	{superKey{'?', 0}, f1, nil, ""},
	{superKey{'o', modShift}, f3, []string{"F3"}, "⌘⇧O"},
	{superKey{'s', modShift}, f4, []string{"F4"}, "⌘⇧S"},
	{superKey{'d', modShift}, f6, []string{"F6"}, "⌘⇧D"},
	{superKey{'e', modShift}, f7, []string{"F7"}, "⌘⇧E"},
	{superKey{'l', modAlt}, f8, []string{"F8"}, "⌥⌘L"},
	{superKey{'f', modShift}, f8, nil, ""},
	{superKey{'m', modShift}, f9, []string{"F9"}, "⌘⇧M"},
	{superKey{'\r', modShift}, f10, []string{"F10"}, "⌘⇧↩"},
	{superKey{0x7f, modShift}, f11, []string{"F11"}, "⌘⇧⌫"},
	// v7(R50·R51·R53·R54·R57). ⌘V 는 터미널이 붙여넣기로 보내 표에 두지 않는다
	{superKey{'u', 0}, "\x15", []string{"Ctrl+U"}, "⌘U"},
	{superKey{'l', 0}, "\x0c", []string{"Ctrl+L"}, "⌘L"},
	{superKey{'c', modShift}, "\x1bc", []string{"Alt+C"}, "⌘⇧C"},
	{superKey{'r', modShift}, "\x1br", []string{"Alt+R"}, "⌘⇧R"},
	{superKey{'p', modShift}, "\x1bp", []string{"Alt+P"}, "⌘⇧P"},
	// R64 대시보드, R70 보드
	{superKey{'g', 0}, "\x07", []string{"Ctrl+G"}, "⌘G"},
	{superKey{'b', modShift}, "\x1bb", []string{"Alt+B"}, "⌘⇧B"},
}

func buildTables() (map[superKey]string, map[string]string) {
	seqs := map[superKey]string{}
	labels := map[string]string{}
	for _, b := range bindings {
		seqs[b.key] = b.seq
		for _, l := range b.labels {
			if _, ok := labels[l]; !ok {
				labels[l] = b.mac
			}
		}
	}
	return seqs, labels
}

// Reader 는 입력 바이트에서 kitty 키 시퀀스를 찾아 레거시 바이트로 바꾼다.
// 잘린 시퀀스는 다음 읽기까지 들고 있다가 이어 붙인다. tty 가 아닌 원본은 더 올 때까지 막혀 기다린다.
type Reader struct {
	src io.Reader
	// tty 가 있으면 읽기 한 번 안에서 잘린 꼬리를 정리한다. bubbletea 의 cancelreader 는
	// fd 가 읽을 거리가 있을 때만 Read 를 부르므로, 꼬리를 다음 호출로 미루면 다음 키까지 멈춘다
	tty        *os.File
	buf        []byte
	in         []byte // 아직 번역하지 않은 입력. 늘 불완전한 ESC 시퀀스 하나뿐이다
	out        []byte
	err        error
	paste      bool
	pasteMatch int
}

// TTY 는 터미널 입력을 감싼다. Fd·Name 을 내줘서 bubbletea 가 원래 터미널에 raw 모드를 걸고
// cancelreader 가 fd 를 감시해 읽기를 취소할 수 있게 한다.
type TTY struct {
	*Reader
	f *os.File
}

// Writer 는 터미널 출력을 감싸 대체 화면 진입·이탈에 맞춰 kitty 키 플래그를 넣고 뺀다.
// bubbletea 는 정상 종료·패닉 복구·시그널 종료 모두 대체 화면을 나오며 끝나므로 끄는 시퀀스도 늘 나간다.
type Writer struct {
	f *os.File
}

func newReader(src io.Reader) *Reader {
	return &Reader{src: src}
}

func NewTTY(f *os.File) *TTY {
	return &TTY{Reader: &Reader{src: f, tty: f}, f: f}
}

func NewWriter(f *os.File) *Writer {
	return &Writer{f: f}
}

func (r *Reader) Read(p []byte) (int, error) {
	if len(p) == 0 {
		return 0, nil
	}
	for len(r.out) == 0 && r.err == nil {
		if r.tty == nil {
			// tty 가 아니면 출력이 p 를 넘어도 다음 Read 에 넘기면 되므로 꼬리 길이만큼은 늘 더 읽는다
			r.readMore(max(len(p)-len(r.in), maxSeq))
			continue
		}
		r.readMore(len(p) - len(r.in))
		// tty 는 이미 들어온 바이트만 읽고 돌아간다. 막혀 기다리면 cancelreader 가 취소하지 못한다
		for len(r.in) > 0 && r.err == nil && len(r.in)+len(r.out) < len(p) && inputReady(r.tty) {
			r.readMore(len(p) - len(r.in) - len(r.out))
		}
		// 기다려도 안 온 꼬리는 내보낸다. 단, ESC [ 로 시작한 덜 온 CSI 는 들고 있는다. 내보내면 bubbletea 가
		// ESC 키와 글자로 갈라 읽어 마우스 보고(`<64;84;23M`)가 편집기에 글자로 들어간다. 홀로 친 ESC·ESC O 만 내보낸다
		if !(len(r.in) >= 2 && r.in[1] == '[') {
			r.out = append(r.out, r.in...)
			r.in = r.in[:0]
		}
		// 버린 키만 있었거나 덜 온 CSI 만 있으면 0 바이트를 돌려준다. 다음 Read 는 다시 cancelreader 의 대기를 거친다
		break
	}
	if r.err != nil {
		r.out = append(r.out, r.in...)
		r.in = r.in[:0]
	}
	if len(r.out) == 0 {
		return 0, r.err
	}
	n := copy(p, r.out)
	r.out = r.out[n:]
	return n, nil
}

// readMore 는 원본에서 room 바이트까지 읽어 번역한다. 원본 오류는 남은 출력을 다 내보낸 뒤 전한다.
// 출력은 입력보다 길지 않으므로 tty 는 room 을 지키면 출력이 p 에 다 들어가 다음 호출로 넘길 게 없다.
func (r *Reader) readMore(room int) {
	if room <= 0 {
		// 꼬리가 p 를 다 채울 만큼 길면 키 시퀀스일 수 없다
		r.out = append(r.out, r.in...)
		r.in = r.in[:0]
		return
	}
	if cap(r.buf) < room {
		r.buf = make([]byte, room)
	}
	n, err := r.src.Read(r.buf[:room])
	r.in = append(r.in, r.buf[:n]...)
	r.err = err
	r.translate()
}

// translate 는 r.in 을 앞에서부터 번역해 r.out 에 붙이고, 끝의 불완전한 ESC 시퀀스만 r.in 에 남긴다.
func (r *Reader) translate() {
	in := r.in
	i := 0
	for i < len(in) {
		b := in[i]
		if r.paste {
			// 붙여넣기 본문은 그대로 흘리며 끝 표시만 센다. 꼬리를 들고 있지 않아 tty 에서도 멈추지 않는다
			r.out = append(r.out, b)
			i++
			switch {
			case b == pasteEnd[r.pasteMatch]:
				r.pasteMatch++
			case b == esc:
				r.pasteMatch = 1
			default:
				r.pasteMatch = 0
			}
			if r.pasteMatch == len(pasteEnd) {
				r.paste = false
				r.pasteMatch = 0
			}
			continue
		}
		if b != esc {
			r.out = append(r.out, b)
			i++
			continue
		}
		n := scanCSI(in[i:])
		if n == 0 {
			break
		}
		if n < 0 {
			r.out = append(r.out, b)
			i++
			continue
		}
		seq := in[i : i+n]
		if bytes.Equal(seq, pasteStart) {
			r.paste = true
		}
		r.out = convert(r.out, seq)
		i += n
	}
	r.in = append(r.in[:0], in[i:]...)
}

func (t *TTY) Fd() uintptr                 { return t.f.Fd() }
func (t *TTY) Name() string                { return t.f.Name() }
func (t *TTY) Write(p []byte) (int, error) { return t.f.Write(p) }
func (t *TTY) Close() error                { return t.f.Close() }

func (w *Writer) Write(p []byte) (int, error) {
	if !bytes.Contains(p, altOn) && !bytes.Contains(p, altOff) {
		return w.f.Write(p)
	}
	// bubbletea 는 대체 화면 시퀀스를 Write 한 번에 통째로 쓴다(standardRenderer.execute).
	// 한계: 시퀀스가 Write 둘로 쪼개지면 켜기·끄기를 놓친다. bubbletea 를 올릴 때 이 전제를 다시 확인한다.
	out := bytes.ReplaceAll(p, altOn, append(append([]byte{}, altOn...), pushFlags...))
	out = bytes.ReplaceAll(out, altOff, append(append([]byte{}, popFlags...), altOff...))
	if _, err := w.f.Write(out); err != nil {
		return 0, err
	}
	return len(p), nil
}

func (w *Writer) Read(p []byte) (int, error) { return w.f.Read(p) }
func (w *Writer) Close() error               { return w.f.Close() }
func (w *Writer) Fd() uintptr                { return w.f.Fd() }

// scanCSI 는 b(ESC 로 시작)가 완결된 CSI 면 길이를, 아직 덜 왔으면 0 을, CSI 가 아니면 -1 을 돌려준다.
// 덜 온 SS3(ESC O)도 0 으로 보고 기다린다.
func scanCSI(b []byte) int {
	if len(b) < 2 {
		return 0
	}
	if b[1] == 'O' && len(b) == 2 {
		return 0
	}
	if b[1] != '[' {
		return -1
	}
	for i := 2; i < len(b); i++ {
		if i >= maxSeq {
			return -1
		}
		c := b[i]
		switch {
		case c >= 0x40 && c <= 0x7e:
			return i + 1
		case c < 0x20 || c > 0x7e:
			return -1
		}
	}
	if len(b) >= maxSeq {
		return -1
	}
	return 0
}

// convert 는 완결된 CSI 시퀀스 하나를 dst 에 붙인다. kitty 키면 레거시 바이트로, ⌘ 조합이면 표의 단축키로
// 바꾸고 표에 없는 ⌘ 조합과 키를 뗀 이벤트는 버린다. 모르는 시퀀스는 그대로 둔다.
func convert(dst, seq []byte) []byte {
	final := seq[len(seq)-1]
	params := string(seq[2 : len(seq)-1])
	if strings.Trim(params, "0123456789;:") != "" {
		return append(dst, seq...)
	}
	keyField, rest, _ := strings.Cut(params, ";")
	modField, _, _ := strings.Cut(rest, ";")
	cpText, _, _ := strings.Cut(keyField, ":")
	modText, event, _ := strings.Cut(modField, ":")
	cp, err := strconv.Atoi(cpText)
	if err != nil {
		return append(dst, seq...)
	}
	mods := 1
	if modText != "" {
		mods, err = strconv.Atoi(modText)
	}
	if err != nil || mods < 1 || cp > unicode.MaxRune {
		return append(dst, seq...)
	}
	// 3 = 키 뗌. 이벤트 보고 플래그를 켜지 않았지만 들어와도 키 입력으로 치지 않는다
	if event == "3" {
		return dst
	}
	m := (mods - 1) &^ modLocks
	key := rune(cp)
	special := final == '~' || bytes.IndexByte(letterFinals, final) >= 0

	switch {
	case final == 'u' && m&modSuper != 0:
		k := superKey{key, m &^ modSuper}
		if key >= 'A' && key <= 'Z' {
			k = superKey{key + 'a' - 'A', k.mods | modShift}
		}
		return append(dst, superKeys[k]...)
	case special && m&modSuper != 0:
		if cp == 1 && m == modSuper {
			return append(dst, superArrows[final]...)
		}
		return dst
	case special && m != mods-1:
		// 잠금 비트만 지워 bubbletea 가 아는 꼴로 다시 쓴다
		if m == 0 && final != '~' {
			return append(dst, esc, '[', final)
		}
		dst = append(append(dst, esc, '['), cpText...)
		if m != 0 {
			dst = strconv.AppendInt(append(dst, ';'), int64(m+1), 10)
		}
		return append(dst, final)
	case final != 'u':
		return append(dst, seq...)
	}

	if m&^(modShift|modAlt|modCtrl) != 0 {
		return append(dst, seq...)
	}
	var b []byte
	switch {
	case key == esc:
		b = []byte{esc}
	case key == '\r' || key == kpEnter:
		b = []byte{'\r'}
	case key == '\t' && m&modShift != 0 && m&modCtrl == 0:
		b = []byte("\x1b[Z")
	case key == '\t':
		b = []byte{'\t'}
	case key == 0x7f && m&modCtrl != 0:
		b = []byte{0x08}
	case key == 0x7f:
		b = []byte{0x7f}
	case m&modCtrl != 0 && key < utf8.RuneSelf && unicode.IsLetter(key):
		b = []byte{byte(key) & 0x1f}
	case m&modCtrl != 0:
		c, ok := ctrlBytes[key]
		if !ok {
			return append(dst, seq...)
		}
		b = []byte{c}
	case m&modShift != 0 && key >= 'a' && key <= 'z':
		b = []byte{byte(key) - 'a' + 'A'}
	// shift 붙은 기호는 자판 배열을 몰라 레거시 글자를 알 수 없다
	case m&modShift != 0 || key < 0x20 || (key >= 0xe000 && key <= 0xf8ff) || !utf8.ValidRune(key):
		return append(dst, seq...)
	default:
		b = utf8.AppendRune(nil, key)
	}
	if m&modAlt != 0 {
		dst = append(dst, esc)
	}
	return append(dst, b...)
}

// MacStyle 은 키 안내를 ⌘ 글자로 보일지다(R47.1). DB_KEYS=cmd·ctrl 이 먼저고, 없으면 macOS 이거나
// kitty 키 프로토콜을 켰을 때다(⌘ 조합을 받을 수 있는 터미널).
func MacStyle(kittyOn bool) bool {
	switch os.Getenv("DB_KEYS") {
	case "cmd":
		return true
	case "ctrl":
		return false
	}
	return runtime.GOOS == "darwin" || kittyOn
}

// labelMods 는 키 안내의 수정자 글자와 맥 기호다. ⌘ 표에 없는 조합(Ctrl+C·Ctrl+Space)은 이 기호로만 바꾼다
var labelMods = [][2]string{{"Ctrl+", "⌃"}, {"Alt+", "⌥"}, {"Shift+", "⇧"}}

// Label 은 mac 이면 s 안의 키 안내(Ctrl+R·Alt+E·F3)를 맥 글자로 바꾼다. ⌘ 표에 있는 키는 그 ⌘ 조합으로,
// 없는 수정자 조합은 ⌃⌥⇧ 기호로, 표에 없는 F 키는 그대로 둔다. 키 이름은 ASCII 영숫자 한 덩이(PgUp·F5)거나
// 글자 하나(← · /)라 `Ctrl+←→↑↓` 는 ⌃←→↑↓, `Alt+1..9` 는 ⌘1..9 가 된다.
func Label(s string, mac bool) string {
	if !mac {
		return s
	}
	var b strings.Builder
	for i := 0; i < len(s); {
		if i > 0 && isASCIIAlnum(s[i-1]) {
			b.WriteByte(s[i])
			i++
			continue
		}
		j := i
		var mods strings.Builder
		for matched := true; matched; {
			matched = false
			for _, m := range labelMods {
				if strings.HasPrefix(s[j:], m[0]) {
					mods.WriteString(m[1])
					j += len(m[0])
					matched = true
				}
			}
		}
		k := j
		for k < len(s) && isASCIIAlnum(s[k]) {
			k++
		}
		if k == j && j > i && j < len(s) && s[j] != ' ' {
			_, n := utf8.DecodeRuneInString(s[j:])
			k = j + n
		}
		// 수정자 없이 표에 든 글자는 F 키뿐이다
		if label, ok := macLabels[s[i:k]]; ok {
			b.WriteString(label)
			i = k
			continue
		}
		if j == i {
			b.WriteByte(s[i])
			i++
			continue
		}
		b.WriteString(mods.String())
		b.WriteString(s[j:k])
		i = k
	}
	return b.String()
}

func isASCIIAlnum(c byte) bool {
	return c >= '0' && c <= '9' || c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z'
}
