// Package tableview 는 DataGrip 의 테이블 데이터 보기처럼 거르기·정렬·페이지를 서버 쿼리로 만든다.
// SQL 실행과 표 그리기는 통합(ui.go)이 기존 실행기와 grid 로 한다. 이 패키지는 상태와 SQL 만 다룬다.
package tableview

import (
	"errors"
	"fmt"
	"slices"
	"strings"
	"unicode/utf8"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/x/ansi"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/grid"
	"github.com/Jeong-Jae-Hun/i-db/internal/textutil"
)

const DefaultPageSize = 200

var (
	errWhere = errors.New("WHERE must be a single expression without a top-level ';'")
	// 한 줄 입력이라 붙여넣은 줄바꿈·탭은 보이는 대로 공백으로 바꾼다
	pasteFlattener = strings.NewReplacer("\r\n", " ", "\n", " ", "\r", " ", "\t", " ")
)

// Options.PageSize 가 0 이하면 DefaultPageSize, db.MaxRows 를 넘으면 db.MaxRows 다.
// 실행기가 1,000행에서 자르므로 그보다 큰 페이지는 끝을 잃는다.
type Options struct{ PageSize int }

// Action 은 키·클릭 처리 뒤 통합이 할 일이다. Requery 면 SQL(), Count 면 CountSQL() 을 실행한다.
// Handled 가 false 면 이 모델이 쓰지 않은 키라 grid 에 넘긴다.
type Action struct {
	Requery bool
	Count   bool
	Handled bool
	Status  string
}

// field 는 열린 입력 줄이다(R16.4).
type field int

const (
	fieldNone field = iota
	fieldWhere
	fieldOrder
)

const (
	barGap   = "   " // WHERE 칸과 ORDER BY 칸 사이
	barEmpty = "(없음)"
)

var fieldHeads = map[field]string{fieldWhere: "WHERE ", fieldOrder: "ORDER BY "}

type sortKey struct {
	col  string
	desc bool
}

// Model 은 값으로 다룬다. orders 는 바꿀 때마다 새 슬라이스를 만들어 이전 Model 과 공유하지 않는다.
type Model struct {
	table    db.TableRef
	pageSize int
	page     int
	where    string
	orders   []sortKey
	orderRaw string // 열 이름·방향으로 읽지 못한 ORDER BY 글(식 등). 있으면 orders 대신 쓴다
	short    string // WHERE 입력의 짧은 꼴(R14.4)에서 바꾼 조건. esc 는 WHERE 가 이것일 때만 지운다
	cols     []string
	numeric  []bool
	total    int64
	known    bool // total 을 안다(추정이든 정확하든)
	exact    bool
	typing   field
	input    string
	// Tab 완성 후보와 바꿀 자리. 다른 키를 누르면 비워 다음 Tab 이 새로 찾는다
	comp    []string
	compAt  int
	compIdx int
}

func New(t db.TableRef, opt Options) Model {
	size := opt.PageSize
	if size <= 0 {
		size = DefaultPageSize
	}
	return Model{table: t, pageSize: min(size, db.MaxRows)}
}

func (m Model) Typing() bool { return m.typing != fieldNone }

// SetColumns 는 지금 테이블의 열 이름·타입이다. Tab 완성, 짧은 꼴 조건, ORDER BY 읽기가 쓴다.
func (m Model) SetColumns(cols, types []string) Model {
	m.cols, m.numeric = cols, make([]bool, len(cols))
	if len(types) == len(cols) {
		for i, t := range types {
			m.numeric[i] = db.IsNumericType(t)
		}
	}
	return m
}

// SetTotal 은 행 수를 알린다. 추정치(reltuples)가 음수면 PostgreSQL 14+ 의 "아직 ANALYZE 안 함"이라 모르는 것으로 둔다.
// 통합은 받은 페이지가 PageSize 보다 짧으면 SetTotal(offset+행 수, true) 로 끝을 알려 다음 페이지를 막을 수 있다.
func (m Model) SetTotal(n int64, exact bool) Model {
	m.total, m.known, m.exact = n, n >= 0, exact
	return m
}

