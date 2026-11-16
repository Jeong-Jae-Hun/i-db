package db

import (
	"slices"
	"strings"
	"testing"
	"time"
)

// ─── R73 연결된 곳 찾기 ───

func TestLinkedBase_열_이름과_원본_테이블에서_이름_뿌리를_뽑는다(t *testing.T) {
	for _, c := range []struct{ col, table, want string }{
		{"user_id", "", "user"},
		{"User_ID", "orders", "user"},
		{"sender_user_id", "", "sender_user"},
		{"id", "t_users", "user"},
		{"id", "tb_categories", "category"},
		{"id", "addresses", "address"},
		{"id", "boxes", "box"},
		{"id", "status", "status"},
		{"id", "types", "type"},
		{"id", "", ""},
		{"email", "users", ""},
		{"_id", "", ""},
	} {
		if got := LinkedBase(c.col, c.table); got != c.want {
			t.Errorf("LinkedBase(%q, %q) = %q, want %q", c.col, c.table, got, c.want)
		}
	}
}

func TestLinkedOwnerNames_주인_테이블_이름은_복수형과_접두어를_더한다(t *testing.T) {
	got := LinkedOwnerNames("user")
	for _, want := range []string{"user", "users", "t_users", "t_user", "tb_user", "tb_users"} {
		if !slices.Contains(got, want) {
			t.Errorf("%q 가 없음: %v", want, got)
		}
	}
	if got := LinkedOwnerNames("category"); !slices.Contains(got, "t_categories") {
		t.Errorf("y → ies: %v", got)
	}
	if LinkedOwnerNames("") != nil {
		t.Error("base 가 없으면 주인 테이블도 없다")
	}
}

func TestLinkedTypes_값에_맞는_타입만_고른다(t *testing.T) {
	for _, c := range []struct {
		v        string
		has, not []string
	}{
		{"1315", []string{"text", "varchar", "int2", "int4", "int8"}, []string{"uuid"}},
		{"70000", []string{"int4", "int8"}, []string{"int2"}},
		{"9999999999", []string{"int8"}, []string{"int4", "int2"}},
		{"99999999999999999999", []string{"text"}, []string{"int8"}},
		{"007", []string{"text"}, []string{"int2", "int4", "int8"}},
		{"-5", []string{"int2"}, nil},
		{"0b6c1f0e-4c1a-4b8e-9a51-3f0d2c7e9a10", []string{"uuid", "text"}, []string{"int8"}},
		{"abc", []string{"text", "varchar"}, []string{"int8", "uuid"}},
	} {
		got := LinkedTypes(c.v)
		for _, h := range c.has {
			if !slices.Contains(got, h) {
				t.Errorf("%q: %s 가 없음 %v", c.v, h, got)
			}
		}
		for _, n := range c.not {
			if slices.Contains(got, n) {
				t.Errorf("%q: %s 가 있으면 안 됨 %v", c.v, n, got)
			}
		}
	}
}

func TestLinkedCountSQL_식별자와_값을_감싸고_101행에서_멈춘다(t *testing.T) {
	c := LinkedColumn{Table: TableRef{Schema: "s", Table: `t"x`}, Column: `we"ird`}
	got := LinkedCountSQL(c, `a'b\c`)
	want := `SELECT count(*) FROM (SELECT 1 FROM "s"."t""x" WHERE "we""ird" = E'a''b\\c' LIMIT 101) s`
	if got != want {
		t.Fatalf("got  %s\nwant %s", got, want)
	}
}

