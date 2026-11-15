package main

import (
	"context"
	"fmt"
	"strconv"
	"strings"
	"time"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
	"github.com/charmbracelet/x/ansi"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
	"github.com/Jeong-Jae-Hun/i-db/internal/textutil"
)

// ─── 대시보드 (R64) ───

var (
	// probeConn 은 접속 하나를 읽기 전용 메타데이터로 살펴본다(R64.2). 화면과 `db status` 가 같이 쓴다.
	// ctx 에는 probeTimeout 이 걸려 있다. 결과의 Err 는 비밀값을 지운 한 줄이어야 한다.
	// 테스트가 바꿔 끼운다
	probeConn = probeAny
	// 접속마다 확인 한 번의 시간 제한(R64.2)
	probeTimeout = 5 * time.Second
	// 대시보드가 활성 탭인 동안 다시 확인하는 간격(R64.2). 테스트가 줄인다
	dashEvery = 30 * time.Second
)

// 요약 카드가 더하는 수치 이름. 확인 함수가 Metrics 에 이 이름으로 정수(앞자리 숫자)를 싣는다
const (
	metricPGActive     = "활성"
	metricRedisClients = "클라이언트"
	dashProdHint       = "--prod 로 확인" // CLI 가 확인하지 않은 prod 접속(R64.3)
	dashHint           = "r 새로고침 · Enter 트리에서 고르기 · / 거르기 · Ctrl+W 닫기 · F1 도움말"
)

var (
	dashCardStyle = lipgloss.NewStyle().Border(lipgloss.RoundedBorder()).BorderForeground(colBorder).Padding(0, 1)
	dashOKStyle   = lipgloss.NewStyle().Foreground(lipgloss.Color("114"))
	dashWarnStyle = lipgloss.NewStyle().Foreground(colStg).Bold(true)
)

// dashState 는 대시보드 탭의 상태다. 확인 결과는 접속 uuid 로 들고 있어 접속을 다시 읽어 자리가 바뀌어도 맞는다.
type dashState struct {
	health  map[string]db.Health
	pending map[string]bool
	// 확인을 돌릴 때마다 올린다. 늦게 온 이전 확인 결과와 이전 타이머(r 로 다시 센 뒤)를 버린다
	gen    int
	cursor int
	filter string
}

// dashProbeMsg 는 접속 하나의 확인 결과다.
type dashProbeMsg struct {
	tab, gen int
	uuid     string
	h        db.Health
}

// dashTickMsg 는 다시 확인할 때가 됐다는 알림이다. 탭을 닫았거나 gen 이 낡았으면 버려 타이머가 멈춘다.
type dashTickMsg struct{ tab, gen int }

// connOrder 는 트리와 같은 순서(그룹 이름 순, 그룹 안은 접속 순)의 접속 인덱스다. 지운 자리(Kind "")는 뺀다.
func connOrder(conns []db.Conn) []int {
	var out []int
	for _, g := range connRoots(conns, nil, nil) {
		for _, cn := range g.children {
			out = append(out, cn.conn)
		}
	}
	return out
}

// openDash 는 대시보드 탭을 열거나 이미 있으면 그 탭으로 옮겨 바로 다시 확인한다. 대시보드 탭은 하나뿐이다.
func (m *model) openDash() tea.Cmd {
	for i, t := range m.tabs {
		if t.dash != nil {
			m.switchTab(i)
			m.setFocus(fResult)
			return m.dashRefresh(t)
		}
	}
	t := m.newTab()
	m.setConn(t, -1)
	t.title = "대시보드"
	t.dash = &dashState{health: map[string]db.Health{}, pending: map[string]bool{}}
	m.setFocus(fResult)
	return m.dashRefresh(t)
}

// dashRefresh 는 모든 접속을 동시에 확인하고 dashEvery 뒤 다시 확인할 타이머를 건다. 이전 타이머는 gen 이 낡아 버려진다.
func (m *model) dashRefresh(t *queryTab) tea.Cmd {
	d := t.dash
	d.gen++
	id, gen, life, clients, sources := t.id, d.gen, m.life, m.clients, m.sources
	cmds := []tea.Cmd{tea.Tick(dashEvery, func(time.Time) tea.Msg { return dashTickMsg{tab: id, gen: gen} })}
	for _, i := range connOrder(m.proj.Conns) {
		c := m.conn(i)
		d.pending[c.UUID] = true
		cmds = append(cmds, func() tea.Msg {
			ctx, cancel := context.WithTimeout(life, probeTimeout)
			defer cancel()
			return dashProbeMsg{tab: id, gen: gen, uuid: c.UUID, h: probeConn(ctx, clients, sources, c)}
		})
	}
	m.status = "대시보드 확인 중…"
	return tea.Batch(cmds...)
}

