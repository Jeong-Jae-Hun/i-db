package editor

import (
	"strconv"
	"strings"
	"unicode"
	"unicode/utf8"
)

const (
	rolePlain role = iota
	roleKeyword
	roleString
	roleNumber
	roleComment
	roleQuoted
	roleFunc
	roleOperator
	roleParam
	roleCommand
	roleGutter
	roleGutterCur
	rolePopup
	rolePopupKind
	rolePopupDetail
)

const (
	sqlOperators = "+-*/<>=~!@#%^&|?:"
	// 문자열 바로 앞에 붙는 PostgreSQL 접두사(E·B·X·N). E 만 역슬래시 이스케이프를 쓴다
	sqlStringPrefixes = "eEbBxXnN"
	sqlKeywordList    = `
abort absolute access action add admin after aggregate all also alter always analyse analyze and any array as asc
assertion assignment asymmetric at attach attribute authorization backward before begin between bigint bigserial binary
bit bool boolean both breadth by bytea cache call called cascade cascaded case cast catalog chain char character
characteristics check checkpoint cidr class close cluster coalesce collate collation column columns comment comments commit
committed concurrently configuration conflict connection constraint constraints content continue conversion copy cost
costs create cross csv cube current current_catalog current_date current_role current_schema current_time current_timestamp
current_user cursor cycle data database date day deallocate dec decimal declare default defaults deferrable deferred
definer delete delimiter delimiters depends depth desc detach dictionary disable discard distinct do document domain double
drop each else elsif enable encoding encrypted end enum escape event except exception exclude excluding exclusive execute
exists explain expression extension external extract false family fetch filter first float following for force foreign
format forward freeze from full function functions generated global grant granted greatest group grouping groups handler
having header hold hour identity if ilike immediate immutable implicit import in include including increment index indexes
inet inherit inherits initially inline inner inout input insensitive insert instead int integer intersect interval into
invoker is isnull isolation join json jsonb key label language large last lateral leading leakproof least left level like
limit listen load local localtime localtimestamp location lock locked logged loop mapping match materialized maxvalue merge
method minute minvalue mode money month move name names national natural nchar new next nfc nfd nfkc nfkd no none normalize
normalized not nothing notify notnull nowait null nullif nulls numeric object of off offset oids old on only operator option
options or order ordinality others out outer over overlaps overlay overriding owned owner parallel parser partial partition
passing password perform placing plans policy position preceding precision prepare prepared preserve primary prior
privileges procedural procedure procedures program publication quote raise range read real reassign record recursive ref
references referencing refresh reindex relative release rename repeatable replace replica reset restart restrict return
returning returns revoke right role rollback rollup routine routines row rows rule savepoint schema schemas scroll search
second security select sequence sequences serial serializable server session session_user set setof sets share show similar
simple skip smallint snapshot some sql stable standalone start statement statistics stdin stdout storage stored strict strip
subscription substring support symmetric sysid system table tables tablesample tablespace temp template temporary text then
ties time timestamp timestamptz to trailing transaction transform treat trigger trim true truncate trusted type types
uescape unbounded uncommitted unencrypted union unique unknown unlisten unlogged until update user using uuid vacuum valid
validate validator value values varchar variadic varying verbose version view views volatile when where while whitespace
window with within without work wrapper write xml year yes zone`
)

var sqlKeywords = map[string]bool{}

type role int

type token struct {
	start, end int
	role       role
}

func init() {
	for _, w := range strings.Fields(sqlKeywordList) {
		sqlKeywords[w] = true
	}
}

func isWord(r rune) bool {
	return r == '_' || unicode.IsLetter(r) || unicode.IsDigit(r)
}

func isDigit(c byte) bool {
	return c >= '0' && c <= '9'
}

