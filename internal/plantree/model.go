package plantree

import (
	"cmp"
	"fmt"
	"math"
	"slices"
	"strconv"
	"strings"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
	"github.com/charmbracelet/x/ansi"

	"github.com/Jeong-Jae-Hun/i-db/internal/textutil"
)

const (
	topRanks        = 3  // 색으로 강조할 비싼 노드 수
	detailMinHeight = 10 // 전체 높이가 이보다 작으면 상세 칸 없이 나무만 그린다
	detailMaxHeight = 10 // 구분선 포함 상세 칸 최대 줄 수
	barCells        = 8
)

// 밝은·어두운 터미널 배경 모두에서 읽히도록 색을 둘씩 둔다(grid 와 같은 256색 표)
var (
	colDim  = lipgloss.AdaptiveColor{Light: "245", Dark: "244"}
	colHead = lipgloss.AdaptiveColor{Light: "235", Dark: "253"}
	colWarn = lipgloss.AdaptiveColor{Light: "130", Dark: "214"}

	dimStyle  = lipgloss.NewStyle().Foreground(colDim)
	headStyle = lipgloss.NewStyle().Foreground(colHead).Bold(true)
	warnStyle = lipgloss.NewStyle().Foreground(colWarn)
	selStyle  = lipgloss.NewStyle().Reverse(true)
	// 1·2·3위 순서. 빨강 → 주황 → 노랑
	rankStyles = []lipgloss.Style{
		lipgloss.NewStyle().Foreground(lipgloss.AdaptiveColor{Light: "160", Dark: "203"}).Bold(true),
		lipgloss.NewStyle().Foreground(lipgloss.AdaptiveColor{Light: "166", Dark: "209"}),
		lipgloss.NewStyle().Foreground(lipgloss.AdaptiveColor{Light: "136", Dark: "221"}),
	}
)

// Action 은 키 처리 뒤 통합(ui.go)이 할 일이다. Close 면 창을 닫는다.
type Action struct{ Close bool }

// Model 은 상태 바꾸기(Update·SetSize)에서 스크롤 위치를 맞추고, View 는 그리기만 한다.
type Model struct {
	root      *Node
	rows      []row // 접히지 않아 보이는 노드
	collapsed map[*Node]bool
	rank      map[*Node]int // 비싼 순서 0..topRanks-1
	cur, top  int
	w, h      int
}

// treeCells 는 나무 한 줄의 칸이다. 열 폭을 맞추려고 그리기 전에 모은다.
type treeCells struct{ name, bar, cost, rows string }

type row struct {
	n     *Node
	depth int
}

func New(root *Node) Model {
	m := Model{root: root, collapsed: map[*Node]bool{}, rank: map[*Node]int{}}
	if root == nil {
		return m
	}
	nodes := flatten(root)
	slices.SortStableFunc(nodes, func(a, b *Node) int { return cmp.Compare(b.Self(), a.Self()) })
	for i, n := range nodes[:min(topRanks, len(nodes))] {
		if n.Self() > 0 {
			m.rank[n] = i
		}
	}
	m.rebuild()
	return m
}

func (m *Model) SetSize(w, h int) {
	m.w, m.h = max(0, w), max(0, h)
	m.follow()
}

// Selected 는 고른 노드다. 나무가 없으면 nil 이다.
func (m Model) Selected() *Node {
	if len(m.rows) == 0 {
		return nil
	}
	return m.rows[m.cur].n
}

func (m Model) Update(k tea.KeyMsg) (Model, Action) {
	if k.String() == "esc" || k.String() == "q" {
		return m, Action{Close: true}
	}
	if len(m.rows) == 0 {
		return m, Action{}
	}
	treeH, _ := m.layout()
	r := m.rows[m.cur]
	switch k.String() {
	case "up", "k":
		m.cur--
	case "down", "j":
		m.cur++
	case "pgup":
		m.cur -= treeH
	case "pgdown":
		m.cur += treeH
	case "home", "g":
		m.cur = 0
	case "end", "G":
		m.cur = len(m.rows) - 1
	case "enter", " ":
		if len(r.n.Children) > 0 {
			m.collapsed[r.n] = !m.collapsed[r.n]
			m.rebuild()
		}
	case "left", "h":
		if len(r.n.Children) > 0 && !m.collapsed[r.n] {
			m.collapsed[r.n] = true
			m.rebuild()
			break
		}
		for i := m.cur - 1; i >= 0; i-- {
			if m.rows[i].depth < r.depth {
				m.cur = i
				break
			}
		}
	case "right", "l":
		switch {
		case m.collapsed[r.n]:
			m.collapsed[r.n] = false
			m.rebuild()
		case len(r.n.Children) > 0:
			m.cur++
		}
	}
	m.follow()
	return m, Action{}
}

