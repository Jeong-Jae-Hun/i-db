// Package editor 는 SQL·Redis 명령용 여러 줄 편집기다. bubbles textarea 대신 줄 번호, 구문 색,
// 선택, 되돌리기, 자동완성 팝업, 마우스를 한 컴포넌트에서 처리한다. 위치는 모두 Value() 기준 바이트다.
package editor

import (
	"cmp"
	"encoding/base64"
	"fmt"
	"io"
	"maps"
	"os"
	"regexp"
	"slices"
	"sort"
	"strings"
	"time"
	"unicode"
	"unicode/utf8"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
)

const (
	LangSQL Lang = iota
	LangRedis
)

const (
	mvLeft moveKind = iota
	mvRight
	mvUp
	mvDown
	mvHome
	mvEnd
	mvWordLeft
	mvWordRight
	mvDocStart
	mvDocEnd
	mvPageUp
	mvPageDown
)

const (
	opNone opKind = iota
	opType
	opSpace
	opBackspace
	opDelete
	opOther
)

const (
	tabCells    = 4
	indentUnit  = "    "
	undoLimit   = 200
	popupRows   = 10
	popupMaxW   = 60
	wheelStep   = 3
	doubleClick = 400 * time.Millisecond
	commentMark = "-- "
	// 자동완성 후보 중 템플릿의 Kind. 고르면 글을 넣지 않고 템플릿을 펼친다
	templateKind = "template"
)

var (
	// 테스트가 시각과 OSC 52 출력처를 바꿔 끼운다
	now                    = time.Now
	clipboardOut io.Writer = os.Stdout

	// 터미널은 붙여넣기 줄바꿈을 \r 로 보낸다
	pasteNewlines = strings.NewReplacer("\r\n", "\n", "\r", "\n")

	moveKeys = map[string]motion{
		"left": {mvLeft, false}, "shift+left": {mvLeft, true},
		"right": {mvRight, false}, "shift+right": {mvRight, true},
		"up": {mvUp, false}, "shift+up": {mvUp, true},
		"down": {mvDown, false}, "shift+down": {mvDown, true},
		"home": {mvHome, false}, "shift+home": {mvHome, true},
		"end": {mvEnd, false}, "shift+end": {mvEnd, true},
		"ctrl+left": {mvWordLeft, false}, "alt+left": {mvWordLeft, false}, "ctrl+shift+left": {mvWordLeft, true},
		"ctrl+right": {mvWordRight, false}, "alt+right": {mvWordRight, false}, "ctrl+shift+right": {mvWordRight, true},
		"ctrl+home": {mvDocStart, false}, "ctrl+shift+home": {mvDocStart, true},
		"ctrl+end": {mvDocEnd, false}, "ctrl+shift+end": {mvDocEnd, true},
		"pgup": {mvPageUp, false}, "pgdown": {mvPageDown, false},
	}

	popupSteps = map[string]int{"up": -1, "down": 1, "ctrl+p": -1, "ctrl+n": 1, "pgup": -popupRows, "pgdown": popupRows}

	// 템플릿 자리 $NAME$. $$ 인용과 섞이지 않게 대문자 이름만 자리로 본다
	placeholderRe = regexp.MustCompile(`\$([A-Z_][A-Z0-9_]*)\$`)
)

// Lang 은 구문 색과 주석 토글 규칙을 고른다.
type Lang int

// Item 은 자동완성 후보 하나다. Insert 가 비면 Label 을 넣는다.
// PrefixLen 은 고를 때 바꿀 커서 앞 바이트 수다. 0 이면 커서 앞 단어 조각을 바꾼다.
type Item struct {
	Label, Kind, Detail, Insert string
	PrefixLen                   int
}

// ClipboardErrMsg 는 OSC 52 복사 쓰기가 실패했을 때 돌아오는 메시지다.
type ClipboardErrMsg struct {
	Err error
}

type moveKind int

type motion struct {
	kind   moveKind
	extend bool
}

type opKind int

type edit struct {
	start, end int
	text       string
	op         opKind
}

type snapshot struct {
	text        string
	cur, anchor int
}

// stop 은 펼친 템플릿의 자리 text[start:end] 다.
type stop struct {
	start, end int
}

type popup struct {
	open     bool
	items    []Item
	sel, top int
}

// Model 은 편집기 상태다. anchor 는 선택 시작점이며 -1 이면 선택이 없다.
type Model struct {
	text   string
	lines  []int
	toks   []token
	lang   Lang
	cur    int
	anchor int
	// 위아래 이동 때 지켜야 할 화면 열. -1 이면 현재 커서 열을 쓴다
	goalX     int
	w, h      int
	top, left int
	focused   bool

	undo, redo     []snapshot
	prevOp, lastOp opKind
	clip           string

	completer func(text string, offset int) []Item
	pop       popup

	templates map[string]string
	// 펼친 템플릿의 남은 자리. 첫 원소가 지금 자리이고 마지막은 $END$ 다. 비어 있으면 템플릿 모드가 아니다
	stops []stop

	dragging bool
	clickAt  time.Time
	clickOff int
}

