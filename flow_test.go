package main

import (
	"context"
	"encoding/json"
	"errors"
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
	{"name":"hub","outbox":{"conn":"shop-stg","table":"public.t_outbox","type":"event_type","retry":"retry_count","due":"check_needed_at"},"aws":"aws-stg","topic":"stg-hub"},
	{"name":"bad","aws":"shop-stg","topic":"x"}
]`

// sampleTopic 은 큐 둘(첫째는 DLQ 4건·Lambda 하나, 둘째는 비어 있음)과 https 구독 하나다.
func sampleTopic() *awsres.Topic {
	return &awsres.Topic{Arn: "arn:aws:sns:ap-northeast-2:1:stg-hub", Name: "stg-hub", Subs: []awsres.Sub{
		{Protocol: "sqs", Endpoint: "q1", Queue: &awsres.Queue{Name: "q1", Visible: "0", InFlight: "2", Delayed: "0", DLQ: "q1-dlq", DLQArn: "arn:aws:sqs:ap-northeast-2:999:q1-dlq", DLQCount: "4",
			Lambdas: []awsres.Mapping{{Function: "worker", State: "Enabled", LastResult: "OK"}}}},
		{Protocol: "sqs", Endpoint: "q2", Queue: &awsres.Queue{Name: "q2", Visible: "1", InFlight: "0", Delayed: "0"}},
		{Protocol: "https", Endpoint: "https://hook.example.com"},
	}}
}

// fakeFlow 는 아웃박스·토픽 풀이를 바꿔 끼우고 부른 횟수를 센다. 타이머는 1ms 다.
func fakeFlow(t *testing.T, stat flows.Stat, topic *awsres.Topic, topicErr error) (*atomic.Int32, *atomic.Int32) {
	t.Helper()
	var no, nt atomic.Int32
	oldO, oldT, oldE := flowOutbox, flowTopic, flowEvery
	flowOutbox = func(context.Context, *db.Clients, db.Conn, flows.Outbox) (flows.Stat, error) {
		no.Add(1)
		return stat, nil
	}
	flowTopic = func(context.Context, *sourcePool, db.Conn, string) (*awsres.Topic, error) {
		nt.Add(1)
		return topic, topicErr
	}
	flowEvery = time.Millisecond
	t.Cleanup(func() { flowOutbox, flowTopic, flowEvery = oldO, oldT, oldE })
	return &no, &nt
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
	return flows.Flow{Name: "hub", AWS: "aws-stg", Topic: "stg-hub",
		Outbox: &flows.Outbox{Conn: "shop-stg", Table: "public.t_outbox", Type: "event_type", Retry: "retry_count", Due: "check_needed_at"}}
}

// ─── 상자 ───

func TestFlowBoxes_아웃박스_SNS_큐_DLQ_Lambda_순서와_경고(t *testing.T) {
	st := flows.Stat{Pending: 120, Retry: 3, Stale: 1}
	boxes := flowBoxes(hubFlow(), flowSnap{outbox: &st, topic: sampleTopic()}, true)
	var got []string
	for _, b := range boxes {
		w := ""
		if b.warn {
			w = "!"
		}
		got = append(got, b.stage+":"+b.name+":"+b.metrics+w)
	}
	want := []string{
		"outbox:shop-stg public.t_outbox:대기 120 · 재시도 3 · 오래됨 1!",
		"sns:stg-hub:구독 3",
		"sqs:q1:대기 0 · 처리 중 2 · 지연 0",
		"dlq:q1-dlq:4!",
		"lambda:worker:Enabled · OK",
		"sqs:q2:대기 1 · 처리 중 0 · 지연 0",
		"sub:https://hook.example.com:https",
	}
	if strings.Join(got, "\n") != strings.Join(want, "\n") {
		t.Fatalf("상자:\n%s", strings.Join(got, "\n"))
	}
	if boxes[2].path != "sqs/q1" || boxes[4].path != "lambda/worker" || boxes[1].path != "sns/arn:aws:sns:ap-northeast-2:1:stg-hub" || boxes[6].path != "" {
		t.Fatalf("열 경로: %+v", boxes)
	}
	if boxes[3].path != "sqs/arn:aws:sqs:ap-northeast-2:999:q1-dlq" {
		t.Fatalf("DLQ 는 다른 계정일 수 있어 ARN 으로 연다: %q", boxes[3].path)
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
		tp := &awsres.Topic{Name: "t", Subs: []awsres.Sub{{Protocol: "sqs", Queue: &awsres.Queue{Name: "q", Visible: "0", InFlight: "0", Delayed: "0",
			Lambdas: []awsres.Mapping{{Function: "f", State: c.state, LastResult: c.last}}}}}}
		boxes := flowBoxes(flows.Flow{Name: "x", AWS: "a", Topic: "t"}, flowSnap{topic: tp}, true)
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
	wait := flowBoxes(hubFlow(), flowSnap{}, false)
	if len(wait) != 2 || wait[0].metrics != "…" || wait[1].metrics != "…" {
		t.Fatalf("결과 없음: %+v", wait)
	}
	prod := flowBoxes(hubFlow(), flowSnap{outboxNote: dashProdHint, topicNote: dashProdHint}, true)
	if prod[0].note != dashProdHint || prod[0].failed() || boxLabel(prod[1]) != "[SNS stg-hub | --prod 로 확인]" {
		t.Fatalf("prod: %+v", prod)
	}
}

// ─── 그림 ───

func TestFlowLayout_넓으면_왼쪽에서_오른쪽으로_DLQ_는_큐_아래_가지다(t *testing.T) {
	st := flows.Stat{Pending: 120, Retry: 3, Stale: 1}
	boxes := flowBoxes(hubFlow(), flowSnap{outbox: &st, topic: sampleTopic()}, true)
	head := "[아웃박스 shop-stg public.t_outbox | 대기 120 · 재시도 3 · 오래됨 1] ──▶ [SNS stg-hub]"
	pad := strings.Repeat(" ", ansi.StringWidth(head))
	want := strings.Join([]string{
		head + " ─┬─▶ [SQS q1 | 대기 0 · 처리 중 2] ──▶ [λ worker OK]",
		pad + "  │   └─ DLQ q1-dlq 4",
		pad + "  ├─▶ [SQS q2 | 대기 1 · 처리 중 0]",
		pad + "  └─▶ [https https://hook.example.com]",
	}, "\n")
	if got := plain(flowLayout(boxes, 200)); got != want {
		t.Fatalf("가로:\n%s\n\nwant:\n%s", got, want)
	}
	// 구독이 하나면 갈래 없이 잇는다
	one := flowBoxes(flows.Flow{Name: "x", AWS: "a", Topic: "t"}, flowSnap{topic: &awsres.Topic{Name: "t", Subs: sampleTopic().Subs[:1]}}, true)
	want = "[SNS t] ──▶ [SQS q1 | 대기 0 · 처리 중 2] ──▶ [λ worker OK]\n" + strings.Repeat(" ", len("[SNS t]")) + "     └─ DLQ q1-dlq 4"
	if got := plain(flowLayout(one, 200)); got != want {
		t.Fatalf("구독 하나:\n%s", got)
	}
}

func TestFlowLayout_좁으면_위에서_아래로_쌓는다(t *testing.T) {
	st := flows.Stat{Pending: 120, Retry: 3, Stale: 1}
	boxes := flowBoxes(hubFlow(), flowSnap{outbox: &st, topic: sampleTopic()}, true)
	want := strings.Join([]string{
		"[아웃박스 shop-stg public.t_outbox | 대기 120 · 재시도 3 · 오래됨 1]",
		"  ▼",
		"[SNS stg-hub]",
		"  ├─▶ [SQS q1 | 대기 0 · 처리 중 2]",
		"  │     ├─ DLQ q1-dlq 4",
		"  │     └─▶ [λ worker OK]",
		"  ├─▶ [SQS q2 | 대기 1 · 처리 중 0]",
		"  └─▶ [https https://hook.example.com]",
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

func TestFlow_Alt_F_와_팔레트는_흐름_탭을_하나만_열고_설정_오류를_보인다(t *testing.T) {
	no, nt := fakeFlow(t, flows.Stat{Pending: 120, Retry: 3, Stale: 1}, sampleTopic(), nil)
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
	for _, want := range []string{"! hub", "✕ bad", "[아웃박스 shop-stg public.t_outbox | 대기 120 · 재시도 3 · 오래됨 1]", "DLQ q1-dlq 4"} {
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
	fakeFlow(t, flows.Stat{}, sampleTopic(), nil)
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
	fakeFlow(t, flows.Stat{}, sampleTopic(), nil)
	var seen atomic.Pointer[context.Context]
	flowTopic = func(ctx context.Context, _ *sourcePool, _ db.Conn, _ string) (*awsres.Topic, error) {
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
	fakeFlow(t, flows.Stat{Pending: 1, Retry: 0, Stale: 0}, sampleTopic(), nil)
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
	if _, opens, _ := f.snapshot(); len(opens) != 1 || opens[0] != "sqs/q1" {
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

func TestFlow_트리의_SNS_토픽에서_F_는_그_토픽의_흐름을_연다(t *testing.T) {
	_, nt := fakeFlow(t, flows.Stat{}, sampleTopic(), nil)
	var asked atomic.Value
	flowTopic = func(_ context.Context, _ *sourcePool, c db.Conn, topic string) (*awsres.Topic, error) {
		nt.Add(1)
		asked.Store(c.Name + " " + topic)
		return sampleTopic(), nil
	}
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
	_, cmd := m.Update(key("F"))
	if flowTabs(m) != 1 {
		t.Fatal("토픽 줄 F 는 흐름 탭을 연다")
	}
	feedFlow(m, cmd)
	s := m.cur().flow
	if len(s.list) != 3 || s.list[s.cursor].Name != "orders · aws-stg" || asked.Load() != "aws-stg "+arn {
		t.Fatalf("트리 흐름: %+v cursor=%d asked=%v", s.list, s.cursor, asked.Load())
	}
}

// ─── 대시보드 카드 ───

func TestDash_DLQ_쌓인_큐와_아웃박스_재시도_카드를_더한다(t *testing.T) {
	fakeFlow(t, flows.Stat{Pending: 9, Retry: 3, Stale: -1}, nil, nil)
	fakeProbe(t, func(c db.Conn) db.Health {
		if c.Kind == db.KindAWS {
			return db.Health{OK: true, Metrics: []db.Metric{{Name: "큐", Value: "10"}, {Name: awsres.MetricDLQ, Value: "2 (앞 200개)", Warn: true}}}
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
	if s.aws != 2 || s.dlq != 4 || s.outboxes != 1 || s.retry != 3 {
		t.Fatalf("요약: %+v", s)
	}
	v := ansi.Strip(m.View())
	if !strings.Contains(v, "DLQ 쌓인 큐") || !strings.Contains(v, "아웃박스 재시도") {
		t.Fatalf("카드가 없다:\n%s", v)
	}
	// 흐름이 없으면 아웃박스 카드도 없다
	must(t, os.Remove(filepath.Join(m.proj.Dir, ".dbtui", "flows.json")))
	feedFlow(m, m.dashRefresh(m.cur()))
	if s := m.dashSum(m.cur().dash); s.outboxes != 0 || strings.Contains(ansi.Strip(m.View()), "아웃박스 재시도") {
		t.Fatalf("아웃박스 카드가 남았다: %+v", s)
	}
}

func TestDash_아웃박스를_못_읽으면_재시도_합에서_뺀다(t *testing.T) {
	fakeFlow(t, flows.Stat{}, nil, nil)
	flowOutbox = func(context.Context, *db.Clients, db.Conn, flows.Outbox) (flows.Stat, error) {
		return flows.Stat{}, errors.New("timeout")
	}
	fakeProbe(t, healthy)
	m := flowModel(t)
	_, cmd := m.Update(tea.KeyMsg{Type: tea.KeyCtrlG})
	feedFlow(m, cmd)
	if s := m.dashSum(m.cur().dash); s.outboxes != 1 || s.retry != 0 || len(m.cur().dash.pending) != 0 {
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
		{"name":"hub","outbox":{"conn":"app-stg","table":"t_outbox","type":"event_type","retry":"retry_count"},"aws":"aws-stg","topic":"stg-hub"},
		{"name":"live","outbox":{"conn":"app-prod","table":"t_outbox","type":"event_type"},"aws":"aws-prod","topic":"prod-hub"}
	]`)
	return dir
}

