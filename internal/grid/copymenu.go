package grid

// ─── 복사 꼴 고르기 (R71) ───
// Ctrl+K·오른쪽 클릭으로 대상(행·선택·보이는 행·열) × 꼴(TSV·CSV·JSON·Markdown·INSERT·IN)을 골라 복사한다.
// 값은 y·Y 처럼 화면 값(보류 중인 바꿈·복호화 보기 반영)이다. INSERT·IN 은 i·C 와 같은 DB 값이다.

import (
	"fmt"
	"strings"

	"github.com/Jeong-Jae-Hun/i-db/internal/clipboard"
	"github.com/Jeong-Jae-Hun/i-db/internal/db"
)

const (
	TargetRow CopyTarget = iota
	TargetSelection
	TargetVisible
	TargetColumn
)

const (
	FormatTSV CopyFormat = iota
	FormatCSV
	FormatJSON
	FormatMarkdown
	FormatInsert
	FormatIN
)

var (
	targetNames = [...]string{TargetRow: "현재 행", TargetSelection: "선택", TargetVisible: "보이는 행 전체", TargetColumn: "현재 열"}
	formatNames = [...]string{FormatTSV: "TSV (헤더)", FormatCSV: "CSV (헤더)", FormatJSON: "JSON", FormatMarkdown: "Markdown 표",
		FormatInsert: "SQL INSERT", FormatIN: "IN 목록"}
	// Markdown 칸 안의 | 는 칸 경계, 줄바꿈은 행 경계로 읽혀 표가 깨진다
	mdEscaper = strings.NewReplacer("|", `\|`, "\r\n", "<br>", "\n", "<br>", "\r", "<br>")
)

type CopyTarget int

type CopyFormat int

// CopyChoice 는 복사 고르기 창의 항목 하나다.
type CopyChoice struct {
	Target CopyTarget
	Format CopyFormat
}

func (c CopyChoice) String() string {
	return targetNames[c.Target] + " · " + formatNames[c.Format]
}

// CopyChoices 는 지금 고를 수 있는 대상 × 꼴이다. 표 화면에 행이 없으면 비어 있다.
// 선택은 V 중에만, INSERT 는 원본 테이블을 아는 결과에서 열 대상이 아닐 때만, IN 은 열 대상에만 있다.
func (m Model) CopyChoices() []CopyChoice {
	if m.res == nil || m.mode != modeTable || len(m.res.Cols) == 0 || len(m.view) == 0 {
		return nil
	}
	var out []CopyChoice
	for _, t := range []CopyTarget{TargetRow, TargetSelection, TargetVisible, TargetColumn} {
		if t == TargetSelection && !m.sel {
			continue
		}
		for _, f := range []CopyFormat{FormatTSV, FormatCSV, FormatJSON, FormatMarkdown, FormatInsert, FormatIN} {
			if (f == FormatInsert && (m.res.Origin == nil || t == TargetColumn)) || (f == FormatIN && t != TargetColumn) {
				continue
			}
			out = append(out, CopyChoice{Target: t, Format: f})
		}
	}
	return out
}

// Selecting 은 사각형 선택(V) 중인지다. 통합이 복사 고르기 창의 기본 대상을 정한다.
func (m Model) Selecting() bool {
	return m.sel
}

