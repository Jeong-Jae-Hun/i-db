package db

import (
	"context"
	"reflect"
	"testing"
	"time"
)

// v5dbFixture 는 FK·영향 행 수 시험용이다. members_org_fk 는 (b, a) 순서로 선언해 attnum 순서와 다르게 둔다.
const v5dbFixture = `
create schema v5db;
create table v5db.users (id int primary key, parent_id int references v5db.users(id));
create table v5db.orgs (a int, b int, primary key (a, b));
create table v5db.members (
	id int primary key,
	user_id int constraint members_user_fk references v5db.users(id),
	org_b int, org_a int,
	constraint members_org_fk foreign key (org_b, org_a) references v5db.orgs (b, a)
);
create table v5db.events (id int, user_id int constraint events_user_fk references v5db.users(id)) partition by range (id);
create table v5db.events_1 partition of v5db.events for values from (0) to (100);
create table v5db.pref (id int primary key) partition by range (id);
create table v5db.pref_1 partition of v5db.pref for values from (0) to (100);
create table v5db.pref_2 partition of v5db.pref for values from (100) to (200);
create table v5db.link (pref_id int constraint link_pref_fk references v5db.pref(id));
insert into v5db.users select g, null from generate_series(1, 2000) g;
insert into v5db.orgs values (1, 2), (3, 4);
insert into v5db.members values (1, 1, 2, 1), (2, 1, 4, 3), (3, 2, 2, 1);
insert into v5db.events select g, g + 1 from generate_series(0, 99) g;
analyze v5db.users;
analyze v5db.events;
analyze v5db.members;
`

// v5dbPG 는 v5db 픽스처를 깐 접속을 돌려주고 끝나면 스키마를 지운다.
func v5dbPG(t *testing.T) *PGConn {
	t.Helper()
	cs := NewClients()
	t.Cleanup(cs.Close)
	p, err := cs.PG(ctx(t), testPGConn(t))
	must(t, err)
	const drop = "drop schema if exists v5db cascade"
	_, err = p.pool.Exec(ctx(t), drop+";"+v5dbFixture)
	must(t, err)
	t.Cleanup(func() {
		c, cancel := context.WithTimeout(context.Background(), 10*time.Second)
		defer cancel()
		if _, err := p.pool.Exec(c, drop); err != nil {
			t.Errorf("cleanup: %v", err)
		}
	})
	return p
}

func TestForeignKeys_나가는_FK_는_선언한_열_순서와_자기_참조를_싣는다(t *testing.T) {
	p := v5dbPG(t)
	users, members := TableRef{"v5db", "users"}, TableRef{"v5db", "members"}
	got, err := p.ForeignKeys(ctx(t), members)
	must(t, err)
	want := []FK{
		{Name: "members_org_fk", Table: members, Cols: []string{"org_b", "org_a"}, Ref: TableRef{"v5db", "orgs"}, RefCols: []string{"b", "a"}},
		{Name: "members_user_fk", Table: members, Cols: []string{"user_id"}, Ref: users, RefCols: []string{"id"}},
	}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("members:\n%+v\nwant\n%+v", got, want)
	}
	got, err = p.ForeignKeys(ctx(t), users)
	must(t, err)
	if len(got) != 1 || got[0].Ref != users || !reflect.DeepEqual(got[0].Cols, []string{"parent_id"}) {
		t.Fatalf("자기 참조: %+v", got)
	}
	// 파티션 테이블을 가리키면 참조 대상 파티션마다 복제 제약이 생기지만 FK 는 하나다
	got, err = p.ForeignKeys(ctx(t), TableRef{"v5db", "link"})
	must(t, err)
	if len(got) != 1 || got[0].Ref != (TableRef{"v5db", "pref"}) {
		t.Fatalf("파티션 참조: %+v", got)
	}
	// 파티션은 부모에게 물려받은 FK 를 자기 FK 로 가진다
	got, err = p.ForeignKeys(ctx(t), TableRef{"v5db", "events_1"})
	must(t, err)
	if len(got) != 1 || got[0].Ref != users || got[0].Table != (TableRef{"v5db", "events_1"}) {
		t.Fatalf("파티션의 FK: %+v", got)
	}
	if got, err = p.ForeignKeys(ctx(t), TableRef{"v5db", "orgs"}); err != nil || len(got) != 0 {
		t.Fatalf("FK 없음: %+v %v", got, err)
	}
}

func TestReferencedBy_들어오는_FK_는_자기_참조를_포함하고_파티션_복제는_뺀다(t *testing.T) {
	p := v5dbPG(t)
	users := TableRef{"v5db", "users"}
	got, err := p.ReferencedBy(ctx(t), users)
	must(t, err)
	var names []string
	for _, fk := range got {
		names = append(names, fk.Table.Table+"."+fk.Name)
		if fk.Ref != users || !reflect.DeepEqual(fk.RefCols, []string{"id"}) {
			t.Errorf("참조 대상: %+v", fk)
		}
	}
	if want := []string{"events.events_user_fk", "members.members_user_fk", "users.users_parent_id_fkey"}; !reflect.DeepEqual(names, want) {
		t.Fatalf("users 를 가리키는 FK: %v want %v", names, want)
	}
	got, err = p.ReferencedBy(ctx(t), TableRef{"v5db", "orgs"})
	must(t, err)
	if len(got) != 1 || !reflect.DeepEqual(got[0].Cols, []string{"org_b", "org_a"}) || !reflect.DeepEqual(got[0].RefCols, []string{"b", "a"}) {
		t.Fatalf("여러 열 FK: %+v", got)
	}
	// 참조 대상 파티션 하나를 봐도 부모 FK 가 보인다
	got, err = p.ReferencedBy(ctx(t), TableRef{"v5db", "pref_1"})
	must(t, err)
	if len(got) != 1 || got[0].Table != (TableRef{"v5db", "link"}) {
		t.Fatalf("파티션을 가리키는 FK: %+v", got)
	}
}

func TestFKWhere_식별자와_값을_따옴표로_감싸_AND_로_잇는다(t *testing.T) {
	cases := []struct {
		cols, vals []string
		want       string
	}{
		{[]string{"id"}, []string{"1315"}, `"id" = '1315'`},
		{[]string{"org_b", `we"ird`}, []string{"2", `o'k\x`}, `"org_b" = '2' AND "we""ird" = E'o''k\\x'`},
	}
	for _, c := range cases {
		if got, err := FKWhere(c.cols, c.vals); err != nil || got != c.want {
			t.Errorf("got %s %v want %s", got, err, c.want)
		}
	}
}

func TestFKWhere_열과_값_수가_다르거나_비면_패닉_없이_오류다(t *testing.T) {
	for _, c := range [][2][]string{{{"a", "b"}, {"1"}}, {{"a"}, {"1", "2"}}, {nil, nil}} {
		if got, err := FKWhere(c[0], c[1]); err == nil {
			t.Errorf("%v %v: 오류여야 한다, got %q", c[0], c[1], got)
		}
	}
}

func TestFKWhere_정수_열에_문자열_리터럴로_넣어도_PostgreSQL_이_맞춰_찾는다(t *testing.T) {
	p := v5dbPG(t)
	var n int
	where, err := FKWhere([]string{"org_b", "org_a"}, []string{"2", "1"})
	must(t, err)
	must(t, p.pool.QueryRow(ctx(t), `select count(*) from v5db.members where `+where).Scan(&n))
	if n != 2 {
		t.Fatalf("count: %d", n)
	}
}