// Page 는 지금 페이지의 OFFSET 과 페이지 크기다. 통합이 짧은 페이지로 끝을 알 때 쓴다.
func (m Model) Page() (offset, size int) { return m.page * m.pageSize, m.pageSize }

func (m Model) SQL() string {
	order := make([]string, len(m.orders))
	for i, o := range m.orders {
		order[i] = db.QuoteIdent(o.col) + " ASC"
		if o.desc {
			order[i] = db.QuoteIdent(o.col) + " DESC"
		}
	}
	s := "SELECT * FROM " + m.relation() + m.whereClause()
	switch {
	case m.orderRaw != "" && strings.Contains(m.orderRaw, "--"):
		s += " ORDER BY " + m.orderRaw + "\n"
	case m.orderRaw != "":
		s += " ORDER BY " + m.orderRaw
	case len(order) > 0:
		s += " ORDER BY " + strings.Join(order, ", ")
	}
	return s + fmt.Sprintf(" LIMIT %d OFFSET %d", m.pageSize, m.page*m.pageSize)
}

func (m Model) CountSQL() string { return "SELECT count(*) FROM " + m.relation() + m.whereClause() }

// EstimateSQL 은 WHERE 와 무관한 테이블 전체 추정치다. 실행기에 바인딩 인자가 없어 regclass 이름을 문자열 리터럴로 싣는다.
func (m Model) EstimateSQL() string {
	return "SELECT reltuples::bigint FROM pg_class WHERE oid = " + db.QuoteLiteral(m.relation()) + "::regclass"
}

func (m Model) relation() string {
	return db.QuoteIdent(m.table.Schema) + "." + db.QuoteIdent(m.table.Table)
}

func (m Model) whereClause() string {
	if m.where == "" {
		return ""
	}
	// 끝의 -- 주석이 뒤에 붙는 ORDER BY·LIMIT 을 삼키지 않도록 줄을 바꾼다
	if strings.Contains(m.where, "--") {
		return " WHERE " + m.where + "\n"
	}
	return " WHERE " + m.where
}

// Update 는 입력 줄이 열려 있으면 모든 키를 먹는다. ctrl+c 같은 전역 키는 통합이 먼저 가로챈다고 본다.
func (m Model) Update(k tea.KeyMsg) (Model, Action) {
	if m.typing != fieldNone {
		if k.Type != tea.KeyTab {
			m.comp = nil
		}
		switch k.Type {
		case tea.KeyRunes:
			m.input += pasteFlattener.Replace(string(k.Runes))
		case tea.KeySpace:
			m.input += " "
		case tea.KeyBackspace:
			_, size := utf8.DecodeLastRuneInString(m.input)
			m.input = m.input[:len(m.input)-size]
		case tea.KeyTab:
			m = m.complete()
		case tea.KeyEsc:
			m.typing, m.input = fieldNone, ""
		case tea.KeyEnter:
			text := strings.TrimSpace(m.input)
			if err := CheckWhere(text); err != nil {
				return m, Action{Handled: true, Status: err.Error()}
			}
			f := m.typing
			m.typing, m.input = fieldNone, ""
			if f == fieldOrder {
				m.orders, m.orderRaw = m.parseOrder(text)
				m.page = 0
				return m, Action{Handled: true, Requery: true}
			}
			cond, short := m.shortCondition(text)
			m = m.withWhere(cond)
			if short {
				m.short = cond
			}
			return m, Action{Handled: true, Requery: true}
		}
		return m, Action{Handled: true}
	}
	// bubbletea 1.3 은 빠르게 친 글자를 KeyRunes 하나로 묶어 보낸다. 글자 키는 한 글자만 키 이름으로 읽는다
	if k.Type == tea.KeyRunes && len(k.Runes) != 1 {
		return m, Action{}
	}
	switch k.String() {
	case "w", "/":
		return m.open(fieldWhere), Action{Handled: true}
	case "O":
		return m.open(fieldOrder), Action{Handled: true}
	case "esc":
		// / 짧은 꼴로 건 거르기만 지운다. 그 밖의 esc 는 결과 표(선택 끝 등)가 쓴다
		if m.short == "" || m.where != m.short {
			return m, Action{}
		}
		return m.withWhere(""), Action{Handled: true, Requery: true}
	case "]":
		if m.known && m.exact && int64((m.page+1)*m.pageSize) >= m.total {
			return m, Action{Handled: true, Status: "마지막 페이지"}
		}
		m.page++
		return m, Action{Handled: true, Requery: true}
	case "[":
		if m.page == 0 {
			return m, Action{Handled: true, Status: "첫 페이지"}
		}
		m.page--
		return m, Action{Handled: true, Requery: true}
	case "#":
		return m, Action{Handled: true, Count: true}
	}
	return m, Action{}
}

