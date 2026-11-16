package main

import (
	"errors"
	"fmt"
	"os"
	"slices"
	"strings"
	"sync"
	"time"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/finder"
	"github.com/Jeong-Jae-Hun/i-db/internal/recent"
	"github.com/Jeong-Jae-Hun/i-db/internal/tableview"
)

// ─── 최근 기록 (R75) ───
// 팔레트의 자주 쓴 동작·최근 접속, Ctrl+N 의 최근 테이블, 테이블 보기의 열 폭·정렬·WHERE, 입력 기록, 트리 펼침을
// 개인 상태 폴더의 recent.json 에 남긴다. m.recent 가 nil 이면 모두 하지 않는다(DB_RECENT=0·쓰기 실패·CLI·시험 기본).

const (
	paletteTopActions = 5  // 빈 팔레트 맨 위에 보일 자주 쓴 동작 수
	paletteTopConns   = 5  // 빈 팔레트에 보일 최근 접속 수. 나머지는 거를 때 나온다
	finderRecent      = 20 // Ctrl+N 맨 앞에 둘 최근 테이블 수
)

// recentConn 은 명령 팔레트의 최근 접속 항목이다.
type recentConn struct {
	label string
	conn  int
}

// recallState 는 ↑↓ 로 고른 입력 기록 자리다. pos 0 은 기록을 고르지 않은 것이고, draft 는 처음 ↑ 를 누를 때 치던 글이다.
type recallState struct {
	pos   int
	draft string
	tab   int // 고르기를 시작한 탭
}

// loadRecent 는 기억을 읽는다. DB_RECENT=0 이면 nil 이다. 읽지 못하면 빈 기억으로 시작하고 한 번 알린다.
func (m *model) loadRecent() {
	if os.Getenv("DB_RECENT") == "0" {
		return
	}
	path, err := recent.Path()
	if err != nil {
		return
	}
	st, err := recent.Load(path, m.proj.Dir)
	if err != nil && st != nil {
		m.addStatus("최근 기록을 읽지 못해 새로 시작: " + err.Error())
	}
	m.recent = st
	if st != nil {
		m.restoreGroups()
	}
}

// saveRecent 는 지금 기억의 사본을 떠 저장 고루틴에 넘긴다. 화면은 파일 잠금·쓰기를 기다리지 않는다.
// 실패는 recentSavedMsg 로 와서 이번 실행에서 기록을 끄고 한 번 알린다.
func (m *model) saveRecent() {
	if m.recent == nil {
		return
	}
	snap, err := m.recent.Snapshot()
	if err != nil {
		m.recent = nil
		m.status = "최근 기록 저장 실패, 이번 실행에서 끔: " + err.Error()
		return
	}
	if cmd := m.recentSaver.submit(snap); cmd != nil {
		m.recentCmd = cmd
	}
}

// saveRecentOnQuit 는 끝낼 때 트리 펼침을 남기고 기억을 쓴다. recentFlushWait 까지만 기다린다.
// 화면을 닫은 뒤라 실패는 터미널에 알린다.
func (m *model) saveRecentOnQuit() {
	if m.recent == nil {
		return
	}
	m.snapshotTree()
	m.saveRecent()
	m.recentCmd = nil
	if err := m.recentSaver.flush(recentFlushWait); err != nil {
		m.quitNotes = append(m.quitNotes, "recent save failed: "+err.Error())
	}
}

// recentSavedMsg 는 저장 고루틴이 할 일을 다 했다는 알림이다. err 는 그동안 난 첫 실패다.
type recentSavedMsg struct{ err error }

var (
	// writeRecent 는 기억 쓰기다. 시험이 막히는 쓰기로 바꿔 화면이 쓰기를 기다리지 않는지 본다
	writeRecent = (*recent.Snapshot).Write
	// recentFlushWait 는 끝낼 때 남은 쓰기를 기다리는 시간이다. 잠금 기다림(recent.LockWait)과 쓰기 몫을 더했다
	recentFlushWait = recent.LockWait + time.Second
	errRecentFlush  = errors.New("recent save still running at quit")
)

// recentSaver 는 기억 쓰기를 한 번에 하나씩 화면 밖에서 한다. 쓰는 동안 들어온 사본은 가장 새 것 하나만 남겨 다음에 쓴다.
// 첫 실패는 failed 에 남아 지워지지 않는다. 그 뒤 사본은 받지 않고, 끝 알림과 끝낼 때 flush 가 같은 실패를 본다.
type recentSaver struct {
	mu     sync.Mutex
	next   *recent.Snapshot
	idle   chan struct{} // 돌고 있는 고루틴이 끝나면 닫힌다. 돌지 않으면 nil
	failed error
}

