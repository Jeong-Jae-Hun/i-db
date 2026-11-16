package main

import (
	"cmp"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"regexp"
	"slices"
	"strconv"
	"strings"
	"sync"
	"time"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
	"github.com/charmbracelet/x/ansi"

	"github.com/Jeong-Jae-Hun/i-db/internal/chart"
	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/flows"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
	"github.com/Jeong-Jae-Hun/i-db/internal/textutil"
)

// ─── 쿼리 보드 (R70) ───

const (
	boardDir          = ".dbtui/boards"
	boardDefaultEvery = 60 * time.Second
	boardMinEvery     = 10 * time.Second
	// boardTimeout 은 패널 하나(여는 시간 포함)의 제한이다
	boardTimeout = 15 * time.Second
	// boardParallel 은 동시에 도는 패널 수다. 접속이 다른 패널만 함께 돈다
	boardParallel = 4
	// boardRows 는 숫자·표 패널이 받는 행 수이자 CLI json 상한이다. 차트는 시계열이라 저장소 기본(1,000)을 받는다
	boardRows         = 50
	boardHint         = "r 새로고침 · 방향키·hjkl 패널 · Enter 쿼리 탭에 열기 · Ctrl+W 닫기 · F1 도움말"
	boardNoFile       = "보드 없음 · .dbtui/boards/<이름>.json 을 쓰세요"
	msgBoardExpensive = "비싼 질의라 보드에서 돌리지 않음"
)

var (
	// boardRun 은 패널 하나를 한 번 돌린다. 화면과 `db board` 가 같이 쓰고 테스트가 바꿔 끼운다
	boardRun = runPanel
	// boardWait 는 refresh 간격을 타이머 시간으로 바꾼다. 테스트가 줄인다
	boardWait = func(d time.Duration) time.Duration { return d }
)

var (
	boardViews  = []string{"number", "table", "chart", "flow"}
	warnRe      = regexp.MustCompile(`^\s*(>=|<=|==|!=|>|<|=)\s*(-?[0-9][0-9,]*(?:\.[0-9]+)?)\s*$`)
	boardNumber = lipgloss.NewStyle().Bold(true).Foreground(colText)
	prodBadge   = lipgloss.NewStyle().Foreground(colProd).Bold(true)
)

// boardPanel 은 패널 하나다. 소문자 칸은 검사가 채운다. err 가 있으면 돌리지 않고 `설정 오류: <err>` 를 보인다.
type boardPanel struct {
	Title string `json:"title"`
	Conn  string `json:"conn,omitempty"`
	Query string `json:"query,omitempty"`
	Flow  string `json:"flow,omitempty"`
	View  string `json:"view"`
	Since string `json:"since,omitempty"`
	Warn  string `json:"warn,omitempty"`

	err   string
	since time.Duration
	warn  *warnRule
	flow  flows.Flow
	// key 는 차례로 돌릴 묶음 이름(접속 이름, 흐름은 aws 접속)이다
	key string
}

// board 는 .dbtui/boards/<name>.json 하나다. err 는 파일 단위 오류(JSON·refresh)이고 패널과 따로 보인다.
type board struct {
	name, title string
	every       time.Duration
	panels      []boardPanel
	err         string
}

type boardFile struct {
	Title   string       `json:"title"`
	Refresh string       `json:"refresh"`
	Panels  []boardPanel `json:"panels"`
}

// warnRule 은 숫자 패널의 경고 조건(`>0`·`>=100`)이다.
type warnRule struct {
	op string
	v  float64
}

func parseWarn(s string) (*warnRule, error) {
	m := warnRe.FindStringSubmatch(s)
	if m == nil {
		return nil, fmt.Errorf("warn must be like >0 or >=100: %q", s)
	}
	v, err := strconv.ParseFloat(strings.ReplaceAll(m[2], ",", ""), 64)
	if err != nil {
		return nil, fmt.Errorf("warn must be like >0 or >=100: %q", s)
	}
	return &warnRule{op: m[1], v: v}, nil
}

// hit 은 칸이 조건에 맞는지다. 숫자가 아니면 경고하지 않는다.
func (r *warnRule) hit(cell string) bool {
	if r == nil {
		return false
	}
	v, err := strconv.ParseFloat(strings.ReplaceAll(strings.TrimSpace(cell), ",", ""), 64)
	if err != nil {
		return false
	}
	switch r.op {
	case ">":
		return v > r.v
	case ">=":
		return v >= r.v
	case "<":
		return v < r.v
	case "<=":
		return v <= r.v
	case "!=":
		return v != r.v
	}
	return v == r.v
}

// ─── 파일 ───

