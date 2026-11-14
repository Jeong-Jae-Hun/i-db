package db

import (
	"strings"
	"testing"
)

func TestDangerOf_위험_문장과_이유(t *testing.T) {
	cases := []struct{ stmt, want string }{
		{"delete from customers;", "WHERE 없는 DELETE"},
		{"update t set a=1;", "WHERE 없는 UPDATE"},
		{"truncate t;", "TRUNCATE"},
		{"drop table t;", "DROP"},
		{"alter table t drop column c;", "DROP"},
		{"DELETE FROM t", "WHERE 없는 DELETE"},
		{"-- where 지우기\n/* where */ delete from t", "WHERE 없는 DELETE"},
		{"delete from t -- where id = 1", "WHERE 없는 DELETE"},
		{"update t set note = 'where id = 1'", "WHERE 없는 UPDATE"},
		{`update t set "where" = 1`, "WHERE 없는 UPDATE"},
		{"update t set a = $$ where $$", "WHERE 없는 UPDATE"},
		{"update t set a = (select b from u where u.id = 1)", "WHERE 없는 UPDATE"},
		{"delete from t using (select id from u where x) s", "WHERE 없는 DELETE"},
		{"select 1; delete from t; truncate u", "WHERE 없는 DELETE, TRUNCATE"},
		{"drop table a; drop table b", "DROP"},
		{"alter table t drop constraint t_pk;", "DROP"},
		{"alter table t drop c", "DROP"},
		{"alter table t alter column a drop not null, drop column b", "DROP"},
		// COLUMN 을 뺀 열 지우기: 열 이름이 identity·expression 이어도 지운다
		{"alter table t drop identity", "DROP"},
		{"alter table t drop expression;", "DROP"},
		{"alter domain d drop constraint c", "DROP"},
	}
	for _, c := range cases {
		if got := DangerOf(c.stmt); got != c.want {
			t.Errorf("DangerOf(%q) = %q, want %q", c.stmt, got, c.want)
		}
	}
}

func TestDangerOf_안전한_문장은_빈_문자열(t *testing.T) {
	for _, stmt := range []string{
		"delete from t where id = 1;",
		"update t set a = 1 from u where u.id = t.id",
		"delete from t using u where u.id = t.id",
		"update t set a = 'x' WHERE id in (select id from u)",
		"select * from t",
		"alter table t add column c int",
		"insert into t values ('drop', 'truncate')",
		"select 'delete from t'",
		"-- delete from t\nselect 1",
		"create table drop_log (id int)",
		"alter table t alter column c drop not null",
		"alter table t alter c drop default",
		"ALTER TABLE t ALTER COLUMN c DROP IDENTITY IF EXISTS",
		"alter table t alter column c drop expression",
		"alter domain d drop not null",
		"",
		// CTE 안 쓰기는 R6.1 과 같이 잡지 못한다(명세 R25.1 단서)
		"with d as (select 1) delete from t",
	} {
		if got := DangerOf(stmt); got != "" {
			t.Errorf("DangerOf(%q) = %q, want 빈 문자열", stmt, got)
		}
	}
}

func TestDangerOfRedis_FLUSH_명령만_잡는다(t *testing.T) {
	cases := []struct {
		args []string
		want string
	}{
		{[]string{"FLUSHDB"}, "FLUSHDB"},
		{[]string{"flushall", "async"}, "FLUSHALL"},
		{[]string{"DEL", "flushdb"}, ""},
		{nil, ""},
	}
	for _, c := range cases {
		if got := DangerOfRedis(c.args); got != c.want {
			t.Errorf("DangerOfRedis(%v) = %q, want %q", c.args, got, c.want)
		}
	}
}

func TestMaskSQL_문자열_주석_따옴표_이름_안을_지운다(t *testing.T) {
	in := `select 'a;b', "c;d", $t$ e;f $t$, E'g\'h;' -- i;j` + "\n/* k /* l; */ m; */ n"
	want := `select '   ', "   ", $t$     $t$, E'     '` + strings.Repeat(" ", 7) + "\n" + strings.Repeat(" ", 20) + "n"
	if got := MaskSQL(in); got != want {
		t.Fatalf("MaskSQL\n got %q\nwant %q", got, want)
	}
}

func TestHasComment_따옴표와_달러_인용_밖의_주석만_찾는다(t *testing.T) {
	for in, want := range map[string]bool{
		"a = 1 -- 메모":        true,
		"a = 1 /* x */":      true,
		"a = '--' and b = 2": false,
		"a = $$/* x */$$":    false,
		`a = E'\' -- '`:      false,
		"a = 1":              false,
	} {
		if got := HasComment(in); got != want {
			t.Errorf("%q: %v, want %v", in, got, want)
		}
	}
}

func TestHiddenWriteOf_CTE_쓰기_SELECT_INTO_부수효과_함수(t *testing.T) {
	cases := map[string]string{
		"with x as (delete from t returning id) select * from x":                   "CTE 안 쓰기",
		"WITH x AS (select 1) UPDATE t SET a = 1 FROM x WHERE t.id = 1":            "CTE 안 쓰기",
		"select * into t2 from t":                                                  "SELECT INTO",
		"update t set a = pg_terminate_backend(1) where id = 1":                    "부수효과 함수 pg_terminate_backend",
		"insert into t select dblink_exec('x')":                                    "부수효과 함수 dblink_exec",
		"with x as (insert into t values (1) returning 1) select * into t3 from x": "CTE 안 쓰기, SELECT INTO",
	}
	for sql, want := range cases {
		if got := HiddenWriteOf(sql); got != want {
			t.Errorf("%q: got %q want %q", sql, got, want)
		}
	}
	for _, sql := range []string{
		"insert into t values (1)",
		"update t set a = 'with x as (delete' where id = 1",
		"with x as (select 1) select * from x",
		"insert into t (\"update\") values (1)",
		"select 'into' from t",
	} {
		if got := HiddenWriteOf(sql); got != "" {
			t.Errorf("%q: 빈 문자열이어야 한다: %q", sql, got)
		}
	}
}