// CopyWith 는 고른 대상·꼴로 복사할 글을 만든다. y·Y·V→y 출력은 그대로 두고 헤더 붙은 꼴은 여기서만 만든다.
func (m Model) CopyWith(c CopyChoice) Action {
	if m.res == nil || len(m.res.Cols) == 0 || len(m.view) == 0 {
		return Action{Status: "복사할 행 없음"}
	}
	r0, r1, c0, c1 := m.row, m.row, 0, len(m.res.Cols)-1
	switch c.Target {
	case TargetSelection:
		if m.sel {
			r0, r1, c0, c1 = m.selRect()
		}
	case TargetVisible:
		r0, r1 = 0, len(m.view)-1
	case TargetColumn:
		r0, r1, c0, c1 = 0, len(m.view)-1, m.col, m.col
	}
	// 다 만든 뒤 clipboard.Write 가 거절하면 그 사이 화면이 멈추고 큰 버퍼를 잡는다. 칸 길이로 먼저 어림해 만들기 전에 멈춘다
	// INSERT 는 행 전체, INSERT·IN 은 DB 값으로 만들므로 같은 범위·값으로 잰다
	size, sc0, sc1 := 0, c0, c1
	if c.Format == FormatInsert {
		sc0, sc1 = 0, len(m.res.Cols)-1
	}
	for vi := r0; vi <= r1 && size <= clipboard.MaxBytes; vi++ {
		i := m.view[vi]
		row := m.shown(i)
		if (c.Format == FormatInsert || c.Format == FormatIN) && i < len(m.res.Rows) {
			row = m.res.Rows[i]
		}
		for col := sc0; col <= sc1; col++ {
			size += len(cellOf(row, col)) + 1
		}
	}
	if size > clipboard.MaxBytes {
		return Action{Status: clipboard.TooBigError{Bytes: size}.Status()}
	}
	var text string
	switch c.Format {
	case FormatIN:
		// 상태는 C 의 글(잘림 안내 포함)을 메뉴 꼴 뒤에 붙인다
		a := m.inList(r0, r1, c0, c1)
		if a.Copy != nil {
			a.Status = fmt.Sprintf("복사함 (1줄) · %s · %s", c, a.Status)
		}
		return a
	case FormatInsert:
		// i 와 같은 db.InsertSQL 이다. 행 전체를 넣으므로 선택의 열 범위는 보지 않는다
		var rows []int
		for vi := r0; vi <= r1; vi++ {
			if i := m.view[vi]; i < len(m.res.Rows) {
				rows = append(rows, i)
			}
		}
		if text = db.InsertSQL(m.res, rows); text == "" {
			return Action{Status: "추가한 행은 아직 결과에 없어 SQL 로 복사할 수 없음"}
		}
	default:
		sub := &db.Result{Cols: m.res.Cols[c0 : c1+1], Rows: make([][]string, 0, r1-r0+1)}
		if typed(m.res) {
			sub.Types = m.res.Types[c0 : c1+1]
		}
		for vi := r0; vi <= r1; vi++ {
			row := m.shown(m.view[vi])
			cells := make([]string, 0, c1-c0+1)
			for col := c0; col <= c1; col++ {
				cells = append(cells, cellOf(row, col))
			}
			sub.Rows = append(sub.Rows, cells)
		}
		switch kinds := m.kinds[c0 : c1+1]; c.Format {
		case FormatCSV:
			text = ToCSV(sub)
		case FormatJSON:
			text = ToJSON(sub, kinds)
		case FormatMarkdown:
			// GitHub 꼴 표다. 숫자 열은 오른쪽 정렬이다
			var b strings.Builder
			b.Grow(resultBytes(sub) * 2)
			mdRow(&b, sub.Cols)
			b.WriteByte('|')
			for _, k := range kinds {
				if k == kindNumber {
					b.WriteString(" ---: |")
				} else {
					b.WriteString(" --- |")
				}
			}
			b.WriteByte('\n')
			for _, cells := range sub.Rows {
				mdRow(&b, cells)
			}
			text = b.String()
		default:
			lines := make([]string, 0, len(sub.Rows)+1)
			lines = append(lines, RowTSV(sub.Cols))
			for _, row := range sub.Rows {
				lines = append(lines, RowTSV(row))
			}
			text = strings.Join(lines, "\n")
		}
	}
	lines := strings.Count(strings.TrimSuffix(text, "\n"), "\n") + 1
	return Action{Copy: &text, Status: fmt.Sprintf("복사함 (%d줄) · %s", lines, c)}
}

func mdRow(b *strings.Builder, cells []string) {
	b.WriteByte('|')
	for _, v := range cells {
		b.WriteString(" " + mdEscaper.Replace(v) + " |")
	}
	b.WriteByte('\n')
}

// CellAt 은 grid 영역 기준 좌표가 표 본문의 칸인지다. 오른쪽 클릭이 머리·빈 자리면 창을 열지 않는다.
func (m Model) CellAt(x, y int) bool {
	if m.res == nil || m.mode != modeTable || len(m.res.Cols) == 0 {
		return false
	}
	l := m.layout()
	bodyTop := b2i(l.caption) + l.headLines
	if y < bodyTop || y >= bodyTop+l.body || m.rowTop+y-bodyTop >= len(m.view) {
		return false
	}
	for _, s := range l.cols {
		if x >= s.x && x < s.x+s.w+3 {
			return true
		}
	}
	return false
}
