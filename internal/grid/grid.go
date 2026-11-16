// Package grid 는 쿼리 결과를 DataGrip 처럼 보여 주는 표 컴포넌트다.
// 상태 바꾸기(Update·SetSize·HandleMouse)에서 스크롤 위치를 맞추고, View 는 그리기만 한다.
//
// 결과에 Origin(한 테이블+PK 전부)이 있으면 표에서 바로 고친다(edit.go).
// enter·f2 칸 편집(편집할 수 없는 칸의 enter 는 전체 값 보기), o 전체 값 보기, a 행 추가, ctrl+d 행 복제,
// d 삭제 표시, u 행 되돌리기, ctrl+s 제출, i·U 행을 INSERT·UPDATE 문으로 복사.
// ctrl+s 는 보류 중인 바꿈이 있을 때만 받고, 없으면 처리하지 않아 통합의 전역 ctrl+s(파일 저장)로 간다.
// 표가 아닌 화면·선택 중에는 제출하지 않고 표로 돌아가라고 알린다.
// 읽기 전용 결과에서 ctrl+d 는 v2 처럼 반 쪽 스크롤이다.
//
// v4: c 차트(R28), V 사각형 선택과 합계(R31.1), = 고정 결과와 비교 요청(R31.2, ShowDiff), 값 보기의 JSON 나무(R32).
// 차트·비교·선택은 읽기만 하고 보류 중인 바꿈을 건드리지 않는다.
//
// v5(v5.go): C IN 목록 복사(R35), F·R 외래 키 따라가기 요청(R36), D 복호화 보기 요청(R37, SetDecrypted),
// +·- 칸 값으로 거르기 요청(R38).
//
// v7(v7.go): 시트에서 붙여넣기(R48, Paste), 칸 입력 도우미(R49: bool space 돌리기, 날짜 now·today,
// enum 고르기 요청, ctrl+o 전체 화면 편집기 요청).
package grid

import (
	"cmp"
	"errors"
	"fmt"
	"slices"
	"sort"
	"strconv"
	"strings"
	"unicode/utf8"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/x/ansi"

	"github.com/Jeong-Jae-Hun/i-db/internal/chart"
	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/jsonview"
	"github.com/Jeong-Jae-Hun/i-db/internal/resultdiff"
	"github.com/Jeong-Jae-Hun/i-db/internal/textutil"
)

const (
	autoMaxWidth = 40  // 자동 폭 상한. 넘는 값은 enter 로 본다
	minColWidth  = 3   // NULL·숫자 한두 자리도 읽히는 최소 폭
	maxColWidth  = 200 // 수동 조절 상한
	resizeStep   = 2
	wheelRows    = 3
	textStep     = 8       // 줄바꿈을 끈 텍스트의 가로 이동 칸
	farAway      = 1 << 30 // g/G 처럼 끝까지 가는 이동. 더한 뒤 clamp 한다
	nullText     = "NULL"  // db.collectRows 가 NULL 을 이 글자로 싣는다
)

const (
	kindText kind = iota
	kindNumber
	kindBool
)

const (
	modeTable mode = iota
	modeDetail
	modeCell
	modeChart
	modeDiff
)

const (
	sortNone sortDir = iota
	sortAsc
	sortDesc
)

var (
	jsonTypes  = map[string]bool{"json": true, "jsonb": true}
	sortArrows = map[sortDir]string{sortAsc: "↑", sortDesc: "↓"}
	// 붙여넣은 거르기 입력의 줄바꿈·탭은 한 줄 입력에서 보이지 않으므로 공백으로 바꾼다
	pasteFlattener = strings.NewReplacer("\r\n", " ", "\n", " ", "\r", " ", "\t", " ")
)

type kind int

type mode int

type sortDir int

// Action 은 키 처리 뒤 통합(ui.go)이 할 일이다.
//
// 복사는 grid 가 터미널에 직접 쓰지 않고 Copy 로 원문만 넘긴다. grid 는 출력 장치를 모르고
// Bubble Tea 렌더러와 같은 출력에 섞어 쓰면 화면이 깨질 수 있어서, 통합이 tea.Cmd 안에서
// OSC 52(github.com/aymanbagabas/go-osc52/v2, 이미 의존성 그래프에 있음)로 보낸다.
// 빈 문자열도 복사할 수 있도록 포인터로 둔다(nil 이면 복사 없음).
//
// Submit 은 ctrl+s 로 만든 편집 묶음이다. 통합이 prod 확인 뒤 db 로 제출하고, 성공하면 ClearSubmitted 를 부른다.
// Compare 는 = 로 비교를 청한 것이다. 통합이 고정 결과 하나를 고르게 한 뒤 ShowDiff 로 넘긴다.
// Follow·Filter 는 F·R, +·- 요청이다(v5.go). Decrypt 는 D 다. 통합이 Decrypted() 를 보고 SetDecrypted·ClearDecrypted 를 부른다.
//
// PickEnum·EditLong 은 enum 고르기 목록·전체 화면 편집기를 열어 달라는 요청이다(v7.go). 통합은 고른 값·편집한 값을 SetCell 로 넘긴다.
// CopyMenu 는 Ctrl+K 로 복사 고르기 창을 청한 것이다(R71). 통합이 CopyChoices 로 창을 열고 CopyWith 로 만든다.
// Linked 는 Ctrl+B 로 커서 칸 값이 든 다른 테이블을 찾아 달라는 요청이다(R73, v5.go).
type Action struct {
	Status   string
	Copy     *string
	Export   *Export
	Submit   *db.ChangeSet
	CopyAs   *CopyAs
	Compare  bool
	Follow   *Follow
	Filter   *CellFilter
	Decrypt  bool
	PickEnum *PickEnum
	EditLong *EditLong
	CopyMenu bool
	Linked   *Linked
}

// Export 는 내보내기 요청이다. 파일 이름·저장 위치는 통합이 정한다.
type Export struct {
	Format  string // "csv" 또는 "json"
	Content string
	Rows    int
}

