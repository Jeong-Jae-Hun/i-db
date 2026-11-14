// Package jsonview 는 칸 값 하나의 JSON 을 접이식 나무로 보여 준다(R32).
// 키 순서와 숫자 원문을 지키려고 map 으로 풀지 않고 json.Decoder 토큰으로 직접 나무를 만든다.
package jsonview

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"strconv"
	"strings"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
	"github.com/charmbracelet/x/ansi"

	"github.com/Jeong-Jae-Hun/i-db/internal/textutil"
)

const (
	maxBytes = 1 << 20 // 넘으면 나무를 만들지 않고 통합이 원문으로 보인다(R32.1)
	// 처음에는 깊이 openDepth 미만인 묶음(루트와 그 바로 아래)만 펼친다. 손자 줄까지 보여 생김새를 알 수 있고
	// 큰 배열 안의 객체들은 접혀 한 줄씩이라 화면이 넘치지 않는다
	openDepth = 2
)

const (
	kindObject kind = iota
	kindArray
	kindString
	kindNumber
	kindBool
	kindNull
)

// 밝은·어두운 터미널 배경 모두에서 읽히도록 색을 둘씩 둔다(grid 와 같은 256색 표)
var (
	colDim    = lipgloss.AdaptiveColor{Light: "245", Dark: "244"}
	colFaint  = lipgloss.AdaptiveColor{Light: "250", Dark: "240"}
	colAccent = lipgloss.AdaptiveColor{Light: "25", Dark: "117"}
	colNumber = lipgloss.AdaptiveColor{Light: "25", Dark: "111"}
	colString = lipgloss.AdaptiveColor{Light: "28", Dark: "114"}
	colBool   = lipgloss.AdaptiveColor{Light: "130", Dark: "214"}

	dimStyle = lipgloss.NewStyle().Foreground(colDim)
	keyStyle = lipgloss.NewStyle().Foreground(colAccent)
	selStyle = lipgloss.NewStyle().Reverse(true)

	valueStyles = map[kind]lipgloss.Style{
		kindObject: dimStyle,
		kindArray:  dimStyle,
		kindString: lipgloss.NewStyle().Foreground(colString),
		kindNumber: lipgloss.NewStyle().Foreground(colNumber),
		kindBool:   lipgloss.NewStyle().Foreground(colBool),
		kindNull:   lipgloss.NewStyle().Foreground(colFaint).Italic(true),
	}
)

type kind int

// Action 은 키 처리 뒤 통합이 할 일이다. grid.Action 처럼 복사는 원문만 넘기고 OSC 52 는 통합이 보낸다.
// Copy 가 nil 이면 복사 없음, Close 면 창을 닫는다.
type Action struct {
	Copy   *string
	Status string
	Close  bool
}

// node 는 JSON 값 하나다. 객체 멤버면 key, 배열 원소면 index(아니면 -1)를 갖는다.
// text 는 문자열이면 푼 글, 숫자면 원문, bool·null 이면 그 낱말이다. 묶음은 raw[start:end] 가 원문이다.
type node struct {
	kind       kind
	key        string
	index      int
	text       string
	start, end int64
	children   []*node
	parent     *node
	depth      int
	collapsed  bool
}

// Model 은 상태 바꾸기(Update·SetSize)에서 스크롤 위치를 맞추고, View 는 그리기만 한다.
type Model struct {
	raw      string
	root     *node
	rows     []*node // 접히지 않아 보이는 노드
	cur, top int
	w, h     int
}

// ─── Parse ───

// New 는 raw 를 나무로 읽는다. 1MiB 를 넘거나 JSON 이 아니면 오류이고, 통합은 원문 보기로 돌아간다.
// 스칼라 하나(jsonb 의 "abc"·1)도 받는다.
func New(raw string) (Model, error) {
	if len(raw) > maxBytes {
		return Model{}, fmt.Errorf("json value is %d bytes, over the %d byte tree limit", len(raw), maxBytes)
	}
	root, err := parse(raw)
	if err != nil {
		return Model{}, err
	}
	m := Model{raw: raw, root: root}
	m.rebuild()
	return m, nil
}