// listBoards 는 .dbtui/boards 의 보드 이름(.json 뺀 파일 이름) 순서다. 폴더가 없으면 빈 목록이다.
func listBoards(dir string) []string {
	ents, _ := os.ReadDir(filepath.Join(dir, boardDir))
	var out []string
	for _, e := range ents {
		if n, ok := strings.CutSuffix(e.Name(), ".json"); ok && !e.IsDir() && n != "" && !strings.HasPrefix(n, ".") {
			out = append(out, n)
		}
	}
	return out
}

// loadBoard 는 보드 하나를 읽는다. 이름이 틀렸거나 파일이 없으면 오류다. JSON 이 깨졌으면 패널 없이 err 를,
// 패널 하나가 틀리면 그 패널 err 에만 까닭을 적는다(db 는 그대로 켜진다).
func loadBoard(proj *db.Project, name string) (*board, error) {
	if name == "" || name != filepath.Base(name) || strings.HasPrefix(name, ".") {
		return nil, fmt.Errorf("board name must be a file name in %s: %q", boardDir, name)
	}
	data, err := os.ReadFile(filepath.Join(proj.Dir, boardDir, name+".json"))
	if err != nil {
		return nil, err
	}
	b := &board{name: name, title: name, every: boardDefaultEvery}
	var f boardFile
	if err := json.Unmarshal(data, &f); err != nil {
		b.err = fmt.Sprintf("%s/%s.json: %v", boardDir, name, err)
		return b, nil
	}
	if strings.TrimSpace(f.Title) != "" {
		b.title = f.Title
	}
	if f.Refresh != "" {
		switch d, err := time.ParseDuration(f.Refresh); {
		case err != nil:
			b.err = fmt.Sprintf("refresh: invalid duration %q", f.Refresh)
		case d < boardMinEvery:
			// 접속마다 질의를 보내므로 너무 잦게 돌지 않게 10초로 올린다
			b.err, b.every = fmt.Sprintf("refresh must be at least %s: %s", boardMinEvery, f.Refresh), boardMinEvery
		default:
			b.every = d
		}
	}
	fs, ferr := flows.Load(proj.Dir, proj.Conns)
	b.panels = f.Panels
	for i := range b.panels {
		p := &b.panels[i]
		if err := checkPanel(p, proj, fs, ferr); err != nil {
			p.err = err.Error()
		}
		if strings.TrimSpace(p.Title) == "" {
			p.Title = fmt.Sprintf("#%d", i+1)
		}
	}
	return b, nil
}

// checkPanel 은 패널 설정을 검사하고 돌릴 값(범위·경고·흐름·묶음)을 채운다. 쓰기로 보이는 질의는 여기서 막는다.
func checkPanel(p *boardPanel, proj *db.Project, fs []flows.Flow, ferr error) error {
	if !slices.Contains(boardViews, p.View) {
		return fmt.Errorf("view must be one of %s: %q", strings.Join(boardViews, "|"), p.View)
	}
	if p.Warn != "" {
		if p.View != "number" {
			return errors.New("warn is only for view number")
		}
		w, err := parseWarn(p.Warn)
		if err != nil {
			return err
		}
		p.warn = w
	}
	if p.View == "flow" {
		switch i := slices.IndexFunc(fs, func(f flows.Flow) bool { return f.Name == p.Flow }); {
		case strings.TrimSpace(p.Flow) == "":
			return errors.New("flow is empty")
		case ferr != nil:
			return ferr
		case i < 0:
			return fmt.Errorf("flow %s: no such flow (.dbtui/flows.json)", p.Flow)
		case fs[i].Err != "":
			return fmt.Errorf("flow %s: %s", p.Flow, fs[i].Err)
		default:
			p.flow, p.key = fs[i], fs[i].AWS
		}
		return nil
	}
	if strings.TrimSpace(p.Conn) == "" {
		return errors.New("conn is empty")
	}
	if strings.TrimSpace(p.Query) == "" {
		return errors.New("query is empty")
	}
	i := connByName(proj, p.Conn)
	if i < 0 || proj.Conns[i].Kind == "" {
		return fmt.Errorf("conn %s: no such connection", p.Conn)
	}
	c := proj.Conns[i]
	p.key = c.Name
	switch c.Kind {
	case db.KindAWS:
		return fmt.Errorf("conn %s: aws connections are not query panels (use view flow)", c.Name)
	case db.KindPG:
		if len(db.SplitStatements(p.Query)) != 1 {
			return errors.New("query must be a single statement")
		}
		if err := db.CheckReadOnlySQL(p.Query); err != nil {
			return fmt.Errorf("query is not read-only: %w", err)
		}
	case db.KindRedis:
		args := db.SplitArgs(strings.TrimSpace(p.Query))
		switch {
		case strings.Contains(strings.TrimSpace(p.Query), "\n"):
			return errors.New("redis panel takes one command")
		case len(args) > 0 && strings.EqualFold(args[0], "KEYS"):
			// CLI 와 같다(R58.2): KEYS 는 키 전체를 훑는 동안 서버를 멈춘다
			return errors.New("KEYS is not run on boards (it blocks the server): use SCAN")
		case !db.IsReadOnlyRedis(args):
			return errors.New("redis command is not a read command")
		}
	}
	if p.Since != "" {
		if !usesRange(c.Kind) {
			return errors.New("since is only for loki·prometheus·grafana")
		}
		var d durFlag
		if err := d.Set(p.Since); err != nil {
			return fmt.Errorf("since: %w", err)
		}
		p.since = time.Duration(d)
	}
	return nil
}

