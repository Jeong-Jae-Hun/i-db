package finder

import (
	"fmt"
	"slices"
	"strings"
	"unicode"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
	"github.com/charmbracelet/x/ansi"

	"github.com/Jeong-Jae-Hun/i-db/internal/textutil"
)

const maxQueryRunes = 200 // 붙여넣기로 긴 글이 들어와도 키 입력마다 훑는 비용이 커지지 않게 자른다

// 밝은·어두운 터미널 배경 모두에서 읽히도록 색을 둘씩 둔다(grid 와 같은 256색 표)
var (
	colDim    = lipgloss.AdaptiveColor{Light: "245", Dark: "244"}
	colAccent = lipgloss.AdaptiveColor{Light: "25", Dark: "117"}

	dimStyle    = lipgloss.NewStyle().Foreground(colDim)
	matchStyle  = lipgloss.NewStyle().Foreground(colAccent).Bold(true)
	selStyle    = lipgloss.NewStyle().Reverse(true)
	borderStyle = lipgloss.NewStyle().Border(lipgloss.RoundedBorder()).BorderForeground(colDim)
	kindTags    = map[string]string{KindTable: "T", KindView: "V", KindFunction: "F", KindRedisKey: "K", KindColumn: "C"}
)

// Action 은 키 처리 뒤 통합(ui.go)이 할 일이다. Chosen 이 있으면 그 객체를 열고 창을 닫는다.
type Action struct {
	Chosen *Item
	Close  bool
}

// Model 은 입력 한 줄과 후보 목록이다. 글자가 바뀔 때마다 Match 를 다시 돌린다.
type Model struct {
	items    []Item
	query    []rune
	hits     []Hit
	cur, top int
	w, h     int
}

func New(items []Item) Model {
	return Model{items: items, hits: Match(items, "")}
}

func (m *Model) SetSize(w, h int) {
	m.w, m.h = max(0, w), max(0, h)
	m.follow()
}

func (m Model) Query() string { return string(m.query) }

func (m Model) Update(k tea.KeyMsg) (Model, Action) {
	before := len(m.query)
	// 값으로 넘긴 이전 Model 과 배열을 나눠 쓰면 append 가 그쪽 글을 덮을 수 있어 늘 새 배열에 붙인다
	m.query = slices.Clip(m.query)
	switch k.Type {
	case tea.KeyEsc:
		return m, Action{Close: true}
	case tea.KeyEnter:
		if len(m.hits) == 0 {
			return m, Action{}
		}
		it := m.hits[m.cur].Item
		return m, Action{Chosen: &it, Close: true}
	case tea.KeyUp, tea.KeyCtrlP:
		m.cur--
	case tea.KeyDown, tea.KeyCtrlN:
		m.cur++
	case tea.KeyPgUp:
		m.cur -= m.listHeight()
	case tea.KeyPgDown:
		m.cur += m.listHeight()
	case tea.KeyBackspace:
		if len(m.query) > 0 {
			m.query = m.query[:len(m.query)-1]
		}
	case tea.KeyCtrlU:
		m.query = nil
	case tea.KeySpace:
		m.query = append(m.query, ' ')
	case tea.KeyRunes:
		// 붙여넣은 줄바꿈·탭은 한 줄 입력에서 보이지 않으므로 공백으로, 나머지 제어 문자는 버린다
		for _, r := range k.Runes {
			switch {
			case unicode.IsSpace(r):
				m.query = append(m.query, ' ')
			case !unicode.IsControl(r):
				m.query = append(m.query, r)
			}
		}
	}
	m.query = m.query[:min(len(m.query), maxQueryRunes)]
	if len(m.query) != before || k.Type == tea.KeyCtrlU {
		m.hits = Match(m.items, string(m.query))
		m.cur, m.top = 0, 0
	}
	m.follow()
	return m, Action{}
}

// View 는 w×h 상자(테두리 포함) 안에 입력 줄과 후보를 그린다. 너무 작으면 테두리 없이 그린다.
func (m Model) View() string {
	if m.w <= 0 || m.h <= 0 {
		return ""
	}
	bordered := m.w >= 4 && m.h >= 3
	iw := m.w
	if bordered {
		iw -= 2
	}
	count := dimStyle.Render(fmt.Sprintf("%d/%d", len(m.hits), len(m.items)))
	input := ansi.Truncate("> "+textutil.Printable(string(m.query), false)+"▏", max(0, iw-ansi.StringWidth(count)-1), "…")
	lines := []string{input + strings.Repeat(" ", max(1, iw-ansi.StringWidth(input)-ansi.StringWidth(count))) + count}
	listH := m.listHeight()
	for i := m.top; i < len(m.hits) && i < m.top+listH; i++ {
		h := m.hits[i]
		tag := kindTags[h.Item.Kind]
		if tag == "" {
			tag = "?"
		}
		var plain, styled strings.Builder
		pi := 0
		for ri, r := range []rune(Target(h.Item)) {
			s := textutil.Printable(string(r), false)
			plain.WriteString(s)
			if pi < len(h.Pos) && h.Pos[pi] == ri {
				styled.WriteString(matchStyle.Render(s))
				pi++
			} else {
				styled.WriteString(s)
			}
		}
		conn := textutil.Printable(h.Item.Conn, false)
		connW := ansi.StringWidth(conn)
		// 접속 이름이 폭의 1/3 을 넘으면 객체 이름을 살리려고 뺀다
		if connW > iw/3 {
			conn, connW = "", 0
		}
		nameW := max(0, iw-2-connW-1)
		if i == m.cur {
			name := ansi.Truncate(tag+" "+plain.String(), nameW+2, "…")
			line := name + strings.Repeat(" ", max(1, iw-ansi.StringWidth(name)-connW)) + conn
			lines = append(lines, selStyle.Render(ansi.Truncate(line, iw, "")))
			continue
		}
		name := ansi.Truncate(dimStyle.Render(tag)+" "+styled.String(), nameW+2, "…")
		lines = append(lines, name+strings.Repeat(" ", max(1, iw-ansi.StringWidth(name)-connW))+dimStyle.Render(conn))
	}
	if len(m.hits) == 0 && listH > 0 {
		lines = append(lines, dimStyle.Render("일치하는 객체 없음"))
	}
	if !bordered {
		for i, ln := range lines {
			lines[i] = ansi.Truncate(ln, m.w, "")
		}
		return strings.Join(lines[:min(len(lines), m.h)], "\n")
	}
	for i, ln := range lines {
		lines[i] = ansi.Truncate(ln, iw, "")
	}
	for len(lines) < m.h-2 {
		lines = append(lines, "")
	}
	return borderStyle.Width(iw).Render(strings.Join(lines[:m.h-2], "\n"))
}

// listHeight 는 후보 줄 수다. 입력 줄 하나와 테두리를 뺀다.
func (m Model) listHeight() int {
	if m.w >= 4 && m.h >= 3 {
		return max(0, m.h-3)
	}
	return max(0, m.h-1)
}

func (m *Model) follow() {
	m.cur = max(0, min(m.cur, len(m.hits)-1))
	listH := max(1, m.listHeight())
	if m.cur < m.top {
		m.top = m.cur
	}
	if m.cur >= m.top+listH {
		m.top = m.cur - listH + 1
	}
}
