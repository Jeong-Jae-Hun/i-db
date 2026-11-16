package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/x/ansi"

	"github.com/Jeong-Jae-Hun/i-db/internal/awsres"
	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/flows"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
)

// ─── R69 이벤트 흐름 ───

const flowsJSON = `[
	{"name":"hub","outbox":{"conn":"shop-stg","table":"public.t_outbox","type":"event_type","retry":"retry_count","due":"check_needed_at"},"aws":"aws-stg","topic":"stg-hub",
		"queues":[{"name":"q1","dlq":"q1-dlq"},{"name":"q2"}]},
	{"name":"bad","aws":"shop-stg","queues":[{"name":"x"}]}
]`

const (
	q1Arn  = "arn:aws:sqs:ap-northeast-2:111:q1"
	dlqArn = "arn:aws:sqs:ap-northeast-2:999:q1-dlq"
)

// sampleQueues 는 hubFlow 의 큐 둘을 푼 결과다. 첫째는 DLQ·Lambda 하나, 둘째는 매핑이 없다. 수치는 없다(R72).
func sampleQueues() []awsres.Queue {
	return []awsres.Queue{
		{Name: "q1", Arn: q1Arn, DLQ: "q1-dlq", DLQArn: dlqArn, Lambdas: []awsres.Mapping{{Function: "worker", State: "Enabled", LastResult: "OK"}}},
		{Name: "q2", Arn: "arn:aws:sqs:ap-northeast-2:111:q2"},
	}
}

// fakeFlow 는 아웃박스·큐 풀이를 바꿔 끼우고 부른 횟수를 센다. 타이머는 1ms 다.
func fakeFlow(t *testing.T, stat flows.Stat, queues []awsres.Queue, awsErr error) (*atomic.Int32, *atomic.Int32) {
	t.Helper()
	var no, nq atomic.Int32
	oldO, oldQ, oldE := flowOutbox, flowQueues, flowEvery
	flowOutbox = func(context.Context, *db.Clients, db.Conn, flows.Outbox) (flows.Stat, error) {
		no.Add(1)
		return stat, nil
	}
	flowQueues = func(context.Context, *sourcePool, db.Conn, []awsres.QueueRef) ([]awsres.Queue, error) {
		nq.Add(1)
		return queues, awsErr
	}
	flowEvery = time.Millisecond
	t.Cleanup(func() { flowOutbox, flowQueues, flowEvery = oldO, oldQ, oldE })
	return &no, &nq
}

func writeFlowsFile(t *testing.T, dir, body string) {
	t.Helper()
	must(t, os.MkdirAll(filepath.Join(dir, ".dbtui"), 0o755))
	must(t, os.WriteFile(filepath.Join(dir, ".dbtui", "flows.json"), []byte(body), 0o600))
}

func flowModel(t *testing.T) *model {
	t.Helper()
	p := &db.Project{Dir: t.TempDir(), Mapping: map[string]string{}, Conns: []db.Conn{
		{Name: "shop-stg", UUID: "pg", Kind: db.KindPG, Group: "shop", Host: "127.0.0.1", Port: 1},
		{Name: "aws-stg", UUID: "aw", Kind: db.KindAWS, Group: "shop", Region: "ap-northeast-2", Source: db.SourceDB},
	}}
	writeFlowsFile(t, p.Dir, flowsJSON)
	m := newModel(p)
	m.Update(tea.WindowSizeMsg{Width: 200, Height: 40})
	t.Cleanup(func() { m.stop(); m.sources.close() })
	return m
}

// feedFlow 는 타이머 메시지를 빼고 넣는다.
func feedFlow(m *model, cmd tea.Cmd) {
	for _, msg := range collect(cmd) {
		if _, ok := msg.(flowTickMsg); ok {
			continue
		}
		if _, ok := msg.(dashTickMsg); ok {
			continue
		}
		m.Update(msg)
	}
}

func flowTabs(m *model) int {
	n := 0
	for _, t := range m.tabs {
		if t.flow != nil {
			n++
		}
	}
	return n
}

func plain(lines []flowLine) string {
	out := make([]string, len(lines))
	for i, l := range lines {
		var b strings.Builder
		for _, s := range l {
			b.WriteString(s.text)
		}
		out[i] = b.String()
	}
	return strings.Join(out, "\n")
}

func hubFlow() flows.Flow {
	return flows.Flow{Name: "hub", AWS: "aws-stg", Topic: "stg-hub", Queues: []flows.Queue{{Name: "q1", DLQ: "q1-dlq"}, {Name: "q2"}},
		Outbox: &flows.Outbox{Conn: "shop-stg", Table: "public.t_outbox", Type: "event_type", Retry: "retry_count", Due: "check_needed_at"}}
}

// ─── 상자 ───