// panelConns 는 패널이 묻는 접속 이름이다(흐름은 아웃박스·aws 접속).
func panelConns(p boardPanel) []string {
	if p.View != "flow" {
		return []string{p.Conn}
	}
	var out []string
	if p.flow.Outbox != nil {
		out = append(out, p.flow.Outbox.Conn)
	}
	return append(out, p.flow.AWS)
}

// panelProd 는 패널이 prod 접속(R6.2)을 묻는지다.
func panelProd(proj *db.Project, p boardPanel) bool {
	for _, n := range panelConns(p) {
		if i := connByName(proj, n); i >= 0 && proj.Conns[i].Env() == "prod" {
			return true
		}
	}
	return false
}

// ─── 돌리기 ───

// panelResult 는 패널 한 번의 결과다. 흐름 패널은 boxes 를 든다.
type panelResult struct {
	res       *db.Result
	boxes     []flowBox
	err, note string
	at        time.Time
	elapsed   time.Duration
}

// failed 는 실패한 패널인지다. 흐름은 실패한 상자가 하나라도 있으면 실패다.
func (r panelResult) failed() bool {
	return r.err != "" || slices.ContainsFunc(r.boxes, flowBox.failed)
}

// runPanel 은 패널 하나를 15초 안에 돌린다. PostgreSQL 은 READ ONLY 트랜잭션, Redis 는 읽기 명령, 저장소는 Backend.Run 이고
// 비싼 질의(ErrNeedsConfirm)는 Force 로 다시 보내지 않는다. prod 가 거짓이면 흐름의 prod 접속은 묻지 않는다(CLI).
func runPanel(ctx context.Context, clients *db.Clients, sources *sourcePool, proj *db.Project, p boardPanel, prod bool) panelResult {
	r := panelResult{at: time.Now()}
	ctx, cancel := context.WithTimeout(ctx, boardTimeout)
	defer cancel()
	if p.View == "flow" {
		r.boxes = flowBoxes(p.flow, resolveFlow(ctx, clients, sources, proj, p.flow, prod), true)
		r.elapsed = time.Since(r.at)
		return r
	}
	i := connByName(proj, p.Conn)
	if i < 0 {
		r.err = "no such connection: " + p.Conn
		return r
	}
	c := proj.Conns[i]
	limit := boardRows
	if p.View == "chart" {
		limit = sourceDefaultRows
	}
	var res *db.Result
	var err error
	switch c.Kind {
	case db.KindPG:
		var pg *db.PGConn
		if pg, err = clients.PG(ctx, c); err == nil {
			res, err = pg.RunReadOnlyRows(ctx, p.Query, limit)
		}
	case db.KindRedis:
		var rc *db.RedisConn
		if rc, err = clients.Redis(ctx, c); err == nil {
			res, err = rc.Run(ctx, db.SplitArgs(strings.TrimSpace(p.Query)))
		}
	default:
		var b source.Backend
		if b, err = sources.get(ctx, c); err == nil {
			q := source.Query{Text: strings.TrimSpace(p.Query), Limit: limit}
			if usesRange(c.Kind) {
				q.Since = cmpRange(p.since)
			}
			res, err = b.Run(ctx, q)
		}
	}
	switch {
	case errors.Is(err, source.ErrNeedsConfirm):
		r.err = msgBoardExpensive
		if reason := source.ConfirmReason(err); reason != "" {
			r.err += ": " + reason
		}
	case err != nil:
		r.err = oneLine(db.Redact(sourceErrText(err), c.Password))
	default:
		r.res = res
	}
	r.elapsed = time.Since(r.at)
	return r
}

// boardGroups 는 돌릴 패널을 묶음(같은 접속)마다 차례로 늘어놓는다. 설정 오류 패널은 뺀다.
func boardGroups(panels []boardPanel) [][]int {
	var out [][]int
	at := map[string]int{}
	for i, p := range panels {
		if p.err != "" {
			continue
		}
		g, ok := at[p.key]
		if !ok {
			g = len(out)
			at[p.key] = g
			out = append(out, nil)
		}
		out[g] = append(out[g], i)
	}
	return out
}