// ToggleWhere 는 칸 값 조건(Condition)을 WHERE 에 더하거나 이미 있으면 빼고 다시 읽게 한다(R38).
func (m Model) ToggleWhere(cond string) (Model, Action) {
	return m.withWhere(toggleCondition(m.where, cond)), Action{Handled: true, Requery: true}
}

// withWhere 는 WHERE 를 바꾸고 첫 페이지로 간다. 거르기가 바뀌면 이전 정확한 개수는 맞지 않는다.
// 추정치는 테이블 전체 기준이라 남긴다.
func (m Model) withWhere(where string) Model {
	m.where, m.page, m.short = where, 0, ""
	if m.exact {
		m.known, m.exact = false, false
	}
	return m
}

// CheckWhere 는 WHERE 글이 문장 하나 안의 식인지 본다. 빈 글은 거르기 없음이라 통과다.
// 글은 편집기처럼 사용자가 친 SQL 이라 그대로 넣지만, 따옴표·주석 밖의 ';' 로 두 번째 문장을 붙이지는 못하게 한다.
func CheckWhere(where string) error {
	if strings.TrimSpace(where) == "" {
		return nil
	}
	stmts := db.SplitStatements(where)
	if len(stmts) != 1 || stmts[0] != strings.TrimSpace(where) {
		return errWhere
	}
	return nil
}

// HeaderClick 은 그 열만으로 정렬을 ASC → DESC → 없음 순으로 돌린다. 다른 열 정렬은 지운다.
func (m Model) HeaderClick(col string) (Model, Action) {
	next := cycle(m.orders, col)
	m.orders, m.orderRaw = nil, ""
	if i := indexOf(next, col); i >= 0 {
		m.orders = []sortKey{next[i]}
	}
	m.page = 0
	return m, Action{Handled: true, Requery: true}
}

// HeaderClickAdd 는 다른 열 정렬을 두고 그 열만 순환한다(shift+클릭). 새 열은 뒤에 붙는다.
func (m Model) HeaderClickAdd(col string) (Model, Action) {
	m.orders, m.orderRaw, m.page = cycle(m.orders, col), "", 0
	return m, Action{Handled: true, Requery: true}
}

// cycle 은 orders 를 바꾸지 않고 col 을 한 단계 돌린 새 목록을 돌려준다.
func cycle(orders []sortKey, col string) []sortKey {
	out := slices.Clone(orders)
	i := indexOf(out, col)
	switch {
	case i < 0:
		return append(out, sortKey{col: col})
	case !out[i].desc:
		out[i].desc = true
		return out
	}
	return slices.Delete(out, i, i+1)
}

func indexOf(orders []sortKey, col string) int {
	for i, o := range orders {
		if o.col == col {
			return i
		}
	}
	return -1
}

