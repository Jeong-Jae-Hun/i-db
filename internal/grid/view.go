package grid

import (
	"fmt"
	"slices"
	"strconv"
	"strings"

	"github.com/charmbracelet/lipgloss"
	"github.com/charmbracelet/x/ansi"

	"github.com/Jeong-Jae-Hun/i-db/internal/resultdiff"
	"github.com/Jeong-Jae-Hun/i-db/internal/textutil"
)

// 밝은·어두운 터미널 배경 모두에서 읽히도록 색을 둘씩 둔다(256색)
var (
	colDim    = lipgloss.AdaptiveColor{Light: "245", Dark: "244"}
	colFaint  = lipgloss.AdaptiveColor{Light: "250", Dark: "240"}
	colHead   = lipgloss.AdaptiveColor{Light: "235", Dark: "253"}
	colAccent = lipgloss.AdaptiveColor{Light: "25", Dark: "117"}
	colNumber = lipgloss.AdaptiveColor{Light: "25", Dark: "111"}
	colTrue   = lipgloss.AdaptiveColor{Light: "28", Dark: "114"}
	colFalse  = lipgloss.AdaptiveColor{Light: "124", Dark: "174"}
	bgZebra   = lipgloss.AdaptiveColor{Light: "255", Dark: "234"}
	bgRow     = lipgloss.AdaptiveColor{Light: "254", Dark: "236"}
	bgCell    = lipgloss.AdaptiveColor{Light: "153", Dark: "24"}
	fgCell    = lipgloss.AdaptiveColor{Light: "16", Dark: "231"}
	bgEdited  = lipgloss.AdaptiveColor{Light: "229", Dark: "58"}
	fgEdited  = lipgloss.AdaptiveColor{Light: "94", Dark: "229"}
	colWarn   = lipgloss.AdaptiveColor{Light: "130", Dark: "214"}
	bgSel     = lipgloss.AdaptiveColor{Light: "189", Dark: "60"}
	colPlain  = lipgloss.AdaptiveColor{Light: "90", Dark: "176"} // 복호화한 칸(R37)

	dimStyle  = lipgloss.NewStyle().Foreground(colDim)
	sepStyle  = lipgloss.NewStyle().Foreground(colFaint)
	sortMarks = map[sortDir]string{sortAsc: "▲", sortDesc: "▼"}

	headStyle = lipgloss.NewStyle().Foreground(colHead).Bold(true)
	typeStyle = lipgloss.NewStyle().Foreground(colDim).Italic(true)

	cursorStyle = lipgloss.NewStyle().Reverse(true)
	// 행 번호 칸 앞자리 표시: 추가 +, 삭제 −, 수정 *
	insertMark = lipgloss.NewStyle().Foreground(colTrue).Bold(true).Render("+")
	deleteMark = lipgloss.NewStyle().Foreground(colFalse).Bold(true).Render("−")
	editMark   = lipgloss.NewStyle().Foreground(fgEdited).Bold(true).Render("*")

	// 비교 화면(R31.2): 추가 초록, 삭제 빨강, 바뀜 노랑
	diffColors = map[resultdiff.Op]lipgloss.AdaptiveColor{resultdiff.Added: colTrue, resultdiff.Removed: colFalse, resultdiff.Changed: colWarn}
	diffMarks  = map[resultdiff.Op]string{resultdiff.Added: "+", resultdiff.Removed: "−", resultdiff.Changed: "~"}
)

