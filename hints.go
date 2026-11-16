package main

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"strings"

	"github.com/charmbracelet/lipgloss"

	"github.com/Jeong-Jae-Hun/i-db/internal/atomicfile"
	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/grid"
	"github.com/Jeong-Jae-Hun/i-db/internal/session"
	"github.com/Jeong-Jae-Hun/i-db/internal/textutil"
)

// ─── 키 안내 (R74) ───
// 상태 줄 키 안내는 지금 상태에서 실제로 듣는 키만 중요한 순서로 늘어놓고, 폭이 모자라면 뒤에서부터 버린다.

const hintSep = " · "

var (
	// 결과 표의 고칠 수 있는 결과에서 쓰는 키. a·Enter 는 커서 칸에 따라 resultHints 가 앞에 둔다
	editTail = []string{"d 삭제", "Ctrl+D 복제", "Ctrl+K 복사", "Ctrl+B 연결된 곳", "s 정렬", "/ 거르기"}
	// 칸 값 보기·나무·차트 같은 결과 영역의 다른 화면(grid.Screen)
	screenHints = map[grid.Screen][]string{
		grid.ScreenNone:      {"Shift+Tab 편집기", "Ctrl+R 실행", "F1 도움말"},
		grid.ScreenText:      {"↑↓ 스크롤", "w 줄바꿈", "y 복사", "p 고정", "? 키"},
		grid.ScreenFilter:    {"Enter 확정", "esc 취소"},
		grid.ScreenEdit:      {"Enter 확정", "esc 취소", "Ctrl+N NULL", "Ctrl+O 큰 편집기"},
		grid.ScreenSelect:    {"y 복사", "C IN 목록", "Ctrl+K 꼴 고르기", "esc 끝"},
		grid.ScreenRecord:    {"↑↓ 칸", "Enter 값", "y 값 복사", "Y 행 복사", "esc 표로"},
		grid.ScreenValue:     {"y 복사", "esc 닫기"},
		grid.ScreenValueJSON: {"t 나무", "y 복사", "esc 닫기"},
		grid.ScreenValueTree: {"Enter 접기·펴기", "y 값 복사", "Y 경로 복사", "t 원문", "esc 닫기"},
		grid.ScreenChart:     {"c 표로", "esc 표로"},
		grid.ScreenDiff:      {"↑↓ 줄", "←→ 열", "esc 닫기"},
	}
	// Redis 값 편집기. 고치기는 string·모음 타입, 항목 추가·지우기는 모음 타입(hash·list·set·zset)만 듣고
	// 그 밖의 타입은 TTL·이름·키 삭제만 된다(redisview.Value)
	redisHints     = []string{"t TTL", "r 이름 바꾸기", "D 키 지우기", "esc 닫기", "? 키"}
	redisTypeHints = map[string][]string{
		"string": {"e 고치기"},
		"hash":   {"e 고치기", "a 항목 추가", "d 항목 지우기"},
		"list":   {"e 고치기", "a 항목 추가", "d 항목 지우기"},
		"set":    {"e 고치기", "a 항목 추가", "d 항목 지우기"},
		"zset":   {"e 고치기", "a 항목 추가", "d 항목 지우기"},
	}
	redisEditHints = []string{"Enter 확정", "esc 취소"}
	redisAskHints  = []string{"y 삭제", "esc 취소"}
	whereHints     = []string{"Enter 적용", "esc 취소", "Tab 열 이름"}
	planHints      = []string{"Enter 접기·펴기", "t JSON", "esc 닫기", "? 키"}
	editorHints    = []string{"Ctrl+R 실행", "Ctrl+E 실행 계획", "F8 정렬", "F1 도움말", "Ctrl+B 정의", "Ctrl+Space 자동완성", "Ctrl+P 팔레트"}
	// 트리 줄 종류별. 접속 줄은 종류와 db 전용 여부에 따라 treeConnHints 가 만든다
	treeHints = map[nodeKind][]string{
		nGroup:   {"Enter 펼치기", "E 폴더 이름", "N 새 접속", "/ 거르기", "? 키"},
		nSchema:  {"Enter 펼치기", "F 검색", "= 비교", "r 새로고침", "/ 거르기", "? 키"},
		nTable:   {"Enter 열기", "d 구조", "D DDL", "I CSV 가져오기", "/ 거르기", "? 키"},
		nKey:     {"Enter 값 편집", "/ SCAN 패턴", "r 새로고침", "? 키"},
		nFolder:  {"Enter 펼치기", "X 키 삭제", "/ SCAN 패턴", "r 새로고침", "? 키"},
		nQueries: {"Enter 펼치기", "r 새로고침", "? 키"},
		nFile:    {"Enter 파일 열기", "r 새로고침", "? 키"},
		nInfo:    {"↑↓ 이동", "? 키"},
	}
	// 대시보드·흐름·보드 탭(R64·R69·R70)
	dashHints  = []string{"r 새로고침", "Enter 트리에서 고르기", "/ 거르기", "? 키", "Ctrl+W 닫기"}
	flowHints  = []string{"r 새로고침", "↑↓ 흐름", "←→ 상자", "Enter 상세", "? 키", "Ctrl+W 닫기"}
	boardHints = []string{"r 새로고침", "방향키·hjkl 패널", "Enter 쿼리 탭에 열기", "? 키", "Ctrl+W 닫기"}
)

