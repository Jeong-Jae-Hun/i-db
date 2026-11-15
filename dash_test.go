package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
	"github.com/charmbracelet/x/ansi"
	"github.com/muesli/termenv"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
)

// ─── R64 대시보드 ───

// fakeProbe 는 확인 함수를 바꿔 끼우고 부른 횟수를 센다. 타이머는 1ms 로 줄여 collect 가 기다리지 않게 한다.
func fakeProbe(t *testing.T, f func(c db.Conn) db.Health) *atomic.Int32 {
	t.Helper()
	var n atomic.Int32
	oldProbe, oldEvery := probeConn, dashEvery
	probeConn = func(_ context.Context, _ *db.Clients, _ *sourcePool, c db.Conn) db.Health {
		n.Add(1)
		return f(c)
	}
	dashEvery = time.Millisecond
	t.Cleanup(func() { probeConn, dashEvery = oldProbe, oldEvery })
	return &n
}

// healthy 는 종류별로 그럴듯한 확인 결과다. PG 는 활성 3, Redis 는 클라이언트 1,200 이다.
func healthy(c db.Conn) db.Health {
	switch c.Kind {
	case db.KindPG:
		return db.Health{OK: true, Latency: 12 * time.Millisecond, Version: "16.4",
			Metrics: []db.Metric{{Name: "활성", Value: "3"}, {Name: "크기", Value: "1.2 GB"}}}
	case db.KindRedis:
		return db.Health{OK: true, Latency: 3 * time.Millisecond, Version: "7.2",
			Metrics: []db.Metric{{Name: "클라이언트", Value: "1,200"}, {Name: "메모리", Value: "95%", Warn: true}}}
	}
	return db.Health{Err: "dial tcp: refused"}
}

// dashModel 은 PG prod·stg, Redis stg, OpenSearch local, 지운 자리 하나를 둔 모델이다.
func dashModel(t *testing.T) *model {
	t.Helper()
	p := &db.Project{Dir: t.TempDir(), Mapping: map[string]string{}, Conns: []db.Conn{
		{Name: "x-prod", UUID: "p", Kind: db.KindPG, Group: "app", Host: "127.0.0.1", Port: 1},
		{Name: "x-stg", UUID: "s", Kind: db.KindPG, Group: "app", Host: "127.0.0.1", Port: 1},
		{Name: "cache-stg", UUID: "r", Kind: db.KindRedis, Group: "cache", Host: "127.0.0.1", Port: 1},
		{UUID: "gone"}, // 지운 접속 자리(Kind "")
		{Name: "search", UUID: "o", Kind: db.KindOpenSearch, Group: "obs", URL: "http://127.0.0.1:1"},
	}}
	m := newModel(p)
	m.Update(tea.WindowSizeMsg{Width: 160, Height: 40})
	return m
}

// feed 는 메시지를 모델에 넣는다. 타이머 메시지는 넣지 않고 돌려준다.
func feed(m *model, msgs []tea.Msg) []dashTickMsg {
	var ticks []dashTickMsg
	for _, msg := range msgs {
		if tk, ok := msg.(dashTickMsg); ok {
			ticks = append(ticks, tk)
			continue
		}
		m.Update(msg)
	}
	return ticks
}

func dashTabs(m *model) int {
	n := 0
	for _, t := range m.tabs {
		if t.dash != nil {
			n++
		}
	}
	return n
}

func TestDash_Ctrl_G_는_대시보드_탭을_하나만_열고_다시_누르면_그_탭으로_간다(t *testing.T) {
	fakeProbe(t, healthy)
	m := dashModel(t)
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlG})
	if dashTabs(m) != 1 || m.cur().dash == nil || m.cur().title != "대시보드" || m.focus != fResult {
		t.Fatalf("대시보드 탭이 활성이어야 한다: tabs=%d focus=%v", dashTabs(m), m.focus)
	}
	m.switchTab(0)
	_, cmd := m.Update(tea.KeyMsg{Type: tea.KeyCtrlG})
	if dashTabs(m) != 1 || m.cur().dash == nil || len(m.tabs) != 2 {
		t.Fatalf("두 번째 Ctrl+G 는 새 탭을 만들지 않는다: tabs=%d", len(m.tabs))
	}
	if cmd == nil {
		t.Fatal("옮기면서 바로 다시 확인한다")
	}
}

