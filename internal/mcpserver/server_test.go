package mcpserver

import (
	"bufio"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/url"
	"os"
	"strconv"
	"strings"
	"testing"
	"time"
	"unicode/utf8"

	"github.com/modelcontextprotocol/go-sdk/mcp"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
)

// 닿지 않는 주소(포트 1)라 도구가 거부하지 않고 접속을 시도하면 접속 오류가 나 구분된다.
var unitProject = &db.Project{Conns: []db.Conn{
	{Name: "app-local", UUID: "u1", Group: "app", Kind: db.KindPG, Host: "127.0.0.1", Port: 1, DB: "app", User: "u", Password: "s3cr3t-pw"},
	{Name: "app-prod", UUID: "u2", Kind: db.KindPG, Host: "127.0.0.1", Port: 1, DB: "app", User: "u", Password: "prod-pw"},
	{Name: "cache-local", UUID: "u3", Kind: db.KindRedis, Host: "127.0.0.1", Port: 1, Password: "redis-pw"},
	{Name: "cache-prod", UUID: "u4", Kind: db.KindRedis, Host: "127.0.0.1", Port: 1},
}}

type client struct {
	t  *testing.T
	cs *mcp.ClientSession
}

type call struct {
	name string
	args map[string]any
}

func connect(t *testing.T, h *handler) client {
	t.Helper()
	c, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	t.Cleanup(cancel)
	st, ct := mcp.NewInMemoryTransports()
	ss, err := h.server().Connect(c, st, nil)
	if err != nil {
		t.Fatal(err)
	}
	cs, err := mcp.NewClient(&mcp.Implementation{Name: "test"}, nil).Connect(c, ct, nil)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		// 테스트 정리라 닫기·대기 오류로 할 일이 없다
		_ = cs.Close()
		_ = ss.Wait()
		h.clients.Close()
	})
	return client{t: t, cs: cs}
}

// callTool 은 도구를 부르고 (본문 텍스트, 오류 여부)를 돌려준다.
func (cl client) callTool(c call) (string, bool) {
	cl.t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	res, err := cl.cs.CallTool(ctx, &mcp.CallToolParams{Name: c.name, Arguments: c.args})
	if err != nil {
		cl.t.Fatalf("%s: protocol error: %v", c.name, err)
	}
	var b strings.Builder
	for _, ct := range res.Content {
		if tc, ok := ct.(*mcp.TextContent); ok {
			b.WriteString(tc.Text)
		}
	}
	return b.String(), res.IsError
}

func TestListConnections_비밀번호_없이_이름_종류_환경을_준다(t *testing.T) {
	cs := connect(t, newHandler(unitProject, Options{}))
	text, isErr := cs.callTool(call{name: "list_connections"})
	if isErr {
		t.Fatal(text)
	}
	for _, pw := range []string{"s3cr3t-pw", "prod-pw", "redis-pw", "Password"} {
		if strings.Contains(text, pw) {
			t.Fatalf("비밀번호가 나가면 안 된다: %s", text)
		}
	}
	var out listConnectionsOutput
	if err := json.Unmarshal([]byte(text), &out); err != nil {
		t.Fatal(err)
	}
	want := connInfo{Name: "app-prod", Group: "기타", Kind: "postgresql", Env: "prod"}
	if len(out.Connections) != 4 || out.Connections[1] != want || out.Connections[0].Group != "app" {
		t.Fatalf("접속 목록: %+v", out.Connections)
	}
}

