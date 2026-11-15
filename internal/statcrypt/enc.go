package statcrypt

import (
	"fmt"
	"strconv"
	"strings"
	"unicode/utf8"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
)

// serverFunc 는 키·IV 가 없을 때 enc 를 바꿔 쓸 서버 함수다. db 는 만들지 않고 DB 에 이미 있을 때만 동작한다
const serverFunc = "staticEncrypt"

// ExpandEnc 는 문자열·주석·$태그$ 밖의 enc('<글>') 호출을 그 글의 staticEncrypt 암호문 리터럴로 바꾼다(명세 R52).
// 이름은 대소문자를 가리지 않고, 인자는 문자열 리터럴 하나(작은따옴표 두 번은 한 번, E'…' 도 받음)여야 한다.
// key 나 ivHex 가 비었으면 암호화하지 않고 enc 를 서버 함수 staticEncrypt 로 바꾸고 server 를 참으로 돌려준다.
// n 은 바꾼 호출 수다. 인자가 리터럴이 아니거나 닫히지 않았으면 그 위치(줄:칸)를 담은 오류이고,
// 오류에는 키·글을 싣지 않는다. schema.enc(…)·"enc"(…) 처럼 한정·따옴표 이름과 into·from·join·update·table·function·procedure
// 바로 뒤의 enc( (테이블·함수 이름 자리)는 건드리지 않는다.
func ExpandEnc(sql, key, ivHex string) (out string, n int, server bool, err error) {
	server = key == "" || ivHex == ""
	m := db.MaskSQL(sql)
	var b strings.Builder
	last := 0
	for i := 0; i+3 <= len(m); i++ {
		if !strings.EqualFold(m[i:i+3], "enc") ||
			i > 0 && (db.IsIdentByte(m[i-1]) || m[i-1] == '.') ||
			i+3 < len(m) && db.IsIdentByte(m[i+3]) {
			continue
		}
		open := skipSpace(m, i+3)
		if open >= len(m) || m[open] != '(' || nameKeywords[prevWord(m, i)] {
			continue // enc 라는 열·별칭이거나 enc 라는 테이블·함수 이름이다
		}
		q := skipSpace(m, open+1)
		escapes := q+1 < len(m) && (m[q] == 'E' || m[q] == 'e') && m[q+1] == '\''
		if escapes {
			q++
		}
		if q >= len(m) || m[q] != '\'' {
			return "", 0, false, posErr(sql, i, "argument must be a single string literal")
		}
		// 마스크는 리터럴 안을 공백으로 채우고 닫는 따옴표만 남긴다
		end := strings.IndexByte(m[q+1:], '\'')
		if end < 0 {
			return "", 0, false, posErr(sql, i, "unterminated string literal")
		}
		end += q + 1
		closeParen := skipSpace(m, end+1)
		if closeParen >= len(m) || m[closeParen] != ')' {
			return "", 0, false, posErr(sql, i, "argument must be a single string literal followed by )")
		}

		var repl string
		if server {
			repl = serverFunc + sql[i+3:closeParen+1]
		} else {
			plain, err := unquote(sql[q+1:end], escapes)
			if err != nil {
				return "", 0, false, posErr(sql, i, err.Error())
			}
			cipher, err := Encrypt(plain, key, ivHex)
			if err != nil {
				return "", 0, false, posErr(sql, i, err.Error())
			}
			repl = db.QuoteLiteral(cipher)
		}
		b.WriteString(sql[last:i])
		b.WriteString(repl)
		last = closeParen + 1
		i = closeParen
		n++
	}
	if n == 0 {
		return sql, 0, false, nil
	}
	b.WriteString(sql[last:])
	return b.String(), n, server, nil
}