func New() Model {
	m := Model{anchor: -1, goalX: -1}
	m.setText("")
	return m
}

// DefaultTemplates 는 JetBrains 식 SQL 라이브 템플릿이다. 부를 때마다 새 맵을 돌려준다.
func DefaultTemplates() map[string]string {
	return map[string]string{
		"sel":  "SELECT * FROM $TABLE$ $END$;",
		"selw": "SELECT * FROM $TABLE$ WHERE $END$;",
		"selc": "SELECT count(*) FROM $TABLE$ $END$;",
		"lim":  "LIMIT $END$",
		"ins":  "INSERT INTO $TABLE$ ($COLS$) VALUES ($END$);",
		"upd":  "UPDATE $TABLE$ SET $END$ WHERE ;",
		"del":  "DELETE FROM $TABLE$ WHERE $END$;",
		"cnt":  "SELECT count(*) FROM $TABLE$;",
	}
}

func (e ClipboardErrMsg) Error() string {
	return "clipboard write failed: " + e.Err.Error()
}

func (m *Model) SetSize(w, h int) {
	m.w, m.h = max(0, w), max(0, h)
	m.ensureVisible()
}

func (m *Model) Focus() {
	m.focused = true
}

func (m *Model) Blur() {
	m.focused, m.dragging, m.pop, m.stops = false, false, popup{}, nil
}

// SetTemplates 는 SQL 편집기에서 단어 뒤 Tab 으로 펼칠 템플릿을 바꾼다. 처음에는 없고 nil 이면 끈다.
func (m *Model) SetTemplates(t map[string]string) {
	m.templates, m.stops = maps.Clone(t), nil
}

// WantsTab 은 Tab 을 편집기가 쓸지다. 자동완성 창이 열렸거나, 템플릿 자리를 도는 중이거나, 커서 앞 단어가
// 템플릿 이름이면 true 다. 아니면 Tab 은 부모의 포커스 이동이다.
func (m Model) WantsTab() bool {
	_, _, ok := m.templateAt()
	return m.pop.open || len(m.stops) > 0 || ok
}

// SetValue 는 내용을 통째로 바꾸고 커서를 맨 앞에 둔다. 다른 파일을 연 것이므로 되돌리기 기록도 비운다.
func (m *Model) SetValue(s string) {
	m.setText(s)
	m.cur, m.anchor, m.goalX, m.top, m.left = 0, -1, -1, 0, 0
	m.undo, m.redo, m.lastOp, m.pop, m.stops = nil, nil, opNone, popup{}, nil
}

func (m Model) Value() string {
	return m.text
}

func (m Model) CursorOffset() int {
	return m.cur
}

// SetCursorOffset 는 커서를 바이트 위치 off 로 옮긴다(세션 복원 R41). 범위 밖이면 끝으로, 글자 가운데면 그 글자 앞으로 맞춘다.
func (m *Model) SetCursorOffset(off int) {
	off = max(0, min(off, len(m.text)))
	for off > 0 && off < len(m.text) && !utf8.RuneStart(m.text[off]) {
		off--
	}
	m.cur, m.anchor, m.goalX = off, -1, -1
	m.ensureVisible()
}

func (m Model) Selection() (start, end int, ok bool) {
	return m.selRange()
}

func (m *Model) SetLanguage(l Lang) {
	m.lang = l
	m.setText(m.text)
}

func (m *Model) SetCompleter(f func(text string, offset int) []Item) {
	m.completer = f
}

func (m Model) PopupOpen() bool {
	return m.pop.open
}

func (m *Model) setText(s string) {
	m.text = s
	// Update 가 값으로 복사되므로 이전 Model 과 배열을 나눠 쓰지 않게 새로 만든다
	lines := []int{0}
	for i := 0; i < len(s); i++ {
		if s[i] == '\n' {
			lines = append(lines, i+1)
		}
	}
	m.lines = lines
	if m.lang == LangRedis {
		m.toks = lexRedis(s)
	} else {
		m.toks = lexSQL(s)
	}
}

func (m Model) selRange() (start, end int, ok bool) {
	if m.anchor < 0 || m.anchor == m.cur {
		return 0, 0, false
	}
	return min(m.anchor, m.cur), max(m.anchor, m.cur), true
}

func (m Model) rowOf(off int) int {
	return sort.SearchInts(m.lines, off+1) - 1
}

// lineEnd 는 줄 끝 바이트 위치다. CRLF 의 \r 앞에서 멈춰 커서가 \r 과 \n 사이에 서지 않게 한다.
func (m Model) lineEnd(row int) int {
	end := len(m.text)
	if row+1 < len(m.lines) {
		end = m.lines[row+1] - 1
	}
	if end > m.lines[row] && m.text[end-1] == '\r' {
		end--
	}
	return end
}

func runeCells(r rune) int {
	switch {
	case r == '\t':
		return tabCells
	case r < 0x80 || unicode.IsControl(r):
		return 1
	}
	return lipgloss.Width(string(r))
}

