package db

import (
	"context"
	"os"
	"slices"
	"strconv"
	"strings"
	"testing"
	"time"

	"github.com/jackc/pgx/v5"
)

// cell 은 findRow 가 찾을 열 번호와 값이다.
type cell struct {
	col int
	v   string
}

func TestActivity_다른_세션을_보고_자기_연결은_뺀다(t *testing.T) {
	p := opsPG(t)
	other := otherSession(t, "dbtui-it-activity")
	res, err := p.Activity(ctx(t))
	if err != nil {
		t.Fatal(err)
	}
	want := []string{"pid", "usename", "application_name", "client_addr", "state", "wait", "xact_age", "query_age", "query"}
	if !slices.Equal(res.Cols, want) {
		t.Fatalf("cols: %q", res.Cols)
	}
	if res.Types[0] != "int4" {
		t.Errorf("pid 는 숫자 열이어야 정렬된다: %q", res.Types)
	}
	if row := findRow(res, cell{0, strconv.Itoa(int(other.PgConn().PID()))}); row == nil || row[2] != "dbtui-it-activity" {
		t.Fatalf("다른 세션이 없다: %q", res.Rows)
	}
	// 풀의 다른 연결은 보일 수 있다. 조회를 돌린 연결 자신만 빠진다
	for _, r := range res.Rows {
		if strings.Contains(r[8], "pg_stat_activity where pid <> pg_backend_pid()") {
			t.Fatalf("자기 조회가 보인다: %q", r)
		}
	}
}

func TestActivity_다른_세션의_비밀번호가_든_쿼리는_가린다(t *testing.T) {
	p := opsPG(t)
	other := otherSession(t, "dbtui-it-secret")
	// ALTER ROLE … PASSWORD 를 계속 실행 중으로 둘 수 없어 같은 패턴을 주석에 담아 마지막 쿼리로 남긴다
	if _, err := other.Exec(ctx(t), "select 1 /* alter role x password 'hunter2' */"); err != nil {
		t.Fatal(err)
	}
	res, err := p.Activity(ctx(t))
	if err != nil {
		t.Fatal(err)
	}
	row := findRow(res, cell{0, strconv.Itoa(int(other.PgConn().PID()))})
	if row == nil || row[8] != MaskedSQL {
		t.Fatalf("가려야 한다: %q", row)
	}
}

func TestMaskQueryCols_쿼리_글_열만_가린다(t *testing.T) {
	secret := "ALTER ROLE app PASSWORD 'hunter2'"
	res := &Result{
		Cols: []string{"pid", "blocked_query", "blocking_query", "query", "note"},
		Rows: [][]string{{"1", secret, "select 1", secret, secret}},
	}
	maskQueryCols(res)
	if want := []string{"1", MaskedSQL, "select 1", MaskedSQL, secret}; !slices.Equal(res.Rows[0], want) {
		t.Fatalf("got %q", res.Rows[0])
	}
}

func TestLocks_막힌_세션과_막는_세션을_쌍으로_보여준다(t *testing.T) {
	p := opsPG(t)
	c := ctx(t)
	if _, err := p.pool.Exec(c, "drop table if exists ops_lock; create table ops_lock (id int primary key); insert into ops_lock values (1)"); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		cctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
		defer cancel()
		if _, err := p.pool.Exec(cctx, "drop table if exists ops_lock"); err != nil {
			t.Errorf("cleanup: %v", err)
		}
	})
	holder := otherSession(t, "dbtui-it-holder")
	waiter := otherSession(t, "dbtui-it-waiter")
	tx, err := holder.Begin(c)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := tx.Exec(c, "update ops_lock set id = 1 where id = 1"); err != nil {
		t.Fatal(err)
	}
	done := make(chan error, 1)
	go func() {
		_, err := waiter.Exec(c, "update ops_lock set id = 2 where id = 1")
		done <- err
	}()
	waiterPID := strconv.Itoa(int(waiter.PgConn().PID()))
	holderPID := strconv.Itoa(int(holder.PgConn().PID()))
	var res *Result
	for range 50 {
		if res, err = p.Locks(c); err != nil {
			t.Fatal(err)
		}
		if findRow(res, cell{0, waiterPID}) != nil {
			break
		}
		time.Sleep(100 * time.Millisecond)
	}
	row := findRow(res, cell{0, waiterPID})
	if row == nil {
		t.Fatalf("막힌 쌍이 없다: %q", res.Rows)
	}
	if row[4] != holderPID || row[6] != "idle in transaction" || !strings.Contains(row[7], "update ops_lock set id = 2") {
		t.Fatalf("쌍이 틀리다: %q (holder %s)", row, holderPID)
	}
	if !strings.HasPrefix(res.Text, "pg_locks: ") || !strings.Contains(res.Text, "waiting") {
		t.Fatalf("잠금 요약: %q", res.Text)
	}
	if err := tx.Rollback(c); err != nil {
		t.Fatal(err)
	}
	if err := <-done; err != nil {
		t.Fatal(err)
	}
}

