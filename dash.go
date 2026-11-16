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

	"github.com/Jeong-Jae-Hun/i-db/internal/awsres"
	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/flows"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
	"github.com/Jeong-Jae-Hun/i-db/internal/textutil"
)

// ─── 대시보드 (R64) ───

var (
	// probeConn 은 접속 하나를 읽기 전용 메타데이터로 살펴본다(R64.2). 화면과 `db status` 가 같이 쓴다.
	// ctx 는 취소만 넘기고 제한 시간은 확인 함수가 건다(db.ProbeOpenTimeout·db.ProbeTimeout).
	// 결과의 Err 는 비밀값을 지운 한 줄이어야 한다. 테스트가 바꿔 끼운다
	probeConn = probeAny
	// 대시보드가 활성 탭인 동안 다시 확인하는 간격(R64.2). 테스트가 줄인다
	dashEvery = 30 * time.Second
)

const (
	dashProdHint = "--prod 로 확인" // CLI 가 확인하지 않은 prod 접속(R64.3)
	dashHint     = "r 새로고침 · Enter 트리에서 고르기 · / 거르기 · Ctrl+W 닫기 · F1 도움말"
	// 표가 이만큼 줄을 못 얻으면 요약 카드를 한 줄로 접는다(R64.1)
	dashMinRows = 5
)

var (
	dashCardStyle = lipgloss.NewStyle().Border(lipgloss.RoundedBorder()).BorderForeground(colBorder).Padding(0, 1)
	dashOKStyle   = lipgloss.NewStyle().Foreground(lipgloss.Color("114"))
	dashWarnStyle = lipgloss.NewStyle().Foreground(colStg).Bold(true)
)

// dashState 는 대시보드 탭의 상태다. 확인 결과는 접속 uuid 로 들고 있어 접속을 다시 읽어 자리가 바뀌어도 맞는다.
// 다시 확인하는 동안 health 는 이전 결과를 그대로 두고, pending 은 머리의 `확인 중 N` 만 센다.
type dashState struct {
	health  map[string]db.Health
	pending map[string]bool
	// 확인을 돌릴 때마다 올린다. 늦게 온 이전 확인 결과와 이전 타이머(r 로 다시 센 뒤)를 버린다
	gen int
	// cancel 은 진행 중인 확인 한 회차를 멈춘다. 다시 확인하거나 탭을 닫을 때 부른다
	cancel context.CancelFunc
	// cursor 는 거른 줄 수를 넘을 수 있다(접속을 다시 읽어 줄었을 때). 그릴 때는 잘라 쓰고 키를 받을 때 맞춘다
	cursor int
	filter string
	// outbox 는 흐름(R69) 아웃박스의 재시도 수다. 흐름 이름으로 들고 재시도 열이 있는 아웃박스만 센다
	outbox map[string]int64
}

// dashProbeMsg 는 접속 하나의 확인 결과다.
type dashProbeMsg struct {
	tab, gen int
	uuid     string
	h        db.Health
}

// dashOutboxMsg 는 흐름 아웃박스 하나의 재시도 수다. 실패하면 ok 가 거짓이고 합에서 뺀다.
type dashOutboxMsg struct {
	tab, gen int
	name     string
	retry    int64
	ok       bool
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
	t.dash = &dashState{health: map[string]db.Health{}, pending: map[string]bool{}, outbox: map[string]int64{}}
	m.setFocus(fResult)
	return m.dashRefresh(t)
}

// dashRefresh 는 모든 접속을 동시에 확인하고 dashEvery 뒤 다시 확인할 타이머를 건다.
// 진행 중인 이전 회차는 취소하고, 이전 타이머·늦게 온 결과는 gen 이 낡아 버려진다.
func (m *model) dashRefresh(t *queryTab) tea.Cmd {
	d := t.dash
	d.stop()
	d.gen++
	ctx, cancel := context.WithCancel(m.life)
	d.cancel = cancel
	id, gen, clients, sources := t.id, d.gen, m.clients, m.sources
	cmds := []tea.Cmd{tea.Tick(dashEvery, func(time.Time) tea.Msg { return dashTickMsg{tab: id, gen: gen} })}
	for _, i := range connOrder(m.proj.Conns) {
		c := m.conn(i)
		d.pending[c.UUID] = true
		cmds = append(cmds, func() tea.Msg {
			return dashProbeMsg{tab: id, gen: gen, uuid: c.UUID, h: probeConn(ctx, clients, sources, c)}
		})
	}
	cmds = append(cmds, m.dashOutboxes(ctx, t)...)
	m.status = "대시보드 확인 중…"
	return tea.Batch(cmds...)
}