func TestDash_팔레트의_대시보드_항목이_대시보드를_연다(t *testing.T) {
	fakeProbe(t, healthy)
	m := dashModel(t)
	palette(m, "대시보드")
	if dashTabs(m) != 1 {
		t.Fatalf("팔레트로 열려야 한다: %d", dashTabs(m))
	}
}

func TestDash_확인_중에는_줄마다_말줄임표이고_결과가_오는_대로_바뀐다(t *testing.T) {
	n := fakeProbe(t, healthy)
	m := dashModel(t)
	_, cmd := m.Update(tea.KeyMsg{Type: tea.KeyCtrlG})
	d := m.cur().dash
	if len(d.pending) != 4 {
		t.Fatalf("지운 자리를 뺀 접속 넷을 확인한다: %v", d.pending)
	}
	if v := ansi.Strip(m.View()); strings.Count(v, "…  ") < 4 || !strings.Contains(v, "확인 중 4") {
		t.Fatalf("확인 중 표시가 없다:\n%s", v)
	}
	msgs := collect(cmd)
	if n.Load() != 4 {
		t.Fatalf("확인 횟수 %d", n.Load())
	}
	// 하나씩 넣으며 그 줄만 바뀌는지 본다
	var probes []dashProbeMsg
	for _, msg := range msgs {
		if p, ok := msg.(dashProbeMsg); ok {
			probes = append(probes, p)
		}
	}
	m.Update(probes[0])
	if len(d.pending) != 3 {
		t.Fatalf("결과 하나가 오면 확인 중이 하나 준다: %d", len(d.pending))
	}
	for _, p := range probes[1:] {
		m.Update(p)
	}
	v := ansi.Strip(m.View())
	for _, want := range []string{"●", "✕", "12ms", "16.4", "활성 3 · 크기 1.2 GB", "클라이언트 1,200 · 메모리 95%", "dial tcp: refused"} {
		if !strings.Contains(v, want) {
			t.Fatalf("%q 가 없다:\n%s", want, v)
		}
	}
	if strings.Contains(v, "확인 중") {
		t.Fatal("다 오면 확인 중 표시가 사라진다")
	}
}

func TestDash_요약은_종류_환경_연결_세션_트랜잭션_잠금_해제를_센다(t *testing.T) {
	fakeProbe(t, healthy)
	m := dashModel(t)
	m.inTx[1] = true
	m.unlocked[0] = time.Now().Add(time.Minute)
	_, cmd := m.Update(tea.KeyMsg{Type: tea.KeyCtrlG})
	feed(m, collect(cmd))
	s := m.dashSum(m.cur().dash)
	want := dashSummary{total: 4, pg: 2, redis: 1, source: 1, prod: 1, stg: 2, local: 1, ok: 3, fail: 1,
		pgActive: 6, redisClients: 1200, openTx: 1, unlocked: 1}
	if s != want {
		t.Fatalf("요약\n got %+v\nwant %+v", s, want)
	}
	v := ansi.Strip(m.View())
	for _, card := range []string{"4 (PG 2 · Redis 1 · 저장소 1)", "prod 1 · stg 2 · local 1", "● 3 / ✕ 1", "PG 활성 세션", "Redis 클라이언트", "1200", "열린 트랜잭션", "prod 잠금 해제"} {
		if !strings.Contains(v, card) {
			t.Fatalf("카드 %q 가 없다:\n%s", card, v)
		}
	}
}

func TestDash_좁은_화면에서는_카드가_두_줄로_넘어간다(t *testing.T) {
	cards := []string{dashCard("a", "1234567890"), dashCard("b", "1234567890"), dashCard("c", "1234567890")}
	wide, narrow := wrapCards(cards, 200), wrapCards(cards, 32)
	if lipgloss.Height(wide) != 4 || lipgloss.Height(narrow) != 8 {
		t.Fatalf("넓으면 한 줄(4), 좁으면 두 줄(8): %d %d", lipgloss.Height(wide), lipgloss.Height(narrow))
	}
	if lipgloss.Width(narrow) > 32 {
		t.Fatalf("폭을 넘으면 안 된다: %d", lipgloss.Width(narrow))
	}
}