// colOf 는 off 가 있는 줄에서 off 앞까지의 화면 칸 수다.
func (m Model) colOf(off int) int {
	x := 0
	for _, r := range m.text[m.lines[m.rowOf(off)]:off] {
		x += runeCells(r)
	}
	return x
}

// offsetAt 은 row 줄에서 화면 칸 x 를 덮는 글자의 시작 바이트를 돌려준다. 줄보다 오른쪽이면 줄 끝이다.
func (m Model) offsetAt(row, x int) int {
	row = max(0, min(row, len(m.lines)-1))
	end, col := m.lineEnd(row), 0
	for off := m.lines[row]; off < end; {
		r, size := utf8.DecodeRuneInString(m.text[off:end])
		w := runeCells(r)
		if col+w > x {
			return off
		}
		col, off = col+w, off+size
	}
	return end
}

func (m Model) gutterWidth() int {
	g := max(3, len(fmt.Sprint(len(m.lines)))) + 1
	if m.w < g+4 {
		return 0
	}
	return g
}

func (m Model) contentWidth() int {
	return max(0, m.w-m.gutterWidth())
}

func (m *Model) ensureVisible() {
	row := m.rowOf(m.cur)
	if row < m.top {
		m.top = row
	}
	if m.h > 0 && row >= m.top+m.h {
		m.top = row - m.h + 1
	}
	x, need := m.colOf(m.cur), 1
	if m.cur < m.lineEnd(row) {
		r, _ := utf8.DecodeRuneInString(m.text[m.cur:])
		need = max(1, runeCells(r))
	}
	if x < m.left {
		m.left = x
	}
	if cw := m.contentWidth(); cw > 0 && x+need > m.left+cw {
		m.left = x + need - cw
	}
}

// replace 는 모든 편집이 지나는 길이다. 연속 타이핑·연속 지우기는 되돌리기 한 번으로 묶는다.
func (m *Model) replace(e edit) {
	coalesce := e.op == m.prevOp && e.op != opOther && e.op != opNone
	if !coalesce {
		m.undo = append(m.undo, snapshot{text: m.text, cur: m.cur, anchor: m.anchor})
		if len(m.undo) > undoLimit {
			m.undo = append([]snapshot(nil), m.undo[len(m.undo)-undoLimit:]...)
		}
	}
	m.redo = nil
	if len(m.stops) > 0 {
		// 지금 자리 안의 편집은 자리를 늘리고 뒤 자리를 민다. 자리 밖을 고치면 템플릿 모드를 끝낸다
		c, d := m.stops[0], len(e.text)-(e.end-e.start)
		if e.start < c.start || e.end > c.end {
			m.stops = nil
		} else {
			stops := []stop{{c.start, c.end + d}}
			for _, s := range m.stops[1:] {
				if s.start >= c.end {
					s.start, s.end = s.start+d, s.end+d
				}
				stops = append(stops, s)
			}
			m.stops = stops
		}
	}
	m.setText(m.text[:e.start] + e.text + m.text[e.end:])
	m.cur, m.anchor, m.goalX, m.lastOp = e.start+len(e.text), -1, -1, e.op
	m.ensureVisible()
}

// selEdit 는 선택이 있으면 선택을, 없으면 커서 자리를 text 로 바꾸는 편집이다.
func (m Model) selEdit(text string, op opKind) edit {
	if s, e, ok := m.selRange(); ok {
		return edit{start: s, end: e, text: text, op: opOther}
	}
	return edit{start: m.cur, end: m.cur, text: text, op: op}
}

func (m *Model) restore(redo bool) {
	src, dst := &m.undo, &m.redo
	if redo {
		src, dst = &m.redo, &m.undo
	}
	if len(*src) == 0 {
		return
	}
	s := (*src)[len(*src)-1]
	*src = (*src)[:len(*src)-1]
	*dst = append(*dst, snapshot{text: m.text, cur: m.cur, anchor: m.anchor})
	m.setText(s.text)
	m.cur, m.anchor, m.goalX, m.pop, m.stops = s.cur, s.anchor, -1, popup{}, nil
	m.ensureVisible()
}

// selectedRows 는 선택(없으면 커서)이 걸친 줄 범위다. 선택이 다음 줄 맨 앞에서 끝나면 그 줄은 뺀다.
func (m Model) selectedRows() (first, last int) {
	s, e, ok := m.selRange()
	if !ok {
		row := m.rowOf(m.cur)
		return row, row
	}
	first, last = m.rowOf(s), m.rowOf(e)
	if last > first && e == m.lines[last] {
		last--
	}
	return first, last
}

func (m Model) newline() string {
	if strings.Contains(m.text, "\r\n") {
		return "\r\n"
	}
	return "\n"
}

