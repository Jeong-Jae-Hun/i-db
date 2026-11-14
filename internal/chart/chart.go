// Package chart 는 결과 표(db.Result)의 숫자 열 하나를 글자 차트로 그린다(R28).
// 상태 없이 그리기만 한다. 차트 켜고 끄기·커서·거르기 순서는 통합(grid)이 들고 있다가 Render 에 넘긴다.
package chart

import (
	"errors"
	"fmt"
	"math"
	"slices"
	"strconv"
	"strings"
	"time"

	"github.com/charmbracelet/lipgloss"
	"github.com/charmbracelet/x/ansi"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/textutil"
)

const nullText = "NULL" // db.collectRows 가 NULL 을 이 글자로 싣는다

// errNoNumeric 은 숫자 열이 없어 차트를 그릴 수 없다는 뜻이다. 통합은 이 글을 상태 줄에 그대로 보인다(R28.1).
var errNoNumeric = errors.New("차트로 그릴 숫자 열이 없음")

var (
	timeTypes = map[string]bool{"date": true, "timestamp": true, "timestamptz": true}
	// 타입이 없는 결과(Redis)의 첫 열을 날짜로 볼지 정할 때 쓰는 형식. PostgreSQL 텍스트 출력과 RFC 3339 를 받는다
	timeLayouts = []string{time.RFC3339Nano, "2006-01-02 15:04:05.999999999Z07", "2006-01-02 15:04:05.999999999Z07:00", "2006-01-02 15:04:05.999999999", "2006-01-02"}
	// 왼쪽부터 차는 1/8 칸 막대. 인덱스는 채운 1/8 수
	eighths = []string{"", "▏", "▎", "▍", "▌", "▋", "▊", "▉"}
	// 점자 한 칸(2×4 점)의 점 비트. [x][y], y 는 위에서부터
	brailleBits = [2][4]rune{{0x01, 0x02, 0x04, 0x40}, {0x08, 0x10, 0x20, 0x80}}
)

// 밝은·어두운 터미널 배경 모두에서 읽히도록 색을 둘씩 둔다(grid 와 같은 256색 표)
var (
	colDim    = lipgloss.AdaptiveColor{Light: "245", Dark: "244"}
	colHead   = lipgloss.AdaptiveColor{Light: "235", Dark: "253"}
	colNumber = lipgloss.AdaptiveColor{Light: "25", Dark: "111"}
	colNeg    = lipgloss.AdaptiveColor{Light: "124", Dark: "174"}
	colWarn   = lipgloss.AdaptiveColor{Light: "130", Dark: "214"}

	dimStyle  = lipgloss.NewStyle().Foreground(colDim)
	headStyle = lipgloss.NewStyle().Foreground(colHead).Bold(true)
	posStyle  = lipgloss.NewStyle().Foreground(colNumber)
	negStyle  = lipgloss.NewStyle().Foreground(colNeg)
	warnStyle = lipgloss.NewStyle().Foreground(colWarn)
)

// point 는 차트에 올릴 행 하나다. raw 는 화면에 적을 값 원문이다.
type point struct {
	label, raw string
	v          float64
}

// ─── Render ───