// parse 는 토큰을 읽어 나무를 만든다. 열린 묶음을 스택에 두어 깊게 중첩된 입력에도 재귀하지 않는다.
func parse(raw string) (*node, error) {
	dec := json.NewDecoder(strings.NewReader(raw))
	dec.UseNumber()
	var root *node
	var stack []*node
	pendingKey, haveKey := "", false
	for {
		tok, err := dec.Token()
		if err == io.EOF {
			break
		}
		if err != nil {
			return nil, fmt.Errorf("invalid json: %w", err)
		}
		if root != nil && len(stack) == 0 {
			return nil, errors.New("invalid json: extra data after top-level value")
		}
		parent := (*node)(nil)
		if len(stack) > 0 {
			parent = stack[len(stack)-1]
		}
		// 객체 안에서 값 자리가 아닌 문자열 토큰은 키다
		if s, ok := tok.(string); ok && parent != nil && parent.kind == kindObject && !haveKey {
			pendingKey, haveKey = s, true
			continue
		}
		if d, ok := tok.(json.Delim); ok && (d == '}' || d == ']') {
			parent.end = dec.InputOffset()
			stack = stack[:len(stack)-1]
			continue
		}
		n := &node{parent: parent, index: -1}
		if parent != nil {
			n.depth = parent.depth + 1
			if parent.kind == kindObject {
				n.key = pendingKey
			} else {
				n.index = len(parent.children)
			}
			parent.children = append(parent.children, n)
		}
		haveKey = false
		switch v := tok.(type) {
		case json.Delim:
			n.kind, n.start = kindArray, dec.InputOffset()-1
			if v == '{' {
				n.kind = kindObject
			}
			n.collapsed = n.depth >= openDepth
			stack = append(stack, n)
		case string:
			n.kind, n.text = kindString, v
		case json.Number:
			n.kind, n.text = kindNumber, v.String()
		case bool:
			n.kind, n.text = kindBool, strconv.FormatBool(v)
		default:
			n.kind, n.text = kindNull, "null"
		}
		if root == nil {
			root = n
		}
	}
	if root == nil {
		return nil, errors.New("invalid json: empty input")
	}
	if len(stack) > 0 {
		return nil, errors.New("invalid json: unexpected end of input")
	}
	return root, nil
}

// ─── Update ───

func (m *Model) SetSize(w, h int) {
	m.w, m.h = max(0, w), max(0, h)
	m.follow()
}

func (m Model) Update(k tea.KeyMsg) (Model, Action) {
	if k.String() == "esc" || k.String() == "q" {
		return m, Action{Close: true}
	}
	if len(m.rows) == 0 {
		return m, Action{}
	}
	n := m.rows[m.cur]
	switch k.String() {
	case "up", "k":
		m.cur--
	case "down", "j":
		m.cur++
	case "pgup":
		m.cur -= max(1, m.h)
	case "pgdown":
		m.cur += max(1, m.h)
	case "home", "g":
		m.cur = 0
	case "end", "G":
		m.cur = len(m.rows) - 1
	case "enter", " ":
		if container(n) {
			n.collapsed = !n.collapsed
			m.rebuild()
		}
	case "left", "h":
		if container(n) && !n.collapsed {
			n.collapsed = true
			m.rebuild()
			break
		}
		for i := m.cur - 1; i >= 0; i-- {
			if m.rows[i] == n.parent {
				m.cur = i
				break
			}
		}
	case "right", "l":
		switch {
		case n.collapsed:
			n.collapsed = false
			m.rebuild()
		case len(n.children) > 0:
			m.cur++
		}
	case "y":
		v := m.value(n)
		return m, Action{Copy: &v, Status: fmt.Sprintf("값 복사 (%d바이트)", len(v))}
	case "Y":
		p := path(n)
		return m, Action{Copy: &p, Status: "경로 복사 " + textutil.Printable(p, false)}
	}
	m.follow()
	return m, Action{}
}

// value 는 y 로 복사할 글이다. 묶음은 원문을 한 줄로 줄인 JSON(키 순서·숫자 원문 그대로), 문자열은 따옴표 없는 글이다.
func (m Model) value(n *node) string {
	if !container(n) {
		return n.text
	}
	var b bytes.Buffer
	// parse 를 통과한 원문 조각이라 Compact 는 실패하지 않는다
	_ = json.Compact(&b, []byte(m.raw[n.start:n.end]))
	return b.String()
}