func TestFlowCLI_json_은_상자마다_한_줄이고_prod_는_묻지_않는다(t *testing.T) {
	no, nt := fakeFlow(t, flows.Stat{Pending: 5, Retry: 1, Stale: -1}, sampleTopic(), nil)
	dir := flowCLIProject(t)
	code, out, errw := runCLITest(t, "", "flows", "-p", dir, "--format", "json")
	if code != exitOK {
		t.Fatalf("%d %q", code, errw)
	}
	var rows []flowRow
	must(t, json.Unmarshal([]byte(out), &rows))
	if len(rows) != 9 || no.Load() != 1 || nt.Load() != 1 {
		t.Fatalf("줄 %d, 아웃박스 %d, 토픽 %d: %s", len(rows), no.Load(), nt.Load(), out)
	}
	if r := rows[0]; r.Flow != "hub" || r.Stage != "outbox" || r.Metrics != "대기 5 · 재시도 1" || !r.Warn {
		t.Fatalf("아웃박스 줄: %+v", r)
	}
	if r := rows[3]; r.Stage != "dlq" || r.Name != "q1-dlq" || r.Metrics != "4" || !r.Warn {
		t.Fatalf("DLQ 줄: %+v", r)
	}
	if r := rows[7]; r.Flow != "live" || r.Stage != "outbox" || r.Note != dashProdHint || r.Error != "" {
		t.Fatalf("prod 줄: %+v", r)
	}
	// -f 로 고르고 --prod 면 prod 도 묻는다
	code, out, _ = runCLITest(t, "", "flows", "-p", dir, "-f", "live", "--prod", "--format", "tsv")
	if code != exitOK || !strings.HasPrefix(out, "flow\tstage\tname\tmetrics\twarn\nlive\toutbox\tapp-prod t_outbox\t대기 5 · 재시도 1\ttrue\n") {
		t.Fatalf("--prod: %d %q", code, out)
	}
}

func TestFlowCLI_실패와_설정_오류는_1_이고_없는_흐름은_2다(t *testing.T) {
	fakeFlow(t, flows.Stat{}, nil, errors.New("aws sns:ListTopics: ExpiredToken: expired"))
	dir := flowCLIProject(t)
	code, out, _ := runCLITest(t, "", "flows", "-p", dir, "-f", "hub", "--format", "json")
	if code != exitErr || !strings.Contains(out, `"error":"aws sns:ListTopics: ExpiredToken: expired"`) {
		t.Fatalf("실패: %d %s", code, out)
	}
	fakeFlow(t, flows.Stat{}, sampleTopic(), nil)
	writeFlowsFile(t, dir, `[{"name":"bad","aws":"app-stg","topic":"x"}]`)
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