// Render 는 res 를 w×h 안에 차트로 그린다. 줄 수는 h, 줄 폭은 w 를 넘지 않는다.
// order 는 거르기·정렬을 거친 보이는 행의 res.Rows 인덱스이고 nil 이면 원래 순서다.
// 값 열은 cursorCol 이 숫자 열이면 그 열, 아니면 첫 숫자 열이다. 숫자 열이 없으면 errNoNumeric 이다.
func Render(res *db.Result, order []int, cursorCol, w, h int) (string, error) {
	if res == nil {
		return "", errNoNumeric
	}
	numeric := numericCols(res)
	valCol := slices.Index(numeric, true)
	if valCol < 0 {
		return "", errNoNumeric
	}
	if cursorCol >= 0 && cursorCol < len(numeric) && numeric[cursorCol] {
		valCol = cursorCol
	}
	labelCol := slices.Index(numeric, false)
	if w <= 0 || h <= 0 {
		return "", nil
	}
	if order == nil {
		order = make([]int, len(res.Rows))
		for i := range order {
			order[i] = i
		}
	}

	var pts []point
	skipped := 0
	for pos, ri := range order {
		if ri < 0 || ri >= len(res.Rows) {
			continue
		}
		row := res.Rows[ri]
		raw := strings.TrimSpace(cellOf(row, valCol))
		v, err := strconv.ParseFloat(raw, 64)
		if raw == nullText || err != nil || math.IsNaN(v) || math.IsInf(v, 0) {
			skipped++
			continue
		}
		// 이름표 열이 없으면 보이는 순서의 행 번호(1부터)를 이름표로 쓴다
		label := strconv.Itoa(pos + 1)
		if labelCol >= 0 {
			label = cellOf(row, labelCol)
		}
		pts = append(pts, point{label: textutil.Printable(label, false), raw: textutil.Printable(raw, false), v: v})
	}

	labelName := "#"
	if labelCol >= 0 {
		labelName = textutil.Printable(res.Cols[labelCol], false)
	}
	lines := []string{headStyle.Render(fmt.Sprintf("값: %s · 이름표: %s", textutil.Printable(res.Cols[valCol], false), labelName))}
	if skipped > 0 {
		lines = append(lines, warnStyle.Render(fmt.Sprintf("건너뜀 %d", skipped)))
	}
	if len(pts) > 0 {
		if labelCol == 0 && timeColumn(res) {
			lines = append(lines, lineChart(pts, w, max(1, h-len(lines)))...)
		} else {
			lines = append(lines, barChart(pts, w, max(0, h-len(lines)))...)
		}
	}
	lines = lines[:min(len(lines), h)]
	for i, ln := range lines {
		lines[i] = ansi.Truncate(ln, w, "…")
	}
	return strings.Join(lines, "\n"), nil
}

// ─── Bar ───

// barChart 는 앞에서부터 h 행까지 가로 막대로 그린다. 한 줄은 「이름표 막대 값」이다.
// 음수가 섞이면 0 축(│)을 두고 음수는 왼쪽, 양수는 오른쪽으로 뻗는다.
func barChart(pts []point, w, h int) []string {
	pts = pts[:min(len(pts), h)]
	labelW, valueW := 0, 0
	lo, hi := 0.0, 0.0
	for _, p := range pts {
		labelW, valueW = max(labelW, ansi.StringWidth(p.label)), max(valueW, ansi.StringWidth(p.raw))
		lo, hi = min(lo, p.v), max(hi, p.v)
	}
	labelW = min(labelW, max(4, w/3))
	valueW = min(valueW, max(4, w/4))
	barW := max(1, w-labelW-valueW-2)

	// 막대 칸을 음수·양수 쪽으로 나눈다. 양쪽이 다 있으면 축 한 칸을 뺀 나머지를 크기 비율로 나눈다
	negW, posW, axis := 0, barW, false
	switch {
	case lo < 0 && hi > 0:
		axis = barW >= 3
		room := barW - b2i(axis)
		negW = int(math.Round(float64(room) * spanRatio(0, lo, hi)))
		posW = room - negW
	case lo < 0:
		negW, posW = barW, 0
	}

	lines := make([]string, 0, len(pts))
	for _, p := range pts {
		label := ansi.Truncate(p.label, labelW, "…")
		label += strings.Repeat(" ", labelW-ansi.StringWidth(label))
		var neg, pos string
		if p.v < 0 && negW > 0 {
			neg = negBar(-p.v / -lo * float64(negW))
		}
		if p.v > 0 && posW > 0 {
			pos = posBar(p.v / hi * float64(posW))
		}
		bar := strings.Repeat(" ", negW-ansi.StringWidth(neg)) + negStyle.Render(neg)
		if axis {
			bar += dimStyle.Render("│")
		}
		bar += posStyle.Render(pos) + strings.Repeat(" ", posW-ansi.StringWidth(pos))
		lines = append(lines, label+" "+bar+" "+dimStyle.Render(ansi.Truncate(p.raw, valueW, "…")))
	}
	return lines
}