func TestFlowBoxes_아웃박스_SNS_큐_DLQ_Lambda_순서이고_SNS_큐_DLQ_는_수치가_없다(t *testing.T) {
	st := flows.Stat{Pending: 120, Retry: 3, Stale: 1}
	boxes := flowBoxes(hubFlow(), flowSnap{outbox: &st, queues: sampleQueues()}, true)
	var got []string
	for _, b := range boxes {
		w := ""
		if b.warn {
			w = "!"
		}
		got = append(got, b.stage+":"+b.name+":"+b.metrics+w)
	}
	want := []string{
		"outbox:shop-stg public.t_outbox:대기 120 · 재시도 3 · 한도 소진 0 · 오래됨 1!",
		"sns:stg-hub:",
		"sqs:q1:",
		"dlq:q1-dlq:",
		"lambda:worker:Enabled · OK",
		"sqs:q2:",
	}
	if strings.Join(got, "\n") != strings.Join(want, "\n") {
		t.Fatalf("상자:\n%s", strings.Join(got, "\n"))
	}
	if boxes[2].path != "sqs/"+q1Arn || boxes[4].path != "lambda/worker" || boxes[1].path != "" {
		t.Fatalf("열 경로: %+v", boxes)
	}
	if boxes[3].path != "sqs/"+dlqArn {
		t.Fatalf("DLQ 는 다른 계정일 수 있어 ARN 으로 연다: %q", boxes[3].path)
	}
	// 토픽이 ARN 이면 이름은 끝 마디이고 구독 표(요금 허용 때만 열림)를 연다
	f := hubFlow()
	f.Topic = "arn:aws:sns:ap-northeast-2:111:stg-hub"
	if b := flowBoxes(f, flowSnap{}, true)[1]; b.name != "stg-hub" || b.path != "sns/"+f.Topic || b.conn != "aws-stg" {
		t.Fatalf("ARN 토픽: %+v", b)
	}
}

func TestFlowBoxes_큐_상자는_결과_전에도_설정_순서대로_이름을_보이고_계정_오류는_큐마다_붙는다(t *testing.T) {
	wait := flowBoxes(hubFlow(), flowSnap{}, false)
	var got []string
	for _, b := range wait {
		got = append(got, b.stage+":"+b.name+":"+b.metrics)
	}
	if want := "outbox:shop-stg public.t_outbox:…\nsns:stg-hub:\nsqs:q1:…\ndlq:q1-dlq:\nsqs:q2:…"; strings.Join(got, "\n") != want {
		t.Fatalf("결과 전:\n%s", strings.Join(got, "\n"))
	}
	if wait[2].path != "sqs/q1" || wait[3].path != "sqs/q1-dlq" {
		t.Fatalf("풀기 전 경로는 설정 이름: %q %q", wait[2].path, wait[3].path)
	}
	bad := flowBoxes(hubFlow(), flowSnap{awsErr: "aws sts:GetCallerIdentity: ExpiredToken"}, true)
	if !bad[2].failed() || !bad[4].failed() || bad[1].failed() || boxLabel(bad[2]) != "[SQS q1 | 오류: aws sts:GetCallerIdentity: ExpiredToken]" {
		t.Fatalf("계정 오류: %+v", bad)
	}
}

func TestFlowBoxes_Lambda_는_꺼졌거나_문제면_경고이고_처리할_것_없음은_아니다(t *testing.T) {
	for _, c := range []struct {
		state, last string
		warn        bool
	}{
		{"Enabled", "OK", false},
		{"Enabled", "No records processed", false},
		{"Disabled", "OK", true},
		{"Enabled", "PROBLEM: Function call failed", true},
	} {
		qs := []awsres.Queue{{Name: "q", Lambdas: []awsres.Mapping{{Function: "f", State: c.state, LastResult: c.last}}}}
		boxes := flowBoxes(flows.Flow{Name: "x", AWS: "a", Queues: []flows.Queue{{Name: "q"}}}, flowSnap{queues: qs}, true)
		if b := boxes[len(boxes)-1]; b.stage != "lambda" || b.warn != c.warn {
			t.Errorf("%s %s: %+v", c.state, c.last, b)
		}
	}
}

func TestFlowBoxes_설정_오류와_아직_결과_없음과_prod_미확인(t *testing.T) {
	bad := flowBoxes(flows.Flow{Name: "bad", Err: "topic is empty"}, flowSnap{}, true)
	if len(bad) != 1 || bad[0].err != "설정 오류: topic is empty" || !bad[0].failed() {
		t.Fatalf("설정 오류: %+v", bad)
	}
	prod := flowBoxes(hubFlow(), flowSnap{outboxNote: dashProdHint, awsNote: dashProdHint}, true)
	if prod[0].note != dashProdHint || prod[0].failed() || boxLabel(prod[1]) != "[SNS stg-hub]" || boxLabel(prod[2]) != "[SQS q1 | --prod 로 확인]" {
		t.Fatalf("prod: %+v", prod)
	}
}

// ─── 그림 ───

func TestFlowLayout_넓으면_왼쪽에서_오른쪽으로_DLQ_는_큐_아래_가지다(t *testing.T) {
	st := flows.Stat{Pending: 120, Retry: 3, Stale: 1}
	boxes := flowBoxes(hubFlow(), flowSnap{outbox: &st, queues: sampleQueues()}, true)
	head := "[아웃박스 shop-stg public.t_outbox | 대기 120 · 재시도 3 · 한도 소진 0 · 오래됨 1] ──▶ [SNS stg-hub]"
	pad := strings.Repeat(" ", ansi.StringWidth(head))
	want := strings.Join([]string{
		head + " ─┬─▶ [SQS q1] ──▶ [λ worker OK]",
		pad + "  │   └─ DLQ q1-dlq",
		pad + "  └─▶ [SQS q2]",
	}, "\n")
	if got := plain(flowLayout(boxes, 200)); got != want {
		t.Fatalf("가로:\n%s\n\nwant:\n%s", got, want)
	}
	// 큐가 하나면 갈래 없이 잇는다
	one := flowBoxes(flows.Flow{Name: "x", AWS: "a", Topic: "t", Queues: []flows.Queue{{Name: "q1", DLQ: "q1-dlq"}}}, flowSnap{queues: sampleQueues()[:1]}, true)
	want = "[SNS t] ──▶ [SQS q1] ──▶ [λ worker OK]\n" + strings.Repeat(" ", len("[SNS t]")) + "     └─ DLQ q1-dlq"
	if got := plain(flowLayout(one, 200)); got != want {
		t.Fatalf("큐 하나:\n%s", got)
	}
}

