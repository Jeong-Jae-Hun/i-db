package db

import (
	"context"
	"errors"
	"slices"
	"testing"
	"time"
)

func TestDatabases_접속할_수_있는_DB_만_보인다(t *testing.T) {
	p := opsPG(t)
	dbs := []string{"v4sql_open", "v4sql_tmpl", "v4sql_noconn"}
	dropTestDBs(t, p, dbs)
	for _, sql := range []string{
		"create database v4sql_open",
		"create database v4sql_tmpl is_template true",
		"create database v4sql_noconn allow_connections false",
	} {
		_, err := p.pool.Exec(ctx(t), sql)
		must(t, err)
	}
	t.Cleanup(func() { dropTestDBs(t, p, dbs) })
	got, err := p.Databases(ctx(t))
	must(t, err)
	if !slices.Contains(got, "v4sql_open") || !slices.Contains(got, "postgres") ||
		slices.Contains(got, "v4sql_tmpl") || slices.Contains(got, "v4sql_noconn") || slices.Contains(got, "template0") {
		t.Fatalf("목록: %v", got)
	}
	if !slices.IsSorted(got) {
		t.Fatalf("정렬돼야 한다: %v", got)
	}
}

func TestForget_다음_PG_는_바꾼_DB_로_새로_연다(t *testing.T) {
	c := testPGConn(t)
	cs := NewClients()
	t.Cleanup(cs.Close)
	old, err := cs.PG(ctx(t), c)
	must(t, err)
	dropTestDBs(t, old, []string{"v4sql_other"})
	_, err = old.pool.Exec(ctx(t), "create database v4sql_other")
	must(t, err)
	t.Cleanup(func() {
		cs2 := NewClients()
		defer cs2.Close()
		p, err := cs2.PG(ctx(t), c)
		must(t, err)
		dropTestDBs(t, p, []string{"v4sql_other"})
	})
	must(t, old.SetSearchPath(ctx(t), "v4sql"))

	c2 := c
	c2.DB = "v4sql_other"
	must(t, cs.Forget(ctx(t), c.UUID))
	if _, err := old.Run(ctx(t), "select 1"); !errors.Is(err, errClosed) {
		t.Fatalf("옛 핸들의 편집기 실행은 errClosed: %v", err)
	}
	p, err := cs.PG(ctx(t), c2)
	must(t, err)
	res, err := p.Run(ctx(t), "select current_database(), current_setting('search_path')")
	must(t, err)
	if res.Rows[0][0] != "v4sql_other" || res.Rows[0][1] == `"v4sql", public` || p.SearchPath() != "" {
		t.Fatalf("새 DB 에 search_path 없이 붙어야 한다: %v %q", res.Rows, p.SearchPath())
	}
	must(t, cs.Forget(ctx(t), "never-opened")) // 연 적 없는 uuid 는 할 일이 없다
}

func TestForget_실행_중이면_닫지_않고_오류를_돌려준다(t *testing.T) {
	c := testPGConn(t)
	cs := NewClients()
	t.Cleanup(cs.Close)
	p, err := cs.PG(ctx(t), c)
	must(t, err)
	done := make(chan error, 1)
	go func() {
		_, err := p.Run(ctx(t), "select pg_sleep(1)")
		done <- err
	}()
	time.Sleep(200 * time.Millisecond)
	fctx, cancel := context.WithTimeout(context.Background(), 200*time.Millisecond)
	defer cancel()
	if err := cs.Forget(fctx, c.UUID); err == nil {
		t.Fatal("실행 중인 세션이면 오류여야 한다")
	}
	must(t, <-done)
	if _, err := p.Run(ctx(t), "select 1"); err != nil {
		t.Fatalf("실패한 Forget 뒤에도 그대로 써야 한다: %v", err)
	}
}

func TestSetSearchPath_세션이_다시_열리거나_ROLLBACK_해도_유지한다(t *testing.T) {
	p := v4sqlPG(t, "create table v4sql.sp_t (id int); insert into v4sql.sp_t values (7);")
	must(t, p.SetSearchPath(ctx(t), "v4sql"))
	if p.SearchPath() != "v4sql" {
		t.Fatalf("SearchPath = %q", p.SearchPath())
	}
	res, err := p.Run(ctx(t), "select id from sp_t")
	must(t, err)
	if res.Rows[0][0] != "7" {
		t.Fatalf("rows: %v", res.Rows)
	}

	// 트랜잭션 안에서 고르고 ROLLBACK 해도 남는다
	_, err = p.Run(ctx(t), "begin")
	must(t, err)
	must(t, p.SetSearchPath(ctx(t), "public"))
	_, err = p.Run(ctx(t), "rollback")
	must(t, err)
	assertSearchPath(t, p, `public, public`)
	must(t, p.SetSearchPath(ctx(t), "v4sql"))

	// 세션 연결이 끊기면 한 번 알리고, 새 연결에 다시 보낸다
	res, err = p.Run(ctx(t), "select pg_backend_pid()")
	must(t, err)
	_, err = p.pool.Exec(ctx(t), "select pg_terminate_backend($1::int)", res.Rows[0][0])
	must(t, err)
	var lastErr error
	for range 3 {
		if _, lastErr = p.Run(ctx(t), "select 1"); lastErr == nil {
			break
		}
	}
	must(t, lastErr)
	assertSearchPath(t, p, `v4sql, public`)

	// 따옴표가 든 이름도 식별자로 감싼다(없는 스키마여도 SET 은 된다)
	must(t, p.SetSearchPath(ctx(t), `we"ird`))
	assertSearchPath(t, p, `"we""ird", public`)
}

func assertSearchPath(t *testing.T, p *PGConn, want string) {
	t.Helper()
	res, err := p.Run(ctx(t), "show search_path")
	must(t, err)
	if res.Rows[0][0] != want {
		t.Fatalf("search_path = %q, want %q", res.Rows[0][0], want)
	}
}

func dropTestDBs(t *testing.T, p *PGConn, names []string) {
	t.Helper()
	c, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	for _, n := range names {
		// 템플릿 DB 는 지울 수 없어 먼저 푼다
		_, _ = p.pool.Exec(c, "alter database "+QuoteIdent(n)+" is_template false") // 없으면 실패해도 된다
		if _, err := p.pool.Exec(c, "drop database if exists "+QuoteIdent(n)+" with (force)"); err != nil {
			t.Errorf("drop %s: %v", n, err)
		}
	}
}
