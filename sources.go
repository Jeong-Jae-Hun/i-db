package main

import (
	"context"
	"errors"
	"fmt"
	"strings"
	"sync"
	"time"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
)

// ─── 읽기 전용 저장소 (R60~R63) ───
// 화면·CLI 는 source.Backend 만 본다. 종류별 구현은 openSource 에 등록한 생성기로 연다.

// openSource 는 종류별 Backend 생성기다. 등록하지 않은 종류는 `<종류> source is not built in` 오류다.
// 각 저장소 패키지를 합칠 때 sources_register.go 같은 곳에서 채운다. 예:
//
//	openSource[db.KindOpenSearch] = func(_ context.Context, c db.Conn) (source.Backend, error) { return opensearch.New(c) }
//	openSource[db.KindDynamoDB] = dynamo.New
var openSource = map[db.Kind]func(context.Context, db.Conn) (source.Backend, error){}

const (
	msgReadOnlySource = "읽기 전용 저장소라 보내지 않음"
	msgExpensive      = "비싼 질의"
	msgUnsupported    = "이 접속 종류에서는 안 됨"

	// 트리 Enter 미리 보기 행 수. Loki 는 최근 100줄, 나머지는 50개다(R61.1·R62.1·R63.1)
	sourceOpenLimit = 50
	lokiOpenLimit   = 100
	// R62.2 기본 범위
	defaultSourceRange = time.Hour
)

// sourceRanges 는 Ctrl+] 가 도는 Loki·Prometheus 범위다(R62.2).
var sourceRanges = []time.Duration{15 * time.Minute, time.Hour, 6 * time.Hour, 24 * time.Hour, 7 * 24 * time.Hour}

func newSource(ctx context.Context, c db.Conn) (source.Backend, error) {
	open := openSource[c.Kind]
	if open == nil {
		return nil, fmt.Errorf("%s source is not built in", c.Kind)
	}
	return open(ctx, c)
}

// usesRange 는 편집기 실행에 범위(Since)를 싣는 종류다. Grafana 는 Loki·Prometheus 데이터 소스를 거쳐 보내 같이 쓴다.
func usesRange(k db.Kind) bool {
	return k == db.KindLoki || k == db.KindPrometheus || k == db.KindGrafana
}

func openLimit(k db.Kind) int {
	if k == db.KindLoki {
		return lokiOpenLimit
	}
	return sourceOpenLimit
}

// rangeLabel 은 범위의 짧은 글이다(15m·1h·24h·7d).
func rangeLabel(d time.Duration) string {
	if d <= 0 {
		d = defaultSourceRange
	}
	switch {
	case d%(24*time.Hour) == 0 && d >= 48*time.Hour:
		return fmt.Sprintf("%dd", d/(24*time.Hour))
	case d%time.Hour == 0:
		return fmt.Sprintf("%dh", d/time.Hour)
	case d%time.Minute == 0:
		return fmt.Sprintf("%dm", d/time.Minute)
	}
	return d.String()
}

// nextRange 는 sourceRanges 에서 d 다음 범위다. 목록에 없으면 기본(1h) 다음이 아니라 처음부터 돈다.
func nextRange(d time.Duration) time.Duration {
	if d <= 0 {
		d = defaultSourceRange
	}
	for i, r := range sourceRanges {
		if r == d {
			return sourceRanges[(i+1)%len(sourceRanges)]
		}
	}
	return sourceRanges[0]
}

// dynamoScanSQL 은 DynamoDB 테이블 Enter 가 확인 뒤 보내는 전체 스캔이다(R63.1). 경로의 첫 마디가 테이블이다.
func dynamoScanSQL(path string) string {
	table, _, _ := strings.Cut(path, "/")
	return `SELECT * FROM "` + strings.ReplaceAll(table, `"`, `""`) + `"`
}

// sourceErrText 는 Backend 오류의 보일 글이다. 읽기 전용 거절은 정한 한국어 안내로 바꾼다.
func sourceErrText(err error) string {
	switch {
	case errors.Is(err, source.ErrWrite):
		return msgReadOnlySource
	case errors.Is(err, source.ErrNeedsConfirm):
		return msgExpensive
	}
	return err.Error()
}

// testConn 은 접속 창·db conn 의 시험이다. 읽기 전용 저장소는 맨 위 Children("") 가 되면 접속된 것으로 본다.
// 오류에 비밀값이 나오지 않게 지운다.
func testConn(ctx context.Context, c db.Conn) (string, error) {
	if !c.Kind.IsSource() {
		return db.TestConn(ctx, c)
	}
	b, err := newSource(ctx, c)
	if err == nil {
		defer b.Close()
		var nodes []source.Node
		if nodes, err = b.Children(ctx, ""); err == nil {
			return fmt.Sprintf("%s · %d개", connKinds[c.Kind], len(nodes)), nil
		}
	}
	return "", errors.New(db.Redact(err.Error(), c.Password))
}

// sourcePool 은 화면이 쓰는 저장소 Backend 를 접속 uuid 마다 하나씩 들고 있다. 명령 고루틴이 같이 써서 잠근다.
type sourcePool struct {
	mu sync.Mutex
	m  map[string]source.Backend
}

func newSourcePool() *sourcePool { return &sourcePool{m: map[string]source.Backend{}} }

// get 은 c 의 Backend 를 열거나 열린 것을 돌려준다. 여는 동안은 잠그지 않아(DynamoDB 는 자격 증명을 읽는다)
// 같은 접속을 동시에 열면 늦게 연 것을 닫는다.
func (p *sourcePool) get(ctx context.Context, c db.Conn) (source.Backend, error) {
	p.mu.Lock()
	b := p.m[c.UUID]
	p.mu.Unlock()
	if b != nil {
		return b, nil
	}
	nb, err := newSource(ctx, c)
	if err != nil {
		return nil, err
	}
	p.mu.Lock()
	defer p.mu.Unlock()
	if b := p.m[c.UUID]; b != nil {
		_ = nb.Close()
		return b, nil
	}
	p.m[c.UUID] = nb
	return nb, nil
}