// boardRunAll 은 묶음 안은 차례로, 묶음끼리는 동시에(최대 boardParallel) 돌린다. 같은 접속에 질의를 겹쳐 보내지 않게 한다.
func boardRunAll(groups [][]int, run func(i int)) {
	sem := make(chan struct{}, boardParallel)
	var wg sync.WaitGroup
	for _, g := range groups {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for _, i := range g {
				sem <- struct{}{}
				run(i)
				<-sem
			}
		}()
	}
	wg.Wait()
}

// ─── 탭 ───

// boardState 는 보드 탭의 상태다. 다시 돌리는 동안 이전 결과를 그대로 보이고 pending 만 센다.
type boardState struct {
	b       *board
	results map[int]panelResult
	pending map[int]bool
	gen     int
	ctx     context.Context
	cancel  context.CancelFunc
	sem     chan struct{}
	cursor  int
}

type boardPanelMsg struct {
	tab, gen int
	group    []int
	pos      int
	r        panelResult
}

type boardTickMsg struct{ tab, gen int }

func (s *boardState) stop() {
	if s.cancel != nil {
		s.cancel()
	}
}

// pickBoard 는 보드 파일을 골라 연다(팔레트 `보드`·Alt+B).
func (m *model) pickBoard() tea.Cmd {
	names := listBoards(m.proj.Dir)
	if len(names) == 0 {
		m.status = boardNoFile
		return nil
	}
	m.openPicker("보드 ", names, nil, func(name string) tea.Cmd { return m.openBoard(name) }, nil)
	return nil
}

// openBoard 는 보드 탭을 열거나 같은 보드 탭이 있으면 그 탭으로 옮겨 바로 다시 돌린다. 보드마다 탭 하나다.
func (m *model) openBoard(name string) tea.Cmd {
	for i, t := range m.tabs {
		if t.board != nil && t.board.b.name == name {
			m.switchTab(i)
			m.setFocus(fResult)
			return m.boardRefresh(t)
		}
	}
	b, err := loadBoard(m.proj, name)
	if err != nil {
		m.status = "보드를 열지 못함: " + oneLine(err.Error())
		return nil
	}
	t := m.newTab()
	m.setConn(t, -1)
	t.title = b.title
	t.board = &boardState{b: b, results: map[int]panelResult{}, pending: map[int]bool{}}
	m.setFocus(fResult)
	return m.boardStart(t)
}

// boardRefresh 는 파일을 다시 읽고(고친 설정이 바로 듣게) 모든 패널을 다시 돌린다. 파일이 사라졌으면 이전 설정으로 돈다.
func (m *model) boardRefresh(t *queryTab) tea.Cmd {
	s := t.board
	if b, err := loadBoard(m.proj, s.b.name); err == nil {
		if len(b.panels) != len(s.b.panels) {
			clear(s.results)
		}
		s.b = b
	} else {
		m.status = "보드 파일을 다시 읽지 못해 이전 설정으로 돎: " + oneLine(err.Error())
	}
	return m.boardStart(t)
}

// boardStart 는 진행 중인 회차를 취소하고 묶음마다 첫 패널을 돌린다. 다음 패널은 앞 패널 결과가 오면 돌린다(boardStep).
func (m *model) boardStart(t *queryTab) tea.Cmd {
	s := t.board
	s.stop()
	s.gen++
	s.ctx, s.cancel = context.WithCancel(m.life)
	s.sem = make(chan struct{}, boardParallel)
	s.cursor = clamp(s.cursor, len(s.b.panels))
	clear(s.pending)
	id, gen := t.id, s.gen
	cmds := []tea.Cmd{tea.Tick(boardWait(s.b.every), func(time.Time) tea.Msg { return boardTickMsg{tab: id, gen: gen} })}
	for _, g := range boardGroups(s.b.panels) {
		for _, i := range g {
			s.pending[i] = true
		}
		cmds = append(cmds, m.boardStep(t, g, 0))
	}
	m.status = "보드 확인 중…"
	return tea.Batch(cmds...)
}

// boardStep 은 묶음 g 의 pos 번째 패널을 돌린다. 화면에서는 prod 패널도 돌린다(읽기 전용, 제목에 prod 표시).
func (m *model) boardStep(t *queryTab, g []int, pos int) tea.Cmd {
	s := t.board
	ctx, sem, p := s.ctx, s.sem, s.b.panels[g[pos]]
	id, gen, clients, sources, proj := t.id, s.gen, m.clients, m.sources, m.proj
	return func() tea.Msg {
		select {
		case sem <- struct{}{}:
		case <-ctx.Done():
			return nil
		}
		r := boardRun(ctx, clients, sources, proj, p, true)
		<-sem
		return boardPanelMsg{tab: id, gen: gen, group: g, pos: pos, r: r}
	}
}

