package db

import (
	"context"
	"fmt"
	"reflect"
	"strings"
	"testing"
	"time"

	"github.com/jackc/pgx/v5"
	"github.com/jackc/pgx/v5/pgconn"
)

// ─── 가짜 조회 (readSnapshot) ───

// fakeRows 는 pgx.ForEachRow 가 쓰는 만큼만 흉내 낸 행 목록이다.
type fakeRows struct {
	rows [][]any
	i    int
}

func (r *fakeRows) Close()                                       {}
func (r *fakeRows) Err() error                                   { return nil }
func (r *fakeRows) CommandTag() pgconn.CommandTag                { return pgconn.NewCommandTag("SELECT") }
func (r *fakeRows) FieldDescriptions() []pgconn.FieldDescription { return nil }
func (r *fakeRows) Values() ([]any, error)                       { return r.rows[r.i-1], nil }
func (r *fakeRows) RawValues() [][]byte                          { return nil }
func (r *fakeRows) Conn() *pgx.Conn                              { return nil }
func (r *fakeRows) Next() bool {
	r.i++
	return r.i <= len(r.rows)
}

func (r *fakeRows) Scan(dest ...any) error {
	for j, d := range dest {
		reflect.ValueOf(d).Elem().Set(reflect.ValueOf(r.rows[r.i-1][j]))
	}
	return nil
}

type fakeSnapQuerier map[string][][]any

func (q fakeSnapQuerier) Query(_ context.Context, sql string, _ ...any) (pgx.Rows, error) {
	rows, ok := q[sql]
	if !ok {
		return nil, fmt.Errorf("unexpected query %q", sql)
	}
	return &fakeRows{rows: rows}, nil
}

func TestReadSnapshot_테이블_목록에_없는_테이블의_열_인덱스_제약은_버린다(t *testing.T) {
	q := fakeSnapQuerier{
		snapRelQuery: {{"t", "r", ""}},
		snapColQuery: {
			{"t", "id", "integer", true, "", "", ""},
			{"ghost", "x", "text", false, "", "", ""},
		},
		snapObjQuery: {
			{"t", true, "t_idx", "CREATE INDEX t_idx ON t USING btree (id)"},
			{"ghost", true, "g_idx", "CREATE INDEX g_idx ON ghost USING btree (x)"},
			{"ghost", false, "g_pk", "PRIMARY KEY (x)"},
		},
	}
	snap, err := readSnapshot(context.Background(), q, "s")
	if err != nil {
		t.Fatalf("없는 테이블 줄에서 멈추면 안 된다: %v", err)
	}
	if len(snap.Tables) != 1 || len(snap.Tables["t"].Columns) != 1 || len(snap.Tables["t"].Indexes) != 1 {
		t.Fatalf("스냅숏: %+v", snap.Tables)
	}
}

// ─── 되돌리기 INSERT·RETURNING 권한 ───

const v7fixFixture = `create schema v7fix;
create table v7fix.t (id int primary key, name text, secret text);
insert into v7fix.t values (1, 'a', 's1'), (2, 'b', 's2');
create function v7fix.skip() returns trigger language plpgsql as $$ begin return null; end $$;`

func TestPG_되살리는_INSERT_를_트리거가_건너뛰면_되돌리기는_실패한다(t *testing.T) {
	p := v7dbPG(t, v7fixFixture, "v7fix")
	cs := ChangeSet{Table: TableRef{"v7fix", "t"}, PK: []string{"id"}, Changes: []Change{
		{Op: ChangeDelete, Key: map[string]string{"id": "1"}, Old: map[string]*string{"name": strp("a")}},
	}}
	res, err := p.ApplyChanges(ctx(t), cs)
	must(t, err)
	undo, err := UndoChanges(cs, res.Applied)
	must(t, err)
	if _, err := p.pool.Exec(ctx(t), `create trigger skip before insert on v7fix.t for each row execute function v7fix.skip()`); err != nil {
		t.Fatal(err)
	}
	if _, err := p.ApplyChanges(ctx(t), undo); err == nil || !strings.Contains(err.Error(), "affected 0 rows, expected 1") {
		t.Fatalf("0행 되살리기는 실패여야 한다: %v", err)
	}
	got, err := p.Run(ctx(t), `select count(*) from v7fix.t where id = 1`)
	must(t, err)
	if got.Rows[0][0] != "0" {
		t.Fatalf("행이 없어야 한다: %v", got.Rows)
	}
}

