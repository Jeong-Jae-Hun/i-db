package grid

// ─── 시트에서 붙여넣기·칸 입력 도우미 (v7) ───
// 붙여넣기(R48)와 입력 도우미(R49)는 칸 편집과 같은 보류 중인 바꿈(put)에 값을 둔다.
// 그래서 제출 미리 보기·옛값 확인·u 되돌리기·ClearSubmitted 가 손으로 고친 칸과 똑같이 다룬다.

import (
	"errors"
	"fmt"
	"strings"
	"time"

	"github.com/Jeong-Jae-Hun/i-db/internal/textutil"
)

const maxPasteCells = 10_000 // 시트 한 화면을 넉넉히 넘는다. 실수로 붙인 큰 글이 미리 보기를 수만 문장으로 만들지 않게 한다

// now·today 를 그 열 타입의 리터럴로 바꿀 꼴. timestamptz 는 지역 시간에 오프셋을 붙인다
var nowLayouts = map[string]string{
	"date":        time.DateOnly,
	"timestamp":   time.DateTime,
	"timestamptz": "2006-01-02 15:04:05-07:00",
}

// clock 은 now·today 의 지금 시각이다. 시험이 바꿔 끼운다
var clock = time.Now

// PickEnum 은 enum 칸에서 enter·f2 를 눌러 고르기 목록을 열어 달라는 요청이다(R49).
// Type 은 결과 열 타입(db.Result.Types 값)이고, Current 는 지금 값이다(NULL 이면 "NULL").
type PickEnum struct {
	Col     int
	Type    string
	Current string
}

// EditLong 은 칸 편집 중 ctrl+o 로 전체 화면 편집기를 열어 달라는 요청이다(R49).
// 인라인 편집은 끝나고, 편집기를 저장하면 통합이 SetCell 로 값을 넘긴다. 바꾸지 않고 닫으면 부르지 않는다.
// JSON 은 열 타입이 json·jsonb 라 편집기가 검증·정렬해야 하는지다. Null 이면 지금 값이 NULL 이고 Value 는 "" 다.
type EditLong struct {
	Value string
	JSON  bool
	Null  bool
}

// PasteResult 는 붙여넣기 결과 수다.
type PasteResult struct {
	Cells   int // 값을 둔 칸
	Added   int // 표 끝에 새로 만든 추가 행
	Skipped int // 식·계산 열, 열 범위 밖, 삭제 표시한 행이라 버린 칸
}

// Status 는 상태 줄 글이다.
func (p PasteResult) Status() string {
	s := fmt.Sprintf("붙여넣기 %s칸", textutil.GroupDigits(int64(p.Cells)))
	if p.Added > 0 {
		s += fmt.Sprintf(" · 새 행 %d개", p.Added)
	}
	if p.Skipped > 0 {
		s += fmt.Sprintf(" · 건너뜀 %d칸(식 열·범위 밖·삭제 행)", p.Skipped)
	}
	return s + " · ctrl+s 로 미리 보기"
}

// SetEnumTypes 는 enum 인 열 타입 이름(db.Result.Types 값)을 알린다. grid 는 카탈로그를 모르므로 통합이 채운다.
// 새 결과(SetResult)에도 이어진다.
func (m *Model) SetEnumTypes(types map[string]bool) {
	m.enums = types
}

