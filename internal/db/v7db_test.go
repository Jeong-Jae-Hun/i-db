package db

import (
	"context"
	"slices"
	"strings"
	"testing"
	"time"
)

// v7dbPG 는 schemas 를 지우고 fixture 를 깐 접속을 돌려주고 끝나면 schemas 를 지운다.
func v7dbPG(t *testing.T, fixture string, schemas ...string) *PGConn {
	t.Helper()
	cs := NewClients()
	t.Cleanup(cs.Close)
	p, err := cs.PG(ctx(t), testPGConn(t))
	must(t, err)
	drop := "drop schema if exists " + strings.Join(schemas, ", ") + " cascade"
	if _, err := p.pool.Exec(ctx(t), drop+";"+fixture); err != nil {
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

// ─── R50 되돌리기 ───

const undoFixture = `create schema v7db;
create table v7db.t (id int primary key, name text, j jsonb, b bool, extra text default 'x',
	up text generated always as (upper(name)) stored);
insert into v7db.t (id, name, j, b, extra) values (1, 'a', '{"k":1}', true, 'e1'), (2, 'b', null, null, 'e2'), (3, 'c', '[]', false, null);
create table v7db.ident (id int generated always as identity primary key, name text);
insert into v7db.ident (name) values ('x'), ('y');`

const undoSelect = `select id, coalesce(name, '<null>'), coalesce(j::text, '<null>'), coalesce(b::text, '<null>'),
	coalesce(extra, '<null>'), coalesce(up, '<null>') from v7db.t order by id`

func TestUndoChanges_반영_뒤_행으로_거꾸로_된_순서의_되돌리기_묶음을_만든다(t *testing.T) {
	cs := ChangeSet{Table: TableRef{"s", "t"}, PK: []string{"id"}, Changes: []Change{
		{Op: ChangeUpdate, Key: map[string]string{"id": "1"}, Values: map[string]*string{"id": strp("5"), "b": strp("true")},
			Old: map[string]*string{"id": strp("1"), "b": nil}},
		{Op: ChangeInsert, Values: map[string]*string{"id": strp("9")}},
		{Op: ChangeDelete, Key: map[string]string{"id": "2"}, Old: map[string]*string{"name": strp("b")}},
	}}
	applied := &Applied{Generated: []string{"up"}, Rows: []map[string]*string{
		{"id": strp("5"), "b": strp("t"), "name": strp("a")},
		{"id": strp("9"), "b": nil, "name": strp("d")},
		{"id": strp("2"), "b": nil, "name": strp("b"), "up": strp("B")},
	}}
	undo, err := UndoChanges(cs, applied)
	must(t, err)
	want := []string{
		`INSERT INTO "s"."t" ("b", "id", "name") OVERRIDING SYSTEM VALUE VALUES (NULL, '2', 'b') RETURNING *;`,
		`DELETE FROM "s"."t" WHERE "id" = '9' AND "b" IS NULL AND "name" IS NOT NULL AND format('%s', "name") = 'd' RETURNING *;`,
		`UPDATE "s"."t" SET "b" = NULL, "id" = '1' WHERE "id" = '5' AND "b" IS NOT NULL AND format('%s', "b") = 't' AND "id" IS NOT NULL AND format('%s', "id") = '5' RETURNING *;`,
	}
	if got := ChangesSQL(undo); !slices.Equal(got, want) {
		t.Fatalf("got\n%s\nwant\n%s", strings.Join(got, "\n"), strings.Join(want, "\n"))
	}
}

func TestUndoChanges_되돌릴_값을_모르면_만들지_않는다(t *testing.T) {
	cs := ChangeSet{Table: TableRef{"s", "t"}, PK: []string{"id"}, Changes: []Change{
		{Op: ChangeUpdate, Key: map[string]string{"id": "1"}, Values: map[string]*string{"name": strp("n")}},
	}}
	cases := map[string]*Applied{
		"applied rows do not match": nil,
		"old value of column":       {Rows: []map[string]*string{{"id": strp("1"), "name": strp("n")}}},
		"no primary key column":     {Rows: []map[string]*string{{"name": strp("n")}}},
		"no returned row":           {Rows: []map[string]*string{nil}},
	}
	for want, applied := range cases {
		if _, err := UndoChanges(cs, applied); err == nil || !strings.Contains(err.Error(), want) {
			t.Errorf("%s: %v", want, err)
		}
	}
}

func TestPG_되돌리기는_제출한_묶음을_원래대로_돌린다(t *testing.T) {
	p := v7dbPG(t, undoFixture, "v7db")
	before, err := p.Run(ctx(t), undoSelect)
	must(t, err)
	// 삭제의 옛값은 결과에 보인 일부 열뿐이어도 RETURNING * 로 받은 행 전체가 되살아난다. bool 은 출력('t')과 다른 글('true')로 고친다
	cs := ChangeSet{Table: TableRef{"v7db", "t"}, PK: []string{"id"}, Changes: []Change{
		{Op: ChangeUpdate, Key: map[string]string{"id": "1"}, Values: map[string]*string{"name": strp("A2"), "b": strp("false"), "j": nil},
			Old: map[string]*string{"name": strp("a"), "b": strp("t"), "j": strp(`{"k": 1}`)}},
		{Op: ChangeInsert, Values: map[string]*string{"id": strp("10"), "name": strp("new")}},
		{Op: ChangeDelete, Key: map[string]string{"id": "2"}, Old: map[string]*string{"name": strp("b")}},
		{Op: ChangeUpdate, Key: map[string]string{"id": "3"}, Values: map[string]*string{"id": strp("30")}, Old: map[string]*string{"id": strp("3")}},
	}}
	res, err := p.ApplyChanges(ctx(t), cs)
	must(t, err)
	if len(res.Rows) != 1 || res.Rows[0][0] != "10" || len(res.Applied.Rows) != 4 || !slices.Equal(res.Applied.Generated, []string{"up"}) {
		t.Fatalf("반영 결과: %+v %+v", res, res.Applied)
	}
	undo, err := UndoChanges(cs, res.Applied)
	must(t, err)
	ures, err := p.ApplyChanges(ctx(t), undo)
	must(t, err)
	if ures.Text != "UPDATE 2 · INSERT 1 · DELETE 1" {
		t.Fatalf("되돌리기 요약: %q", ures.Text)
	}
	after, err := p.Run(ctx(t), undoSelect)
	must(t, err)
	if !slices.EqualFunc(before.Rows, after.Rows, slices.Equal) {
		t.Fatalf("되돌린 뒤\n%v\n원래\n%v", after.Rows, before.Rows)
	}
}

func TestPG_되돌리기는_제출_뒤_다른_사람이_고친_행을_덮지_않는다(t *testing.T) {
	p := v7dbPG(t, undoFixture, "v7db")
	cs := ChangeSet{Table: TableRef{"v7db", "t"}, PK: []string{"id"}, Changes: []Change{
		{Op: ChangeUpdate, Key: map[string]string{"id": "1"}, Values: map[string]*string{"name": strp("mine")}, Old: map[string]*string{"name": strp("a")}},
		{Op: ChangeInsert, Values: map[string]*string{"id": strp("10"), "name": strp("new")}},
	}}
	res, err := p.ApplyChanges(ctx(t), cs)
	must(t, err)
	undo, err := UndoChanges(cs, res.Applied)
	must(t, err)
	// 넣은 행을 다른 세션이 고쳤다. 첫 되돌리기(DELETE)가 0행이 되어 묶음 전체가 되돌려져야 한다
	if _, err := p.pool.Exec(ctx(t), `update v7db.t set extra = 'theirs' where id = 10`); err != nil {
		t.Fatal(err)
	}
	if _, err := p.ApplyChanges(ctx(t), undo); err == nil || !strings.Contains(err.Error(), "DELETE failed: row (id=10) changed since it was read") {
		t.Fatalf("다른 사람이 고친 행은 지우지 않아야 한다: %v", err)
	}
	got, err := p.Run(ctx(t), `select id, name, extra from v7db.t where id in (1, 10) order by id`)
	must(t, err)
	want := [][]string{{"1", "mine", "e1"}, {"10", "new", "theirs"}}
	if !slices.EqualFunc(got.Rows, want, slices.Equal) {
		t.Fatalf("하나도 되돌리지 않아야 한다: %v", got.Rows)
	}
}

func TestPG_되돌리기는_지운_IDENTITY_ALWAYS_행을_같은_키로_되살린다(t *testing.T) {
	p := v7dbPG(t, undoFixture, "v7db")
	cs := ChangeSet{Table: TableRef{"v7db", "ident"}, PK: []string{"id"}, Changes: []Change{
		{Op: ChangeDelete, Key: map[string]string{"id": "1"}, Old: map[string]*string{"name": strp("x")}},
	}}
	res, err := p.ApplyChanges(ctx(t), cs)
	must(t, err)
	undo, err := UndoChanges(cs, res.Applied)
	must(t, err)
	_, err = p.ApplyChanges(ctx(t), undo)
	must(t, err)
	got, err := p.Run(ctx(t), `select id, name from v7db.ident order by id`)
	must(t, err)
	if want := [][]string{{"1", "x"}, {"2", "y"}}; !slices.EqualFunc(got.Rows, want, slices.Equal) {
		t.Fatalf("되살린 행: %v", got.Rows)
	}
}

// ─── R49 enum 값 ───

func TestPG_EnumLabels_는_정의_순서로_돌려주고_enum_이_아니면_비어_있다(t *testing.T) {
	p := v7dbPG(t, `create schema v7db;
		create type v7db.mood as enum ('sad', 'happy');
		alter type v7db.mood add value 'ok' before 'happy';
		create domain v7db.mood_d as v7db.mood;
		create table v7db.e (id int primary key, m v7db.mood, d v7db.mood_d);`, "v7db")
	res, err := p.Run(ctx(t), `select m, d, id from v7db.e`)
	must(t, err)
	want := []string{"sad", "ok", "happy"}
	for _, typ := range []string{res.Types[0], res.Types[1], "v7db.mood", `"v7db"."mood"`} {
		got, err := p.EnumLabels(ctx(t), typ)
		must(t, err)
		if !slices.Equal(got, want) {
			t.Errorf("%s: %v", typ, got)
		}
	}
	// search_path 밖의 맨 이름, enum 이 아닌 타입, 없는 타입
	for _, typ := range []string{"mood", res.Types[2], "int4", "no_such_type", "0"} {
		got, err := p.EnumLabels(ctx(t), typ)
		must(t, err)
		if len(got) != 0 {
			t.Errorf("%s 는 비어야 한다: %v", typ, got)
		}
	}
}

// ─── R53 스키마 비교 ───

const schemaDiffFixture = `create schema v7db_a; create schema v7db_b;
create table v7db_a.same (id serial primary key, name text not null default 'n');
create table v7db_b.same (id serial primary key, name text not null default 'n');
create table v7db_a.child (same_id int references v7db_a.same (id) check (same_id > 0));
create table v7db_b.child (same_id int references v7db_b.same (id) check (same_id > 0));
create table v7db_a.t (id int primary key, a int, b text, c int);
create index t_a_idx on v7db_a.t (a);
create index t_b_idx on v7db_a.t (b);
create table v7db_b.t (id int primary key, a bigint, c int not null, d text);
create index t_b_idx on v7db_b.t (d);
create table v7db_a.only_a (x int);
create table v7db_b.only_b (y int);
create view v7db_a.v as select id from v7db_a.t;
create view v7db_b.v as select id, d from v7db_b.t;`

// same·child 는 시퀀스 기본값·FK·PK 인덱스에 스키마 이름이 들어가도 같은 정의로 보여야 한다.
func TestPG_스키마_비교는_다른_점만_정해진_순서로_보인다(t *testing.T) {
	p := v7dbPG(t, schemaDiffFixture, "v7db_a", "v7db_b")
	a, err := p.SchemaSnapshot(ctx(t), "v7db_a")
	must(t, err)
	b, err := p.SchemaSnapshot(ctx(t), "v7db_b")
	must(t, err)
	if got := DiffSnapshots(a, a); len(got.Rows) != 0 {
		t.Fatalf("같은 스냅숏은 다른 점이 없어야 한다: %v", got.Rows)
	}
	got := DiffSnapshots(a, b)
	if !slices.Equal(got.Cols, []string{"구분", "대상", "왼쪽", "오른쪽"}) {
		t.Fatalf("열: %v", got.Cols)
	}
	want := [][]string{
		{"−", "테이블 only_a", "TABLE", ""},
		{"+", "테이블 only_b", "", "TABLE"},
		{"~", "열 t.a", "integer", "bigint"},
		{"−", "열 t.b", "text", ""},
		{"~", "열 t.c", "integer", "integer NOT NULL"},
		{"+", "열 t.d", "", "text"},
		{"−", "인덱스 t.t_a_idx", "CREATE INDEX t_a_idx ON t USING btree (a)", ""},
		{"~", "인덱스 t.t_b_idx", "CREATE INDEX t_b_idx ON t USING btree (b)", "CREATE INDEX t_b_idx ON t USING btree (d)"},
		{"~", "테이블 v", "VIEW AS  SELECT id\n   FROM t;", "VIEW AS  SELECT id,\n    d\n   FROM t;"},
		{"+", "열 v.d", "", "text"},
	}
	if !slices.EqualFunc(got.Rows, want, slices.Equal) {
		t.Fatalf("got\n%q\nwant\n%q", got.Rows, want)
	}
}

func TestPG_없는_스키마의_스냅숏은_비어_있다(t *testing.T) {
	p := v7dbPG(t, "", "v7db_none")
	s, err := p.SchemaSnapshot(ctx(t), "v7db_none")
	must(t, err)
	if len(s.Tables) != 0 {
		t.Fatalf("%v", s.Tables)
	}
}