// Model 은 결과 하나를 보여 주는 표다. 커서 row 는 거르기·정렬을 거친 view 의 인덱스다.
type Model struct {
	res    *db.Result
	kinds  []kind
	widths []int
	view   []int // 보이는 행의 res.Rows 인덱스(거르기·정렬 반영)
	w, h   int

	row, col       int
	rowTop, colTop int

	sortCol int
	sortDir sortDir
	filter  string
	typing  bool
	// server 면 행이 이미 서버에서 정렬·거른 것이다(R14.5). 근사인 받은 행 거르기·정렬(R14.4)을 다시 돌리면
	// 큰 정수·모든 열 찾기·글자 순서가 서버와 어긋나 맞는 행이 사라질 수 있어 건너뛴다. 정렬·거르기를 바꾸면 꺼진다
	server bool
	// 거르기용 소문자 칸. 64KiB 칸 1000개를 키마다 소문자로 바꾸면 100ms 넘게 걸려 결과마다 한 번만 만든다(E63).
	// 대문자가 든 칸만 사본이 생겨 결과 크기만큼 메모리를 더 쓸 수 있다. 커지면 접두어만 두는 방식으로 바꾼다
	folded [][]string

	mode        mode
	detailField int
	detailTop   int
	cell        cellView
	chart       string // 차트 화면. 켤 때와 크기가 바뀔 때만 그린다
	diff        diffView

	// 사각형 선택(V). 시작 칸(view 인덱스·열)과 커서 칸이 두 모서리다
	sel            bool
	selRow, selCol int
	agg            selAgg

	// 복호화 보기(R37). decOn 이면 dec 에 든 칸을 푼 값으로 보인다
	dec   map[cellKey]plainCell
	decOn bool

	// 편집(edit.go). 행 인덱스는 결과 행 뒤에 추가 행이 이어지는 결합 인덱스다
	pkCols  []int           // Origin.PK 각각의 결과 열. nil 이면 편집할 수 없다
	enums   map[string]bool // enum 타입 이름(SetEnumTypes). 새 결과에도 이어진다
	aim     *cellKey        // PickEnum·EditLong 을 연 칸. SetCell 이 이 칸에 값을 둔다
	edits   map[int]*rowEdit
	inserts int // 지금까지 만든 추가 행 수(지운 추가 행 포함)
	editing bool
	edit    cellEdit
	rev     int            // 바꿈이 달라질 때마다 오른다. 결과를 바꿔 끼워도 이어진다
	log     map[int][]snap // 행마다 판별 바꿈 기록. ClearSubmitted 가 제출한 판의 상태를 찾는다
	gone    map[int]bool   // 제출로 DB 에서 지운 결과 행. 다시 읽기 전까지 표에서 감춘다

	text      []string // 표가 아닌 결과의 줄(제어 문자 처리 후)
	textLines []string // 폭·줄바꿈을 반영한 줄
	textWidth int      // 줄바꿈을 껐을 때 가장 긴 줄의 폭(가로 스크롤 끝)
	textTop   int
	textLeft  int
	wrap      bool
}

type cellView struct {
	title string
	raw   string
	body  string
	lines []string
	top   int
	back  mode
	// JSON 으로 읽히는 값은 나무(R32)로 먼저 보인다. hasTree 가 거짓이면 원문만 있다
	tree    jsonview.Model
	hasTree bool
	treeOn  bool
}

// selAgg 는 선택 집계 글의 캐시다. 키는 선택 사각형(view 행·열 양끝)과 바꿈 판(rev)이다.
// 큰 선택을 키마다 다시 세지 않으려는 것이다. view 가 바뀌면(rebuild) 같은 사각형도 다른 칸이라 비운다.
type selAgg struct {
	key  [5]int
	text string
	ok   bool
}

// diffView 는 비교 화면이다. row 는 고른 줄, left 는 가로로 민 첫 열이다.
type diffView struct {
	rep            resultdiff.Report
	row, top, left int
}

// layout 은 현재 크기·스크롤에서 화면 자리다. 그리기와 마우스 좌표 변환이 같은 계산을 쓴다.
type layout struct {
	caption   bool // 표 위 한 줄 설명(Redis 키 정보 등)
	headLines int  // 열 이름 + 타입 + 구분선
	body      int  // 데이터·본문 줄 수
	numW      int  // 행 번호 숫자 폭
	avail     int  // 행 번호 칸을 뺀 열 영역 폭
	cols      []span
}

// span 은 보이는 열 하나다. x 는 열 앞 구분선 자리, w 는 내용 폭(앞뒤 여백 1칸씩 제외)이다.
type span struct {
	col, x, w int
}

// rowOrder 는 한 열 기준의 정렬이다. 숫자 열은 미리 바꿔 둔 값으로 비교한다.
type rowOrder struct {
	idx     []int
	cells   []string // res.Rows 인덱스별 정렬 열 값
	nums    []float64
	isNum   []bool
	numeric bool
	desc    bool
}

// cellAt 은 전체 값 보기로 열 칸과 닫은 뒤 돌아갈 화면이다.
type cellAt struct {
	col  int
	back mode
}

func New() Model {
	return Model{wrap: true}
}

// SetResult 는 새 결과를 보인다. 보류 중인 편집은 버린다. 통합은 Pending() 이 0 이 아니면 덮기 전에 확인을 받는다.
func (m *Model) SetResult(r *db.Result) {
	*m = Model{res: r, w: m.w, h: m.h, wrap: m.wrap, edits: map[int]*rowEdit{}, rev: m.rev + 1,
		log: map[int][]snap{}, gone: map[int]bool{}, enums: m.enums}
	if r == nil {
		return
	}
	if o := r.Origin; o != nil && len(o.Cols) == len(r.Cols) {
		for _, pk := range o.PK {
			j := slices.Index(o.Cols, pk)
			if j < 0 {
				m.pkCols = nil
				break
			}
			m.pkCols = append(m.pkCols, j)
		}
	}
	if len(r.Cols) == 0 {
		m.text = strings.Split(textutil.Printable(r.Text, true), "\n")
	} else {
		m.kinds = columnKinds(r)
		m.widths = autoWidths(r, m.kinds)
	}
	m.rebuild()
	m.reflow()
	m.follow()
}

func (m *Model) SetSize(w, h int) {
	m.w, m.h = max(0, w), max(0, h)
	m.reflow()
	m.follow()
}

// Typing 은 거르기 입력이나 칸 편집 중인지 알린다. 이때 통합은 q·ctrl+s 같은 키도 grid 로 넘겨야 한다.
func (m Model) Typing() bool {
	return m.typing || m.editing
}

// Viewing 은 표가 아닌 화면(레코드·값·차트·비교)을 보이거나 사각형 선택 중인지 알린다.
// 이때 통합은 t·esc 같은 결과 영역 키를 가로채지 않고 grid 로 넘긴다.
func (m Model) Viewing() bool {
	return m.mode != modeTable || m.sel
}