// Paste 는 시트에서 복사한 TSV 를 커서 칸부터 보이는 순서대로 덮어쓴다(R48). 보이는 행이 끝나면 남은 줄은 추가 행이 된다.
//
// 칸은 Excel·구글 시트가 쓰는 꼴로 읽는다: 탭으로 칸, 줄바꿈(\r\n·\n)으로 행을 나누고, "로 시작하는 칸은
// 따옴표 안의 탭·줄바꿈을 값으로 두며 ""는 " 하나다. 마지막 줄바꿈 하나는 빈 행으로 보지 않는다.
// 빈 칸은 빈 글자이고 정확히 NULL 인 칸은 NULL 이다. 표가 NULL 을 그렇게 보이고 y·Y 로 그렇게 복사하므로
// 표에서 복사한 행을 그대로 붙이면 NULL 이 지켜진다. 단, 'NULL' 글자를 넣으려면 칸 편집을 쓴다.
// 식·계산 열, 열 범위 밖, 삭제 표시한 행의 칸은 건너뛰고 수를 센다. 추가 행에 쓸 칸이 하나도 없으면 행을 만들지 않는다.
func (m *Model) Paste(text string) (PasteResult, error) {
	var p PasteResult
	if why := m.editBlock(); why != "" {
		return p, errors.New(why)
	}
	switch {
	case m.decOn:
		return p, errors.New("붙여 넣을 수 없음: 복호화 보기 중 (D 로 원래 값)")
	case m.Typing() || m.Viewing():
		return p, errors.New("표로 돌아가 붙여넣기")
	case text == "":
		return p, errors.New("붙여 넣을 내용 없음")
	}
	rows, ok := parseTSV(text, maxPasteCells)
	if !ok {
		return p, fmt.Errorf("붙여넣기는 %s칸까지", textutil.GroupDigits(maxPasteCells))
	}
	o := m.res.Origin
	m.rev++
	for r, cells := range rows {
		vals := map[int]*string{}
		for j, cell := range cells {
			c := m.col + j
			if c >= len(o.Cols) || o.Cols[c] == "" {
				p.Skipped++
				continue
			}
			vals[c] = &cell
			if cell == nullText {
				vals[c] = nil
			}
		}
		var i int
		switch vi := m.row + r; {
		case vi < len(m.view):
			i = m.view[vi]
			if e := m.edits[i]; e != nil && e.deleted {
				p.Skipped += len(vals)
				continue
			}
		case len(vals) == 0:
			continue
		default:
			i = len(m.res.Rows) + m.inserts
			m.inserts++
			m.edits[i] = &rowEdit{vals: map[int]*string{}, inserted: true}
			p.Added++
		}
		if len(vals) == 0 {
			continue
		}
		for c, v := range vals {
			m.put(i, c, v)
		}
		p.Cells += len(vals)
		m.touch(i)
	}
	m.rebuild()
	m.follow()
	return p, nil
}

// parseTSV 는 TSV 를 행·칸으로 나눈다. 칸이 limit 를 넘으면 거짓이다.
// encoding/csv 는 빈 줄을 건너뛰어 한 열을 복사한 시트의 빈 칸이 사라지므로 직접 읽는다.
func parseTSV(s string, limit int) ([][]string, bool) {
	var rows [][]string
	var row []string
	for n := 1; ; n++ {
		if n > limit {
			return nil, false
		}
		cell, rest, end := tsvCell(s)
		row, s = append(row, cell), rest
		if end == '\t' {
			continue
		}
		rows, row = append(rows, row), nil
		if s == "" {
			return rows, true
		}
	}
}

var cellNewlines = strings.NewReplacer("\r\n", "\n", "\r", "\n")

// tsvCell 은 s 앞 칸 하나와 나머지, 칸을 끝낸 글자('\t', '\n', 끝이면 0)를 돌려준다.
// 닫히지 않은 따옴표로 시작하면 따옴표 칸이 아니라 글자 그대로 읽는다.
func tsvCell(s string) (string, string, byte) {
	if strings.HasPrefix(s, `"`) {
		var b strings.Builder
		for i := 1; i < len(s); i++ {
			switch {
			case s[i] != '"':
				b.WriteByte(s[i])
			case i+1 < len(s) && s[i+1] == '"':
				b.WriteByte('"')
				i++
			default:
				// 닫는 따옴표 뒤 구분자 전까지는 시트가 쓰지 않지만 버리지 않고 잇는다.
				// 윈도 클립보드는 칸 안 줄바꿈을 \r\n 으로, 옛 맥 앱은 \r 하나로 싣는 경우가 있어 \n 으로 맞춘다
				tail, rest, end := bareCell(s[i+1:])
				return cellNewlines.Replace(b.String()) + tail, rest, end
			}
		}
	}
	return bareCell(s)
}

// bareCell 은 따옴표 없는 칸 하나를 읽는다.
func bareCell(s string) (string, string, byte) {
	i := strings.IndexAny(s, "\t\r\n")
	switch {
	case i < 0:
		return s, "", 0
	case s[i] == '\t':
		return s[:i], s[i+1:], '\t'
	case strings.HasPrefix(s[i:], "\r\n"):
		return s[:i], s[i+2:], '\n'
	}
	return s[:i], s[i+1:], '\n'
}

