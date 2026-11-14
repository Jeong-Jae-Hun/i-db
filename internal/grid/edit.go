package grid

// ─── 결과 표 편집 (v3) ───
// 바꿈은 결과 행 인덱스에 붙여 두므로 정렬·거르기로 보이는 순서가 바뀌어도 원래 행을 따라간다.
// 추가 행은 결과 행 뒤에 이어지는 결합 인덱스(len(res.Rows)+k)를 쓰고, 거르기·정렬과 무관하게 표 끝에 둔다.

import (
	"maps"
	"slices"
	"unicode/utf8"

	"github.com/charmbracelet/lipgloss"
	"github.com/charmbracelet/x/ansi"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
)

const defaultText = "DEFAULT" // 추가 행에서 값을 정하지 않은 칸. INSERT 에서 빠져 DB 기본값을 받는다

// 같은 유일 키를 지우고 다시 넣는 묶음이 한 트랜잭션 안에서 부딪히지 않게 삭제를 먼저, 추가를 마지막에 둔다
var (
	submitOrder = []db.ChangeOp{db.ChangeDelete, db.ChangeUpdate, db.ChangeInsert}
	copyKinds   = map[string]string{"i": "insert", "U": "update"}
)

// CopyAs 는 고른 행을 SQL 문으로 복사해 달라는 요청이다. 문 만들기는 통합이 db 쪽 함수로 한다.
type CopyAs struct {
	Kind string // "insert" 또는 "update"
	Rows []int  // res.Rows 인덱스
}

// rowEdit 은 행 하나의 보류 중인 바꿈이다. vals 는 열 → 새 값이고 nil 은 NULL 이다.
// 추가 행에서 vals 에 없는 열은 INSERT 에서 빠진다.
type rowEdit struct {
	vals     map[int]*string
	deleted  bool
	inserted bool
}

// snap 은 판 rev 에서 행 하나의 바꿈 복사본이다. edit 이 nil 이면 그 판에 바꿈이 없었다.
type snap struct {
	rev  int
	edit *rowEdit
}

// cellEdit 은 칸 하나의 인라인 편집 상태다. 줄바꿈이 든 값도 그대로 지키려고 한 줄 입력 대신 룬 버퍼를 쓴다.
type cellEdit struct {
	row, col int
	buf      []rune
	cur      int
	null     bool // ctrl+n 으로 NULL 을 고른 상태. 글자를 치면 풀린다
	dirty    bool // 키로 값을 바꿨는지. 바꾸지 않고 확정하면 아무것도 기록하지 않는다
	cycled   bool // bool 칸을 space 로만 바꿨는지. 이때는 space 를 다시 눌러도 글자 대신 값을 돌린다
}

// Pending 은 제출할 행 바꿈 수다(수정·추가·삭제 각 행이 1건).
func (m Model) Pending() int {
	return len(m.edits)
}

// Revision 은 바꿈의 판이다. 통합이 실행을 시작할 때 적어 두고 결과가 올 때 그사이 고쳤는지 가른다.
func (m Model) Revision() int {
	return m.rev
}

// ClearChanges 는 보류 중인 바꿈을 모두 버린다.
func (m *Model) ClearChanges() {
	m.rev++
	m.edits, m.inserts, m.editing, m.log = map[int]*rowEdit{}, 0, false, map[int][]snap{}
	m.rebuild()
	m.follow()
}

// ClearSubmitted 는 판 rev 에서 시작한 제출이 성공한 뒤 그 제출에 담긴 바꿈만 버리고, 그 뒤에 고친 것은 남긴다.
// 수정 행은 낸 값을 새 원래 값으로 삼는다. 그래야 다음 제출의 옛값 확인이 DB 와 맞고, 그 뒤에 고친 칸만 다시 간다.
// 낸 추가 행은 DB 가 준 PK·기본값을 몰라 버린다. 낸 삭제 행은 DB 에 없으므로 표에서 감춘다.
// 돌려주는 값은 낸 뒤에 고쳐서 버린 추가·삭제 행 수다. 통합이 다시 읽은 뒤 고치라고 알린다.
// 단, 새 원래 값은 보낸 글자 그대로라 DB 가 값을 다르게 저장하면(숫자 정규화·트리거) 다음 제출의 옛값 확인이 실패한다.
// 실패는 묶음 전체를 되돌리므로 안전하고, 다시 읽으면 풀린다.
func (m *Model) ClearSubmitted(rev int) int {
	if m.res == nil {
		return 0
	}
	// 결과는 통합(탭 결과·고정 결과)과 함께 쓰므로 행 목록을 복사한 뒤 원래 값을 고친다
	res := *m.res
	res.Rows = slices.Clone(m.res.Rows)
	m.res = &res
	dropped := 0
	for i, log := range m.log {
		var sent *rowEdit
		for _, s := range log {
			if s.rev <= rev {
				sent = s.edit
			}
		}
		if sent == nil {
			continue
		}
		later := log[len(log)-1].rev > rev
		switch {
		case sent.inserted || sent.deleted:
			delete(m.edits, i)
			m.gone[i] = sent.deleted
			dropped += b2i(later)
		default:
			row := slices.Clone(m.res.Rows[i])
			for c, v := range sent.vals {
				row[c] = nullText
				if v != nil {
					row[c] = *v
				}
			}
			m.res.Rows[i] = row
			now := m.edits[i]
			if now == nil {
				continue
			}
			for c, v := range now.vals {
				if unchanged(v, row[c]) {
					delete(now.vals, c)
				}
			}
			if len(now.vals) == 0 && !now.deleted {
				delete(m.edits, i)
			}
		}
	}
	// 남은 바꿈을 지금 판의 기록으로 다시 시작한다. 다음 제출은 이 판 뒤에 시작하므로 이전 기록은 쓰이지 않는다
	m.rev++
	m.log = map[int][]snap{}
	for i := range m.edits {
		m.touch(i)
	}
	m.folded = nil
	m.rebuild()
	m.follow()
	return dropped
}

