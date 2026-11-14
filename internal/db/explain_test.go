package db

import (
	"context"
	"testing"
	"time"
)

func TestExplainSQL_옵션을_붙이고_끝_세미콜론을_뗀다(t *testing.T) {
	cases := []struct {
		stmt string
		opt  ExplainOpt
		want string
	}{
		{"select 1;", ExplainOpt{Analyze: true, Buffers: true}, "EXPLAIN (ANALYZE, BUFFERS, FORMAT TEXT) select 1"},
		{"  select 1 ; ;\n", ExplainOpt{Format: "json"}, "EXPLAIN (FORMAT JSON) select 1"},
		{"select 1", ExplainOpt{Format: "text) delete from t; --"}, "EXPLAIN (FORMAT TEXT) select 1"},
		{"select 1 -- 끝;", ExplainOpt{}, "EXPLAIN (FORMAT TEXT) select 1 -- 끝"},
		{"/* x */ EXPLAIN analyze select 1;", ExplainOpt{Buffers: true}, "/* x */ EXPLAIN analyze select 1"},
		{" ; ", ExplainOpt{Analyze: true}, ""},
	}
	for _, c := range cases {
		if got := ExplainSQL(c.stmt, c.opt); got != c.want {
			t.Errorf("%q %+v: got %q want %q", c.stmt, c.opt, got, c.want)
		}
	}
}

func TestExplainSQL_ANALYZE를_붙인_결과는_읽기_전용이_아니다(t *testing.T) {
	if IsReadOnlySQL(ExplainSQL("delete from t", ExplainOpt{Analyze: true})) {
		t.Fatal("EXPLAIN ANALYZE 는 확인 대상이어야 한다")
	}
	if !IsReadOnlySQL(ExplainSQL("delete from t", ExplainOpt{})) {
		t.Fatal("ANALYZE 없는 EXPLAIN 은 읽기 전용이다")
	}
}

func TestModifyTableRows_Outer_자식의_Plan_Rows_를_쓰고_InitPlan_은_건너뛴다(t *testing.T) {
	raw := `[{"Plan": {"Node Type": "ModifyTable", "Plan Rows": 0, "Plans": [
		{"Node Type": "Result", "Parent Relationship": "InitPlan", "Plan Rows": 1},
		{"Node Type": "Seq Scan", "Parent Relationship": "Outer", "Plan Rows": 1234.6}]}}]`
	if n, err := modifyTableRows([]byte(raw)); err != nil || n != 1235 {
		t.Fatalf("got %d %v", n, err)
	}
	for _, bad := range []string{
		`[{"Plan": {"Node Type": "Seq Scan", "Plan Rows": 5}}]`,
		`[{"Plan": {"Node Type": "ModifyTable", "Plan Rows": 0}}]`,
		`not json`,
	} {
		if _, err := modifyTableRows([]byte(bad)); err == nil {
			t.Errorf("오류여야 한다: %s", bad)
		}
	}
}

func TestModifyTableRows_PG13_이하의_Member_자식은_Plan_Rows_를_더한다(t *testing.T) {
	// PostgreSQL 13 의 파티션 테이블 DELETE 계획(ModifyTable 아래 파티션마다 Member)
	raw := `[{"Plan": {"Node Type": "ModifyTable", "Operation": "Delete", "Plan Rows": 0, "Plans": [
		{"Node Type": "Seq Scan", "Parent Relationship": "Member", "Relation Name": "events_1", "Plan Rows": 40.4},
		{"Node Type": "Seq Scan", "Parent Relationship": "Member", "Relation Name": "events_2", "Plan Rows": 9.4},
		{"Node Type": "Result", "Parent Relationship": "InitPlan", "Plan Rows": 1}]}}]`
	if n, err := modifyTableRows([]byte(raw)); err != nil || n != 50 {
		t.Fatalf("got %d %v", n, err)
	}
}