func TestDash_경고_수치는_경고_색으로_그린다(t *testing.T) {
	old := lipgloss.ColorProfile()
	lipgloss.SetColorProfile(termenv.ANSI256)
	t.Cleanup(func() { lipgloss.SetColorProfile(old) })
	d := &dashState{health: map[string]db.Health{"r": healthy(db.Conn{Kind: db.KindRedis})}, pending: map[string]bool{}}
	got := dashMetrics(d, db.Conn{UUID: "r"})
	if !strings.Contains(got, dashWarnStyle.Render("메모리 95%")) || strings.Contains(got, dashWarnStyle.Render("클라이언트 1,200")) {
		t.Fatalf("Warn 인 수치만 경고 색: %q", got)
	}
}

func TestDash_r_은_바로_다시_확인하고_늦게_온_이전_결과는_버린다(t *testing.T) {
	n := fakeProbe(t, healthy)
	m := dashModel(t)
	_, first := m.Update(tea.KeyMsg{Type: tea.KeyCtrlG})
	old := collect(first)
	_, again := m.Update(key("r"))
	if again == nil || len(m.cur().dash.pending) != 4 {
		t.Fatal("r 은 모두 다시 확인한다")
	}
	feed(m, old)
	if len(m.cur().dash.health) != 0 {
		t.Fatal("r 이전 확인의 결과는 버린다")
	}
	feed(m, collect(again))
	if n.Load() != 8 || len(m.cur().dash.health) != 4 || len(m.cur().dash.pending) != 0 {
		t.Fatalf("다시 확인한 결과가 들어와야 한다: n=%d", n.Load())
	}
}

func TestDash_타이머는_활성일_때만_확인하고_탭을_닫으면_멈춘다(t *testing.T) {
	n := fakeProbe(t, healthy)
	m := dashModel(t)
	_, cmd := m.Update(tea.KeyMsg{Type: tea.KeyCtrlG})
	ticks := feed(m, collect(cmd))
	if len(ticks) != 1 {
		t.Fatalf("타이머 하나: %v", ticks)
	}
	// 활성이면 다시 확인하고 새 타이머를 건다
	_, cmd = m.Update(ticks[0])
	ticks = feed(m, collect(cmd))
	if n.Load() != 8 || len(ticks) != 1 {
		t.Fatalf("활성 탭 타이머는 다시 확인: n=%d ticks=%d", n.Load(), len(ticks))
	}
	// 다른 탭이 활성이면 확인 없이 타이머만 다시 건다
	m.switchTab(0)
	_, cmd = m.Update(ticks[0])
	ticks = feed(m, collect(cmd))
	if n.Load() != 8 || len(ticks) != 1 {
		t.Fatalf("비활성이면 확인하지 않는다: n=%d ticks=%d", n.Load(), len(ticks))
	}
	// 낡은 세대의 타이머는 버린다(r 로 다시 센 뒤)
	stale := ticks[0]
	m.switchTab(1)
	m.Update(key("r"))
	if _, cmd := m.Update(stale); cmd != nil {
		t.Fatal("낡은 세대 타이머는 아무것도 하지 않는다")
	}
	// 탭을 닫으면 그 탭의 타이머가 끝난다
	cur := m.cur().dash.gen
	m.closeTab(m.cur())
	if dashTabs(m) != 0 {
		t.Fatal("대시보드 탭이 닫혀야 한다")
	}
	if _, cmd := m.Update(dashTickMsg{tab: stale.tab, gen: cur}); cmd != nil {
		t.Fatal("닫힌 탭의 타이머는 멈춘다")
	}
}

