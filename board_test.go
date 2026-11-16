package main

import (
	"context"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
	"github.com/charmbracelet/x/ansi"
	"github.com/muesli/termenv"

	"github.com/Jeong-Jae-Hun/i-db/internal/awsres"
	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/flows"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
)

// ─── R70 쿼리 보드 ───

func writeBoard(t *testing.T, dir, name, body string) {
	t.Helper()
	must(t, os.MkdirAll(filepath.Join(dir, boardDir), 0o755))
	must(t, os.WriteFile(filepath.Join(dir, boardDir, name+".json"), []byte(body), 0o600))
}

// boardProject 는 PostgreSQL·Redis·Loki(stg·prod)·aws 접속과 흐름 하나를 가진 프로젝트다.
func boardProject(t *testing.T) *db.Project {
	t.Helper()
	p := &db.Project{Dir: t.TempDir(), Mapping: map[string]string{}, Conns: []db.Conn{
		{Name: "shop-stg", UUID: "pg", Kind: db.KindPG, Group: "shop", Host: "127.0.0.1", Port: 1},
		{Name: "cache-stg", UUID: "rd", Kind: db.KindRedis, Group: "shop", Host: "127.0.0.1", Port: 1},
		{Name: "logs-stg", UUID: "l1", Kind: db.KindLoki, URL: "http://loki:3100", Source: db.SourceDB},
		{Name: "logs-main", UUID: "l2", Kind: db.KindLoki, URL: "http://loki.prod.internal:3100", Source: db.SourceDB},
		{Name: "aws-stg", UUID: "aw", Kind: db.KindAWS, Region: "ap-northeast-2", Source: db.SourceDB},
	}}
	writeFlowsFile(t, p.Dir, flowsJSON)
	return p
}

func panelErrs(b *board) []string {
	out := make([]string, len(b.panels))
	for i, p := range b.panels {
		out[i] = p.err
	}
	return out
}

func TestLoadBoard_패널마다_검사하고_틀린_패널만_설정_오류다(t *testing.T) {
	p := boardProject(t)
	writeBoard(t, p.Dir, "ops", `{"title":"운영","refresh":"30s","panels":[
		{"title":"결제","conn":"shop-stg","query":"select count(*) from t","view":"number","warn":">0"},
		{"title":"이상","conn":"shop-stg","query":"select 1","view":"pie"},
		{"title":"aws","conn":"aws-stg","query":"x","view":"table"},
		{"title":"쓰기","conn":"shop-stg","query":"delete from t","view":"table"},
		{"title":"두 문장","conn":"shop-stg","query":"select 1; select 2","view":"table"},
		{"title":"키","conn":"cache-stg","query":"KEYS *","view":"table"},
		{"title":"레디스 쓰기","conn":"cache-stg","query":"DEL a","view":"table"},
		{"title":"레디스 읽기","conn":"cache-stg","query":"GET a","view":"number"},
		{"title":"범위","conn":"shop-stg","query":"select 1","view":"chart","since":"6h"},
		{"title":"로그","conn":"logs-stg","query":"{app=\"a\"}","view":"chart","since":"6h"},
		{"title":"경고 꼴","conn":"shop-stg","query":"select 1","view":"number","warn":"크다"},
		{"title":"표 경고","conn":"shop-stg","query":"select 1","view":"table","warn":">1"},
		{"title":"흐름","flow":"hub","view":"flow"},
		{"title":"없는 흐름","flow":"nope","view":"flow"},
		{"title":"틀린 흐름","flow":"bad","view":"flow"},
		{"conn":"nope","query":"select 1","view":"number"}
	]}`)
	b, err := loadBoard(p, "ops")
	must(t, err)
	if b.title != "운영" || b.every != 30*time.Second || b.err != "" {
		t.Fatalf("머리: %+v", b)
	}
	want := []string{
		"",
		`view must be one of number|table|chart|flow: "pie"`,
		"conn aws-stg: aws connections are not query panels (use view flow)",
		"query is not read-only: " + db.ErrNotReadOnly.Error(),
		"query must be a single statement",
		"KEYS is not run on boards (it blocks the server): use SCAN",
		"redis command is not a read command",
		"",
		"since is only for loki·prometheus·grafana",
		"",
		`warn must be like >0 or >=100: "크다"`,
		"warn is only for view number",
		"",
		"flow nope: no such flow (.dbtui/flows.json)",
		"flow bad: aws shop-stg: want kind aws, got postgresql",
		"conn nope: no such connection",
	}
	if got := panelErrs(b); strings.Join(got, "\n") != strings.Join(want, "\n") {
		t.Fatalf("패널 오류:\n%s", strings.Join(got, "\n"))
	}
	if b.panels[9].since != 6*time.Hour || b.panels[12].key != "aws-stg" || b.panels[15].Title != "#16" {
		t.Fatalf("채운 값: %v %q %q", b.panels[9].since, b.panels[12].key, b.panels[15].Title)
	}
}

