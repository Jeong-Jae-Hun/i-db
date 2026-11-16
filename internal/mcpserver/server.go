// Package mcpserver 는 DataGrip 프로젝트와 db 전용 접속을 MCP(stdio) 도구로 내보낸다. 모든 도구는 읽기 전용이다.
package mcpserver

import (
	"context"
	"errors"
	"fmt"
	"os"
	"slices"
	"strings"
	"time"

	"github.com/modelcontextprotocol/go-sdk/mcp"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
)

const (
	defaultMaxRows = 200
	defaultKeys    = 100
	// 응답 하나가 LLM 컨텍스트를 채우지 않도록 칸과 전체 크기를 db 의 화면용 상한(64KiB·1MiB)보다 작게 둔다
	maxCellBytes   = 4 << 10
	maxOutputBytes = 256 << 10
	clipMark       = "…"
)

// scanTimeout 은 redis_scan 의 시간 상한이다. SCAN 은 패턴에 맞는 키가 드물면 키 공간 전체를 돌아서다.
// 테스트가 줄일 수 있게 변수로 둔다
var scanTimeout = 10 * time.Second

type Options struct {
	AllowProd bool // false 면 Env()=="prod" 접속을 쓰는 도구를 모두 거부한다
	MaxRows   int  // 표 결과의 기본·최대 행 수. 0 이면 200, db.MaxRows 를 넘으면 db.MaxRows
	// OpenSource 는 읽기 전용 저장소(R60~R68) Backend 생성기다. 종류별 구현은 main 에 등록돼 있어 주입받는다. nil 이면 저장소 도구를 거부한다
	OpenSource func(context.Context, db.Conn) (source.Backend, error)
}

type handler struct {
	proj    *db.Project
	opt     Options
	clients *db.Clients
}

type connInfo struct {
	Name  string `json:"name"`
	Group string `json:"group"`
	Kind  string `json:"kind" jsonschema:"postgresql, redis, or a read-only source: opensearch, loki, prometheus, grafana, dynamodb, athena, aws"`
	Env   string `json:"env" jsonschema:"prod, stg or local (derived from the name)"`
	// 리전·프로필은 비밀값이 아니다. AWS 자격 증명은 프로필로 받고 접속에 저장하지 않는다(R60.1)
	Region  string `json:"region,omitempty" jsonschema:"AWS region (dynamodb, athena, aws)"`
	Profile string `json:"profile,omitempty" jsonschema:"AWS profile name; empty means the default credential chain"`
}

type listConnectionsOutput struct {
	Connections []connInfo `json:"connections"`
}

type connInput struct {
	Connection string `json:"connection" jsonschema:"connection name from list_connections"`
}

type schemaInput struct {
	Connection string `json:"connection" jsonschema:"PostgreSQL connection name"`
	Schema     string `json:"schema"`
}

type tableInput struct {
	Connection string `json:"connection" jsonschema:"PostgreSQL connection name"`
	Schema     string `json:"schema"`
	Table      string `json:"table"`
}

type queryInput struct {
	Connection string `json:"connection" jsonschema:"PostgreSQL or read-only source connection name (not aws)"`
	SQL        string `json:"sql" jsonschema:"a single read-only statement: PostgreSQL or Athena SQL (select, with, show, describe, explain), OpenSearch SQL or DSL, LogQL, PromQL or DynamoDB PartiQL SELECT"`
	MaxRows    int    `json:"max_rows,omitempty" jsonschema:"maximum rows to return; capped by the server limit"`
	Force      bool   `json:"force,omitempty" jsonschema:"send a query the source flagged as expensive. Athena bills per byte scanned and DynamoDB scans read the whole table; set only after the user accepts the cost. Ignored for PostgreSQL"`
}

type awsPathInput struct {
	Connection string `json:"connection" jsonschema:"aws connection name"`
	Path       string `json:"path,omitempty" jsonschema:"node path from aws_tree; empty for the top folders"`
}