// View 는 w×h 안에 표를 그린다. 줄 수는 h, 줄 폭은 w 를 넘지 않는다.
func (m Model) View(focused bool) string {
	if m.res == nil || m.w <= 0 || m.h <= 0 {
		return ""
	}
	l := m.layout()
	r := m.res
	var lines []string
	switch {
	case m.mode == modeChart:
		if m.chart != "" {
			lines = append(lines, strings.Split(m.chart, "\n")...)
		}
		lines = append(lines, dimStyle.Render("차트 · c·esc 표로"))

	case m.mode == modeDiff:
		dv := m.diff
		lines = append(lines, headStyle.Render(dv.rep.Summary())+dimStyle.Render(" · 고정 결과 → 지금 결과"))
		cols := m.spans(dv.left, 2)
		head := "  "
		for _, s := range cols {
			head += sepStyle.Render("│") + headStyle.Render(" "+fit(textutil.Printable(r.Cols[s.col], false), s.w)+" ")
		}
		lines = append(lines, head)
		if len(dv.rep.Rows) == 0 && l.body > 0 {
			lines = append(lines, dimStyle.Render(" 차이 없음"))
		}
		for i := dv.top; i < len(dv.rep.Rows) && i < dv.top+l.body; i++ {
			d := dv.rep.Rows[i]
			base := lipgloss.NewStyle().Foreground(diffColors[d.Op])
			if i == dv.row {
				base = base.Background(bgRow)
			}
			line := base.Bold(true).Render(diffMarks[d.Op] + " ")
			for _, s := range cols {
				st := base
				if slices.Contains(d.ChangedCols, s.col) {
					st = st.Background(bgEdited).Foreground(fgEdited).Bold(true)
				}
				line += base.Foreground(colFaint).Render("│") + st.Render(" "+fit(cellText(cellOf(d.Row, s.col), s.w), s.w)+" ")
			}
			lines = append(lines, line)
		}
		n := len(dv.rep.Rows)
		lines = append(lines, dimStyle.Render(fmt.Sprintf("행 %d–%d / %d · j/k 이동 · h/l 열 · esc 닫기",
			min(dv.top+1, n), min(n, dv.top+max(1, l.body)), n)))

	case m.mode == modeCell && m.cell.treeOn:
		lines = append(lines, headStyle.Render(m.cell.title))
		if t := m.cell.tree.View(); t != "" {
			lines = append(lines, strings.Split(t, "\n")...)
		}
		lines = append(lines, dimStyle.Render("t 원문 · enter 접기/펴기 · y 값 · Y 경로 · esc 닫기"))

	case m.mode == modeCell:
		end := min(len(m.cell.lines), m.cell.top+l.body)
		lines = append(lines, headStyle.Render(m.cell.title))
		lines = append(lines, m.cell.lines[m.cell.top:end]...)
		tree := ""
		if m.cell.hasTree {
			tree = " · t 나무"
		}
		lines = append(lines, dimStyle.Render(fmt.Sprintf("줄 %d–%d / %d%s · j/k 스크롤 · y 복사 · esc 닫기", min(m.cell.top+1, end), end, len(m.cell.lines), tree)))

	case m.mode == modeDetail:
		row := m.current(m.view[m.row])
		// 추가 행에서 정하지 않은 칸은 표 보기처럼 DEFAULT 로 그린다
		unset := map[int]bool{}
		if e := m.edits[m.view[m.row]]; e != nil && e.inserted {
			for c := range r.Cols {
				_, set := e.vals[c]
				unset[c] = !set
			}
		}
		hasTypes := typed(r)
		nameW, typeW := 0, 0
		for i, c := range r.Cols {
			nameW = max(nameW, min(24, ansi.StringWidth(textutil.Printable(c, false))))
			if hasTypes {
				typeW = max(typeW, min(14, ansi.StringWidth(textutil.Printable(r.Types[i], false))))
			}
		}
		lines = append(lines, headStyle.Render(fmt.Sprintf("레코드 %d / %d", m.row+1, len(m.view))))
		for i := m.detailTop; i < len(r.Cols) && i < m.detailTop+l.body; i++ {
			base := lipgloss.NewStyle()
			if i == m.detailField {
				base = base.Background(bgRow)
			}
			line := base.Foreground(colHead).Bold(true).Render(" " + fit(textutil.Printable(r.Cols[i], false), nameW) + " ")
			if hasTypes {
				line += base.Foreground(colDim).Italic(true).Render(fit(textutil.Printable(r.Types[i], false), typeW) + " ")
			}
			valW := max(0, m.w-ansi.StringWidth(line)-3)
			raw := cellOf(row, i)
			shown, plain := m.plain(m.view[m.row], i, raw)
			vs := base
			text := cellText(display(m.kinds[i], shown), valW)
			switch {
			case unset[i]:
				vs, text = vs.Foreground(colFaint).Italic(true), defaultText
			case plain:
				vs = vs.Foreground(colPlain)
			case raw == nullText:
				vs = vs.Foreground(colFaint).Italic(true)
			case m.kinds[i] == kindNumber:
				vs = vs.Foreground(colNumber)
			}
			if focused && i == m.detailField {
				vs = vs.Background(bgCell).Foreground(fgCell)
			}
			line += base.Foreground(colFaint).Render("│") + vs.Render(" "+fit(text, valW)+" ")
			lines = append(lines, line)
		}
		lines = append(lines, dimStyle.Render("j/k 필드 · enter 전체 값 · y 값 복사 · Y 행 복사 · esc 닫기"))

	case len(r.Cols) == 0:
		end := min(len(m.textLines), m.textTop+l.body)
		for _, t := range m.textLines[m.textTop:end] {
			if !m.wrap {
				t = ansi.Cut(t, m.textLeft, m.textLeft+m.w)
			}
			lines = append(lines, t)
		}
		wrapState := "w 줄바꿈 켜기"
		if m.wrap {
			wrapState = "w 줄바꿈 끄기"
		}
		lines = append(lines, dimStyle.Render(fmt.Sprintf("줄 %d–%d / %d · %s", min(m.textTop+1, end), end, len(m.textLines), wrapState)))

	default:
		if l.caption {
			first, _, _ := strings.Cut(r.Text, "\n")
			lines = append(lines, dimStyle.Render(cellText(first, m.w)))
		}
		hasTypes := l.headLines == 3
		head := strings.Repeat(" ", l.numW+2)
		types, rule := head, strings.Repeat("─", l.numW+2)
		for _, s := range l.cols {
			label := fit(textutil.Printable(r.Cols[s.col], false), s.w)
			if s.col == m.sortCol && m.sortDir != sortNone && s.w >= 3 {
				label = fit(textutil.Printable(r.Cols[s.col], false), s.w-2) + " " + sortMarks[m.sortDir]
			}
			hs := headStyle
			if s.col == m.col {
				hs = hs.Foreground(colAccent)
			}
			head += sepStyle.Render("│") + hs.Render(" "+label+" ")
			if hasTypes {
				types += sepStyle.Render("│") + typeStyle.Render(" "+fit(textutil.Printable(r.Types[s.col], false), s.w)+" ")
			}
			rule += "┼" + strings.Repeat("─", s.w+2)
		}
		lines = append(lines, head)
		if hasTypes {
			lines = append(lines, types)
		}
		lines = append(lines, sepStyle.Render(rule))
		if len(m.view) == 0 && l.body > 0 {
			empty := " (0 rows)"
			if m.filter != "" {
				empty = " 일치하는 행 없음"
			}
			lines = append(lines, dimStyle.Render(empty))
		}
		r0, r1, c0, c1 := -1, -1, -1, -1
		if m.sel {
			r0, r1, c0, c1 = m.selRect()
		}
		for vi := m.rowTop; vi < len(m.view) && vi < m.rowTop+l.body; vi++ {
			i := m.view[vi]
			row, e := m.current(i), m.edits[i]
			base := lipgloss.NewStyle()
			switch {
			case vi == m.row:
				base = base.Background(bgRow)
			case vi%2 == 1:
				base = base.Background(bgZebra)
			}
			num := base.Foreground(colDim)
			if vi == m.row {
				num = base.Foreground(colHead).Bold(true)
			}
			mark := num.Render(" ")
			switch {
			case e == nil:
			case e.inserted:
				mark = insertMark
			case e.deleted:
				mark = deleteMark
			default:
				mark = editMark
			}
			line := mark + num.Render(fitRight(strconv.Itoa(vi+1), l.numW)+" ")
			for _, s := range l.cols {
				raw, k := cellOf(row, s.col), m.kinds[s.col]
				shown, plain := m.plain(i, s.col, raw)
				st := base
				text := fit(cellText(display(k, shown), s.w), s.w)
				changed := false
				if e != nil {
					_, changed = e.vals[s.col]
				}
				switch {
				case e != nil && e.inserted && !changed:
					st = st.Foreground(colFaint).Italic(true)
					text = fit(defaultText, s.w)
				case plain:
					st = st.Foreground(colPlain)
				case raw == nullText:
					st = st.Foreground(colFaint).Italic(true)
				case k == kindNumber:
					st = st.Foreground(colNumber)
					text = fitRight(cellText(raw, s.w), s.w)
				case k == kindBool && raw == "t":
					st = st.Foreground(colTrue)
				case k == kindBool && raw == "f":
					st = st.Foreground(colFalse)
				}
				switch {
				case e != nil && e.deleted:
					st = st.Foreground(colFalse).Strikethrough(true)
				case changed:
					st = st.Background(bgEdited).Foreground(fgEdited)
				}
				if vi >= r0 && vi <= r1 && s.col >= c0 && s.col <= c1 {
					st = st.Background(bgSel).Foreground(fgCell)
				}
				if vi == m.row && s.col == m.col {
					if focused {
						st = st.Background(bgCell).Foreground(fgCell).Bold(true)
					} else {
						st = st.Underline(true)
					}
				}
				cell := st.Render(" " + text + " ")
				if m.editing && m.edit.row == i && m.edit.col == s.col {
					cell = m.edit.render(s.w, st)
				}
				line += base.Foreground(colFaint).Render("│") + cell
			}
			lines = append(lines, line)
		}

		var parts []string
		n := len(m.view)
		rows := fmt.Sprintf("rows 0 of %d", n)
		if n > 0 {
			rows = fmt.Sprintf("rows %d–%d of %d", m.rowTop+1, min(n, m.rowTop+max(1, l.body)), n)
		}
		if r.Truncated && m.filter == "" {
			rows += "+"
		}
		parts = append(parts, rows)
		if m.filter != "" {
			parts = append(parts, fmt.Sprintf("filtered %d/%d", n, len(r.Rows)))
		}
		if len(l.cols) > 0 {
			parts = append(parts, fmt.Sprintf("cols %d–%d of %d", l.cols[0].col+1, l.cols[len(l.cols)-1].col+1, len(r.Cols)))
		}
		if m.sortDir != sortNone {
			parts = append(parts, sortMarks[m.sortDir]+" "+textutil.Printable(r.Cols[m.sortCol], false))
		}
		if n := m.Pending(); n > 0 {
			parts = append(parts, fmt.Sprintf("변경 %d건 · ctrl+s 제출", n))
		}
		if m.decOn {
			parts = append(parts, "복호화 보기 · D 원래 값")
		}
		footer := dimStyle.Render(strings.Join(parts, " · "))
		switch {
		case m.sel:
			footer = lipgloss.NewStyle().Foreground(colAccent).Render(fmt.Sprintf("선택 %d×%d · y 복사 · C IN 목록 · esc 끝", r1-r0+1, c1-c0+1)) + "  " + footer
		case m.editing:
			footer = lipgloss.NewStyle().Foreground(colAccent).Render("편집 중 · "+m.editHint()) + "  " + footer
		case m.typing:
			footer = lipgloss.NewStyle().Foreground(colAccent).Render("/"+textutil.Printable(m.filter, false)+"▏") + "  " + footer
		case m.filter != "":
			footer = dimStyle.Render("/"+textutil.Printable(m.filter, false)+" (esc 해제) · ") + footer
		}
		lines = append(lines, footer)
	}

	// 작은 영역에서도 주어진 크기를 넘지 않게 마지막에 한 번 자른다. 위치 줄(마지막 줄)을 남긴다
	if len(lines) > m.h {
		lines = append(lines[:m.h-1], lines[len(lines)-1])
	}
	for i, ln := range lines {
		lines[i] = ansi.Truncate(ln, m.w, "")
	}
	return strings.Join(lines, "\n")
}