func TestLoadBoard_refresh_는_기본_60초_최소_10초이고_깨진_JSON_은_패널_없이_오류다(t *testing.T) {
	p := boardProject(t)
	writeBoard(t, p.Dir, "a", `{"panels":[]}`)
	writeBoard(t, p.Dir, "fast", `{"refresh":"5s","panels":[]}`)
	writeBoard(t, p.Dir, "weird", `{"refresh":"soon","panels":[]}`)
	writeBoard(t, p.Dir, "broken", `{"panels":[`)
	cases := []struct {
		name, err string
		every     time.Duration
	}{
		{"a", "", boardDefaultEvery},
		{"fast", "refresh must be at least 10s: 5s", boardMinEvery},
		{"weird", `refresh: invalid duration "soon"`, boardDefaultEvery},
	}
	for _, c := range cases {
		b, err := loadBoard(p, c.name)
		must(t, err)
		if b.err != c.err || b.every != c.every || b.title != c.name {
			t.Fatalf("%s: %+v", c.name, b)
		}
	}
	b, err := loadBoard(p, "broken")
	if err != nil || !strings.HasPrefix(b.err, ".dbtui/boards/broken.json: ") || len(b.panels) != 0 {
		t.Fatalf("깨진 JSON: %+v %v", b, err)
	}
	for _, bad := range []string{"../x", ".hidden", ""} {
		if _, err := loadBoard(p, bad); err == nil {
			t.Fatalf("%q 는 보드 이름이 아니다", bad)
		}
	}
	if _, err := loadBoard(p, "none"); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("없는 보드: %v", err)
	}
	if got := strings.Join(listBoards(p.Dir), ","); got != "a,broken,fast,weird" {
		t.Fatalf("목록: %s", got)
	}
}

func TestBoardRedis_컬렉션_전체를_읽는_명령은_받지_않고_범위를_정한_읽기만_받는다(t *testing.T) {
	ok := []string{"GET a", "MGET a b", "HGET h f", "LLEN l", "ZCARD z", "INFO memory", "DBSIZE", "LRANGE l 0 49",
		"ZRANGE z 10 20 WITHSCORES", "SCAN 0", "SCAN 0 MATCH a:* COUNT 50"}
	bad := []string{"LRANGE l 0 -1", "LRANGE l 0 50", "LRANGE l -10 -1", "HGETALL h", "SMEMBERS s", "ZRANGE z 0 -1",
		"ZRANGE z 0 10 BYSCORE", "SCAN 0 COUNT 1000", "SCAN 0 COUNT", "HKEYS h"}
	for _, q := range ok {
		if !boardRedisBounded(db.SplitArgs(q)) {
			t.Errorf("받아야 함: %s", q)
		}
	}
	for _, q := range bad {
		if boardRedisBounded(db.SplitArgs(q)) {
			t.Errorf("막아야 함: %s", q)
		}
	}
	p := boardProject(t)
	panel := boardPanel{Conn: "cache-stg", Query: "HGETALL h", View: "table"}
	if err := checkPanel(&panel, p, nil, nil); err == nil || err.Error() != "보드에서는 범위를 정한 읽기만: HGETALL" {
		t.Fatalf("설정 오류: %v", err)
	}
}

