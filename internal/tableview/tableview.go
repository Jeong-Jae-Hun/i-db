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
	total    int64
	known    bool // total 을 안다(추정이든 정확하든)
	exact    bool
	typing   bool
	input    string
}

func New(t db.TableRef, opt Options) Model {
	size := opt.PageSize
	if size <= 0 {
		size = DefaultPageSize
	}
	return Model{table: t, pageSize: min(size, db.MaxRows)}
}

func (m Model) Typing() bool { return m.typing }

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
	if len(order) > 0 {
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
	if m.typing {
		switch k.Type {
		case tea.KeyRunes:
			m.input += pasteFlattener.Replace(string(k.Runes))
		case tea.KeySpace:
			m.input += " "
		case tea.KeyBackspace:
			_, size := utf8.DecodeLastRuneInString(m.input)
			m.input = m.input[:len(m.input)-size]
		case tea.KeyEsc:
			m.typing, m.input = false, ""
		case tea.KeyEnter:
			where := strings.TrimSpace(m.input)
			if err := CheckWhere(where); err != nil {
				return m, Action{Handled: true, Status: err.Error()}
			}
			m.typing, m.input = false, ""
			return m.withWhere(where), Action{Handled: true, Requery: true}
		}
		return m, Action{Handled: true}
	}
	// bubbletea 1.3 은 빠르게 친 글자를 KeyRunes 하나로 묶어 보낸다. 글자 키는 한 글자만 키 이름으로 읽는다
	if k.Type == tea.KeyRunes && len(k.Runes) != 1 {
		return m, Action{}
	}
	switch k.String() {
	case "w":
		m.typing, m.input = true, m.where
		return m, Action{Handled: true}
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
	m.where, m.page = where, 0
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
	m.orders = nil
	if i := indexOf(next, col); i >= 0 {
		m.orders = []sortKey{next[i]}
	}
	m.page = 0
	return m, Action{Handled: true, Requery: true}
}

// HeaderClickAdd 는 다른 열 정렬을 두고 그 열만 순환한다(shift+클릭). 새 열은 뒤에 붙는다.
func (m Model) HeaderClickAdd(col string) (Model, Action) {
	m.orders, m.page = cycle(m.orders, col), 0
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

// View 는 폭 w 한 줄이다. 입력 중이면 WHERE 입력 줄을, 아니면 테이블·거르기·정렬·행 범위·키 도움말을 보여 준다.
func (m Model) View(w int) string {
	if w <= 0 {
		return ""
	}
	if m.typing {
		head, tail := "WHERE ", "█  Enter 적용 · Esc 취소 · 빈 값은 해제"
		room := w - ansi.StringWidth(head) - ansi.StringWidth(tail)
		in := textutil.Printable(m.input, false)
		// 긴 입력은 커서가 있는 끝이 보이도록 앞을 자른다
		if room > 0 && ansi.StringWidth(in) > room {
			in = ansi.TruncateLeft(in, ansi.StringWidth(in)-room+1, "…")
		}
		return ansi.Truncate(head+in+tail, w, "")
	}
	where := "WHERE 없음"
	if m.where != "" {
		where = "WHERE " + m.where
	}
	parts := []string{textutil.Printable(m.table.Schema+"."+m.table.Table, false), textutil.Printable(where, false)}
	if len(m.orders) > 0 {
		order := make([]string, len(m.orders))
		for i, o := range m.orders {
			order[i] = o.col + "↑"
			if o.desc {
				order[i] = o.col + "↓"
			}
		}
		parts = append(parts, textutil.Printable("ORDER BY "+strings.Join(order, ", "), false))
	}
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
	parts = append(parts, rows, "w 거르기 · + - 칸 조건 · [ ] 페이지 · # 개수")
	return ansi.Truncate(strings.Join(parts, " · "), w, "…")
}