// cellText 는 칸 하나를 폭 w 한 줄로 그릴 글자로 바꾼다. 칸은 64KiB 까지라 w 칸을 채울 앞부분만 다룬다(E63).
// 한 글자는 4바이트 이하이고 대부분 폭이 1 이상이라 4w 바이트면 w 칸을 넘는다(폭 0 결합 문자가 몰리면 덜 보일 수 있다).
func cellText(raw string, w int) string {
	if n := 4*w + 16; len(raw) > n {
		raw = raw[:n]
	}
	return oneLine(raw)
}

// oneLine 은 값을 한 줄로 그릴 글자로 바꾼다. 줄바꿈은 ↵, 잘못된 UTF-8 은 �, 제어 문자는 textutil.Printable 로 바꾼다.
func oneLine(s string) string {
	return textutil.Printable(strings.ReplaceAll(strings.ToValidUTF8(s, "�"), "\n", "↵"), false)
}

func fit(s string, w int) string {
	if w <= 0 {
		return ""
	}
	t := ansi.Truncate(s, w, "…")
	return t + strings.Repeat(" ", max(0, w-ansi.StringWidth(t)))
}

func fitRight(s string, w int) string {
	if w <= 0 {
		return ""
	}
	t := ansi.Truncate(s, w, "…")
	return strings.Repeat(" ", max(0, w-ansi.StringWidth(t))) + t
}

// wrapLines 는 줄마다 폭 w 로 접는다. 공백에서 먼저 끊고, 긴 낱말(JSON 문자열 등)은 글자 단위로 자른다.
func wrapLines(lines []string, w int) []string {
	if w <= 0 {
		return lines
	}
	out := make([]string, 0, len(lines))
	for _, l := range lines {
		out = append(out, strings.Split(ansi.Wrap(l, w, ""), "\n")...)
	}
	return out
}