func TestBoardRedisIntegration_결과는_패널_행_수에서_자른다(t *testing.T) {
	addr := os.Getenv("DBTUI_TEST_REDIS_ADDR")
	if addr == "" {
		t.Skip("DBTUI_TEST_REDIS_ADDR not set")
	}
	host, port, _ := strings.Cut(addr, ":")
	n, err := strconv.Atoi(port)
	must(t, err)
	c := db.Conn{Name: "r", UUID: "r", Kind: db.KindRedis, Host: host, Port: n, DB: "0"}
	clients := db.NewClients()
	defer clients.Close()
	keys := strings.TrimSpace(strings.Repeat("board:none ", boardRows+10))
	r := runPanel(context.Background(), clients, nil, &db.Project{Conns: []db.Conn{c}}, boardPanel{Conn: "r", Query: "MGET " + keys, View: "table"}, true)
	if r.err != "" || len(r.res.Rows) != boardRows || !r.res.Truncated {
		t.Fatalf("50행에서 자름: %q %d", r.err, len(r.res.Rows))
	}
}

func TestWarnRule_비교와_숫자가_아닌_칸(t *testing.T) {
	cases := []struct {
		rule, cell string
		want       bool
	}{
		{">0", "1", true}, {">0", "0", false}, {">=100", "100", true}, {">=100", "1,234", true},
		{"<5", "4.5", true}, {"<=5", "6", false}, {"=0", "0", true}, {"==0", "1", false}, {"!=0", "3", true},
		{">0", "abc", false}, {">0", "", false},
	}
	for _, c := range cases {
		r, err := parseWarn(c.rule)
		must(t, err)
		if r.hit(c.cell) != c.want {
			t.Fatalf("%s %q: %v", c.rule, c.cell, !c.want)
		}
	}
	var none *warnRule
	if none.hit("100") {
		t.Fatal("조건이 없으면 경고하지 않는다")
	}
}

func TestBoardCols_폭_80_아래_1열_140_아래_2열_그_위_3열(t *testing.T) {
	for w, want := range map[int]int{40: 1, 79: 1, 80: 2, 139: 2, 140: 3, 220: 3} {
		if got := boardCols(w); got != want {
			t.Fatalf("폭 %d: %d열", w, got)
		}
	}
}

func TestBoardRunAll_같은_접속은_차례로_다른_접속은_4개까지_동시에(t *testing.T) {
	var panels []boardPanel
	for i := range 12 {
		panels = append(panels, boardPanel{key: string(rune('a' + i%6))})
	}
	panels = append(panels, boardPanel{key: "a", err: "설정 오류"})
	groups := boardGroups(panels)
	if len(groups) != 6 || len(groups[0]) != 2 || groups[0][1] != 6 {
		t.Fatalf("묶음: %v", groups)
	}
	var mu sync.Mutex
	perKey, total, maxTotal := map[string]int{}, 0, 0
	var overlap []string
	ran := 0
	boardRunAll(groups, func(i int) {
		k := panels[i].key
		mu.Lock()
		perKey[k]++
		total++
		maxTotal = max(maxTotal, total)
		if perKey[k] > 1 {
			overlap = append(overlap, k)
		}
		mu.Unlock()
		time.Sleep(5 * time.Millisecond)
		mu.Lock()
		perKey[k]--
		total--
		ran++
		mu.Unlock()
	})
	if len(overlap) > 0 || maxTotal > boardParallel || maxTotal < 2 || ran != 12 {
		t.Fatalf("겹침 %v · 최대 동시 %d · 돈 수 %d", overlap, maxTotal, ran)
	}
}