func TestDash_Enter_는_트리에서_그_접속을_고르고_트리로_옮긴다(t *testing.T) {
	fakeProbe(t, healthy)
	m := dashModel(t)
	for _, g := range m.roots {
		g.expanded = false
	}
	m.filter = "zzz"
	m.rebuild()
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlG})
	// 순서는 트리와 같다: app(x-prod, x-stg) · cache(cache-stg) · obs(search)
	m.Update(key("j"))
	m.Update(key("j"))
	m.Update(key("enter"))
	if m.focus != fTree || m.filter != "" {
		t.Fatalf("트리로 옮기고 거르기를 푼다: focus=%v filter=%q", m.focus, m.filter)
	}
	if n := m.current(); n == nil || n.kind != nConn || m.conn(n.conn).Name != "cache-stg" {
		t.Fatalf("cache-stg 를 골라야 한다: %+v", n)
	}
}

func TestDash_슬래시는_그룹_이름_환경으로_거른다(t *testing.T) {
	fakeProbe(t, healthy)
	m := dashModel(t)
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlG})
	m.Update(key("/"))
	typeText(m, "stg")
	m.Update(key("enter"))
	d := m.cur().dash
	if rows := m.dashRows(d); len(rows) != 2 || m.conn(rows[0]).Name != "x-stg" || m.conn(rows[1]).Name != "cache-stg" {
		t.Fatalf("stg 로 거른 줄: %v", rows)
	}
	d.filter = "obs"
	if rows := m.dashRows(d); len(rows) != 1 || m.conn(rows[0]).Name != "search" {
		t.Fatalf("그룹으로도 거른다: %v", rows)
	}
	m.Update(tea.KeyMsg{Type: tea.KeyEsc})
	if d.filter != "" || len(m.dashRows(d)) != 4 {
		t.Fatal("esc 는 거르기를 푼다")
	}
}

func TestDash_다시_확인하는_동안_이전_값을_그대로_보이고_머리에_확인_중을_센다(t *testing.T) {
	fakeProbe(t, healthy)
	m := dashModel(t)
	_, cmd := m.Update(tea.KeyMsg{Type: tea.KeyCtrlG})
	feed(m, collect(cmd))
	m.Update(key("r"))
	d := m.cur().dash
	if len(d.pending) != 4 {
		t.Fatalf("r 은 모두 다시 확인한다: %v", d.pending)
	}
	if s := m.dashSum(d); s.ok != 3 || s.fail != 1 || s.pgActive != 6 || s.redisClients != 1200 {
		t.Fatalf("요약은 이전 값을 센다: %+v", s)
	}
	v := ansi.Strip(m.View())
	for _, want := range []string{"확인 중 4", "●", "✕", "12ms", "활성 3 · 크기 1.2 GB", "dial tcp: refused"} {
		if !strings.Contains(v, want) {
			t.Fatalf("%q 가 없다:\n%s", want, v)
		}
	}
	if body := ansi.Strip(m.dashView(m.cur(), 160, 30)); strings.Contains(body, "…") {
		t.Fatalf("확인한 적 있는 접속은 말줄임표가 아니다:\n%s", body)
	}
}

// spawnCmds 는 명령을 고루틴에서 돌린다. 막히는 확인을 기다리지 않고 묶음(Batch)은 펴서 각각 돌린다.
func spawnCmds(cmd tea.Cmd) {
	if cmd == nil {
		return
	}
	go func() {
		if b, ok := cmd().(tea.BatchMsg); ok {
			for _, c := range b {
				spawnCmds(c)
			}
		}
	}()
}

// waitN 은 ch 에서 n 번 받을 때까지 기다린다. 5초 안에 오지 않으면 시험을 멈춘다.
func waitN(t *testing.T, ch chan struct{}, n int, what string) {
	t.Helper()
	for range n {
		select {
		case <-ch:
		case <-time.After(5 * time.Second):
			t.Fatalf("%s 를 기다리다 멈춤", what)
		}
	}
}