type treeNode struct {
	Name        string `json:"name"`
	Path        string `json:"path" jsonschema:"pass to aws_tree or aws_open"`
	Info        string `json:"info,omitempty"`
	Warn        bool   `json:"warn,omitempty" jsonschema:"info needs attention, e.g. messages waiting in a dead-letter queue"`
	HasChildren bool   `json:"has_children"`
	Openable    bool   `json:"openable" jsonschema:"aws_open returns a table for this node"`
}

type treeOutput struct {
	Nodes     []treeNode `json:"nodes"`
	Truncated bool       `json:"truncated"`
}

type redisScanInput struct {
	Connection string `json:"connection" jsonschema:"Redis connection name"`
	Pattern    string `json:"pattern,omitempty" jsonschema:"SCAN MATCH pattern, default *"`
	Limit      int    `json:"limit,omitempty" jsonschema:"maximum keys to return, default 100, at most 1000"`
}

type redisGetInput struct {
	Connection string `json:"connection" jsonschema:"Redis connection name"`
	Key        string `json:"key"`
}

type redisCommandInput struct {
	Connection string   `json:"connection" jsonschema:"Redis connection name"`
	Args       []string `json:"args" jsonschema:"command and arguments, e.g. [\"HGET\", \"user:1\", \"name\"]"`
}

type namesOutput struct {
	Names     []string `json:"names"`
	Truncated bool     `json:"truncated"`
}

type resultOutput struct {
	Columns   []string   `json:"columns,omitempty"`
	Rows      [][]string `json:"rows,omitempty"`
	Text      string     `json:"text,omitempty" jsonschema:"non-tabular result such as a command tag or a string value"`
	Truncated bool       `json:"truncated" jsonschema:"true when rows or values were cut to fit the limits"`
}

// Run 은 stdio 로 MCP 서버를 돌리고, 끝나면 연 접속을 모두 닫는다.
func Run(ctx context.Context, proj *db.Project, opt Options) error {
	h := newHandler(proj, opt)
	defer h.clients.Close()
	return h.server().Run(ctx, &mcp.StdioTransport{})
}

func newHandler(proj *db.Project, opt Options) *handler {
	if opt.MaxRows <= 0 {
		opt.MaxRows = defaultMaxRows
	}
	opt.MaxRows = min(opt.MaxRows, db.MaxRows)
	return &handler{proj: proj, opt: opt, clients: db.NewClients()}
}

