package db

import (
	"regexp"
	"strings"
	"unicode"
)

var (
	dollarTagRe = regexp.MustCompile(`^\$[A-Za-z_][A-Za-z0-9_]*\$|^\$\$`)
	analyzeRe   = regexp.MustCompile(`(?i)\banaly[sz]e\b`) // PostgreSQL 은 ANALYSE 철자도 받는다
)

var redisReadCmds = map[string]bool{
	"GET": true, "MGET": true, "STRLEN": true, "GETRANGE": true, "EXISTS": true, "TYPE": true, "TTL": true, "PTTL": true,
	"KEYS": true, "SCAN": true, "HGET": true, "HGETALL": true, "HMGET": true, "HKEYS": true, "HVALS": true, "HLEN": true,
	"HSCAN": true, "LRANGE": true, "LLEN": true, "LINDEX": true, "SMEMBERS": true, "SCARD": true, "SISMEMBER": true,
	"SSCAN": true, "ZRANGE": true, "ZRANGEBYSCORE": true, "ZREVRANGE": true, "ZCARD": true, "ZSCORE": true, "ZRANK": true,
	"ZSCAN": true, "DBSIZE": true, "INFO": true, "PING": true, "XRANGE": true, "XLEN": true, "PFCOUNT": true,
	"GETBIT": true, "BITCOUNT": true, "TIME": true,
}

// 관리 명령은 하위 명령까지 봐야 한다. SLOWLOG RESET·CLIENT KILL·MEMORY PURGE 는 쓰기로 본다.
var redisReadSubCmds = map[string]map[string]bool{
	"CLIENT":  {"LIST": true, "INFO": true, "GETNAME": true, "ID": true},
	"SLOWLOG": {"GET": true, "LEN": true},
	"MEMORY":  {"USAGE": true, "STATS": true, "DOCTOR": true},
	"OBJECT":  {"ENCODING": true, "FREQ": true, "IDLETIME": true, "REFCOUNT": true},
	"XINFO":   {"STREAM": true, "GROUPS": true, "CONSUMERS": true},
}

// stmtSpan 은 text[start:end] 조각이다. end 는 경계 ';' 의 위치이거나 len(text) 다.
type stmtSpan struct {
	start, end int
	code       bool // 공백·주석 말고 내용이 있다
}

// StatementAt 는 offset(바이트)이 속한 SQL 문장을 돌려준다.
// 경계는 따옴표·$태그$·주석 밖의 ';' 다. 앞뒤 공백과 끝 ';' 는 뗀다.
// 틀려도 실행은 extended protocol 이 여러 문장을 거부해 막는다(backend.go PGConn.run).
func StatementAt(text string, offset int) string {
	spans := scanStatements(text)
	s := spans[len(spans)-1]
	for _, sp := range spans {
		if offset <= sp.end {
			s = sp
			break
		}
	}
	return strings.TrimSpace(text[s.start:s.end])
}

// SplitStatements 는 text 를 StatementAt 과 같은 경계로 나눈다. 공백·주석뿐인 조각은 뺀다.
func SplitStatements(text string) []string {
	var out []string
	for _, s := range scanStatements(text) {
		if stmt := strings.TrimSpace(text[s.start:s.end]); s.code && stmt != "" {
			out = append(out, stmt)
		}
	}
	return out
}

// scanStatements 는 따옴표·$태그$·주석 밖의 ';' 로 text 를 조각낸다. 조각은 하나 이상이다.
// StatementAt 과 SplitStatements 가 같은 어휘 규칙을 쓰도록 MaskSQL 하나로 읽는다.
func scanStatements(text string) []stmtSpan {
	m := MaskSQL(text)
	var spans []stmtSpan
	cur := stmtSpan{}
	for i := 0; i < len(m); i++ {
		switch {
		case m[i] == ';':
			cur.end = i
			spans = append(spans, cur)
			cur = stmtSpan{start: i + 1}
		case strings.IndexByte(" \t\n\r\f\v", m[i]) < 0:
			cur.code = true
		}
	}
	cur.end = len(m)
	return append(spans, cur)
}