// dashOutboxes 는 흐름(R69) 아웃박스 가운데 재시도 열이 있는 것의 재시도 수를 확인과 같은 제한(5초)으로 센다.
// 대시보드는 prod 접속도 확인하므로 prod 아웃박스도 센다. flows.json 이 없거나 깨졌으면 아무것도 하지 않는다.
func (m *model) dashOutboxes(ctx context.Context, t *queryTab) []tea.Cmd {
	d := t.dash
	fs, _ := flows.Load(m.proj.Dir, m.proj.Conns)
	id, gen, clients := t.id, d.gen, m.clients
	// 다시 확인하는 동안에도 이전 수를 보인다(R64.1). flows.json 에서 빠진 흐름만 지운다
	keep := map[string]bool{}
	var cmds []tea.Cmd
	for _, f := range fs {
		o := f.Outbox
		if f.Err != "" || o == nil || o.Retry == "" {
			continue
		}
		i := connByName(m.proj, o.Conn)
		if i < 0 {
			continue
		}
		c, name := m.conn(i), f.Name
		d.pending["outbox:"+name] = true
		keep[name] = true
		if _, ok := d.outbox[name]; !ok {
			d.outbox[name] = -1 // 아직 결과 없음. 카드는 보이되 합에 넣지 않는다
		}
		cmds = append(cmds, func() tea.Msg {
			st, err := flowOutbox(ctx, clients, c, *o)
			return dashOutboxMsg{tab: id, gen: gen, name: name, retry: st.Retry, ok: err == nil}
		})
	}
	for name := range d.outbox {
		if !keep[name] {
			delete(d.outbox, name)
		}
	}
	return cmds
}

func (m *model) dashOutboxed(msg dashOutboxMsg) {
	t := m.findTab(msg.tab)
	if t == nil || t.dash == nil || t.dash.gen != msg.gen {
		return
	}
	t.dash.outbox[msg.name] = -1
	if msg.ok {
		t.dash.outbox[msg.name] = msg.retry
	}
	m.dashSettle(t.dash, "outbox:"+msg.name)
}

// dashSettle 은 확인 하나가 끝났다고 적고, 모두 끝났으면 상태 줄을 바꾼다.
func (m *model) dashSettle(d *dashState, key string) {
	delete(d.pending, key)
	if len(d.pending) == 0 && strings.HasPrefix(m.status, "대시보드 확인 중") {
		m.status = "대시보드 확인함 " + time.Now().In(textutil.KST).Format("15:04:05")
	}
}

// stop 은 진행 중인 확인 회차를 취소한다.
func (d *dashState) stop() {
	if d.cancel != nil {
		d.cancel()
	}
}