// path 는 JSONPath 꼴 경로다. 식별자가 아닌 키는 $["a b"] 처럼 JSON 문자열로 감싼다.
func path(n *node) string {
	var parts []string
	for ; n.parent != nil; n = n.parent {
		switch {
		case n.parent.kind == kindArray:
			parts = append(parts, "["+strconv.Itoa(n.index)+"]")
		case identifier(n.key):
			parts = append(parts, "."+n.key)
		default:
			parts = append(parts, "["+quote(n.key)+"]")
		}
	}
	var b strings.Builder
	b.WriteString("$")
	for i := len(parts) - 1; i >= 0; i-- {
		b.WriteString(parts[i])
	}
	return b.String()
}

func identifier(s string) bool {
	if s == "" {
		return false
	}
	for i, r := range s {
		if !(r == '_' || r >= 'a' && r <= 'z' || r >= 'A' && r <= 'Z' || i > 0 && r >= '0' && r <= '9') {
			return false
		}
	}
	return true
}

// quote 는 JSON 문자열 표기다. 경로를 붙여 넣는 곳(jq·SQL)에서 < > & 가 < 로 바뀌면 안 되므로 HTML 이스케이프를 끈다.
func quote(s string) string {
	var b bytes.Buffer
	enc := json.NewEncoder(&b)
	enc.SetEscapeHTML(false)
	_ = enc.Encode(s)
	return strings.TrimSuffix(b.String(), "\n")
}

// ─── View ───

// View 는 w×h 안에 나무를 그린다. 줄 수는 h, 줄 폭은 w 를 넘지 않는다. 머리·도움말 줄은 통합이 붙인다.
func (m Model) View() string {
	if m.root == nil || m.w <= 0 || m.h <= 0 {
		return ""
	}
	end := min(len(m.rows), m.top+m.h)
	lines := make([]string, 0, end-m.top)
	for i, n := range m.rows[m.top:end] {
		marker := "  "
		switch {
		case container(n) && n.collapsed:
			marker = "▸ "
		case container(n):
			marker = "▾ "
		}
		indent := strings.Repeat("  ", n.depth) + marker
		key := "$"
		switch {
		case n.parent == nil:
		case n.parent.kind == kindArray:
			key = "[" + strconv.Itoa(n.index) + "]"
		default:
			key = textutil.Printable(n.key, false)
		}
		key += ": "
		val := summary(n)
		if m.top+i == m.cur {
			lines = append(lines, selStyle.Render(ansi.Truncate(indent+key+val, m.w, "…")))
			continue
		}
		ks := keyStyle
		if n.parent == nil || n.parent.kind == kindArray {
			ks = dimStyle
		}
		lines = append(lines, ansi.Truncate(indent+ks.Render(key)+valueStyles[n.kind].Render(val), m.w, "…"))
	}
	return strings.Join(lines, "\n")
}

// summary 는 한 줄에 보일 값이다. 묶음은 원소 수, 문자열은 따옴표와 보이는 글자로 바꾼 제어 문자다.
func summary(n *node) string {
	switch n.kind {
	case kindObject:
		return fmt.Sprintf("{%d}", len(n.children))
	case kindArray:
		return fmt.Sprintf("[%d]", len(n.children))
	case kindString:
		return `"` + textutil.Printable(n.text, false) + `"`
	}
	return n.text
}

// rebuild 는 접힘이 바뀐 뒤 보이는 줄을 다시 만들고 고른 노드를 따라간다.
func (m *Model) rebuild() {
	var sel *node
	if len(m.rows) > 0 {
		sel = m.rows[m.cur]
	}
	m.rows = m.rows[:0]
	m.appendRows(m.root)
	for i, n := range m.rows {
		if n == sel {
			m.cur = i
		}
	}
	m.follow()
}

func (m *Model) appendRows(n *node) {
	m.rows = append(m.rows, n)
	if n.collapsed {
		return
	}
	for _, c := range n.children {
		m.appendRows(c)
	}
}

func (m *Model) follow() {
	m.cur = max(0, min(m.cur, len(m.rows)-1))
	h := max(1, m.h)
	if m.cur < m.top {
		m.top = m.cur
	}
	if m.cur >= m.top+h {
		m.top = m.cur - h + 1
	}
	m.top = max(0, min(m.top, len(m.rows)-h))
}

func container(n *node) bool {
	return n.kind == kindObject || n.kind == kindArray
}
