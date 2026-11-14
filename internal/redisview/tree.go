// Package redisview 는 Redis 키를 이름공간 트리로 묶고 값 하나를 편집하는 화면 부품이다.
// 네트워크를 쓰지 않는다. 쓰기는 실행할 명령 인자(argv)만 만들고 실행·prod 확인은 통합(ui.go)이 한다.
package redisview

import (
	"cmp"
	"slices"
	"strconv"
	"strings"

	"github.com/Jeong-Jae-Hun/i-db/internal/textutil"
)

const (
	separator = ":"
	// 이보다 깊은 조각은 나누지 않고 나머지를 키 이름 하나로 둔다. 깊은 키가 트리를 끝없이 들여 쓰지 않게 한다
	maxDepth = 8
)

// Node 는 이름공간 트리의 마디다. 폴더의 Full 은 구분자까지 붙인 접두어(`user:`)라 SCAN 패턴(`user:*`)으로
// 바로 쓸 수 있고, 같은 이름의 키(`user`)와 겹치지 않는다. 키의 Full 은 원래 키다.
type Node struct {
	Name     string
	Full     string
	IsKey    bool
	Count    int // 아래에 있는 키 수. 키 자신은 1
	Children []Node
}

// Row 는 펼침을 반영해 한 줄로 그릴 마디다.
type Row struct {
	Name     string
	Full     string
	IsKey    bool
	Count    int
	Depth    int
	Expanded bool
}

// Viewport 는 트리를 그릴 크기와 커서 줄이다.
type Viewport struct {
	Width  int
	Height int
	Cursor int
}

type trie struct {
	folders map[string]*trie
	keys    map[string]string // 마지막 조각 → 원래 키
	count   int
}

type flattener struct {
	expanded map[string]bool
	rows     []Row
}

// BuildTree 는 키를 `:` 로 나눠 폴더 → 키 트리로 묶는다. 키이면서 접두어인 이름(`a` 와 `a:b`)은
// 키와 폴더 둘 다로 나온다. 빈 조각(`a::b`, 끝의 `:`)도 이름이 빈 마디로 그대로 둔다.
func BuildTree(keys []string) []Node {
	root := &trie{}
	seen := make(map[string]bool, len(keys))
	for _, key := range keys {
		if seen[key] {
			continue
		}
		seen[key] = true
		parts := strings.SplitN(key, separator, maxDepth+1)
		t := root
		t.count++
		for _, p := range parts[:len(parts)-1] {
			if t.folders == nil {
				t.folders = map[string]*trie{}
			}
			next, ok := t.folders[p]
			if !ok {
				next = &trie{}
				t.folders[p] = next
			}
			next.count++
			t = next
		}
		if t.keys == nil {
			t.keys = map[string]string{}
		}
		t.keys[parts[len(parts)-1]] = key
	}
	return root.nodes("")
}

// Flatten 은 expanded 에 Full 이 있는 폴더만 펼쳐 그릴 줄 목록을 만든다.
func Flatten(nodes []Node, expanded map[string]bool) []Row {
	f := &flattener{expanded: expanded}
	f.walk(nodes, 0)
	return f.rows
}

// RowLabel 은 들여쓰기 없이 줄 하나의 글자를 만든다. 키 이름의 제어 문자는 보이는 글자로 바꾼다.
func RowLabel(r Row) string {
	name := textutil.Printable(r.Name, false)
	if name == "" {
		name = `""`
	}
	if r.IsKey {
		return "  " + name
	}
	icon := "▸ "
	if r.Expanded {
		icon = "▾ "
	}
	return icon + name + " (" + strconv.Itoa(r.Count) + ")"
}

// RenderTree 는 커서 줄이 보이도록 밀어 v.Height 줄, 줄마다 v.Width 칸 안으로 그린다.
func RenderTree(rows []Row, v Viewport) string {
	if v.Width <= 0 || v.Height <= 0 {
		return ""
	}
	top := max(0, v.Cursor-v.Height+1)
	end := min(len(rows), top+v.Height)
	lines := make([]string, 0, v.Height)
	for i := top; i < end; i++ {
		mark := "  "
		if i == v.Cursor {
			mark = "▌ "
		}
		lines = append(lines, fit(mark+strings.Repeat("  ", rows[i].Depth)+RowLabel(rows[i]), v.Width))
	}
	if len(rows) == 0 {
		lines = append(lines, fit("(키 없음)", v.Width))
	}
	return strings.Join(lines, "\n")
}

// compareNodes 는 폴더를 키보다 앞에 두고, 같은 종류끼리는 숫자를 크기로 비교하는 자연 정렬을 한다(user:2 < user:10).
func compareNodes(a, b Node) int {
	if a.IsKey != b.IsKey {
		if a.IsKey {
			return 1
		}
		return -1
	}
	return naturalCompare(a.Name, b.Name)
}

// naturalCompare 는 연속한 숫자를 수로 비교한다. 수가 같고 표기만 다르면(007 과 7) 바이트 순으로 갈라 순서를 고정한다.
func naturalCompare(a, b string) int {
	i, j := 0, 0
	for i < len(a) && j < len(b) {
		if isDigit(a[i]) && isDigit(b[j]) {
			si, sj := i, j
			for i < len(a) && isDigit(a[i]) {
				i++
			}
			for j < len(b) && isDigit(b[j]) {
				j++
			}
			na := strings.TrimLeft(a[si:i], "0")
			nb := strings.TrimLeft(b[sj:j], "0")
			if c := cmp.Compare(len(na), len(nb)); c != 0 {
				return c
			}
			if c := strings.Compare(na, nb); c != 0 {
				return c
			}
			continue
		}
		if c := cmp.Compare(a[i], b[j]); c != 0 {
			return c
		}
		i++
		j++
	}
	if c := cmp.Compare(len(a)-i, len(b)-j); c != 0 {
		return c
	}
	return strings.Compare(a, b)
}

func isDigit(c byte) bool { return c >= '0' && c <= '9' }

func (t *trie) nodes(prefix string) []Node {
	out := make([]Node, 0, len(t.folders)+len(t.keys))
	for name, child := range t.folders {
		full := prefix + name + separator
		out = append(out, Node{Name: name, Full: full, Count: child.count, Children: child.nodes(full)})
	}
	for name, key := range t.keys {
		out = append(out, Node{Name: name, Full: key, IsKey: true, Count: 1})
	}
	slices.SortFunc(out, compareNodes)
	return out
}

func (f *flattener) walk(nodes []Node, depth int) {
	for _, n := range nodes {
		open := !n.IsKey && f.expanded[n.Full]
		f.rows = append(f.rows, Row{Name: n.Name, Full: n.Full, IsKey: n.IsKey, Count: n.Count, Depth: depth, Expanded: open})
		if open {
			f.walk(n.Children, depth+1)
		}
	}
}