// submit 은 snap 을 다음에 쓸 사본으로 둔다. 쓰는 고루틴이 없으면 띄우고, 그 끝을 recentSavedMsg 로 알릴 명령을 돌려준다.
// 이미 실패했으면 버린다(화면이 실패 알림을 받기 전에 들어온 저장도 다시 쓰지 않는다).
func (s *recentSaver) submit(snap *recent.Snapshot) tea.Cmd {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.failed != nil {
		return nil
	}
	s.next = snap
	if s.idle != nil {
		return nil
	}
	idle := make(chan struct{})
	s.idle = idle
	go s.run(idle)
	return func() tea.Msg {
		<-idle
		return recentSavedMsg{err: s.failure()}
	}
}

func (s *recentSaver) run(idle chan struct{}) {
	defer close(idle)
	for {
		s.mu.Lock()
		snap := s.next
		s.next = nil
		if snap == nil {
			s.idle = nil
			s.mu.Unlock()
			return
		}
		s.mu.Unlock()
		if err := writeRecent(snap); err != nil {
			s.mu.Lock()
			s.failed, s.idle = err, nil
			s.mu.Unlock()
			return
		}
	}
}

func (s *recentSaver) failure() error {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.failed
}

// flush 는 돌고 있는 쓰기가 끝나기를 wait 까지 기다리고 실패가 있었으면 돌려준다.
func (s *recentSaver) flush(wait time.Duration) error {
	s.mu.Lock()
	idle := s.idle
	s.mu.Unlock()
	if idle != nil {
		select {
		case <-idle:
		case <-time.After(wait):
			return errRecentFlush
		}
	}
	return s.failure()
}

// ─── 팔레트·찾기 ───

// actionKey 는 동작의 기억 이름이다. 보이는 글자(⌘)는 환경마다 달라 묶음 제목과 원래 키 이름을 쓴다.
func actionKey(section, key string) string { return section + "\x00" + key }

// recentActions 는 자주 쓴 동작 라벨을 frecency 순으로 paletteTopActions 개까지 돌려준다.
func (m *model) recentActions(labels []string, keys map[string]string) []string {
	if m.recent == nil {
		return nil
	}
	var out []string
	for _, k := range m.recent.TopActions(time.Now(), paletteTopActions) {
		if i := slices.IndexFunc(labels, func(l string) bool { return keys[l] == k }); i >= 0 {
			out = append(out, labels[i])
		}
	}
	return out
}

// noteAction 은 팔레트로 고른 동작을 센다.
func (m *model) noteAction(key string) {
	if m.recent == nil || key == "" {
		return
	}
	m.recent.UseAction(key, time.Now())
	m.saveRecent()
}

// recentConnLabels 는 팔레트에 넣을 최근 접속이다. 앞 paletteTopConns 개만 빈 팔레트에 보인다(pickerMatches 가 pickerQuiet 로 가린다).
func (m *model) recentConnLabels() []recentConn {
	if m.recent == nil {
		return nil
	}
	var out []recentConn
	seen := map[string]bool{}
	for _, uuid := range m.recent.TopConns(time.Now(), 50) {
		ci, ok := m.proj.ConnIndex(uuid)
		if !ok {
			continue
		}
		label := "접속 " + m.conn(ci).Name
		if seen[label] {
			label += fmt.Sprintf(" #%d", ci+1)
		}
		seen[label] = true
		out = append(out, recentConn{label: label, conn: ci})
	}
	return out
}

// revealConn 은 팔레트에서 고른 접속을 트리에서 골라 Enter 처럼 연다(펼치기·탭 접속 바꾸기).
func (m *model) revealConn(ci int) tea.Cmd {
	cn := m.connNode(ci)
	if cn == nil {
		m.status = "접속을 찾지 못함"
		return nil
	}
	m.filter = ""
	for p := cn.parent; p != nil; p = p.parent {
		p.expanded = true
	}
	m.rebuild()
	m.selectNode(cn)
	m.setFocus(fTree)
	return m.activate(cn, false)
}

// noteConn 은 접속을 쓴 것으로 센다(트리에서 고름·테이블 보기).
func (m *model) noteConn(ci int) {
	if m.recent == nil || ci < 0 {
		return
	}
	m.recent.UseConn(m.conn(ci).UUID, time.Now())
	m.saveRecent()
}

