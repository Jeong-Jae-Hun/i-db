package editor

import (
	"sort"
	"strconv"
	"strings"
	"unicode"
	"unicode/utf8"

	"github.com/charmbracelet/lipgloss"
)

const (
	bgNone bgKind = iota
	bgLine
	bgSel
	bgPopup
	bgPopupSel
)

// 어두운 배경 기준(DataGrip Darcula 계열) 색이다
var (
	fgColors = map[role]lipgloss.Color{
		roleKeyword:     "#CC7832",
		roleString:      "#6A8759",
		roleNumber:      "#6897BB",
		roleComment:     "#808080",
		roleQuoted:      "#A9B7C6",
		roleFunc:        "#FFC66D",
		roleOperator:    "#A9B7C6",
		roleParam:       "#9876AA",
		roleCommand:     "#CC7832",
		roleGutter:      "#606366",
		roleGutterCur:   "#A4A3A3",
		rolePopup:       "#BBBBBB",
		rolePopupKind:   "#9876AA",
		rolePopupDetail: "#808080",
	}
	bgColors = map[bgKind]lipgloss.Color{
		bgLine:     "#2A2D33",
		bgSel:      "#214283",
		bgPopup:    "#3C3F41",
		bgPopupSel: "#2F65CA",
	}
)

type bgKind int

// cell 은 화면 한 칸이다. 넓은 글자는 머리 칸(w=2) 뒤에 w=0 인 빈 칸이 붙는다.
type cell struct {
	s      string
	w      int
	fg     role
	bg     bgKind
	cursor bool
}

// appendText 는 c.s 를 글자마다 칸으로 펼쳐 붙인다. 탭은 공백, 제어 문자는 ? 로 그려 터미널을 망가뜨리지 않는다.
func appendText(dst []cell, c cell) []cell {
	base := len(dst)
	for _, r := range c.s {
		w := runeCells(r)
		g := c
		switch {
		case r == '\t':
			g.s, g.w = " ", 1
			for i := 0; i < tabCells; i++ {
				dst = append(dst, g)
			}
			continue
		case unicode.IsControl(r):
			// C1(U+0080–U+009F)도 폭 0 이라 앞 칸에 붙으면 터미널 제어로 읽힌다
			g.s = "?"
		case w == 0:
			// 결합 문자는 이번에 붙인 앞 글자 칸에 합친다. 받은 칸은 고치지 않으므로 맨 앞 결합 문자는 버린다
			i := len(dst) - 1
			for i > base && dst[i].w == 0 {
				i--
			}
			if i >= base {
				dst[i].s += string(r)
			}
			continue
		default:
			g.s = string(r)
		}
		g.w = max(1, w)
		dst = append(dst, g)
		for i := 1; i < w; i++ {
			g.s, g.w = "", 0
			dst = append(dst, g)
		}
	}
	return dst
}

// renderRow 는 한 화면 줄(거터+내용)을 정확히 m.w 칸으로 만든다.
func (m Model) renderRow(row int) []cell {
	out := make([]cell, 0, m.w)
	if row >= len(m.lines) {
		return appendText(out, cell{s: strings.Repeat(" ", m.w)})
	}
	bg := bgNone
	if m.focused && row == m.rowOf(m.cur) {
		bg = bgLine
	}
	if gw := m.gutterWidth(); gw > 0 {
		num := strconv.Itoa(row + 1)
		fg := roleGutter
		if bg == bgLine {
			fg = roleGutterCur
		}
		out = appendText(out, cell{s: strings.Repeat(" ", gw-1-len(num)) + num + " ", fg: fg, bg: bg})
	}
	cw := m.contentWidth()
	start, end := m.lines[row], m.lineEnd(row)
	ss, se, hasSel := m.selRange()
	ti := sort.Search(len(m.toks), func(i int) bool { return m.toks[i].end > start })
	var line []cell
	for off := start; off < end && len(line) < m.left+cw; {
		// 결합 문자는 앞 글자와 한 칸에 그린다
		_, size := utf8.DecodeRuneInString(m.text[off:])
		for off+size < end {
			r, n := utf8.DecodeRuneInString(m.text[off+size:])
			if runeCells(r) != 0 {
				break
			}
			size += n
		}
		for ti < len(m.toks) && m.toks[ti].end <= off {
			ti++
		}
		c := cell{s: m.text[off : off+size], bg: bg, cursor: m.focused && m.cur >= off && m.cur < off+size}
		if ti < len(m.toks) && m.toks[ti].start <= off {
			c.fg = m.toks[ti].role
		}
		if hasSel && off >= ss && off < se {
			c.bg = bgSel
		}
		line = appendText(line, c)
		off += size
	}
	eol := cell{s: " ", bg: bg, cursor: m.focused && m.cur == end}
	if hasSel && end >= ss && end < se && row+1 < len(m.lines) {
		eol.bg = bgSel
	}
	line = appendText(line, eol)
	vis := line[min(m.left, len(line)):]
	vis = vis[:min(len(vis), cw)]
	// 가로 스크롤 경계에 걸려 반만 보이는 넓은 글자는 공백으로 그린다
	if len(vis) > 0 && vis[0].w == 0 {
		vis[0].s, vis[0].w = " ", 1
	}
	if n := len(vis); n > 0 && vis[n-1].w > 1 {
		vis[n-1].s, vis[n-1].w = " ", 1
	}
	out = append(out, vis...)
	for len(out) < m.w {
		out = append(out, cell{s: " ", w: 1, bg: bg})
	}
	return out
}