// touch 는 행 i 의 지금 바꿈을 현재 판으로 기록한다. 바꿈을 고친 곳은 모두 판을 올린 뒤 부른다.
func (m *Model) touch(i int) {
	var e *rowEdit
	if cur := m.edits[i]; cur != nil {
		e = &rowEdit{vals: maps.Clone(cur.vals), deleted: cur.deleted, inserted: cur.inserted}
	}
	m.log[i] = append(m.log[i], snap{rev: m.rev, edit: e})
}

// editBlock 은 결과 전체를 편집할 수 없는 이유다. 편집할 수 있으면 "" 다.
func (m Model) editBlock() string {
	switch {
	case m.res == nil || len(m.res.Cols) == 0:
		return "편집할 수 없음: 표 결과가 아님"
	case m.res.Origin == nil:
		return "편집할 수 없음: PK 를 모르는 결과 (한 테이블의 PK 를 모두 조회해야 함)"
	case m.pkCols == nil:
		return "편집할 수 없음: 결과에 PK 열이 모두 있지 않음"
	}
	return ""
}

// cellBlock 은 커서 행의 열 c 를 편집할 수 없는 이유다. 편집할 수 있으면 "" 다.
func (m Model) cellBlock(c int) string {
	if why := m.editBlock(); why != "" {
		return why
	}
	if len(m.view) == 0 {
		return "편집할 행 없음 (a 로 행 추가)"
	}
	if cellOf(m.res.Origin.Cols, c) == "" {
		return "편집할 수 없음: 식·계산 열"
	}
	if e := m.edits[m.view[m.row]]; e != nil && e.deleted {
		return "편집할 수 없음: 삭제 표시한 행 (u 로 되돌리기)"
	}
	// 푼 값을 보이며 고치면 입력 칸에 원래 값(암호문)이 떠 무엇을 고치는지 헷갈리고, 푼 값을 그대로 저장할 위험이 있다
	if m.decOn {
		return "편집할 수 없음: 복호화 보기 중 (D 로 원래 값)"
	}
	return ""
}

// current 는 보류 중인 바꿈을 얹은 행 값이다. 바꿈이 없으면 결과 행을 복사 없이 돌려준다.
// 추가 행에서 정하지 않은 칸(DB 기본값을 받을 칸)은 "" 다. NULL 로 두면 복사·내보내기·편집 시작이 NULL 을 정한 칸과
// 구별하지 못한다. 표와 레코드 보기는 이 칸을 DEFAULT 로 그리고, 복사·내보내기는 빈 값으로 싣는다.
func (m Model) current(i int) []string {
	var base []string
	if i < len(m.res.Rows) {
		base = m.res.Rows[i]
	}
	e := m.edits[i]
	if e == nil || (len(e.vals) == 0 && !e.inserted) {
		return base
	}
	row := make([]string, len(m.res.Cols))
	for c := range row {
		row[c] = cellOf(base, c) // 추가 행은 base 가 nil 이라 "" 다
		if v, ok := e.vals[c]; ok {
			row[c] = nullText
			if v != nil {
				row[c] = *v
			}
		}
	}
	return row
}

// addRow 는 추가 행을 만들고 커서를 그 행으로 옮긴다.
func (m *Model) addRow(vals map[int]*string) {
	i := len(m.res.Rows) + m.inserts
	m.inserts++
	m.rev++
	m.edits[i] = &rowEdit{vals: vals, inserted: true}
	m.touch(i)
	m.rebuild()
	m.row = slices.Index(m.view, i)
}

// commitEdit 은 인라인 편집을 끝내고 값을 기록한다. 원래 값으로 돌아온 칸은 바꿈에서 뺀다.
func (m *Model) commitEdit() {
	if !m.editing {
		return
	}
	m.editing = false
	e := m.edit
	if !e.dirty {
		return
	}
	var v *string
	if !e.null {
		s := string(e.buf)
		v = &s
	}
	m.rev++
	m.put(e.row, e.col, v)
	m.touch(e.row)
}

