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

// koreanUnits 는 만·억·조 단위다. 큰 단위부터 본다.
var koreanUnits = []struct {
	name string
	size uint64
}{{"조", 1e12}, {"억", 1e8}, {"만", 1e4}}

// KoreanCount 는 개수를 만·억·조 단위로 줄인다(대시보드 R64). 1만 미만은 쉼표 꼴이고, 단위로 10 미만이면
// 소수 한 자리(.0 은 뗌), 10 이상이면 정수다. 올리면 단위 경계를 넘을 수 있어(9.99만 → 10.0만) 버린다.
// 크기는 uint64 로 센다. int64 로 부호를 뒤집으면 MinInt64 가 넘쳐 자기 자신이 된다.
//   - 예: 37 → "37", 12000 → "1.2만", 1200000 → "120만", 150000000 → "1.5억"
func KoreanCount(n int64) string {
	sign, u := "", uint64(n)
	if n < 0 {
		sign, u = "-", -u
	}
	for _, unit := range koreanUnits {
		if u < unit.size {
			continue
		}
		if whole := u / unit.size; whole >= 10 {
			return sign + GroupDigits(int64(whole)) + unit.name // whole ≤ 2^63/1e4 라 int64 에 든다
		}
		tenths := u * 10 / unit.size // u < 10*unit.size 라 넘치지 않는다
		if tenths%10 == 0 {
			return fmt.Sprintf("%s%d%s", sign, tenths/10, unit.name)
		}
		return fmt.Sprintf("%s%d.%d%s", sign, tenths/10, tenths%10, unit.name)
	}
	return sign + GroupDigits(int64(u))
}
