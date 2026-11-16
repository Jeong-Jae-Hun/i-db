package main

import (
	"context"
	"errors"
	"fmt"
	"net/url"
	"strings"
	"sync"
	"time"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/Jeong-Jae-Hun/i-db/internal/athena"
	"github.com/Jeong-Jae-Hun/i-db/internal/awsres"
	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/dynamo"
	"github.com/Jeong-Jae-Hun/i-db/internal/obs"
	"github.com/Jeong-Jae-Hun/i-db/internal/opensearch"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
)

// ─── 읽기 전용 저장소 (R60~R63) ───
// 화면·CLI 는 source.Backend 만 본다. 종류별 구현은 sourceOpeners 에 등록한 생성기로 연다.

// sourceOpeners 는 종류별 Backend 생성기다. 등록하지 않은 종류는 `<종류> source is not built in` 오류다(테스트가 가짜로 바꿔 끼운다).
var sourceOpeners = map[db.Kind]func(context.Context, db.Conn) (source.Backend, error){
	db.KindOpenSearch: func(_ context.Context, c db.Conn) (source.Backend, error) { return opensearch.New(c) },
	db.KindLoki:       func(_ context.Context, c db.Conn) (source.Backend, error) { return obs.NewLoki(c) },
	db.KindPrometheus: func(_ context.Context, c db.Conn) (source.Backend, error) { return obs.NewPrometheus(c) },
	db.KindGrafana:    func(_ context.Context, c db.Conn) (source.Backend, error) { return obs.NewGrafana(c) },
	db.KindDynamoDB:   dynamo.New,
	db.KindAWS:        awsres.New,
	db.KindAthena:     athena.New,
}

const (
	msgReadOnlySource = "읽기 전용 저장소라 보내지 않음"
	msgExpensive      = "비싼 질의"
	msgUnsupported    = "이 접속 종류에서는 안 됨"
	msgPlainHTTP      = "비밀번호가 평문 http 로 감"
	msgPlainPG        = "TLS 없이 붙으면 비밀번호가 평문으로 감"
	msgForeignSigV4   = "AWS 서명을 AWS 밖 주소로 보냄"
	msgTreeOnly       = "aws 접속은 트리로 본다" // R67.1: 편집기 질의 언어가 없다
)

// sourceRanges 는 Ctrl+] 가 도는 Loki·Prometheus 범위다(R62.2).
var sourceRanges = []time.Duration{15 * time.Minute, time.Hour, 6 * time.Hour, 24 * time.Hour, 7 * 24 * time.Hour}

func newSource(ctx context.Context, c db.Conn) (source.Backend, error) {
	open := sourceOpeners[c.Kind]
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
		return source.LokiOpenLimit
	}
	return source.OpenLimit
}

// rangeLabel 은 범위의 짧은 글이다(15m·1h·24h·7d).
func rangeLabel(d time.Duration) string {
	if d <= 0 {
		d = source.DefaultRange
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
		d = source.DefaultRange
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

// sourceErrText 는 Backend 오류의 보일 글이다. 읽기 전용 거절은 정한 한국어 안내로, 비싼 질의는 까닭을 붙여 바꾼다
// (`비싼 질의: 전체 스캔`, R60.2).
func sourceErrText(err error) string {
	switch {
	case errors.Is(err, source.ErrWrite):
		return msgReadOnlySource
	case errors.Is(err, source.ErrBilled):
		return source.ErrBilled.Error()
	case errors.Is(err, source.ErrNeedsConfirm):
		if reason := source.ConfirmReason(err); reason != "" {
			return msgExpensive + ": " + reason
		}
		return msgExpensive
	}
	return err.Error()
}

// connWarning 은 저장·시험할 때 알릴 저장소 접속 위험이다(R60.1). 비면 없다.
// basic·토큰은 http 면 비밀값이 평문으로 가고, DynamoDB 사용자 주소는 SigV4 서명(자격 증명으로 만든)을 그 주소로 보낸다.
func connWarning(c db.Conn) string {
	// TLS 를 끄거나 실패 시 평문으로 떨어지게 한 원격 PostgreSQL 은 비밀번호가 평문으로 갈 수 있다(R59.1)
	if c.Kind == db.KindPG && (c.SSLMode == "disable" || c.SSLMode == "prefer") && c.Host != "localhost" && c.Host != "127.0.0.1" {
		return msgPlainPG
	}
	u, err := url.Parse(c.URL)
	if !c.Kind.IsSource() || c.URL == "" || err != nil {
		return ""
	}
	host := strings.ToLower(u.Hostname())
	switch {
	case u.Scheme == "http" && (c.Auth == "basic" || c.Auth == "token" || c.Kind == db.KindGrafana):
		return msgPlainHTTP
	case c.Kind == db.KindDynamoDB && host != "localhost" && host != "127.0.0.1" && host != "::1" &&
		host != "amazonaws.com" && !strings.HasSuffix(host, ".amazonaws.com"):
		return msgForeignSigV4
	}
	return ""
}

// testConn 은 접속 창·db conn 의 시험이다. 읽기 전용 저장소는 맨 위 Children("") 가 되면 접속된 것으로 본다.
// 맨 위 목록부터 요금이 붙는 종류(Athena 는 Glue 카탈로그, R72)는 막혀 있으면 목록을 보내지 않고, 요금 없는 대시보드 확인
// (Athena GetWorkGroup)으로 자격 증명만 본다. 확인도 없는 종류는 시험을 건너뛰었다고 알린다.
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
		if errors.Is(err, source.ErrBilled) {
			p, ok := b.(source.Prober)
			if !ok {
				return fmt.Sprintf("%s · 시험 건너뜀 (%s)", connKinds[c.Kind], err), nil
			}
			h := p.Probe(ctx)
			if h.OK {
				return fmt.Sprintf("%s · 목록은 요금이 붙어 시험 안 함", connKinds[c.Kind]), nil
			}
			err = errors.New(h.Err)
		}
	}
	return "", errors.New(db.Redact(err.Error(), c.Password))
}