// posErr 는 sql[at] 의 1부터 센 줄·칸(글자 단위)을 붙인 오류다.
func posErr(sql string, at int, msg string) error {
	before := sql[:at]
	line := strings.Count(before, "\n") + 1
	col := utf8.RuneCountInString(before[strings.LastIndexByte(before, '\n')+1:]) + 1
	return fmt.Errorf("enc() at %d:%d: %s", line, col, msg)
}

// nameKeywords 뒤의 enc( 는 호출이 아니라 테이블·함수 이름이다(insert into enc (a) …, create table enc (…)).
// 한계: 목록 밖 자리(create table if not exists enc (…) 등)는 호출로 읽는다. 그때는 "enc" 나 schema.enc 로 쓴다.
var nameKeywords = map[string]bool{"into": true, "from": true, "join": true, "update": true, "table": true, "function": true, "procedure": true}

// prevWord 는 m[:i] 끝의 공백을 건너뛴 바로 앞 단어(소문자)다. 단어가 아니면 빈 글이다.
func prevWord(m string, i int) string {
	j := i
	for j > 0 && strings.IndexByte(" \t\n\r\f\v", m[j-1]) >= 0 {
		j--
	}
	k := j
	for k > 0 && db.IsIdentByte(m[k-1]) {
		k--
	}
	return strings.ToLower(m[k:j])
}

func skipSpace(s string, i int) int {
	for i < len(s) && strings.IndexByte(" \t\n\r\f\v", s[i]) >= 0 {
		i++
	}
	return i
}

// unquote 는 따옴표 안 글을 값으로 읽는다. 작은따옴표 두 번은 한 번이고, escapes(E'…')면 PostgreSQL 처럼 \b \f \n \r \t,
// \ooo(8진 1~3자리), \xh(h)(16진 1~2자리), \uXXXX, \UXXXXXXXX 를 풀고 그 밖의 \c 는 c 다.
// \u·\U 자리가 모자라면 PostgreSQL 처럼 오류다.
func unquote(body string, escapes bool) (string, error) {
	body = strings.ReplaceAll(body, "''", "'")
	if !escapes || !strings.Contains(body, `\`) {
		return body, nil
	}
	var b strings.Builder
	for i := 0; i < len(body); i++ {
		if body[i] != '\\' || i+1 == len(body) {
			b.WriteByte(body[i])
			continue
		}
		i++
		switch c := body[i]; c {
		case 'b':
			b.WriteByte('\b')
		case 'f':
			b.WriteByte('\f')
		case 'n':
			b.WriteByte('\n')
		case 'r':
			b.WriteByte('\r')
		case 't':
			b.WriteByte('\t')
		case 'x', 'u', 'U':
			width := hexWidth[c]
			j := i + 1
			for j < len(body) && j-i-1 < width && strings.IndexByte("0123456789abcdefABCDEF", body[j]) >= 0 {
				j++
			}
			if c != 'x' && j-i-1 != width {
				return "", fmt.Errorf("invalid Unicode escape \\%c", c)
			}
			if j == i+1 { // \x 뒤에 16진이 없으면 x 그대로다
				b.WriteByte(c)
				continue
			}
			v, _ := strconv.ParseUint(body[i+1:j], 16, 32)
			if c == 'x' {
				b.WriteByte(byte(v))
			} else if r := rune(v); utf8.ValidRune(r) {
				b.WriteRune(r)
			} else {
				return "", fmt.Errorf("invalid Unicode escape value \\%c", c)
			}
			i = j - 1
		case '0', '1', '2', '3', '4', '5', '6', '7':
			j := i
			for j < len(body) && j-i < 3 && body[j] >= '0' && body[j] <= '7' {
				j++
			}
			v, _ := strconv.ParseUint(body[i:j], 8, 16)
			b.WriteByte(byte(v))
			i = j - 1
		default:
			b.WriteByte(c)
		}
	}
	return b.String(), nil
}

// hexWidth 는 E'…' 의 16진 이스케이프마다 읽을 최대 자리 수다
var hexWidth = map[byte]int{'x': 2, 'u': 4, 'U': 8}