// toggleComment 는 걸친 줄의 비어 있지 않은 줄이 모두 -- 로 시작하면 주석을 벗기고, 아니면 줄 맨 앞에 "-- " 를 붙인다.
func (m *Model) toggleComment() {
	first, last := m.selectedRows()
	start, end := m.lines[first], m.lineEnd(last)
	lines := strings.Split(m.text[start:end], "\n")
	commented, blank := true, true
	for _, l := range lines {
		t := l[len(indentOf(l)):]
		if strings.TrimSpace(t) == "" {
			continue
		}
		blank = false
		if !strings.HasPrefix(t, "--") {
			commented = false
		}
	}
	uncomment := commented && !blank
	_, _, hadSel := m.selRange()
	oldCur, cutAt, cutLen := m.cur, 0, 0
	for i, l := range lines {
		cutAt = len(indentOf(l))
		t := l[cutAt:]
		switch {
		case uncomment && strings.HasPrefix(t, commentMark):
			cutLen = len(commentMark)
			lines[i] = l[:cutAt] + t[cutLen:]
		case uncomment && strings.HasPrefix(t, "--"):
			cutLen = 2
			lines[i] = l[:cutAt] + t[cutLen:]
		case !uncomment && (len(lines) == 1 || strings.TrimSpace(t) != ""):
			lines[i] = commentMark + l
		}
	}
	block := strings.Join(lines, "\n")
	m.replace(edit{start: start, end: end, text: block, op: opOther})
	switch {
	case hadSel:
		m.anchor, m.cur = start, start+len(block)
	case !uncomment:
		m.cur = oldCur + len(commentMark)
	default:
		col := oldCur - start
		m.cur = oldCur
		if col > cutAt {
			m.cur = start + max(cutAt, col-cutLen)
		}
	}
	m.ensureVisible()
}

// duplicateLines 는 걸친 줄을 바로 아래에 복제하고 커서·선택을 복제본으로 옮긴다.
func (m *Model) duplicateLines() {
	first, last := m.selectedRows()
	end := m.lineEnd(last)
	add := m.newline() + m.text[m.lines[first]:end]
	oldCur, oldAnchor := m.cur, m.anchor
	m.replace(edit{start: end, end: end, text: add, op: opOther})
	m.cur = oldCur + len(add)
	if oldAnchor >= 0 {
		m.anchor = oldAnchor + len(add)
	}
	m.ensureVisible()
}

// complete 는 완성기 후보에 커서 앞 단어로 시작하는 템플릿을 더해 팝업을 연다. 이름이 꼭 맞는 템플릿은
// 맨 앞에, 앞부분만 맞는 템플릿은 맨 뒤에 둔다. sel 을 친 뒤 Tab 이 SELECT 가 아니라 템플릿을 펼치게 하려는 것이다.
// 템플릿 자리를 도는 중에는 템플릿을 더하지 않는다.
func (m *Model) complete() {
	var items []Item
	if m.completer != nil {
		items = m.completer(m.text, m.cur)
	}
	// 템플릿 자리 안에서 친 단어로 템플릿을 내밀면 고를 때 템플릿 안에 템플릿이 펼쳐진다
	if start := prefixStart(m.text, m.cur); m.lang == LangSQL && len(m.stops) == 0 && start < m.cur && (start == 0 || m.text[start-1] != '.') {
		word := m.text[start:m.cur]
		var more []Item
		for _, key := range slices.Sorted(maps.Keys(m.templates)) {
			it := Item{Label: key, Kind: templateKind, Detail: m.templates[key]}
			switch {
			case key == word:
				items = append([]Item{it}, items...)
			case strings.HasPrefix(key, word):
				more = append(more, it)
			}
		}
		items = append(items, more...)
	}
	if len(items) == 0 {
		m.pop = popup{}
		return
	}
	m.pop = popup{open: true, items: items}
}

// acceptStart 는 후보 it 를 고를 때 바꿀 구간의 시작이다. 완성기의 접두어는 따옴표·공백·':' 를 품을 수 있어
// 편집기의 단어 규칙 대신 PrefixLen 을 따른다.
func (m Model) acceptStart(it Item) int {
	if it.PrefixLen > 0 {
		start := max(0, m.cur-it.PrefixLen)
		for start < m.cur && !utf8.RuneStart(m.text[start]) {
			start++
		}
		return start
	}
	start := prefixStart(m.text, m.cur)
	// "Us 처럼 따옴표를 연 채 고르면 따옴표째 바꿔 "" 가 겹치지 않게 한다
	if strings.HasPrefix(cmp.Or(it.Insert, it.Label), `"`) && start > 0 && m.text[start-1] == '"' {
		start--
	}
	return start
}

// expandAt 은 text[start:cur] 를 템플릿 body 로 바꾸고 첫 자리를 고른다.
func (m *Model) expandAt(start int, body string) {
	text, stops := expand(body)
	m.replace(edit{start: start, end: m.cur, text: text, op: opOther})
	for i := range stops {
		stops[i].start, stops[i].end = stops[i].start+start, stops[i].end+start
	}
	m.stops = stops
	m.selectStop()
}