func Test도구_prod_접속은_허용하지_않으면_접속도_하지_않고_거부한다(t *testing.T) {
	cs := connect(t, newHandler(unitProject, Options{}))
	for _, c := range []call{
		{"list_schemas", map[string]any{"connection": "app-prod"}},
		{"list_tables", map[string]any{"connection": "app-prod", "schema": "public"}},
		{"describe_table", map[string]any{"connection": "app-prod", "schema": "public", "table": "t"}},
		{"query", map[string]any{"connection": "app-prod", "sql": "select 1"}},
		{"redis_scan", map[string]any{"connection": "cache-prod"}},
		{"redis_get", map[string]any{"connection": "cache-prod", "key": "k"}},
		{"redis_command", map[string]any{"connection": "cache-prod", "args": []string{"GET", "k"}}},
	} {
		text, isErr := cs.callTool(c)
		if !isErr || !strings.Contains(text, "is prod") {
			t.Fatalf("%s: prod 거부여야 한다: %s", c.name, text)
		}
	}

	// 허용하면 거부하지 않고 접속을 시도한다(포트 1 이라 접속 오류)
	cs = connect(t, newHandler(unitProject, Options{AllowProd: true}))
	text, isErr := cs.callTool(call{"query", map[string]any{"connection": "app-prod", "sql": "select 1"}})
	if !isErr || strings.Contains(text, "is prod") {
		t.Fatalf("AllowProd 면 접속을 시도해야 한다: %s", text)
	}
}

func Test도구_쓰기_SQL과_쓰기_Redis_명령과_KEYS_는_접속_전에_거부한다(t *testing.T) {
	cs := connect(t, newHandler(unitProject, Options{}))
	for _, c := range []call{
		{"query", map[string]any{"connection": "app-local", "sql": "delete from t"}},
		{"query", map[string]any{"connection": "app-local", "sql": "explain analyze delete from t"}},
		{"query", map[string]any{"connection": "app-local", "sql": "set role postgres"}},
		{"redis_command", map[string]any{"connection": "cache-local", "args": []string{"SET", "k", "v"}}},
		{"redis_command", map[string]any{"connection": "cache-local", "args": []string{"SLOWLOG", "RESET"}}},
		{"redis_command", map[string]any{"connection": "cache-local", "args": []string{}}},
		{"redis_command", map[string]any{"connection": "cache-local", "args": []string{"keys", "*"}}},
		{"query", map[string]any{"connection": "app-local", "sql": "select pg_terminate_backend(1)"}},
		{"redis_command", map[string]any{"connection": "cache-local", "args": []string{"HGETALL", "k"}}},
	} {
		text, isErr := cs.callTool(c)
		if !isErr || !(strings.Contains(text, "read-only") || strings.Contains(text, "redis_scan") || strings.Contains(text, "whole collection")) {
			t.Fatalf("%s %v: 접속 전 거부여야 한다: %s", c.name, c.args, text)
		}
	}
}

func Test도구_접속_이름과_종류가_맞지_않으면_거부한다(t *testing.T) {
	cs := connect(t, newHandler(unitProject, Options{}))
	text, isErr := cs.callTool(call{"query", map[string]any{"connection": "nope", "sql": "select 1"}})
	if !isErr || !strings.Contains(text, "unknown connection") {
		t.Fatalf("없는 접속: %s", text)
	}
	text, isErr = cs.callTool(call{"redis_get", map[string]any{"connection": "app-local", "key": "k"}})
	if !isErr || !strings.Contains(text, "not redis") {
		t.Fatalf("종류 불일치: %s", text)
	}
}

func Test도구_접속_오류에_실린_비밀번호를_지운다(t *testing.T) {
	// 호스트 이름이 비밀번호를 담고 있어 DNS 오류 메시지에 비밀번호가 실린다. 끝 점은 검색 도메인을 붙여 느려지는 것을 막는다
	p := &db.Project{Conns: []db.Conn{{Name: "leak-local", UUID: "l", Kind: db.KindPG, Host: "leaky-pw.invalid.", Port: 5432, User: "u", Password: "leaky-pw"}}}
	cs := connect(t, newHandler(p, Options{}))
	text, isErr := cs.callTool(call{"list_schemas", map[string]any{"connection": "leak-local"}})
	if !isErr || strings.Contains(text, "leaky-pw") || !strings.Contains(text, "***") {
		t.Fatalf("비밀번호를 지워야 한다: %s", text)
	}
}