// StopTyping 은 거르기 입력·칸 편집을 끝낸다. 이미 입력한 거르기 글과 편집한 값은 남긴다(Enter 와 같다).
func (m *Model) StopTyping() {
	m.typing = false
	m.commitEdit()
}

func (m Model) Update(k tea.KeyMsg) (Model, Action) {
	if m.res == nil {
		return m, Action{}
	}
	var a Action
	ks := k.String()
	// bubbletea 1.3 은 빠르게 친 글자를 KeyRunes 하나로 묶어 보낸다. 글자 키는 한 글자만 키 이름으로 읽는다
	if k.Type == tea.KeyRunes && len(k.Runes) != 1 {
		ks = ""
	}
	page := max(1, m.layout().body)
	// 세로 이동은 표·텍스트·전체 값·레코드 화면이 같은 키를 쓴다. 더한 뒤 follow 가 범위로 맞춘다
	d, moved := 0, true
	switch ks {
	case "j", "down":
		d = 1
	case "k", "up":
		d = -1
	case "pgdown":
		d = page
	case "pgup":
		d = -page
	case "ctrl+d":
		d = max(1, page/2)
	case "g", "home":
		d = -farAway
	case "G", "end":
		d = farAway
	default:
		moved = false
	}
	switch {
	case m.editing:
		e := &m.edit
		switch k.Type {
		case tea.KeyRunes, tea.KeySpace:
			if k.Type == tea.KeySpace && m.kinds[e.col] == kindBool && (!e.dirty || e.cycled) {
				e.cycleBool()
				break
			}
			rs := k.Runes
			if k.Type == tea.KeySpace {
				rs = []rune{' '}
			}
			e.buf = slices.Insert(e.buf, e.cur, rs...)
			e.cur += len(rs)
			e.null, e.dirty, e.cycled = false, true, false
		case tea.KeyBackspace:
			if e.cur > 0 {
				e.buf = slices.Delete(e.buf, e.cur-1, e.cur)
				e.cur--
			}
			e.null, e.dirty, e.cycled = false, true, false
		case tea.KeyDelete:
			if e.cur < len(e.buf) {
				e.buf = slices.Delete(e.buf, e.cur, e.cur+1)
			}
			e.null, e.dirty, e.cycled = false, true, false
		case tea.KeyLeft:
			e.cur = max(0, e.cur-1)
		case tea.KeyRight:
			e.cur = min(len(e.buf), e.cur+1)
		case tea.KeyHome, tea.KeyCtrlA:
			e.cur = 0
		case tea.KeyEnd, tea.KeyCtrlE:
			e.cur = len(e.buf)
		case tea.KeyCtrlN:
			e.buf, e.cur, e.null, e.dirty, e.cycled = nil, 0, true, true, false
		case tea.KeyCtrlO:
			a = m.editLong()
		case tea.KeyEnter:
			m.expandNow()
			m.commitEdit()
		case tea.KeyEsc:
			m.editing = false
		}
		// 편집 중에는 다른 키를 전역으로 흘리지 않는다(Typing). 커서 이동이 없으므로 follow 도 필요 없다
		return m, a

	case m.typing:
		switch k.Type {
		case tea.KeyRunes:
			m.filter += pasteFlattener.Replace(string(k.Runes))
		case tea.KeySpace:
			m.filter += " "
		case tea.KeyBackspace:
			_, size := utf8.DecodeLastRuneInString(m.filter)
			m.filter = m.filter[:len(m.filter)-size]
		case tea.KeyEnter:
			m.typing = false
		case tea.KeyEsc:
			m.filter, m.typing = "", false
		default:
			return m, a
		}
		m.rebuild()

	case ks == "ctrl+s" && (m.mode != modeTable || m.sel) && m.Pending() > 0:
		// 제출 미리보기는 표 자리에 그려지므로 다른 화면에서는 받지 않고 이유를 알린다
		a.Status = fmt.Sprintf("표로 돌아가 Ctrl+S 로 제출 (변경 %d건)", m.Pending())

	case m.mode == modeChart:
		if ks == "c" || ks == "esc" {
			m.mode, m.chart = modeTable, ""
		}

	case m.mode == modeDiff:
		if moved {
			m.diff.row += d
		}
		switch ks {
		case "h", "left":
			m.diff.left--
		case "l", "right":
			m.diff.left++
		case "0":
			m.diff.left = 0
		case "esc":
			m.mode, m.diff = modeTable, diffView{}
		}

	case m.mode == modeCell && m.cell.treeOn:
		switch ks {
		case "t":
			m.cell.treeOn = false
		case "o":
			m.mode = m.cell.back
		case "q", "":
			// q 는 전역 키(종료)라 나무로 넘기지 않는다. jsonview 는 q 를 닫기로 읽는다
		default:
			var ja jsonview.Action
			m.cell.tree, ja = m.cell.tree.Update(k)
			a = Action{Status: ja.Status, Copy: ja.Copy}
			if ja.Close {
				m.mode = m.cell.back
			}
		}

	case m.mode == modeCell:
		if moved {
			m.cell.top += d
		}
		switch ks {
		case "esc", "enter", "o":
			m.mode = m.cell.back
		case "y":
			a = copyAction(m.cell.raw, "값")
		case "t":
			m.cell.treeOn = m.cell.hasTree
		}

	case m.mode == modeDetail:
		if moved {
			m.detailField += d
		}
		row := m.shown(m.view[m.row])
		switch ks {
		case "esc", "v":
			m.mode = modeTable
		case "enter":
			m.openCell(cellAt{col: m.detailField, back: modeDetail})
		case "y":
			a = copyAction(cellOf(row, m.detailField), "값")
		case "Y":
			a = copyAction(RowTSV(row), "행(TSV)")
		}

	case len(m.res.Cols) == 0:
		if moved {
			m.textTop += d
		}
		switch ks {
		case "h", "left":
			m.textLeft -= textStep
		case "l", "right":
			m.textLeft += textStep
		case "0":
			m.textLeft = 0
		case "w":
			m.wrap = !m.wrap
			m.textLeft = 0
			m.reflow()
			a.Status = "줄바꿈 끔"
			if m.wrap {
				a.Status = "줄바꿈 켬"
			}
		case "y":
			a = copyAction(m.res.Text, "텍스트")
		case "C", "F", "R", "D", "+", "-", "ctrl+b":
			a.Status = "표 결과가 아님"
		}

	case m.sel:
		// 선택 중에는 이동·복사·끝내기만 받는다. 편집·정렬·거르기 키가 모르는 새 바꿈을 만들지 않게 한다
		if moved {
			m.row = clamp(m.row+d, len(m.view))
		}
		switch ks {
		case "l", "right":
			m.col++
		case "h", "left":
			m.col--
		case "0":
			m.col = 0
		case "$":
			m.col = len(m.res.Cols) - 1
		case "esc", "V":
			m.sel = false
			a.Status = "선택 끝"
		case "y":
			m.follow()
			r0, r1, c0, c1 := m.selRect()
			lines := make([]string, 0, r1-r0+1)
			for vi := r0; vi <= r1; vi++ {
				row := m.shown(m.view[vi])
				cells := make([]string, 0, c1-c0+1)
				for c := c0; c <= c1; c++ {
					cells = append(cells, cellOf(row, c))
				}
				lines = append(lines, RowTSV(cells))
			}
			a = copyAction(strings.Join(lines, "\n"), fmt.Sprintf("선택 %d×%d(TSV)", r1-r0+1, c1-c0+1))
		case "C":
			m.follow()
			a = m.inList(m.selRect())
		case "ctrl+k":
			m.follow()
			a.CopyMenu = true
		}

	default:
		// 편집할 수 있는 결과에서 ctrl+d 는 반 쪽 스크롤 대신 행 복제다(DataGrip 과 같다)
		dup := ks == "ctrl+d" && m.editBlock() == ""
		if moved && !dup {
			m.row = clamp(m.row+d, len(m.view)) // 아래에서 커서 행의 바꿈을 바로 읽으므로 follow 전에 맞춘다
		}
		hasRow := len(m.view) > 0
		var cur *rowEdit
		if hasRow {
			cur = m.edits[m.view[m.row]]
		}
		switch ks {
		case "l", "right":
			m.col++
		case "h", "left":
			m.col--
		case "0":
			m.col = 0
		case "$":
			m.col = len(m.res.Cols) - 1
		case "<", ">":
			delta := resizeStep
			if ks == "<" {
				delta = -resizeStep
			}
			m.widths[m.col] = min(maxColWidth, max(minColWidth, m.widths[m.col]+delta))
			a.Status = fmt.Sprintf("열 폭 %d", m.widths[m.col])
		case "s":
			m.CycleSort()
		case "/":
			m.typing, m.server = true, false
			m.rebuild()
		case "esc":
			if m.filter != "" {
				m.filter, m.server = "", false
				m.rebuild()
			}
		case "v":
			if hasRow {
				m.mode, m.detailField, m.detailTop = modeDetail, m.col, 0
			}
		case "o":
			if hasRow {
				m.openCell(cellAt{col: m.col, back: modeTable})
			}
		case "enter", "f2":
			// 편집할 수 없는 칸의 enter 는 v2 처럼 전체 값 보기다. f2 는 편집 전용이라 이유를 알린다
			why := m.cellBlock(m.col)
			switch {
			case why == "" && m.colIsEnum(m.col):
				a = m.pickEnum()
			case why == "":
				i := m.view[m.row]
				m.edit = cellEdit{row: i, col: m.col}
				if v := cellOf(m.current(i), m.col); v == nullText {
					m.edit.null = true
				} else {
					m.edit.buf = []rune(v)
					m.edit.cur = len(m.edit.buf)
				}
				m.editing = true
				a.Status = "편집: " + m.editHint()
			case ks == "enter" && hasRow:
				m.openCell(cellAt{col: m.col, back: modeTable})
			default:
				a.Status = why
			}
		case "a":
			if why := m.editBlock(); why != "" {
				a.Status = why
				break
			}
			m.addRow(map[int]*string{})
			a.Status = "행 추가 · enter 로 값 입력 · 값을 넣지 않은 칸은 DB 기본값"
		case "ctrl+d":
			if !dup || !hasRow {
				break
			}
			// PK 는 비워 DB 기본값(시퀀스 등)을 받게 하고, 나머지 테이블 열은 NULL 까지 그대로 옮긴다
			o, i, vals := m.res.Origin, m.view[m.row], map[int]*string{}
			if e := m.edits[i]; e != nil && e.inserted {
				// 추가 행은 정한 값만 옮긴다. 정하지 않은 칸을 NULL 로 채우면 NOT NULL 열이 실패하고 DB 기본값을 잃는다
				for c, v := range e.vals {
					if !slices.Contains(o.PK, o.Cols[c]) {
						vals[c] = v
					}
				}
			} else {
				src := m.current(i)
				for c, name := range o.Cols {
					if name == "" || slices.Contains(o.PK, name) {
						continue
					}
					v := cellOf(src, c)
					vals[c] = &v
					if v == nullText {
						vals[c] = nil
					}
				}
			}
			m.addRow(vals)
			a.Status = "행 복제 · PK 는 DB 기본값"
		case "d":
			switch why := m.editBlock(); {
			case why != "":
				a.Status = why
			case !hasRow:
			case cur != nil && cur.inserted:
				m.rev++
				delete(m.edits, m.view[m.row])
				m.touch(m.view[m.row])
				vr := m.row
				m.rebuild()
				m.row = vr
				a.Status = "추가한 행 지움"
			case cur != nil && cur.deleted:
				m.rev++
				cur.deleted = false
				if len(cur.vals) == 0 {
					delete(m.edits, m.view[m.row])
				}
				m.touch(m.view[m.row])
				a.Status = "삭제 표시 해제"
			default:
				m.rev++
				if cur == nil {
					cur = &rowEdit{}
					m.edits[m.view[m.row]] = cur
				}
				cur.deleted = true
				m.touch(m.view[m.row])
				a.Status = "삭제 표시 · ctrl+s 로 제출 · d 다시 누르면 해제"
			}
		case "u":
			switch why := m.editBlock(); {
			case why != "":
				a.Status = why
			case cur == nil:
				a.Status = "되돌릴 변경 없음"
			default:
				m.rev++
				delete(m.edits, m.view[m.row])
				m.touch(m.view[m.row])
				vr := m.row
				m.rebuild()
				m.row = vr
				a.Status = "행 변경 되돌림"
			}
		case "ctrl+s":
			// 보류 중인 바꿈이 없으면 처리하지 않는다. 통합이 전역 ctrl+s(파일 저장)로 보낸다
			if n := m.Pending(); n > 0 {
				cs := m.changeSet()
				a.Submit = &cs
				a.Status = fmt.Sprintf("변경 %d건 제출", n)
			}
		case "i", "U":
			kind := copyKinds[ks]
			switch why := m.editBlock(); {
			case why != "":
				a.Status = why
			case !hasRow:
			case m.view[m.row] >= len(m.res.Rows):
				a.Status = "추가한 행은 아직 결과에 없어 SQL 로 복사할 수 없음"
			default:
				a.CopyAs = &CopyAs{Kind: kind, Rows: []int{m.view[m.row]}}
				a.Status = kind + " 문 복사"
			}
		case "y":
			if hasRow {
				a = copyAction(cellOf(m.shown(m.view[m.row]), m.col), "셀")
			}
		case "Y":
			if hasRow {
				a = copyAction(RowTSV(m.shown(m.view[m.row])), "행(TSV)")
			}
		case "e":
			a = m.export("csv")
		case "E":
			a = m.export("json")
		case "c":
			// 차트는 결과 원래 값만 그린다. 추가 행(view 의 결과 밖 인덱스)은 chart 가 건너뛴다
			out, err := chart.Render(m.res, m.view, m.col, m.w, max(0, m.h-1))
			if err != nil {
				a.Status = err.Error()
				break
			}
			m.mode, m.chart = modeChart, out
		case "V":
			if hasRow {
				m.sel, m.selRow, m.selCol = true, m.row, m.col
			}
		case "=":
			a.Compare = true
		case "C":
			a = m.inList(0, len(m.view)-1, m.col, m.col)
		case "ctrl+k":
			if hasRow {
				a.CopyMenu = true
			}
		case "D":
			a.Decrypt = true
		case "F", "R":
			// 다음 쿼리의 조건으로 쓸 값이라 보류 중인 바꿈 전, DB 에 있는 값을 넘긴다
			o := m.res.Origin
			switch {
			case !hasRow:
				a.Status = "따라갈 행 없음"
			case o == nil || len(o.Cols) != len(m.res.Cols):
				a.Status = "원본 테이블을 모르는 결과"
			case m.view[m.row] >= len(m.res.Rows):
				a.Status = "추가한 행은 아직 DB 에 없음"
			case ks == "F" && o.Cols[m.col] == "":
				a.Status = "외래 키 열이 아님"
			case ks == "F" && cellOf(m.res.Rows[m.view[m.row]], m.col) == nullText:
				a.Status = "참조 값이 NULL"
			default:
				src, row := m.res.Rows[m.view[m.row]], map[string]string{}
				for c, name := range o.Cols {
					if name != "" {
						row[name] = cellOf(src, c)
					}
				}
				a.Follow = &Follow{Col: o.Cols[m.col], Row: row, Reverse: ks == "R"}
			}
		case "ctrl+b":
			// F 처럼 보류 중인 바꿈 전, DB 에 있는 값을 넘긴다. 열 이름은 원본을 알면 별칭이 아닌 테이블 열 이름이다
			switch {
			case !hasRow:
				a.Status = "찾을 행 없음"
			case m.view[m.row] >= len(m.res.Rows):
				a.Status = "추가한 행은 아직 DB 에 없음"
			case cellOf(m.res.Rows[m.view[m.row]], m.col) == nullText:
				a.Status = "값이 NULL"
			case db.IsCappedCell(cellOf(m.res.Rows[m.view[m.row]], m.col)):
				a.Status = "잘린 값이라 찾지 않음"
			default:
				l := Linked{Col: m.res.Cols[m.col], Value: cellOf(m.res.Rows[m.view[m.row]], m.col)}
				if o := m.res.Origin; o != nil && len(o.Cols) == len(m.res.Cols) {
					l.Table = o.Table.Table
					l.Col = cmp.Or(o.Cols[m.col], l.Col)
				}
				a.Linked = &l
			}
		case "+", "-":
			switch {
			case !hasRow:
				a.Status = "거를 행 없음"
			case m.view[m.row] >= len(m.res.Rows):
				a.Status = "추가한 행은 아직 DB 에 없음"
			default:
				v := cellOf(m.res.Rows[m.view[m.row]], m.col)
				a.Filter = &CellFilter{Col: m.res.Cols[m.col], Value: v, Null: v == nullText, Negate: ks == "-"}
			}
		}
	}
	m.follow()
	// 선택 중에는 집계를 상태 줄에 보인다. 복사 결과 등 다른 글은 덮지 않는다
	if m.sel && m.mode == modeTable && a.Copy == nil && a.Status == "" {
		r0, r1, c0, c1 := m.selRect()
		if key := [5]int{r0, r1, c0, c1, m.rev}; !m.agg.ok || m.agg.key != key {
			var cells []string
			var numeric []bool
			for vi := r0; vi <= r1; vi++ {
				row := m.current(m.view[vi])
				for c := c0; c <= c1; c++ {
					cells = append(cells, cellOf(row, c))
					numeric = append(numeric, m.kinds[c] == kindNumber)
				}
			}
			m.agg = selAgg{key: key, text: resultdiff.Aggregate(cells, numeric).String(), ok: true}
		}
		a.Status = m.agg.text
	}
	return m, a
}