func (m *model) boardDone(msg boardPanelMsg) tea.Cmd {
	t := m.findTab(msg.tab)
	if t == nil || t.board == nil || t.board.gen != msg.gen {
		return nil
	}
	s := t.board
	s.results[msg.group[msg.pos]] = msg.r
	delete(s.pending, msg.group[msg.pos])
	if len(s.pending) == 0 && strings.HasPrefix(m.status, "보드 확인 중") {
		m.status = "보드 확인함 " + time.Now().Format("15:04:05")
	}
	if msg.pos+1 < len(msg.group) {
		return m.boardStep(t, msg.group, msg.pos+1)
	}
	return nil
}

// boardTicked 는 보드가 활성 탭일 때만 다시 돌린다. 다른 탭이면 다음 타이머만 건다.
func (m *model) boardTicked(msg boardTickMsg) tea.Cmd {
	t := m.findTab(msg.tab)
	if t == nil || t.board == nil || t.board.gen != msg.gen {
		return nil
	}
	if m.cur() == t {
		return m.boardRefresh(t)
	}
	id, gen := msg.tab, msg.gen
	return tea.Tick(boardWait(t.board.b.every), func(time.Time) tea.Msg { return boardTickMsg{tab: id, gen: gen} })
}

func (m *model) boardKey(k tea.KeyMsg) tea.Cmd {
	t := m.cur()
	s := t.board
	cols := boardCols(m.layout().rightW)
	n := len(s.b.panels)
	switch k.String() {
	case "r":
		return m.boardRefresh(t)
	case "left", "h":
		s.cursor--
	case "right", "l":
		s.cursor++
	case "up", "k":
		s.cursor -= cols
	case "down", "j":
		if s.cursor+cols < n {
			s.cursor += cols
		}
	case "home", "g":
		s.cursor = 0
	case "end", "G":
		s.cursor = n - 1
	case "enter":
		if n == 0 {
			return nil
		}
		return m.openPanel(s.b.panels[clamp(s.cursor, n)])
	}
	s.cursor = clamp(s.cursor, n)
	return nil
}

// openPanel 은 패널의 질의를 새 쿼리 탭에 채우고 그 접속에서 실행한다(보통 실행이라 R6 확인을 그대로 탄다).
// 흐름 패널은 흐름 탭에서 그 흐름을 고른다.
func (m *model) openPanel(p boardPanel) tea.Cmd {
	if p.err != "" {
		m.status = "설정 오류: " + oneLine(p.err)
		return nil
	}
	if p.View == "flow" {
		cmd := m.openFlows(nil)
		if s := m.cur().flow; s != nil {
			s.cursor = max(0, slices.IndexFunc(s.list, func(f flows.Flow) bool { return f.Name == p.Flow }))
			s.box = 0
		}
		return cmd
	}
	ci := connByName(m.proj, p.Conn)
	if ci < 0 {
		m.status = "접속 없음: " + p.Conn
		return nil
	}
	t := m.newTab()
	t.title = p.Title
	t.editor.SetValue(p.Query)
	cmd := m.setConn(t, ci)
	if usesRange(m.conn(ci).Kind) {
		t.since = cmpRange(p.since)
	}
	m.setFocus(fEditor)
	return tea.Batch(cmd, m.run(p.Query))
}

// ─── 그리기 ───

// boardCols 는 보드 영역 폭에 맞는 패널 열 수다: 80칸 아래 1, 140칸 아래 2, 그 위 3.
func boardCols(w int) int {
	switch {
	case w < 80:
		return 1
	case w < 140:
		return 2
	}
	return 3
}

// panelHeight 는 패널 안쪽 줄 수(머리 두 줄 포함)다: 숫자 4, 표·차트 10, 흐름은 상자 수 + 2.
func panelHeight(p boardPanel, r panelResult) int {
	switch {
	case p.err != "":
		return 4
	case p.View == "number":
		return 4
	case p.View == "flow":
		n := len(r.boxes)
		if n == 0 {
			n = len(flowBoxes(p.flow, flowSnap{}, false))
		}
		return max(4, n+2)
	}
	return 10
}