// templateAt 은 커서 바로 앞 단어가 템플릿 이름이면 그 단어의 시작과 본문을 돌려준다.
// 선택이 없고, 단어 가운데가 아니고, t.sel 처럼 점 뒤가 아니어야 한다. Redis 편집기에서는 펼치지 않는다.
func (m Model) templateAt() (start int, body string, ok bool) {
	if _, _, sel := m.selRange(); sel || m.lang != LangSQL || len(m.templates) == 0 {
		return 0, "", false
	}
	if r, _ := utf8.DecodeRuneInString(m.text[m.cur:]); m.cur < len(m.text) && isWord(r) {
		return 0, "", false
	}
	start = prefixStart(m.text, m.cur)
	if start == m.cur || start > 0 && m.text[start-1] == '.' {
		return 0, "", false
	}
	body, ok = m.templates[m.text[start:m.cur]]
	return start, body, ok
}

// expand 는 템플릿 본문의 $NAME$ 를 소문자 이름으로 바꾼 글과 자리 목록(본문 기준 위치)을 돌려준다.
// $END$ 는 빈 자리로 맨 뒤에 두고, 없으면 글 끝을 $END$ 로 본다.
func expand(body string) (string, []stop) {
	var b strings.Builder
	var stops []stop
	end := stop{-1, -1}
	last := 0
	for _, loc := range placeholderRe.FindAllStringSubmatchIndex(body, -1) {
		b.WriteString(body[last:loc[0]])
		name := body[loc[2]:loc[3]]
		at := b.Len()
		last = loc[1]
		if name == "END" {
			if end.start < 0 {
				end = stop{at, at}
			}
			continue
		}
		b.WriteString(strings.ToLower(name))
		stops = append(stops, stop{at, b.Len()})
	}
	b.WriteString(body[last:])
	if end.start < 0 {
		end = stop{b.Len(), b.Len()}
	}
	return b.String(), append(stops, end)
}

// selectStop 은 지금 자리를 고른다. 빈 자리면 커서만 두고, 마지막($END$) 자리면 템플릿 모드를 끝낸다.
func (m *Model) selectStop() {
	c := m.stops[0]
	m.cur, m.anchor, m.goalX = c.end, c.start, -1
	if c.start == c.end {
		m.anchor = -1
	}
	if len(m.stops) == 1 {
		m.stops = nil
	}
	m.ensureVisible()
}

func indentOf(line string) string {
	return line[:len(line)-len(strings.TrimLeft(line, " \t"))]
}

func prefixStart(s string, off int) int {
	for off > 0 {
		r, size := utf8.DecodeLastRuneInString(s[:off])
		if !isWord(r) {
			break
		}
		off -= size
	}
	return off
}

// charClass 는 단어 이동의 경계를 정한다. 0 공백, 1 단어, 2 줄바꿈, 3 그 밖의 기호.
func charClass(r rune) int {
	switch {
	case r == ' ' || r == '\t':
		return 0
	case r == '\n' || r == '\r':
		return 2
	case isWord(r):
		return 1
	}
	return 3
}

func wordRight(s string, off int) int {
	if off >= len(s) {
		return len(s)
	}
	if r, size := utf8.DecodeRuneInString(s[off:]); charClass(r) == 2 {
		if strings.HasPrefix(s[off:], "\r\n") {
			return off + 2
		}
		return off + size
	}
	for off < len(s) {
		r, size := utf8.DecodeRuneInString(s[off:])
		if charClass(r) != 0 {
			break
		}
		off += size
	}
	if off >= len(s) {
		return off
	}
	r, _ := utf8.DecodeRuneInString(s[off:])
	class := charClass(r)
	if class == 2 {
		return off
	}
	for off < len(s) {
		r, size := utf8.DecodeRuneInString(s[off:])
		if charClass(r) != class {
			break
		}
		off += size
	}
	return off
}

func wordLeft(s string, off int) int {
	if off <= 0 {
		return 0
	}
	if r, size := utf8.DecodeLastRuneInString(s[:off]); charClass(r) == 2 {
		if strings.HasSuffix(s[:off], "\r\n") {
			return off - 2
		}
		return off - size
	}
	for off > 0 {
		r, size := utf8.DecodeLastRuneInString(s[:off])
		if charClass(r) != 0 {
			break
		}
		off -= size
	}
	if off <= 0 {
		return 0
	}
	r, _ := utf8.DecodeLastRuneInString(s[:off])
	class := charClass(r)
	if class == 2 {
		return off
	}
	for off > 0 {
		r, size := utf8.DecodeLastRuneInString(s[:off])
		if charClass(r) != class {
			break
		}
		off -= size
	}
	return off
}

// copyCmd 는 OSC 52 로 터미널 클립보드에 쓴다. SSH 너머에서도 로컬 클립보드로 간다.
func copyCmd(s string) tea.Cmd {
	return func() tea.Msg {
		_, err := fmt.Fprintf(clipboardOut, "\x1b]52;c;%s\a", base64.StdEncoding.EncodeToString([]byte(s)))
		if err != nil {
			return ClipboardErrMsg{Err: err}
		}
		return nil
	}
}

