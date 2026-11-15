package grid

// ─── 거르기 식 (R14.4) ───

import (
	"cmp"
	"strconv"
	"strings"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
)

// FilterOp 는 거르기 식의 비교 방법이다.
type FilterOp int

const (
	OpContains    FilterOp = iota // 값이 든다(대소문자 무시)
	OpNotContains                 // 어느 열에도 없다(!text, 모든 열만)
	OpEq                          // 같다(대소문자 구분)
	OpNe
	OpGt
	OpGe
	OpLt
	OpLe
	OpNull
	OpNotNull
)

var (
	// filterTokens 는 열 이름 뒤에 오는 연산자다. 두 글자 연산자를 먼저 본다
	filterTokens = map[string]FilterOp{":": OpContains, "=": OpEq, "!=": OpNe, ">": OpGt, ">=": OpGe, "<": OpLt, "<=": OpLe}
	compareSQL   = map[FilterOp]string{OpEq: "=", OpNe: "<>", OpGt: ">", OpGe: ">=", OpLt: "<", OpLe: "<="}
	likeEscaper  = strings.NewReplacer(`\`, `\\`, `%`, `\%`, `_`, `\_`)
)

// Filter 는 `/` 거르기 글을 읽은 조건이다. Col 이 -1 이면 모든 열에서 Value 를 찾는다.
type Filter struct {
	Col   int
	Name  string // Col 열의 결과 이름
	Op    FilterOp
	Value string
}

// ParseFilter 는 거르기 글을 조건으로 읽는다. 열 이름은 대소문자를 무시하고, 모르는 열이면 글 전체를 모든 열 찾기로 본다.
//   - 예: "status=ACTIVE" → status 열 OpEq "ACTIVE". "email=null" → OpNull.
//   - 예: "!abc" → 모든 열 OpNotContains "abc". "http://x" → 열 http 가 없으면 모든 열 OpContains.
func ParseFilter(expr string, cols []string) Filter {
	if i, tok := filterToken(expr); i > 0 {
		name := strings.TrimSpace(expr[:i])
		for c, col := range cols {
			if !strings.EqualFold(col, name) {
				continue
			}
			f := Filter{Col: c, Name: col, Op: filterTokens[tok], Value: strings.TrimSpace(expr[i+len(tok):])}
			switch {
			case !strings.EqualFold(f.Value, "null"):
			case f.Op == OpEq:
				f.Op = OpNull
			case f.Op == OpNe:
				f.Op = OpNotNull
			}
			return f
		}
	}
	if rest, ok := strings.CutPrefix(expr, "!"); ok {
		return Filter{Col: -1, Op: OpNotContains, Value: rest}
	}
	return Filter{Col: -1, Op: OpContains, Value: expr}
}

// filterToken 은 처음 나오는 연산자의 자리와 글자다. 없으면 -1 이다. '!' 는 뒤에 '=' 가 올 때만 연산자다.
func filterToken(expr string) (int, string) {
	for i := 0; i < len(expr); i++ {
		switch c := expr[i]; c {
		case ':', '=':
			return i, string(c)
		case '<', '>', '!':
			if i+1 < len(expr) && expr[i+1] == '=' {
				return i, expr[i : i+2]
			}
			if c != '!' {
				return i, string(c)
			}
		}
	}
	return -1, ""
}

// SQL 은 조건을 WHERE 식으로 바꾼다. 모든 열 찾기는 열을 몰라 false 다.
// 같음·크기 비교는 numeric(숫자 열)이고 값이 십진수면 숫자로(인덱스를 쓴다), 아니면 글자(::text)로 비교한다.
//   - 예: name:a_b → "name"::text ILIKE '%a\_b%'. id>100(숫자 열) → "id" > 100. status=A → "status"::text = 'A'.
func (f Filter) SQL(numeric bool) (string, bool) {
	if f.Col < 0 {
		return "", false
	}
	col := db.QuoteIdent(f.Name)
	switch f.Op {
	case OpContains:
		return col + "::text ILIKE " + db.QuoteLiteral("%"+likeEscaper.Replace(f.Value)+"%"), true
	case OpNull:
		return col + " IS NULL", true
	case OpNotNull:
		return col + " IS NOT NULL", true
	}
	if numeric && isDecimal(f.Value) {
		return col + " " + compareSQL[f.Op] + " " + f.Value, true
	}
	return col + "::text " + compareSQL[f.Op] + " " + db.QuoteLiteral(f.Value), true
}

// isDecimal 은 SQL 에 그대로 실어도 되는 십진수인지 본다. ParseFloat 만으로는 Inf·NaN·0x1p4 도 통과해 글자를 제한한다.
func isDecimal(v string) bool {
	if v == "" || strings.Trim(v, "0123456789+-.eE") != "" {
		return false
	}
	_, err := strconv.ParseFloat(v, 64)
	return err == nil
}

// matches 는 결과 행 i 가 조건에 맞는지다. 찾기는 화면 글자(소문자 사본)로, 같음·크기는 SQL 과 같은 규칙
// (숫자 열에 십진수 값이면 숫자, 아니면 화면 글자)으로 비교하고, 같음·크기 비교에서 NULL 칸은 SQL 처럼 맞지 않는다.
func (m *Model) matches(f Filter, i int) bool {
	q := strings.ToLower(f.Value)
	switch f.Op {
	case OpContains, OpNotContains:
		if q == "" {
			return true
		}
		found := false
		for c := 0; !found && c < len(m.folded[i]); c++ {
			found = (f.Col < 0 || c == f.Col) && strings.Contains(m.folded[i][c], q)
		}
		return found == (f.Op == OpContains)
	}
	raw := cellOf(m.res.Rows[i], f.Col)
	switch {
	case f.Op == OpNull:
		return raw == nullText
	case raw == nullText:
		return false
	case f.Op == OpNotNull:
		return true
	}
	shown := display(m.kinds[f.Col], raw)
	var c int
	switch a, errA := strconv.ParseFloat(raw, 64); {
	case m.kinds[f.Col] == kindNumber && isDecimal(f.Value):
		if errA != nil {
			return false
		}
		b, _ := strconv.ParseFloat(f.Value, 64)
		c = cmp.Compare(a, b)
	default:
		c = strings.Compare(shown, f.Value)
	}
	switch f.Op {
	case OpEq:
		return c == 0
	case OpNe:
		return c != 0
	case OpGt:
		return c > 0
	case OpGe:
		return c >= 0
	case OpLt:
		return c < 0
	}
	return c <= 0
}
