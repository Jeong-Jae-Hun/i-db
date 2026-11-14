package db

import (
	"reflect"
	"slices"
	"testing"

	"github.com/Jeong-Jae-Hun/i-db/internal/complete"
)

func TestPG_LoadCatalog_스키마_테이블_뷰_열_함수(t *testing.T) {
	c := testPGConn(t)
	cs := NewClients()
	t.Cleanup(cs.Close) // 정리 함수는 거꾸로 돌아 스키마 삭제가 연결 닫기보다 먼저다
	pg, err := cs.PG(ctx(t), c)
	must(t, err)

	for _, sql := range []string{
		`drop schema if exists it_catalog cascade`,
		`create schema it_catalog`,
		`create table it_catalog."Users" (id int, gone text, "Name" varchar(20), total numeric(10,2))`,
		`alter table it_catalog."Users" drop column gone`,
		`create view it_catalog.v as select id from it_catalog."Users"`,
		`create function it_catalog.it_fn() returns int language sql as 'select 1'`,
	} {
		_, err := pg.Run(ctx(t), sql)
		must(t, err)
	}
	t.Cleanup(func() {
		if _, err := pg.Run(ctx(t), `drop schema if exists it_catalog cascade`); err != nil {
			t.Errorf("cleanup: %v", err)
		}
	})

	cat, err := pg.LoadCatalog(ctx(t))
	must(t, err)
	var s *complete.Schema
	for i := range cat.Schemas {
		if n := cat.Schemas[i].Name; n == "pg_catalog" || n == "information_schema" {
			t.Fatalf("시스템 스키마 포함: %s", n)
		}
		if cat.Schemas[i].Name == "it_catalog" {
			s = &cat.Schemas[i]
		}
	}
	if s == nil {
		t.Fatal("it_catalog 스키마 없음")
	}
	want := []complete.Table{
		{Name: "Users", Columns: []complete.Column{{Name: "id", Type: "integer"}, {Name: "Name", Type: "character varying(20)"}, {Name: "total", Type: "numeric(10,2)"}}},
		{Name: "v", View: true, Columns: []complete.Column{{Name: "id", Type: "integer"}}},
	}
	if !reflect.DeepEqual(s.Tables, want) {
		t.Fatalf("%+v", s.Tables)
	}
	if !slices.Contains(cat.Functions, "it_fn") {
		t.Fatalf("함수 목록: %v", cat.Functions)
	}

	got := complete.SuggestSQL(cat, complete.Query{Text: `select u. from it_catalog."Users" u`, Offset: 9})
	if len(got) != 3 || got[1].Insert != `"Name"` {
		t.Fatalf("카탈로그로 자동완성: %+v", got)
	}
}