// sourcePool 은 화면이 쓰는 저장소 Backend 를 접속 uuid 마다 하나씩, 연 때의 접속 값과 함께 들고 있다.
// 명령 고루틴이 같이 써서 잠근다.
type sourcePool struct {
	mu sync.Mutex
	m  map[string]pooledSource
}

// pooledSource 는 Backend 와 그것을 연 접속 값(비밀값 포함)이다. 값이 바뀌면 새로 연다.
type pooledSource struct {
	conn db.Conn
	b    source.Backend
}

func newSourcePool() *sourcePool { return &sourcePool{m: map[string]pooledSource{}} }

// get 은 c 의 Backend 를 열거나 열린 것을 돌려준다. 열린 것이 다른 접속 값(주소·인증·비밀값·프로필)으로 열렸으면
// 닫고 다시 연다: forget 을 거치지 않은 바뀜(.env 를 고친 뒤 다시 읽기 등)에도 옛 비밀값으로 보내지 않는다.
// 여는 동안은 잠그지 않아(DynamoDB 는 자격 증명을 읽는다) 같은 접속을 동시에 열면 늦게 연 것을 닫는다.
func (p *sourcePool) get(ctx context.Context, c db.Conn) (source.Backend, error) {
	p.mu.Lock()
	ps, ok := p.m[c.UUID]
	if ok && ps.conn != c {
		delete(p.m, c.UUID)
		_ = ps.b.Close()
		ok = false
	}
	p.mu.Unlock()
	if ok {
		return ps.b, nil
	}
	nb, err := newSource(ctx, c)
	if err != nil {
		return nil, err
	}
	p.mu.Lock()
	defer p.mu.Unlock()
	if ps, ok := p.m[c.UUID]; ok && ps.conn == c {
		_ = nb.Close()
		return ps.b, nil
	}
	p.m[c.UUID] = pooledSource{conn: c, b: nb}
	return nb, nil
}

// peek 은 c 의 값으로 열려 있는 Backend 다. 없으면 nil 이고 열지 않는다(화면 고루틴이 설정·네트워크를 기다리지 않게).
func (p *sourcePool) peek(c db.Conn) source.Backend {
	p.mu.Lock()
	defer p.mu.Unlock()
	if ps, ok := p.m[c.UUID]; ok && ps.conn == c {
		return ps.b
	}
	return nil
}

// forget 은 고치거나 지운 접속의 Backend 를 닫는다(R59.3).
func (p *sourcePool) forget(uuid string) error {
	p.mu.Lock()
	ps, ok := p.m[uuid]
	delete(p.m, uuid)
	p.mu.Unlock()
	if !ok {
		return nil
	}
	return ps.b.Close()
}