func TestRunPanel_비싼_질의는_Force_로_보내지_않고_까닭을_보인다(t *testing.T) {
	f := newFake()
	f.run = func(q source.Query) (*db.Result, error) {
		if !q.Force {
			return nil, source.NeedsConfirm("범위 7일 초과")
		}
		return &db.Result{Cols: []string{"n"}, Rows: [][]string{{"1"}}}, nil
	}
	registerFake(t, f)
	p := boardProject(t)
	sources := newSourcePool()
	defer sources.close()
	panel := boardPanel{Title: "로그", Conn: "logs-stg", Query: `{app="a"}`, View: "chart", since: 8 * 24 * time.Hour}
	r := runPanel(context.Background(), nil, sources, p, panel, true)
	runs, _, _ := f.snapshot()
	if r.err != "비싼 질의라 보드에서 돌리지 않음: 범위 7일 초과" || len(runs) != 1 || runs[0].Force {
		t.Fatalf("%+v %+v", r, runs)
	}
	if runs[0].Since != 8*24*time.Hour || runs[0].Limit != sourceDefaultRows {
		t.Fatalf("범위·한도: %+v", runs[0])
	}
	f.run = func(source.Query) (*db.Result, error) { return nil, source.ErrWrite }
	if r := runPanel(context.Background(), nil, sources, p, boardPanel{Conn: "logs-stg", Query: "x", View: "table"}, true); r.err != msgReadOnlySource {
		t.Fatalf("쓰기 거절: %+v", r)
	}
}

func TestRunPanel_흐름_패널도_15초_제한_안에서_푼다(t *testing.T) {
	fakeFlow(t, flows.Stat{}, sampleTopic(), nil)
	var left time.Duration
	flowTopic = func(ctx context.Context, _ *sourcePool, _ db.Conn, _ string) (*awsres.Topic, error) {
		if d, ok := ctx.Deadline(); ok {
			left = time.Until(d)
		}
		return sampleTopic(), nil
	}
	runPanel(context.Background(), nil, nil, boardProject(t), boardPanel{View: "flow", flow: flows.Flow{Name: "x", AWS: "aws-stg", Topic: "t"}}, true)
	if left <= 0 || left > boardTimeout {
		t.Fatalf("흐름 풀이에 패널 제한이 걸린다: %v", left)
	}
}

// ─── 화면 ───

// fakeBoardRun 은 패널을 돌리지 않고 제목을 값으로 돌려준다. 돈 패널 제목을 차례로 적는다.
func fakeBoardRun(t *testing.T) *[]string {
	t.Helper()
	var mu sync.Mutex
	var ran []string
	oldR, oldW := boardRun, boardWait
	boardRun = func(_ context.Context, _ *db.Clients, _ *sourcePool, _ *db.Project, p boardPanel, prod bool) panelResult {
		mu.Lock()
		ran = append(ran, p.Title)
		mu.Unlock()
		if !prod {
			t.Error("화면은 prod 패널도 돌린다")
		}
		return panelResult{res: &db.Result{Cols: []string{"v"}, Rows: [][]string{{"7"}}}, at: time.Now(), elapsed: 12 * time.Millisecond}
	}
	boardWait = func(time.Duration) time.Duration { return time.Millisecond }
	t.Cleanup(func() { boardRun, boardWait = oldR, oldW })
	return &ran
}

// feedBoard 는 타이머 메시지를 빼고 나온 메시지를 다시 넣는다(패널 차례 실행을 따라간다).
func feedBoard(m *model, cmd tea.Cmd) {
	queue := []tea.Cmd{cmd}
	for len(queue) > 0 {
		c := queue[0]
		queue = queue[1:]
		if c == nil {
			continue
		}
		switch msg := c().(type) {
		case nil, tickMsg, lockTickMsg, boardTickMsg, flowTickMsg:
		case tea.BatchMsg:
			queue = append(queue, msg...)
		default:
			_, next := m.Update(msg)
			queue = append(queue, next)
		}
	}
}