func TestBoundResult_행수_칸_전체_크기를_자르고_잘림을_알린다(t *testing.T) {
	res := &db.Result{Cols: []string{"a"}}
	for i := 0; i < 50; i++ {
		res.Rows = append(res.Rows, []string{strconv.Itoa(i)})
	}
	if out := boundResult(res, 10); len(out.Rows) != 10 || !out.Truncated {
		t.Fatalf("행 수: %d %v", len(out.Rows), out.Truncated)
	}
	if out := boundResult(res, 50); len(out.Rows) != 50 || out.Truncated {
		t.Fatalf("상한 안이면 자르지 않는다: %d %v", len(out.Rows), out.Truncated)
	}

	big := strings.Repeat("가", maxCellBytes) // 3바이트 글자라 바이트 경계에서 글자가 깨진다
	res = &db.Result{Cols: []string{"a"}, Rows: [][]string{{big}}}
	out := boundResult(res, 10)
	if !out.Truncated || len(out.Rows[0][0]) > maxCellBytes+len(clipMark) || !strings.HasSuffix(out.Rows[0][0], clipMark) {
		t.Fatalf("칸 자르기: len=%d truncated=%v", len(out.Rows[0][0]), out.Truncated)
	}
	if !utf8.ValidString(out.Rows[0][0]) {
		t.Fatal("깨진 UTF-8 조각이 남으면 안 된다")
	}

	res = &db.Result{Cols: []string{"a"}}
	for i := 0; i < 1000; i++ {
		res.Rows = append(res.Rows, []string{strings.Repeat("x", maxCellBytes)})
	}
	out = boundResult(res, 1000)
	if !out.Truncated || len(out.Rows) > maxOutputBytes/maxCellBytes+1 {
		t.Fatalf("전체 크기: rows=%d", len(out.Rows))
	}

	out = boundResult(&db.Result{Text: strings.Repeat("y", maxOutputBytes+10)}, 10)
	if !out.Truncated || len(out.Text) > maxOutputBytes+len(clipMark) {
		t.Fatalf("텍스트: len=%d", len(out.Text))
	}
}

func TestNewHandler_MaxRows_기본값과_상한(t *testing.T) {
	for in, want := range map[int]int{0: defaultMaxRows, -5: defaultMaxRows, 50: 50, 5000: db.MaxRows} {
		h := newHandler(unitProject, Options{MaxRows: in})
		if h.opt.MaxRows != want {
			t.Fatalf("%d → %d, want %d", in, h.opt.MaxRows, want)
		}
		h.clients.Close()
	}
}

func TestRedisScan_끝나지_않는_SCAN은_시간_제한으로_끊는다(t *testing.T) {
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = ln.Close() }) // 테스트 정리라 닫기 오류로 할 일이 없다
	go serveEndlessScan(ln)
	old := scanTimeout
	scanTimeout = 300 * time.Millisecond
	t.Cleanup(func() { scanTimeout = old })

	port := ln.Addr().(*net.TCPAddr).Port
	p := &db.Project{Conns: []db.Conn{{Name: "slow-local", UUID: "s", Kind: db.KindRedis, Host: "127.0.0.1", Port: port}}}
	cs := connect(t, newHandler(p, Options{}))
	start := time.Now()
	text, isErr := cs.callTool(call{"redis_scan", map[string]any{"connection": "slow-local", "pattern": "none:*"}})
	if !isErr || !strings.Contains(text, "narrower pattern") || time.Since(start) > 5*time.Second {
		t.Fatalf("시간 제한으로 끊어야 한다 (%s): %s", time.Since(start), text)
	}
}

