package db

import (
	"context"
	"net"
	"os"
	"slices"
	"strconv"
	"strings"
	"testing"
	"time"
)

// DBTUI_TEST_PG_URL=postgres://user:pass@host:port/db, DBTUI_TEST_REDIS_ADDR=host:port 가 있을 때만 돈다.

func testPGConn(t *testing.T) Conn {
	t.Helper()
	raw := os.Getenv("DBTUI_TEST_PG_URL")
	if raw == "" {
		t.Skip("DBTUI_TEST_PG_URL not set")
	}
	rest := strings.TrimPrefix(raw, "postgres://")
	cred, hostdb, _ := strings.Cut(rest, "@")
	user, pass, _ := strings.Cut(cred, ":")
	hp, db, _ := strings.Cut(hostdb, "/")
	host, port, _ := net.SplitHostPort(hp)
	p, _ := strconv.Atoi(port)
	return Conn{Name: "it-local", UUID: "it-pg", Kind: KindPG, Host: host, Port: p, DB: db, User: user, Password: pass}
}

func testRedisConn(t *testing.T) Conn {
	t.Helper()
	addr := os.Getenv("DBTUI_TEST_REDIS_ADDR")
	if addr == "" {
		t.Skip("DBTUI_TEST_REDIS_ADDR not set")
	}
	host, port, _ := net.SplitHostPort(addr)
	p, _ := strconv.Atoi(port)
	return Conn{Name: "it-redis", UUID: "it-redis", Kind: KindRedis, Host: host, Port: p, DB: "0"}
}

func ctx(t *testing.T) context.Context {
	c, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	t.Cleanup(cancel)
	return c
}

func TestPG_스키마_테이블_쿼리(t *testing.T) {
	c := testPGConn(t)
	cs := NewClients()
	defer cs.Close()
	pg, err := cs.PG(ctx(t), c)
	must(t, err)

	_, err = pg.Run(ctx(t), `drop schema if exists it cascade`)
	must(t, err)
	_, err = pg.Run(ctx(t), `create schema it`)
	must(t, err)
	_, err = pg.Run(ctx(t), `create table it.t (id int, name text)`)
	must(t, err)
	res, err := pg.Run(ctx(t), `insert into it.t select g, case when g % 2 = 0 then null else 'n' || g end from generate_series(1, 1500) g`)
	must(t, err)
	if res.Text != "INSERT 0 1500" || len(res.Cols) != 0 {
		t.Fatalf("명령 태그: %+v", res)
	}

	schemas, err := pg.FindSchemas(ctx(t))
	must(t, err)
	if !slices.Contains(schemas, "it") || slices.Contains(schemas, "pg_catalog") || slices.Contains(schemas, "information_schema") {
		t.Fatalf("스키마 목록: %v", schemas)
	}
	tables, err := pg.FindTables(ctx(t), "it")
	must(t, err)
	if len(tables) != 1 || tables[0] != "t" {
		t.Fatalf("테이블 목록: %v", tables)
	}

	res, err = pg.Run(ctx(t), `select id, name from it.t order by id`)
	must(t, err)
	if len(res.Rows) != MaxRows || !res.Truncated || res.Cols[1] != "name" {
		t.Fatalf("1,000행에서 잘라야 한다: rows=%d truncated=%v", len(res.Rows), res.Truncated)
	}
	if res.Rows[0][0] != "1" || res.Rows[0][1] != "n1" || res.Rows[1][1] != "NULL" {
		t.Fatalf("값: %v %v", res.Rows[0], res.Rows[1])
	}

	// 잘라낸 뒤에도 같은 풀로 다음 쿼리가 된다
	res, err = pg.Run(ctx(t), `select count(*) from it.t`)
	must(t, err)
	if res.Rows[0][0] != "1500" {
		t.Fatalf("count: %v", res.Rows)
	}

	_, err = pg.Run(ctx(t), `select * from it.nope`)
	if err == nil || !strings.Contains(err.Error(), "does not exist") {
		t.Fatalf("PostgreSQL 오류를 그대로 돌려줘야 한다: %v", err)
	}
}