// View 는 w×h 안에 머리 줄·나무·상세 칸을 그린다. 줄 수는 h, 줄 폭은 w 를 넘지 않는다.
func (m Model) View() string {
	if m.root == nil || m.w <= 0 || m.h <= 0 {
		return ""
	}
	treeH, detailH := m.layout()
	var lines []string
	if m.h >= 2 {
		head := fmt.Sprintf("total cost %.2f (ANALYZE 없음: 비용 기준) · enter 펼침 · esc 닫기", m.root.TotalCost)
		if m.root.Analyzed {
			head = fmt.Sprintf("planning %s · execution %s · enter 펼침 · esc 닫기", ms(m.root.PlanningTime), ms(m.root.ExecutionTime))
		}
		lines = append(lines, dimStyle.Render(head))
	}
	// 이름·비용 열을 보이는 줄 안에서 맞춘다. 이름이 길면 화면 폭 60% 에서 자른다
	end := min(len(m.rows), m.top+treeH)
	cells := make([]treeCells, 0, end-m.top)
	nameW, costW := 0, 0
	for _, r := range m.rows[m.top:end] {
		marker := "  "
		switch {
		case len(r.n.Children) > 0 && m.collapsed[r.n]:
			marker = "▸ "
		case len(r.n.Children) > 0:
			marker = "▾ "
		}
		c := treeCells{name: strings.Repeat("  ", r.depth) + marker + label(r.n)}
		filled := int(math.Round(r.n.Percent / 100 * barCells))
		c.bar = strings.Repeat("█", filled) + strings.Repeat("·", barCells-filled)
		switch {
		case r.n.Analyzed && r.n.Loops == 0:
			c.cost = "never executed"
		case r.n.Analyzed:
			c.cost = fmt.Sprintf("%s %5.1f%%", ms(r.n.SelfTime), r.n.Percent)
			c.rows = fmt.Sprintf("rows %s/%s", num(r.n.ActualRows), num(r.n.PlanRows))
		default:
			c.cost = fmt.Sprintf("cost %.2f %5.1f%%", r.n.SelfCost, r.n.Percent)
			c.rows = "rows " + num(r.n.PlanRows)
		}
		nameW, costW = max(nameW, ansi.StringWidth(c.name)), max(costW, ansi.StringWidth(c.cost))
		cells = append(cells, c)
	}
	nameW = min(nameW, max(8, m.w*6/10))
	for i, c := range cells {
		r := m.rows[m.top+i]
		name := ansi.Truncate(c.name, nameW, "…")
		name += strings.Repeat(" ", nameW-ansi.StringWidth(name))
		cost := strings.Repeat(" ", costW-ansi.StringWidth(c.cost)) + c.cost
		mark := r.n.MisestimateMark()
		if m.top+i == m.cur {
			line := strings.Join([]string{name, c.bar, cost, c.rows, mark}, "  ")
			lines = append(lines, selStyle.Render(ansi.Truncate(line, m.w, "…")))
			continue
		}
		bar := dimStyle.Render(c.bar)
		if rk, ranked := m.rank[r.n]; ranked {
			bar, cost = rankStyles[rk].Render(c.bar), rankStyles[rk].Render(cost)
		}
		lines = append(lines, strings.Join([]string{name, bar, cost, dimStyle.Render(c.rows), warnStyle.Render(mark)}, "  "))
	}
	if detailH > 0 {
		for len(lines) < m.h-detailH {
			lines = append(lines, "")
		}
		lines = append(lines, dimStyle.Render(strings.Repeat("─", m.w)))
		d := details(m.rows[m.cur].n)
		d[0] = headStyle.Render(d[0])
		lines = append(lines, d[:min(len(d), detailH-1)]...)
	}
	for i, ln := range lines {
		lines[i] = ansi.Truncate(ln, m.w, "…")
	}
	return strings.Join(lines[:min(len(lines), m.h)], "\n")
}

// rebuild 는 접힘이 바뀐 뒤 보이는 줄을 다시 만들고 고른 노드를 따라간다.
func (m *Model) rebuild() {
	sel := m.Selected()
	m.rows = m.rows[:0]
	m.appendRows(m.root, 0)
	for i, r := range m.rows {
		if r.n == sel {
			m.cur = i
		}
	}
	m.follow()
}

func (m *Model) appendRows(n *Node, depth int) {
	m.rows = append(m.rows, row{n, depth})
	if m.collapsed[n] {
		return
	}
	for _, c := range n.Children {
		m.appendRows(c, depth+1)
	}
}