func TestDash_r_과_탭_닫기는_진행_중인_이전_확인을_취소한다(t *testing.T) {
	started, canceled := make(chan struct{}, 16), make(chan struct{}, 16)
	oldProbe, oldEvery := probeConn, dashEvery
	t.Cleanup(func() { probeConn, dashEvery = oldProbe, oldEvery })
	dashEvery = time.Hour
	probeConn = func(ctx context.Context, _ *db.Clients, _ *sourcePool, _ db.Conn) db.Health {
		started <- struct{}{}
		<-ctx.Done()
		if errors.Is(ctx.Err(), context.Canceled) {
			canceled <- struct{}{}
		}
		return db.Health{Err: ctx.Err().Error()}
	}
	m := dashModel(t)
	_, cmd := m.Update(tea.KeyMsg{Type: tea.KeyCtrlG})
	spawnCmds(cmd)
	waitN(t, started, 4, "첫 확인 시작")
	_, cmd = m.Update(key("r"))
	waitN(t, canceled, 4, "r 이 이전 확인 취소")
	spawnCmds(cmd)
	waitN(t, started, 4, "다시 확인 시작")
	m.closeTab(m.cur())
	waitN(t, canceled, 4, "탭 닫기가 확인 취소")
}

func TestDash_낮은_화면에서는_카드를_한_줄로_접어_표가_다섯_줄을_넘게_보인다(t *testing.T) {
	fakeProbe(t, healthy)
	conns := make([]db.Conn, 10)
	for i := range conns {
		conns[i] = db.Conn{Name: fmt.Sprintf("pg-%02d-stg", i), UUID: fmt.Sprint(i), Kind: db.KindPG, Group: "app", Host: "127.0.0.1", Port: 1}
	}
	m := newModel(&db.Project{Dir: t.TempDir(), Mapping: map[string]string{}, Conns: conns})
	m.Update(tea.WindowSizeMsg{Width: 160, Height: 40})
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlG})
	tab := m.cur()
	tab.dash.cursor = 99
	lines := strings.Split(ansi.Strip(m.dashView(tab, 160, 9)), "\n")
	if len(lines) > 9 || !strings.Contains(lines[0], "접속 10") || strings.Contains(lines[0], "╭") {
		t.Fatalf("카드가 한 줄 요약이어야 한다:\n%s", strings.Join(lines, "\n"))
	}
	if rows := strings.Count(strings.Join(lines, "\n"), "pg-"); rows < 5 {
		t.Fatalf("표가 다섯 줄 이상: %d\n%s", rows, strings.Join(lines, "\n"))
	}
	if !strings.Contains(lines[len(lines)-1], "▸ ") || !strings.Contains(lines[len(lines)-1], "pg-09") {
		t.Fatalf("범위를 넘은 커서는 마지막 줄로 보인다:\n%s", strings.Join(lines, "\n"))
	}
	if tab.dash.cursor != 99 {
		t.Fatalf("그리기는 커서를 바꾸지 않는다: %d", tab.dash.cursor)
	}
	if v := ansi.Strip(m.dashView(tab, 160, 30)); !strings.Contains(v, "╭") {
		t.Fatalf("높으면 카드로 그린다:\n%s", v)
	}
}

func TestDash_세션에는_대시보드_탭을_남기지_않는다(t *testing.T) {
	fakeProbe(t, healthy)
	t.Setenv("XDG_STATE_HOME", t.TempDir())
	m := dashModel(t)
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlG})
	must(t, m.saveSession())
	tabs := savedTabs(t, m)
	if len(tabs) != 1 || tabs[0].Title == "대시보드" {
		t.Fatalf("대시보드 탭은 저장하지 않는다: %+v", tabs)
	}
}

// ─── db status (R64.3) ───