// ShowDiff 는 고정한 결과 base 에서 지금 결과로 바뀐 행을 보인다(R31.2). 보류 중인 바꿈은 넣지 않고 그대로 둔다.
// 열이 달라 비교할 수 없으면 오류를 돌려주고 화면은 그대로다. 통합은 오류 글을 상태 줄에 보인다.
func (m *Model) ShowDiff(base *db.Result) error {
	if m.res == nil {
		return errors.New("비교할 결과가 없음")
	}
	rep, err := resultdiff.Diff(base, m.res)
	if err != nil {
		return err
	}
	m.sel = false
	m.mode, m.diff = modeDiff, diffView{rep: rep}
	m.follow()
	return nil
}

// selRect 는 선택 사각형의 view 행·열 범위(양끝 포함)다. 커서는 follow 로 범위 안에 맞춘 뒤 부른다.
func (m Model) selRect() (r0, r1, c0, c1 int) {
	sr := clamp(m.selRow, len(m.view))
	return min(sr, m.row), max(sr, m.row), min(m.selCol, m.col), max(m.selCol, m.col)
}

// HandleMouse 는 grid 영역 기준 좌표의 마우스 입력을 처리한다.
// 클릭(왼쪽·오른쪽)은 칸 선택, 휠은 행 스크롤, shift+휠·가로 휠은 열 스크롤이다.
func (m *Model) HandleMouse(msg tea.MouseMsg) {
	e := tea.MouseEvent(msg)
	if m.res == nil || e.Action != tea.MouseActionPress {
		return
	}
	// 다른 칸을 누르면 편집 중인 값을 확정한다(DataGrip 과 같다)
	m.commitEdit()
	l := m.layout()
	table := m.mode == modeTable && len(m.res.Cols) > 0
	delta := 0
	switch e.Button {
	case tea.MouseButtonWheelUp, tea.MouseButtonWheelLeft:
		delta = -1
	case tea.MouseButtonWheelDown, tea.MouseButtonWheelRight:
		delta = 1
	}
	horizontal := e.Button == tea.MouseButtonWheelLeft || e.Button == tea.MouseButtonWheelRight || (e.Shift && delta != 0)

	switch {
	case horizontal && m.mode == modeDiff:
		m.diff.left += delta
	case delta != 0 && m.mode == modeDiff:
		m.diff.row += delta * wheelRows
	case horizontal && table:
		m.colTop = clamp(m.colTop+delta, len(m.res.Cols))
		// 커서 열을 새 화면 안으로 끌어온다. 그러지 않으면 follow 가 colTop 을 되돌린다
		vis := m.layout().cols
		last := m.colTop
		for _, s := range vis {
			if s.w == m.widths[s.col] {
				last = s.col
			}
		}
		m.col = min(max(m.col, m.colTop), last)
	case horizontal && len(m.res.Cols) == 0 && !m.wrap:
		m.textLeft += delta * textStep
	case delta != 0 && m.mode == modeCell && m.cell.treeOn:
		k := tea.KeyMsg{Type: tea.KeyDown}
		if delta < 0 {
			k.Type = tea.KeyUp
		}
		for range wheelRows {
			m.cell.tree, _ = m.cell.tree.Update(k)
		}
	case delta != 0 && m.mode == modeCell:
		m.cell.top += delta * wheelRows
	case delta != 0 && m.mode == modeDetail:
		m.detailField += delta
	case delta != 0 && len(m.res.Cols) == 0:
		m.textTop += delta * wheelRows
	case delta != 0 && table:
		body := max(1, l.body)
		m.rowTop = max(0, min(m.rowTop+delta*wheelRows, len(m.view)-body))
		m.row = min(max(m.row, m.rowTop), m.rowTop+body-1)
	case (e.Button == tea.MouseButtonLeft || e.Button == tea.MouseButtonRight) && table:
		// 오른쪽 클릭도 칸을 고른다. 통합이 그 칸에서 복사 고르기 창을 연다(R71.3)
		headTop := b2i(l.caption)
		bodyTop := headTop + l.headLines
		if e.Y < headTop || e.Y >= bodyTop+l.body {
			return
		}
		if e.Y >= bodyTop {
			r := m.rowTop + e.Y - bodyTop
			if r >= len(m.view) {
				return
			}
			m.row = r
		}
		for _, s := range l.cols {
			if e.X >= s.x && e.X < s.x+s.w+3 {
				m.col = s.col
			}
		}
	}
	m.follow()
}