func TestTableSizes_큰_테이블부터_크기와_바이트를_보여준다(t *testing.T) {
	p := opsPG(t)
	c := ctx(t)
	if _, err := p.pool.Exec(c, `drop table if exists ops_big, ops_small;
		create table ops_big (id int primary key, note text); insert into ops_big select g, repeat('x', 3000) from generate_series(1, 2000) g;
		create table ops_small (id int); analyze ops_big`); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		cctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
		defer cancel()
		if _, err := p.pool.Exec(cctx, "drop table if exists ops_big, ops_small"); err != nil {
			t.Errorf("cleanup: %v", err)
		}
	})
	res, err := p.TableSizes(c)
	if err != nil {
		t.Fatal(err)
	}
	if res.Cols[8] != "total_bytes" || res.Types[8] != "int8" || res.Types[3] != "int8" {
		t.Fatalf("cols %q types %q", res.Cols, res.Types)
	}
	big, small := findRow(res, cell{1, "ops_big"}), findRow(res, cell{1, "ops_small"})
	if big == nil || small == nil {
		t.Fatalf("테이블이 없다: %q", res.Rows)
	}
	if big[3] != "2000" || small[3] != "NULL" {
		t.Errorf("행 추정: big %q small %q", big[3], small[3])
	}
	if big[6] == "0 bytes" || !strings.HasSuffix(big[7], "kB") && !strings.HasSuffix(big[7], "MB") {
		t.Errorf("toast·전체 크기: %q", big)
	}
	var prev int64 = 1 << 62
	for _, r := range res.Rows {
		n, err := strconv.ParseInt(r[8], 10, 64)
		if err != nil || n > prev {
			t.Fatalf("total_bytes 내림차순이 아니다: %q", res.Rows)
		}
		prev = n
	}
}

func TestCancelTerminate_pg_sleep_세션을_취소하고_끊는다(t *testing.T) {
	p := opsPG(t)
	c := ctx(t)
	sleeper := otherSession(t, "dbtui-it-sleep")
	pid := int(sleeper.PgConn().PID())
	done := make(chan error, 1)
	go func() {
		_, err := sleeper.Exec(c, "select pg_sleep(30)")
		done <- err
	}()
	p.waitActive(t, pid)
	if err := p.CancelBackend(c, pid); err != nil {
		t.Fatal(err)
	}
	if err := <-done; err == nil || !strings.Contains(err.Error(), "canceling statement") {
		t.Fatalf("취소되지 않았다: %v", err)
	}
	go func() {
		_, err := sleeper.Exec(c, "select pg_sleep(30)")
		done <- err
	}()
	p.waitActive(t, pid)
	if err := p.TerminateBackend(c, pid); err != nil {
		t.Fatal(err)
	}
	if err := <-done; err == nil {
		t.Fatal("끊기지 않았다")
	}
	if err := p.CancelBackend(c, 1); err == nil || !strings.Contains(err.Error(), "not signaled") {
		t.Fatalf("없는 pid 는 오류여야 한다: %v", err)
	}
}

func opsPG(t *testing.T) *PGConn {
	t.Helper()
	cs := NewClients()
	t.Cleanup(cs.Close)
	p, err := cs.PG(ctx(t), testPGConn(t))
	if err != nil {
		t.Fatal(err)
	}
	return p
}

// otherSession 은 풀과 별개인 연결 하나다. 다른 사용자의 세션 역할을 한다.
func otherSession(t *testing.T, app string) *pgx.Conn {
	t.Helper()
	cfg, err := pgx.ParseConfig(os.Getenv("DBTUI_TEST_PG_URL") + "?sslmode=disable")
	if err != nil {
		t.Fatal(err)
	}
	cfg.RuntimeParams["application_name"] = app
	conn, err := pgx.ConnectConfig(ctx(t), cfg)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		cctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		_ = conn.Close(cctx) // 끊긴 연결을 닫을 때 나는 오류는 테스트 대상이 아니다
	})
	return conn
}

func (p *PGConn) waitActive(t *testing.T, pid int) {
	t.Helper()
	for range 50 {
		res, err := p.Activity(ctx(t))
		if err != nil {
			t.Fatal(err)
		}
		if row := findRow(res, cell{0, strconv.Itoa(pid)}); row != nil && row[4] == "active" {
			return
		}
		time.Sleep(50 * time.Millisecond)
	}
	t.Fatalf("pid %d 가 active 가 되지 않았다", pid)
}

func findRow(res *Result, want cell) []string {
	for _, r := range res.Rows {
		if r[want.col] == want.v {
			return r
		}
	}
	return nil
}