// serveEndlessScan 은 SCAN 에 늘 커서 1·빈 결과로 답해 키 공간이 끝없이 이어지는 Redis 를 흉내 낸다.
// 접속은 차례로 받는다. Clients.Redis 는 평문 확인용 연결을 닫은 뒤 새로 붙어 동시에 둘을 쓰지 않는다.
// 읽기·쓰기 오류는 상대가 끊었다는 뜻이라 그 연결을 닫고 다음 접속을 받는다.
func serveEndlessScan(ln net.Listener) {
	conn, err := ln.Accept()
	if err != nil {
		return // 테스트가 끝나 리스너가 닫혔다
	}
	r := bufio.NewReader(conn)
	for err == nil {
		var n int
		if _, err = fmt.Fscanf(r, "*%d\r\n", &n); err != nil {
			break
		}
		args := make([]string, max(n, 1))
		for i := 0; i < n && err == nil; i++ {
			var size int
			if _, err = fmt.Fscanf(r, "$%d\r\n", &size); err == nil {
				buf := make([]byte, size+2)
				_, err = io.ReadFull(r, buf)
				args[i] = string(buf[:size])
			}
		}
		reply := "+OK\r\n"
		switch strings.ToUpper(args[0]) {
		case "HELLO":
			reply = "-ERR unknown command 'HELLO'\r\n"
		case "PING":
			reply = "+PONG\r\n"
		case "SCAN":
			reply = "*2\r\n$1\r\n1\r\n*0\r\n"
		}
		if err == nil {
			_, err = io.WriteString(conn, reply)
		}
	}
	_ = conn.Close() // 이미 끊긴 가짜 서버 연결이라 닫기 오류로 할 일이 없다
	serveEndlessScan(ln)
}

// ─── 통합 (DBTUI_TEST_PG_URL·DBTUI_TEST_REDIS_ADDR 가 있을 때만) ───

func integrationProject(t *testing.T) *db.Project {
	t.Helper()
	raw, addr := os.Getenv("DBTUI_TEST_PG_URL"), os.Getenv("DBTUI_TEST_REDIS_ADDR")
	if raw == "" || addr == "" {
		t.Skip("DBTUI_TEST_PG_URL / DBTUI_TEST_REDIS_ADDR not set")
	}
	u, err := url.Parse(raw)
	if err != nil {
		t.Fatal(err)
	}
	pw, _ := u.User.Password()
	pgPort, err := strconv.Atoi(u.Port())
	if err != nil {
		t.Fatal(err)
	}
	host, port, err := net.SplitHostPort(addr)
	if err != nil {
		t.Fatal(err)
	}
	rPort, err := strconv.Atoi(port)
	if err != nil {
		t.Fatal(err)
	}
	return &db.Project{Conns: []db.Conn{
		{Name: "it-local", UUID: "it-pg", Kind: db.KindPG, Host: u.Hostname(), Port: pgPort, DB: strings.TrimPrefix(u.Path, "/"), User: u.User.Username(), Password: pw},
		{Name: "it-redis", UUID: "it-redis", Kind: db.KindRedis, Host: host, Port: rPort, DB: "0"},
	}}
}

