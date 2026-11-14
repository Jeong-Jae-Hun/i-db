package db

import (
	"context"
	"errors"
	"strings"
	"testing"
	"time"
)

func TestRunReadOnly_읽기는_돌고_쓰기_여러문장은_서버가_거부한다(t *testing.T) {
	c := testPGConn(t)
	cs := NewClients()
	defer cs.Close()
	pg, err := cs.PG(ctx(t), c)
	must(t, err)
	_, err = pg.Run(ctx(t), `drop table if exists ro_t`)
	must(t, err)
	_, err = pg.Run(ctx(t), `create table ro_t (id int)`)
	must(t, err)
	_, err = pg.Run(ctx(t), `insert into ro_t select generate_series(1, 1500)`)
	must(t, err)

	res, err := pg.RunReadOnly(ctx(t), `select id from ro_t order by id;`)
	must(t, err)
	if len(res.Rows) != MaxRows || !res.Truncated || res.Rows[0][0] != "1" {
		t.Fatalf("커서로 1,000행에서 끊어야 한다: rows=%d truncated=%v", len(res.Rows), res.Truncated)
	}
	res, err = pg.RunReadOnly(ctx(t), `show statement_timeout`)
	must(t, err)
	if res.Rows[0][0] != "30s" {
		t.Fatalf("SET LOCAL 시간 제한이 걸려야 한다: %v", res.Rows)
	}
	_, err = pg.RunReadOnly(ctx(t), `explain select * from ro_t`)
	must(t, err)

	for _, sql := range []string{
		`delete from ro_t`,
		`with d as (delete from ro_t returning id) select * from d`,
		`select 1; delete from ro_t`,
		`show timezone; delete from ro_t`,
		`select * into ro_copy from ro_t`,
	} {
		if _, err := pg.RunReadOnly(ctx(t), sql); err == nil {
			t.Fatalf("거부해야 한다: %s", sql)
		}
	}
	res, err = pg.RunReadOnly(ctx(t), `select count(*) from ro_t`)
	must(t, err)
	if res.Rows[0][0] != "1500" {
		t.Fatalf("쓰기가 반영되면 안 된다: %v", res.Rows)
	}
	if _, err := pg.RunReadOnly(ctx(t), `-- 주석만`); err == nil || !strings.Contains(err.Error(), "empty") {
		t.Fatalf("빈 문장은 거부한다: %v", err)
	}
}

func TestRunReadOnly_편집기_세션의_트랜잭션과_섞이지_않는다(t *testing.T) {
	c := testPGConn(t)
	cs := NewClients()
	defer cs.Close()
	pg, err := cs.PG(ctx(t), c)
	must(t, err)
	_, err = pg.Run(ctx(t), `begin`)
	must(t, err)
	_, err = pg.Run(ctx(t), `create temp table ro_sess (id int)`)
	must(t, err)

	// 풀 연결이라 편집기 세션의 임시 테이블이 보이지 않아야 한다
	if _, err := pg.RunReadOnly(ctx(t), `select * from ro_sess`); err == nil {
		t.Fatal("편집기 세션 연결을 쓰면 안 된다")
	}
	res, err := pg.Run(ctx(t), `select count(*) from ro_sess`)
	must(t, err)
	if !res.InTx {
		t.Fatal("편집기 세션의 트랜잭션은 그대로 열려 있어야 한다")
	}
	_, err = pg.Run(ctx(t), `rollback`)
	must(t, err)
	// 편집기 세션이 잡고 있는 연결 하나만 남아야 한다
	if st := pg.pool.Stat(); st.AcquiredConns() != 1 {
		t.Fatalf("풀 연결을 돌려줘야 한다: acquired=%d", st.AcquiredConns())
	}
}

func TestRunReadOnly_읽기_전용이_아닌_문장은_접속_전에_거부한다(t *testing.T) {
	// 풀이 없는 PGConn 이라 접속을 시도하면 패닉이 난다. 거부는 그 전에 해야 한다
	p := &PGConn{}
	for _, sql := range []string{
		`copy (select 'x') to program 'touch /tmp/dbtui_pwn'`,
		`commit`,
		`set transaction read write`,
		`call some_proc()`,
		`do $$ begin perform 1; end $$`,
	} {
		if _, err := p.RunReadOnly(context.Background(), sql); !errors.Is(err, ErrNotReadOnly) {
			t.Fatalf("%s: ErrNotReadOnly 여야 한다: %v", sql, err)
		}
	}
}