// fitHints 는 항목을 앞에서부터 w 칸에 들어가는 만큼만 잇는다. 항목 중간에서 자르지 않는다.
func fitHints(items []string, w int) string {
	out := ""
	for _, it := range items {
		next := it
		if out != "" {
			next = out + hintSep + it
		}
		if lipgloss.Width(next) > w {
			break
		}
		out = next
	}
	return out
}

// hints 는 지금 포커스·상태의 키 안내 항목이다. 키 이름은 m.label 로 ⌘ 글자가 된다(R47.1).
func (m *model) hints() []string {
	t := m.cur()
	var items []string
	switch {
	case m.focus != fTree && t.dash != nil:
		items = dashHints
	case m.focus != fTree && t.flow != nil:
		items = flowHints
	case m.focus != fTree && t.board != nil:
		items = boardHints
	case m.focus == fEditor:
		items = editorHints
	case m.focus == fTree:
		items = m.treeHints()
	default:
		return m.resultHints(t)
	}
	return m.labels(items)
}

func (m *model) labels(items []string) []string {
	out := make([]string, len(items))
	for i, it := range items {
		out[i] = m.label(it)
	}
	return out
}

// treeHints 는 커서 줄 종류의 키다. 거르기가 걸려 있으면 esc 해제를 앞에 둔다.
func (m *model) treeHints() []string {
	n := m.current()
	var items []string
	switch {
	case n == nil:
		items = []string{"N 새 접속", "F1 도움말"}
	case n.kind == nConn:
		items = m.treeConnHints(m.conn(n.conn))
	case n.kind == nSource:
		if _, ok := m.snsTopic(n); ok {
			items = append(items, "F 흐름")
		}
		if n.openable {
			items = append(items, "Enter 미리 보기")
		}
		if n.hasKids {
			items = append(items, "l 펼치기")
		}
		items = append(items, "r 새로고침", "? 키")
	default:
		items = treeHints[n.kind]
	}
	if m.filter != "" {
		items = append([]string{"esc 거르기 해제"}, items...)
	}
	return items
}

// treeConnHints 는 접속 줄의 키다. 고치기·지우기는 db 전용 접속(R59)에서만 듣는다.
func (m *model) treeConnHints(c db.Conn) []string {
	own := editRefusal(c) == ""
	items := []string{"Enter 펼치기"}
	if own {
		items = append(items, "E 고치기")
	}
	items = append(items, "N 새 접속")
	switch {
	case c.Kind == db.KindRedis:
		items = append(items, "X 키 삭제")
		if own {
			items = append(items, "Alt+X 지우기")
		}
		items = append(items, "A/L/S 운영", "/ SCAN 패턴")
	case own:
		items = append(items, "X 지우기")
	}
	if c.Kind == db.KindPG {
		items = append(items, "A/L/S 운영", "B DB", "F 검색", "= 비교")
	}
	return append(items, "M 폴더", "r 새로고침", "? 키")
}