func TestIntegration_PG_도구가_읽기만_하고_행수를_제한한다(t *testing.T) {
	cs := connect(t, newHandler(integrationProject(t), Options{MaxRows: 20}))
	if text, isErr := cs.callTool(call{"query", map[string]any{"connection": "it-local", "sql": "drop schema if exists mcp_it cascade"}}); !isErr {
		t.Fatalf("drop 은 거부해야 한다: %s", text)
	}
	// 준비는 MCP 밖에서 한다(도구로는 쓸 수 없다)
	setup := db.NewClients()
	defer setup.Close()
	pg, err := setup.PG(context.Background(), integrationProject(t).Conns[0])
	if err != nil {
		t.Fatal(err)
	}
	for _, sql := range []string{`drop schema if exists mcp_it cascade`, `create schema mcp_it`,
		`create table mcp_it.t (id int not null default 7 primary key, name text)`, `insert into mcp_it.t select generate_series(1, 100), 'n'`} {
		if _, err := pg.Run(context.Background(), sql); err != nil {
			t.Fatal(err)
		}
	}

	text, isErr := cs.callTool(call{"list_schemas", map[string]any{"connection": "it-local"}})
	if isErr || !strings.Contains(text, `"mcp_it"`) {
		t.Fatalf("list_schemas: %s", text)
	}
	text, isErr = cs.callTool(call{"list_tables", map[string]any{"connection": "it-local", "schema": "mcp_it"}})
	if isErr || !strings.Contains(text, `"t"`) {
		t.Fatalf("list_tables: %s", text)
	}
	text, isErr = cs.callTool(call{"describe_table", map[string]any{"connection": "it-local", "schema": "mcp_it", "table": "t"}})
	if isErr || !strings.Contains(text, `["id","integer","NO","DEFAULT 7",""]`) || !strings.Contains(text, "t_pkey") {
		t.Fatalf("describe_table 은 열과 제약을 준다: %s", text)
	}
	text, isErr = cs.callTool(call{"describe_table", map[string]any{"connection": "it-local", "schema": "mcp_it", "table": "x' or '1'='1"}})
	if !isErr || !strings.Contains(text, "not found") {
		t.Fatalf("따옴표는 리터럴로 처리해야 한다: %s", text)
	}

	var out resultOutput
	text, isErr = cs.callTool(call{"query", map[string]any{"connection": "it-local", "sql": "select id from mcp_it.t order by id"}})
	if isErr || json.Unmarshal([]byte(text), &out) != nil || len(out.Rows) != 20 || !out.Truncated {
		t.Fatalf("Options.MaxRows 로 잘라야 한다: %s", text)
	}
	text, isErr = cs.callTool(call{"query", map[string]any{"connection": "it-local", "sql": "select id from mcp_it.t order by id", "max_rows": 5}})
	if isErr || json.Unmarshal([]byte(text), &out) != nil || len(out.Rows) != 5 {
		t.Fatalf("max_rows: %s", text)
	}
	for _, sql := range []string{
		"with d as (delete from mcp_it.t returning id) select * from d",
		"select 1; delete from mcp_it.t",
		"select pg_notify('mcp_it', 'x')",
		"select pg_catalog.pg_try_advisory_lock(424242)",
	} {
		if text, isErr := cs.callTool(call{"query", map[string]any{"connection": "it-local", "sql": sql}}); !isErr {
			t.Fatalf("서버가 거부해야 한다: %s → %s", sql, text)
		}
	}
	text, _ = cs.callTool(call{"query", map[string]any{"connection": "it-local", "sql": "select count(*) from mcp_it.t"}})
	if !strings.Contains(text, `[["100"]]`) {
		t.Fatalf("쓰기가 반영되면 안 된다: %s", text)
	}
}

func TestIntegration_Redis_도구(t *testing.T) {
	proj := integrationProject(t)
	setup := db.NewClients()
	defer setup.Close()
	r, err := setup.Redis(context.Background(), proj.Conns[1])
	if err != nil {
		t.Fatal(err)
	}
	for _, args := range [][]string{{"DEL", "mcp:h", "mcp:s"}, {"HSET", "mcp:h", "f", "v"}, {"SET", "mcp:s", "hello"}} {
		if _, err := r.Run(context.Background(), args); err != nil {
			t.Fatal(err)
		}
	}
	cs := connect(t, newHandler(proj, Options{}))
	text, isErr := cs.callTool(call{"redis_scan", map[string]any{"connection": "it-redis", "pattern": "mcp:*"}})
	if isErr || !strings.Contains(text, `"mcp:h"`) || !strings.Contains(text, `"mcp:s"`) {
		t.Fatalf("redis_scan: %s", text)
	}
	text, isErr = cs.callTool(call{"redis_scan", map[string]any{"connection": "it-redis", "pattern": "mcp:*", "limit": 1}})
	if isErr || !strings.Contains(text, `"truncated":true`) {
		t.Fatalf("limit: %s", text)
	}
	text, isErr = cs.callTool(call{"redis_get", map[string]any{"connection": "it-redis", "key": "mcp:h"}})
	if isErr || !strings.Contains(text, `["f","v"]`) {
		t.Fatalf("redis_get: %s", text)
	}
	text, isErr = cs.callTool(call{"redis_command", map[string]any{"connection": "it-redis", "args": []string{"GET", "mcp:s"}}})
	if isErr || !strings.Contains(text, "hello") {
		t.Fatalf("redis_command: %s", text)
	}
	if text, isErr = cs.callTool(call{"redis_command", map[string]any{"connection": "it-redis", "args": []string{"DEL", "mcp:s"}}}); !isErr {
		t.Fatalf("DEL 은 거부해야 한다: %s", text)
	}
}