func TestRunReadOnly_붙여_쓴_select도_커서로_행수를_자른다(t *testing.T) {
	cs := NewClients()
	defer cs.Close()
	pg, err := cs.PG(ctx(t), testPGConn(t))
	must(t, err)
	res, err := pg.RunReadOnly(ctx(t), `select*from generate_series(1, 3000)`)
	must(t, err)
	if len(res.Rows) != MaxRows || !res.Truncated {
		t.Fatalf("커서 경로여야 한다: rows=%d truncated=%v", len(res.Rows), res.Truncated)
	}
}

func TestRunReadOnly_세션_상태를_남기지_않는다(t *testing.T) {
	cs := NewClients()
	defer cs.Close()
	pg, err := cs.PG(ctx(t), testPGConn(t))
	must(t, err)
	// 이름 차단(CheckReadOnlySQL)을 비켜 가는 사용자 함수로 잠근다. 차단은 최선일 뿐이라 연결 폐기가 따로 막아야 한다
	_, err = pg.Run(ctx(t), `create or replace function ro_lock() returns boolean language sql as 'select pg_try_advisory_lock(424242)'`)
	must(t, err)
	res, err := pg.RunReadOnly(ctx(t), `select ro_lock()`)
	must(t, err)
	if res.Rows[0][0] != "t" {
		t.Fatalf("잠금을 잡아야 한다: %v", res.Rows)
	}
	res, err = pg.RunReadOnly(ctx(t), `show standard_conforming_strings`)
	must(t, err)
	if res.Rows[0][0] != "on" {
		t.Fatalf("standard_conforming_strings 를 on 으로 고정해야 한다: %v", res.Rows)
	}
	// 연결을 닫은 뒤 백엔드가 끝나며 잠금을 푸는 것은 비동기라 잠깐 기다린다
	deadline := time.Now().Add(5 * time.Second)
	for {
		res, err = pg.Run(ctx(t), `select count(*) from pg_locks where locktype = 'advisory' and objid = 424242`)
		must(t, err)
		if res.Rows[0][0] == "0" {
			return
		}
		if time.Now().After(deadline) {
			t.Fatal("RunReadOnly 가 잡은 advisory lock 이 남아 있다")
		}
		time.Sleep(50 * time.Millisecond)
	}
}

func TestCheckReadOnlySQL_부수효과_함수_호출을_막는다(t *testing.T) {
	blocked := []string{
		`select pg_terminate_backend(1)`,
		`SELECT PG_CATALOG.PG_TERMINATE_BACKEND (1)`,
		`select "pg_notify"('c', 'x')`,
		`select pg_catalog./**/pg_cancel_backend/* x */(1)`,
		`select dblink_exec('host=x', 'drop table t')`,
		`select pg_try_advisory_lock(1)`,
		`select lo_import('/etc/passwd')`,
		`select * from pg_ls_dir('.')`,
		`select pg_read_file('postgresql.conf')`,
		`select set_config('statement_timeout', '0', true)`,
		`select query_to_xml('select pg_notify(''a'', ''b'')', true, true, '')`,
		`select U&"pg_notify"('a', 'b')`,
		`select e'\'' , pg_notify('a', 'b')`,
		`select 1 where $$ $$ = '' or pg_notify('a', 'b') is null`,
		`select postgres_fdw_disconnect_all()`,
	}
	for _, sql := range blocked {
		if err := CheckReadOnlySQL(sql); err == nil {
			t.Errorf("막아야 한다: %s", sql)
		}
	}
	allowed := []string{
		`select 'pg_terminate_backend(1)'`,
		`select 1 -- pg_notify(1)`,
		`/* pg_notify( */ select 1`,
		`select $$pg_notify('a')$$`,
		`select $t$ $$ pg_notify( $t$`,
		`select "pg_notify" from t`,
		`select lower(name), count(*) from lo_table group by 1`,
		`select 'it''s pg_notify('`,
		`select e'it\'s pg_notify('`,
		`select a$pg_notify(1)`,
	}
	for _, sql := range allowed {
		if err := CheckReadOnlySQL(sql); err != nil {
			t.Errorf("통과해야 한다: %s: %v", sql, err)
		}
	}
}