func TestFlowLayout_아웃박스_토픽_없이_큐만_있으면_큐가_줄_맨_앞이다(t *testing.T) {
	f := flows.Flow{Name: "x", AWS: "a", Queues: []flows.Queue{{Name: "q1", DLQ: "q1-dlq"}, {Name: "q2"}}}
	want := "[SQS q1] ──▶ [λ worker OK]\n  └─ DLQ q1-dlq\n[SQS q2]"
	if got := plain(flowLayout(flowBoxes(f, flowSnap{queues: sampleQueues()}, true), 200)); got != want {
		t.Fatalf("큐만:\n%s", got)
	}
}

func TestFlowLayout_좁으면_위에서_아래로_쌓는다(t *testing.T) {
	st := flows.Stat{Pending: 120, Retry: 3, Stale: 1}
	boxes := flowBoxes(hubFlow(), flowSnap{outbox: &st, queues: sampleQueues()}, true)
	want := strings.Join([]string{
		"[아웃박스 shop-stg public.t_outbox | 대기 120 · 재시도 3 · 한도 소진 0 · 오래됨 1]",
		"  ▼",
		"[SNS stg-hub]",
		"  ├─▶ [SQS q1]",
		"  │     ├─ DLQ q1-dlq",
		"  │     └─▶ [λ worker OK]",
		"  └─▶ [SQS q2]",
	}, "\n")
	if got := plain(flowLayout(boxes, 80)); got != want {
		t.Fatalf("세로:\n%s", got)
	}
	for _, l := range renderFlow(flowLayout(boxes, 40), boxes, 0, 40) {
		if ansi.StringWidth(l) > 40 {
			t.Fatalf("폭을 넘음: %q", ansi.Strip(l))
		}
	}
}

// ─── 탭 ───

func TestFlowView_낮으면_고른_흐름과_고른_상자가_보이게_내린다(t *testing.T) {
	m := flowModel(t)
	st := flows.Stat{Pending: 120, Retry: 3, Stale: 1}
	s := &flowState{snaps: map[string]flowSnap{"hub": {outbox: &st, queues: sampleQueues()}}, cursor: 7, box: 5}
	for i := range 7 {
		s.list = append(s.list, flows.Flow{Name: fmt.Sprintf("f%d", i), Err: "x"})
	}
	s.list = append(s.list, hubFlow())
	tab := &queryTab{flow: s}
	for _, w := range []int{60, 400} { // 세로로 쌓는 그림·가로 그림
		v := ansi.Strip(m.flowView(tab, w, 5))
		if n := strings.Count(v, "\n") + 1; n != 5 || !strings.Contains(v, "▸ ● hub") && !strings.Contains(v, "▸ ! hub") {
			t.Fatalf("w=%d 고른 흐름이 보인다 (%d줄):\n%s", w, n, v)
		}
		if !strings.Contains(v, "[SQS q2]") {
			t.Fatalf("w=%d 고른 상자(둘째 큐)가 보인다:\n%s", w, v)
		}
	}
}

func TestFlow_Alt_F_와_팔레트는_흐름_탭을_하나만_열고_설정_오류를_보인다(t *testing.T) {
	no, nt := fakeFlow(t, flows.Stat{Pending: 120, Retry: 3, Stale: 1}, sampleQueues(), nil)
	m := flowModel(t)
	_, cmd := m.Update(tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("f"), Alt: true})
	if flowTabs(m) != 1 || m.cur().flow == nil || m.cur().title != "흐름" || m.focus != fResult {
		t.Fatalf("흐름 탭: tabs=%d focus=%v", flowTabs(m), m.focus)
	}
	if v := ansi.Strip(m.View()); !strings.Contains(v, "확인 중 1") || !strings.Contains(v, "… hub") {
		t.Fatalf("확인 중:\n%s", v)
	}
	feedFlow(m, cmd)
	if no.Load() != 1 || nt.Load() != 1 {
		t.Fatalf("설정 오류 흐름은 풀지 않는다: %d %d", no.Load(), nt.Load())
	}
	v := ansi.Strip(m.View())
	for _, want := range []string{"! hub", "✕ bad", "[아웃박스 shop-stg public.t_outbox | 대기 120 · 재시도 3 · 한도 소진 0 · 오래됨 1]", "DLQ q1-dlq", "hub  " + flowNoMetrics} {
		if !strings.Contains(v, want) {
			t.Fatalf("%q 가 없다:\n%s", want, v)
		}
	}
	m.Update(key("down"))
	if v := ansi.Strip(m.View()); !strings.Contains(v, "설정 오류: aws shop-stg: want kind aws, got postgresql") {
		t.Fatalf("설정 오류:\n%s", v)
	}
	m.switchTab(0)
	palette(m, "흐름 (아웃박스")
	if flowTabs(m) != 1 || m.cur().flow == nil {
		t.Fatalf("팔레트는 같은 탭으로 간다: %d", flowTabs(m))
	}
}

func TestFlow_flows_json_이_깨졌거나_없어도_탭을_연다(t *testing.T) {
	fakeFlow(t, flows.Stat{}, sampleQueues(), nil)
	m := flowModel(t)
	writeFlowsFile(t, m.proj.Dir, `[{`)
	m.openFlows(nil)
	if v := ansi.Strip(m.View()); !strings.Contains(v, "설정 오류: .dbtui/flows.json") {
		t.Fatalf("깨진 파일:\n%s", v)
	}
	must(t, os.Remove(filepath.Join(m.proj.Dir, ".dbtui", "flows.json")))
	m.Update(key("r"))
	if v := ansi.Strip(m.View()); !strings.Contains(v, "흐름 없음") {
		t.Fatalf("없는 파일:\n%s", v)
	}
}