func (m Model) Update(msg tea.Msg) (Model, tea.Cmd) {
	k, ok := msg.(tea.KeyMsg)
	if !ok || !m.focused {
		return m, nil
	}
	m.prevOp, m.lastOp = m.lastOp, opNone
	ks := k.String()
	// bubbletea 1.3 은 빠르게 친 글자를 KeyRunes 하나로 묶어 보낸다. "end" 같은 묶음이 키 이름으로 읽히지 않게 한다
	if k.Type == tea.KeyRunes {
		ks = ""
	}

	if m.pop.open {
		n := len(m.pop.items)
		if d, ok := popupSteps[ks]; ok {
			next := m.pop.sel + d
			switch {
			case next < 0 && d == -1:
				next = n - 1
			case next >= n && d == 1:
				next = 0
			}
			m.pop.sel = max(0, min(next, n-1))
			if m.pop.sel < m.pop.top {
				m.pop.top = m.pop.sel
			}
			if m.pop.sel >= m.pop.top+popupRows {
				m.pop.top = m.pop.sel - popupRows + 1
			}
			return m, nil
		}
		switch ks {
		case "tab", "enter":
			// 템플릿 자리를 도는 중에는 Tab 이 언제나 다음 자리로 간다. 창 아래 Tab 처리에서 창을 닫는다
			if ks == "tab" && len(m.stops) > 0 {
				break
			}
			it := m.pop.items[m.pop.sel]
			m.pop = popup{}
			if body, ok := m.templates[it.Label]; ok && it.Kind == templateKind {
				m.stops = nil
				m.expandAt(m.acceptStart(it), body)
				return m, nil
			}
			m.replace(edit{start: m.acceptStart(it), end: m.cur, text: cmp.Or(it.Insert, it.Label), op: opOther})
			return m, nil
		case "esc":
			m.pop = popup{}
			return m, nil
		}
	}

	if k.Paste {
		m.pop = popup{}
		m.replace(m.selEdit(pasteNewlines.Replace(string(k.Runes)), opOther))
		return m, nil
	}

	if mv, ok := moveKeys[ks]; ok {
		m.pop, m.stops = popup{}, nil
		row := m.rowOf(m.cur)
		s, e, hasSel := m.selRange()
		target := m.cur
		switch mv.kind {
		case mvLeft:
			switch {
			case hasSel && !mv.extend:
				target = s
			case m.cur == m.lines[row] && row > 0:
				target = m.lineEnd(row - 1)
			case m.cur > 0:
				_, size := utf8.DecodeLastRuneInString(m.text[:m.cur])
				target = m.cur - size
			}
		case mvRight:
			switch {
			case hasSel && !mv.extend:
				target = e
			case m.cur == m.lineEnd(row) && row+1 < len(m.lines):
				target = m.lines[row+1]
			case m.cur < len(m.text):
				_, size := utf8.DecodeRuneInString(m.text[m.cur:])
				target = m.cur + size
			}
		case mvUp, mvDown, mvPageUp, mvPageDown:
			if m.goalX < 0 {
				m.goalX = m.colOf(m.cur)
			}
			step := max(1, m.h-1)
			switch mv.kind {
			case mvUp:
				step = -1
			case mvDown:
				step = 1
			case mvPageUp:
				step = -step
			}
			next := row + step
			switch {
			case next < 0:
				target = 0
			case next >= len(m.lines):
				target = m.lineEnd(len(m.lines) - 1)
			default:
				target = m.offsetAt(next, m.goalX)
			}
			if mv.kind == mvPageUp || mv.kind == mvPageDown {
				m.top = max(0, min(m.top+step, len(m.lines)-1))
			}
		case mvHome:
			// 스마트 홈: 들여쓰기 끝으로 가고, 이미 거기면 줄 맨 앞으로
			lineStart := m.lines[row]
			target = lineStart + len(indentOf(m.text[lineStart:m.lineEnd(row)]))
			if m.cur == target {
				target = lineStart
			}
		case mvEnd:
			target = m.lineEnd(row)
		case mvWordLeft:
			target = wordLeft(m.text, m.cur)
		case mvWordRight:
			target = wordRight(m.text, m.cur)
		case mvDocStart:
			target = 0
		case mvDocEnd:
			target = m.lineEnd(len(m.lines) - 1)
		}
		vertical := mv.kind == mvUp || mv.kind == mvDown || mv.kind == mvPageUp || mv.kind == mvPageDown
		if !vertical {
			m.goalX = -1
		}
		switch {
		case mv.extend && m.anchor < 0:
			m.anchor = m.cur
		case !mv.extend:
			m.anchor = -1
		}
		m.cur = target
		m.ensureVisible()
		return m, nil
	}

	if (k.Type == tea.KeyRunes || k.Type == tea.KeySpace) && !k.Alt && len(k.Runes) > 0 {
		s, op := string(k.Runes), opType
		if s == " " {
			op = opSpace
		}
		m.replace(m.selEdit(s, op))
		last, _ := utf8.DecodeLastRuneInString(s)
		if isWord(last) || last == '.' {
			m.complete()
		} else {
			m.pop = popup{}
		}
		return m, nil
	}

	var cmd tea.Cmd
	wasOpen := m.pop.open
	m.pop = popup{}
	switch ks {
	case "enter":
		// 선택을 바꾸는 줄바꿈이면 선택 앞에 남는 줄의 들여쓰기를 잇는다
		at := m.cur
		if s, _, ok := m.selRange(); ok {
			at = s
		}
		before := m.text[m.lines[m.rowOf(at)]:at]
		indent := indentOf(before)
		if strings.HasSuffix(strings.TrimRight(before, " \t"), "(") {
			indent += indentUnit
		}
		m.replace(m.selEdit(m.newline()+indent, opOther))
	case "backspace", "alt+backspace":
		if _, _, ok := m.selRange(); ok {
			m.replace(m.selEdit("", opOther))
			break
		}
		if m.cur == 0 {
			break
		}
		start := wordLeft(m.text, m.cur)
		if ks == "backspace" {
			_, size := utf8.DecodeLastRuneInString(m.text[:m.cur])
			start = m.cur - size
			if m.text[start] == '\n' && start > 0 && m.text[start-1] == '\r' {
				start--
			}
		}
		m.replace(edit{start: start, end: m.cur, op: opBackspace})
		if wasOpen {
			m.complete()
		}
	case "delete":
		if _, _, ok := m.selRange(); ok {
			m.replace(m.selEdit("", opOther))
			break
		}
		if m.cur >= len(m.text) {
			break
		}
		_, size := utf8.DecodeRuneInString(m.text[m.cur:])
		if strings.HasPrefix(m.text[m.cur:], "\r\n") {
			size = 2
		}
		m.replace(edit{start: m.cur, end: m.cur + size, op: opDelete})
	case "tab":
		// 템플릿 자리를 돌거나 커서 앞 템플릿 이름을 펼친다. 둘 다 아니면 Tab 은 부모 몫이라 아무것도 하지 않는다
		if len(m.stops) > 0 {
			m.stops = m.stops[1:]
			m.selectStop()
			break
		}
		if start, body, ok := m.templateAt(); ok {
			m.expandAt(start, body)
		}
	case "ctrl+a":
		m.anchor, m.cur = 0, len(m.text)
		m.ensureVisible()
	case "ctrl+_", "ctrl+/":
		if m.lang == LangSQL {
			m.toggleComment()
		}
	case "ctrl+d":
		m.duplicateLines()
	case "ctrl+z":
		m.restore(false)
	case "ctrl+y":
		m.restore(true)
	case "ctrl+x":
		s, e, ok := m.selRange()
		if !ok {
			// 선택이 없으면 현재 줄을 줄바꿈째 잘라낸다. 마지막 줄이면 앞 줄바꿈을 함께 지운다
			row := m.rowOf(m.cur)
			s, e = m.lines[row], len(m.text)
			switch {
			case row+1 < len(m.lines):
				e = m.lines[row+1]
			case row > 0:
				s = m.lines[row] - 1
				if s > 0 && m.text[s-1] == '\r' {
					s--
				}
			}
		}
		if s == e {
			break
		}
		m.clip = m.text[s:e]
		cmd = copyCmd(m.clip)
		m.replace(edit{start: s, end: e, op: opOther})
	case "ctrl+k":
		s, e, ok := m.selRange()
		if !ok {
			row := m.rowOf(m.cur)
			s, e = m.lines[row], m.lineEnd(row)
		}
		// 빈 줄을 복사하면 빈 OSC 52 가 시스템 클립보드를 지우는 터미널이 있다
		if s == e {
			break
		}
		m.clip = m.text[s:e]
		cmd = copyCmd(m.clip)
	case "ctrl+v":
		if m.clip != "" {
			m.replace(m.selEdit(m.clip, opOther))
		}
	case "ctrl+@":
		m.complete()
		m.ensureVisible()
	case "ctrl+up":
		m.top = max(0, m.top-1)
	case "ctrl+down":
		m.top = max(0, min(m.top+1, len(m.lines)-1))
	case "esc":
		m.anchor, m.stops = -1, nil
	}
	return m, cmd
}