func TestEstimateRows_UPDATE_DELETE_는_실행하지_않고_바꿀_행_수를_추정한다(t *testing.T) {
	p := v5dbPG(t)
	cases := map[string][2]int64{
		"delete from v5db.users where id <= 100":                                {80, 120},
		"update v5db.users set parent_id = 1 where id > 1000;":                  {800, 1200},
		"/* 주석 */ delete from v5db.users where id < 10 returning id":            {5, 15},
		"update v5db.users set parent_id = 2 where id = 5 and 'analyze' <> ''":  {1, 1},
		"delete from v5db.events where id < 50":                                 {40, 60}, // 파티션 테이블은 Append 가 Outer 자식이다
		"delete from v5db.users where id in (select user_id from v5db.members)": {1, 10},
	}
	for sql, rng := range cases {
		n, err := p.EstimateRows(ctx(t), sql)
		if err != nil || n < rng[0] || n > rng[1] {
			t.Errorf("%s: got %d %v, want %d..%d", sql, n, err, rng[0], rng[1])
		}
	}
	var count, touched int
	must(t, p.pool.QueryRow(ctx(t), "select count(*), count(parent_id) from v5db.users").Scan(&count, &touched))
	if count != 2000 || touched != 0 {
		t.Fatalf("문장이 실행됐다: %d행, parent_id %d", count, touched)
	}
}

func TestEstimateRows_UPDATE_DELETE_한_문장이_아니거나_ANALYZE_가_있으면_거부한다(t *testing.T) {
	p := v5dbPG(t)
	for _, sql := range []string{
		"select * from v5db.users",
		"insert into v5db.users values (5000)",
		"with d as (delete from v5db.users returning id) select * from d",
		"delete from v5db.users where id = 1; delete from v5db.users",
		"delete from v5db.users where id = 1; explain analyze delete from v5db.users",
		"explain analyze delete from v5db.users",
		"delete from v5db.users where analyze = 1",
		"",
	} {
		if n, err := p.EstimateRows(ctx(t), sql); err == nil {
			t.Errorf("%q: 거부해야 한다, got %d", sql, n)
		}
	}
	var count int
	must(t, p.pool.QueryRow(ctx(t), "select count(*) from v5db.users").Scan(&count))
	if count != 2000 {
		t.Fatalf("문장이 실행됐다: %d", count)
	}
}

func TestEstimateRows_편집기_search_path_를_따르고_풀_연결에는_남기지_않는다(t *testing.T) {
	p := v5dbPG(t)
	if _, err := p.EstimateRows(ctx(t), "delete from users where id = 1"); err == nil {
		t.Fatal("search_path 를 고르기 전에는 users 를 찾지 못해야 한다")
	}
	must(t, p.SetSearchPath(ctx(t), "v5db"))
	if n, err := p.EstimateRows(ctx(t), "delete from users where id = 1"); err != nil || n != 1 {
		t.Fatalf("search_path 를 따라야 한다: %d %v", n, err)
	}
	// 풀 연결을 모두 잡아 보며 SET LOCAL 이 남은 연결이 없는지 본다
	stat := p.pool.Stat()
	for range stat.TotalConns() {
		c, err := p.pool.Acquire(ctx(t))
		must(t, err)
		defer c.Release()
		var sp string
		must(t, c.QueryRow(ctx(t), "show search_path").Scan(&sp))
		if sp != `"$user", public` {
			t.Fatalf("풀 연결 search_path: %q", sp)
		}
	}
}

func TestEstimateRows_잠금에_막히면_ctx_시간_안에_돌아온다(t *testing.T) {
	p := v5dbPG(t)
	locker, err := p.pool.Acquire(ctx(t))
	must(t, err)
	defer locker.Release()
	_, err = locker.Exec(ctx(t), "begin; lock table v5db.users in access exclusive mode")
	must(t, err)
	defer func() { _, _ = locker.Exec(context.Background(), "rollback") }()

	c, cancel := context.WithTimeout(context.Background(), 300*time.Millisecond)
	defer cancel()
	start := time.Now()
	if _, err := p.EstimateRows(c, "delete from v5db.users where id = 1"); err == nil {
		t.Fatal("잠금에 막혀 시간 초과여야 한다")
	}
	if d := time.Since(start); d > 2*time.Second {
		t.Fatalf("시간 제한을 넘겼다: %v", d)
	}
}