func TestPG_실행_중_취소(t *testing.T) {
	c := testPGConn(t)
	cs := NewClients()
	defer cs.Close()
	pg, err := cs.PG(ctx(t), c)
	must(t, err)
	qctx, cancel := context.WithCancel(context.Background())
	time.AfterFunc(300*time.Millisecond, cancel)
	start := time.Now()
	_, err = pg.Run(qctx, `select pg_sleep(10)`)
	if err == nil || time.Since(start) > 5*time.Second {
		t.Fatalf("취소돼야 한다: err=%v elapsed=%s", err, time.Since(start))
	}
}

func TestPG_비밀번호가_틀리면_오류에_비밀번호를_싣지_않는다(t *testing.T) {
	c := testPGConn(t)
	c.Password = "wrong-secret-xyz"
	_, err := NewClients().PG(ctx(t), c)
	if err == nil || strings.Contains(err.Error(), "wrong-secret-xyz") {
		t.Fatalf("오류: %v", err)
	}
}

func TestRedis_키_목록_값_명령(t *testing.T) {
	c := testRedisConn(t)
	cs := NewClients()
	defer cs.Close()
	rc, err := cs.Redis(ctx(t), c)
	must(t, err)
	cl := rc.cl
	must(t, cl.FlushDB(ctx(t)).Err())
	must(t, cl.Set(ctx(t), "it:s", `{"a":1}`, time.Hour).Err())
	must(t, cl.HSet(ctx(t), "it:h", "f2", "v2", "f1", "v1").Err())
	must(t, cl.RPush(ctx(t), "it:l", "x", "y").Err())
	must(t, cl.Set(ctx(t), "other", "1", 0).Err())

	keys, more, err := rc.GetKeys(ctx(t), "it:*")
	must(t, err)
	if more || strings.Join(keys, ",") != "it:h,it:l,it:s" {
		t.Fatalf("SCAN MATCH it:* 결과: %v", keys)
	}

	v, err := rc.GetValue(ctx(t), "it:s")
	must(t, err)
	if !strings.Contains(v.Text, "string") || !strings.Contains(v.Text, "\"a\": 1") {
		t.Fatalf("string 값은 JSON 을 들여써야 한다: %q", v.Text)
	}
	v, err = rc.GetValue(ctx(t), "it:h")
	must(t, err)
	if len(v.Rows) != 2 || v.Rows[0][0] != "f1" || !strings.Contains(v.Text, "TTL ∞") {
		t.Fatalf("hash: %+v", v)
	}

	r, err := rc.Run(ctx(t), SplitArgs(`LRANGE it:l 0 -1`))
	must(t, err)
	if len(r.Rows) != 2 || r.Rows[1][1] != "y" {
		t.Fatalf("LRANGE: %+v", r)
	}
	r, err = rc.Run(ctx(t), SplitArgs(`GET nope`))
	must(t, err)
	if r.Text != "(nil)" {
		t.Fatalf("없는 키: %q", r.Text)
	}
}

func TestRedis_평문이_안_되면_TLS_까지_시도하고_둘_다_실패를_알린다(t *testing.T) {
	// 아무도 듣지 않는 포트: 평문·TLS 모두 실패해야 한다
	l, err := net.Listen("tcp", "127.0.0.1:0")
	must(t, err)
	addr := l.Addr().(*net.TCPAddr)
	l.Close()
	_, err = NewClients().Redis(ctx(t), Conn{Name: "x", UUID: "x", Kind: KindRedis, Host: "127.0.0.1", Port: addr.Port})
	if err == nil || !strings.Contains(err.Error(), "plain:") || !strings.Contains(err.Error(), "tls:") {
		t.Fatalf("오류: %v", err)
	}
}