// linkedFixture 는 lnk_member 이름 규칙의 테이블들이다. 다른 시험과 겹치지 않게 lnk_it 스키마에만 만든다.
func linkedFixture(t *testing.T) *PGConn {
	t.Helper()
	c := testPGConn(t)
	cs := NewClients()
	t.Cleanup(cs.Close)
	pg, err := cs.PG(ctx(t), c)
	must(t, err)
	for _, q := range []string{
		`drop schema if exists lnk_it cascade`,
		`create schema lnk_it`,
		`create table lnk_it.lnk_members (id bigint primary key)`,
		`create table lnk_it.lnk_orders (id serial primary key, lnk_member_id bigint)`,
		`create index on lnk_it.lnk_orders (lnk_member_id)`,
		`create table lnk_it.lnk_msgs (id serial primary key, sender_lnk_member_id int4, target_lnk_member_id bigint)`,
		`create index on lnk_it.lnk_msgs (sender_lnk_member_id)`,
		`create table lnk_it.lnk_small (lnk_member_id int2)`,
		`create index on lnk_it.lnk_small (lnk_member_id)`,
		`create table lnk_it.lnk_multi (a int, lnk_member_id bigint)`,
		`create index on lnk_it.lnk_multi (a, lnk_member_id)`,
		`create table lnk_it.lnk_tags (lnk_member_id uuid)`,
		`create index on lnk_it.lnk_tags (lnk_member_id)`,
		`create view lnk_it.lnk_view as select lnk_member_id from lnk_it.lnk_orders`,
		// 부분·깨진·식 인덱스는 col = 값 에 못 쓸 수 있어 인덱스 없음으로 본다
		`create table lnk_it.lnk_part (lnk_member_id bigint)`,
		`create index on lnk_it.lnk_part (lnk_member_id) where lnk_member_id > 0`,
		`create table lnk_it.lnk_inval (lnk_member_id bigint)`,
		`create index lnk_inval_idx on lnk_it.lnk_inval (lnk_member_id)`,
		`update pg_index set indisvalid = false where indexrelid = 'lnk_it.lnk_inval_idx'::regclass`,
		`create table lnk_it.lnk_expr (lnk_member_id bigint)`,
		`create index on lnk_it.lnk_expr ((lnk_member_id + 0))`,
		`insert into lnk_it.lnk_members values (1315), (1316)`,
		`insert into lnk_it.lnk_orders (lnk_member_id) select 1315 from generate_series(1, 150)`,
		`insert into lnk_it.lnk_msgs (sender_lnk_member_id, target_lnk_member_id) values (1315, 1315), (1316, 1315)`,
	} {
		_, err := pg.Run(ctx(t), q)
		must(t, err)
	}
	t.Cleanup(func() { _, _ = pg.Run(ctx(t), `drop schema if exists lnk_it cascade`) })
	return pg
}

// lnkOnly 는 다른 시험이 만든 테이블을 빼고 lnk_it 스키마 열만 "테이블.열" 로 남긴다.
func lnkOnly(cols []LinkedColumn) []LinkedColumn {
	return slices.DeleteFunc(slices.Clone(cols), func(c LinkedColumn) bool { return c.Table.Schema != "lnk_it" })
}

