// Package textutil 은 여러 화면 부품이 같이 쓰는 글자 처리다.
package textutil

import (
	"fmt"
	"regexp"
	"strconv"
	"strings"
)

// secretNameRe 는 비밀 값이 들 만한 이름이다. 넓게 잡아 api_key·monkey 처럼 비밀이 아닌 이름도 걸리지만 가리는 쪽이 안전하다
var secretNameRe = regexp.MustCompile(`(?i)pass|secret|token|key|pwd`)

// LooksSecret 은 파라미터·열 이름이 비밀 값을 담을 만한지다. 그런 값은 개인 상태 파일에도 남기지 않는다(R50·R55).
func LooksSecret(name string) bool { return secretNameRe.MatchString(name) }

// Printable 은 DB 값·키·오류의 제어 문자를 보이는 글자로 바꾼다. 터미널 제어(ESC·OSC)로 해석되지 않게 한다.
// 탭은 칸 폭을 지키려고 공백 두 칸으로, 깨진 UTF-8 바이트는 U+FFFD 로 바꾼다.
func Printable(s string, keepNewline bool) string {
	var b strings.Builder
	for _, r := range s {
		switch {
		case r == '\n' && keepNewline:
			b.WriteRune(r)
		case r == '\t':
			b.WriteString("  ")
		case r < 0x20 || r == 0x7f || (r >= 0x80 && r < 0xa0):
			fmt.Fprintf(&b, "\\x%02x", r)
		default:
			b.WriteRune(r)
		}
	}
	return b.String()
}

// GroupDigits 는 세 자리마다 쉼표를 넣는다. n 은 0 이상이다(행 번호·개수).
func GroupDigits(n int64) string {
	s := strconv.FormatInt(n, 10)
	for i := len(s) - 3; i > 0; i -= 3 {
		s = s[:i] + "," + s[i:]
	}
	return s
}
