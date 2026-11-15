package obs

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/url"
	"regexp"
	"strings"
	"sync"
	"time"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
)

// directiveRe 는 편집기 첫 줄의 데이터 소스 지정이다. LogQL·PromQL 모두 '#' 를 주석으로 받아
// 지시줄을 지우지 않고 그대로 보내도 질의가 깨지지 않는다(그래도 보내기 전에 지운다).
var directiveRe = regexp.MustCompile(`(?i)^\s*#\s*datasource\s*:\s*(.+?)\s*$`)

type datasource struct {
	UID  string `json:"uid"`
	Name string `json:"name"`
	Type string `json:"type"`
}

// grafana 는 Loki·Prometheus 데이터 소스를 /api/datasources/proxy/uid/<uid> 로 부른다.
// 트리 Path 는 "<uid>" 또는 "<uid>/<그 데이터 소스 안 경로>" 다.
type grafana struct {
	c   *client
	now func() time.Time

	mu  sync.Mutex
	ds  []datasource // Loki·Prometheus 만, Grafana 가 준 순서(이름순)
	sub map[string]source.Backend
}

// NewGrafana 는 Grafana 접속이다. Password 에 든 서비스 계정 토큰을 Authorization: Bearer 로 보낸다.
func NewGrafana(c db.Conn) (source.Backend, error) {
	cl, err := newClient("grafana", c, true)
	if err != nil {
		return nil, err
	}
	return &grafana{c: cl, now: time.Now, sub: map[string]source.Backend{}}, nil
}

// datasources 는 목록을 읽어 둔다. refresh 가 아니면 읽어 둔 것을 쓴다.
func (g *grafana) datasources(ctx context.Context, refresh bool) ([]datasource, error) {
	g.mu.Lock()
	defer g.mu.Unlock()
	if g.ds != nil && !refresh {
		return g.ds, nil
	}
	body, err := g.c.get(ctx, "/api/datasources", nil)
	if err != nil {
		return nil, err
	}
	var all []datasource
	if err := json.Unmarshal(body, &all); err != nil {
		return nil, fmt.Errorf("grafana: unexpected datasources response: %w", err)
	}
	g.ds = []datasource{}
	for _, d := range all {
		if d.Type == "loki" || d.Type == "prometheus" {
			g.ds = append(g.ds, d)
		}
	}
	return g.ds, nil
}

// backend 는 데이터 소스 하나로 가는 Loki·Prometheus 백엔드다. 같은 HTTP 연결·토큰을 쓴다.
func (g *grafana) backend(d datasource) source.Backend {
	g.mu.Lock()
	defer g.mu.Unlock()
	if b, ok := g.sub[d.UID]; ok {
		return b
	}
	cl := g.c.sub("grafana "+d.Name, "/api/datasources/proxy/uid/"+url.PathEscape(d.UID))
	var b source.Backend
	if d.Type == "loki" {
		b = &loki{c: cl, now: g.now}
	} else {
		b = &prom{c: cl, now: g.now}
	}
	g.sub[d.UID] = b
	return b
}

// resolve 는 uid 또는 이름이 같은 데이터 소스다. uid 를 먼저 본다.
func (g *grafana) resolve(ctx context.Context, key string) (datasource, error) {
	ds, err := g.datasources(ctx, false)
	if err != nil {
		return datasource{}, err
	}
	for _, d := range ds {
		if d.UID == key {
			return d, nil
		}
	}
	for _, d := range ds {
		if d.Name == key {
			return d, nil
		}
	}
	return datasource{}, fmt.Errorf("grafana: no loki/prometheus datasource %q", key)
}

func (g *grafana) Children(ctx context.Context, path string) ([]source.Node, error) {
	if path == "" {
		ds, err := g.datasources(ctx, true)
		if err != nil {
			return nil, err
		}
		nodes := make([]source.Node, 0, len(ds))
		for _, d := range ds {
			nodes = append(nodes, source.Node{Name: d.Name, Path: d.UID, Info: d.Type, HasChildren: true})
		}
		return nodes, nil
	}
	uid, rest, _ := strings.Cut(path, "/")
	d, err := g.resolve(ctx, uid)
	if err != nil {
		return nil, err
	}
	nodes, err := g.backend(d).Children(ctx, rest)
	for i := range nodes {
		nodes[i].Path = uid + "/" + nodes[i].Path
	}
	return nodes, err
}

func (g *grafana) Open(ctx context.Context, path string, limit int) (*db.Result, error) {
	uid, rest, _ := strings.Cut(path, "/")
	d, err := g.resolve(ctx, uid)
	if err != nil {
		return nil, err
	}
	return g.backend(d).Open(ctx, rest, limit)
}

// Run 은 첫 비어 있지 않은 줄이 "# datasource: <이름|uid>" 면 그 데이터 소스로, 아니면
// 목록의 첫 Loki(없으면 첫 Prometheus) 데이터 소스로 보낸다. 지시줄은 지우고 보낸다.
func (g *grafana) Run(ctx context.Context, q source.Query) (*db.Result, error) {
	key, text := splitDirective(q.Text)
	var (
		d   datasource
		err error
	)
	if key != "" {
		d, err = g.resolve(ctx, key)
	} else {
		d, err = g.defaultDatasource(ctx)
	}
	if err != nil {
		return nil, err
	}
	q.Text = text
	return g.backend(d).Run(ctx, q)
}

func (g *grafana) defaultDatasource(ctx context.Context) (datasource, error) {
	ds, err := g.datasources(ctx, false)
	if err != nil {
		return datasource{}, err
	}
	for _, typ := range []string{"loki", "prometheus"} {
		for _, d := range ds {
			if d.Type == typ {
				return d, nil
			}
		}
	}
	return datasource{}, errors.New("grafana: no loki/prometheus datasource")
}

// splitDirective 는 첫 비어 있지 않은 줄의 지시를 떼어 낸다. 지시가 없으면 key 는 "" 이고 text 는 그대로다.
func splitDirective(text string) (key, rest string) {
	lines := strings.Split(text, "\n")
	for i, ln := range lines {
		if strings.TrimSpace(ln) == "" {
			continue
		}
		m := directiveRe.FindStringSubmatch(ln)
		if m == nil {
			return "", text
		}
		return m[1], strings.Join(lines[i+1:], "\n")
	}
	return "", text
}

func (g *grafana) Close() error {
	g.c.close()
	return nil
}
