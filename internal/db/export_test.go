package db

import (
	"bytes"
	"context"
	"errors"
	"slices"
	"strings"
	"testing"
	"time"
)

// v4sqlPG 는 빈 v4sql 스키마에 fixture 를 깐 접속을 돌려주고 끝나면 스키마를 지운다.
func v4sqlPG(t *testing.T, fixture string) *PGConn {
	t.Helper()
	cs := NewClients()
	t.Cleanup(cs.Close)
	p, err := cs.PG(ctx(t), testPGConn(t))
	if err != nil {
		t.Fatal(err)
	}
	const drop = "drop schema if exists v4sql cascade"
	if _, err := p.pool.Exec(ctx(t), drop+"; create schema v4sql;"+fixture); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		c, cancel := context.WithTimeout(context.Background(), 10*time.Second)
		defer cancel()
		if _, err := p.pool.Exec(c, drop); err != nil {
			t.Errorf("cleanup: %v", err)
		}
	})
	return p
}

const exportFixture = `
create table v4sql.big (id int primary key, name text);
insert into v4sql.big select g, 'n' || g from generate_series(1, 2500) g;`

func TestCopyOut_제한_없이_CSV_로_쓰고_진행을_알린다(t *testing.T) {
	p := v4sqlPG(t, exportFixture)
	var buf bytes.Buffer
	var last int64
	calls := 0
	rows, err := p.CopyOut(ctx(t), "select id, name from v4sql.big order by id -- 끝 주석\n;", &buf, func(n int64) { last, calls = n, calls+1 })
	must(t, err)
	lines := strings.Split(strings.TrimSuffix(buf.String(), "\n"), "\n")
	if rows != 2500 || len(lines) != 2501 || lines[0] != "id,name" || lines[2500] != "2500,n2500" {
		t.Fatalf("rows=%d lines=%d 첫 줄=%q 끝 줄=%q", rows, len(lines), lines[0], lines[len(lines)-1])
	}
	if calls == 0 || last != int64(buf.Len()) {
		t.Fatalf("진행 바이트: calls=%d last=%d len=%d", calls, last, buf.Len())
	}
}

func TestCopyOut_쓰기_여러_문장_괄호_어긋남은_거부한다(t *testing.T) {
	p := v4sqlPG(t, exportFixture)
	var buf bytes.Buffer
	if _, err := p.CopyOut(ctx(t), "delete from v4sql.big", &buf, nil); !errors.Is(err, ErrNotReadOnly) {
		t.Errorf("쓰기 문장: %v", err)
	}
	for _, sql := range []string{"select 1; select 2", "select 1) to stdout with (format text", "  ;  "} {
		if _, err := p.CopyOut(ctx(t), sql, &buf, nil); err == nil {
			t.Errorf("%q 는 거부해야 한다", sql)
		}
	}
	// 첫 단어 판정이 놓치는 CTE 안 쓰기는 READ ONLY 트랜잭션이 막는다
	_, err := p.CopyOut(ctx(t), "with d as (delete from v4sql.big returning *) select * from d", &buf, nil)
	if err == nil || !strings.Contains(err.Error(), "read-only") {
		t.Errorf("CTE 안 쓰기: %v", err)
	}
	var n int
	must(t, p.pool.QueryRow(ctx(t), "select count(*) from v4sql.big").Scan(&n))
	if n != 2500 || buf.Len() != 0 {
		t.Fatalf("행이 지워지거나 무언가 쓰였다: n=%d buf=%d", n, buf.Len())
	}
}

func TestCopyOut_취소하면_멈춘다(t *testing.T) {
	p := v4sqlPG(t, "")
	qctx, cancel := context.WithCancel(context.Background())
	time.AfterFunc(300*time.Millisecond, cancel)
	start := time.Now()
	_, err := p.CopyOut(qctx, "select pg_sleep(10)", &bytes.Buffer{}, nil)
	if err == nil || time.Since(start) > 5*time.Second {
		t.Fatalf("취소돼야 한다: err=%v elapsed=%s", err, time.Since(start))
	}
	// 풀은 계속 쓸 수 있다
	if _, err := p.CopyOut(ctx(t), "select 1", &bytes.Buffer{}, nil); err != nil {
		t.Fatalf("취소 뒤 다음 내보내기: %v", err)
	}
}

const searchFixture = `
create table v4sql.users (id int primary key, name text, email varchar(100), code char(6), meta jsonb, uid uuid, age int);
insert into v4sql.users values
	(1, 'alice', 'alice@x.io', 'A100', '{"tag": "needle"}', '00000000-0000-0000-0000-000000000001', 30),
	(2, 'needle bob', 'bob@x.io', '100%', null, null, 40),
	(3, 'carol', 'NEEDLE@x.io', '100x', null, null, 50);
create table v4sql.multi (a int, b text, note text, primary key (a, b));
insert into v4sql.multi values (1, 'k', 'needle');
create table v4sql.nopk (note text);
insert into v4sql.nopk select 'needle ' || g from generate_series(1, 9) g;
create table v4sql.nums (n int);
create table v4sql.zlocked (note text);
insert into v4sql.zlocked values ('needle');
create view v4sql.v_users as select * from v4sql.users;`