const opsBoard = `{"title":"shop 운영","panels":[
	{"title":"결제","conn":"shop-stg","query":"select count(*) from t","view":"number","warn":">5"},
	{"title":"에러","conn":"logs-main","query":"sum(count_over_time({app=\"a\"}[5m]))","since":"6h","view":"chart"},
	{"title":"흐름","flow":"hub","view":"flow"},
	{"title":"틀림","conn":"shop-stg","query":"update t set a=1","view":"table"}
]}`

func boardModel(t *testing.T, width int) *model {
	t.Helper()
	p := boardProject(t)
	writeBoard(t, p.Dir, "ops", opsBoard)
	m := newModel(p)
	m.Update(tea.WindowSizeMsg{Width: width, Height: 50})
	t.Cleanup(func() { m.stop(); m.sources.close() })
	return m
}

func TestBoard_AltB_는_보드를_골라_열고_패널을_돌려_격자에_보인다(t *testing.T) {
	ran := fakeBoardRun(t)
	m := boardModel(t, 220)
	_, cmd := m.Update(tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("b"), Alt: true})
	if cmd != nil || m.overlay != oPicker || strings.Join(m.pickerItems, ",") != "ops" {
		t.Fatalf("고르기 창: %v %v", m.overlay, m.pickerItems)
	}
	_, cmd = m.Update(tea.KeyMsg{Type: tea.KeyEnter})
	feedBoard(m, cmd)
	tab := m.cur()
	if tab.board == nil || tab.title != "shop 운영" || len(*ran) != 3 || len(tab.board.pending) != 0 {
		t.Fatalf("보드 탭: %+v 돈 패널 %v", tab.board, *ran)
	}
	v := ansi.Strip(m.View())
	for _, want := range []string{"결제 · shop-stg", "에러 · logs-main prod", "흐름 · 흐름 hub", "설정 오류: query is not read-only", "12ms", "보드 · 패널 4 · 1m0s마다"} {
		if !strings.Contains(v, want) {
			t.Fatalf("화면에 %q 가 없다:\n%s", want, v)
		}
	}
	if strings.Contains(v, "shop-stg prod") {
		t.Fatal("stg 패널에는 prod 표시가 없다")
	}
	// 같은 보드를 다시 고르면 탭을 새로 만들지 않고 다시 돌린다
	n := len(m.tabs)
	feedBoard(m, m.openBoard("ops"))
	if len(m.tabs) != n || len(*ran) != 6 {
		t.Fatalf("다시 열기: 탭 %d 돈 %d", len(m.tabs), len(*ran))
	}
}

func TestBoard_패널_시각은_머신_시각대와_상관없이_KST_다(t *testing.T) {
	fakeBoardRun(t)
	boardRun = func(context.Context, *db.Clients, *sourcePool, *db.Project, boardPanel, bool) panelResult {
		return panelResult{res: &db.Result{Cols: []string{"v"}, Rows: [][]string{{"7"}}}, at: time.Date(2026, 11, 16, 3, 4, 5, 0, time.UTC)}
	}
	old := time.Local
	t.Cleanup(func() { time.Local = old })
	seoul, err := time.LoadLocation("Asia/Seoul")
	must(t, err)
	for _, loc := range []*time.Location{time.UTC, seoul} {
		time.Local = loc
		m := boardModel(t, 220)
		feedBoard(m, m.openBoard("ops"))
		if v := ansi.Strip(m.View()); !strings.Contains(v, "12:04:05 · 0ms") {
			t.Fatalf("%s 에서 KST 12:04:05 가 없다:\n%s", loc, v)
		}
	}
}