// SetCell 은 PickEnum·EditLong 을 연 칸에 값 v(nil 은 NULL)를 보류 중인 바꿈으로 둔다.
// 그사이 새 결과를 받았거나 행을 지웠거나 삭제 표시했으면 오류이고 아무것도 바꾸지 않는다. 한 요청에 한 번만 받는다.
func (m *Model) SetCell(v *string) error {
	t := m.aim
	m.aim = nil
	if t == nil {
		return errors.New("고칠 칸이 없음")
	}
	if why := m.editBlock(); why != "" {
		return errors.New(why)
	}
	e := m.edits[t.row]
	switch {
	case m.decOn:
		return errors.New("편집할 수 없음: 복호화 보기 중 (D 로 원래 값)")
	case m.gone[t.row] || (t.row >= len(m.res.Rows) && e == nil):
		return errors.New("고칠 행이 없어짐")
	case e != nil && e.deleted:
		return errors.New("편집할 수 없음: 삭제 표시한 행 (u 로 되돌리기)")
	}
	m.rev++
	m.put(t.row, t.col, v)
	m.touch(t.row)
	return nil
}

// colType 은 열 c 의 타입 이름이다. 타입이 없는 결과(Redis)면 "" 다.
func (m Model) colType(c int) string {
	if !typed(m.res) {
		return ""
	}
	return m.res.Types[c]
}

func (m Model) colIsEnum(c int) bool {
	return m.enums[m.colType(c)]
}

// pickEnum 은 커서 칸의 enum 고르기를 청한다. 커서 칸은 cellBlock 으로 편집할 수 있음을 확인한 뒤 부른다.
func (m *Model) pickEnum() Action {
	i := m.view[m.row]
	m.aim = &cellKey{i, m.col}
	return Action{
		PickEnum: &PickEnum{Col: m.col, Type: m.colType(m.col), Current: cellOf(m.current(i), m.col)},
		Status:   "enum 값 고르기",
	}
}

// editLong 은 인라인 편집을 끝내고 그 버퍼로 전체 화면 편집기를 청한다. 인라인에서 친 글은 편집기에 옮겨 가고
// 편집기를 저장하지 않으면 버린다(esc 와 같다).
func (m *Model) editLong() Action {
	e := m.edit
	m.editing = false
	m.aim = &cellKey{e.row, e.col}
	return Action{
		EditLong: &EditLong{Value: string(e.buf), JSON: jsonTypes[m.colType(e.col)], Null: e.null},
		Status:   "전체 화면 편집기",
	}
}

// expandNow 는 날짜·시각 열에 친 now·today 를 지금 시각 리터럴로 바꾼다. today 는 지역 시간 자정이다.
// 서버의 now()·'today' 대신 리터럴을 두는 것은 제출 미리 보기에 실제로 들어갈 값을 보이고, 추가 행·옛값 확인이
// 손으로 친 값과 같은 길을 타게 하려는 것이다. 단, 시각은 이 컴퓨터 시계이고 초 단위까지다.
func (m *Model) expandNow() {
	e := &m.edit
	layout := nowLayouts[m.colType(e.col)]
	word := strings.ToLower(strings.TrimSpace(string(e.buf)))
	if layout == "" || !e.dirty || e.null || (word != "now" && word != "today") {
		return
	}
	t := clock()
	if word == "today" {
		t = time.Date(t.Year(), t.Month(), t.Day(), 0, 0, 0, 0, t.Location())
	}
	e.buf = []rune(t.Format(layout))
	e.cur = len(e.buf)
}

// cycleBool 은 bool 칸 값을 true → false → NULL → true 로 돌린다. PostgreSQL 텍스트 꼴(t·f)로 둬
// 원래 값으로 돌아오면 바꿈에서 빠진다.
func (e *cellEdit) cycleBool() {
	switch {
	case e.null:
		e.buf, e.null = []rune("t"), false
	case strings.EqualFold(string(e.buf), "t") || strings.EqualFold(string(e.buf), "true"):
		e.buf = []rune("f")
	case strings.EqualFold(string(e.buf), "f") || strings.EqualFold(string(e.buf), "false"):
		e.buf, e.null = nil, true
	default:
		e.buf = []rune("t")
	}
	e.cur, e.dirty, e.cycled = len(e.buf), true, true
}

// editHint 는 칸 편집 중 키 안내다. 열 타입에 맞는 도우미 키를 더한다.
func (m Model) editHint() string {
	h := "enter 확정 · esc 취소 · ctrl+n NULL · ctrl+o 큰 편집기"
	switch c := m.edit.col; {
	case m.kinds[c] == kindBool:
		h += " · space 참/거짓/NULL"
	case nowLayouts[m.colType(c)] != "":
		h += " · now·today"
	}
	return h
}