// View 는 폭 w 한 줄이다. 입력 중이면 WHERE·ORDER BY 입력 줄을, 아니면 WHERE·ORDER BY 칸(R16.4)과 행 범위·테이블·키 도움말을 보여 준다.
func (m Model) View(w int) string {
	if w <= 0 {
		return ""
	}
	if m.typing != fieldNone {
		head, tail := fieldHeads[m.typing], "█  Enter 적용 · Esc 취소 · Tab 열 이름"
		room := w - ansi.StringWidth(head) - ansi.StringWidth(tail)
		in := textutil.Printable(m.input, false)
		// 긴 입력은 커서가 있는 끝이 보이도록 앞을 자른다
		if room > 0 && ansi.StringWidth(in) > room {
			in = ansi.TruncateLeft(in, ansi.StringWidth(in)-room+1, "…")
		}
		return ansi.Truncate(head+in+tail, w, "")
	}
	whereSeg, orderSeg := m.bar(w)
	// 정확한 개수를 알면 끝을 개수로 줄인다. 추정치는 틀릴 수 있어 줄이지 않는다
	from, to := int64(m.page*m.pageSize), int64((m.page+1)*m.pageSize)
	rows := fmt.Sprintf("rows %s–%s", textutil.GroupDigits(from+1), textutil.GroupDigits(to))
	switch {
	case m.known && m.exact && from >= m.total:
		rows = "rows 0 of " + textutil.GroupDigits(m.total)
	case m.known && m.exact:
		rows = fmt.Sprintf("rows %s–%s of %s", textutil.GroupDigits(from+1), textutil.GroupDigits(min(to, m.total)), textutil.GroupDigits(m.total))
	case m.known:
		rows += " of ~" + textutil.GroupDigits(m.total) + " (추정)"
	}
	parts := []string{whereSeg + barGap + orderSeg, rows, textutil.Printable(m.table.Schema+"."+m.table.Table, false),
		"w·/ WHERE · O 정렬 · + - 칸 조건 · [ ] 페이지 · # 개수"}
	return ansi.Truncate(strings.Join(parts, " · "), w, "…")
}

// bar 는 표 위 줄 앞의 WHERE 칸과 ORDER BY 칸이다. 그리기와 클릭(BarClick)이 같은 폭을 쓴다.
// 긴 WHERE 가 ORDER BY 칸을 밀어내지 않게 각 칸 폭을 줄 폭의 반·4분의 1로 자른다.
func (m Model) bar(w int) (string, string) {
	where, order := barEmpty, barEmpty
	if m.where != "" {
		where = m.where
	}
	if t := m.orderText(); t != "" {
		order = t
	}
	return fieldHeads[fieldWhere] + ansi.Truncate(textutil.Printable(where, false), max(8, w/2), "…"),
		fieldHeads[fieldOrder] + ansi.Truncate(textutil.Printable(order, false), max(8, w/4), "…")
}

// BarClick 은 표 위 줄의 x 자리 클릭이다. WHERE 칸이면 WHERE 입력, ORDER BY 칸이면 ORDER BY 입력을 연다.
// 칸 사이 빈칸은 가까운 쪽으로 본다. 입력 중이거나 칸 밖이면 Handled 가 false 다.
func (m Model) BarClick(x, w int) (Model, Action) {
	if m.typing != fieldNone || x < 0 {
		return m, Action{}
	}
	whereSeg, orderSeg := m.bar(w)
	ww := ansi.StringWidth(whereSeg)
	switch {
	case x <= ww:
		return m.open(fieldWhere), Action{Handled: true}
	case x < ww+len(barGap)+ansi.StringWidth(orderSeg):
		return m.open(fieldOrder), Action{Handled: true}
	}
	return m, Action{}
}

// open 은 입력 줄을 지금 값으로 채워 연다. 커서는 끝이다.
func (m Model) open(f field) Model {
	m.typing, m.input, m.comp = f, m.where, nil
	if f == fieldOrder {
		m.input = m.orderText()
	}
	return m
}

// orderText 는 ORDER BY 칸·입력에 보일 글이다. 단순한 소문자 이름은 따옴표 없이 쓴다.
//   - 예: name 오름, "createdAt" 내림 → name, "createdAt" DESC.
func (m Model) orderText() string {
	if m.orderRaw != "" {
		return m.orderRaw
	}
	parts := make([]string, len(m.orders))
	for i, o := range m.orders {
		parts[i] = identText(o.col)
		if o.desc {
			parts[i] += " DESC"
		}
	}
	return strings.Join(parts, ", ")
}

