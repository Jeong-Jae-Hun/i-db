package obs

import (
	"context"
	"fmt"
	"net/url"
	"strconv"
	"strings"
	"time"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
)

const maxMetricNames = 1000 // R62.1 지표 이름 앞 1,000개

type prom struct {
	c   *client
	now func() time.Time
}

// NewPrometheus 는 Prometheus 접속이다. 인증 규칙은 NewLoki 와 같다.
func NewPrometheus(c db.Conn) (source.Backend, error) {
	cl, err := newClient("prometheus", c, false)
	if err != nil {
		return nil, err
	}
	return &prom{c: cl, now: time.Now}, nil
}

// Children 은 "" 이면 지표 이름 앞 1,000개다. 지표 아래는 없다.
func (p *prom) Children(ctx context.Context, path string) ([]source.Node, error) {
	if path != "" {
		return nil, nil
	}
	var names []string
	// limit 은 Prometheus 2.47+ 만 따르므로 받은 뒤에도 자른다
	if err := p.c.getData(ctx, "/api/v1/label/__name__/values", url.Values{"limit": {strconv.Itoa(maxMetricNames)}}, &names); err != nil {
		return nil, err
	}
	names = names[:min(len(names), maxMetricNames)]
	nodes := make([]source.Node, 0, len(names))
	for _, n := range names {
		nodes = append(nodes, source.Node{Name: n, Path: n, Openable: true})
	}
	return nodes, nil
}

// Open 은 지표의 즉시값(vector)이다.
func (p *prom) Open(ctx context.Context, path string, limit int) (*db.Result, error) {
	if path == "" {
		return nil, fmt.Errorf("prometheus: open needs a metric name")
	}
	return p.query(ctx, "/api/v1/query", url.Values{"query": {path}, "time": {secs(p.now())}}, limitOf(limit, db.MaxRows, 0))
}

// Run 은 PromQL 을 늘 범위 질의(query_range)로 보낸다. Since 가 0 이면 최근 1시간, step 은 범위/250(최소 15초)이다.
// 즉시값은 트리 Enter(Open)로 본다. 범위 결과라 차트(R28)로 바로 그릴 수 있다.
func (p *prom) Run(ctx context.Context, q source.Query) (*db.Result, error) {
	if strings.TrimSpace(q.Text) == "" {
		return nil, errEmptyQuery
	}
	start, end, err := window(q, p.now())
	if err != nil {
		return nil, err
	}
	return p.query(ctx, "/api/v1/query_range", url.Values{
		"query": {q.Text},
		"start": {secs(start)},
		"end":   {secs(end)},
		"step":  {step(start, end)},
	}, limitOf(q.Limit, db.MaxRows, 0))
}

func (p *prom) query(ctx context.Context, path string, params url.Values, limit int) (*db.Result, error) {
	began := time.Now()
	var d queryData
	if err := p.c.getData(ctx, path, params, &d); err != nil {
		return nil, err
	}
	res, err := toResult(d, limit)
	if err != nil {
		return nil, fmt.Errorf("prometheus: %w", err)
	}
	res.Elapsed = time.Since(began)
	return res, nil
}

func (p *prom) Close() error {
	p.c.close()
	return nil
}

func secs(t time.Time) string { return strconv.FormatFloat(float64(t.UnixMilli())/1000, 'f', 3, 64) }