func (m *Model) follow() {
	m.cur = max(0, min(m.cur, len(m.rows)-1))
	treeH, _ := m.layout()
	if m.cur < m.top {
		m.top = m.cur
	}
	if m.cur >= m.top+treeH {
		m.top = m.cur - treeH + 1
	}
	m.top = max(0, min(m.top, len(m.rows)-treeH))
}

// layout 은 나무 줄 수와 상세 칸 줄 수(구분선 포함)다. 머리 줄은 높이 2 이상일 때 한 줄이다.
func (m Model) layout() (treeH, detailH int) {
	head := 0
	if m.h >= 2 {
		head = 1
	}
	if m.h >= detailMinHeight {
		detailH = min(detailMaxHeight, m.h/2)
	}
	return max(1, m.h-head-detailH), detailH
}

// label 은 노드 한 줄 이름이다. 예: "CTE big: Hash Join (Left) on users u using users_pkey".
func label(n *Node) string {
	var b strings.Builder
	if n.SubplanName != "" {
		b.WriteString(n.SubplanName + ": ")
	}
	b.WriteString(n.Type)
	if n.JoinType != "" && n.JoinType != "Inner" {
		b.WriteString(" (" + n.JoinType + ")")
	}
	if n.Relation != "" {
		b.WriteString(" on " + n.Relation)
		if n.Alias != "" && n.Alias != n.Relation {
			b.WriteString(" " + n.Alias)
		}
	} else if n.CTEName != "" {
		b.WriteString(" on " + n.CTEName)
	}
	if n.Index != "" {
		b.WriteString(" using " + n.Index)
	}
	return textutil.Printable(b.String(), false)
}

// details 는 상세 칸 줄이다. 첫 줄은 label 이고 나머지는 값이 있는 항목만 싣는다.
func details(n *Node) []string {
	lines := []string{label(n), fmt.Sprintf("cost %.2f..%.2f · rows %s · width %d", n.StartupCost, n.TotalCost, num(n.PlanRows), n.PlanWidth)}
	switch {
	case n.Analyzed && n.Loops == 0:
		lines = append(lines, "never executed")
	case n.Analyzed:
		lines = append(lines, fmt.Sprintf("actual %s..%s × %s loops · rows %s · self %s (%.1f%%)",
			ms(n.ActualStart), ms(n.ActualTime), num(n.Loops), num(n.ActualRows), ms(n.SelfTime), n.Percent))
		if mark := n.MisestimateMark(); mark != "" {
			lines = append(lines, fmt.Sprintf("row estimate off %s (actual %s, plan %s)", mark, num(n.ActualRows), num(n.PlanRows)))
		}
	default:
		lines = append(lines, fmt.Sprintf("self cost %.2f (%.1f%%)", n.SelfCost, n.Percent))
	}
	if n.Buffers != (Buffers{}) {
		s := fmt.Sprintf("buffers shared hit %d read %d dirtied %d written %d", n.SharedHit, n.SharedRead, n.SharedDirtied, n.SharedWritten)
		if n.TempRead > 0 || n.TempWritten > 0 {
			s += fmt.Sprintf(" · temp read %d written %d", n.TempRead, n.TempWritten)
		}
		lines = append(lines, s)
	}
	if n.WorkersPlanned > 0 {
		lines = append(lines, fmt.Sprintf("workers %d planned · %d launched", n.WorkersPlanned, n.WorkersLaunched))
	}
	for _, kv := range []struct{ k, v string }{
		{"Sort Key", strings.Join(n.SortKey, ", ")}, {"Group Key", strings.Join(n.GroupKey, ", ")},
		{"Index Cond", n.IndexCond}, {"Recheck Cond", n.RecheckCond}, {"Hash Cond", n.HashCond}, {"Merge Cond", n.MergeCond},
		{"Join Filter", n.JoinFilter}, {"Filter", n.Filter},
	} {
		if kv.v != "" {
			lines = append(lines, kv.k+": "+textutil.Printable(kv.v, false))
		}
	}
	for _, kv := range []struct {
		k string
		v float64
	}{{"Rows Removed by Index Recheck", n.RemovedRecheck}, {"Rows Removed by Join Filter", n.RemovedByJoin}, {"Rows Removed by Filter", n.RemovedByFilter}} {
		if kv.v > 0 {
			lines = append(lines, kv.k+": "+num(kv.v))
		}
	}
	return lines
}

func ms(v float64) string {
	switch {
	case v < 1:
		return fmt.Sprintf("%.3fms", v)
	case v < 1000:
		return fmt.Sprintf("%.1fms", v)
	}
	return fmt.Sprintf("%.2fs", v/1000)
}

func num(v float64) string { return strconv.FormatFloat(v, 'f', -1, 64) }