func TestSearchData_글자_열을_찾고_PK_를_보인다(t *testing.T) {
	p := v4sqlPG(t, searchFixture)
	n, err := p.CountSearchTables(ctx(t), "v4sql")
	must(t, err)
	if n != 4 { // users·multi·nopk·zlocked. 글자 열 없는 nums 와 뷰는 뺀다
		t.Fatalf("CountSearchTables = %d", n)
	}
	res, skipped, err := p.SearchData(ctx(t), "v4sql", "needle")
	must(t, err)
	if !slices.Equal(res.Cols, []string{"테이블", "열", "PK", "값"}) || len(skipped) != 0 {
		t.Fatalf("열=%v skipped=%v", res.Cols, skipped)
	}
	got := map[string]int{}
	for _, r := range res.Rows {
		got[r[0]]++
	}
	if got["users"] != 3 || got["multi"] != 1 || got["nopk"] != 5 || got["zlocked"] != 1 {
		t.Fatalf("테이블별 건수(nopk 는 5행 상한): %v\n%v", got, res.Rows)
	}
	for _, want := range [][]string{
		{"users", "meta", "id=1", `{"tag": "needle"}`},
		{"users", "name", "id=2", "needle bob"},
		{"users", "email", "id=3", "NEEDLE@x.io"},
		{"multi", "note", "a=1, b=k", "needle"},
		{"nopk", "note", "", "needle 1"},
	} {
		if !slices.ContainsFunc(res.Rows, func(r []string) bool { return slices.Equal(r, want) }) {
			t.Errorf("%v 행이 없다: %v", want, res.Rows)
		}
	}
}

func TestSearchData_LIKE_와일드카드는_글자_그대로_찾는다(t *testing.T) {
	p := v4sqlPG(t, searchFixture)
	for text, want := range map[string]string{"100%": "100%", "10_x": "", "-000000000001": "00000000-0000-0000-0000-000000000001"} {
		res, _, err := p.SearchData(ctx(t), "v4sql", text)
		must(t, err)
		var vals []string
		for _, r := range res.Rows {
			vals = append(vals, r[3])
		}
		if (want == "" && len(vals) != 0) || (want != "" && !slices.Equal(vals, []string{want})) {
			t.Errorf("SearchData(%q) 값 = %q, want %q", text, vals, want)
		}
	}
}

func TestSearchData_시간이_넘은_테이블은_건너뛰고_계속한다(t *testing.T) {
	p := v4sqlPG(t, searchFixture)
	locker := otherSession(t, "v4sql-locker")
	_, err := locker.Exec(ctx(t), "begin; lock table v4sql.zlocked in access exclusive mode")
	must(t, err)
	start := time.Now()
	res, skipped, err := p.SearchData(ctx(t), "v4sql", "needle")
	must(t, err)
	if !slices.Equal(skipped, []string{"zlocked"}) || len(res.Rows) != 9 || time.Since(start) > 15*time.Second {
		t.Fatalf("skipped=%v rows=%d elapsed=%s", skipped, len(res.Rows), time.Since(start))
	}
}

func TestCopyOut_F6_으로_고른_스키마에서_이름을_찾는다(t *testing.T) {
	p := v4sqlPG(t, "")
	const drop = "drop schema if exists v4fix_app cascade"
	must(t, func() error {
		_, err := p.pool.Exec(ctx(t), drop+`; create schema v4fix_app;
			create table v4fix_app.v4fix_orders (id int);
			insert into v4fix_app.v4fix_orders values (7), (8);`)
		return err
	}())
	t.Cleanup(func() {
		c, cancel := context.WithTimeout(context.Background(), 10*time.Second)
		defer cancel()
		if _, err := p.pool.Exec(c, drop); err != nil {
			t.Errorf("cleanup: %v", err)
		}
	})
	must(t, p.SetSearchPath(ctx(t), "v4fix_app"))
	var buf bytes.Buffer
	rows, err := p.CopyOut(ctx(t), "select id from v4fix_orders order by id", &buf, nil)
	if err != nil || rows != 2 || buf.String() != "id\n7\n8\n" {
		t.Fatalf("rows=%d err=%v csv=%q", rows, err, buf.String())
	}
}

func TestCopyOut_SELECT_꼴이_아닌_읽기_문장은_ErrExportNotSelect(t *testing.T) {
	p := v4sqlPG(t, "")
	for _, sql := range []string{"show search_path", "explain select 1"} {
		if _, err := p.CopyOut(ctx(t), sql, &bytes.Buffer{}, nil); !errors.Is(err, ErrExportNotSelect) {
			t.Errorf("%q: %v", sql, err)
		}
	}
	for _, sql := range []string{"values (1)", "table pg_namespace", "with a as (select 1) select * from a"} {
		if _, err := p.CopyOut(ctx(t), sql, &bytes.Buffer{}, nil); err != nil {
			t.Errorf("%q 는 내보내야 한다: %v", sql, err)
		}
	}
}
