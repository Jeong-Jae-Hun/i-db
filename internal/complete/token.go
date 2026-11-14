package complete

import "strings"

const (
	tokWord    tokKind = iota // 따옴표 없는 식별자·키워드
	tokQuoted                 // "식별자"
	tokNumber                 // 숫자, $1 같은 매개변수
	tokString                 // '...', E'...', $tag$...$tag$
	tokComment                // -- ..., /* ... */
	tokPunct                  // 그 밖의 한 바이트
)

type tokKind uint8

// token 은 text[start:end] 를 가리킨다. open 은 닫히지 않아 커서가 end 에 있어도 안쪽인 토큰이다.
// 줄 주석은 줄바꿈을 end 에 넣지 않으므로 늘 open 이다.
type token struct {
	kind       tokKind
	start, end int
	open       bool
	ch         byte // tokPunct 의 글자
}

// tokenize 는 db.StatementAt 와 같은 경계 규칙(따옴표·$태그$·중첩 블록 주석)으로 SQL 을 토큰으로 나눈다.
// 닫히지 않은 문자열·주석은 끝까지 한 토큰이다.
func tokenize(text string) []token {
	var toks []token
	n := len(text)
	for i := 0; i < n; {
		c := text[i]
		start := i
		switch {
		case c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == '\f' || c == '\v':
			i++
			continue
		case strings.HasPrefix(text[i:], "--"):
			end := strings.IndexByte(text[i:], '\n')
			if end < 0 {
				end = n - i
			}
			i += end
			toks = append(toks, token{kind: tokComment, start: start, end: i, open: true})
		case strings.HasPrefix(text[i:], "/*"):
			depth := 0
			for i < n {
				if strings.HasPrefix(text[i:], "/*") {
					depth, i = depth+1, i+2
				} else if strings.HasPrefix(text[i:], "*/") {
					depth, i = depth-1, i+2
					if depth == 0 {
						break
					}
				} else {
					i++
				}
			}
			toks = append(toks, token{kind: tokComment, start: start, end: i, open: depth != 0})
		case c == '\'' || (c == 'e' || c == 'E') && i+1 < n && text[i+1] == '\'':
			// E'...' 안에서는 백슬래시가 다음 글자를 이스케이프한다
			escapes := c != '\''
			if escapes {
				i++
			}
			open := true
			for i++; i < n; i++ {
				if escapes && text[i] == '\\' {
					i++
					continue
				}
				if text[i] != '\'' {
					continue
				}
				if i+1 < n && text[i+1] == '\'' { // '' 이스케이프
					i++
					continue
				}
				open = false
				break
			}
			i = min(i+1, n)
			toks = append(toks, token{kind: tokString, start: start, end: i, open: open})
		case c == '"':
			open := true
			for i++; i < n; i++ {
				if text[i] != '"' {
					continue
				}
				if i+1 < n && text[i+1] == '"' {
					i++
					continue
				}
				open = false
				break
			}
			i = min(i+1, n)
			toks = append(toks, token{kind: tokQuoted, start: start, end: i, open: open})
		case c == '$':
			j := i + 1
			for j < n && isIdentByte(text[j]) && text[j] != '$' && !(j == i+1 && isDigit(text[j])) {
				j++
			}
			if j < n && text[j] == '$' {
				tag := text[i : j+1]
				open := true
				i = n
				if k := strings.Index(text[j+1:], tag); k >= 0 {
					i, open = j+1+k+len(tag), false
				}
				toks = append(toks, token{kind: tokString, start: start, end: i, open: open})
				continue
			}
			for i++; i < n && isDigit(text[i]); i++ {
			}
			toks = append(toks, token{kind: tokNumber, start: start, end: i})
		case isDigit(c):
			for i < n && (isDigit(text[i]) || text[i] == '.') {
				i++
			}
			toks = append(toks, token{kind: tokNumber, start: start, end: i})
		case isIdentByte(c):
			for i < n && isIdentByte(text[i]) {
				i++
			}
			toks = append(toks, token{kind: tokWord, start: start, end: i})
		default:
			i++
			toks = append(toks, token{kind: tokPunct, start: start, end: i, ch: c})
		}
	}
	return toks
}

func isDigit(b byte) bool { return b >= '0' && b <= '9' }

// isIdentByte 는 db.isIdentByte 와 같다. 0x80 이상은 한글 같은 멀티바이트 식별자의 일부로 본다.
func isIdentByte(b byte) bool {
	return b == '_' || b == '$' || isDigit(b) || b >= 'a' && b <= 'z' || b >= 'A' && b <= 'Z' || b >= 0x80
}

// identName 은 식별자 토큰의 이름이다. "따옴표" 는 벗기고 "" 는 " 로 푼다.
func identName(text string, t token) string {
	s := text[t.start:t.end]
	if t.kind != tokQuoted {
		return s
	}
	s = strings.TrimPrefix(s, `"`)
	if !t.open {
		s = strings.TrimSuffix(s, `"`)
	}
	return strings.ReplaceAll(s, `""`, `"`)
}

// keywordOf 는 따옴표 없는 단어 토큰을 소문자로 돌려준다. 그 밖의 토큰은 빈 문자열이다.
func keywordOf(text string, t token) string {
	if t.kind != tokWord {
		return ""
	}
	return strings.ToLower(text[t.start:t.end])
}

func (t token) isPunct(c byte) bool { return t.kind == tokPunct && t.ch == c }

func isIdent(t token) bool { return t.kind == tokWord || t.kind == tokQuoted }