func TestRedis_큰_hash_와_string_은_상한까지만_읽는다(t *testing.T) {
	c := testRedisConn(t)
	cs := NewClients()
	defer cs.Close()
	rc, err := cs.Redis(ctx(t), c)
	must(t, err)
	must(t, rc.cl.FlushDB(ctx(t)).Err())
	kv := make([]any, 0, 2*(MaxRows+300))
	for i := 0; i < MaxRows+300; i++ {
		kv = append(kv, "f"+strconv.Itoa(i), "v")
	}
	must(t, rc.cl.HSet(ctx(t), "big:h", kv...).Err())
	must(t, rc.cl.Set(ctx(t), "big:s", strings.Repeat("x", maxStringBytes+10), 0).Err())

	h, err := rc.GetValue(ctx(t), "big:h")
	must(t, err)
	if !h.Truncated || len(h.Rows) < MaxRows || len(h.Rows) > MaxRows+500 {
		t.Fatalf("hash 는 HSCAN 으로 상한 근처에서 멈춰야 한다: rows=%d truncated=%v", len(h.Rows), h.Truncated)
	}
	s, err := rc.GetValue(ctx(t), "big:s")
	must(t, err)
	if !s.Truncated || !strings.Contains(s.Text, "앞 1048576바이트만") || len(s.Text) > maxStringBytes+500 {
		t.Fatalf("string 은 앞 1MiB 만: len=%d truncated=%v", len(s.Text), s.Truncated)
	}
}

func TestPG_큰_결과는_1000행에서_끊고_남은_행을_기다리지_않는다(t *testing.T) {
	if raceOn {
		t.Skip("속도 측정은 -race 빌드에서 의미가 없다")
	}
	c := testPGConn(t)
	cs := NewClients()
	defer cs.Close()
	pg, err := cs.PG(ctx(t), c)
	must(t, err)
	start := time.Now()
	res, err := pg.Run(ctx(t), `select g, repeat('x', 200) from generate_series(1, 20000000) g`)
	must(t, err)
	if !res.Truncated || time.Since(start) > 3*time.Second {
		t.Fatalf("truncated=%v elapsed=%s", res.Truncated, time.Since(start))
	}
	// 끊은 뒤에도 풀이 다음 쿼리를 받는다
	_, err = pg.Run(ctx(t), `select 1`)
	must(t, err)
}

func TestRedis_키가_1000개를_넘으면_더_있음으로_알린다(t *testing.T) {
	c := testRedisConn(t)
	cs := NewClients()
	defer cs.Close()
	rc, err := cs.Redis(ctx(t), c)
	must(t, err)
	must(t, rc.cl.FlushDB(ctx(t)).Err())
	kv := make([]any, 0, 2*(MaxRows+200))
	for i := 0; i < MaxRows+200; i++ {
		kv = append(kv, "k:"+strconv.Itoa(i), "v")
	}
	must(t, rc.cl.MSet(ctx(t), kv...).Err())
	keys, more, err := rc.GetKeys(ctx(t), "k:*")
	must(t, err)
	if !more || len(keys) != MaxRows {
		t.Fatalf("more=%v keys=%d", more, len(keys))
	}
}

// DBTUI_TEST_PROJECT=<DataGrip 프로젝트 경로> 와 DBTUI_TEST_CONNS=<쉼표로 나눈 접속 이름>(예: PG 하나, TLS Redis 하나,
// 평문 Redis 하나)가 있을 때만 그 접속에 읽기 전용으로 붙어 본다(명세 R4.3 실측).
func TestStg_실접속_PG_와_TLS_평문_Redis(t *testing.T) {
	dir, conns := os.Getenv("DBTUI_TEST_PROJECT"), os.Getenv("DBTUI_TEST_CONNS")
	if dir == "" || conns == "" {
		t.Skip("DBTUI_TEST_PROJECT or DBTUI_TEST_CONNS not set")
	}
	p, err := LoadProject(dir)
	must(t, err)
	cs := NewClients()
	defer cs.Close()
	for _, name := range strings.Split(conns, ",") {
		i := slices.IndexFunc(p.Conns, func(c Conn) bool { return c.Name == name })
		if i < 0 {
			t.Fatalf("%s 접속이 없다", name)
		}
		c := p.Conns[i]
		if c.Kind == KindPG {
			pg, err := cs.PG(ctx(t), c)
			must(t, err)
			res, err := pg.Run(ctx(t), "select 1")
			must(t, err)
			if res.Rows[0][0] != "1" {
				t.Fatalf("%s: %v", name, res.Rows)
			}
			continue
		}
		rc, err := cs.Redis(ctx(t), c)
		must(t, err)
		res, err := rc.Run(ctx(t), []string{"PING"})
		must(t, err)
		if res.Text != "PONG" {
			t.Fatalf("%s: %q", name, res.Text)
		}
		t.Logf("%s tls=%v", name, rc.cl.Options().TLSConfig != nil)
	}
}

