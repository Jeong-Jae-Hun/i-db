// Package sqlfmt 는 PostgreSQL 문장을 읽기 좋게 줄 맞춤한다. 공백과 키워드 대소문자만 바꾸고 토큰 순서는 지킨다.
package sqlfmt

import "strings"

const (
	Word Kind = iota
	Number
	String
	Quoted
	Param
	LineComment
	BlockComment
	Punct
)

const (
	spaceBytes = " \t\n\r\f\v"
	// 문자열 바로 앞에 붙는 PostgreSQL 접두사(E·B·X·N). E 만 역슬래시 이스케이프를 쓴다
	stringPrefixes = "eEbBxXnN"
)

// Kind 는 토큰 종류다. 연산자·괄호·쉼표 같은 기호는 모두 한 바이트짜리 Punct 다.
type Kind int

// Token 은 sql[Start:End] 조각이다. 토큰 사이 빈틈은 언제나 공백뿐이다.
type Token struct {
	Kind       Kind
	Start, End int
	Text       string
}

// Tokenize 는 sql 을 공백 밖의 토큰으로 나눈다. 닫히지 않은 문자열·주석은 끝까지 한 토큰이다.
// 편집기 색칠(editor/lexer.go)과 문장 나누기(db/sqltext.go)에도 비슷한 스캐너가 있다. 둘 다 내보내지 않는
// 목적별 스캐너라 여기서 위치·공백을 지키는 전체 토큰화를 따로 둔다. sqlfmt 와 params 는 이것 하나를 쓴다.
func Tokenize(sql string) []Token {
	var out []Token
	for i := 0; i < len(sql); {
		c := sql[i]
		if strings.IndexByte(spaceBytes, c) >= 0 {
			i++
			continue
		}
		start, kind := i, Punct
		switch {
		case strings.HasPrefix(sql[i:], "--"):
			i, kind = i+2, LineComment
			for i < len(sql) && sql[i] != '\n' {
				i++
			}
		case strings.HasPrefix(sql[i:], "/*"):
			// PostgreSQL 블록 주석은 중첩된다
			depth := 0
			for i < len(sql) {
				if strings.HasPrefix(sql[i:], "/*") {
					depth, i = depth+1, i+2
				} else if strings.HasPrefix(sql[i:], "*/") {
					depth, i = depth-1, i+2
					if depth == 0 {
						break
					}
				} else {
					i++
				}
			}
			kind = BlockComment
		case c == '\'':
			i, kind = i+quotedLen(sql[i:], false), String
		case c == '"':
			i, kind = i+quotedLen(sql[i:], false), Quoted
		case strings.IndexByte(stringPrefixes, c) >= 0 && i+1 < len(sql) && sql[i+1] == '\'':
			i, kind = i+1+quotedLen(sql[i+1:], c == 'e' || c == 'E'), String
		case c == '$' && i+1 < len(sql) && isDigit(sql[i+1]):
			i++
			for i < len(sql) && isDigit(sql[i]) {
				i++
			}
			kind = Param
		case c == '$':
			// $태그$ ... $태그$ 인용. 태그 모양이 아니면 '$' 한 글자짜리 기호다
			j := i + 1
			for j < len(sql) && (sql[j] == '_' || isLetter(sql[j]) || j > i+1 && isDigit(sql[j])) {
				j++
			}
			if j >= len(sql) || sql[j] != '$' {
				i++
				break
			}
			tag := sql[i : j+1]
			i, kind = len(sql), String
			if end := strings.Index(sql[j+1:], tag); end >= 0 {
				i = j + 1 + end + len(tag)
			}
		case isDigit(c) || c == '.' && i+1 < len(sql) && isDigit(sql[i+1]):
			i++
			for i < len(sql) {
				if (sql[i] == 'e' || sql[i] == 'E') && i+1 < len(sql) && (sql[i+1] == '+' || sql[i+1] == '-') {
					i += 2
					continue
				}
				if !isIdentByte(sql[i]) && sql[i] != '.' {
					break
				}
				i++
			}
			kind = Number
		case isIdentStart(c):
			// 0x80 이상 바이트는 모두 식별자 글자로 본다(PostgreSQL 과 같다). 덕분에 UTF-8 글자가 쪼개지지 않는다
			for i++; i < len(sql) && isIdentByte(sql[i]); i++ {
			}
			kind = Word
		default:
			i++
		}
		out = append(out, Token{Kind: kind, Start: start, End: i, Text: sql[start:i]})
	}
	return out
}

// quotedLen 은 s[0] 을 여는 따옴표로 보고 닫는 따옴표까지의 길이다. 같은 따옴표 두 개는 이스케이프다.
func quotedLen(s string, backslash bool) int {
	mark := s[0]
	for i := 1; i < len(s); i++ {
		switch {
		case backslash && s[i] == '\\':
			i++
		case s[i] == mark && i+1 < len(s) && s[i+1] == mark:
			i++
		case s[i] == mark:
			return i + 1
		}
	}
	return len(s)
}

func isDigit(c byte) bool { return c >= '0' && c <= '9' }

func isLetter(c byte) bool { return c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' }

func isIdentStart(c byte) bool { return c == '_' || isLetter(c) || c >= 0x80 }

func isIdentByte(c byte) bool { return isIdentStart(c) || isDigit(c) || c == '$' }
