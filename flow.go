package main

import (
	"cmp"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"maps"
	"slices"
	"strconv"
	"strings"
	"sync"
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

// ─── 이벤트 흐름 (R69) ───

var (
	// flowOutbox 는 아웃박스 수를 읽는다. 화면·대시보드·`db flows` 가 같이 쓰고 테스트가 바꿔 끼운다
	flowOutbox = outboxCount
	// flowTopic 은 aws 접속으로 토픽을 풀어 낸다. 테스트가 바꿔 끼운다
	flowTopic = topicFlow
	// 흐름 탭이 활성인 동안 다시 푸는 간격. 테스트가 줄인다
	flowEvery = 30 * time.Second
)

const (
	// flowAWSTimeout 은 토픽 하나를 푸는 시간이다. 구독·큐·DLQ·매핑을 차례로 물어 질의 한 번(ProbeTimeout)보다 길게 준다
	flowAWSTimeout = 15 * time.Second
	flowHint       = "r 새로고침 · ↑↓ 흐름 · ←→ 상자 · Enter 상세 · Ctrl+W 닫기 · F1 도움말"
	flowNoFile     = "흐름 없음 · .dbtui/flows.json 을 쓰거나 aws 트리의 SNS 토픽에서 F"
	// stageType 은 아웃박스 종류 한 줄(R69.6)이다. 그림에서는 아웃박스 아래 줄로, `db flows` 에서는 한 줄로 보인다
	stageType = "outbox-type"
)

var flowSelStyle = lipgloss.NewStyle().Reverse(true)

// outboxCount 는 대시보드 확인처럼 여는 데 ProbeOpenTimeout, 질의에 ProbeTimeout(5초)을 걸고 READ ONLY 로 센다.
func outboxCount(ctx context.Context, clients *db.Clients, c db.Conn, o flows.Outbox) (flows.Stat, error) {
	octx, cancel := context.WithTimeout(ctx, db.ProbeOpenTimeout)
	pg, err := clients.PG(octx, c)
	cancel()
	if err != nil {
		return flows.Stat{}, err
	}
	ctx, cancel = context.WithTimeout(ctx, db.ProbeTimeout)
	defer cancel()
	return flows.Count(ctx, pg, o)
}

// topicResolver 는 흐름을 풀 수 있는 Backend(awsres.Backend)다.
type topicResolver interface {
	TopicFlow(ctx context.Context, topic string) (*awsres.Topic, error)
}

func topicFlow(ctx context.Context, sources *sourcePool, c db.Conn, topic string) (*awsres.Topic, error) {
	octx, cancel := context.WithTimeout(ctx, db.ProbeOpenTimeout)
	b, err := sources.get(octx, c)
	cancel()
	if err != nil {
		return nil, err
	}
	r, ok := b.(topicResolver)
	if !ok {
		return nil, fmt.Errorf("%s is not an aws connection", c.Name)
	}
	ctx, cancel = context.WithTimeout(ctx, flowAWSTimeout)
	defer cancel()
	return r.TopicFlow(ctx, topic)
}

// flowSnap 은 흐름 하나를 한 번 푼 결과다. note 는 확인하지 않은 prod(`--prod 로 확인`, CLI)다.
type flowSnap struct {
	outbox                *flows.Stat
	outboxErr, outboxNote string
	topic                 *awsres.Topic
	topicErr, topicNote   string
}

// resolveFlow 는 아웃박스 수와 토픽을 동시에 푼다. prod 가 거짓이면 prod 접속은 묻지 않는다(R58.2). 오류는 비밀번호를 지운 한 줄이다.
func resolveFlow(ctx context.Context, clients *db.Clients, sources *sourcePool, proj *db.Project, f flows.Flow, prod bool) flowSnap {
	var s flowSnap
	if f.Err != "" {
		return s
	}
	var wg sync.WaitGroup
	if o := f.Outbox; o != nil {
		switch i := connByName(proj, o.Conn); {
		case i < 0:
			s.outboxErr = "no such connection: " + o.Conn
		case proj.Conns[i].Env() == "prod" && !prod:
			s.outboxNote = dashProdHint
		default:
			c := proj.Conns[i]
			wg.Add(1)
			go func() {
				defer wg.Done()
				st, err := flowOutbox(ctx, clients, c, *o)
				if err != nil {
					s.outboxErr = oneLine(db.Redact(err.Error(), c.Password))
					return
				}
				s.outbox = &st
			}()
		}
	}
	switch i := connByName(proj, f.AWS); {
	case f.Topic == "":
	case i < 0:
		s.topicErr = "no such connection: " + f.AWS
	case proj.Conns[i].Env() == "prod" && !prod:
		s.topicNote = dashProdHint
	default:
		tp, err := flowTopic(ctx, sources, proj.Conns[i], f.Topic)
		if err != nil {
			s.topicErr = oneLine(err.Error())
		}
		s.topic = tp
	}
	wg.Wait()
	return s
}

// ─── 상자 ───

// flowBox 는 그림의 상자 하나이자 `db flows` 의 한 줄이다. sub 는 구독 줄 자리(-1 이면 아웃박스·SNS·아웃박스 종류)다.
// conn·path 는 Enter 로 열 상세다(아웃박스·종류는 path 가 비고 conn 이 PostgreSQL 접속이다).
// 종류 줄은 pad 만큼 이름 뒤를 띄워 수치 열을 맞추고, idle 은 설정에만 있고 행이 없는 종류다(흐리게, 자리가 있을 때만).
// nullType 은 종류 열이 NULL 인 줄이다(이름은 `(NULL)`, 상세는 is null).
type flowBox struct {
	stage, name, metrics string
	short                string // 그림에 보일 짧은 수치. 비면 metrics 다
	warn, idle, nullType bool
	err, note            string
	sub, pad             int
	conn, path           string
}

func (b flowBox) failed() bool { return b.err != "" }

// lambdaOK 는 매핑의 마지막 처리 결과가 문제 없음인지다. `No records processed` 는 메시지가 없던 매핑의 보통 값이라 경고하지 않는다.
func lambdaOK(r string) bool { return r == "OK" || r == "" || r == "No records processed" }

func atoiOr0(s string) int {
	n, _ := strconv.Atoi(s)
	return n
}

// flowBoxes 는 흐름을 아웃박스 → SNS → 구독(큐 → DLQ·Lambda) 순서의 상자로 편다. 아직 결과가 없으면 수치는 `…` 다.
func flowBoxes(f flows.Flow, s flowSnap, done bool) []flowBox {
	if f.Err != "" {
		return []flowBox{{stage: "config", name: f.Name, err: "설정 오류: " + f.Err, sub: -1}}
	}
	pending := "…"
	if done {
		pending = ""
	}
	var out []flowBox
	if o := f.Outbox; o != nil {
		b := flowBox{stage: "outbox", name: o.Conn + " " + o.Table, metrics: pending, err: s.outboxErr, note: s.outboxNote, sub: -1, conn: o.Conn}
		if s.outbox != nil {
			b.metrics, b.warn = s.outbox.Metrics()
		}
		out = append(out, b)
	}
	topic := f.Topic
	if i := strings.LastIndex(topic, ":"); strings.HasPrefix(topic, "arn:") && i >= 0 {
		topic = topic[i+1:]
	}
	if f.Topic != "" {
		sns := flowBox{stage: "sns", name: topic, metrics: pending, err: s.topicErr, note: s.topicNote, sub: -1, conn: f.AWS}
		if s.topic != nil {
			sns.name, sns.metrics, sns.path = s.topic.Name, fmt.Sprintf("구독 %d", len(s.topic.Subs)), "sns/"+s.topic.Arn
		}
		out = append(out, sns)
	}
	var subs []awsres.Sub
	if s.topic != nil {
		subs = s.topic.Subs
	}
	for i, sub := range subs {
		q := sub.Queue
		if q == nil {
			out = append(out, flowBox{stage: "sub", name: sub.Endpoint, metrics: sub.Protocol, sub: i})
			continue
		}
		qb := flowBox{stage: "sqs", name: q.Name, err: q.Err, sub: i, conn: f.AWS, path: "sqs/" + q.Name}
		if q.Visible != "" {
			qb.metrics = fmt.Sprintf("대기 %s · 처리 중 %s · 지연 %s", q.Visible, q.InFlight, q.Delayed)
			qb.short = fmt.Sprintf("대기 %s · 처리 중 %s", q.Visible, q.InFlight)
			if q.Delayed != "0" {
				qb.short = qb.metrics
			}
		}
		out = append(out, qb)
		if q.DLQ != "" {
			// DLQ 는 다른 계정일 수 있어 이름이 아닌 ARN 으로 연다
			out = append(out, flowBox{stage: "dlq", name: q.DLQ, metrics: q.DLQCount, warn: atoiOr0(q.DLQCount) > 0, sub: i, conn: f.AWS, path: "sqs/" + q.DLQArn})
		}
		for _, mp := range q.Lambdas {
			b := flowBox{stage: "lambda", name: mp.Function, metrics: mp.State + " · " + mp.LastResult, short: mp.LastResult, sub: i, conn: f.AWS, path: "lambda/" + mp.Function,
				warn: mp.State != "Enabled" || !lambdaOK(mp.LastResult)}
			if mp.State != "Enabled" || mp.LastResult == "" {
				b.short = strings.TrimSuffix(b.metrics, " · ")
			}
			out = append(out, b)
		}
	}
	if f.Outbox == nil || s.outbox == nil {
		return out
	}
	// 종류 줄(R69.6)은 설정의 종류 ∪ 테이블에 있는 종류다. 한도 소진 많은 순 → 대기 많은 순 → 이름 순이고 행 없는 설정 종류는 맨 뒤다
	o, types := f.Outbox, slices.Clone(s.outbox.Types)
	for _, k := range slices.Sorted(maps.Keys(o.Types)) {
		if !slices.ContainsFunc(types, func(t flows.TypeStat) bool { return !t.Null && t.Name == k }) {
			types = append(types, flows.TypeStat{Name: k, Retrying: min(s.outbox.Retry, 0), Exhausted: min(s.outbox.Exhausted, 0)})
		}
	}
	slices.SortFunc(types, func(a, b flows.TypeStat) int {
		return cmp.Or(cmp.Compare(min(b.Rows, 1), min(a.Rows, 1)), cmp.Compare(b.Exhausted, a.Exhausted), cmp.Compare(b.Waiting, a.Waiting), strings.Compare(a.Label(), b.Label()))
	})
	for _, t := range types {
		m, warn := t.Metrics()
		consumer := "?"
		if !t.Null {
			consumer = cmp.Or(o.Types[t.Name].Consumer, consumer)
		}
		out = append(out, flowBox{stage: stageType, name: t.Label(), metrics: m + " · → " + consumer, short: m + "  → " + consumer, warn: warn, idle: t.Rows == 0,
			nullType: t.Null, sub: -1, conn: o.Conn})
	}
	return alignTypes(out)
}

// alignTypes 는 종류 줄 이름 뒤 빈칸(pad)을 가장 긴 이름에 맞춘다. 종류 줄을 거른 뒤(흐름 탭 높이, 보드 상한)에도 다시 부른다.
func alignTypes(boxes []flowBox) []flowBox {
	w := 0
	for _, b := range boxes {
		if b.stage == stageType {
			w = max(w, ansi.StringWidth(textutil.Printable(b.name, false)))
		}
	}
	for i, b := range boxes {
		if b.stage == stageType {
			boxes[i].pad = w - ansi.StringWidth(textutil.Printable(b.name, false))
		}
	}
	return boxes
}

// ─── 그림 ───

// flowSeg 는 그림 한 줄의 조각이다. box 가 -1 이면 잇는 선이다.
type flowSeg struct {
	text string
	box  int
}

type flowLine []flowSeg

func (l flowLine) width() int {
	w := 0
	for _, s := range l {
		w += ansi.StringWidth(s.text)
	}
	return w
}

var flowStageNames = map[string]string{"outbox": "아웃박스", "sns": "SNS", "sqs": "SQS", "lambda": "λ"}

// boxLabel 은 `[SQS q1 | 대기 0 · 처리 중 2]` 같은 상자 글이다. DLQ 는 상자 없이 `DLQ q1-dlq 4` 다.
func boxLabel(b flowBox) string {
	m := b.metrics
	if b.short != "" {
		m = b.short
	}
	switch {
	case b.err != "":
		m = "오류: " + b.err
	case b.note != "":
		m = b.note
	}
	name := textutil.Printable(b.name, false)
	m = textutil.Printable(m, false)
	switch b.stage {
	case stageType:
		return name + strings.Repeat(" ", max(0, b.pad)) + "  " + m
	case "dlq":
		return strings.TrimSpace("DLQ " + name + " " + m)
	case "lambda":
		return "[λ " + strings.TrimSpace(name+" "+m) + "]"
	case "sub":
		return "[" + m + " " + name + "]"
	case "sns":
		if b.err != "" || b.note != "" {
			return "[SNS " + name + " | " + m + "]"
		}
		return "[SNS " + name + "]"
	}
	if m == "" {
		return "[" + flowStageNames[b.stage] + " " + name + "]"
	}
	return "[" + flowStageNames[b.stage] + " " + name + " | " + m + "]"
}

// flowLayout 은 상자를 왼쪽 → 오른쪽으로 잇고, 가장 긴 줄이 w 를 넘으면 위 → 아래로 쌓는다.
// 아웃박스 종류 줄(R69.6)은 어느 쪽이든 아래에 붙고, 길어도 가로·세로 고르기에는 넣지 않는다(잘려 보인다):
//
//	├─ CREATE_PAYOUT  대기 3 · 재시도 1 · 한도 소진 2  → consume-outbox-create-payout
//	└─ CLIP                대기 0 · 재시도 0 · 한도 소진 0  → consume-outbox-clip
func flowLayout(boxes []flowBox, w int) []flowLine {
	lines := flowWide(boxes)
	if maxWidth(lines) > w {
		lines = flowTall(boxes)
	}
	var at []int
	for i, b := range boxes {
		if b.stage == stageType {
			at = append(at, i)
		}
	}
	for k, i := range at {
		branch := "  ├─ "
		if k == len(at)-1 {
			branch = "  └─ "
		}
		lines = append(lines, flowLine{{branch, -1}, seg(boxes, i)})
	}
	return lines
}

func maxWidth(ls []flowLine) int {
	n := 0
	for _, l := range ls {
		n = max(n, l.width())
	}
	return n
}

// subGroups 는 구독 줄마다의 상자 자리(첫째가 큐·잎)다.
func subGroups(boxes []flowBox) (head []int, subs [][]int) {
	for i, b := range boxes {
		if b.stage == stageType {
			continue
		}
		if b.sub < 0 {
			head = append(head, i)
			continue
		}
		if b.sub >= len(subs) {
			subs = append(subs, nil)
		}
		subs[len(subs)-1] = append(subs[len(subs)-1], i)
	}
	return head, subs
}

func seg(boxes []flowBox, i int) flowSeg { return flowSeg{boxLabel(boxes[i]), i} }

// flowWide 는 가로 그림이다:
//
//	[아웃박스 …] ──▶ [SNS t] ─┬─▶ [SQS q1 | …] ──▶ [λ worker OK]
//	                          │     └─ DLQ q1-dlq 4
//	                          └─▶ [SQS q2 | …]
func flowWide(boxes []flowBox) []flowLine {
	head, subs := subGroups(boxes)
	var first flowLine
	for k, i := range head {
		if k > 0 {
			first = append(first, flowSeg{" ──▶ ", -1})
		}
		first = append(first, seg(boxes, i))
	}
	if len(head) > 0 && boxes[head[0]].stage == "config" {
		return []flowLine{first}
	}
	if len(subs) == 0 {
		return []flowLine{first}
	}
	pad := strings.Repeat(" ", first.width())
	var out []flowLine
	for r, g := range subs {
		last := r == len(subs)-1
		conn, under := "  ├─▶ ", "  │   "
		switch {
		case len(subs) == 1:
			conn, under = " ──▶ ", "     "
		case r == 0:
			conn = " ─┬─▶ "
		case last:
			conn, under = "  └─▶ ", "      "
		}
		var line flowLine
		if r == 0 {
			line = append(slices.Clone(first), flowSeg{conn, -1})
		} else {
			line = flowLine{{pad + conn, -1}}
		}
		line = append(line, seg(boxes, g[0]))
		var dlq []int
		lambdas := 0
		for _, i := range g[1:] {
			if boxes[i].stage == "dlq" {
				dlq = append(dlq, i)
				continue
			}
			sep := " "
			if lambdas == 0 {
				sep = " ──▶ "
			}
			lambdas++
			line = append(line, flowSeg{sep, -1}, seg(boxes, i))
		}
		out = append(out, line)
		for _, i := range dlq {
			out = append(out, flowLine{{pad + under + "└─ ", -1}, seg(boxes, i)})
		}
	}
	return out
}

// flowTall 은 좁을 때의 세로 그림이다:
//
//	[아웃박스 …]
//	  ▼
//	[SNS t]
//	  ├─▶ [SQS q1 | …]
//	  │     ├─ DLQ q1-dlq 4
//	  │     └─▶ [λ worker OK]
//	  └─▶ [SQS q2 | …]
func flowTall(boxes []flowBox) []flowLine {
	head, subs := subGroups(boxes)
	var out []flowLine
	for k, i := range head {
		if k > 0 {
			out = append(out, flowLine{{"  ▼", -1}})
		}
		out = append(out, flowLine{seg(boxes, i)})
	}
	for r, g := range subs {
		conn, under := "  ├─▶ ", "  │     "
		if r == len(subs)-1 {
			conn, under = "  └─▶ ", "        "
		}
		out = append(out, flowLine{{conn, -1}, seg(boxes, g[0])})
		for k, i := range g[1:] {
			branch := "├─"
			if k == len(g)-2 {
				branch = "└─"
			}
			if boxes[i].stage != "dlq" {
				branch += "▶"
			}
			out = append(out, flowLine{{under + branch + " ", -1}, seg(boxes, i)})
		}
	}
	return out
}

// renderFlow 는 그림 줄에 색을 입힌다. 고른 상자는 뒤집고, 경고는 dashWarnStyle, 오류는 errStyle 이다.
func renderFlow(lines []flowLine, boxes []flowBox, sel, w int) []string {
	out := make([]string, len(lines))
	for i, l := range lines {
		var b strings.Builder
		for _, s := range l {
			if s.box < 0 {
				b.WriteString(dimStyle.Render(s.text))
				continue
			}
			st := lipgloss.NewStyle()
			switch bx := boxes[s.box]; {
			case bx.failed():
				st = errStyle
			case bx.warn:
				st = dashWarnStyle
			case bx.note != "" || bx.metrics == "…" || bx.idle:
				st = dimStyle
			}
			if s.box == sel {
				st = st.Inherit(flowSelStyle)
			}
			b.WriteString(st.Render(s.text))
		}
		out[i] = truncate(b.String(), w)
	}
	return out
}

// ─── 탭 ───

// flowState 는 흐름 탭의 상태다. 결과는 흐름 이름으로 들고, 다시 푸는 동안 이전 결과를 그대로 보인다.
type flowState struct {
	list    []flows.Flow // flows.json 다음에 트리에서 연 토픽(adhoc)
	adhoc   []flows.Flow
	fileErr string
	snaps   map[string]flowSnap
	pending map[string]bool
	gen     int
	cancel  context.CancelFunc
	cursor  int
	box     int
	// viewH·diagW 는 마지막으로 그린 그림 자리다. 행 없는 설정 종류를 자리가 있을 때만 보이는 데 쓴다(0 이면 다 보인다)
	viewH, diagW int
}

type flowSnapMsg struct {
	tab, gen int
	name     string
	snap     flowSnap
}

type flowTickMsg struct{ tab, gen int }

func (s *flowState) stop() {
	if s.cancel != nil {
		s.cancel()
	}
}

// openFlows 는 흐름 탭을 열거나 이미 있으면 그 탭으로 옮겨 바로 다시 푼다. adhoc 이 있으면 목록 끝에 더하고 고른다.
func (m *model) openFlows(adhoc *flows.Flow) tea.Cmd {
	var t *queryTab
	for i, x := range m.tabs {
		if x.flow != nil {
			m.switchTab(i)
			t = x
			break
		}
	}
	if t == nil {
		t = m.newTab()
		m.setConn(t, -1)
		t.title = "흐름"
		t.flow = &flowState{snaps: map[string]flowSnap{}, pending: map[string]bool{}}
	}
	m.setFocus(fResult)
	s := t.flow
	if adhoc != nil && !slices.ContainsFunc(s.adhoc, func(f flows.Flow) bool { return f.Name == adhoc.Name }) {
		s.adhoc = append(s.adhoc, *adhoc)
	}
	cmd := m.flowRefresh(t)
	if adhoc != nil {
		s.cursor = slices.IndexFunc(s.list, func(f flows.Flow) bool { return f.Name == adhoc.Name })
		s.box = 0
	}
	return cmd
}

// flowRefresh 는 flows.json 을 다시 읽고 모든 흐름을 동시에 푼다. 진행 중인 이전 회차는 취소한다(대시보드와 같다).
func (m *model) flowRefresh(t *queryTab) tea.Cmd {
	s := t.flow
	s.stop()
	s.gen++
	fs, err := flows.Load(m.proj.Dir, m.proj.Conns)
	s.fileErr = ""
	if err != nil {
		s.fileErr = oneLine(err.Error())
	}
	s.list = append(fs, s.adhoc...)
	s.cursor = clamp(s.cursor, len(s.list))
	ctx, cancel := context.WithCancel(m.life)
	s.cancel = cancel
	id, gen, clients, sources, proj := t.id, s.gen, m.clients, m.sources, m.proj
	cmds := []tea.Cmd{tea.Tick(flowEvery, func(time.Time) tea.Msg { return flowTickMsg{tab: id, gen: gen} })}
	clear(s.pending)
	for _, f := range s.list {
		if f.Err != "" {
			continue
		}
		s.pending[f.Name] = true
		cmds = append(cmds, func() tea.Msg {
			return flowSnapMsg{tab: id, gen: gen, name: f.Name, snap: resolveFlow(ctx, clients, sources, proj, f, true)}
		})
	}
	m.status = "흐름 확인 중…"
	return tea.Batch(cmds...)
}

func (m *model) flowSnapped(msg flowSnapMsg) {
	t := m.findTab(msg.tab)
	if t == nil || t.flow == nil || t.flow.gen != msg.gen {
		return
	}
	t.flow.snaps[msg.name] = msg.snap
	delete(t.flow.pending, msg.name)
	if len(t.flow.pending) == 0 && strings.HasPrefix(m.status, "흐름 확인 중") {
		m.status = "흐름 확인함 " + time.Now().In(textutil.KST).Format("15:04:05")
	}
}

// flowTicked 는 흐름 탭이 활성일 때만 다시 푼다. 다른 탭이면 다음 타이머만 건다.
func (m *model) flowTicked(msg flowTickMsg) tea.Cmd {
	t := m.findTab(msg.tab)
	if t == nil || t.flow == nil || t.flow.gen != msg.gen {
		return nil
	}
	if m.cur() == t {
		return m.flowRefresh(t)
	}
	id, gen := msg.tab, msg.gen
	return tea.Tick(flowEvery, func(time.Time) tea.Msg { return flowTickMsg{tab: id, gen: gen} })
}

// curBoxes 는 고른 흐름의 상자다.
func (s *flowState) curBoxes() (flows.Flow, []flowBox, bool) {
	if len(s.list) == 0 {
		return flows.Flow{}, nil, false
	}
	f := s.list[clamp(s.cursor, len(s.list))]
	snap, done := s.snaps[f.Name]
	boxes := flowBoxes(f, snap, done)
	// 행 없는 설정 종류는 맨 뒤라 빼도 앞 상자 자리가 그대로다. 이름·빈 줄 두 줄을 더해 높이를 넘으면 뺀다
	if s.viewH > 0 && slices.ContainsFunc(boxes, func(b flowBox) bool { return b.idle }) && len(flowLayout(boxes, s.diagW))+2 > s.viewH {
		boxes = alignTypes(slices.DeleteFunc(boxes, func(b flowBox) bool { return b.idle }))
	}
	return f, boxes, true
}

func (m *model) flowKey(k tea.KeyMsg) tea.Cmd {
	t := m.cur()
	s := t.flow
	switch k.String() {
	case "r":
		return m.flowRefresh(t)
	case "up", "k":
		s.cursor, s.box = s.cursor-1, 0
	case "down", "j":
		s.cursor, s.box = s.cursor+1, 0
	case "home", "g":
		s.cursor, s.box = 0, 0
	case "end", "G":
		s.cursor, s.box = len(s.list)-1, 0
	case "left", "h":
		s.box--
	case "right", "l":
		s.box++
	case "enter":
		f, boxes, ok := s.curBoxes()
		if !ok {
			return nil
		}
		return m.openFlowBox(f, boxes[clamp(s.box, len(boxes))])
	}
	s.cursor = clamp(s.cursor, len(s.list))
	_, boxes, _ := s.curBoxes()
	s.box = clamp(s.box, len(boxes))
	return nil
}

// openFlowBox 는 상자의 상세를 새 결과 탭에 연다. 아웃박스는 종류별 수, 종류 줄은 그 종류의 재시도 중·한도 소진 행,
// 큐·DLQ 는 속성, 토픽은 구독, Lambda 는 매핑 표다.
func (m *model) openFlowBox(f flows.Flow, b flowBox) tea.Cmd {
	ci := connByName(m.proj, b.conn)
	outbox := b.stage == "outbox" || b.stage == stageType
	if ci < 0 || (b.path == "" && !outbox) {
		m.status = "열 상세가 없음: " + textutil.Printable(b.name, false)
		return nil
	}
	c := m.conn(ci)
	t := m.newTab()
	p := pendingRun{tab: t.id, conn: ci, readOnly: true, retarget: true}
	if outbox {
		sql, clients := flows.CountSQL(*f.Outbox), m.clients
		if b.stage == stageType {
			sql = flows.RetrySQL(*f.Outbox, flows.TypeStat{Name: b.name, Null: b.nullType})
		}
		p.sql = sql
		p.exec = func(ctx context.Context) (*db.Result, error) {
			pg, err := clients.PG(ctx, c)
			if err != nil {
				return nil, err
			}
			ctx, cancel := context.WithTimeout(ctx, db.ProbeTimeout)
			defer cancel()
			return pg.RunReadOnly(ctx, sql)
		}
	} else {
		path, limit := b.path, openLimit(c.Kind)
		p.exec = m.sourceExec(c, func(ctx context.Context, sb source.Backend) (*db.Result, error) { return sb.Open(ctx, path, limit) })
	}
	return m.guard(p)
}

// flowView 는 대시보드처럼 편집기·결과 자리 전체에 그린다. 왼쪽은 흐름 목록, 오른쪽은 고른 흐름의 그림이다.
func (m *model) flowView(t *queryTab, w, h int) string {
	s := t.flow
	listW := min(28, max(12, w/4))
	if len(s.list) == 0 {
		msg := dimStyle.Render(truncate(flowNoFile, w))
		if s.fileErr != "" {
			msg = errStyle.Render(truncate(textutil.Printable("설정 오류: "+s.fileErr, false), w))
		}
		return msg
	}
	var left []string
	if s.fileErr != "" {
		left = append(left, errStyle.Render(truncate(textutil.Printable("설정 오류: "+s.fileErr, false), listW)))
	}
	// 목록·그림은 높이 h 에서 잘린다. 고른 흐름·상자가 보이게 그 줄이 맨 아래에 오도록 내린다(대시보드 표와 같은 방식)
	cur := clamp(s.cursor, len(s.list))
	top := max(0, cur-max(1, h-len(left))+1)
	for i, f := range s.list[top:] {
		mark := "  "
		if top+i == cur {
			mark = lipgloss.NewStyle().Foreground(colAccent).Bold(true).Render("▸ ")
		}
		icon, st := m.flowIcon(s, f)
		left = append(left, mark+st.Render(icon)+" "+truncate(textutil.Printable(f.Name, false), listW-4))
	}
	diagW := max(10, w-listW-3)
	s.viewH, s.diagW = h, diagW
	var right []string
	if f, boxes, ok := s.curBoxes(); ok {
		right = append(right, lipgloss.NewStyle().Bold(true).Render(truncate(textutil.Printable(f.Name, false), diagW)), "")
		sel, lines := clamp(s.box, len(boxes)), flowLayout(boxes, diagW)
		at := slices.IndexFunc(lines, func(l flowLine) bool {
			return slices.ContainsFunc(l, func(sg flowSeg) bool { return sg.box == sel })
		})
		top := max(0, at-max(1, h-len(right))+1)
		right = append(right, renderFlow(lines[top:], boxes, sel, diagW)...)
	}
	n := min(h, max(len(left), len(right)))
	rows := make([]string, n)
	for i := range n {
		l, r := "", ""
		if i < len(left) {
			l = left[i]
		}
		if i < len(right) {
			r = right[i]
		}
		rows[i] = l + strings.Repeat(" ", max(0, listW-lipgloss.Width(l))) + dimStyle.Render(" │ ") + r
	}
	return strings.Join(rows, "\n")
}

// flowIcon 은 목록 줄의 상태 표시다: `…` 아직 결과 없음, `✕` 설정 오류·실패, `!` 경고, `●` 문제 없음.
func (m *model) flowIcon(s *flowState, f flows.Flow) (string, lipgloss.Style) {
	snap, done := s.snaps[f.Name]
	if f.Err == "" && !done {
		return "…", dimStyle
	}
	warn := false
	for _, b := range flowBoxes(f, snap, done) {
		if b.failed() {
			return "✕", errStyle
		}
		warn = warn || b.warn
	}
	if warn {
		return "!", dashWarnStyle
	}
	return "●", dashOKStyle
}

// ─── 트리 F ───

// treeFlow 는 aws 트리의 SNS 토픽 줄에서 그 토픽만의 흐름을 연다. 트리에서 연 흐름은 아웃박스가 없다.
func (m *model) treeFlow(n *node) (tea.Cmd, bool) {
	if n == nil || n.kind != nSource || m.conn(n.conn).Kind != db.KindAWS {
		return nil, false
	}
	arn, ok := strings.CutPrefix(n.key, "sns/")
	if !ok || strings.Contains(arn, "/") {
		return nil, false
	}
	c := m.conn(n.conn)
	return m.openFlows(&flows.Flow{Name: n.label + " · " + c.Name, AWS: c.Name, Topic: arn}), true
}

// ─── db flows ───

// flowRow 는 `db flows` 의 한 줄(상자 하나)이다.
type flowRow struct {
	Flow    string `json:"flow"`
	Stage   string `json:"stage"`
	Name    string `json:"name"`
	Metrics string `json:"metrics"`
	Warn    bool   `json:"warn"`
	Error   string `json:"error,omitempty"`
	Note    string `json:"note,omitempty"`
}

// runFlows 는 흐름(-f 로 고른 것, 없으면 모두)을 동시에 한 번 풀어 상자마다 한 줄을 찍는다.
// prod 접속은 --prod 가 없으면 묻지 않고 `--prod 로 확인` 이다(R58.2). 설정 오류·실패가 하나라도 있으면 1 이다.
func runFlows(ctx context.Context, o cliOpts, proj *db.Project, clients *db.Clients, stdout, stderr io.Writer) int {
	if o.format == "csv" {
		fmt.Fprintln(stderr, "db flows: --format must be one of table|tsv|json")
		return exitUsage
	}
	all, err := flows.Load(proj.Dir, proj.Conns)
	if err != nil {
		fmt.Fprintln(stderr, textutil.Printable(err.Error(), true))
		return exitErr
	}
	list := all
	if names := append(append([]string{}, o.conns...), o.args...); len(names) > 0 {
		list = nil
		for _, n := range names {
			i := slices.IndexFunc(all, func(f flows.Flow) bool { return f.Name == n })
			if i < 0 {
				fmt.Fprintf(stderr, "db flows: 흐름 없음: %s (.dbtui/flows.json)\n", n)
				return exitUsage
			}
			list = append(list, all[i])
		}
	}
	if len(all) == 0 {
		fmt.Fprintln(stderr, "db flows: 흐름 없음 (.dbtui/flows.json)")
	}
	sources := newSourcePool()
	defer sources.close()
	snaps := make([]flowSnap, len(list))
	var wg sync.WaitGroup
	for i, f := range list {
		wg.Add(1)
		go func() {
			defer wg.Done()
			snaps[i] = resolveFlow(ctx, clients, sources, proj, f, o.prod)
		}()
	}
	wg.Wait()

	rows := []flowRow{}
	failed := false
	for i, f := range list {
		for _, b := range flowBoxes(f, snaps[i], true) {
			r := flowRow{Flow: f.Name, Stage: b.stage, Name: b.name, Metrics: b.metrics, Warn: b.warn, Note: b.note}
			if b.failed() {
				failed, r.Error = true, cliErrText(ctx, proj, o, errors.New(b.err), b.conn)
			}
			rows = append(rows, r)
		}
	}
	if o.format == "json" {
		enc := json.NewEncoder(stdout)
		enc.SetEscapeHTML(false)
		err = enc.Encode(rows)
	} else {
		res := &db.Result{Cols: []string{"flow", "stage", "name", "metrics", "warn"}}
		for _, r := range rows {
			m := r.Metrics
			if r.Error != "" {
				m = r.Error
			} else if r.Note != "" {
				m = r.Note
			}
			res.Rows = append(res.Rows, []string{r.Flow, r.Stage, r.Name, m, strconv.FormatBool(r.Warn)})
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