func TestRedis_응답_없는_서버는_평문_3초_TLS_3초_안에_포기한다(t *testing.T) {
	// 접속은 받지만 아무 말도 하지 않는 서버. TLS 전용 서버에 평문으로 붙었을 때와 같다.
	l, err := net.Listen("tcp", "127.0.0.1:0")
	must(t, err)
	defer l.Close()
	go func() {
		for {
			c, err := l.Accept()
			if err != nil {
				return
			}
			defer c.Close()
		}
	}()
	start := time.Now()
	_, err = NewClients().Redis(ctx(t), Conn{Name: "silent", UUID: "silent", Kind: KindRedis, Host: "127.0.0.1", Port: l.Addr().(*net.TCPAddr).Port})
	if err == nil || time.Since(start) > 8*time.Second {
		t.Fatalf("err=%v elapsed=%s", err, time.Since(start))
	}
}

func TestRedis_명령은_컨텍스트_취소로_끊긴다(t *testing.T) {
	c := testRedisConn(t)
	cs := NewClients()
	defer cs.Close()
	rc, err := cs.Redis(ctx(t), c)
	must(t, err)
	qctx, cancel := context.WithCancel(context.Background())
	time.AfterFunc(300*time.Millisecond, cancel)
	start := time.Now()
	_, err = rc.Run(qctx, []string{"BLPOP", "never", "10"})
	if err == nil || time.Since(start) > 3*time.Second {
		t.Fatalf("err=%v elapsed=%s", err, time.Since(start))
	}
}

func TestPG_여러_문장은_실행하지_않는다(t *testing.T) {
	c := testPGConn(t)
	cs := NewClients()
	defer cs.Close()
	pg, err := cs.PG(ctx(t), c)
	must(t, err)
	_, err = pg.Run(ctx(t), `create table if not exists it_multi(a int)`)
	must(t, err)
	// 커서 경로(select)와 일반 경로(update) 둘 다 막아야 한다
	for _, q := range []string{`select 1; drop table it_multi`, `update it_multi set a = 1; drop table it_multi`} {
		if _, err := pg.Run(ctx(t), q); err == nil {
			t.Fatalf("여러 문장은 거부돼야 한다: %s", q)
		}
	}
	res, err := pg.Run(ctx(t), `select count(*) from pg_tables where tablename = 'it_multi'`)
	must(t, err)
	if res.Rows[0][0] != "1" {
		t.Fatal("뒤 문장(drop)이 실행됐다")
	}
}

func TestPG_편집기_세션은_트랜잭션을_이어간다(t *testing.T) {
	c := testPGConn(t)
	cs := NewClients()
	defer cs.Close()
	pg, err := cs.PG(ctx(t), c)
	must(t, err)
	for _, q := range []string{`drop table if exists it_tx`, `create table it_tx(a int)`} {
		_, err = pg.Run(ctx(t), q)
		must(t, err)
	}
	res, err := pg.Run(ctx(t), `begin`)
	must(t, err)
	if !res.InTx {
		t.Fatal("begin 뒤에는 트랜잭션 열림")
	}
	_, err = pg.Run(ctx(t), `insert into it_tx select generate_series(1, 3)`)
	must(t, err)
	// 트랜잭션 안에서 1,000행을 넘는 읽기를 잘라도 트랜잭션이 깨지지 않아야 한다
	res, err = pg.Run(ctx(t), `select g from generate_series(1, 5000) g`)
	must(t, err)
	if !res.Truncated || !res.InTx {
		t.Fatalf("truncated=%v inTx=%v", res.Truncated, res.InTx)
	}
	res, err = pg.Run(ctx(t), `select count(*) from it_tx`)
	must(t, err)
	if res.Rows[0][0] != "3" {
		t.Fatalf("같은 세션에서 보여야 한다: %v", res.Rows)
	}
	_, err = pg.Run(ctx(t), `rollback`)
	must(t, err)
	res, err = pg.Run(ctx(t), `select count(*) from it_tx`)
	must(t, err)
	if res.Rows[0][0] != "0" || res.InTx {
		t.Fatalf("rollback 뒤 0행·트랜잭션 닫힘이어야 한다: %v inTx=%v", res.Rows, res.InTx)
	}
}