// boardView 는 대시보드처럼 편집기·결과 자리 전체에 패널 격자를 그린다. 고른 패널이 보이게 위를 맞춘다.
func (m *model) boardView(t *queryTab, w, h int) string {
	s := t.board
	var head []string
	if s.b.err != "" {
		head = append(head, errStyle.Render(truncate(textutil.Printable("설정 오류: "+s.b.err, false), w)))
	}
	if len(s.b.panels) == 0 {
		if s.b.err == "" {
			head = append(head, dimStyle.Render("패널 없음 · panels 에 패널을 더하세요"))
		}
		return strings.Join(head, "\n")
	}
	cols := boardCols(w)
	pw := max(12, (w-(cols-1))/cols)
	cur := clamp(s.cursor, len(s.b.panels))
	var rows []string
	var heights []int
	curRow := 0
	for start := 0; start < len(s.b.panels); start += cols {
		end := min(start+cols, len(s.b.panels))
		rh := 0
		for i := start; i < end; i++ {
			rh = max(rh, panelHeight(s.b.panels[i], s.results[i]))
		}
		cells := make([]string, 0, 2*cols)
		for i := start; i < end; i++ {
			if i > start {
				cells = append(cells, " ")
			}
			cells = append(cells, m.panelBox(s, i, pw, rh, i == cur && m.focus != fTree))
		}
		if cur >= start && cur < end {
			curRow = len(rows)
		}
		rows = append(rows, lipgloss.JoinHorizontal(lipgloss.Top, cells...))
		heights = append(heights, rh+2)
	}
	// 고른 줄이 보이도록 그 줄부터 위로 들어가는 만큼 올라간다
	room, top := h-len(head), curRow
	for used := heights[curRow]; top > 0 && used+heights[top-1] <= room; top-- {
		used += heights[top-1]
	}
	lines := append(head, strings.Split(strings.Join(rows[top:], "\n"), "\n")...)
	return strings.Join(lines[:min(len(lines), h)], "\n")
}

// panelBox 는 테두리를 두른 패널 하나다. pw 는 테두리 포함 폭, ih 는 안쪽 줄 수다.
func (m *model) panelBox(s *boardState, i, pw, ih int, focused bool) string {
	p, r := s.b.panels[i], s.results[i]
	iw := max(4, pw-4)
	title := lipgloss.NewStyle().Bold(true).Render(textutil.Printable(p.Title, false))
	src := p.Conn
	if p.View == "flow" {
		src = "흐름 " + p.Flow
	}
	title += dimStyle.Render(" · " + textutil.Printable(src, false))
	if panelProd(m.proj, p) {
		title += " " + prodBadge.Render("prod")
	}
	meta := "…"
	switch {
	case p.err != "":
		meta = "돌리지 않음"
	case !r.at.IsZero():
		meta = fmt.Sprintf("%s · %dms", r.at.Format("15:04:05"), r.elapsed.Milliseconds())
		if s.pending[i] {
			meta += " · 확인 중"
		}
	}
	lines := []string{truncate(title, iw), dimStyle.Render(truncate(meta, iw))}
	body := ih - 2
	switch {
	case p.err != "":
		lines = append(lines, wrapLines(errStyle, "설정 오류: "+p.err, iw, body)...)
	case r.at.IsZero():
	case r.err != "":
		lines = append(lines, wrapLines(errStyle, r.err, iw, body)...)
	case r.note != "":
		lines = append(lines, wrapLines(dimStyle, r.note, iw, body)...)
	default:
		lines = append(lines, panelBody(p, r, iw, body)...)
	}
	border := colBorder
	if focused {
		border = colAccent
	}
	st := lipgloss.NewStyle().Border(lipgloss.RoundedBorder()).BorderForeground(border).Padding(0, 1).Width(pw - 2).Height(ih)
	return st.Render(strings.Join(lines[:min(len(lines), ih)], "\n"))
}

// wrapLines 는 글을 폭 w 로 접어 최대 n 줄을 st 로 칠한다.
func wrapLines(st lipgloss.Style, s string, w, n int) []string {
	parts := strings.Split(ansi.Hardwrap(textutil.Printable(oneLine(s), false), w, true), "\n")
	parts = parts[:min(len(parts), max(1, n))]
	for i, p := range parts {
		parts[i] = st.Render(p)
	}
	return parts
}

// panelValue 는 숫자 패널 값(첫 행 첫 칸, 행이 없고 글이면 첫 줄)이다. 없으면 거짓이다.
func panelValue(res *db.Result) (string, bool) {
	switch {
	case res == nil:
		return "", false
	case len(res.Rows) > 0 && len(res.Rows[0]) > 0:
		return res.Rows[0][0], true
	case res.Text != "":
		v, _, _ := strings.Cut(res.Text, "\n")
		return v, true
	}
	return "", false
}

// panelBody 는 결과 본문 n 줄 이하다.
func panelBody(p boardPanel, r panelResult, w, n int) []string {
	switch p.View {
	case "number":
		v, ok := panelValue(r.res)
		if !ok {
			return []string{dimStyle.Render("행 없음")}
		}
		st := boardNumber
		if p.warn.hit(v) {
			st = dashWarnStyle
		}
		return []string{st.Render(truncate(textutil.Printable(v, false), w))}
	case "chart":
		out, err := chart.Render(r.res, nil, -1, w, n)
		if err != nil {
			return []string{dimStyle.Render(truncate(err.Error(), w))}
		}
		return strings.Split(out, "\n")
	case "flow":
		out := make([]string, 0, len(r.boxes))
		for _, b := range r.boxes {
			st := lipgloss.NewStyle()
			switch {
			case b.failed():
				st = errStyle
			case b.warn:
				st = dashWarnStyle
			case b.note != "":
				st = dimStyle
			}
			out = append(out, st.Render(truncate(boxLabel(b), w)))
		}
		return out
	}
	return compactTable(r.res, w, n)
}