// finderOrder 는 Ctrl+N 후보에서 최근 연 테이블을 최근 순으로 맨 앞에 둔다(R75.3). 나머지 순서는 그대로다.
func (m *model) finderOrder(items []finder.Item) []finder.Item {
	if m.recent == nil {
		return items
	}
	rank := map[finder.Item]int{}
	for i, t := range m.recent.RecentTables(finderRecent) {
		ci, ok := m.proj.ConnIndex(t.Conn)
		if !ok {
			continue
		}
		for _, kind := range []string{finder.KindTable, finder.KindView} {
			rank[finder.Item{Conn: m.conn(ci).Name, Kind: kind, Schema: t.Schema, Name: t.Name}] = i + 1
		}
	}
	var front, rest []finder.Item
	for _, it := range items {
		if rank[it] > 0 {
			front = append(front, it)
		} else {
			rest = append(rest, it)
		}
	}
	slices.SortStableFunc(front, func(a, b finder.Item) int { return rank[a] - rank[b] })
	return append(front, rest...)
}

// ─── 테이블 보기 설정 ───

// restoreTable 은 기억한 정렬·WHERE 로 테이블 보기를 연다(R75.4). prod 면 정렬·WHERE 를 쓰지 않는다(예전 파일에 남았어도). 바꾼 것을 알릴 글을 돌려준다.
func (m *model) restoreTable(tv tableview.Model, conn int) (tableview.Model, string) {
	if m.recent == nil || tv.NoHistory() {
		return tv, ""
	}
	c, ref := m.conn(conn), tv.Table()
	pref, ok := m.recent.Table(c.UUID, ref.Schema, ref.Table)
	if !ok {
		return tv, ""
	}
	where, order := pref.Where, pref.Order
	if c.Env() == "prod" {
		where, order = "", ""
	}
	var what []string
	if order != "" {
		what = append(what, "정렬")
	}
	if where != "" {
		what = append(what, "조건")
	}
	if len(what) == 0 {
		return tv, ""
	}
	return tv.Restore(where, order), "이전 " + strings.Join(what, "·") + " 적용 (w·O 칸을 비우고 Enter 로 해제)"
}

// noteTableResult 는 테이블 보기 페이지가 성공하면 그 정렬·WHERE 를 남기고 기억한 열 폭을 입힌다(R75.4).
// 조건을 주고 연 보기(R36 등)는 사용자가 WHERE 입력을 적용하기 전까지 남기지 않는다. 그 WHERE 는 사용자가 친 글이 아니다.
func (m *model) noteTableResult(t *queryTab) {
	if m.recent == nil || t.tv == nil || t.tv.NoHistory() || t.viewConn < 0 {
		return
	}
	c, ref := m.conn(t.viewConn), t.tv.Table()
	if pref, ok := m.recent.Table(c.UUID, ref.Schema, ref.Table); ok {
		t.grid.SetWidths(pref.Widths)
	}
	if t.tv.PresetWhere() {
		return
	}
	if m.recent.SetView(c.UUID, ref.Schema, ref.Table, t.tv.Order(), t.tv.Where(), c.Env() == "prod", time.Now()) {
		m.saveRecent()
	}
}

// noteWidth 는 테이블 보기에서 <·> 로 바꾼 커서 열 폭을 남긴다.
func (m *model) noteWidth(t *queryTab) {
	if m.recent == nil || t.tv == nil || t.tv.NoHistory() || t.viewConn < 0 {
		return
	}
	col := t.grid.CurrentCol()
	ref := t.tv.Table()
	m.recent.SetWidth(m.conn(t.viewConn).UUID, ref.Schema, ref.Table, col, t.grid.Width(col), time.Now())
	m.saveRecent()
}

// noteTableOpen 은 테이블 보기가 실제로 시작될 때 연 때와 접속 사용을 남긴다.
func (m *model) noteTableOpen(conn int, ref db.TableRef) {
	if m.recent == nil {
		return
	}
	m.recent.TouchTable(m.conn(conn).UUID, ref.Schema, ref.Table, time.Now())
	m.noteConn(conn)
}

// ─── 입력 기록 ───

// whereRecall 은 지금 테이블 보기의 WHERE 입력 기록이다. prod 접속이면 비어 있다.
func (m *model) whereRecall(t *queryTab) []string {
	if m.recent == nil || t.tv == nil || t.tv.NoHistory() || t.viewConn < 0 {
		return nil
	}
	c, ref := m.conn(t.viewConn), t.tv.Table()
	return m.recent.Wheres(c.UUID, ref.Schema, ref.Table, c.Env() == "prod")
}