// resultHints 는 결과 영역의 키다. 입력 중·다른 화면이면 그 화면 키만, 표면 보류 변경 → 더 읽기 → 테이블 보기 →
// 편집·읽기 전용 순이다.
func (m *model) resultHints(t *queryTab) []string {
	sc := t.grid.Screen()
	switch {
	case t.redis != nil && t.redis.Confirming():
		return m.labels(redisAskHints)
	case t.redis != nil && t.redis.Editing():
		return m.labels(redisEditHints)
	case t.redis != nil:
		return m.labels(append(slices.Clone(redisTypeHints[t.redis.Type()]), redisHints...))
	case t.tv != nil && t.tv.TypingWhere() && len(m.whereRecall(t)) > 0:
		return m.labels(append(slices.Clone(whereHints), "↑↓ 이전 조건"))
	case t.tv != nil && t.tv.Typing():
		return m.labels(whereHints)
	case t.planOn:
		return m.labels(planHints)
	case sc == grid.ScreenFilter && len(m.filterRecall()) > 0:
		return m.labels(append(slices.Clone(screenHints[sc]), "↑↓ 이전 거르기"))
	case sc != grid.ScreenTable:
		return m.labels(screenHints[sc])
	}
	pg := t.viewConn >= 0 && m.conn(t.viewConn).Kind == db.KindPG
	var items []string
	if t.linked != nil && t.shown() == t.linked.res {
		items = append(items, "Enter 테이블 열기")
	}
	if _, _, why := m.moreReason(t); t.tv == nil && why == "" {
		items = append(items, "] 더 읽기")
	}
	if t.tv != nil {
		items = append(items, "[ ] 페이지", "w WHERE", "O 정렬식")
	}
	tail := editTail
	if t.tv != nil {
		// 테이블 보기의 / 는 WHERE 입력이라 w 와 겹친다
		tail = slices.DeleteFunc(slices.Clone(tail), func(s string) bool { return s == "/ 거르기" })
	}
	if !pg {
		tail = slices.DeleteFunc(slices.Clone(tail), func(s string) bool { return s == "Ctrl+B 연결된 곳" })
	}
	// 고칠 수 없는 칸(식 열·삭제 표시 행·복호화 보기)의 Enter 는 칸 값 보기다
	var enter []string
	switch {
	case t.grid.CellEditable():
		enter = []string{"Enter 고치기"}
	case t.grid.CurrentRow() != nil:
		enter = []string{"Enter 값"}
	}
	switch n := t.grid.Pending(); {
	case n > 0:
		items = append(append(append([]string{"Ctrl+S 제출", "u 되돌리기"}, enter...), "a 행 추가"), items...)
		items = append(items, tail...)
		return append([]string{fmt.Sprintf("변경 %d건", n)}, m.labels(append(items, "? 키"))...)
	case t.grid.Editable():
		items = append(append(append(items, "a 행 추가"), enter...), tail...)
	default:
		ro := []string{"s 정렬", "/ 거르기", "Ctrl+K 복사", "Ctrl+B 연결된 곳", "v 레코드", "o 값"}
		if !pg {
			ro = slices.DeleteFunc(ro, func(s string) bool { return s == "Ctrl+B 연결된 곳" })
		}
		if t.tv != nil {
			ro = slices.DeleteFunc(ro, func(s string) bool { return s == "/ 거르기" })
		}
		items = append(items, ro...)
		if pg && t.linked == nil {
			items = append(items, "편집: PK 있는 단일 테이블만")
		}
	}
	return m.labels(append(items, "? 키"))
}

// ─── 이 화면의 키 (?) ───

// keySections 는 ? 창에 보일 helpSections 인덱스다. 지금 포커스·탭에 맞는 묶음만 고른다.
func (m *model) keySections() []int {
	t := m.cur()
	var prefixes []string
	switch {
	case m.focus != fTree && t.dash != nil:
		prefixes = []string{"대시보드"}
	case m.focus != fTree && t.flow != nil:
		prefixes = []string{"흐름"}
	case m.focus != fTree && t.board != nil:
		prefixes = []string{"보드"}
	case m.focus == fTree:
		prefixes = []string{"트리"}
	case t.redis != nil:
		prefixes = []string{"Redis 값 편집"}
	case t.grid.Screen() == grid.ScreenValueTree || t.grid.Screen() == grid.ScreenValueJSON:
		prefixes = []string{"JSON 값 보기", "결과"}
	default:
		prefixes = []string{"결과"}
	}
	var out []int
	for _, p := range prefixes {
		out = append(out, slices.IndexFunc(helpSections, func(s helpSection) bool { return strings.HasPrefix(s.title, p) }))
	}
	return slices.DeleteFunc(out, func(i int) bool { return i < 0 })
}