func (m Model) View() string {
	if m.w <= 0 || m.h <= 0 {
		return ""
	}
	rows := make([][]cell, m.h)
	for y := range rows {
		rows[y] = m.renderRow(m.top + y)
	}

	cy := m.rowOf(m.cur) - m.top
	if m.pop.open && m.focused && cy >= 0 && cy < m.h {
		items := m.pop.items
		labelW, detailW := 0, 0
		for _, it := range items {
			labelW, detailW = max(labelW, lipgloss.Width(it.Label)), max(detailW, lipgloss.Width(it.Detail))
		}
		detailX := 3 + labelW + 2
		pw := detailX - 1
		if detailW > 0 {
			pw = detailX + detailW + 1
		}
		pw = min(pw, popupMaxW, m.w)
		ph := min(popupRows, len(items)-m.pop.top, m.h)
		x0 := max(0, min(m.gutterWidth()+m.colOf(m.acceptStart(items[m.pop.sel]))-m.left, m.w-pw))
		y0 := cy + 1
		if y0+ph > m.h {
			y0 = cy - ph
			if y0 < 0 {
				y0 = max(0, m.h-ph)
			}
		}
		for i := 0; i < ph; i++ {
			idx := m.pop.top + i
			it := items[idx]
			bg := bgPopup
			if idx == m.pop.sel {
				bg = bgPopupSel
			}
			tag := " "
			if r, _ := utf8.DecodeRuneInString(it.Kind); it.Kind != "" {
				tag = strings.ToUpper(string(r))
			}
			line := appendText(nil, cell{s: " " + tag + " ", fg: rolePopupKind, bg: bg})
			line = appendText(line, cell{s: it.Label, fg: rolePopup, bg: bg})
			if it.Detail != "" {
				line = appendText(line, cell{s: strings.Repeat(" ", max(1, detailX-len(line))), bg: bg})
				line = appendText(line, cell{s: it.Detail, fg: rolePopupDetail, bg: bg})
			}
			line = line[:min(len(line), pw)]
			if n := len(line); n > 0 && line[n-1].w > 1 {
				line[n-1].s, line[n-1].w = " ", 1
			}
			for len(line) < pw {
				line = append(line, cell{s: " ", w: 1, bg: bg})
			}
			row := rows[y0+i]
			if x0 > 0 && row[x0].w == 0 {
				row[x0-1] = cell{s: " ", w: 1, bg: row[x0-1].bg}
			}
			if x0+pw < len(row) && row[x0+pw].w == 0 {
				row[x0+pw] = cell{s: " ", w: 1, bg: row[x0+pw].bg}
			}
			copy(row[x0:], line)
		}
	}

	var b strings.Builder
	for y, row := range rows {
		if y > 0 {
			b.WriteByte('\n')
		}
		for i := 0; i < len(row); {
			c := row[i]
			var run strings.Builder
			j := i
			for j < len(row) && row[j].fg == c.fg && row[j].bg == c.bg && row[j].cursor == c.cursor {
				run.WriteString(row[j].s)
				j++
			}
			i = j
			if c.fg == rolePlain && c.bg == bgNone && !c.cursor {
				b.WriteString(run.String())
				continue
			}
			st := lipgloss.NewStyle()
			if col, ok := fgColors[c.fg]; ok {
				st = st.Foreground(col)
			}
			if col, ok := bgColors[c.bg]; ok {
				st = st.Background(col)
			}
			if c.fg == roleComment {
				st = st.Italic(true)
			}
			if c.cursor {
				st = st.Reverse(true)
			}
			b.WriteString(st.Render(run.String()))
		}
	}
	return b.String()
}