func (m *model) dashProbed(msg dashProbeMsg) {
	t := m.findTab(msg.tab)
	if t == nil || t.dash == nil || t.dash.gen != msg.gen {
		return
	}
	t.dash.health[msg.uuid] = msg.h
	delete(t.dash.pending, msg.uuid)
	if len(t.dash.pending) == 0 && strings.HasPrefix(m.status, "대시보드 확인 중") {
		m.status = "대시보드 확인함 " + time.Now().Format("15:04:05")
	}
}

// dashTicked 는 타이머가 돌아오면 대시보드가 활성 탭일 때만 다시 확인한다. 다른 탭이면 확인 없이 다음 타이머만 건다.
func (m *model) dashTicked(msg dashTickMsg) tea.Cmd {
	t := m.findTab(msg.tab)
	if t == nil || t.dash == nil || t.dash.gen != msg.gen {
		return nil
	}
	if m.cur() == t {
		return m.dashRefresh(t)
	}
	id, gen := msg.tab, msg.gen
	return tea.Tick(dashEvery, func(time.Time) tea.Msg { return dashTickMsg{tab: id, gen: gen} })
}

// dashRows 는 거르기(그룹·이름·환경 부분 글자)를 지난 접속 인덱스다.
func (m *model) dashRows(d *dashState) []int {
	f := strings.ToLower(d.filter)
	var out []int
	for _, i := range connOrder(m.proj.Conns) {
		c := m.conn(i)
		if f == "" || strings.Contains(strings.ToLower(db.GroupOf(c)+"\x00"+c.Name+"\x00"+c.Env()), f) {
			out = append(out, i)
		}
	}
	return out
}

func (m *model) dashKey(k tea.KeyMsg) tea.Cmd {
	t := m.cur()
	d := t.dash
	rows := m.dashRows(d)
	switch k.String() {
	case "r":
		return m.dashRefresh(t)
	case "up", "k":
		d.cursor--
	case "down", "j":
		d.cursor++
	case "pgup":
		d.cursor -= 10
	case "pgdown":
		d.cursor += 10
	case "home", "g":
		d.cursor = 0
	case "end", "G":
		d.cursor = len(rows) - 1
	case "/":
		m.prompt("대시보드 거르기 (그룹·이름·환경) ", d.filter, func(v string) tea.Cmd {
			d.filter, d.cursor = strings.TrimSpace(v), 0
			return nil
		})
		return nil
	case "esc":
		d.filter = ""
	case "enter":
		if len(rows) == 0 {
			return nil
		}
		m.dashSelect(rows[clamp(d.cursor, len(rows))])
		return nil
	}
	d.cursor = clamp(d.cursor, len(m.dashRows(d)))
	return nil
}

// dashSelect 는 트리에서 접속 노드를 골라 보이게 하고 트리로 옮긴다. 탭 접속은 바꾸지 않는다(트리 Enter 로 바꾼다).
func (m *model) dashSelect(ci int) {
	cn := m.connNode(ci)
	if cn == nil {
		m.status = "트리에서 찾지 못함: " + m.conn(ci).Name
		return
	}
	m.filter = ""
	for p := cn.parent; p != nil; p = p.parent {
		p.expanded = true
	}
	m.rebuild()
	m.selectNode(cn)
	m.setFocus(fTree)
}

// ─── 요약 ───

// dashSummary 는 요약 카드 숫자다(R64.1). ok·fail 은 확인이 끝난 접속만 센다.
type dashSummary struct {
	total, pg, redis, source int
	prod, stg, local         int
	ok, fail                 int
	pgActive, redisClients   int
	openTx, unlocked         int
}