// HandleMouse 는 편집기 내용 영역 왼쪽 위를 (0,0) 으로 바꾼 마우스 메시지를 받는다. 거터도 X 에 포함된다.
func (m *Model) HandleMouse(msg tea.MouseMsg) {
	m.lastOp = opNone
	dx, dy := 0, 0
	switch msg.Button {
	case tea.MouseButtonWheelUp:
		dy = -wheelStep
	case tea.MouseButtonWheelDown:
		dy = wheelStep
	case tea.MouseButtonWheelLeft:
		dx = -wheelStep
	case tea.MouseButtonWheelRight:
		dx = wheelStep
	}
	if dx != 0 || dy != 0 {
		if msg.Shift {
			dx, dy = dy, dx
		}
		// 팝업은 커서 줄에 붙어 있어 화면이 굴러 커서가 가려지면 그릴 자리가 없다
		m.pop = popup{}
		widest := 0
		for row := m.top; row < min(len(m.lines), m.top+max(1, m.h)); row++ {
			widest = max(widest, m.colOf(m.lineEnd(row)))
		}
		m.top = max(0, min(m.top+dy, len(m.lines)-1))
		m.left = max(0, min(m.left+dx, widest))
		return
	}

	row := m.top + msg.Y
	off := m.offsetAt(row, max(0, msg.X-m.gutterWidth()+m.left))
	switch {
	case msg.Button == tea.MouseButtonLeft && msg.Action == tea.MouseActionPress:
		m.pop, m.goalX, m.stops = popup{}, -1, nil
		if msg.Shift {
			if m.anchor < 0 {
				m.anchor = m.cur
			}
			m.cur, m.dragging = off, true
			m.ensureVisible()
			return
		}
		if now().Sub(m.clickAt) < doubleClick && off == m.clickOff {
			end := off
			for end < len(m.text) {
				r, size := utf8.DecodeRuneInString(m.text[end:])
				if !isWord(r) {
					break
				}
				end += size
			}
			m.anchor, m.cur, m.dragging, m.clickAt = prefixStart(m.text, off), end, false, time.Time{}
			m.ensureVisible()
			return
		}
		m.clickAt, m.clickOff = now(), off
		m.cur, m.anchor, m.dragging = off, off, true
		m.ensureVisible()
	case msg.Action == tea.MouseActionMotion && m.dragging:
		m.cur = off
		m.ensureVisible()
	case msg.Action == tea.MouseActionRelease:
		m.dragging = false
		if m.anchor == m.cur {
			m.anchor = -1
		}
	}
}