func (m *model) dashProbed(msg dashProbeMsg) {
	t := m.findTab(msg.tab)
	if t == nil || t.dash == nil || t.dash.gen != msg.gen {
		return
	}
	t.dash.health[msg.uuid] = msg.h
	m.dashSettle(t.dash, msg.uuid)
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
		if f == "" || strings.Contains(strings.ToLower(db.FolderOf(c)+"\x00"+c.Name+"\x00"+c.Env()), f) {
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

// dashSummary 는 요약 카드 숫자다(R64.1). ok·fail 은 결과가 한 번이라도 온 접속을 마지막 결과로 센다.
type dashSummary struct {
	total, pg, redis, source int
	prod, stg, local         int
	ok, fail                 int
	pgActive, redisClients   int
	openTx, unlocked         int
	// aws 는 aws 접속 수, dlq 는 그 접속들의 `DLQ 쌓임` 합이다. outboxes 는 재시도를 세는 흐름 아웃박스 수, retry 는 그 합이다(R69)
	aws, dlq        int
	outboxes, retry int
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
		if c.Kind == db.KindAWS {
			s.aws++
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
		if !ok {
			continue
		}
		if !h.OK {
			s.fail++
			continue
		}
		s.ok++
		switch c.Kind {
		case db.KindPG:
			s.pgActive += metricInt(h, db.MetricPGActive)
		case db.KindRedis:
			s.redisClients += metricInt(h, db.MetricRedisClients)
		case db.KindAWS:
			s.dlq += metricInt(h, awsres.MetricDLQ)
		}
	}
	for _, n := range d.outbox {
		s.outboxes++
		s.retry += int(max(0, n))
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
	if s.aws > 0 {
		cards = append(cards, dashCard("DLQ 쌓인 큐", warnIf(s.dlq)))
	}
	if s.outboxes > 0 {
		cards = append(cards, dashCard("아웃박스 재시도", warnIf(s.retry)))
	}
	lines := strings.Split(wrapCards(cards, w), "\n")
	var extra []string
	if d.filter != "" {
		extra = append(extra, dimStyle.Render("거르기 "+textutil.Printable(d.filter, false)+" · esc 해제"))
	}
	// 머리 줄 하나를 뺀 표 줄이 dashMinRows 보다 적으면 카드를 한 줄 요약으로 접는다
	if h-len(lines)-len(extra)-1 < dashMinRows {
		lines = []string{truncate(fmt.Sprintf("접속 %d · prod %d · stg %d · local %d · ● %d ✕ %d · PG 활성 %d · Redis 클라이언트 %d · 트랜잭션 %d · 잠금 해제 %d",
			s.total, s.prod, s.stg, s.local, s.ok, s.fail, s.pgActive, s.redisClients, s.openTx, s.unlocked)+dashFlowSummary(s), w)}
	}
	lines = append(lines, extra...)
	rows := m.dashRows(d)
	table := dashTable(m, d, rows, w, max(1, h-len(lines)), m.focus != fTree)
	return strings.Join(append(lines, table...), "\n")
}

// dashFlowSummary 는 접은 한 줄 요약에 붙일 DLQ·아웃박스 재시도다. 카드가 없으면 빈 글이다.
func dashFlowSummary(s dashSummary) string {
	var out string
	if s.aws > 0 {
		out += fmt.Sprintf(" · DLQ 쌓인 큐 %d", s.dlq)
	}
	if s.outboxes > 0 {
		out += fmt.Sprintf(" · 아웃박스 재시도 %d", s.retry)
	}
	return out
}

// warnIf 는 0보다 크면 경고 색인 수다.
func warnIf(n int) string {
	if n > 0 {
		return dashWarnStyle.Render(strconv.Itoa(n))
	}
	return strconv.Itoa(n)
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
		if hh, ok := d.health[c.UUID]; ok {
			st, ver = "✕", hh.Version
			if hh.OK {
				st = "●"
			}
			if hh.Latency > 0 {
				lat = fmt.Sprintf("%dms", hh.Latency.Milliseconds())
			}
		}
		cells[r] = []string{db.FolderOf(c), connLabel(c), c.Env(), connKinds[c.Kind], st, lat, ver}
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
	cur := clamp(d.cursor, len(rows))
	room := max(1, h-1)
	top := max(0, cur-room+1)
	for r := top; r < len(rows) && r < top+room; r++ {
		c := m.conn(rows[r])
		mark := "  "
		if r == cur {
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
	if !ok {
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
// PostgreSQL·Redis 는 확인 함수가 제한 시간을 건다. 저장소도 같게 여는 데 ProbeOpenTimeout, 확인에 ProbeTimeout 이다.
func probeAny(ctx context.Context, clients *db.Clients, sources *sourcePool, c db.Conn) db.Health {
	switch c.Kind {
	case db.KindPG:
		return clients.ProbePG(ctx, c)
	case db.KindRedis:
		return clients.ProbeRedis(ctx, c)
	}
	start := time.Now()
	octx, cancel := context.WithTimeout(ctx, db.ProbeOpenTimeout)
	b, err := sources.get(octx, c)
	cancel()
	if err != nil {
		return db.Failed(time.Since(start), err, c.Password)
	}
	p, ok := b.(source.Prober)
	if !ok {
		return db.Health{Err: "이 종류는 확인을 지원하지 않음"}
	}
	ctx, cancel = context.WithTimeout(ctx, db.ProbeTimeout)
	defer cancel()
	return p.Probe(ctx)
}