func (h *handler) server() *mcp.Server {
	s := mcp.NewServer(&mcp.Implementation{Name: "db", Version: "v2"}, nil)
	readOnly := &mcp.ToolAnnotations{ReadOnlyHint: true}
	mcp.AddTool(s, &mcp.Tool{Name: "list_connections", Annotations: readOnly,
		Description: "List PostgreSQL, Redis and read-only source connections (name, group, kind, env, and AWS region/profile). Passwords, tokens and credentials are never returned."},
		h.listConnections)
	mcp.AddTool(s, &mcp.Tool{Name: "list_schemas", Annotations: readOnly,
		Description: "List non-system schemas of a PostgreSQL connection."}, h.listSchemas)
	mcp.AddTool(s, &mcp.Tool{Name: "list_tables", Annotations: readOnly,
		Description: "List tables, views, materialized views and foreign tables in a PostgreSQL schema."}, h.listTables)
	mcp.AddTool(s, &mcp.Tool{Name: "describe_table", Annotations: readOnly,
		Description: "Describe a PostgreSQL table or view: columns (name, type, nullable, default, comment) as rows, plus comment, constraints and indexes as text."}, h.describeTable)
	mcp.AddTool(s, &mcp.Tool{Name: "query", Annotations: readOnly,
		Description: "Run one read-only statement. PostgreSQL: inside a READ ONLY transaction (30s timeout) on a connection that is discarded afterwards; writes and multiple statements are rejected, and calls to known side-effect functions (pg_terminate_backend, pg_notify, dblink, advisory locks, file access, large objects, ...) are blocked on a best-effort basis only, so connect with a read-only database role for real protection. Read-only sources (Athena, DynamoDB, OpenSearch, Loki, Prometheus, Grafana) get their own query language; write-like queries are refused before sending, and expensive ones (Athena SELECT without LIMIT or WHERE, DynamoDB full scan, OpenSearch size over 10000) are refused unless force is true. Athena queries cost money per byte scanned. aws connections have no query language; use aws_tree and aws_open."},
		h.query)
	mcp.AddTool(s, &mcp.Tool{Name: "aws_tree", Annotations: readOnly,
		Description: "List the children of a node in an aws connection (SNS topics and subscriptions, SQS queues with depth and DLQ, Lambda, EventBridge, Scheduler, Step Functions, S3). Read-only List/Get/Describe calls only; never receives, sends or deletes messages. Start with an empty path."},
		h.awsTree)
	mcp.AddTool(s, &mcp.Tool{Name: "aws_open", Annotations: readOnly,
		Description: "Show the detail table of an openable aws_tree node (subscriptions, queue attributes, event source mappings, rule targets, schedule, recent executions, S3 object metadata and small text body). Read-only."},
		h.awsOpen)
	mcp.AddTool(s, &mcp.Tool{Name: "redis_scan", Annotations: readOnly,
		Description: "List Redis keys matching a pattern using SCAN (sorted, deduplicated, stops after 10s)."}, h.redisScan)
	mcp.AddTool(s, &mcp.Tool{Name: "redis_get", Annotations: readOnly,
		Description: "Show a Redis key's type, TTL and value (strings, hashes, lists, sets, sorted sets, streams)."}, h.redisGet)
	mcp.AddTool(s, &mcp.Tool{Name: "redis_command", Annotations: readOnly,
		Description: "Run one read-only Redis command such as GET, HGET, TTL or LRANGE key 0 99. Write and admin commands, KEYS, CLIENT, SLOWLOG, MONITOR and whole-collection reads (SMEMBERS, HGETALL, HKEYS, HVALS) are rejected; range reads must be bounded (LRANGE/ZRANGE index span, XRANGE COUNT, ZRANGE BYSCORE LIMIT) and SCAN COUNT is at most 1000."},
		h.redisCommand)
	return s
}

// queryKinds 는 query 도구가 받는 종류다. aws 는 질의 언어가 없어 aws_tree·aws_open 으로 본다(R67.1).
var queryKinds = append([]db.Kind{db.KindPG}, slices.DeleteFunc(slices.Clone(db.SourceKinds), func(k db.Kind) bool { return k == db.KindAWS })...)

// lookup 은 이름으로 접속을 찾고 종류와 prod 허용을 확인한다. 접속을 열기 전에 거부해야 prod 에 연결 자체를 하지 않는다.
func (h *handler) lookup(name string, kinds ...db.Kind) (db.Conn, error) {
	for _, c := range h.proj.Conns {
		if c.Name != name {
			continue
		}
		if !slices.Contains(kinds, c.Kind) {
			names := make([]string, len(kinds))
			for i, k := range kinds {
				names[i] = string(k)
			}
			err := fmt.Errorf("connection %q is %s, not %s", name, c.Kind, strings.Join(names, "/"))
			if c.Kind == db.KindAWS {
				err = fmt.Errorf("%w; browse aws connections with aws_tree and aws_open", err)
			}
			return db.Conn{}, err
		}
		if c.Env() == "prod" && !h.opt.AllowProd {
			return db.Conn{}, fmt.Errorf("connection %q is prod; this MCP server was started without prod access", name)
		}
		return c, nil
	}
	return db.Conn{}, fmt.Errorf("unknown connection %q; call list_connections", name)
}