// filterRecall 은 결과 거르기 기록이다.
func (m *model) filterRecall() []string {
	if m.recent == nil {
		return nil
	}
	return m.recent.Filters()
}

// endRecall 은 입력 기록 자리를 쓰는 입력(지금 탭의 결과 거르기·WHERE)이 닫혔거나 다른 탭으로 갔으면 자리를 버린다.
// Enter·esc 말고도 마우스·초점 이동으로 닫히므로 키마다가 아니라 Update 끝에서 본다.
func (m *model) endRecall() {
	if m.recall == (recallState{}) {
		return
	}
	t := m.cur()
	open := m.focus == fResult && (t.grid.FilterTyping() || t.tv != nil && t.tv.TypingWhere())
	if !open || m.recall.tab != t.id {
		m.recall = recallState{}
	}
}

// recallStep 은 ↑(up)·↓ 로 list(최근 것부터)에서 다음 글을 고른다. 고를 것이 없으면 false 다.
// 처음 ↑ 는 치던 글을 두고 가장 최근 기록을, ↓ 로 끝까지 내려오면 치던 글로 돌아간다.
func (m *model) recallStep(list []string, up bool, cur string) (string, bool) {
	r := &m.recall
	switch {
	case up && r.pos < len(list):
		if r.pos == 0 {
			r.draft, r.tab = cur, m.cur().id
		}
		r.pos++
	case !up && r.pos > 0:
		r.pos--
	default:
		return "", false
	}
	if r.pos == 0 {
		return r.draft, true
	}
	return list[r.pos-1], true
}

// gridTypingKey 는 결과 거르기 입력의 ↑↓ 를 기록 고르기로 쓰고(R75.5), Enter 로 확정한 글을 남긴다.
// 그 밖의 키는 기록 자리를 처음으로 돌린다. 처리했으면 true 다.
func (m *model) gridTypingKey(t *queryTab, ks string) bool {
	if !t.grid.FilterTyping() || m.recent == nil {
		return false
	}
	switch ks {
	case "up", "down":
		if f, ok := m.recallStep(m.filterRecall(), ks == "up", t.grid.Query().Filter); ok {
			t.grid.SetFilterInput(f)
		}
		return true
	case "enter":
		if m.keepsFilter(t) {
			m.recent.AddFilter(t.grid.Query().Filter)
			m.saveRecent()
		}
	}
	m.recall = recallState{}
	return false
}

// keepsFilter 는 이 탭의 거르기 글을 기록에 남길지다. 거르기 기록은 모든 프로젝트·접속이 같이 보는 목록이라
// 값이 든 결과의 거르기는 남기지 않는다: prod 결과, 사용자 한눈에 보기·비교(R51·R53, viewConn -1), 연결된 곳(R73), 기록 사본(R11.4).
// viewConn 은 마지막 실행의 접속이라 고정 결과·하위 결과(t.view >= 0)는 출처를 모른다. 그래서 탭의 지금 결과에서만 남긴다.
func (m *model) keepsFilter(t *queryTab) bool {
	return t.view < 0 && t.viewConn >= 0 && m.conn(t.viewConn).Env() != "prod" && t.linked == nil && t.snap == nil
}

// whereTypingKey 는 테이블 보기 WHERE 입력의 ↑↓ 를 기록 고르기로 쓴다(R75.5). 기록 자리를 옮겼을 때만 true 고,
// 아니면 키를 테이블 보기로 넘긴다. ORDER BY 입력과 기록이 없는(prod 등) WHERE 입력에서는 하지 않는다. 그 밖의 키는 기록 자리를 처음으로 돌린다.
func (m *model) whereTypingKey(t *queryTab, ks string) bool {
	if (ks == "up" || ks == "down") && t.tv.TypingWhere() {
		w, ok := m.recallStep(m.whereRecall(t), ks == "up", t.tv.Input())
		if ok {
			*t.tv = t.tv.SetInput(w)
		}
		return ok
	}
	m.recall = recallState{}
	return false
}

// noteWhere 는 적용한 WHERE 입력을 테이블마다 남긴다. prod 면 남기지 않는다.
func (m *model) noteWhere(t *queryTab, text string) {
	if m.recent == nil || t.tv == nil || t.tv.NoHistory() || t.viewConn < 0 {
		return
	}
	c, ref := m.conn(t.viewConn), t.tv.Table()
	m.recent.AddWhere(c.UUID, ref.Schema, ref.Table, text, c.Env() == "prod", time.Now())
	m.saveRecent()
}