func (m *model) dashSum(d *dashState) dashSummary {
	var s dashSummary
	for _, i := range connOrder(m.proj.Conns) {
		c := m.conn(i)
		s.total++
		switch {
		case c.Kind == db.KindPG:
			s.pg++
		case c.Kind == db.KindRedis:
			s.redis++
		default:
			s.source++
		}
		switch c.Env() {
		case "prod":
			s.prod++
			if lockNow().Before(m.unlocked[i]) {
				s.unlocked++
			}
		case "stg":
			s.stg++
		default:
			s.local++
		}
		if m.inTx[i] {
			s.openTx++
		}
		h, ok := d.health[c.UUID]
		if !ok || d.pending[c.UUID] {
			continue
		}
		if !h.OK {
			s.fail++
			continue
		}
		s.ok++
		switch c.Kind {
		case db.KindPG:
			s.pgActive += metricInt(h, metricPGActive)
		case db.KindRedis:
			s.redisClients += metricInt(h, metricRedisClients)
		}
	}
	return s
}

// metricInt 는 이름이 name 인 수치 값의 앞자리 정수다(`1,234`·`12 / 100` 도 받는다). 없거나 숫자가 아니면 0 이다.
func metricInt(h db.Health, name string) int {
	for _, x := range h.Metrics {
		if x.Name != name {
			continue
		}
		v := strings.ReplaceAll(strings.TrimSpace(x.Value), ",", "")
		end := strings.IndexFunc(v, func(r rune) bool { return r < '0' || r > '9' })
		if end < 0 {
			end = len(v)
		}
		n, _ := strconv.Atoi(v[:end])
		return n
	}
	return 0
}

// ─── 그리기 ───

// dashView 는 탭 줄 아래 오른쪽 영역 전체(편집기·결과 자리)에 그릴 대시보드다. w·h 는 테두리 안쪽 크기다.
func (m *model) dashView(t *queryTab, w, h int) string {
	d := t.dash
	s := m.dashSum(d)
	cards := []string{
		dashCard("접속", fmt.Sprintf("%d (PG %d · Redis %d · 저장소 %d)", s.total, s.pg, s.redis, s.source)),
		dashCard("환경", envStyle(db.Conn{Name: "prod"}).Render(fmt.Sprintf("prod %d", s.prod))+" · "+
			envStyle(db.Conn{Name: "stg"}).Render(fmt.Sprintf("stg %d", s.stg))+fmt.Sprintf(" · local %d", s.local)),
		dashCard("연결", dashOKStyle.Render(fmt.Sprintf("● %d", s.ok))+" / "+errStyle.Render(fmt.Sprintf("✕ %d", s.fail))),
		dashCard("PG 활성 세션", strconv.Itoa(s.pgActive)),
		dashCard("Redis 클라이언트", strconv.Itoa(s.redisClients)),
		dashCard("열린 트랜잭션", strconv.Itoa(s.openTx)),
		dashCard("prod 잠금 해제", strconv.Itoa(s.unlocked)),
	}
	lines := strings.Split(wrapCards(cards, w), "\n")
	if d.filter != "" {
		lines = append(lines, dimStyle.Render("거르기 "+textutil.Printable(d.filter, false)+" · esc 해제"))
	}
	rows := m.dashRows(d)
	table := dashTable(m, d, rows, w, max(1, h-len(lines)), m.focus != fTree)
	return strings.Join(append(lines, table...), "\n")
}

func dashCard(title, value string) string {
	return dashCardStyle.Render(dimStyle.Render(title) + "\n" + value)
}

// wrapCards 는 카드를 폭 w 에 맞게 왼쪽부터 채우고 넘치면 다음 줄로 넘긴다.
func wrapCards(cards []string, w int) string {
	var out, row []string
	width := 0
	for _, c := range cards {
		cw := lipgloss.Width(c)
		if len(row) > 0 && width+1+cw > w {
			out = append(out, lipgloss.JoinHorizontal(lipgloss.Top, row...))
			row, width = nil, 0
		}
		if len(row) > 0 {
			row, width = append(row, " "), width+1
		}
		row, width = append(row, c), width+cw
	}
	if len(row) > 0 {
		out = append(out, lipgloss.JoinHorizontal(lipgloss.Top, row...))
	}
	return strings.Join(out, "\n")
}

// dashCols 는 표 열 이름과 앞 일곱 열의 최대 폭이다. 수치는 남은 폭을 다 쓴다.
var (
	dashCols  = []string{"그룹", "이름", "환경", "종류", "상태", "지연", "버전", "수치"}
	dashWidth = []int{16, 24, 5, 10, 4, 7, 18}
)

