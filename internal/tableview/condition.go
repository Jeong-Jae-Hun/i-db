package tableview

// ─── 칸 값으로 거르기 (R38) ───

import (
	"slices"
	"strings"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
)

// Condition 은 열 col 의 칸 값 조건이다. null 이면 value 는 쓰지 않는다.
//   - 예: "status", "ACTIVE" → "status" = 'ACTIVE', negate 면 "status" <> 'ACTIVE'.
//   - 예: null 이면 "status" IS NULL, negate 면 "status" IS NOT NULL.
func Condition(col, value string, null, negate bool) string {
	switch {
	case null && negate:
		return db.QuoteIdent(col) + " IS NOT NULL"
	case null:
		return db.QuoteIdent(col) + " IS NULL"
	case negate:
		return db.QuoteIdent(col) + " <> " + db.QuoteLiteral(value)
	}
	return db.QuoteIdent(col) + " = " + db.QuoteLiteral(value)
}

// toggleCondition 은 where 의 최상위 AND 항에 cond 가 있으면(따옴표 밖 공백 차이는 무시) 빼고, 없으면 AND 로 더한다.
// 최상위 OR·주석이 있으면 항을 가르지 않고 where 를 괄호로 감싼 뒤 더한다. 줄 주석은 닫는 괄호를 삼키지 않게 줄을 바꾼다.
// 따옴표·E'..'·$태그$·주석은 db.MaskSQL 의 어휘 규칙으로 가린 사본에서 찾는다.
//   - 예: 빈 where 에 "a" = 1 → "a" = 1. 한 번 더 → 빈 글.
//   - 예: x = 1 OR y = 2 → (x = 1 OR y = 2) AND "a" = 1.
func toggleCondition(where, cond string) string {
	where = strings.TrimSpace(where)
	if where == "" {
		return cond
	}
	terms, ok := andTerms(where)
	if !ok {
		if strings.Contains(where, "--") {
			return "(" + where + "\n) AND " + cond
		}
		return "(" + where + ") AND " + cond
	}
	key := squeeze(cond)
	kept := slices.DeleteFunc(slices.Clone(terms), func(t string) bool { return squeeze(t) == key })
	if len(kept) == len(terms) {
		return where + " AND " + cond
	}
	return strings.Join(kept, " AND ")
}

// andTerms 는 where 를 최상위 AND 로 가른 항(앞뒤 공백 제외)이다. 최상위 OR·주석·닫히지 않은 따옴표·괄호가 있으면 false 다.
// 가린 사본 m 은 s 와 길이가 같고 따옴표 안이 공백이라 따옴표는 다음 같은 따옴표로 건너뛰면 된다.
func andTerms(s string) ([]string, bool) {
	if db.HasComment(s) {
		return nil, false
	}
	m := db.MaskSQL(s)
	var terms []string
	depth, start := 0, 0
	for i := 0; i < len(m); i++ {
		switch c := m[i]; {
		case c == '\'' || c == '"':
			j := strings.IndexByte(m[i+1:], c)
			if j < 0 {
				return nil, false
			}
			i += j + 1
		case c == '(':
			depth++
		case c == ')':
			depth--
			if depth < 0 {
				return nil, false
			}
		case depth == 0 && keywordAt(m, i, "or"):
			return nil, false
		case depth == 0 && keywordAt(m, i, "and"):
			terms = append(terms, strings.TrimSpace(s[start:i]))
			start = i + len("and")
			i = start - 1
		}
	}
	if depth != 0 {
		return nil, false
	}
	return append(terms, strings.TrimSpace(s[start:])), true
}

// keywordAt 은 s[i:] 가 낱말 kw(대소문자 무시)로 시작하고 앞뒤가 이름 글자가 아닌지다.
func keywordAt(s string, i int, kw string) bool {
	end := i + len(kw)
	return end <= len(s) && strings.EqualFold(s[i:end], kw) &&
		(i == 0 || !identByte(s[i-1])) && (end == len(s) || !identByte(s[end]))
}

// identByte 는 따옴표 없는 이름에 들 수 있는 바이트다. 0x80 이상은 UTF-8 글자(한글 이름 등)의 일부다.
func identByte(b byte) bool {
	return b == '_' || b == '$' || b >= 0x80 || ('0' <= b && b <= '9') || ('a' <= b && b <= 'z') || ('A' <= b && b <= 'Z')
}

// squeeze 는 따옴표 밖의 공백 덩어리를 한 칸으로 줄이고 앞뒤 공백을 지운다. 따옴표 안은 값이라 그대로 둔다.
// $태그$ 안 공백도 줄지만 cond 는 달러 인용을 쓰지 않아 그런 항이 cond 와 같아지지는 않는다.
func squeeze(s string) string {
	m := db.MaskSQL(s)
	var b strings.Builder
	space := false
	for i := 0; i < len(m); i++ {
		c := m[i]
		if c == ' ' || c == '\t' || c == '\n' || c == '\r' {
			space = true
			continue
		}
		if space && b.Len() > 0 {
			b.WriteByte(' ')
		}
		space = false
		if c != '\'' && c != '"' {
			b.WriteByte(s[i])
			continue
		}
		end := len(s)
		if j := strings.IndexByte(m[i+1:], c); j >= 0 {
			end = i + j + 2
		}
		b.WriteString(s[i:end])
		i = end - 1
	}
	return b.String()
}