func (h *handler) pg(ctx context.Context, name string) (*db.PGConn, db.Conn, error) {
	c, err := h.lookup(name, db.KindPG)
	if err != nil {
		return nil, c, err
	}
	p, err := h.clients.PG(ctx, c)
	if err != nil {
		return nil, c, redactErr(err, c.Password)
	}
	return p, c, nil
}

func (h *handler) redis(ctx context.Context, name string) (*db.RedisConn, db.Conn, error) {
	c, err := h.lookup(name, db.KindRedis)
	if err != nil {
		return nil, c, err
	}
	r, err := h.clients.Redis(ctx, c)
	if err != nil {
		return nil, c, redactErr(err, c.Password)
	}
	return r, c, nil
}

func (h *handler) listConnections(_ context.Context, _ *mcp.CallToolRequest, _ struct{}) (*mcp.CallToolResult, listConnectionsOutput, error) {
	out := listConnectionsOutput{Connections: []connInfo{}}
	for _, c := range h.proj.Conns {
		out.Connections = append(out.Connections, connInfo{Name: c.Name, Group: db.GroupOf(c), Kind: string(c.Kind), Env: c.Env(), Region: c.Region, Profile: c.Profile})
	}
	return nil, out, nil
}

func (h *handler) listSchemas(ctx context.Context, _ *mcp.CallToolRequest, in connInput) (*mcp.CallToolResult, namesOutput, error) {
	p, c, err := h.pg(ctx, in.Connection)
	if err != nil {
		return nil, namesOutput{}, err
	}
	names, err := p.FindSchemas(ctx)
	if err != nil {
		return nil, namesOutput{}, redactErr(err, c.Password)
	}
	return nil, boundNames(names, db.MaxRows), nil
}

func (h *handler) listTables(ctx context.Context, _ *mcp.CallToolRequest, in schemaInput) (*mcp.CallToolResult, namesOutput, error) {
	p, c, err := h.pg(ctx, in.Connection)
	if err != nil {
		return nil, namesOutput{}, err
	}
	names, err := p.FindTables(ctx, in.Schema)
	if err != nil {
		return nil, namesOutput{}, redactErr(err, c.Password)
	}
	return nil, boundNames(names, db.MaxRows), nil
}

func (h *handler) describeTable(ctx context.Context, _ *mcp.CallToolRequest, in tableInput) (*mcp.CallToolResult, resultOutput, error) {
	p, c, err := h.pg(ctx, in.Connection)
	if err != nil {
		return nil, resultOutput{}, err
	}
	res, err := p.DescribeTable(ctx, db.TableRef{Schema: in.Schema, Table: in.Table})
	if err != nil {
		return nil, resultOutput{}, redactErr(err, c.Password)
	}
	return nil, boundResult(res, db.MaxRows), nil
}

func (h *handler) query(ctx context.Context, _ *mcp.CallToolRequest, in queryInput) (*mcp.CallToolResult, resultOutput, error) {
	c, err := h.lookup(in.Connection, queryKinds...)
	if err != nil {
		return nil, resultOutput{}, err
	}
	limit := h.opt.MaxRows
	if in.MaxRows > 0 {
		limit = min(in.MaxRows, limit)
	}
	if c.Kind != db.KindPG {
		return h.querySource(ctx, c, in, limit)
	}
	// 접속을 열기 전에 거른다(prod 에는 연결도 하지 않는다). 놓친 쓰기는 RunReadOnly 의 READ ONLY 트랜잭션이 막는다
	if err := db.CheckReadOnlySQL(in.SQL); err != nil {
		return nil, resultOutput{}, err
	}
	p, err := h.clients.PG(ctx, c)
	if err != nil {
		return nil, resultOutput{}, redactErr(err, c.Password)
	}
	res, err := p.RunReadOnly(ctx, in.SQL)
	if err != nil {
		return nil, resultOutput{}, redactErr(err, c.Password)
	}
	return nil, boundResult(res, limit), nil
}