// CycleSort 는 커서 열 정렬을 오름 → 내림 → 없음으로 돌린다(s·열 머리 클릭, R14.3). 다른 열이면 그 열 오름차순이다.
func (m *Model) CycleSort() {
	if m.res == nil {
		return
	}
	m.server = false
	if m.sortCol != m.col {
		m.sortCol, m.sortDir = m.col, sortAsc
	} else {
		m.sortDir = (m.sortDir + 1) % 3
	}
	m.rebuild()
}

// CurrentCol 은 커서 열 이름이다. 테이블 보기는 s 를 이 열의 서버 정렬로 바꾼다.
func (m Model) CurrentCol() string {
	if m.res == nil || m.col >= len(m.res.Cols) {
		return ""
	}
	return m.res.Cols[m.col]
}

// FocusCol 은 커서를 이름이 같은 열로 옮긴다. 테이블 보기가 페이지를 다시 읽어도 커서 열을 지키게 한다.
func (m *Model) FocusCol(name string) {
	if m.res == nil {
		return
	}
	if i := slices.Index(m.res.Cols, name); i >= 0 {
		m.col = i
		m.follow()
	}
}

// Summary 는 표 위에 보일 정렬·거르기 줄이다. 둘 다 없으면 빈 글이다.
//   - 예: "정렬 name ↑ · 거르기 status=ACTIVE · 12/200행 · esc 해제".
func (m Model) Summary() string {
	if m.res == nil || len(m.res.Cols) == 0 {
		return ""
	}
	var parts []string
	if m.sortDir != sortNone && m.sortCol < len(m.res.Cols) {
		parts = append(parts, "정렬 "+textutil.Printable(m.res.Cols[m.sortCol], false)+" "+sortArrows[m.sortDir])
	}
	if m.filter != "" {
		parts = append(parts, fmt.Sprintf("거르기 %s · %d/%d행 · esc 해제", textutil.Printable(m.filter, false), len(m.view), len(m.res.Rows)))
	}
	return strings.Join(parts, " · ")
}