func TestPG_RETURNING_쓰기는_1000행을_넘어도_끝까지_반영한다(t *testing.T) {
	c := testPGConn(t)
	cs := NewClients()
	defer cs.Close()
	pg, err := cs.PG(ctx(t), c)
	must(t, err)
	for _, q := range []string{`drop table if exists it_ret`, `create table it_ret(a int)`} {
		_, err = pg.Run(ctx(t), q)
		must(t, err)
	}
	res, err := pg.Run(ctx(t), `insert into it_ret select generate_series(1, 1500) returning a`)
	must(t, err)
	if !res.Truncated || len(res.Rows) != MaxRows {
		t.Fatalf("표시는 1000행: %d", len(res.Rows))
	}
	res, err = pg.Run(ctx(t), `select count(*) from it_ret`)
	must(t, err)
	if res.Rows[0][0] != "1500" {
		t.Fatalf("쓰기는 끝까지 반영돼야 한다: %v", res.Rows)
	}
}

func TestRedis_편집기_세션은_SELECT_를_이어간다(t *testing.T) {
	c := testRedisConn(t)
	cs := NewClients()
	defer cs.Close()
	rc, err := cs.Redis(ctx(t), c)
	must(t, err)
	must(t, rc.cl.FlushAll(ctx(t)).Err())
	for _, cmd := range []string{"SELECT 3", "SET only:db3 1"} {
		_, err := rc.Run(ctx(t), SplitArgs(cmd))
		must(t, err)
	}
	if n := rc.cl.Exists(ctx(t), "only:db3").Val(); n != 0 {
		t.Fatal("db0 에는 없어야 한다")
	}
	res, err := rc.Run(ctx(t), SplitArgs("GET only:db3"))
	must(t, err)
	if res.Text != "1" {
		t.Fatalf("같은 세션(db3)에서 보여야 한다: %q", res.Text)
	}
}

func TestPG_큰_조회를_잘라도_SET_세션_상태가_남는다(t *testing.T) {
	c := testPGConn(t)
	cs := NewClients()
	defer cs.Close()
	pg, err := cs.PG(ctx(t), c)
	must(t, err)
	for _, q := range []string{`create schema if not exists it_sp`, `set search_path to it_sp, public`} {
		_, err = pg.Run(ctx(t), q)
		must(t, err)
	}
	res, err := pg.Run(ctx(t), `select g from generate_series(1, 20000000) g`)
	must(t, err)
	if !res.Truncated || len(res.Rows) != MaxRows || res.InTx {
		t.Fatalf("truncated=%v rows=%d inTx=%v", res.Truncated, len(res.Rows), res.InTx)
	}
	res, err = pg.Run(ctx(t), `show search_path`)
	must(t, err)
	if !strings.HasPrefix(res.Rows[0][0], "it_sp") {
		t.Fatalf("SET 이 사라졌다: %v", res.Rows)
	}
}

func TestPG_세션_연결이_끊기면_한_번은_실행하지_않고_알린다(t *testing.T) {
	c := testPGConn(t)
	cs := NewClients()
	defer cs.Close()
	pg, err := cs.PG(ctx(t), c)
	must(t, err)
	for _, q := range []string{`drop table if exists it_lost`, `create table it_lost(a int)`, `begin`, `insert into it_lost values (1)`} {
		_, err = pg.Run(ctx(t), q)
		must(t, err)
	}
	pid, err := pg.Run(ctx(t), `select pg_backend_pid()`)
	must(t, err)
	_, err = pg.pool.Exec(ctx(t), `select pg_terminate_backend($1::int)`, pid.Rows[0][0])
	must(t, err)
	time.Sleep(200 * time.Millisecond)
	// 끊긴 걸 처음 알아채는 실행은 실패한다(연결 오류). 그다음 실행은 세션 유실을 알리고 insert 를 하지 않는다.
	var errs []string
	for i := 0; i < 3; i++ {
		_, err := pg.Run(ctx(t), `insert into it_lost values (2)`)
		if err == nil {
			break
		}
		errs = append(errs, err.Error())
	}
	if !strings.Contains(strings.Join(errs, " | "), "세션 상태가 사라졌다") {
		t.Fatalf("세션 유실을 알려야 한다: %v", errs)
	}
	res, err := pg.Run(ctx(t), `select count(*) from it_lost`)
	must(t, err)
	if res.Rows[0][0] != "1" {
		t.Fatalf("끊긴 트랜잭션은 롤백, 알린 뒤 실행분 1행만 있어야 한다: %v", res.Rows)
	}
}