// compactTable 은 머리 줄과 들어가는 만큼의 행이다. 열 폭은 내용에 맞추되 20칸을 넘지 않는다.
func compactTable(res *db.Result, w, n int) []string {
	if res == nil {
		return nil
	}
	if len(res.Cols) == 0 {
		return wrapLines(lipgloss.NewStyle(), cmpText(res), w, n)
	}
	rows := res.Rows[:min(len(res.Rows), max(0, n-1))]
	widths := make([]int, len(res.Cols))
	for i, c := range res.Cols {
		widths[i] = min(20, ansi.StringWidth(textutil.Printable(c, false)))
	}
	for _, row := range rows {
		for i := range widths {
			if i < len(row) {
				widths[i] = min(20, max(widths[i], ansi.StringWidth(textutil.Printable(row[i], false))))
			}
		}
	}
	out := []string{dimStyle.Render(tableLine(res.Cols, widths, w))}
	for _, row := range rows {
		out = append(out, tableLine(row, widths, w))
	}
	if len(res.Rows) == 0 {
		out = append(out, dimStyle.Render("행 없음"))
	}
	return out
}

func tableLine(cells []string, widths []int, w int) string {
	var b strings.Builder
	for i := range widths {
		v := ""
		if i < len(cells) {
			v = textutil.Printable(cells[i], false)
		}
		b.WriteString(padCell(v, widths[i]) + "  ")
	}
	return truncate(strings.TrimRight(b.String(), " "), w)
}

// cmpText 는 열이 없는 결과(명령 태그·Redis 글)의 글이다.
func cmpText(res *db.Result) string {
	if res.Text != "" {
		return res.Text
	}
	return "(빈 결과)"
}

// ─── db board ───

// boardRow 는 `db board` 의 패널 한 줄이다. 숫자 패널은 value, 표·차트·흐름은 columns·rows(50행까지)다.
type boardRow struct {
	Panel     string     `json:"panel"`
	Conn      string     `json:"conn"`
	View      string     `json:"view"`
	Prod      bool       `json:"prod,omitempty"`
	Value     *string    `json:"value,omitempty"`
	Warn      bool       `json:"warn,omitempty"`
	Columns   []string   `json:"columns,omitempty"`
	Rows      [][]string `json:"rows,omitempty"`
	Truncated bool       `json:"truncated,omitempty"`
	Error     string     `json:"error,omitempty"`
	Note      string     `json:"note,omitempty"`
}