// HeaderAt 은 grid 영역 기준 좌표가 표 머리(열 이름·타입 줄)의 어느 열 위인지 알린다. 열 머리 클릭 정렬이 쓴다.
func (m Model) HeaderAt(x, y int) (string, bool) {
	if m.res == nil || m.mode != modeTable || len(m.res.Cols) == 0 {
		return "", false
	}
	l := m.layout()
	headTop := b2i(l.caption)
	if y < headTop || y >= headTop+l.headLines {
		return "", false
	}
	for _, s := range l.cols {
		if x >= s.x && x < s.x+s.w+3 {
			return m.res.Cols[s.col], true
		}
	}
	return "", false
}

// CurrentRow 는 커서 행의 결과 값(보류 중인 바꿈 반영)이다. 행이 없으면 nil 이다.
func (m Model) CurrentRow() []string {
	if m.res == nil || m.row >= len(m.view) {
		return nil
	}
	return m.current(m.view[m.row])
}

// rebuild 는 거르기·정렬을 다시 적용한다. 커서가 가리키던 행이 남아 있으면 그 행을 계속 가리킨다.
func (m *Model) rebuild() {
	keep := -1
	if m.row < len(m.view) {
		keep = m.view[m.row]
	}
	m.view, m.row, m.agg = nil, 0, selAgg{}
	r := m.res
	if r == nil || len(r.Cols) == 0 {
		return
	}
	f := ParseFilter(m.filter, r.Cols)
	if m.server {
		f = Filter{Col: -1}
	}
	if (f.Op == OpContains || f.Op == OpNotContains) && f.Value != "" && m.folded == nil {
		m.folded = make([][]string, len(r.Rows))
		for i, row := range r.Rows {
			m.folded[i] = make([]string, min(len(row), len(m.kinds)))
			for c := range m.folded[i] {
				m.folded[i][c] = strings.ToLower(display(m.kinds[c], row[c]))
			}
		}
	}
	view := make([]int, 0, len(r.Rows))
	for i := range r.Rows {
		if !m.gone[i] && m.matches(f, i) {
			view = append(view, i)
		}
	}
	if m.sortDir != sortNone && m.sortCol < len(r.Cols) && !m.server {
		o := rowOrder{idx: view, cells: make([]string, len(r.Rows)), numeric: m.kinds[m.sortCol] == kindNumber, desc: m.sortDir == sortDesc}
		if o.numeric {
			o.nums, o.isNum = make([]float64, len(r.Rows)), make([]bool, len(r.Rows))
		}
		for i, row := range r.Rows {
			o.cells[i] = cellOf(row, m.sortCol)
			if o.numeric {
				v, err := strconv.ParseFloat(o.cells[i], 64)
				o.nums[i], o.isNum[i] = v, err == nil // 숫자로 못 읽는 값은 숫자 뒤로 보낸다
			}
		}
		sort.Stable(o)
	}
	// 추가 행은 거르기·정렬과 무관하게 끝에 둔다. 아직 값이 없어 거르면 바로 사라지기 때문이다
	for i := len(r.Rows); i < len(r.Rows)+m.inserts; i++ {
		if m.edits[i] != nil {
			view = append(view, i)
		}
	}
	m.view = view
	if i := slices.Index(view, keep); i >= 0 {
		m.row = i
	}
}