// MaskSQL 은 text 와 길이가 같은 사본에서 주석을 공백으로, 따옴표·$태그$ 안 글자를 공백으로 바꾼다.
// 따옴표와 태그는 남겨 내용이 있었다는 표시로 쓴다. 남은 글자에서 ';'·괄호·키워드를 찾으면
// 문자열·주석·"따옴표 이름" 안의 것을 잘못 세지 않는다.
func MaskSQL(text string) string {
	m, _ := maskSQL(text)
	return m
}

// HasComment 는 text 에 따옴표·$태그$ 밖의 주석(--·/* */)이 있는지다. MaskSQL 과 같은 어휘 규칙으로 읽는다.
func HasComment(text string) bool {
	_, comment := maskSQL(text)
	return comment
}

// maskSQL 은 MaskSQL 의 사본과 주석이 있었는지다.
func maskSQL(text string) (string, bool) {
	comment := false
	b := []byte(text)
	for i := 0; i < len(b); i++ {
		start := i
		switch {
		case text[i] == '\'' || text[i] == '"':
			q := text[i]
			// E'...' 안에서는 백슬래시가 다음 글자를 이스케이프한다
			escapes := q == '\'' && i > 0 && (text[i-1] == 'E' || text[i-1] == 'e') && (i < 2 || !IsIdentByte(text[i-2]))
			for i++; i < len(text); i++ {
				if escapes && text[i] == '\\' {
					i++
					continue
				}
				if text[i] != q {
					continue
				}
				if i+1 < len(text) && text[i+1] == q { // '' 이스케이프
					i++
					continue
				}
				break
			}
			fillSpaces(b, start+1, i)
		case strings.HasPrefix(text[i:], "--"):
			comment = true
			for i < len(text) && text[i] != '\n' {
				i++
			}
			fillSpaces(b, start, i)
		case strings.HasPrefix(text[i:], "/*"):
			// 블록 주석은 중첩된다
			comment = true
			depth := 0
			for ; i < len(text); i++ {
				if strings.HasPrefix(text[i:], "/*") {
					depth++
					i++
				} else if strings.HasPrefix(text[i:], "*/") {
					depth--
					i++
					if depth == 0 {
						break
					}
				}
			}
			fillSpaces(b, start, i+1)
		case text[i] == '$' && (i == 0 || !IsIdentByte(text[i-1])):
			if tag := dollarTagRe.FindString(text[i:]); tag != "" {
				if j := strings.Index(text[i+len(tag):], tag); j >= 0 {
					i += 2*len(tag) + j - 1
					fillSpaces(b, start+len(tag), i+1-len(tag))
				} else {
					i = len(text)
					fillSpaces(b, start+len(tag), i)
				}
			}
		}
	}
	return string(b), comment
}

// fillSpaces 는 b[from:to] 를 공백으로 채운다. 범위가 b 를 넘으면 끝에서 멈춘다.
func fillSpaces(b []byte, from, to int) {
	for k := from; k < min(to, len(b)); k++ {
		b[k] = ' '
	}
}

// trimStatement 는 앞뒤 공백과 끝의 ';'(여러 개, 사이 공백 포함)를 뗀다.
func trimStatement(s string) string { return strings.TrimRight(strings.TrimSpace(s), "; \t\r\n\f\v") }

// IsIdentByte 는 b 가 따옴표 없는 SQL 식별자에 들 수 있는 바이트인지다($ 와 UTF-8 바이트 포함).
func IsIdentByte(b byte) bool {
	return b == '_' || b == '$' || b >= '0' && b <= '9' || b >= 'a' && b <= 'z' || b >= 'A' && b <= 'Z' || b >= 0x80
}