// runBoard 는 보드 하나의 패널을 한 번 돌려 패널마다 한 줄을 찍는다. 이름이 없으면 보드 목록이다.
// prod 패널은 --prod 가 없으면 묻지 않고 `--prod 로 확인` 이다(R58.2·R64.3). 실패한 패널이 하나라도 있으면 1 이다.
func runBoard(ctx context.Context, o cliOpts, proj *db.Project, clients *db.Clients, stdout, stderr io.Writer) int {
	if o.format == "csv" {
		fmt.Fprintln(stderr, "db board: --format must be one of table|tsv|json")
		return exitUsage
	}
	if len(o.args) > 1 {
		fmt.Fprintln(stderr, "db board: 보드 이름은 하나만")
		return exitUsage
	}
	if len(o.args) == 0 {
		return listBoardsCLI(o, proj, stdout, stderr)
	}
	b, err := loadBoard(proj, o.args[0])
	if errors.Is(err, os.ErrNotExist) {
		fmt.Fprintf(stderr, "db board: 보드 없음: %s (%s)\n", o.args[0], boardDir)
		return exitUsage
	}
	if err != nil {
		fmt.Fprintln(stderr, "db board: "+textutil.Printable(err.Error(), true))
		return exitUsage
	}
	failed := b.err != ""
	if b.err != "" {
		fmt.Fprintln(stderr, "설정 오류: "+textutil.Printable(b.err, true))
	}
	sources := newSourcePool()
	defer sources.close()
	results := make([]panelResult, len(b.panels))
	var run [][]int
	for _, g := range boardGroups(b.panels) {
		var keep []int
		for _, i := range g {
			// 흐름은 상자마다 prod 를 가른다(resolveFlow)
			if p := b.panels[i]; p.View != "flow" && panelProd(proj, p) && !o.prod {
				results[i].note = dashProdHint
				continue
			}
			keep = append(keep, i)
		}
		run = append(run, keep)
	}
	boardRunAll(run, func(i int) { results[i] = boardRun(ctx, clients, sources, proj, b.panels[i], o.prod) })

	rows := make([]boardRow, len(b.panels))
	for i, p := range b.panels {
		r := results[i]
		row := boardRow{Panel: p.Title, Conn: p.Conn, View: p.View, Prod: panelProd(proj, p), Note: r.note}
		if p.View == "flow" {
			row.Conn = p.Flow
		}
		switch {
		case p.err != "":
			row.Error = "설정 오류: " + p.err
		case r.err != "":
			row.Error = cliErrText(ctx, proj, o, errors.New(r.err), panelConns(p)...)
		case r.note != "":
		case p.View == "flow":
			row.Columns = []string{"stage", "name", "metrics", "warn"}
			for _, bx := range r.boxes {
				m := bx.metrics
				switch {
				case bx.err != "":
					m = cliErrText(ctx, proj, o, errors.New(bx.err), panelConns(p)...)
					row.Error = cmp.Or(row.Error, bx.stage+" "+bx.name+": "+m)
				case bx.note != "":
					m = bx.note
				}
				row.Rows = append(row.Rows, []string{bx.stage, bx.name, m, strconv.FormatBool(bx.warn)})
			}
		case p.View == "number":
			v, ok := panelValue(r.res)
			if ok {
				row.Value, row.Warn = &v, p.warn.hit(v)
			} else {
				empty := ""
				row.Value = &empty
			}
		case r.res != nil:
			row.Columns, row.Rows = r.res.Cols, r.res.Rows
			if len(row.Rows) > boardRows {
				row.Rows, row.Truncated = row.Rows[:boardRows], true
			}
			row.Truncated = row.Truncated || r.res.Truncated
			if len(row.Columns) == 0 && r.res.Text != "" {
				v := r.res.Text
				row.Value = &v
			}
		}
		failed = failed || row.Error != ""
		rows[i] = row
	}
	if o.format == "json" {
		enc := json.NewEncoder(stdout)
		enc.SetEscapeHTML(false)
		err = enc.Encode(rows)
	} else {
		res := &db.Result{Cols: []string{"panel", "conn", "view", "value", "error"}}
		for _, r := range rows {
			v := r.Note
			switch {
			case r.Value != nil:
				v = *r.Value
				if r.Warn {
					v += " !"
				}
			case r.Columns != nil && r.View == "flow":
				parts := make([]string, len(r.Rows))
				for i, x := range r.Rows {
					parts[i] = strings.TrimSpace(x[0] + " " + x[1] + " " + x[2])
					if x[3] == "true" {
						parts[i] += " !"
					}
				}
				v = strings.Join(parts, " · ")
			case r.Columns != nil:
				v = fmt.Sprintf("%d행", len(r.Rows))
				if r.Truncated {
					v += "+"
				}
			}
			res.Rows = append(res.Rows, []string{r.Panel, r.Conn, r.View, v, r.Error})
		}
		err = writeResults(stdout, stderr, o, []cliResult{{res: res}})
	}
	if err != nil {
		fmt.Fprintln(stderr, err)
		return exitErr
	}
	if failed {
		return exitErr
	}
	return exitOK
}

// listBoardsCLI 는 `db board` (이름 없음)의 보드 목록이다: name·title·panels·error.
func listBoardsCLI(o cliOpts, proj *db.Project, stdout, stderr io.Writer) int {
	names := listBoards(proj.Dir)
	if len(names) == 0 {
		fmt.Fprintf(stderr, "db board: 보드 없음 (%s/<이름>.json)\n", boardDir)
	}
	type item struct {
		Name   string `json:"name"`
		Title  string `json:"title"`
		Panels int    `json:"panels"`
		Error  string `json:"error,omitempty"`
	}
	items := []item{}
	for _, n := range names {
		it := item{Name: n}
		if b, err := loadBoard(proj, n); err != nil {
			it.Error = err.Error()
		} else {
			it.Title, it.Panels, it.Error = b.title, len(b.panels), b.err
		}
		items = append(items, it)
	}
	var err error
	if o.format == "json" {
		enc := json.NewEncoder(stdout)
		enc.SetEscapeHTML(false)
		err = enc.Encode(items)
	} else {
		res := &db.Result{Cols: []string{"name", "title", "panels", "error"}}
		for _, it := range items {
			res.Rows = append(res.Rows, []string{it.Name, it.Title, strconv.Itoa(it.Panels), it.Error})
		}
		err = writeResults(stdout, stderr, o, []cliResult{{res: res}})
	}
	if err != nil {
		fmt.Fprintln(stderr, err)
		return exitErr
	}
	return exitOK
}