// put 은 행 i 열 c 에 새 값 v(nil 은 NULL)를 보류 중인 바꿈으로 둔다. 원래 값으로 돌아온 칸은 바꿈에서 뺀다.
// 판 올리기와 기록(touch)은 부르는 쪽이 한다. 붙여넣기는 여러 칸을 한 판으로 묶는다.
func (m *Model) put(i, c int, v *string) {
	re := m.edits[i]
	if re == nil {
		re = &rowEdit{}
		m.edits[i] = re
	}
	if re.vals == nil {
		re.vals = map[int]*string{}
	}
	re.vals[c] = v
	if re.inserted {
		return
	}
	if unchanged(v, cellOf(m.res.Rows[i], c)) {
		delete(re.vals, c)
	}
	if len(re.vals) == 0 && !re.deleted {
		delete(m.edits, i)
	}
}

// changeSet 은 보류 중인 바꿈을 제출 묶음으로 만든다. update·delete 의 Key 는 편집 전 PK 값이다.
// 같은 종류 안에서는 행 인덱스 순서(결과 순서, 추가한 순서)다.
func (m Model) changeSet() db.ChangeSet {
	o := m.res.Origin
	cs := db.ChangeSet{Table: o.Table, PK: o.PK}
	idx := slices.Sorted(maps.Keys(m.edits))
	for _, op := range submitOrder {
		for _, i := range idx {
			e := m.edits[i]
			ch := db.Change{Op: db.ChangeUpdate}
			switch {
			case e.inserted:
				ch.Op = db.ChangeInsert
			case e.deleted:
				ch.Op = db.ChangeDelete
			}
			if ch.Op != op {
				continue
			}
			if !e.inserted {
				ch.Key = make(map[string]string, len(o.PK))
				for k, name := range o.PK {
					ch.Key[name] = cellOf(m.res.Rows[i], m.pkCols[k])
				}
				// 옛값: 수정은 바꾸는 열, 삭제는 PK 가 아닌 모든 테이블 열. 그새 바뀐 행·다른 행을 고치지 않게 db 가 WHERE 로 확인한다
				ch.Old = map[string]*string{}
				for c, name := range o.Cols {
					_, edited := e.vals[c]
					if name == "" || (!e.deleted && !edited) || (e.deleted && slices.Contains(o.PK, name)) {
						continue
					}
					v := cellOf(m.res.Rows[i], c)
					// 잘린 칸은 원래 값과 같을 수 없어 확인에서 뺀다
					if len(v) > db.MaxCellBytes {
						continue
					}
					// 결과는 NULL 을 "NULL" 글자로 싣는다. 표가 NULL 로 보여 준 대로 NULL 로 확인한다(진짜 'NULL' 글자면 0행이 되어 안전하게 실패한다)
					ch.Old[name] = &v
					if v == nullText {
						ch.Old[name] = nil
					}
				}
			}
			if !e.deleted {
				ch.Values = make(map[string]*string, len(e.vals))
				for c, v := range e.vals {
					ch.Values[o.Cols[c]] = v
				}
			}
			cs.Changes = append(cs.Changes, ch)
		}
	}
	return cs
}

// unchanged 는 새 값 v 가 결과 칸 base 와 같은지다(nil 은 NULL).
// 결과는 NULL 을 "NULL" 글자로 싣기 때문에 원래 값이 "NULL" 이면 글자 "NULL" 입력은 바꿈으로 본다.
func unchanged(v *string, base string) bool {
	return (v == nil && base == nullText) || (v != nil && *v == base && base != nullText)
}

// render 는 편집 중인 칸을 폭 w(+앞뒤 여백)로 그린다. 커서가 늘 보이도록 왼쪽을 잘라 낸다.
func (e cellEdit) render(w int, st lipgloss.Style) string {
	left := oneLine(string(e.buf[:e.cur]))
	glyph, right := " ", ""
	if e.cur < len(e.buf) {
		glyph, right = oneLine(string(e.buf[e.cur])), oneLine(string(e.buf[e.cur+1:]))
	}
	glyph = ansi.Truncate(glyph, max(0, w), "")
	gw := ansi.StringWidth(glyph)
	if lw := ansi.StringWidth(left); lw+gw > w {
		left = ansi.TruncateLeft(left, lw+gw-w, "")
	}
	// TruncateLeft 는 반쯤 걸친 넓은 글자(한글 등)를 남길 수 있어 한 글자씩 더 덜어 낸다
	for ansi.StringWidth(left)+gw > w {
		_, size := utf8.DecodeRuneInString(left)
		left = left[size:]
	}
	rest := max(0, w-ansi.StringWidth(left)-gw)
	tail := st.Render(fit(right, rest) + " ")
	if e.null && len(e.buf) == 0 {
		tail = st.Foreground(colFaint).Italic(true).Render(fit(nullText, rest) + " ")
	}
	return st.Render(" "+left) + cursorStyle.Render(glyph) + tail
}