func TestLinkedColumns_실제_서버에서_이름_규칙_타입_인덱스로_후보를_고른다(t *testing.T) {
	pg := linkedFixture(t)
	cols, truncated, err := pg.LinkedColumns(ctx(t), "lnk_member_id", "lnk_member", "1315")
	must(t, err)
	cols = lnkOnly(cols)
	got := map[string]LinkedColumn{}
	for _, c := range cols {
		got[c.Table.Table+"."+c.Column] = c
	}
	want := map[string]struct{ owner, indexed bool }{
		"lnk_members.id":                {true, true},
		"lnk_orders.lnk_member_id":      {false, true},
		"lnk_msgs.sender_lnk_member_id": {false, true},
		"lnk_msgs.target_lnk_member_id": {false, false},
		"lnk_small.lnk_member_id":       {false, true},
		"lnk_multi.lnk_member_id":       {false, false}, // 인덱스 둘째 열이라 세지 않는다
		"lnk_part.lnk_member_id":        {false, false},
		"lnk_inval.lnk_member_id":       {false, false},
		"lnk_expr.lnk_member_id":        {false, false},
	}
	if truncated || len(got) != len(want) {
		t.Fatalf("후보: truncated=%v %v", truncated, cols)
	}
	for k, w := range want {
		if c, ok := got[k]; !ok || c.Owner != w.owner || c.Indexed != w.indexed {
			t.Errorf("%s: %+v, want owner=%v indexed=%v", k, c, w.owner, w.indexed)
		}
	}
	if cols[0].Table.Table != "lnk_members" {
		t.Errorf("주인 테이블이 먼저: %v", cols)
	}

	// int2 를 넘는 값은 int2 열을 빼고, uuid 열은 정수 값에서 늘 빠진다
	cols, _, err = pg.LinkedColumns(ctx(t), "lnk_member_id", "lnk_member", "70000")
	must(t, err)
	for _, c := range lnkOnly(cols) {
		if c.Table.Table == "lnk_small" || c.Table.Table == "lnk_tags" {
			t.Errorf("타입이 맞지 않는 열: %+v", c)
		}
	}

	// id 열은 같은 이름 규칙을 쓰지 않아 다른 테이블의 id 가 들지 않는다
	cols, _, err = pg.LinkedColumns(ctx(t), "id", "lnk_member", "1315")
	must(t, err)
	for _, c := range lnkOnly(cols) {
		if c.Column == "id" && !c.Owner {
			t.Errorf("주인이 아닌 id 열: %+v", c)
		}
	}
}

func TestCountLinked_실제_서버에서_인덱스_열만_101행까지_센다(t *testing.T) {
	pg := linkedFixture(t)
	cols, _, err := pg.LinkedColumns(ctx(t), "lnk_member_id", "lnk_member", "1315")
	must(t, err)
	cols = lnkOnly(cols)
	// 인덱스가 없는 열은 묻지 않는다. 없는 테이블을 가리켜도 오류가 나지 않아야 한다
	cols = append(cols, LinkedColumn{Table: TableRef{Schema: "lnk_it", Table: "no_such_table"}, Column: "x"})
	counts, errs := pg.CountLinked(ctx(t), cols, "1315", 3)
	got := map[string]int{}
	for i, c := range cols {
		must(t, errs[i])
		got[c.Table.Table+"."+c.Column] = counts[i]
	}
	want := map[string]int{
		"lnk_members.id": 1, "lnk_orders.lnk_member_id": LinkedCap + 1, "lnk_msgs.sender_lnk_member_id": 1,
		"lnk_msgs.target_lnk_member_id": -1, "lnk_small.lnk_member_id": 0, "lnk_multi.lnk_member_id": -1, "no_such_table.x": -1,
		"lnk_part.lnk_member_id": -1, "lnk_inval.lnk_member_id": -1, "lnk_expr.lnk_member_id": -1,
	}
	for k, w := range want {
		if got[k] != w {
			t.Errorf("%s = %d, want %d", k, got[k], w)
		}
	}
}

func TestCountLinked_한_테이블은_READ_ONLY_트랜잭션에서_2초_제한으로_돈다(t *testing.T) {
	pg := linkedFixture(t)
	n, err := pg.countLinked(ctx(t), `select extract(epoch from current_setting('statement_timeout')::interval)::int`)
	if err != nil || n != 2 {
		t.Fatalf("statement_timeout 2s: %d %v", n, err)
	}
	start := time.Now()
	_, err = pg.countLinked(ctx(t), `select count(*) from (select pg_sleep(5)) s`)
	if !IsStatementTimeout(err) || time.Since(start) > 4*time.Second {
		t.Fatalf("2초에 멈춤: %v (%s)", err, time.Since(start))
	}
	_, err = pg.countLinked(ctx(t), `with x as (insert into lnk_it.lnk_members values (9) returning 1) select count(*) from x`)
	if err == nil || !strings.Contains(err.Error(), "read-only") {
		t.Fatalf("쓰기는 서버가 막는다: %v", err)
	}
}