// reflow 는 폭이 바뀌거나 줄바꿈을 켜고 끌 때 줄을 다시 나눈다. 1MiB 텍스트를 매 렌더마다 나누지 않는다(E63).
func (m *Model) reflow() {
	if m.res != nil && len(m.res.Cols) == 0 {
		m.textLines, m.textWidth = m.text, 0
		if m.wrap {
			m.textLines = wrapLines(m.text, m.w)
		} else {
			for _, l := range m.text {
				m.textWidth = max(m.textWidth, ansi.StringWidth(l))
			}
		}
	}
	if m.mode == modeCell {
		m.cell.lines = wrapLines(strings.Split(m.cell.body, "\n"), m.w)
		if m.cell.hasTree {
			m.cell.tree.SetSize(m.w, max(0, m.h-2)) // 제목 + 안내 줄
		}
	}
	if m.mode == modeChart {
		// 결과가 같으므로 켤 때 통과한 숫자 열 검사는 다시 실패하지 않는다
		m.chart, _ = chart.Render(m.res, m.view, m.col, m.w, max(0, m.h-1))
	}
}

// follow 는 커서를 범위 안으로 맞추고 커서가 보이도록 스크롤한다.
func (m *Model) follow() {
	if m.res == nil {
		return
	}
	// 레코드·값 보기는 m.view[m.row] 를 읽는다. 바꿈을 버려 추가 행만 있던 표가 비면 표 보기로 돌아가 빈 인덱스를 읽지 않는다
	if m.mode == modeDetail || m.mode == modeCell {
		m.row = clamp(m.row, len(m.view))
		if len(m.view) == 0 {
			m.mode = modeTable
		}
	}
	// 바꿈을 버려 선택하던 추가 행이 모두 사라지면 선택도 끝낸다
	if len(m.view) == 0 {
		m.sel = false
	}
	l := m.layout()
	body := max(1, l.body)
	switch {
	case m.mode == modeDiff:
		dv := &m.diff
		dv.row = clamp(dv.row, len(dv.rep.Rows))
		dv.top = min(dv.top, dv.row)
		dv.top = max(dv.top, dv.row-body+1)
		dv.top = max(0, min(dv.top, len(dv.rep.Rows)-body))
		dv.left = clamp(dv.left, len(m.res.Cols))
	case m.mode == modeCell:
		m.cell.top = max(0, min(m.cell.top, len(m.cell.lines)-body))
	case m.mode == modeDetail:
		m.detailField = clamp(m.detailField, len(m.res.Cols))
		m.detailTop = min(m.detailTop, m.detailField)
		m.detailTop = max(m.detailTop, m.detailField-body+1, 0)
	case len(m.res.Cols) == 0:
		m.textTop = max(0, min(m.textTop, len(m.textLines)-body))
		m.textLeft = max(0, min(m.textLeft, m.textWidth-m.w))
	default:
		m.row = clamp(m.row, len(m.view))
		m.col = clamp(m.col, len(m.res.Cols))
		m.rowTop = min(m.rowTop, m.row)
		m.rowTop = max(m.rowTop, m.row-body+1)
		m.rowTop = max(0, min(m.rowTop, len(m.view)-body))
		m.colTop = min(m.colTop, m.col)
		for m.colTop < m.col {
			used := 0
			for c := m.colTop; c <= m.col; c++ {
				used += m.widths[c] + 3
			}
			if used <= l.avail {
				break
			}
			m.colTop++
		}
	}
}

func (m Model) layout() layout {
	var l layout
	switch {
	case m.res == nil:
		return l
	case m.mode == modeDiff:
		l.body = max(0, m.h-3) // 요약 + 열 이름 + 안내 줄
		return l
	case m.mode != modeTable:
		l.body = max(0, m.h-2) // 제목 + 안내 줄
		return l
	case len(m.res.Cols) == 0:
		l.body = max(0, m.h-1) // 위치 줄
		return l
	}
	l.caption = m.res.Text != ""
	l.headLines = 2
	if typed(m.res) {
		l.headLines = 3
	}
	l.body = max(0, m.h-l.headLines-1-b2i(l.caption))
	l.numW = len(strconv.Itoa(max(1, len(m.res.Rows)+m.inserts)))
	x := l.numW + 2
	l.avail = max(0, m.w-x)
	l.cols = m.spans(m.colTop, x)
	return l
}