func TestPG_WITH_쓰기는_1000행을_넘어도_끝까지_반영한다(t *testing.T) {
	c := testPGConn(t)
	cs := NewClients()
	defer cs.Close()
	pg, err := cs.PG(ctx(t), c)
	must(t, err)
	for _, q := range []string{`drop table if exists it_cte`, `create table it_cte(a int)`} {
		_, err = pg.Run(ctx(t), q)
		must(t, err)
	}
	_, err = pg.Run(ctx(t), `with src as (select generate_series(1, 1500) a) insert into it_cte select a from src returning a`)
	must(t, err)
	res, err := pg.Run(ctx(t), `select count(*) from it_cte`)
	must(t, err)
	if res.Rows[0][0] != "1500" {
		t.Fatalf("CTE 쓰기는 끝까지 반영: %v", res.Rows)
	}
}

func TestRedis_세션_연결이_끊기면_다른_DB_로_몰래_보내지_않는다(t *testing.T) {
	c := testRedisConn(t)
	cs := NewClients()
	defer cs.Close()
	rc, err := cs.Redis(ctx(t), c)
	must(t, err)
	must(t, rc.cl.FlushAll(ctx(t)).Err())
	_, err = rc.Run(ctx(t), SplitArgs("SELECT 3"))
	must(t, err)
	id, err := rc.Run(ctx(t), SplitArgs("CLIENT ID"))
	must(t, err)
	must(t, rc.cl.Do(ctx(t), "CLIENT", "KILL", "ID", id.Text).Err())
	var errs []string
	for i := 0; i < 4; i++ {
		_, err := rc.Run(ctx(t), SplitArgs("SET moved 1"))
		if err == nil {
			break
		}
		if rc.cl.Exists(ctx(t), "moved").Val() != 0 {
			t.Fatal("알리기 전에 db0 으로 실행됐다")
		}
		errs = append(errs, err.Error())
	}
	if !strings.Contains(strings.Join(errs, " | "), "세션 상태가 사라졌다") {
		t.Fatalf("세션 유실을 알려야 한다: %v", errs)
	}
}

func TestPG_트랜잭션_안에서_실패하면_결과에_InTx_를_싣는다(t *testing.T) {
	c := testPGConn(t)
	cs := NewClients()
	defer cs.Close()
	pg, err := cs.PG(ctx(t), c)
	must(t, err)
	_, err = pg.Run(ctx(t), `begin`)
	must(t, err)
	res, err := pg.Run(ctx(t), `select * from it_nope_table`)
	if err == nil || res == nil || !res.InTx {
		t.Fatalf("err=%v res=%+v", err, res)
	}
	_, err = pg.Run(ctx(t), `rollback`)
	must(t, err)
}

func TestFormatRedis_큰_문자열과_중첩_맵은_예산까지만_만든다(t *testing.T) {
	big := strings.Repeat("x", 8<<20)
	out := formatRedis([]any{big, map[any]any{"k": big}})
	if len(out) > MaxCellBytes+100 || !strings.HasSuffix(out, "…(잘림)") {
		t.Fatalf("len=%d", len(out))
	}
}

func TestPG_같은_세션에서_열_개수가_다른_select를_이어_실행해도_열_정보가_맞다(t *testing.T) {
	c := testPGConn(t)
	cs := NewClients()
	defer cs.Close()
	pg, err := cs.PG(ctx(t), c)
	must(t, err)
	for _, q := range []string{`select 1 as a`, `select 'x' as b, 2 as c`, `select 1 as d, 2 as e, 3 as f`} {
		res, err := pg.Run(ctx(t), q)
		must(t, err)
		if len(res.Rows) != 1 || len(res.Cols) != len(res.Rows[0]) {
			t.Fatalf("%s: cols=%v row=%v", q, res.Cols, res.Rows)
		}
	}
	res, err := pg.Run(ctx(t), `select 'x' as b, 2 as c`)
	must(t, err)
	if strings.Join(res.Cols, ",") != "b,c" {
		t.Fatalf("열 이름이 옛것이다: %v", res.Cols)
	}
}