// posBar 는 cells 칸 길이의 왼쪽 정렬 막대다. 끝 칸은 1/8 단위로 채운다.
func posBar(cells float64) string {
	full := int(cells)
	part := int(math.Round((cells - float64(full)) * 8))
	if part == 8 {
		full, part = full+1, 0
	}
	return strings.Repeat("█", full) + eighths[part]
}

// negBar 는 오른쪽 정렬 막대다. 오른쪽부터 차는 블록 글자는 1/8(▕)과 1/2(▐)뿐이라 끝 칸은 반 칸 단위로 반올림한다.
func negBar(cells float64) string {
	full := int(cells)
	rem := cells - float64(full)
	switch {
	case rem >= 0.75:
		full++
	case rem >= 0.25:
		return "▐" + strings.Repeat("█", full)
	}
	return strings.Repeat("█", full)
}

// ─── Line ───

// lineChart 는 점자 점으로 선을 그린다. 왼쪽에 최대·최소 값, 아래에 첫·끝 이름표를 적는다.
// 가로 위치는 시각이 아니라 순서다(빠진 날은 띄우지 않는다). 점이 화면 점 열(폭×2)보다 많으면
// 이웃 점을 모두 선으로 잇기 때문에 한 점 열에 여러 점이 겹쳐 그 구간의 최소~최대가 세로선으로 남는다.
func lineChart(pts []point, w, h int) []string {
	lo, hi := pts[0].v, pts[0].v
	for _, p := range pts {
		lo, hi = min(lo, p.v), max(hi, p.v)
	}
	plotH := max(1, h-1)
	hiLabel, loLabel := num(hi), num(lo)
	yW := max(ansi.StringWidth(hiLabel), ansi.StringWidth(loLabel))
	// 폭이 좁으면 축 글자를 빼고 선만 그린다
	if w-yW-1 < 4 {
		yW = -1
	}
	plotW := max(1, w-yW-1)

	dotW, dotH := plotW*2, plotH*4
	grid := make([][]rune, plotH)
	for i := range grid {
		grid[i] = make([]rune, plotW)
	}
	px, py := 0, 0
	for i, p := range pts {
		x := 0
		if len(pts) > 1 {
			x = int(math.Round(float64(i) * float64(dotW-1) / float64(len(pts)-1)))
		}
		y := dotH / 2
		if hi > lo {
			y = int(math.Round(spanRatio(p.v, lo, hi) * float64(dotH-1)))
		}
		y = dotH - 1 - y
		if i == 0 {
			px, py = x, y
		}
		plotSegment(grid, px, py, x, y)
		px, py = x, y
	}

	lines := make([]string, 0, plotH+1)
	for r, cells := range grid {
		var b strings.Builder
		for _, c := range cells {
			if c == 0 {
				b.WriteByte(' ')
				continue
			}
			b.WriteRune(0x2800 + c)
		}
		prefix := ""
		if yW >= 0 {
			tick, label := "│", ""
			switch r {
			case 0:
				tick, label = "┤", hiLabel
			case plotH - 1:
				tick, label = "┤", loLabel
			}
			prefix = dimStyle.Render(strings.Repeat(" ", yW-ansi.StringWidth(label)) + label + tick)
		}
		lines = append(lines, prefix+posStyle.Render(b.String()))
	}

	first, last := pts[0].label, pts[len(pts)-1].label
	pad := max(0, yW+1)
	room := w - pad
	xLine := ansi.Truncate(first, max(0, room), "…")
	if gap := room - ansi.StringWidth(first) - ansi.StringWidth(last); len(pts) > 1 && gap >= 1 {
		xLine = first + strings.Repeat(" ", gap) + last
	}
	return append(lines, dimStyle.Render(strings.Repeat(" ", pad)+xLine))
}