// querySource 는 저장소 Backend.Run 이다. 쓰기 거절·비싼 질의 확인은 Backend 가 보내기 전에 한다(R60.2).
// MCP 는 사람에게 y/N 을 물을 수 없어 force 가 그 확인을 대신한다(CLI --force 와 같다).
func (h *handler) querySource(ctx context.Context, c db.Conn, in queryInput, limit int) (*mcp.CallToolResult, resultOutput, error) {
	text := strings.TrimSpace(in.SQL)
	if text == "" {
		return nil, resultOutput{}, errors.New("sql is empty")
	}
	b, err := h.openSource(ctx, c)
	if err != nil {
		return nil, resultOutput{}, err
	}
	defer b.Close()
	res, err := b.Run(ctx, source.Query{Text: text, Limit: limit, Force: in.Force})
	if errors.Is(err, source.ErrNeedsConfirm) {
		return nil, resultOutput{}, fmt.Errorf("%s; nothing was sent. Retry with force=true only if the user accepts the cost", redactErr(err, c.Password))
	}
	if err != nil {
		return nil, resultOutput{}, redactErr(err, c.Password)
	}
	return nil, boundResult(res, limit), nil
}

func (h *handler) openSource(ctx context.Context, c db.Conn) (source.Backend, error) {
	if h.opt.OpenSource == nil {
		return nil, fmt.Errorf("%s connections are not supported by this server", c.Kind)
	}
	b, err := h.opt.OpenSource(ctx, c)
	if err != nil {
		return nil, redactErr(err, c.Password)
	}
	return b, nil
}

func (h *handler) aws(ctx context.Context, name string) (source.Backend, error) {
	c, err := h.lookup(name, db.KindAWS)
	if err != nil {
		return nil, err
	}
	return h.openSource(ctx, c)
}

func (h *handler) awsTree(ctx context.Context, _ *mcp.CallToolRequest, in awsPathInput) (*mcp.CallToolResult, treeOutput, error) {
	b, err := h.aws(ctx, in.Connection)
	if err != nil {
		return nil, treeOutput{}, err
	}
	defer b.Close()
	nodes, err := b.Children(ctx, in.Path)
	if err != nil {
		return nil, treeOutput{}, err
	}
	out := treeOutput{Nodes: []treeNode{}}
	budget := maxOutputBytes
	for i, n := range nodes {
		if i >= db.MaxRows || budget <= 0 {
			out.Truncated = true
			break
		}
		t := treeNode{Name: clip(n.Name, maxCellBytes), Path: n.Path, Info: clip(n.Info, maxCellBytes), Warn: n.Warn, HasChildren: n.HasChildren, Openable: n.Openable}
		out.Truncated = out.Truncated || t.Name != n.Name || t.Info != n.Info
		budget -= len(t.Name) + len(t.Path) + len(t.Info)
		out.Nodes = append(out.Nodes, t)
	}
	return nil, out, nil
}

func (h *handler) awsOpen(ctx context.Context, _ *mcp.CallToolRequest, in awsPathInput) (*mcp.CallToolResult, resultOutput, error) {
	b, err := h.aws(ctx, in.Connection)
	if err != nil {
		return nil, resultOutput{}, err
	}
	defer b.Close()
	res, err := b.Open(ctx, in.Path, h.opt.MaxRows)
	if err != nil {
		return nil, resultOutput{}, err
	}
	return nil, boundResult(res, h.opt.MaxRows), nil
}