func TestBoard_보드가_없으면_안내하고_좁으면_1열이다(t *testing.T) {
	fakeBoardRun(t)
	m := newModel(&db.Project{Dir: t.TempDir(), Mapping: map[string]string{}})
	m.Update(tea.WindowSizeMsg{Width: 100, Height: 40})
	t.Cleanup(func() { m.stop(); m.sources.close() })
	m.Update(tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("b"), Alt: true})
	if m.overlay != oNone || m.status != boardNoFile {
		t.Fatalf("보드 없음: %v %q", m.overlay, m.status)
	}

	bm := boardModel(t, 100) // 트리를 빼면 보드 영역은 80칸 아래라 1열
	feedBoard(bm, bm.openBoard("ops"))
	if c := boardCols(bm.layout().rightW); c != 1 {
		t.Fatalf("열 수 %d", c)
	}
	// 1열이면 ↓ 는 다음 패널이다
	bm.Update(tea.KeyMsg{Type: tea.KeyDown})
	if bm.cur().board.cursor != 1 {
		t.Fatalf("커서 %d", bm.cur().board.cursor)
	}
}

func TestBoard_숫자_패널은_warn_조건에_맞으면_경고_색이다(t *testing.T) {
	old := lipgloss.ColorProfile()
	lipgloss.SetColorProfile(termenv.ANSI256)
	t.Cleanup(func() { lipgloss.SetColorProfile(old) })
	p := boardPanel{View: "number", warn: &warnRule{op: ">", v: 5}}
	r := panelResult{res: &db.Result{Cols: []string{"n"}, Rows: [][]string{{"7"}}}, at: time.Now()}
	if got := panelBody(p, r, 20, 2)[0]; got != dashWarnStyle.Render("7") {
		t.Fatalf("경고: %q", got)
	}
	r.res.Rows[0][0] = "3"
	if got := panelBody(p, r, 20, 2)[0]; got != boardNumber.Render("3") || got == dashWarnStyle.Render("3") {
		t.Fatalf("보통: %q", got)
	}
	r.res.Rows = nil
	if got := ansi.Strip(panelBody(p, r, 20, 2)[0]); got != "행 없음" {
		t.Fatalf("빈 결과: %q", got)
	}
}

func TestBoard_방향키로_패널을_고르고_Enter_는_질의를_새_쿼리_탭에서_실행한다(t *testing.T) {
	fakeBoardRun(t)
	f := newFake()
	registerFake(t, f)
	m := boardModel(t, 220) // 3열
	feedBoard(m, m.openBoard("ops"))
	m.Update(tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("l")})
	if m.cur().board.cursor != 1 {
		t.Fatalf("l 은 오른쪽: %d", m.cur().board.cursor)
	}
	m.Update(tea.KeyMsg{Type: tea.KeyDown}) // 아래 줄에 3번 패널이 없어(4번만) 그대로
	if m.cur().board.cursor != 1 {
		t.Fatalf("아래에 패널이 없으면 그대로: %d", m.cur().board.cursor)
	}
	boardTab := m.cur()
	_, cmd := m.Update(tea.KeyMsg{Type: tea.KeyEnter})
	drive(m, cmd)
	nt := m.cur()
	if nt == boardTab || nt.title != "에러" || nt.editor.Value() != `sum(count_over_time({app="a"}[5m]))` || m.conn(nt.conn).Name != "logs-main" || nt.since != 6*time.Hour {
		t.Fatalf("쿼리 탭: %q %q %d %v", nt.title, nt.editor.Value(), nt.conn, nt.since)
	}
	runs, _, _ := f.snapshot()
	if len(runs) != 1 || runs[0].Since != 6*time.Hour || runs[0].Force {
		t.Fatalf("실행: %+v", runs)
	}

	// 흐름 패널 Enter 는 흐름 탭에서 그 흐름을 고르고, 설정 오류 패널은 안내만 한다
	fakeFlow(t, flows.Stat{}, sampleTopic(), nil)
	m.switchTab(len(m.tabs) - 2)
	m.setFocus(fResult)
	m.cur().board.cursor = 2
	_, cmd = m.Update(tea.KeyMsg{Type: tea.KeyEnter})
	feedFlow(m, cmd)
	if m.cur().flow == nil || m.cur().flow.list[m.cur().flow.cursor].Name != "hub" {
		t.Fatal("흐름 탭에서 hub 를 고른다")
	}
	m.switchTab(len(m.tabs) - 3)
	m.setFocus(fResult)
	m.cur().board.cursor = 3
	n := len(m.tabs)
	m.Update(tea.KeyMsg{Type: tea.KeyEnter})
	if len(m.tabs) != n || !strings.HasPrefix(m.status, "설정 오류: query is not read-only") {
		t.Fatalf("설정 오류 패널: %d %q", len(m.tabs), m.status)
	}
}