// forget 은 고치거나 지운 접속의 Backend 를 닫는다(R59.3).
func (p *sourcePool) forget(uuid string) error {
	p.mu.Lock()
	b := p.m[uuid]
	delete(p.m, uuid)
	p.mu.Unlock()
	if b == nil {
		return nil
	}
	return b.Close()
}

func (p *sourcePool) close() {
	p.mu.Lock()
	defer p.mu.Unlock()
	for k, b := range p.m {
		_ = b.Close()
		delete(p.m, k)
	}
}

// ─── 화면 ───

// sourceExec 는 화면 풀의 Backend 로 do 를 돌리는 실행이다.
func (m *model) sourceExec(c db.Conn, do func(ctx context.Context, b source.Backend) (*db.Result, error)) func(context.Context) (*db.Result, error) {
	sources := m.sources
	return func(ctx context.Context) (*db.Result, error) {
		b, err := sources.get(ctx, c)
		if err != nil {
			return nil, err
		}
		return do(ctx, b)
	}
}

// runSource 는 편집기 실행(Ctrl+R)을 저장소 Backend.Run 으로 보낸다(R60.3). 읽기 전용이라 prod 확인·잠금(R6·R54)은 없고,
// 비싼 질의(ErrNeedsConfirm)는 확인 뒤 Force 로 다시 보낸다. 기록에는 질의 글이 남는다.
func (m *model) runSource(t *queryTab, text string) tea.Cmd {
	c := m.conn(t.conn)
	q := source.Query{Text: strings.TrimSpace(text)}
	if q.Text == "" {
		m.status = "실행할 문장이 없음"
		return nil
	}
	if usesRange(c.Kind) {
		q.Since = cmpRange(t.since)
	}
	run := func(q source.Query) func(context.Context) (*db.Result, error) {
		return m.sourceExec(c, func(ctx context.Context, b source.Backend) (*db.Result, error) { return b.Run(ctx, q) })
	}
	p := pendingRun{tab: t.id, conn: t.conn, readOnly: true, sql: q.Text, stmts: 1, exec: run(q)}
	forced := q
	forced.Force = true
	retry := p
	retry.exec = run(forced)
	p.retry = &retry
	return m.guard(p)
}

func cmpRange(d time.Duration) time.Duration {
	if d <= 0 {
		return defaultSourceRange
	}
	return d
}

// openSource 는 트리 노드 Enter 의 미리 보기(Backend.Open)를 결과 표에 연다. 쓰던 글·파일·다른 접속의 화면은 덮지 않게 새 탭에 연다.
// DynamoDB 테이블은 스캔이라 Open 이 ErrNeedsConfirm 을 돌려주면 확인 뒤 `SELECT * FROM "테이블"` 을 Force 로 보낸다(R63.1).
func (m *model) openSource(t *queryTab, n *node) tea.Cmd {
	other := n.conn != t.conn
	if t.file != "" || t.editor.Value() != "" || (other && t.bound()) {
		t = m.newTab()
	}
	c, path, limit := m.conn(n.conn), n.key, openLimit(m.conn(n.conn).Kind)
	p := pendingRun{tab: t.id, conn: n.conn, readOnly: true, retarget: true,
		exec: m.sourceExec(c, func(ctx context.Context, b source.Backend) (*db.Result, error) { return b.Open(ctx, path, limit) })}
	if c.Kind == db.KindDynamoDB {
		q := source.Query{Text: dynamoScanSQL(path), Limit: limit, Force: true}
		retry := p
		retry.sql = q.Text
		retry.exec = m.sourceExec(c, func(ctx context.Context, b source.Backend) (*db.Result, error) { return b.Run(ctx, q) })
		p.retry = &retry
	}
	return m.guard(p)
}

// sourceRefused 는 저장소가 보내기 전에 거절한 결과(ErrWrite·ErrNeedsConfirm)를 처리한다. 둘 다 서버에 가지 않아 기록에 남기지 않는다.
// 비싼 질의는 `비싼 질의: y/N` 을 묻고 y 면 retry(Force)를 시작한다.
func (m *model) sourceRefused(msg resultMsg) (tea.Cmd, bool) {
	write, expensive := errors.Is(msg.err, source.ErrWrite), errors.Is(msg.err, source.ErrNeedsConfirm)
	if !write && !expensive {
		return nil, false
	}
	t := m.findTab(msg.tab)
	if t == nil || msg.seq != t.seq {
		return nil, true
	}
	t.running = nil
	if write || msg.retry == nil {
		m.status = sourceErrText(msg.err)
		if expensive {
			m.status += "라 보내지 않음"
		}
		return nil, true
	}
	if m.overlayBusy() {
		return nil, true
	}
	retry := *msg.retry
	m.confirm(msgExpensive, func() tea.Cmd { return m.start(retry) })
	return nil, true
}

// cycleRange 는 Ctrl+] 로 탭의 Loki·Prometheus 범위를 15m·1h·6h·24h·7d 로 돌린다(R62.2).
func (m *model) cycleRange(t *queryTab) {
	if t.conn < 0 || !usesRange(m.conn(t.conn).Kind) {
		m.status = "범위는 Loki·Prometheus·Grafana 탭에서만 바꿈"
		return
	}
	t.since = nextRange(t.since)
	m.status = "범위: 최근 " + rangeLabel(t.since)
}
