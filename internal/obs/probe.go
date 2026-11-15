package obs

import (
	"context"
	"encoding/json"
	"fmt"
	"net/url"
	"strconv"
	"strings"
	"time"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
	"github.com/Jeong-Jae-Hun/i-db/internal/textutil"
)

// 대시보드 확인(R64.2)이다. 모두 GET 이고 지연은 첫 요청(상태 확인) 왕복이다.
// 버전(buildinfo)은 프록시·옛 서버에 없을 수 있어 못 읽어도 실패로 보지 않는다.

var (
	_ source.Prober = (*loki)(nil)
	_ source.Prober = (*prom)(nil)
	_ source.Prober = (*grafana)(nil)
)

// Probe 는 `/ready` 와 최근 1시간 라벨 수다. 준비되지 않았으면 Loki 가 503 을 줘 실패다. 예: `ready · 라벨 13`.
func (l *loki) Probe(ctx context.Context) db.Health {
	start := time.Now()
	_, err := l.c.get(ctx, "/ready", nil)
	latency := time.Since(start)
	if err != nil {
		return db.Failed(latency, err, l.c.secret)
	}
	end := l.now()
	var names []string
	params := url.Values{"start": {nanos(end.Add(-source.DefaultRange))}, "end": {nanos(end)}}
	if err := l.c.getData(ctx, "/loki/api/v1/labels", params, &names); err != nil {
		return db.Failed(latency, err, l.c.secret)
	}
	return db.Health{OK: true, Latency: latency, Version: l.c.buildVersion(ctx, "/loki/api/v1/status/buildinfo", "Loki"), Metrics: []db.Metric{
		{Value: "ready"},
		{Name: "라벨", Value: textutil.KoreanCount(int64(len(names)))},
	}}
}

// Probe 는 즉시 질의 `count(up)`·`count(up == 0)` 이다. down 대상이 있으면 경고다. 예: `up 372/380`.
func (p *prom) Probe(ctx context.Context) db.Health {
	start := time.Now()
	total, err := p.count(ctx, "count(up)")
	latency := time.Since(start)
	if err != nil {
		return db.Failed(latency, err, p.c.secret)
	}
	down, err := p.count(ctx, "count(up == 0)")
	if err != nil {
		return db.Failed(latency, err, p.c.secret)
	}
	return db.Health{OK: true, Latency: latency, Version: p.c.buildVersion(ctx, "/api/v1/status/buildinfo", "Prometheus"), Metrics: []db.Metric{
		{Name: "up", Value: fmt.Sprintf("%d/%d", total-down, total), Warn: down > 0},
	}}
}

// count 는 count(...) 즉시 질의의 값이다. 맞는 시계열이 없으면 count 는 빈 vector 라 0 이다.
func (p *prom) count(ctx context.Context, query string) (int64, error) {
	var d struct {
		Result []series `json:"result"`
	}
	if err := p.c.getData(ctx, "/api/v1/query", url.Values{"query": {query}, "time": {secs(p.now())}}, &d); err != nil {
		return 0, err
	}
	if len(d.Result) == 0 || d.Result[0].Value == nil {
		return 0, nil
	}
	v := d.Result[0].Value.val
	n, err := strconv.ParseFloat(v, 64)
	if err != nil {
		return 0, fmt.Errorf("prometheus: %s: unexpected value %q", query, v)
	}
	return int64(n), nil
}

// Probe 는 `/api/health` 의 database 상태와 버전이다. database 가 `ok` 가 아니면 경고다. 예: `ok · 11.4.0`.
func (g *grafana) Probe(ctx context.Context) db.Health {
	start := time.Now()
	body, err := g.c.get(ctx, "/api/health", nil)
	latency := time.Since(start)
	if err != nil {
		return db.Failed(latency, err, g.c.secret)
	}
	var h struct{ Database, Version string }
	if err := json.Unmarshal(body, &h); err != nil || h.Database == "" {
		return db.Failed(latency, fmt.Errorf("grafana: unexpected /api/health response"), g.c.secret)
	}
	metrics := []db.Metric{{Value: h.Database, Warn: h.Database != "ok"}}
	version := ""
	if h.Version != "" {
		metrics = append(metrics, db.Metric{Value: h.Version})
		version = "Grafana " + h.Version
	}
	return db.Health{OK: true, Latency: latency, Version: version, Metrics: metrics}
}

// buildVersion 은 buildinfo 의 version 에 이름을 붙인다. Prometheus 는 봉투({"status","data"}) 안에,
// Loki 는 맨 위에 있다. 못 읽으면 빈 글이다.
func (c *client) buildVersion(ctx context.Context, path, name string) string {
	body, err := c.get(ctx, path, nil)
	if err != nil {
		return ""
	}
	var b struct {
		Version string `json:"version"`
		Data    struct {
			Version string `json:"version"`
		} `json:"data"`
	}
	if json.Unmarshal(body, &b) != nil {
		return ""
	}
	if v := strings.TrimSpace(b.Version + b.Data.Version); v != "" {
		return name + " " + v
	}
	return ""
}