// plotSegment 는 점 좌표 (x0,y0)–(x1,y1) 를 브레젠험으로 잇는다.
func plotSegment(grid [][]rune, x0, y0, x1, y1 int) {
	dx, dy := abs(x1-x0), -abs(y1-y0)
	sx, sy := sign(x1-x0), sign(y1-y0)
	e := dx + dy
	for {
		grid[y0/4][x0/2] |= brailleBits[x0%2][y0%4]
		if x0 == x1 && y0 == y1 {
			return
		}
		if e2 := 2 * e; e2 >= dy {
			e += dy
			x0 += sx
		} else {
			e += dx
			y0 += sy
		}
	}
}

// ─── Columns ───

// numericCols 는 열마다 숫자 열인지다. 타입이 있으면 타입으로, 없으면(Redis) grid 처럼 값으로 추정한다:
// NULL 이 아닌 값이 하나 이상이고 모두 숫자로 읽히면 숫자 열이다.
func numericCols(r *db.Result) []bool {
	ns := make([]bool, len(r.Cols))
	for i := range r.Cols {
		if typed(r) {
			ns[i] = db.IsNumericType(r.Types[i])
			continue
		}
		ns[i] = allValues(r, i, parsesAsNumber)
	}
	return ns
}

// timeColumn 은 첫 열이 날짜·시간인지다. 타입이 없으면 NULL 이 아닌 값이 모두 날짜로 읽히는지 본다.
func timeColumn(r *db.Result) bool {
	if typed(r) {
		return timeTypes[r.Types[0]]
	}
	return allValues(r, 0, parsesAsTime)
}

func parsesAsNumber(v string) bool {
	_, err := strconv.ParseFloat(v, 64)
	return err == nil
}

func parsesAsTime(v string) bool {
	for _, l := range timeLayouts {
		if _, err := time.Parse(l, v); err == nil {
			return true
		}
	}
	return false
}

// allValues 는 열 c 의 NULL 이 아닌 값이 하나 이상이고 모두 ok 인지다.
func allValues(r *db.Result, c int, ok func(string) bool) bool {
	seen := false
	for _, row := range r.Rows {
		v := cellOf(row, c)
		if v == nullText {
			continue
		}
		if !ok(v) {
			return false
		}
		seen = true
	}
	return seen
}

func typed(r *db.Result) bool {
	return len(r.Types) == len(r.Cols)
}

// cellOf 는 열 수보다 짧은 행에서도 패닉 없이 칸을 꺼낸다.
func cellOf(row []string, c int) string {
	if c < 0 || c >= len(row) {
		return ""
	}
	return row[c]
}

// num 은 축 글자다. 정수는 그대로, 나머지는 유효 숫자 6자리로 줄인다.
func num(v float64) string {
	if v == math.Trunc(v) && math.Abs(v) < 1e15 {
		return strconv.FormatFloat(v, 'f', 0, 64)
	}
	return strconv.FormatFloat(v, 'g', 6, 64)
}

// spanRatio 는 lo ≤ v ≤ hi, lo < hi 일 때 (v-lo)/(hi-lo) 다. ±1e308 처럼 hi-lo 가 float64 를 넘어 Inf 가 되면
// 비율이 NaN 이 되고 int 로 바꾼 좌표가 화면 밖으로 나가므로, 반으로 줄여 뺀다(유한한 두 값의 반의 차는 넘치지 않는다).
func spanRatio(v, lo, hi float64) float64 {
	return (v/2 - lo/2) / (hi/2 - lo/2)
}

func abs(v int) int { return max(v, -v) }

func sign(v int) int {
	switch {
	case v > 0:
		return 1
	case v < 0:
		return -1
	}
	return 0
}

func b2i(b bool) int {
	if b {
		return 1
	}
	return 0
}