func TestFlow_닫으면_진행_중인_풀이를_취소하고_늦은_결과는_버린다(t *testing.T) {
	fakeFlow(t, flows.Stat{}, sampleQueues(), nil)
	var seen atomic.Pointer[context.Context]
	flowQueues = func(ctx context.Context, _ *sourcePool, _ db.Conn, _ []awsres.QueueRef) ([]awsres.Queue, error) {
		seen.Store(&ctx)
		<-ctx.Done()
		return nil, ctx.Err()
	}
	m := flowModel(t)
	cmd := m.openFlows(nil)
	done := make(chan struct{})
	go func() { collect(cmd); close(done) }()
	for seen.Load() == nil {
		time.Sleep(time.Millisecond)
	}
	t0 := m.cur()
	m.closeTab(t0)
	select {
	case <-done:
	case <-time.After(5 * time.Second):
		t.Fatal("탭을 닫으면 풀이가 취소돼야 한다")
	}
	if flowTabs(m) != 0 {
		t.Fatal("흐름 탭이 남았다")
	}
}

func TestFlow_Enter_는_상자의_상세를_새_결과_탭에_연다(t *testing.T) {
	fakeFlow(t, flows.Stat{Pending: 1, Retry: 0, Stale: 0}, sampleQueues(), nil)
	f := newFake()
	registerFake(t, f)
	m := flowModel(t)
	feedFlow(m, m.openFlows(nil))
	flowTab := m.cur()
	// 오른쪽으로 두 칸 가면 첫 큐다
	m.Update(key("right"))
	m.Update(key("right"))
	_, cmd := m.Update(key("enter"))
	if m.cur() == flowTab || m.cur().conn != 1 {
		t.Fatalf("새 탭에 aws 접속으로 연다: conn=%d", m.cur().conn)
	}
	drive(m, cmd)
	if _, opens, _ := f.snapshot(); len(opens) != 1 || opens[0] != "sqs/"+q1Arn {
		t.Fatalf("큐 Open: %v", opens)
	}
	// 아웃박스 상자는 종류별 수 질의를 READ ONLY 로 보낸다
	m.switchTab(0)
	for i, tb := range m.tabs {
		if tb == flowTab {
			m.switchTab(i)
		}
	}
	m.Update(key("home"))
	m.Update(key("left"))
	m.Update(key("left"))
	n := len(m.tabs)
	m.Update(key("enter"))
	if len(m.tabs) != n+1 || m.cur().conn != 0 || m.cur().running == nil || !m.cur().runningReadOnly {
		t.Fatalf("아웃박스 상세: tabs=%d conn=%d running=%v", len(m.tabs), m.cur().conn, m.cur().running != nil)
	}
}

func TestFlow_트리의_SNS_토픽에서_F_는_그_토픽_이름만의_흐름을_열고_AWS_를_묻지_않는다(t *testing.T) {
	_, nq := fakeFlow(t, flows.Stat{}, sampleQueues(), nil)
	f := newFake()
	arn := "arn:aws:sns:ap-northeast-2:1:orders"
	f.children[""] = []source.Node{{Name: "orders", Path: "sns/" + arn, HasChildren: true, Openable: true},
		{Name: "q", Path: "sqs/q", Openable: true}}
	registerFake(t, f)
	m := flowModel(t)
	m.setFocus(fTree)
	cn := expandConn(t, m, 1)
	m.selectNode(childNamed(cn, "q"))
	m.Update(key("F"))
	if flowTabs(m) != 0 {
		t.Fatal("큐 줄의 F 는 흐름이 아니다")
	}
	m.setFocus(fTree)
	m.selectNode(childNamed(cn, "orders"))
	runs0, opens0, _ := f.snapshot()
	_, cmd := m.Update(key("F"))
	if flowTabs(m) != 1 {
		t.Fatal("토픽 줄 F 는 흐름 탭을 연다")
	}
	feedFlow(m, cmd)
	s := m.cur().flow
	if len(s.list) != 3 || s.list[s.cursor].Name != "orders · aws-stg" {
		t.Fatalf("트리 흐름: %+v cursor=%d", s.list, s.cursor)
	}
	// hub 만 큐를 푼다. 트리에서 연 흐름은 토픽 이름뿐이라 AWS 에 아무것도 묻지 않는다(R72)
	if runs, opens, _ := f.snapshot(); nq.Load() != 1 || len(runs) != len(runs0) || len(opens) != len(opens0) {
		t.Fatalf("큐 풀이 %d, 실행 %d → %d, 열기 %d → %d", nq.Load(), len(runs0), len(runs), len(opens0), len(opens))
	}
	if v := ansi.Strip(m.View()); !strings.Contains(v, "[SNS orders]") {
		t.Fatalf("토픽 상자:\n%s", v)
	}
}

// ─── 대시보드 카드 ───

