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

const (
	lokiMaxLimit = 5000 // R62.2 Loki 서버 상한
	openLines    = 100  // R62.1 라벨 값 Enter 는 최근 100줄
)

type loki struct {
	c   *client
	now func() time.Time
}

// NewLoki 는 Loki 접속이다. Auth "basic" 이면 User·Password, "token" 이면 Password 를 Bearer 로 보낸다.
func NewLoki(c db.Conn) (source.Backend, error) {
	cl, err := newClient("loki", c, false)
	if err != nil {
		return nil, err
	}
	return &loki{c: cl, now: time.Now}, nil
}

// Children 은 "" 이면 라벨 이름, "라벨" 이면 그 값(최근 1시간)이다. 값 노드 Path 는 "라벨/값" 이다
// (라벨 이름에는 '/' 가 올 수 없어 첫 '/' 로 가른다).
func (l *loki) Children(ctx context.Context, path string) ([]source.Node, error) {
	end := l.now()
	params := url.Values{"start": {nanos(end.Add(-defaultRange))}, "end": {nanos(end)}}
	if path == "" {
		var names []string
		if err := l.c.getData(ctx, "/loki/api/v1/labels", params, &names); err != nil {
			return nil, err
		}
		nodes := make([]source.Node, 0, len(names))
		for _, n := range names {
			nodes = append(nodes, source.Node{Name: n, Path: n, HasChildren: true})
		}
		return nodes, nil
	}
	if strings.Contains(path, "/") {
		return nil, nil
	}
	var values []string
	if err := l.c.getData(ctx, "/loki/api/v1/label/"+url.PathEscape(path)+"/values", params, &values); err != nil {
		return nil, err
	}
	nodes := make([]source.Node, 0, len(values))
	for _, v := range values {
		nodes = append(nodes, source.Node{Name: v, Path: path + "/" + v, Openable: true})
	}
	return nodes, nil
}

// Open 은 "라벨/값" 을 {라벨="값"} 최근 1시간 로그로 연다. limit 0 이면 100줄이다.
func (l *loki) Open(ctx context.Context, path string, limit int) (*db.Result, error) {
	label, value, ok := strings.Cut(path, "/")
	if !ok || label == "" {
		return nil, fmt.Errorf("loki: open needs label/value, got %q", path)
	}
	return l.Run(ctx, source.Query{Text: "{" + label + "=" + strconv.Quote(value) + "}", Limit: limitOf(limit, openLines, 0)})
}

// Run 은 LogQL 을 query_range 로 보낸다(backward, 최신 먼저). 로그 질의는 서버 limit 으로, 지표 질의는 받은 뒤 행을 자른다.
func (l *loki) Run(ctx context.Context, q source.Query) (*db.Result, error) {
	if strings.TrimSpace(q.Text) == "" {
		return nil, errEmptyQuery
	}
	start, end, err := window(q, l.now())
	if err != nil {
		return nil, err
	}
	limit := limitOf(q.Limit, db.MaxRows, lokiMaxLimit)
	began := time.Now()
	var d queryData
	err = l.c.getData(ctx, "/loki/api/v1/query_range", url.Values{
		"query":     {q.Text},
		"start":     {nanos(start)},
		"end":       {nanos(end)},
		"limit":     {strconv.Itoa(limit)},
		"direction": {"backward"},
		"step":      {step(start, end)},
	}, &d)
	if err != nil {
		return nil, err
	}
	res, err := toResult(d, limit)
	if err != nil {
		return nil, fmt.Errorf("loki: %w", err)
	}
	// 서버가 limit 에서 멈췄으면 더 있을 수 있다
	if d.ResultType == "streams" && len(res.Rows) == limit {
		res.Truncated = true
	}
	res.Elapsed = time.Since(began)
	return res, nil
}

func (l *loki) Close() error {
	l.c.close()
	return nil
}

func nanos(t time.Time) string { return strconv.FormatInt(t.UnixNano(), 10) }