// scanString 은 s[0] 을 여는 따옴표로 보고 닫는 따옴표까지의 길이를 돌려준다. 같은 따옴표 두 개는
// 이스케이프다. 닫히지 않으면 s 전체가 문자열이다(편집 중 화면이 깜빡이지 않게).
func scanString(s string, backslash bool) int {
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

// lexSQL 은 PostgreSQL 문법을 색칠용 토큰으로 나눈다. 색이 없는 구간(공백·일반 식별자·괄호)은 토큰을 만들지 않는다.
func lexSQL(src string) []token {
	var out []token
	for i := 0; i < len(src); {
		start, c := i, src[i]
		r, size := utf8.DecodeRuneInString(src[i:])
		kind := rolePlain
		switch {
		case strings.HasPrefix(src[i:], "--"):
			end := strings.IndexByte(src[i:], '\n')
			if end < 0 {
				end = len(src) - i
			}
			i, kind = i+end, roleComment
		case strings.HasPrefix(src[i:], "/*"):
			// PostgreSQL 블록 주석은 중첩된다
			depth := 0
			for i < len(src) {
				if strings.HasPrefix(src[i:], "/*") {
					depth, i = depth+1, i+2
				} else if strings.HasPrefix(src[i:], "*/") {
					depth, i = depth-1, i+2
					if depth == 0 {
						break
					}
				} else {
					i++
				}
			}
			kind = roleComment
		case c == '\'':
			i, kind = i+scanString(src[i:], false), roleString
		case c == '"':
			i, kind = i+scanString(src[i:], false), roleQuoted
		case strings.IndexByte(sqlStringPrefixes, c) >= 0 && i+1 < len(src) && src[i+1] == '\'':
			i, kind = i+1+scanString(src[i+1:], c == 'e' || c == 'E'), roleString
		case c == '$' && i+1 < len(src) && isDigit(src[i+1]):
			i++
			for i < len(src) && isDigit(src[i]) {
				i++
			}
			kind = roleParam
		case c == '$':
			j := i + 1
			for j < len(src) && (src[j] == '_' || isDigit(src[j]) && j > i+1 || src[j] >= 'a' && src[j] <= 'z' || src[j] >= 'A' && src[j] <= 'Z') {
				j++
			}
			if j >= len(src) || src[j] != '$' {
				i++
				break
			}
			tag := src[i : j+1]
			end := strings.Index(src[j+1:], tag)
			if end < 0 {
				i = len(src)
			} else {
				i = j + 1 + end + len(tag)
			}
			kind = roleString
		case isDigit(c) || c == '.' && i+1 < len(src) && isDigit(src[i+1]):
			i++
			for i < len(src) {
				d := src[i]
				if (d == 'e' || d == 'E') && i+1 < len(src) && (src[i+1] == '+' || src[i+1] == '-') {
					i += 2
					continue
				}
				if !(isDigit(d) || d == '.' || d == '_' || d >= 'a' && d <= 'z' || d >= 'A' && d <= 'Z') {
					break
				}
				i++
			}
			kind = roleNumber
		case r == '_' || unicode.IsLetter(r):
			i += size
			for i < len(src) {
				nr, ns := utf8.DecodeRuneInString(src[i:])
				if !isWord(nr) && nr != '$' {
					break
				}
				i += ns
			}
			// t.type 처럼 점 뒤 이름은 열이므로 키워드로 칠하지 않는다
			afterDot := start > 0 && src[start-1] == '.'
			next := strings.TrimLeft(src[i:], " \t")
			switch {
			case sqlKeywords[strings.ToLower(src[start:i])] && !afterDot:
				kind = roleKeyword
			case strings.HasPrefix(next, "("):
				kind = roleFunc
			}
		case strings.IndexByte(sqlOperators, c) >= 0:
			i, kind = i+1, roleOperator
		default:
			i += size
		}
		// 연산자 여러 글자(::, <>, ->>)는 한 토큰으로 묶는다
		n := len(out)
		switch {
		case kind == rolePlain:
		case kind == roleOperator && n > 0 && out[n-1].role == roleOperator && out[n-1].end == start:
			out[n-1].end = i
		default:
			out = append(out, token{start: start, end: i, role: kind})
		}
	}
	return out
}

// lexRedis 는 줄마다 첫 단어를 명령으로, 따옴표 인자를 문자열로, 숫자 인자를 숫자로 칠한다.
func lexRedis(src string) []token {
	var out []token
	first := true
	for i := 0; i < len(src); {
		c := src[i]
		if c == '\n' {
			first, i = true, i+1
			continue
		}
		if c == ' ' || c == '\t' || c == '\r' {
			i++
			continue
		}
		lineEnd := strings.IndexByte(src[i:], '\n')
		if lineEnd < 0 {
			lineEnd = len(src) - i
		}
		start, kind := i, rolePlain
		if c == '"' || c == '\'' {
			i, kind = i+scanString(src[i:i+lineEnd], true), roleString
		} else {
			end := strings.IndexAny(src[i:i+lineEnd], " \t\r")
			if end < 0 {
				end = lineEnd
			}
			i += end
			if _, err := strconv.ParseFloat(src[start:i], 64); err == nil {
				kind = roleNumber
			}
		}
		if first {
			first, kind = false, roleCommand
		}
		if kind != rolePlain {
			out = append(out, token{start: start, end: i, role: kind})
		}
	}
	return out
}