func (p *sourcePool) close() {
	p.mu.Lock()
	defer p.mu.Unlock()
	for k, ps := range p.m {
		_ = ps.b.Close()
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
	if c.Kind == db.KindAWS {
		m.status = msgTreeOnly
		return nil
	}
	q := source.Query{Text: strings.TrimSpace(text)}
	if q.Text == "" {
		m.status = "실행할 문장이 없음"
		return nil
	}
	if usesRange(c.Kind) {
		q.Since = cmpRange(t.since)
	}
	p := pendingRun{tab: t.id, conn: t.conn, readOnly: true, sql: q.Text, stmts: 1, src: &q, exec: m.sourceExec(c, sourceRun(q))}
	forced := q
	forced.Force = true
	retry := p
	retry.exec, retry.src = m.sourceExec(c, sourceRun(forced)), &forced
	p.retry = &retry
	return m.guard(p)
}

// sourceRun 은 Backend.Run(q) 실행이다.
func sourceRun(q source.Query) func(context.Context, source.Backend) (*db.Result, error) {
	return func(ctx context.Context, b source.Backend) (*db.Result, error) { return b.Run(ctx, q) }
}

func cmpRange(d time.Duration) time.Duration {
	if d <= 0 {
		return source.DefaultRange
	}
	return d
}

// openSourceNode 는 트리 노드 Enter 의 미리 보기(Backend.Open)를 결과 표에 연다. 쓰던 글·파일·다른 접속의 화면은 덮지 않게 새 탭에 연다.
// DynamoDB 테이블은 스캔이라 Open 이 ErrNeedsConfirm 을 돌려주면 확인 뒤 `SELECT * FROM "테이블"` 을 Force 로 보낸다(R63.1).
func (m *model) openSourceNode(t *queryTab, n *node) tea.Cmd {
	other := n.conn != t.conn
	if t.file != "" || t.editor.Value() != "" || (other && t.bound()) {
		t = m.newTab()
	}
	c, path, limit := m.conn(n.conn), n.key, openLimit(m.conn(n.conn).Kind)
	p := pendingRun{tab: t.id, conn: n.conn, readOnly: true, retarget: true,
		exec: m.sourceExec(c, func(ctx context.Context, b source.Backend) (*db.Result, error) { return b.Open(ctx, path, limit) })}
	// 미리 보기도 보낸 요청을 Run 꼴 질의로 남겨 정렬·거르기를 서버로 다시 읽을 수 있게 한다(R61.3·R62.3). OpenSearch 는 기록에도 싣는다
	switch c.Kind {
	case db.KindOpenSearch:
		p.sql = opensearch.OpenText(path, limit)
		p.src = &source.Query{Text: p.sql}
	case db.KindLoki, db.KindGrafana:
		if q, ok := obs.OpenQuery(c.Kind, path, limit); ok {
			p.src = &q
		}
	// Athena 파티션 테이블은 확인 뒤 `SELECT * FROM "db"."테이블" LIMIT 50` 을 Force 로 보낸다(R68.1)
	case db.KindDynamoDB, db.KindAthena:
		q := source.Query{Text: dynamoScanSQL(path), Limit: limit, Force: true}
		if c.Kind == db.KindAthena {
			q.Text = athena.OpenSQL(path, limit)
		}
		retry := p
		retry.sql, retry.src = q.Text, &q
		retry.exec = m.sourceExec(c, sourceRun(q))
		p.retry = &retry
	}
	return m.guard(p)
}

// sourceRefused 는 저장소가 보내기 전에 거절한 결과(ErrWrite·ErrBilled·ErrNeedsConfirm)를 처리한다. 모두 서버에 가지 않아 기록에 남기지 않는다.
// 비싼 질의는 `비싼 질의: <까닭>: y/N` 을 묻고 y 면 retry(Force)를 시작한다. 요금 막음(R72)은 확인으로 풀지 않는다.
func (m *model) sourceRefused(msg resultMsg) (tea.Cmd, bool) {
	write := errors.Is(msg.err, source.ErrWrite) || errors.Is(msg.err, source.ErrBilled)
	expensive := errors.Is(msg.err, source.ErrNeedsConfirm)
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
			m.status = msgExpensive + "라 보내지 않음"
			if reason := source.ConfirmReason(msg.err); reason != "" {
				m.status += " (" + reason + ")"
			}
		}
		return nil, true
	}
	if m.overlayBusy() {
		return nil, true
	}
	retry := *msg.retry
	m.confirm(sourceErrText(msg.err), func() tea.Cmd { return m.start(retry) })
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
