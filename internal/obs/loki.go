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

const lokiMaxLimit = 5000 // R62.2 Loki 서버 상한

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
	params := url.Values{"start": {nanos(end.Add(-source.DefaultRange))}, "end": {nanos(end)}}
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
	q, ok := lokiOpenQuery(path, limit)
	if !ok {
		return nil, fmt.Errorf("loki: open needs label/value, got %q", path)
	}
	return l.Run(ctx, q)
}

func lokiOpenQuery(path string, limit int) (source.Query, bool) {
	label, value, ok := strings.Cut(path, "/")
	if !ok || label == "" {
		return source.Query{}, false
	}
	return source.Query{Text: "{" + label + "=" + strconv.Quote(value) + "}", Limit: limitOf(limit, source.LokiOpenLimit, 0)}, true
}

// OpenQuery 는 트리 Enter 미리 보기(Open)와 같은 질의를 Run 꼴로 쓴 것이다. 미리 보기 결과도 time 정렬을
// 서버로 다시 읽을 수 있게 결과의 원래 질의로 남긴다(R62.3). Grafana 는 path 첫 마디(uid)를 지시줄로 싣는다.
// Loki 라벨 값 경로가 아니면(Prometheus 지표 등) ok 가 false 다.
//   - 예: KindLoki "app/api" → `{app="api"}`, KindGrafana "lk1/app/api" → "# datasource: lk1\n{app=\"api\"}"
func OpenQuery(kind db.Kind, path string, limit int) (source.Query, bool) {
	if kind != db.KindGrafana {
		return lokiOpenQuery(path, limit)
	}
	uid, rest, _ := strings.Cut(path, "/")
	q, ok := lokiOpenQuery(rest, limit)
	if !ok || uid == "" {
		return source.Query{}, false
	}
	q.Text = "# datasource: " + uid + "\n" + q.Text
	return q, true
}

// Run 은 LogQL 을 query_range 로 보낸다(기본 backward 최신 먼저, q.Direction "forward" 면 오래된 것부터).
// 로그 질의는 서버 limit 으로, 지표 질의는 받은 뒤 행을 자른다.
func (l *loki) Run(ctx context.Context, q source.Query) (*db.Result, error) {
	if strings.TrimSpace(q.Text) == "" {
		return nil, errEmptyQuery
	}
	direction := q.Direction
	switch direction {
	case "":
		direction = "backward"
	case "forward", "backward":
	default:
		return nil, fmt.Errorf("loki: unknown direction %q (forward or backward)", direction)
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
		"direction": {direction},
		"step":      {step(start, end)},
	}, &d)
	if err != nil {
		return nil, err
	}
	res, err := toResult(d, limit, direction == "forward")
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