// ─── 트리 펼침 ───
// 그룹(폴더)과 접속 없는 쿼리 묶음은 켤 때 바로 되살린다. 접속 아래(스키마·Redis 이름공간·쿼리 묶음)는 연결이 필요해
// 켤 때 접속하지 않고, 사용자가 그 접속을 처음 펼쳐 목록이 오면 기억한 자식을 다시 펼친다(스키마는 비동기로 읽는다).
// 접속 줄 자체도 켤 때 펼치지 않는다. prod 를 포함해 어떤 접속에도 펼침만을 위해 붙지 않는다.

// treeID 는 노드의 기억 이름이다. 기억하지 않는 노드면 "" 다.
func (m *model) treeID(n *node) string {
	if n.kind == nGroup {
		return "g:" + n.label
	}
	if n.conn < 0 {
		if n.kind == nQueries {
			return "q:"
		}
		return ""
	}
	uuid := m.conn(n.conn).UUID
	switch n.kind {
	case nSchema:
		return "s:" + uuid + ":" + n.label
	case nFolder:
		return "f:" + uuid + ":" + n.key
	case nQueries:
		return "q:" + uuid
	}
	return ""
}

// treeIDConn 은 접속 아래 노드 id 의 접속 uuid 다. 그룹·접속 없는 묶음이면 "" 다.
func treeIDConn(id string) string {
	kind, rest, ok := strings.Cut(id, ":")
	if !ok || kind == "g" {
		return ""
	}
	uuid, _, _ := strings.Cut(rest, ":")
	return uuid
}

// restoreGroups 는 켤 때 접은 그룹을 접고 접속 없는 쿼리 묶음 펼침을 되살린다.
func (m *model) restoreGroups() {
	open, closed := m.recent.Tree()
	for _, n := range m.roots {
		id := m.treeID(n)
		switch {
		case n.kind == nGroup && slices.Contains(closed, id):
			n.expanded = false
		case n.kind == nQueries && slices.Contains(open, id):
			n.expanded = true
		}
	}
	m.rebuild()
}

// restoreConnTree 는 접속 목록이 처음 왔을 때 기억한 자식을 다시 펼친다. 실행마다 접속당 한 번만 한다
// (이번 실행에서 접은 것을 r 새로고침이 되살리지 않게). 스키마 읽기 명령을 돌려준다.
func (m *model) restoreConnTree(cn *node) tea.Cmd {
	if m.recent == nil || cn.kind != nConn || !cn.loaded {
		return nil
	}
	uuid := m.conn(cn.conn).UUID
	if m.treeDone[uuid] {
		return nil
	}
	m.treeDone[uuid] = true
	open, _ := m.recent.Tree()
	var cmds []tea.Cmd
	stack := slices.Clone(cn.children)
	for len(stack) > 0 {
		n := stack[len(stack)-1]
		stack = stack[:len(stack)-1]
		if id := m.treeID(n); id == "" || !slices.Contains(open, id) {
			continue
		}
		if n.loaded {
			n.expanded = true
			stack = append(stack, n.children...)
			continue
		}
		cmds = append(cmds, m.expand(n))
	}
	m.rebuild()
	return tea.Batch(cmds...)
}

// snapshotTree 는 지금 트리 펼침을 기억에 둔다. 이번 실행에서 목록을 읽지 않은 접속은 예전 기억을 그대로 둔다.
func (m *model) snapshotTree() {
	prev, _ := m.recent.Tree()
	var open, closed []string
	loaded := map[string]bool{}
	stack := slices.Clone(m.roots)
	for len(stack) > 0 {
		n := stack[0]
		stack = stack[1:]
		id := m.treeID(n)
		if n.kind == nConn {
			loaded[m.conn(n.conn).UUID] = n.loaded
			if !n.loaded {
				continue
			}
		}
		switch {
		case n.kind == nGroup && !n.expanded:
			closed = append(closed, id)
		case id != "" && n.kind != nGroup && n.expanded:
			open = append(open, id)
		}
		stack = append(stack, n.children...)
	}
	// 이번 실행에서 목록을 읽지 않은 접속의 기억은 두고, .idea 에서 없어진 접속의 기억은 버린다
	for _, id := range prev {
		uuid := treeIDConn(id)
		if done, known := loaded[uuid]; uuid != "" && known && !done {
			open = append(open, id)
		}
	}
	m.recent.SetTree(open, closed)
}
