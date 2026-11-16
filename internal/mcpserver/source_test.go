package mcpserver

import (
	"context"
	"encoding/json"
	"strconv"
	"strings"
	"sync"
	"testing"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
)

var sourceProject = &db.Project{Conns: []db.Conn{
	{Name: "athena-local", UUID: "s1", Group: "data", Kind: db.KindAthena, Region: "ap-northeast-2", Profile: "data-ro", Workgroup: "primary"},
	{Name: "athena-prod", UUID: "s2", Kind: db.KindAthena, Region: "ap-northeast-2"},
	{Name: "aws-local", UUID: "s3", Kind: db.KindAWS, Region: "us-east-1", Profile: "infra"},
	{Name: "aws-prod", UUID: "s4", Kind: db.KindAWS, Region: "ap-northeast-2"},
	{Name: "logs-local", UUID: "s5", Kind: db.KindLoki, URL: "https://loki.example", Auth: "basic", User: "u", Password: "loki-pw"},
}}

// fakeBackend 는 받은 요청을 적고 정한 값을 돌려준다. athena 처럼 Force 없는 "select" 는 보내기 전에 ErrNeedsConfirm 이다.
type fakeBackend struct {
	mu     sync.Mutex
	runs   []source.Query
	opens  []string
	limits []int
}

func (f *fakeBackend) Children(_ context.Context, path string) ([]source.Node, error) {
	if path == "" {
		return []source.Node{{Name: "SQS 큐", Path: "sqs", HasChildren: true}}, nil
	}
	return []source.Node{{Name: "orders", Path: "sqs/orders", Info: "대기 12 · DLQ orders-dlq 3", Warn: true, Openable: true}}, nil
}

func (f *fakeBackend) Run(_ context.Context, q source.Query) (*db.Result, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	if strings.HasPrefix(q.Text, "select") && !q.Force {
		return nil, source.NeedsConfirm("전체 스캔")
	}
	f.runs = append(f.runs, q)
	res := &db.Result{Cols: []string{"n"}, Text: "스캔 1.2GB"}
	for i := range 10 {
		res.Rows = append(res.Rows, []string{strconv.Itoa(i)})
	}
	return res, nil
}

func (f *fakeBackend) Open(_ context.Context, path string, limit int) (*db.Result, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.opens, f.limits = append(f.opens, path), append(f.limits, limit)
	return &db.Result{Cols: []string{"attribute", "value"}, Rows: [][]string{{"QueueUrl", "https://sqs/orders"}}}, nil
}

func (f *fakeBackend) Close() error { return nil }

// sourceHandler 는 가짜 Backend 를 여는 handler 와 연 접속 이름 목록을 돌려준다.
func sourceHandler(opt Options) (*handler, *fakeBackend, *[]string) {
	f, opened := &fakeBackend{}, &[]string{}
	var mu sync.Mutex
	opt.OpenSource = func(_ context.Context, c db.Conn) (source.Backend, error) {
		mu.Lock()
		defer mu.Unlock()
		*opened = append(*opened, c.Name)
		return f, nil
	}
	return newHandler(sourceProject, opt), f, opened
}

func TestQuery_Athena_비싼_질의는_force_없이_거부하고_force_면_보낸다(t *testing.T) {
	h, f, _ := sourceHandler(Options{})
	cs := connect(t, h)
	text, isErr := cs.callTool(call{"query", map[string]any{"connection": "athena-local", "sql": "select * from t"}})
	if !isErr || !strings.Contains(text, "전체 스캔") || !strings.Contains(text, "force=true") || len(f.runs) != 0 {
		t.Fatalf("force 없으면 거부여야 한다: %s runs=%v", text, f.runs)
	}
	text, isErr = cs.callTool(call{"query", map[string]any{"connection": "athena-local", "sql": " select * from t ", "force": true}})
	if isErr {
		t.Fatal(text)
	}
	var out resultOutput
	if err := json.Unmarshal([]byte(text), &out); err != nil {
		t.Fatal(err)
	}
	if len(f.runs) != 1 || !f.runs[0].Force || f.runs[0].Text != "select * from t" || out.Text != "스캔 1.2GB" || out.Columns[0] != "n" {
		t.Fatalf("force 면 보내야 한다: runs=%+v out=%+v", f.runs, out)
	}
}

func TestQuery_저장소_행수는_max_rows_와_서버_상한으로_자른다(t *testing.T) {
	h, f, _ := sourceHandler(Options{MaxRows: 5})
	cs := connect(t, h)
	for _, tc := range []struct {
		maxRows, want int
	}{{0, 5}, {3, 3}, {50, 5}} {
		text, isErr := cs.callTool(call{"query", map[string]any{"connection": "athena-local", "sql": "show databases", "max_rows": tc.maxRows}})
		if isErr {
			t.Fatal(text)
		}
		var out resultOutput
		if err := json.Unmarshal([]byte(text), &out); err != nil {
			t.Fatal(err)
		}
		if len(out.Rows) != tc.want || !out.Truncated || f.runs[len(f.runs)-1].Limit != tc.want {
			t.Fatalf("max_rows=%d: rows=%d limit=%d", tc.maxRows, len(out.Rows), f.runs[len(f.runs)-1].Limit)
		}
	}
}