func TestDash_아웃박스_한도_소진_카드를_더하고_SQS_기반_DLQ_카드는_없다(t *testing.T) {
	fakeFlow(t, flows.Stat{Pending: 9, Retry: 3, Stale: -1, Exhausted: 2}, nil, nil)
	fakeProbe(t, func(c db.Conn) db.Health {
		if c.Kind == db.KindAWS {
			return db.Health{OK: true, Metrics: []db.Metric{{Name: "Lambda", Value: "86"}, {Name: "매핑 꺼짐", Value: "2", Warn: true}, {Name: "실패", Value: "1", Warn: true}}}
		}
		return healthy(c)
	})
	m := flowModel(t)
	m.proj.Conns = append(m.proj.Conns, db.Conn{Name: "aws-prod", UUID: "ap", Kind: db.KindAWS, Region: "ap-northeast-2", Source: db.SourceDB})
	_, cmd := m.Update(tea.KeyMsg{Type: tea.KeyCtrlG})
	if len(m.cur().dash.pending) != 4 {
		t.Fatalf("접속 셋과 아웃박스 하나를 확인한다: %v", m.cur().dash.pending)
	}
	feedFlow(m, cmd)
	s := m.dashSum(m.cur().dash)
	if s.outboxes != 1 || s.exhausted != 2 {
		t.Fatalf("요약: %+v", s)
	}
	v := ansi.Strip(m.View())
	if strings.Contains(v, "DLQ 쌓인 큐") || !strings.Contains(v, "아웃박스 한도 소진") || !strings.Contains(v, "Lambda 86 · 매핑 꺼짐 2 · 실패 1") {
		t.Fatalf("카드·수치:\n%s", v)
	}
	// 흐름이 없으면 아웃박스 카드도 없다
	must(t, os.Remove(filepath.Join(m.proj.Dir, ".dbtui", "flows.json")))
	feedFlow(m, m.dashRefresh(m.cur()))
	if s := m.dashSum(m.cur().dash); s.outboxes != 0 || strings.Contains(ansi.Strip(m.View()), "아웃박스 한도 소진") {
		t.Fatalf("아웃박스 카드가 남았다: %+v", s)
	}
}

func TestDash_아웃박스를_못_읽으면_한도_소진_합에서_뺀다(t *testing.T) {
	fakeFlow(t, flows.Stat{}, nil, nil)
	flowOutbox = func(context.Context, *db.Clients, db.Conn, flows.Outbox) (flows.Stat, error) {
		return flows.Stat{}, errors.New("timeout")
	}
	fakeProbe(t, healthy)
	m := flowModel(t)
	_, cmd := m.Update(tea.KeyMsg{Type: tea.KeyCtrlG})
	feedFlow(m, cmd)
	if s := m.dashSum(m.cur().dash); s.outboxes != 1 || s.exhausted != 0 || len(m.cur().dash.pending) != 0 {
		t.Fatalf("요약: %+v pending=%v", s, m.cur().dash.pending)
	}
}

// ─── db flows ───

func flowCLIProject(t *testing.T) string {
	t.Helper()
	dir := fakeCLIProject(t)
	_, err := db.AddConn(dir, db.Conn{Name: "aws-stg", Kind: db.KindAWS, Region: "ap-northeast-2"})
	must(t, err)
	_, err = db.AddConn(dir, db.Conn{Name: "aws-prod", Kind: db.KindAWS, Region: "ap-northeast-2"})
	must(t, err)
	writeFlowsFile(t, dir, `[
		{"name":"hub","outbox":{"conn":"app-stg","table":"t_outbox","type":"event_type","retry":"retry_count"},"aws":"aws-stg","topic":"stg-hub","queues":[{"name":"q1","dlq":"q1-dlq"},{"name":"q2"}]},
		{"name":"live","outbox":{"conn":"app-prod","table":"t_outbox","type":"event_type"},"aws":"aws-prod","topic":"prod-hub","queues":[{"name":"pq"}]}
	]`)
	return dir
}

func TestFlowCLI_json_은_상자마다_한_줄이고_prod_는_묻지_않는다(t *testing.T) {
	no, nt := fakeFlow(t, flows.Stat{Pending: 5, Retry: 1, Stale: -1}, sampleQueues(), nil)
	dir := flowCLIProject(t)
	code, out, errw := runCLITest(t, "", "flows", "-p", dir, "--format", "json")
	if code != exitOK {
		t.Fatalf("%d %q", code, errw)
	}
	var rows []flowRow
	must(t, json.Unmarshal([]byte(out), &rows))
	if len(rows) != 9 || no.Load() != 1 || nt.Load() != 1 {
		t.Fatalf("줄 %d, 아웃박스 %d, 큐 %d: %s", len(rows), no.Load(), nt.Load(), out)
	}
	if r := rows[0]; r.Flow != "hub" || r.Stage != "outbox" || r.Metrics != "대기 5 · 재시도 1 · 한도 소진 0" || !r.Warn {
		t.Fatalf("아웃박스 줄: %+v", r)
	}
	// SNS·큐·DLQ 줄은 수치가 비어 있다(요금이 붙어 묻지 않음, R72)
	for i, stage := range []string{"sns", "sqs", "dlq"} {
		if r := rows[i+1]; r.Stage != stage || r.Metrics != "" || r.Warn {
			t.Fatalf("%s 줄: %+v", stage, r)
		}
	}
	if r := rows[4]; r.Stage != "lambda" || r.Name != "worker" || r.Metrics != "Enabled · OK" {
		t.Fatalf("Lambda 줄: %+v", r)
	}
	if r := rows[6]; r.Flow != "live" || r.Stage != "outbox" || r.Note != dashProdHint || r.Error != "" {
		t.Fatalf("prod 줄: %+v", r)
	}
	if r := rows[8]; r.Flow != "live" || r.Stage != "sqs" || r.Name != "pq" || r.Note != dashProdHint {
		t.Fatalf("prod 큐 줄: %+v", r)
	}
	// -f 로 고르고 --prod 면 prod 도 묻는다
	code, out, _ = runCLITest(t, "", "flows", "-p", dir, "-f", "live", "--prod", "--format", "tsv")
	if code != exitOK || !strings.HasPrefix(out, "flow\tstage\tname\tmetrics\twarn\nlive\toutbox\tapp-prod t_outbox\t대기 5 · 재시도 1 · 한도 소진 0\ttrue\n") {
		t.Fatalf("--prod: %d %q", code, out)
	}
}