func TestRedis_정확히_1000개인_list는_잘림이_아니다(t *testing.T) {
	c := testRedisConn(t)
	cs := NewClients()
	defer cs.Close()
	rc, err := cs.Redis(ctx(t), c)
	must(t, err)
	must(t, rc.cl.FlushDB(ctx(t)).Err())
	vals := make([]any, MaxRows)
	for i := range vals {
		vals[i] = i
	}
	must(t, rc.cl.RPush(ctx(t), "l1000", vals...).Err())
	res, err := rc.GetValue(ctx(t), "l1000")
	must(t, err)
	if res.Truncated || len(res.Rows) != MaxRows {
		t.Fatalf("truncated=%v rows=%d", res.Truncated, len(res.Rows))
	}
	must(t, rc.cl.RPush(ctx(t), "l1000", "one-more").Err())
	res, err = rc.GetValue(ctx(t), "l1000")
	must(t, err)
	if !res.Truncated || len(res.Rows) != MaxRows {
		t.Fatalf("1001개면 잘림: truncated=%v rows=%d", res.Truncated, len(res.Rows))
	}
}

func TestRedis_평문_시도에는_비밀번호를_보내지_않는다(t *testing.T) {
	l, err := net.Listen("tcp", "127.0.0.1:0")
	must(t, err)
	defer l.Close()
	got := make(chan []byte, 4)
	go func() {
		for {
			c, err := l.Accept()
			if err != nil {
				return
			}
			go func(c net.Conn) {
				defer c.Close()
				buf := make([]byte, 4096)
				_ = c.SetReadDeadline(time.Now().Add(2 * time.Second))
				n, _ := c.Read(buf) // 테스트용 가짜 서버: 받은 만큼만 본다
				got <- buf[:n]
			}(c)
		}
	}()
	_, err = NewClients().Redis(ctx(t), Conn{Name: "x", UUID: "x", Kind: KindRedis, Host: "127.0.0.1", Port: l.Addr().(*net.TCPAddr).Port, Password: "s3cret-xyz"})
	if err == nil {
		t.Fatal("응답 없는 서버라 실패해야 한다")
	}
	// 평문 시도와 TLS 시도, 두 연결이 받은 내용을 본다
	for i := 0; i < 2; i++ {
		select {
		case b := <-got:
			if strings.Contains(string(b), "s3cret-xyz") {
				t.Fatalf("비밀번호가 평문으로 나갔다: %q", b)
			}
		case <-time.After(3 * time.Second):
			t.Fatal("가짜 서버가 연결을 받지 못했다")
		}
	}
}

func TestPG_결과에_열_타입_이름을_싣고_모르는_타입은_OID로_싣는다(t *testing.T) {
	c := testPGConn(t)
	cs := NewClients()
	defer cs.Close()
	pg, err := cs.PG(ctx(t), c)
	must(t, err)
	_, err = pg.Run(ctx(t), `drop type if exists dbtui_mood`)
	must(t, err)
	_, err = pg.Run(ctx(t), `create type dbtui_mood as enum ('ok')`)
	must(t, err)
	// cs.Close 보다 먼저 돌아야 하므로 t.Cleanup 이 아니라 defer 로 지운다
	defer func() {
		if _, err := pg.Run(context.Background(), `drop type dbtui_mood`); err != nil {
			t.Error(err)
		}
	}()
	res, err := pg.Run(ctx(t), `select 'dbtui_mood'::regtype::oid::text`)
	must(t, err)
	oid := res.Rows[0][0]

	// select 는 커서 경로, show 는 끝까지 받는 경로다
	res, err = pg.Run(ctx(t), `select 1::int4 as a, 'x'::text as b, true as c, now() as d, '{}'::jsonb as e, 'ok'::dbtui_mood as f`)
	must(t, err)
	if got, want := strings.Join(res.Types, ","), "int4,text,bool,timestamptz,jsonb,"+oid; got != want {
		t.Fatalf("got %s, want %s", got, want)
	}
	res, err = pg.Run(ctx(t), `show server_version`)
	must(t, err)
	if len(res.Types) != 1 || res.Types[0] != "text" {
		t.Fatalf("show 결과 타입: %v", res.Types)
	}
}