func Test저장소_도구_prod_접속은_허용하지_않으면_열지도_않고_거부한다(t *testing.T) {
	prodCalls := []call{
		{"query", map[string]any{"connection": "athena-prod", "sql": "show databases"}},
		{"aws_tree", map[string]any{"connection": "aws-prod"}},
		{"aws_open", map[string]any{"connection": "aws-prod", "path": "sqs/orders"}},
	}
	h, _, opened := sourceHandler(Options{})
	cs := connect(t, h)
	for _, c := range prodCalls {
		text, isErr := cs.callTool(c)
		if !isErr || !strings.Contains(text, "is prod") {
			t.Fatalf("%s: prod 거부여야 한다: %s", c.name, text)
		}
	}
	if len(*opened) != 0 {
		t.Fatalf("prod 접속을 열면 안 된다: %v", *opened)
	}

	h, _, opened = sourceHandler(Options{AllowProd: true})
	cs = connect(t, h)
	for _, c := range prodCalls {
		if text, isErr := cs.callTool(c); isErr {
			t.Fatalf("%s: AllowProd 면 보내야 한다: %s", c.name, text)
		}
	}
	if len(*opened) != len(prodCalls) {
		t.Fatalf("연 접속: %v", *opened)
	}
}

func TestAwsTree_자식_노드를_경로_정보_경고와_함께_준다(t *testing.T) {
	h, _, _ := sourceHandler(Options{})
	cs := connect(t, h)
	text, isErr := cs.callTool(call{"aws_tree", map[string]any{"connection": "aws-local"}})
	if isErr {
		t.Fatal(text)
	}
	var out treeOutput
	if err := json.Unmarshal([]byte(text), &out); err != nil {
		t.Fatal(err)
	}
	if len(out.Nodes) != 1 || out.Nodes[0] != (treeNode{Name: "SQS 큐", Path: "sqs", HasChildren: true}) || out.Truncated {
		t.Fatalf("맨 위: %+v", out)
	}
	text, _ = cs.callTool(call{"aws_tree", map[string]any{"connection": "aws-local", "path": "sqs"}})
	out = treeOutput{}
	if err := json.Unmarshal([]byte(text), &out); err != nil {
		t.Fatal(err)
	}
	want := treeNode{Name: "orders", Path: "sqs/orders", Info: "대기 12 · DLQ orders-dlq 3", Warn: true, Openable: true}
	if len(out.Nodes) != 1 || out.Nodes[0] != want || !strings.Contains(text, `"has_children":false`) {
		t.Fatalf("sqs: %s", text)
	}
}

func TestAwsOpen_노드_표를_query_와_같은_꼴로_준다(t *testing.T) {
	h, f, _ := sourceHandler(Options{MaxRows: 7})
	cs := connect(t, h)
	text, isErr := cs.callTool(call{"aws_open", map[string]any{"connection": "aws-local", "path": "sqs/orders"}})
	if isErr {
		t.Fatal(text)
	}
	var out resultOutput
	if err := json.Unmarshal([]byte(text), &out); err != nil {
		t.Fatal(err)
	}
	if strings.Join(out.Columns, ",") != "attribute,value" || len(out.Rows) != 1 || out.Rows[0][1] != "https://sqs/orders" {
		t.Fatalf("표: %s", text)
	}
	if f.opens[0] != "sqs/orders" || f.limits[0] != 7 {
		t.Fatalf("Open 인자: %v %v", f.opens, f.limits)
	}
}

func Test저장소_도구_종류가_맞지_않으면_열지_않고_거부한다(t *testing.T) {
	h, _, opened := sourceHandler(Options{})
	cs := connect(t, h)
	text, isErr := cs.callTool(call{"query", map[string]any{"connection": "aws-local", "sql": "select 1"}})
	if !isErr || !strings.Contains(text, "aws_tree") {
		t.Fatalf("aws 질의는 트리 안내로 거부해야 한다: %s", text)
	}
	text, isErr = cs.callTool(call{"aws_tree", map[string]any{"connection": "athena-local"}})
	if !isErr || !strings.Contains(text, "not aws") {
		t.Fatalf("athena 는 aws_tree 대상이 아니다: %s", text)
	}
	if len(*opened) != 0 {
		t.Fatalf("열면 안 된다: %v", *opened)
	}
	// 생성기가 없으면(주입 안 함) 저장소 도구는 거부한다
	cs = connect(t, newHandler(sourceProject, Options{}))
	text, isErr = cs.callTool(call{"query", map[string]any{"connection": "athena-local", "sql": "show databases"}})
	if !isErr || !strings.Contains(text, "not supported") {
		t.Fatalf("생성기 없음: %s", text)
	}
}

func TestListConnections_저장소_리전_프로필을_주고_비밀값은_주지_않는다(t *testing.T) {
	h, _, _ := sourceHandler(Options{})
	cs := connect(t, h)
	text, isErr := cs.callTool(call{name: "list_connections"})
	if isErr {
		t.Fatal(text)
	}
	for _, secret := range []string{"loki-pw", "Password", "password"} {
		if strings.Contains(text, secret) {
			t.Fatalf("비밀값이 나가면 안 된다: %s", text)
		}
	}
	var out listConnectionsOutput
	if err := json.Unmarshal([]byte(text), &out); err != nil {
		t.Fatal(err)
	}
	want := map[string]connInfo{
		"athena-local": {Name: "athena-local", Group: "data", Kind: "athena", Env: "local", Region: "ap-northeast-2", Profile: "data-ro"},
		"aws-local":    {Name: "aws-local", Group: "기타", Kind: "aws", Env: "local", Region: "us-east-1", Profile: "infra"},
	}
	for _, c := range out.Connections {
		if w, ok := want[c.Name]; ok && c != w {
			t.Fatalf("%s: %+v, want %+v", c.Name, c, w)
		}
	}
	if len(out.Connections) != len(sourceProject.Conns) {
		t.Fatalf("접속 수: %d", len(out.Connections))
	}
}