func TestFlowCLI_실패와_설정_오류는_1_이고_없는_흐름은_2다(t *testing.T) {
	fakeFlow(t, flows.Stat{}, nil, errors.New("aws sts:GetCallerIdentity: ExpiredToken: expired"))
	dir := flowCLIProject(t)
	code, out, _ := runCLITest(t, "", "flows", "-p", dir, "-f", "hub", "--format", "json")
	if code != exitErr || !strings.Contains(out, `"error":"aws sts:GetCallerIdentity: ExpiredToken: expired"`) {
		t.Fatalf("실패: %d %s", code, out)
	}
	fakeFlow(t, flows.Stat{}, sampleQueues(), nil)
	writeFlowsFile(t, dir, `[{"name":"bad","aws":"app-stg","queues":[{"name":"x"}]}]`)
	code, out, _ = runCLITest(t, "", "flows", "-p", dir, "--format", "json")
	if code != exitErr || !strings.Contains(out, `"stage":"config"`) || !strings.Contains(out, "설정 오류: aws app-stg: want kind aws, got postgresql") {
		t.Fatalf("설정 오류: %d %s", code, out)
	}
	if code, _, errw := runCLITest(t, "", "flows", "-p", dir, "-f", "nope"); code != exitUsage || !strings.Contains(errw, "흐름 없음: nope") {
		t.Fatalf("없는 흐름: %d %q", code, errw)
	}
	if code, _, _ := runCLITest(t, "", "flows", "-p", dir, "--format", "csv"); code != exitUsage {
		t.Fatalf("csv: %d", code)
	}
}

// ─── 아웃박스 종류 (R69.6) ───

func intp(n int) *int { return &n }

// typeFlow 는 토픽 없는 아웃박스 흐름이다. EARN 은 한도 20·소비자, CLIP 은 소비자만, IDLE 은 설정에만 있는 종류다.
func typeFlow() flows.Flow {
	return flows.Flow{Name: "shop-outbox-stg", Outbox: &flows.Outbox{Conn: "shop-stg", Table: "public.t_outbox", Type: "event_type", Retry: "retry_count", Due: "check_needed_at",
		Types: map[string]flows.TypeConf{
			"EARN": {Limit: intp(20), Consumer: "consume-outbox-earn"},
			"CLIP": {Consumer: "consume-outbox-clip"},
			"IDLE": {Consumer: "consume-outbox-idle"},
		}}}
}

func typeStat() flows.Stat {
	return flows.Stat{Pending: 9, Retry: 2, Stale: 1, Exhausted: 3, Types: []flows.TypeStat{
		{Name: "CLIP", Rows: 4, Waiting: 4, Retrying: 1, Exhausted: 0, Stale: 1},
		{Name: "LONG_UNKNOWN", Rows: 2, Waiting: 2, Retrying: 0, Exhausted: 0},
		{Name: "EARN", Rows: 6, Waiting: 3, Retrying: 1, Exhausted: 3},
	}}
}

func TestFlowBoxes_아웃박스_종류_줄은_한도_소진_대기_이름_순이고_행_없는_설정_종류는_맨_뒤다(t *testing.T) {
	st := typeStat()
	boxes := flowBoxes(typeFlow(), flowSnap{outbox: &st}, true)
	var got []string
	for _, b := range boxes {
		w := ""
		if b.warn {
			w = "!"
		}
		if b.idle {
			w += "(idle)"
		}
		got = append(got, b.stage+":"+b.name+":"+b.metrics+w)
	}
	want := []string{
		"outbox:shop-stg public.t_outbox:대기 9 · 재시도 2 · 한도 소진 3 · 오래됨 1!",
		"outbox-type:EARN:대기 3 · 재시도 1 · 한도 소진 3 · → consume-outbox-earn!",
		"outbox-type:CLIP:대기 4 · 재시도 1 · 한도 소진 0 · 오래됨 1 · → consume-outbox-clip!",
		"outbox-type:LONG_UNKNOWN:대기 2 · 재시도 0 · 한도 소진 0 · → ?",
		"outbox-type:IDLE:대기 0 · 재시도 0 · 한도 소진 0 · → consume-outbox-idle(idle)",
	}
	if strings.Join(got, "\n") != strings.Join(want, "\n") {
		t.Fatalf("상자:\n%s", strings.Join(got, "\n"))
	}
	// 토픽 없는 흐름은 SNS 상자 없이 아웃박스와 종류 줄만 그린다. 종류 이름 뒤를 맞춰 수치 열이 가지런하다
	wantPlain := strings.Join([]string{
		"[아웃박스 shop-stg public.t_outbox | 대기 9 · 재시도 2 · 한도 소진 3 · 오래됨 1]",
		"  ├─ EARN          대기 3 · 재시도 1 · 한도 소진 3  → consume-outbox-earn",
		"  ├─ CLIP          대기 4 · 재시도 1 · 한도 소진 0 · 오래됨 1  → consume-outbox-clip",
		"  ├─ LONG_UNKNOWN  대기 2 · 재시도 0 · 한도 소진 0  → ?",
		"  └─ IDLE          대기 0 · 재시도 0 · 한도 소진 0  → consume-outbox-idle",
	}, "\n")
	for _, w := range []int{200, 30} {
		if got := plain(flowLayout(boxes, w)); got != wantPlain {
			t.Fatalf("w=%d 그림:\n%s", w, got)
		}
	}
	// 토픽이 있는 흐름은 토픽 그림 아래에 종류 줄을 붙인다
	f := typeFlow()
	f.Topic = "stg-hub"
	withTopic := plain(flowLayout(flowBoxes(f, flowSnap{outbox: &st}, true), 400))
	if lines := strings.Split(withTopic, "\n"); !strings.Contains(lines[0], "──▶ [SNS stg-hub]") || !strings.HasPrefix(lines[len(lines)-1], "  └─ IDLE") {
		t.Fatalf("토픽 + 종류:\n%s", withTopic)
	}
}