func TestBoard_활성일_때만_다시_돌고_닫으면_진행_중인_회차를_취소한다(t *testing.T) {
	ran := fakeBoardRun(t)
	m := boardModel(t, 220)
	feedBoard(m, m.openBoard("ops"))
	tab := m.cur()
	gen := tab.board.gen
	m.newTab() // 다른 탭이 활성
	feedBoard(m, m.boardTicked(boardTickMsg{tab: tab.id, gen: gen}))
	if len(*ran) != 3 || tab.board.gen != gen {
		t.Fatalf("다른 탭일 때 돌았다: %v", *ran)
	}
	m.switchTab(0)
	for i, x := range m.tabs {
		if x == tab {
			m.switchTab(i)
		}
	}
	feedBoard(m, m.boardTicked(boardTickMsg{tab: tab.id, gen: gen}))
	if len(*ran) != 6 || tab.board.gen != gen+1 {
		t.Fatalf("활성일 때 다시 돈다: %d", len(*ran))
	}
	ctx := tab.board.ctx
	m.closeTab(tab)
	if ctx.Err() == nil {
		t.Fatal("닫으면 회차를 취소한다")
	}
}

func TestBoard_세션에_보드_탭은_남기지_않는다(t *testing.T) {
	fakeBoardRun(t)
	t.Setenv("XDG_STATE_HOME", t.TempDir())
	m := boardModel(t, 220)
	feedBoard(m, m.openBoard("ops"))
	must(t, m.saveSession())
	tabs := savedTabs(t, m)
	if len(tabs) != 1 || tabs[0].Title == "shop 운영" {
		t.Fatalf("보드 탭은 저장하지 않는다: %+v", tabs)
	}
}

// ─── CLI ───

func boardCLIProject(t *testing.T, body string) string {
	t.Helper()
	dir := sourceCLIProject(t)
	writeBoard(t, dir, "ops", body)
	return dir
}

const cliBoard = `{"title":"운영","panels":[
	{"title":"건수","conn":"logs-stg","query":"count","view":"number","warn":">0"},
	{"title":"표","conn":"logs-stg","query":"rows","view":"table"},
	{"title":"prod 로그","conn":"logs-prod","query":"count","view":"number"}
]}`

func TestCLI_board_json_은_패널마다_값과_행이고_prod_는_건너뛴다(t *testing.T) {
	f := newFake()
	f.run = func(q source.Query) (*db.Result, error) {
		if q.Text == "count" {
			return &db.Result{Cols: []string{"n"}, Rows: [][]string{{"3"}}}, nil
		}
		res := &db.Result{Cols: []string{"id"}}
		for range 80 {
			res.Rows = append(res.Rows, []string{"x"})
		}
		return res, nil
	}
	registerFake(t, f)
	dir := boardCLIProject(t, cliBoard)
	code, out, errw := runCLITest(t, "", "board", "ops", "-p", dir, "--format", "json")
	if code != exitOK {
		t.Fatalf("%d %q", code, errw)
	}
	var rows []map[string]any
	must(t, json.Unmarshal([]byte(out), &rows))
	if len(rows) != 3 || rows[0]["value"] != "3" || rows[0]["warn"] != true || rows[0]["conn"] != "logs-stg" {
		t.Fatalf("숫자: %v", rows)
	}
	if r := rows[1]; len(r["rows"].([]any)) != boardRows || r["truncated"] != true || len(r["columns"].([]any)) != 1 {
		t.Fatalf("표는 50행까지: %v", r)
	}
	if r := rows[2]; r["note"] != dashProdHint || r["prod"] != true || r["value"] != nil {
		t.Fatalf("prod 건너뜀: %v", r)
	}
	if runs, _, _ := f.snapshot(); len(runs) != 2 {
		t.Fatalf("prod 패널은 묻지 않는다: %d", len(runs))
	}
	code, out, _ = runCLITest(t, "", "board", "ops", "-p", dir, "--prod")
	if code != exitOK || !strings.Contains(out, "prod 로그\tlogs-prod\tnumber\t3\t") || !strings.Contains(out, "건수\tlogs-stg\tnumber\t3 !\t") {
		t.Fatalf("--prod tsv: %d %q", code, out)
	}
}