func TestPG_RETURNING_권한이_없으면_RETURNING_없이_반영하고_되돌릴_수_없다고_표시한다(t *testing.T) {
	p := v7dbPG(t, v7fixFixture, "v7fix")
	const role = "v7fix_limited"
	if _, err := p.pool.Exec(ctx(t), `drop role if exists `+role+`; create role `+role+`;
		grant usage on schema v7fix to `+role+`; grant select (id), update (name) on v7fix.t to `+role); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		c, cancel := context.WithTimeout(context.Background(), 10*time.Second)
		defer cancel()
		_, _ = p.Run(c, "reset role")
		if _, err := p.pool.Exec(c, `drop owned by `+role+`; drop role `+role); err != nil {
			t.Errorf("cleanup role: %v", err)
		}
	})
	_, err := p.Run(ctx(t), "set role "+role)
	must(t, err)
	cs := ChangeSet{Table: TableRef{"v7fix", "t"}, PK: []string{"id"}, Changes: []Change{
		{Op: ChangeUpdate, Key: map[string]string{"id": "1"}, Values: map[string]*string{"name": strp("q")}},
	}}
	res, err := p.ApplyChanges(ctx(t), cs)
	must(t, err)
	if !res.Applied.NoReturning || res.Applied.Rows[0] != nil {
		t.Fatalf("RETURNING 없이 반영했다고 표시해야 한다: %+v", res.Applied)
	}
	if _, err := UndoChanges(cs, res.Applied); err == nil {
		t.Fatal("되돌리기 묶음을 만들면 안 된다")
	}
	_, err = p.Run(ctx(t), "reset role")
	must(t, err)
	got, err := p.Run(ctx(t), `select name from v7fix.t where id = 1`)
	must(t, err)
	if got.Rows[0][0] != "q" {
		t.Fatalf("반영돼야 한다: %v", got.Rows)
	}
}

// ─── 잠긴 prod 읽기의 READ ONLY 트랜잭션 (R54) ───

func TestPG_ReadOnly_문장은_숨은_쓰기를_서버가_거절하고_열린_트랜잭션은_원래대로다(t *testing.T) {
	p := v7dbPG(t, v7fixFixture+";create sequence v7fix.s;", "v7fix")
	ro := func(sql string) (*Result, error) { return p.RunArgs(ctx(t), Query{SQL: sql, ReadOnly: true}) }
	for _, sql := range []string{
		"with d as (delete from v7fix.t where id = 1 returning *) select * from d",
		"select nextval('v7fix.s')",
	} {
		if _, err := ro(sql); err == nil || !strings.Contains(err.Error(), "read-only transaction") {
			t.Errorf("%s: 서버가 거절해야 한다: %v", sql, err)
		}
	}
	// select … into 는 커서(DECLARE)가 먼저 거절한다
	if _, err := ro("select * into v7fix.copy from v7fix.t"); err == nil {
		t.Error("select … into 는 거절해야 한다")
	}
	res, err := ro("select count(*) from v7fix.t")
	must(t, err)
	if res.Rows[0][0] != "2" || res.InTx {
		t.Fatalf("읽기는 되고 트랜잭션을 남기지 않는다: %+v", res)
	}

	// 열린 트랜잭션 안: 거절된 뒤에도 트랜잭션은 깨지지 않고 쓰기 모드 그대로다
	_, err = p.Run(ctx(t), "begin")
	must(t, err)
	_, err = p.Run(ctx(t), "update v7fix.t set name = 'w' where id = 2")
	must(t, err)
	if _, err := ro("with d as (delete from v7fix.t where id = 1 returning *) select * from d"); err == nil {
		t.Fatal("트랜잭션 안에서도 거절해야 한다")
	}
	res, err = ro("select name from v7fix.t where id = 2")
	must(t, err)
	if res.Rows[0][0] != "w" || !res.InTx {
		t.Fatalf("트랜잭션 안의 읽기: %+v", res)
	}
	_, err = p.Run(ctx(t), "update v7fix.t set name = 'w2' where id = 2")
	must(t, err)
	_, err = p.Run(ctx(t), "commit")
	must(t, err)
	got, err := p.Run(ctx(t), "select id, name from v7fix.t order by id")
	must(t, err)
	if len(got.Rows) != 2 || got.Rows[1][1] != "w2" {
		t.Fatalf("트랜잭션 결과: %v", got.Rows)
	}
}