func TestStatus_prod_는_prod_플래그가_없으면_확인하지_않고_안내한다(t *testing.T) {
	var probed atomic.Int32
	var allowProd atomic.Bool
	fakeProbe(t, func(c db.Conn) db.Health {
		probed.Add(1)
		if c.Env() == "prod" && !allowProd.Load() {
			t.Errorf("prod 를 확인하면 안 된다: %s", c.Name)
		}
		return healthy(c)
	})
	dir := fakeCLIProject(t)
	code, out, errw := runCLITest(t, "", "status", "-p", dir, "--format", "json")
	if code != exitOK {
		t.Fatalf("확인한 접속이 모두 성공하면 0: %d %q", code, errw)
	}
	var rows []statusRow
	must(t, json.Unmarshal([]byte(out), &rows))
	if len(rows) != 5 || probed.Load() != 3 {
		t.Fatalf("접속 다섯, 확인 셋: %d %d", len(rows), probed.Load())
	}
	for _, r := range rows {
		switch {
		case r.Env == "prod" && (r.OK != nil || r.Note != "--prod 로 확인"):
			t.Errorf("prod 는 확인하지 않고 안내: %+v", r)
		case r.Env != "prod" && (r.OK == nil || !*r.OK):
			t.Errorf("확인한 접속은 ok: %+v", r)
		}
	}
	if !strings.Contains(out, `"metrics":[{"name":"활성","value":"3","warn":false}`) {
		t.Fatalf("수치는 {name,value,warn} 배열: %s", out)
	}
	allowProd.Store(true)
	_, _, _ = runCLITest(t, "", "status", "-p", dir, "--prod", "--format", "tsv")
	if probed.Load() != 3+5 {
		t.Fatalf("--prod 면 prod 도 확인한다: %d", probed.Load())
	}
}

func TestStatus_하나라도_실패하면_1_이고_오류의_비밀번호를_가린다(t *testing.T) {
	fakeProbe(t, func(c db.Conn) db.Health {
		if c.Kind == db.KindRedis {
			return db.Health{Err: "auth failed: " + cliPassword}
		}
		return healthy(c)
	})
	dir := fakeCLIProject(t)
	code, out, _ := runCLITest(t, "", "status", "-p", dir, "--format", "tsv")
	if code != exitErr {
		t.Fatalf("실패가 있으면 1: %d", code)
	}
	if strings.Contains(out, cliPassword) {
		t.Fatalf("비밀번호가 찍혔다: %s", out)
	}
	lines := strings.Split(strings.TrimSpace(out), "\n")
	if lines[0] != "group\tname\tenv\tkind\tok\tlatency_ms\tversion\tmetrics" {
		t.Fatalf("머리 줄: %q", lines[0])
	}
	if !strings.Contains(out, "grp\tapp-stg-redis\tstg\tredis\tfalse\t") || !strings.Contains(out, "grp\tapp-stg\tstg\tpostgresql\ttrue\t12\t16.4\t활성 3 · 크기 1.2 GB") {
		t.Fatalf("tsv 줄:\n%s", out)
	}
	if !strings.Contains(out, "grp\tapp-prod\tprod\tpostgresql\t-\t\t\t--prod 로 확인") {
		t.Fatalf("prod 안내 줄:\n%s", out)
	}
}

func TestStatus_c_로_고른_접속만_확인하고_형식을_검사한다(t *testing.T) {
	n := fakeProbe(t, healthy)
	dir := fakeCLIProject(t)
	code, out, _ := runCLITest(t, "", "status", "-p", dir, "-c", "app-stg", "-c", "nogroup-stg", "--format", "table")
	if code != exitOK || n.Load() != 2 || !strings.Contains(out, "app-stg") || !strings.Contains(out, "nogroup-stg") || strings.Contains(out, "redis") {
		t.Fatalf("-c 둘만: code=%d n=%d\n%s", code, n.Load(), out)
	}
	if !strings.Contains(out, "(2행)") {
		t.Fatalf("table 형식: %s", out)
	}
	if code, _, errw := runCLITest(t, "", "status", "-p", dir, "-c", "nope"); code != exitUsage || !strings.Contains(errw, "접속 없음: nope") {
		t.Fatalf("없는 접속은 2: %d %q", code, errw)
	}
	if code, _, _ := runCLITest(t, "", "status", "-p", dir, "--format", "csv"); code != exitUsage {
		t.Fatalf("csv 는 받지 않는다: %d", code)
	}
	var help strings.Builder
	cliHelp(nil, &help, &help)
	if !strings.Contains(help.String(), "status") {
		t.Fatal("도움말에 status 가 있어야 한다")
	}
}
