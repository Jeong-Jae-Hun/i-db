package grid

// ─── 거르기 식 (R14.4) ───

import (
	"cmp"
	"slices"
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

// ParseFilter 는 거르기 글을 조건으로 읽는다. 열 이름은 대소문자까지 같은 열을 먼저, 없으면 대소문자를 무시해 찾고,
// 모르는 열이면 글 전체를 모든 열 찾기로 본다.
//   - 예: "status=ACTIVE" → status 열 OpEq "ACTIVE". "email=null" → OpNull.
//   - 예: "!abc" → 모든 열 OpNotContains "abc". "http://x" → 열 http 가 없으면 모든 열 OpContains.
func ParseFilter(expr string, cols []string) Filter {
	if i, tok := filterToken(expr); i > 0 {
		name := strings.TrimSpace(expr[:i])
		c := slices.Index(cols, name)
		if c < 0 {
			c = slices.IndexFunc(cols, func(col string) bool { return strings.EqualFold(col, name) })
		}
		if c >= 0 {
			f := Filter{Col: c, Name: cols[c], Op: filterTokens[tok], Value: strings.TrimSpace(expr[i+len(tok):])}
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
	if numeric && IsDecimal(f.Value) {
		return col + " " + compareSQL[f.Op] + " " + f.Value, true
	}
	return col + "::text " + compareSQL[f.Op] + " " + db.QuoteLiteral(f.Value), true
}

// IsDecimal 은 SQL 에 그대로 실어도 되는 십진수인지 본다. ParseFloat 만으로는 Inf·NaN·0x1p4 도 통과해 글자를 제한한다.
func IsDecimal(v string) bool {
	if v == "" || strings.Trim(v, "0123456789+-.eE") != "" {
		return false
	}
	_, err := strconv.ParseFloat(v, 64)
	return err == nil
}

// matches 는 결과 행 i 가 조건에 맞는지다. 찾기는 화면 글자(소문자 사본)로, 같음·크기는 SQL 과 같은 규칙
// (숫자 열에 십진수 값이면 숫자, 아니면 화면 글자)으로 비교하고, 같음·크기 비교에서 NULL 칸은 SQL 처럼 맞지 않는다.
// 서버 SQL 과 다른 점: 숫자 비교는 float64 라 15자리를 넘는 numeric·bigint 는 근사이고, 글자 크기 비교는
// 데이터베이스 정렬 규칙(collation)이 아니라 바이트 순서다. 찾기는 화면 글자라 서버의 ::text 와 꼴이 다를 수 있다.
func (m *Model) matches(f Filter, i int) bool {
	q := strings.ToLower(f.Value)
	switch f.Op {
	case OpContains, OpNotContains:
		if q == "" {
			// col: 은 SQL ILIKE '%%' 처럼 NULL 칸을 뺀다. 모든 열 찾기의 빈 글은 거르기 없음이다
			return f.Col < 0 || cellOf(m.res.Rows[i], f.Col) != nullText
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
	case m.kinds[f.Col] == kindNumber && IsDecimal(f.Value):
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

// ─── 서버로 다시 읽기 (R14.5) ───

// Query 는 결과 표의 정렬·거르기다. 서버에서 다시 읽은 결과에 되돌려 붙인다.
type Query struct {
	SortCol string // 정렬 열 이름. 비면 정렬 없음
	Desc    bool
	Filter  string
}

// Query 는 지금 정렬·거르기다.
func (m Model) Query() Query {
	q := Query{Filter: m.filter}
	if m.res != nil && m.sortDir != sortNone && m.sortCol < len(m.res.Cols) {
		q.SortCol, q.Desc = m.res.Cols[m.sortCol], m.sortDir == sortDesc
	}
	return q
}

// SetQuery 는 정렬·거르기를 q 로 둔다. 열 이름이 결과에 없으면 정렬은 없다.
// server 면 행이 이미 서버에서 그렇게 읽은 것이라 받은 행 거르기·정렬을 다시 하지 않는다.
func (m *Model) SetQuery(q Query, server bool) {
	if m.res == nil {
		return
	}
	m.filter, m.sortDir, m.server = q.Filter, sortNone, server
	if i := slices.Index(m.res.Cols, q.SortCol); q.SortCol != "" && i >= 0 {
		m.sortCol, m.sortDir = i, sortAsc
		if q.Desc {
			m.sortDir = sortDesc
		}
	}
	m.rebuild()
}

// FilterTyping 은 거르기 글을 치는 중인지다. 치는 동안은 받은 행 안에서만 거르고 확정 때 서버로 다시 읽는다.
func (m Model) FilterTyping() bool { return m.typing }

// SetFilterInput 은 치는 중인 거르기 글을 바꾼다(R75.5 이전 거르기). 치는 중이 아니면 하지 않는다.
func (m *Model) SetFilterInput(f string) {
	if !m.typing {
		return
	}
	m.filter = f
	m.rebuild()
}

// ResumeTyping 은 다시 읽은 결과가 오는 사이 치던 거르기 입력을 이어 간다.
func (m *Model) ResumeTyping() { m.typing = true }

// UniqueCols 는 열 이름이 겹치지 않는지다. 겹치면 서브쿼리 바깥에서 열을 가리킬 수 없어 서버로 다시 읽지 않는다.
func (m Model) UniqueCols() bool {
	if m.res == nil {
		return false
	}
	// _q 열이 있으면 모든 열 찾기의 _q::text 가 행이 아닌 그 열을 가리킨다
	seen := map[string]bool{"_q": true}
	for _, c := range m.res.Cols {
		if seen[c] {
			return false
		}
		seen[c] = true
	}
	return true
}

// ServerSQL 은 읽기 문장 base 를 감싸 지금 정렬·거르기를 서버에서 하는 SELECT 다. 둘 다 없으면 base 그대로다.
// base 끝의 줄 주석이 닫는 괄호를 삼키지 않게 괄호 앞에 줄을 바꾼다. NULL 은 받은 행 정렬(R14.3)처럼 늘 뒤다.
//   - 예: SELECT * FROM (\nselect …\n) AS _q WHERE "status"::text = 'PAID' ORDER BY "amount" DESC NULLS LAST
func (m Model) ServerSQL(base string) string {
	where, order := m.serverClauses()
	if where == "" && order == "" {
		return base
	}
	return wrapSQL(base, where, order)
}

// PageSQL 은 ServerSQL 과 같은 정렬·거르기로 offset 행 뒤부터 읽는 SELECT 다(R14.7 더 읽기). 정렬·거르기가 없어도 감싼다.
//   - 예: SELECT * FROM (\nselect …\n) AS _q ORDER BY "id" ASC NULLS LAST OFFSET 1000
func (m Model) PageSQL(base string, offset int) string {
	where, order := m.serverClauses()
	return wrapSQL(base, where, order) + " OFFSET " + strconv.Itoa(offset)
}

func (m Model) serverClauses() (where, order string) {
	if m.res == nil {
		return "", ""
	}
	f := ParseFilter(m.filter, m.res.Cols)
	switch {
	case f.Col >= 0:
		where, _ = f.SQL(m.kinds[f.Col] == kindNumber)
	case f.Value == "":
	case f.Op == OpNotContains:
		where = "NOT (_q::text ILIKE " + db.QuoteLiteral("%"+likeEscaper.Replace(f.Value)+"%") + ")"
	default:
		where = "_q::text ILIKE " + db.QuoteLiteral("%"+likeEscaper.Replace(f.Value)+"%")
	}
	if q := m.Query(); q.SortCol != "" {
		dir := "ASC"
		if q.Desc {
			dir = "DESC"
		}
		order = db.QuoteIdent(q.SortCol) + " " + dir + " NULLS LAST"
	}
	return where, order
}

func wrapSQL(base, where, order string) string {
	s := "SELECT * FROM (\n" + base + "\n) AS _q"
	if where != "" {
		s += " WHERE " + where
	}
	if order != "" {
		s += " ORDER BY " + order
	}
	return s
}

// Extend 는 지금 결과에 행을 덧붙인 결과 r 로 바꾼다(R14.7 더 읽기). SetResult 와 달리 정렬·거르기·커서·열 폭을 둔다.
// 추가 행 자리(len(Rows) 뒤)가 밀리므로 통합이 Pending() 이 0 일 때만 부르고, 남은 빈 편집 기록은 비운다.
func (m *Model) Extend(r *db.Result) {
	if m.res == nil || r == nil || len(r.Cols) != len(m.res.Cols) {
		return
	}
	m.res, m.folded = r, nil
	m.edits, m.inserts, m.log = map[int]*rowEdit{}, 0, map[int][]snap{}
	m.rebuild()
	m.reflow()
	m.follow()
}