func (h *handler) redisScan(ctx context.Context, _ *mcp.CallToolRequest, in redisScanInput) (*mcp.CallToolResult, namesOutput, error) {
	r, c, err := h.redis(ctx, in.Connection)
	if err != nil {
		return nil, namesOutput{}, err
	}
	pattern := in.Pattern
	if pattern == "" {
		pattern = "*"
	}
	sctx, cancel := context.WithTimeout(ctx, scanTimeout)
	defer cancel()
	keys, more, err := r.GetKeys(sctx, pattern)
	// go-redis 는 마감을 소켓 데드라인으로 걸어 context 오류 대신 i/o timeout 이 먼저 올 수 있다
	if ctx.Err() == nil && (errors.Is(err, context.DeadlineExceeded) || errors.Is(err, os.ErrDeadlineExceeded)) {
		return nil, namesOutput{}, fmt.Errorf("redis_scan stopped after %s; use a narrower pattern", scanTimeout)
	}
	if err != nil {
		return nil, namesOutput{}, redactErr(err, c.Password)
	}
	limit := defaultKeys
	if in.Limit > 0 {
		limit = min(in.Limit, db.MaxRows)
	}
	out := boundNames(keys, limit)
	out.Truncated = out.Truncated || more
	return nil, out, nil
}

func (h *handler) redisGet(ctx context.Context, _ *mcp.CallToolRequest, in redisGetInput) (*mcp.CallToolResult, resultOutput, error) {
	r, c, err := h.redis(ctx, in.Connection)
	if err != nil {
		return nil, resultOutput{}, err
	}
	res, err := r.GetValue(ctx, in.Key)
	if err != nil {
		return nil, resultOutput{}, redactErr(err, c.Password)
	}
	return nil, boundResult(res, h.opt.MaxRows), nil
}

func (h *handler) redisCommand(ctx context.Context, _ *mcp.CallToolRequest, in redisCommandInput) (*mcp.CallToolResult, resultOutput, error) {
	if !db.IsReadOnlyRedis(in.Args) {
		return nil, resultOutput{}, errors.New("only read-only Redis commands are allowed")
	}
	if err := checkRedisArgs(in.Args, h.opt.MaxRows); err != nil {
		return nil, resultOutput{}, err
	}
	r, c, err := h.redis(ctx, in.Connection)
	if err != nil {
		return nil, resultOutput{}, err
	}
	res, err := r.Run(ctx, in.Args)
	if err != nil {
		return nil, resultOutput{}, redactErr(err, c.Password)
	}
	return nil, boundResult(res, h.opt.MaxRows), nil
}

func redactErr(err error, secret string) error {
	return errors.New(db.Redact(err.Error(), secret))
}

// boundResult 는 행 수·칸 크기·전체 크기를 MCP 상한에 맞춰 자른다.
func boundResult(res *db.Result, maxRows int) resultOutput {
	out := resultOutput{Columns: res.Cols, Text: clip(res.Text, maxOutputBytes), Truncated: res.Truncated}
	budget := maxOutputBytes - len(out.Text)
	for i, row := range res.Rows {
		if i >= maxRows || budget <= 0 {
			out.Truncated = true
			break
		}
		r := make([]string, len(row))
		for j, cell := range row {
			r[j] = clip(cell, maxCellBytes)
			out.Truncated = out.Truncated || r[j] != cell
			budget -= len(r[j])
		}
		out.Rows = append(out.Rows, r)
	}
	out.Truncated = out.Truncated || out.Text != res.Text
	return out
}

func boundNames(names []string, limit int) namesOutput {
	out := namesOutput{Names: []string{}}
	budget := maxOutputBytes
	for i, n := range names {
		if i >= limit || budget <= 0 {
			out.Truncated = true
			break
		}
		c := clip(n, maxCellBytes)
		out.Truncated = out.Truncated || c != n
		budget -= len(c)
		out.Names = append(out.Names, c)
	}
	return out
}

// clip 은 n 바이트를 넘으면 자르고 끝에 clipMark 를 붙인다. 바이트로 자른 끝의 깨진 UTF-8 조각은 버린다.
func clip(s string, n int) string {
	if len(s) <= n {
		return s
	}
	return strings.ToValidUTF8(s[:n], "") + clipMark
}