func TestResolveFlow_큐_없는_흐름은_토픽이_있어도_AWS_를_묻지_않는다(t *testing.T) {
	no, nq := fakeFlow(t, typeStat(), sampleQueues(), nil)
	m := flowModel(t)
	f := typeFlow()
	f.Topic = "stg-hub"
	s := resolveFlow(context.Background(), m.clients, m.sources, m.proj, f, true)
	if no.Load() != 1 || nq.Load() != 0 || s.awsErr != "" || s.queues != nil || s.outbox == nil {
		t.Fatalf("아웃박스 %d 큐 %d: %+v", no.Load(), nq.Load(), s)
	}
}

func TestFlowView_종류_줄은_경고색이고_행_없는_설정_종류는_자리가_있을_때만_흐리게_보인다(t *testing.T) {
	m := flowModel(t)
	st := typeStat()
	s := &flowState{list: []flows.Flow{typeFlow()}, snaps: map[string]flowSnap{"shop-outbox-stg": {outbox: &st}}}
	tab := &queryTab{flow: s}
	v := m.flowView(tab, 160, 20)
	if !strings.Contains(ansi.Strip(v), "IDLE") {
		t.Fatalf("자리가 있으면 보인다:\n%s", ansi.Strip(v))
	}
	earn := dashWarnStyle.Render("EARN          대기 3 · 재시도 1 · 한도 소진 3  → consume-outbox-earn")
	idle := dimStyle.Render("IDLE          대기 0 · 재시도 0 · 한도 소진 0  → consume-outbox-idle")
	if !strings.Contains(v, earn) || !strings.Contains(v, idle) {
		t.Fatalf("한도 소진 줄은 경고색, 행 없는 줄은 흐림이어야 한다:\n%q", v)
	}
	// 이름·빈 줄 + 아웃박스 + 종류 넷 = 7줄이라 6줄이면 IDLE 을 뺀다. 상자 수도 같이 줄어 고르기가 안 넘친다
	v = ansi.Strip(m.flowView(tab, 160, 6))
	if strings.Contains(v, "IDLE") || !strings.Contains(v, "LONG_UNKNOWN") {
		t.Fatalf("자리가 없으면 뺀다:\n%s", v)
	}
	if _, boxes, _ := s.curBoxes(); len(boxes) != 4 {
		t.Fatalf("고를 상자: %d", len(boxes))
	}
}

func TestFlow_종류_줄을_고르고_Enter_는_그_종류의_재시도_행을_READ_ONLY_로_연다(t *testing.T) {
	fakeFlow(t, typeStat(), nil, nil)
	m := flowModel(t)
	writeFlowsFile(t, m.proj.Dir, `[{"name":"shop-outbox-stg","outbox":{"conn":"shop-stg","table":"public.t_outbox","type":"event_type","retry":"retry_count","due":"check_needed_at",
		"types":{"EARN":{"limit":20,"consumer":"consume-outbox-earn"}}}}]`)
	feedFlow(m, m.openFlows(nil))
	v := ansi.Strip(m.View())
	if !strings.Contains(v, "! shop-outbox-stg") || !strings.Contains(v, "├─ EARN") || !strings.Contains(v, "└─ LONG_UNKNOWN") {
		t.Fatalf("종류 줄:\n%s", v)
	}
	flowTab := m.cur()
	m.Update(key("right"))
	if flowTab.flow.box != 1 {
		t.Fatalf("→ 는 첫 종류 줄로 간다: %d", flowTab.flow.box)
	}
	n := len(m.tabs)
	_, cmd := m.Update(key("enter"))
	if len(m.tabs) != n+1 || m.cur().conn != 0 || m.cur().running == nil || !m.cur().runningReadOnly {
		t.Fatalf("종류 상세: tabs=%d conn=%d running=%v", len(m.tabs), m.cur().conn, m.cur().running != nil)
	}
	o := *flowTab.flow.list[0].Outbox
	var sql string
	for _, msg := range collect(cmd) {
		if r, ok := msg.(resultMsg); ok {
			sql = r.sql
		}
	}
	if sql != flows.RetrySQL(o, flows.TypeStat{Name: "EARN"}) {
		t.Fatalf("종류 상세 질의: %q", sql)
	}
}

