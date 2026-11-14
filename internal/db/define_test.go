package db

import (
	"errors"
	"slices"
	"strings"
	"testing"
)

const defineFixture = `
create table v4sql.v4_users (id int primary key, name text);
create view v4sql.v4_v as select id from v4sql.v4_users;
create materialized view v4sql.v4_mv as select 1 as x;
create table v4sql."Mixed Case" (id int);
create table v4sql.pg_class (id int);
create function v4sql.v4_f(int) returns int language sql as 'select $1';
create function v4sql.v4_f(text) returns text language sql as 'select $1';
create aggregate v4sql.v4_agg(int) (sfunc = int4pl, stype = int);`

func TestSplitQualified_따옴표와_소문자_접기(t *testing.T) {
	cases := []struct {
		in   string
		want []string
	}{
		{"orders", []string{"orders"}},
		{" Public.Orders ", []string{"public", "orders"}},
		{`"My Schema"."A""b"`, []string{"My Schema", `A"b`}},
		{`"a.b".c`, []string{"a.b", "c"}},
		{"db.s.t", []string{"db", "s", "t"}},
		{"한글이름", []string{"한글이름"}},
	}
	for _, c := range cases {
		got, err := splitQualified(c.in)
		if err != nil || !slices.Equal(got, c.want) {
			t.Errorf("splitQualified(%q) = %q, %v, want %q", c.in, got, err, c.want)
		}
	}
	for _, bad := range []string{"", "a.", `"abc`, `"a"b`, ".t"} {
		if _, err := splitQualified(bad); !errors.Is(err, ErrNotFound) {
			t.Errorf("splitQualified(%q) 는 ErrNotFound 여야 한다: %v", bad, err)
		}
	}
}

func TestResolve_스키마_따옴표_search_path_순서로_찾는다(t *testing.T) {
	p := v4sqlPG(t, defineFixture)
	cases := []struct {
		name string
		want Object
	}{
		{"v4sql.v4_users", Object{ObjTable, "v4sql", "v4_users"}},
		{"V4SQL.V4_V", Object{ObjView, "v4sql", "v4_v"}},
		{"v4sql.v4_mv", Object{ObjMatView, "v4sql", "v4_mv"}},
		{`v4sql."Mixed Case"`, Object{ObjTable, "v4sql", "Mixed Case"}},
		{`"v4sql"."v4_f"`, Object{ObjFunction, "v4sql", "v4_f"}},
		{"pg_class", Object{ObjTable, "pg_catalog", "pg_class"}}, // 사용자 스키마가 search_path 에 없다
		{"now", Object{ObjFunction, "pg_catalog", "now"}},
	}
	for _, c := range cases {
		got, err := p.Resolve(ctx(t), c.name)
		if err != nil || got != c.want {
			t.Errorf("Resolve(%q) = %+v, %v, want %+v", c.name, got, err, c.want)
		}
	}
	// 스키마를 적은 이름은 그 스키마에서만 찾는다. pg_catalog 로 넘어가지 않는다
	for _, name := range []string{"v4_users", "v4sql.nope", "v4sql.v4_agg", `v4sql."mixed case"`, "public.now", "v4sql.pg_proc"} {
		if _, err := p.Resolve(ctx(t), name); !errors.Is(err, ErrNotFound) || !strings.Contains(err.Error(), name) {
			t.Errorf("Resolve(%q) 는 이름을 담은 ErrNotFound 여야 한다: %v", name, err)
		}
	}

	// 고른 스키마가 pg_catalog 보다 먼저다
	must(t, p.SetSearchPath(ctx(t), "v4sql"))
	for name, want := range map[string]Object{
		"v4_users": {ObjTable, "v4sql", "v4_users"},
		"pg_class": {ObjTable, "v4sql", "pg_class"},
	} {
		if got, err := p.Resolve(ctx(t), name); err != nil || got != want {
			t.Errorf("search_path 뒤 Resolve(%q) = %+v, %v, want %+v", name, got, err, want)
		}
	}
}

func TestFunctionDef_오버로드는_빈_줄로_잇는다(t *testing.T) {
	p := v4sqlPG(t, defineFixture)
	def, err := p.FunctionDef(ctx(t), "v4sql", "v4_f")
	must(t, err)
	if strings.Count(def, "CREATE OR REPLACE FUNCTION v4sql.v4_f(") != 2 || !strings.Contains(def, "\n\nCREATE") {
		t.Fatalf("정의 두 개가 빈 줄로 이어져야 한다:\n%s", def)
	}
	if _, err := p.FunctionDef(ctx(t), "v4sql", "nope"); !errors.Is(err, ErrNotFound) {
		t.Fatalf("없는 함수는 ErrNotFound: %v", err)
	}
}