// parseOrder 는 ORDER BY 입력을 열·방향 목록으로 읽는다. 모르는 열·식이 있으면 글 그대로(raw) 둔다.
// 목록으로 읽어야 열 머리 클릭·s 가 그 정렬에서 이어 돈다.
//   - 예: "Name desc, id" → name 내림, id 오름. "lower(name)" → raw.
func (m Model) parseOrder(text string) ([]sortKey, string) {
	if text == "" {
		return nil, ""
	}
	var keys []sortKey
	for _, item := range strings.Split(text, ",") {
		f := strings.Fields(item)
		if len(f) == 0 || len(f) > 2 {
			return nil, text
		}
		col, ok := m.column(f[0])
		if !ok {
			return nil, text
		}
		k := sortKey{col: col}
		if len(f) == 2 {
			switch strings.ToLower(f[1]) {
			case "asc":
			case "desc":
				k.desc = true
			default:
				return nil, text
			}
		}
		keys = append(keys, k)
	}
	return keys, ""
}

// column 은 입력한 이름이 가리키는 열이다. "따옴표 이름"은 그대로, 아니면 대소문자를 무시해 찾는다.
func (m Model) column(tok string) (string, bool) {
	if len(tok) >= 2 && tok[0] == '"' && tok[len(tok)-1] == '"' {
		name := strings.ReplaceAll(tok[1:len(tok)-1], `""`, `"`)
		return name, slices.Contains(m.cols, name)
	}
	for _, c := range m.cols {
		if strings.EqualFold(c, tok) {
			return c, true
		}
	}
	return "", false
}

// shortCondition 은 WHERE 입력이 R14.4 짧은 꼴(아는 열 + 한 낱말 값)이면 SQL 조건으로 바꾼다. 아니면 SQL 그대로다.
// SQL 로도 읽히는 글(id = 1 AND …, name = 'x', a<>b, a::text, id = other_col)을 짧은 꼴로 잘못 바꾸지 않도록
// 값에 따옴표·괄호가 있거나, 연산자 글자로 시작하거나, 공백이 있거나(: 제외), 열 이름이면 SQL 로 둔다.
//   - 예: status=ACTIVE → "status"::text = 'ACTIVE'. status = 'ACTIVE' → 그대로.
func (m Model) shortCondition(text string) (string, bool) {
	f := grid.ParseFilter(text, m.cols)
	if f.Col < 0 || strings.ContainsAny(f.Value, `'"()`) || strings.IndexAny(f.Value, "=<>!:~") == 0 {
		return text, false
	}
	if _, isCol := m.column(f.Value); f.Op != grid.OpContains && (isCol || strings.ContainsAny(f.Value, " \t")) {
		return text, false
	}
	cond, _ := f.SQL(m.numeric[f.Col])
	return cond, true
}

// complete 는 커서(끝) 앞 낱말을 열 이름(ORDER BY 는 asc·desc 도)으로 채운다. 대소문자를 무시한 앞부분 일치이고
// 다시 Tab 이면 다음 후보로 돈다.
func (m Model) complete() Model {
	if m.comp == nil {
		start := len(m.input)
		for start > 0 && identByte(m.input[start-1]) {
			start--
		}
		if start > 0 && m.input[start-1] == '"' {
			start--
		}
		prefix := strings.ToLower(strings.TrimPrefix(m.input[start:], `"`))
		words := m.cols
		if m.typing == fieldOrder {
			words = append(slices.Clone(m.cols), "asc", "desc")
		}
		for _, w := range words {
			if strings.HasPrefix(strings.ToLower(w), prefix) {
				m.comp = append(m.comp, identText(w))
			}
		}
		if m.comp == nil {
			return m
		}
		m.compAt, m.compIdx = start, 0
	} else {
		m.compIdx = (m.compIdx + 1) % len(m.comp)
	}
	m.input = m.input[:m.compAt] + m.comp[m.compIdx]
	return m
}

// identText 는 따옴표 없이 써도 PostgreSQL 이 같은 이름으로 읽는 이름(소문자·숫자·_)은 그대로, 아니면 QuoteIdent 다.
func identText(name string) string {
	for i := 0; i < len(name); i++ {
		c := name[i]
		if !(c == '_' || ('a' <= c && c <= 'z') || (i > 0 && '0' <= c && c <= '9')) {
			return db.QuoteIdent(name)
		}
	}
	if name == "" {
		return db.QuoteIdent(name)
	}
	return name
}