// ─── 한 번만 보이는 팁 ───
// 처음 들어간 화면마다 상태 줄 키 안내 앞에 팁을 한 번 보인다. 본 팁은 세션 파일 옆 tips.json(0600)에 남긴다.
// DB_TIPS=0 이면 끈다. CLI·MCP 는 화면이 없어 보이지 않는다.

// tipTexts 는 화면 종류별 팁이다. 키 이름은 보일 때 m.label 로 바꾼다
var tipTexts = map[string]string{
	"result-edit": "팁: a 로 행 추가, Enter 로 고치고 Ctrl+S 로 제출 (? 로 이 화면의 키)",
	"result-ro":   "팁: ? 로 이 화면의 키를 볼 수 있음",
	"table":       "팁: w 로 WHERE, [ ] 로 페이지 (? 로 이 화면의 키)",
	"flow":        "팁: ←→ 로 상자를 고르고 Enter 로 상세 (? 로 이 화면의 키)",
	"board":       "팁: 방향키로 패널을 고르고 Enter 로 쿼리 탭에 열기 (? 로 이 화면의 키)",
}

// tipState 는 본 팁 목록이다. nil 이면 팁을 끈 것이다.
type tipState struct {
	path string
	Seen []string `json:"seen"`
}

// loadTips 는 본 팁 목록을 읽는다. DB_TIPS=0 이면 nil 이다. 파일이 없거나 깨졌으면 빈 목록으로 시작한다(팁은 다시 보여도 해가 없다).
func loadTips() *tipState {
	if os.Getenv("DB_TIPS") == "0" {
		return nil
	}
	sp, err := session.Path()
	if err != nil {
		return nil
	}
	ts := &tipState{path: filepath.Join(filepath.Dir(sp), "tips.json")}
	if data, err := os.ReadFile(ts.path); err == nil {
		_ = json.Unmarshal(data, ts)
	}
	return ts
}

// tipContext 는 지금 화면의 팁 종류다. 팁이 없는 화면이면 "" 다.
func (m *model) tipContext() string {
	t := m.cur()
	switch {
	case m.focus == fTree:
		return ""
	case t.flow != nil:
		return "flow"
	case t.board != nil:
		return "board"
	case m.focus != fResult || t.dash != nil || t.redis != nil || t.grid.Screen() != grid.ScreenTable:
		return ""
	case t.tv != nil:
		return "table"
	case t.grid.Editable():
		return "result-edit"
	}
	return "result-ro"
}

// hintWidth 는 상태 줄에서 키 안내가 쓸 수 있는 폭이다(상태 글 뒤 한 칸 띄우고 끝 한 칸 남김).
func (m *model) hintWidth() int {
	return m.w - lipgloss.Width(barStyle.Render(" "+textutil.Printable(m.status, false))) - 2
}

// noteTip 은 처음 들어간 화면이면 팁을 걸고 본 것으로 남긴다. 창이 떠 있거나 상태 줄에 팁이 다 들어가지 않으면
// 걸지도 남기지도 않아 다음 메시지에서 다시 본다. 남기지 못해도 화면은 그대로 쓴다.
func (m *model) noteTip() {
	if m.tips == nil || m.tip != "" || m.overlay != oNone || m.w < minWidth || m.h < minHeight {
		return
	}
	c := m.tipContext()
	if c == "" || slices.Contains(m.tips.Seen, c) {
		return
	}
	tip := m.label(tipTexts[c])
	if lipgloss.Width(tip) > m.hintWidth() {
		return
	}
	m.tip = tip
	m.tips.Seen = append(m.tips.Seen, c)
	data, err := json.Marshal(m.tips)
	if err == nil {
		err = os.MkdirAll(filepath.Dir(m.tips.path), 0o700)
	}
	if err == nil {
		err = atomicfile.Write(m.tips.path, ".tips-*.json", data, 0o600)
	}
	if err != nil {
		m.tips = nil // 남길 수 없으면 같은 팁을 거듭 보이지 않게 이번 실행에서 끈다
	}
}