// LineAt 는 offset 이 있는 줄을 돌려준다.
func LineAt(text string, offset int) string {
	if offset > len(text) {
		offset = len(text)
	}
	start := strings.LastIndexByte(text[:offset], '\n') + 1
	end := strings.IndexByte(text[offset:], '\n')
	if end < 0 {
		return strings.TrimSpace(text[start:])
	}
	return strings.TrimSpace(text[start : offset+end])
}

// SplitArgs 는 Redis 명령 줄을 인자로 나눈다. 따옴표 안 공백은 유지하고 따옴표 안 \" 는 따옴표로 푼다.
// 한글 같은 멀티바이트 문자를 깨지 않도록 rune 단위로 읽는다.
func SplitArgs(line string) []string {
	var args []string
	var cur strings.Builder
	inArg := false
	var quote rune
	rs := []rune(line)
	for i := 0; i < len(rs); i++ {
		c := rs[i]
		switch {
		case quote != 0:
			if c == '\\' && i+1 < len(rs) {
				i++
				cur.WriteRune(rs[i])
			} else if c == quote {
				quote = 0
			} else {
				cur.WriteRune(c)
			}
		case c == '"' || c == '\'':
			quote = c
			inArg = true
		case unicode.IsSpace(c):
			if inArg {
				args = append(args, cur.String())
				cur.Reset()
				inArg = false
			}
		default:
			cur.WriteRune(c)
			inArg = true
		}
	}
	if inArg {
		args = append(args, cur.String())
	}
	return args
}

// FirstSQLWord 는 앞쪽 공백·주석·여는 괄호를 건너뛴 첫 단어([A-Za-z0-9_] 연속)를 소문자로 돌려준다. 없으면 빈 문자열.
// 앞쪽 주석은 정규식이 아니라 직접 건너뛴다. 블록 주석은 중첩되고 여러 줄일 수 있어서다.
// 단어는 공백이 아니라 식별자 글자에서 끊는다. select*from 처럼 붙여 써도 select 로 봐야 판정과 커서 상한을 피하지 못한다.
func FirstSQLWord(stmt string) string {
	s := stmt
	for {
		s = strings.TrimLeft(s, " \t\r\n(")
		if strings.HasPrefix(s, "--") {
			if i := strings.IndexByte(s, '\n'); i >= 0 {
				s = s[i+1:]
				continue
			}
			return ""
		}
		if !strings.HasPrefix(s, "/*") {
			break
		}
		depth, i := 0, 0
		for i < len(s) {
			if strings.HasPrefix(s[i:], "/*") {
				depth, i = depth+1, i+2
			} else if strings.HasPrefix(s[i:], "*/") {
				depth, i = depth-1, i+2
				if depth == 0 {
					break
				}
			} else {
				i++
			}
		}
		if depth != 0 {
			return "" // 닫히지 않은 주석
		}
		s = s[i:]
	}
	end := 0
	for end < len(s) && (s[end] == '_' || s[end] >= '0' && s[end] <= '9' || s[end] >= 'a' && s[end] <= 'z' || s[end] >= 'A' && s[end] <= 'Z') {
		end++
	}
	return strings.ToLower(s[:end])
}

// IsReadOnlySQL 은 첫 단어로 판정한다. with 안의 쓰기(CTE), select ... into, 부수효과가 있는 함수 호출은 잡지 못한다(명세 R6.1).
func IsReadOnlySQL(stmt string) bool {
	switch FirstSQLWord(stmt) {
	case "", "select", "with", "show", "table", "values":
		return true
	case "explain":
		// EXPLAIN ANALYZE 는 문장을 실제로 실행한다. EXPLAIN (ANALYZE, ...) 처럼 옵션 목록에 있어도 같다.
		return !analyzeRe.MatchString(stmt)
	}
	return false
}

func IsReadOnlyRedis(args []string) bool {
	if len(args) == 0 {
		return false
	}
	cmd := strings.ToUpper(args[0])
	if sub, ok := redisReadSubCmds[cmd]; ok {
		return len(args) > 1 && sub[strings.ToUpper(args[1])]
	}
	return redisReadCmds[cmd]
}