// spans 는 x 자리부터 열 from 부터 폭 안에 드는 열이다. 표와 비교 화면이 같은 열 폭을 쓴다.
func (m Model) spans(from, x int) []span {
	var cols []span
	for c := from; c < len(m.res.Cols); c++ {
		seg := m.widths[c] + 3
		if x+seg <= m.w {
			cols = append(cols, span{col: c, x: x, w: m.widths[c]})
			x += seg
			continue
		}
		// 남은 자리가 여백 둘과 한 글자를 담으면 잘린 열로라도 보여 준다
		if rem := m.w - x; rem >= 4 {
			cols = append(cols, span{col: c, x: x, w: rem - 3})
		}
		break
	}
	return cols
}

func (m *Model) openCell(at cellAt) {
	row := m.shown(m.view[m.row])
	raw := cellOf(row, at.col)
	title := textutil.Printable(m.res.Cols[at.col], false)
	if typed(m.res) {
		title += " · " + textutil.Printable(m.res.Types[at.col], false)
	}
	title += fmt.Sprintf(" · 레코드 %d · %d바이트", m.row+1, len(raw))
	m.cell = cellView{title: title, raw: raw, body: textutil.Printable(db.PrettyJSON(raw), true), back: at.back}
	t := strings.TrimSpace(raw)
	if (typed(m.res) && jsonTypes[m.res.Types[at.col]]) || strings.HasPrefix(t, "{") || strings.HasPrefix(t, "[") {
		// 해석이 안 되거나 1MiB 를 넘으면 원문 보기 그대로다
		if tree, err := jsonview.New(raw); err == nil {
			m.cell.tree, m.cell.hasTree, m.cell.treeOn = tree, true, true
		}
	}
	m.mode = modeCell
	m.reflow()
}

func (m Model) export(format string) Action {
	sub := &db.Result{Cols: m.res.Cols, Types: m.res.Types, Rows: make([][]string, 0, len(m.view))}
	for _, i := range m.view {
		sub.Rows = append(sub.Rows, m.current(i))
	}
	content := ToCSV(sub)
	if format == "json" {
		content = ToJSON(sub, m.kinds)
	}
	return Action{
		Status: fmt.Sprintf("%s 내보내기 %d행", strings.ToUpper(format), len(sub.Rows)),
		Export: &Export{Format: format, Content: content, Rows: len(sub.Rows)},
	}
}

func (o rowOrder) Len() int      { return len(o.idx) }
func (o rowOrder) Swap(i, j int) { o.idx[i], o.idx[j] = o.idx[j], o.idx[i] }

// Less 는 NULL 을 방향과 무관하게 맨 뒤에 둔다(R14.3). 숫자로 못 읽는 값도 방향과 무관하게 숫자 뒤다.
// 숫자는 float64 로 비교하므로 15자리를 넘는 numeric 은 근사 순서다.
func (o rowOrder) Less(i, j int) bool {
	a, b := o.idx[i], o.idx[j]
	if na, nb := o.cells[a] == nullText, o.cells[b] == nullText; na || nb {
		return !na && nb
	}
	var c int
	switch {
	case o.numeric && o.isNum[a] && o.isNum[b]:
		c = cmp.Compare(o.nums[a], o.nums[b])
	case o.numeric && o.isNum[a] != o.isNum[b]:
		return o.isNum[a]
	default:
		c = strings.Compare(o.cells[a], o.cells[b])
	}
	if o.desc {
		c = -c
	}
	return c < 0
}

// columnKinds 는 열마다 정렬·정렬 방향·색을 정할 종류를 고른다.
// 타입이 없는 결과(Redis)는 값으로 추정한다: NULL 이 아닌 값이 하나 이상이고 모두 숫자면 숫자 열이다.
func columnKinds(r *db.Result) []kind {
	ks := make([]kind, len(r.Cols))
	for i := range r.Cols {
		if typed(r) {
			switch {
			case db.IsNumericType(r.Types[i]):
				ks[i] = kindNumber
			case r.Types[i] == "bool":
				ks[i] = kindBool
			}
			continue
		}
		seen, all := false, true
		for _, row := range r.Rows {
			v := cellOf(row, i)
			if v == nullText {
				continue
			}
			if _, err := strconv.ParseFloat(v, 64); err != nil {
				all = false
				break
			}
			seen = true
		}
		if seen && all {
			ks[i] = kindNumber
		}
	}
	return ks
}

// autoWidths 는 열 이름·타입·값 앞부분의 화면 폭으로 열 폭을 정한다. 정렬 표시(" ▲") 자리를 이름 폭에 미리 더해
// 정렬을 켜고 꺼도 폭이 흔들리지 않게 한다.
func autoWidths(r *db.Result, kinds []kind) []int {
	ws := make([]int, len(r.Cols))
	for i, c := range r.Cols {
		ws[i] = ansi.StringWidth(textutil.Printable(c, false)) + 2
		if i < len(r.Types) {
			ws[i] = max(ws[i], ansi.StringWidth(textutil.Printable(r.Types[i], false)))
		}
	}
	for _, row := range r.Rows {
		for i := range ws {
			ws[i] = max(ws[i], ansi.StringWidth(cellText(display(kinds[i], cellOf(row, i)), autoMaxWidth)))
		}
	}
	for i := range ws {
		ws[i] = min(autoMaxWidth, max(minColWidth, ws[i]))
	}
	return ws
}

// display 는 원문 대신 보여 줄 글자다. PostgreSQL 텍스트 형식의 bool 은 t/f 라 true/false 로 펼친다.
// 거르기도 이 글자로 비교해 화면에 보이는 대로 찾힌다.
func display(k kind, raw string) string {
	if k == kindBool {
		switch raw {
		case "t":
			return "true"
		case "f":
			return "false"
		}
	}
	return raw
}

// typed 는 열마다 타입이 실려 있는지 본다. Redis 결과처럼 타입이 없으면 값으로 추정한다.
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

func copyAction(text, what string) Action {
	return Action{Copy: &text, Status: fmt.Sprintf("%s 복사 (%d바이트)", what, len(text))}
}

// clamp 는 길이 n 인 목록의 인덱스로 맞춘다. 빈 목록이면 0.
func clamp(i, n int) int {
	return max(0, min(i, n-1))
}

func b2i(b bool) int {
	if b {
		return 1
	}
	return 0
}