// IdentAt 은 커서 위(없으면 바로 앞) SQL 이름을 한정자까지 붙여 돌려준다: t, s.t, "Big Name", s."T" (R26).
// 따옴표는 남겨 부르는 쪽이 대소문자까지 그대로 찾게 한다. s.| 처럼 점 바로 뒤면 점 앞 이름이다.
// 문자열·주석·숫자 안이거나 공백 위, Redis 편집기면 "" 다. 경계는 색칠 토큰을 써서 lexSQL 과 같게 본다.
// 점 앞뒤에 공백이 있는 s . t 는 이어 보지 않는다.
func (m Model) IdentAt() string {
	if m.lang == LangRedis {
		return ""
	}
	start, end, ok := identSegment(m.text, m.toks, m.cur)
	if !ok && m.cur > 0 {
		r, size := utf8.DecodeLastRuneInString(m.text[:m.cur])
		start, end, ok = identSegment(m.text, m.toks, m.cur-size)
		if !ok && r == '.' && m.cur > size {
			_, before := utf8.DecodeLastRuneInString(m.text[:m.cur-size])
			start, end, ok = identSegment(m.text, m.toks, m.cur-size-before)
		}
	}
	if !ok {
		return ""
	}
	for start > 1 && m.text[start-1] == '.' {
		_, size := utf8.DecodeLastRuneInString(m.text[:start-1])
		s, e, ok := identSegment(m.text, m.toks, start-1-size)
		if !ok || e != start-1 {
			break
		}
		start = s
	}
	for end+1 < len(m.text) && m.text[end] == '.' {
		s, e, ok := identSegment(m.text, m.toks, end+1)
		if !ok || s != end+1 {
			break
		}
		end = e
	}
	return m.text[start:end]
}

// identSegment 는 off 의 글자가 속한 이름 조각(점으로 나뉜 한 칸)을 돌려준다. 따옴표 이름은 토큰 하나가 한 조각이고,
// 따옴표 없는 이름은 lexSQL 처럼 글자·숫자·_·$ 가 이어진 구간이다. 키워드도 조각이다(comments·data 같은 테이블 이름이 흔하다).
func identSegment(text string, toks []token, off int) (start, end int, ok bool) {
	if off < 0 || off >= len(text) {
		return 0, 0, false
	}
	if i := sort.Search(len(toks), func(i int) bool { return toks[i].end > off }); i < len(toks) && toks[i].start <= off {
		switch toks[i].role {
		case roleQuoted:
			return toks[i].start, toks[i].end, true
		case roleString, roleComment, roleNumber, roleParam, roleOperator:
			return 0, 0, false
		}
	}
	if r, _ := utf8.DecodeRuneInString(text[off:]); !isWord(r) && r != '$' {
		return 0, 0, false
	}
	start, end = off, off
	for start > 0 {
		r, size := utf8.DecodeLastRuneInString(text[:start])
		if !isWord(r) && r != '$' {
			break
		}
		start -= size
	}
	for end < len(text) {
		r, size := utf8.DecodeRuneInString(text[end:])
		if !isWord(r) && r != '$' {
			break
		}
		end += size
	}
	return start, end, true
}