func TestCLI_board_실패한_패널이_있으면_1_없는_보드는_2_이름이_없으면_목록이다(t *testing.T) {
	f := newFake()
	f.run = func(q source.Query) (*db.Result, error) { return nil, errors.New("boom") }
	registerFake(t, f)
	dir := boardCLIProject(t, `{"panels":[
		{"title":"a","conn":"logs-stg","query":"x","view":"number"},
		{"title":"b","conn":"app-stg","query":"drop table t","view":"table"}
	]}`)
	code, out, _ := runCLITest(t, "", "board", "ops", "-p", dir, "--format", "json")
	var rows []map[string]any
	must(t, json.Unmarshal([]byte(out), &rows))
	if code != exitErr || rows[0]["error"] != "boom" || !strings.HasPrefix(rows[1]["error"].(string), "설정 오류: query is not read-only") {
		t.Fatalf("%d %v", code, rows)
	}
	if code, _, errw := runCLITest(t, "", "board", "nope", "-p", dir); code != exitUsage || !strings.Contains(errw, "보드 없음: nope") {
		t.Fatalf("없는 보드: %d %q", code, errw)
	}
	if code, _, _ := runCLITest(t, "", "board", "ops", "-p", dir, "--format", "csv"); code != exitUsage {
		t.Fatalf("csv: %d", code)
	}
	code, out, _ = runCLITest(t, "", "board", "-p", dir)
	if code != exitOK || out != "name\ttitle\tpanels\terror\nops\tops\t2\t\n" {
		t.Fatalf("목록: %d %q", code, out)
	}
}

// ─── 통합 (DBTUI_TEST_PG_URL 이 있을 때만) ───

func TestBoardIntegration_PG_패널은_READ_ONLY_로_돌고_행을_줄인다(t *testing.T) {
	c := testPGConn(t)
	p := &db.Project{Dir: t.TempDir(), Conns: []db.Conn{c}}
	clients := db.NewClients()
	defer clients.Close()
	ctx := context.Background()
	r := runPanel(ctx, clients, nil, p, boardPanel{Conn: c.Name, Query: "select 42 as n", View: "number"}, true)
	if v, ok := panelValue(r.res); r.err != "" || !ok || v != "42" {
		t.Fatalf("숫자: %+v", r)
	}
	r = runPanel(ctx, clients, nil, p, boardPanel{Conn: c.Name, Query: "select generate_series(1, 200) as i", View: "table"}, true)
	if r.err != "" || len(r.res.Rows) != boardRows {
		t.Fatalf("표는 50행: %v %d", r.err, len(r.res.Rows))
	}
	// 검사가 놓치는 CTE 안 쓰기는 서버가 막는다(커서·READ ONLY)
	r = runPanel(ctx, clients, nil, p, boardPanel{Conn: c.Name, Query: "with d as (delete from pg_class where false returning 1) select count(*) from d", View: "number"}, true)
	if !strings.Contains(r.err, "SQLSTATE") {
		t.Fatalf("READ ONLY: %+v", r)
	}
}