func TestFlowCLI_종류마다_outbox_type_줄을_더한다(t *testing.T) {
	fakeFlow(t, typeStat(), sampleQueues(), nil)
	dir := flowCLIProject(t)
	writeFlowsFile(t, dir, `[{"name":"ob","outbox":{"conn":"app-stg","table":"t_outbox","type":"event_type","retry":"retry_count",
		"types":{"EARN":{"limit":20,"consumer":"consume-outbox-earn"}}}}]`)
	code, out, errw := runCLITest(t, "", "flows", "-p", dir, "--format", "json")
	if code != exitOK {
		t.Fatalf("%d %q", code, errw)
	}
	var rows []flowRow
	must(t, json.Unmarshal([]byte(out), &rows))
	if len(rows) != 4 || rows[0].Stage != "outbox" {
		t.Fatalf("줄: %s", out)
	}
	want := []flowRow{
		{Flow: "ob", Stage: "outbox-type", Name: "EARN", Metrics: "대기 3 · 재시도 1 · 한도 소진 3 · → consume-outbox-earn", Warn: true},
		{Flow: "ob", Stage: "outbox-type", Name: "CLIP", Metrics: "대기 4 · 재시도 1 · 한도 소진 0 · 오래됨 1 · → ?", Warn: true},
		{Flow: "ob", Stage: "outbox-type", Name: "LONG_UNKNOWN", Metrics: "대기 2 · 재시도 0 · 한도 소진 0 · → ?"},
	}
	for i, w := range want {
		if rows[i+1] != w {
			t.Errorf("%d: %+v, want %+v", i, rows[i+1], w)
		}
	}
}

func TestRunPanel_흐름_패널은_행이_있는_종류만_10개까지_보인다(t *testing.T) {
	st := flows.Stat{Pending: 12, Retry: 0, Stale: 0, Exhausted: 0}
	for i := range 12 {
		st.Types = append(st.Types, flows.TypeStat{Name: fmt.Sprintf("T%02d", i), Rows: 1, Waiting: 1})
	}
	fakeFlow(t, st, nil, nil)
	f := typeFlow() // IDLE·EARN·CLIP 은 행이 없다
	r := runPanel(context.Background(), nil, nil, boardProject(t), boardPanel{View: "flow", flow: f}, true)
	var types []string
	for _, b := range r.boxes {
		if b.stage == stageType {
			types = append(types, b.name)
		}
	}
	if len(r.boxes) != 11 || r.boxes[0].stage != "outbox" || len(types) != boardFlowTypes || types[0] != "T00" || types[9] != "T09" {
		t.Fatalf("패널 상자: %v", types)
	}
	if body := ansi.Strip(strings.Join(panelBody(boardPanel{View: "flow"}, r, 80, 20), "\n")); !strings.Contains(body, "T00  대기 1 · 재시도 0 · 한도 소진 0  → ?") {
		t.Fatalf("패널 본문:\n%s", body)
	}
	if h := panelHeight(boardPanel{View: "flow", flow: f}, r); h != 13 {
		t.Fatalf("패널 높이: %d", h)
	}
}

func TestFlow_NULL_종류는_괄호_NULL_로_보이고_Enter_는_IS_NULL_로_묻는다(t *testing.T) {
	st := flows.Stat{Pending: 3, Retry: 2, Stale: -1, Exhausted: 0, Types: []flows.TypeStat{
		{Name: "NULL", Null: true, Rows: 2, Waiting: 2, Retrying: 1},
		{Name: "NULL", Rows: 1, Waiting: 1, Retrying: 1},
	}}
	fakeFlow(t, st, nil, nil)
	m := flowModel(t)
	writeFlowsFile(t, m.proj.Dir, `[{"name":"ob","outbox":{"conn":"shop-stg","table":"t","type":"event_type","retry":"retry_count","types":{"NULL":{"consumer":"consume-null"}}}}]`)
	feedFlow(m, m.openFlows(nil))
	v := ansi.Strip(m.View())
	// 설정의 "NULL" 소비자는 글자 'NULL' 종류에만 붙는다
	if !strings.Contains(v, "├─ (NULL)  대기 2 · 재시도 1 · 한도 소진 0  → ?") || !strings.Contains(v, "└─ NULL    대기 1 · 재시도 1 · 한도 소진 0  → consume-null") {
		t.Fatalf("NULL 종류 줄:\n%s", v)
	}
	flowTab := m.cur()
	m.Update(key("right"))
	_, cmd := m.Update(key("enter"))
	var sql string
	for _, msg := range collect(cmd) {
		if r, ok := msg.(resultMsg); ok {
			sql = r.sql
		}
	}
	if want := flows.RetrySQL(*flowTab.flow.list[0].Outbox, flows.TypeStat{Null: true}); sql != want || !strings.Contains(sql, `"event_type"::text is null`) {
		t.Fatalf("NULL 종류 상세: %q", sql)
	}
}

func TestFlowCLI_종류가_잘리면_아웃박스_수치에_알린다(t *testing.T) {
	fakeFlow(t, flows.Stat{Pending: 5000, Retry: 0, Stale: -1, Exhausted: 0, Truncated: true, Types: []flows.TypeStat{{Name: "A", Rows: 1, Waiting: 1}}}, nil, nil)
	dir := flowCLIProject(t)
	writeFlowsFile(t, dir, `[{"name":"ob","outbox":{"conn":"app-stg","table":"t_outbox","type":"event_type","retry":"retry_count"}}]`)
	code, out, errw := runCLITest(t, "", "flows", "-p", dir, "--format", "json")
	if code != exitOK {
		t.Fatalf("%d %q", code, errw)
	}
	var rows []flowRow
	must(t, json.Unmarshal([]byte(out), &rows))
	if len(rows) != 2 || rows[0].Metrics != "대기 5000 · 재시도 0 · 한도 소진 0 · 종류 1,000개만" {
		t.Fatalf("잘림: %s", out)
	}
}

func TestFlowBoxes_종류가_잘리면_빠진_설정_종류를_0건으로_그리지_않는다(t *testing.T) {
	st := typeStat()
	st.Truncated = true
	for _, b := range flowBoxes(typeFlow(), flowSnap{outbox: &st}, true) {
		if b.name == "IDLE" {
			t.Fatalf("잘린 결과에 없는 설정 종류를 0건으로 그렸다: %+v", b)
		}
	}
}