// dashTable 은 머리 줄과 접속 줄이다. 커서 줄이 보이게 위를 맞추고, 앞의 ▸ 로 고른 줄을 보인다.
func dashTable(m *model, d *dashState, rows []int, w, h int, focused bool) []string {
	cells := make([][]string, len(rows))
	widths := make([]int, len(dashWidth))
	for i, c := range dashCols[:len(widths)] {
		widths[i] = ansi.StringWidth(c)
	}
	for r, ci := range rows {
		c := m.conn(ci)
		st, lat, ver := "…", "", ""
		if hh, ok := d.health[c.UUID]; ok && !d.pending[c.UUID] {
			st, ver = "✕", hh.Version
			if hh.OK {
				st = "●"
			}
			if hh.Latency > 0 {
				lat = fmt.Sprintf("%dms", hh.Latency.Milliseconds())
			}
		}
		cells[r] = []string{db.GroupOf(c), connLabel(c), c.Env(), connKinds[c.Kind], st, lat, ver}
		for i := range widths {
			cells[r][i] = textutil.Printable(cells[r][i], false)
			widths[i] = min(dashWidth[i], max(widths[i], ansi.StringWidth(cells[r][i])))
		}
	}
	head := "  "
	for i, c := range dashCols[:len(widths)] {
		head += padCell(c, widths[i]) + "  "
	}
	out := []string{dimStyle.Render(truncate(head+dashCols[len(widths)], w))}
	if len(rows) == 0 {
		return append(out, dimStyle.Render("접속 없음"))
	}
	d.cursor = clamp(d.cursor, len(rows))
	room := max(1, h-1)
	top := max(0, d.cursor-room+1)
	for r := top; r < len(rows) && r < top+room; r++ {
		c := m.conn(rows[r])
		mark := "  "
		if r == d.cursor {
			mark = "▸ "
			if focused {
				mark = lipgloss.NewStyle().Foreground(colAccent).Bold(true).Render(mark)
			}
		}
		var b strings.Builder
		b.WriteString(mark)
		for i, v := range cells[r] {
			cell := padCell(v, widths[i])
			switch {
			case i == 2:
				cell = envStyle(c).Render(cell)
			case i == 4 && v == "●":
				cell = dashOKStyle.Render(cell)
			case i == 4 && v == "✕":
				cell = errStyle.Render(cell)
			case i == 4:
				cell = dimStyle.Render(cell)
			}
			b.WriteString(cell + "  ")
		}
		b.WriteString(dashMetrics(d, c))
		out = append(out, truncate(b.String(), w))
	}
	return out
}

// dashMetrics 는 수치 칸이다. Warn 인 수치는 경고 색으로, 실패면 오류 글을 빨강으로 보인다.
func dashMetrics(d *dashState, c db.Conn) string {
	h, ok := d.health[c.UUID]
	if !ok || d.pending[c.UUID] {
		return ""
	}
	if !h.OK {
		return errStyle.Render(textutil.Printable(oneLine(db.Redact(h.Err, c.Password)), false))
	}
	parts := make([]string, len(h.Metrics))
	for i, x := range h.Metrics {
		parts[i] = textutil.Printable(x.String(), false)
		if x.Warn {
			parts[i] = dashWarnStyle.Render(parts[i])
		}
	}
	return strings.Join(parts, " · ")
}

// padCell 은 v 를 폭 w 에 맞춰 자르거나 공백으로 채운다(넓은 글자는 2칸).
func padCell(v string, w int) string {
	v = ansi.Truncate(v, w, "…")
	return v + strings.Repeat(" ", max(0, w-ansi.StringWidth(v)))
}

// probeAny 는 종류별 확인을 고른다. 저장소는 Backend 가 source.Prober 를 구현할 때만 확인한다.
func probeAny(ctx context.Context, clients *db.Clients, sources *sourcePool, c db.Conn) db.Health {
	switch c.Kind {
	case db.KindPG:
		return clients.ProbePG(ctx, c)
	case db.KindRedis:
		return clients.ProbeRedis(ctx, c)
	}
	start := time.Now()
	b, err := sources.get(ctx, c)
	if err != nil {
		return db.Failed(time.Since(start), err, c.Password)
	}
	p, ok := b.(source.Prober)
	if !ok {
		return db.Health{Err: "이 종류는 확인을 지원하지 않음"}
	}
	return p.Probe(ctx)
}
