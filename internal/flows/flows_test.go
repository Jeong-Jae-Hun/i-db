package flows

import (
	"context"
	"net/url"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"testing"
	"time"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
)

var conns = []db.Conn{
	{Name: "shop-stg", Kind: db.KindPG},
	{Name: "aws-stg", Kind: db.KindAWS},
	{Name: "cache", Kind: db.KindRedis},
	{UUID: "gone"}, // 지운 접속 자리
}

func writeFlows(t *testing.T, body string) string {
	t.Helper()
	dir := t.TempDir()
	if err := os.MkdirAll(filepath.Join(dir, ".dbtui"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, file), []byte(body), 0o600); err != nil {
		t.Fatal(err)
	}
	return dir
}

func TestLoad_파일이_없으면_빈_목록이고_깨진_JSON_은_오류다(t *testing.T) {
	if fs, err := Load(t.TempDir(), conns); err != nil || fs != nil {
		t.Fatalf("없음: %v %v", fs, err)
	}
	if _, err := Load(writeFlows(t, `[{"name":`), conns); err == nil || !strings.Contains(err.Error(), ".dbtui/flows.json") {
		t.Fatalf("깨진 JSON: %v", err)
	}
}

func TestLoad_틀린_항목만_까닭을_적고_나머지는_그대로_쓴다(t *testing.T) {
	dir := writeFlows(t, `[
		{"name":"ok","outbox":{"conn":"shop-stg","table":"public.t_outbox","type":"event_type","retry":"retry_count","due":"check_needed_at","where":"event_type like 'DATA_%'"},"aws":"aws-stg","topic":"stg-hub"},
		{"name":"topic-only","aws":"aws-stg","topic":"arn:aws:sns:ap-northeast-2:1:t"},
		{"name":"redis","outbox":{"conn":"cache","table":"t","type":"x"},"aws":"aws-stg","topic":"t"},
		{"name":"pg-as-aws","aws":"shop-stg","topic":"t"},
		{"name":"no-conn","aws":"nope","topic":"t"},
		{"name":"no-topic","aws":"aws-stg"},
		{"name":"two-stmt","outbox":{"conn":"shop-stg","table":"t","type":"x","where":"1=1; delete from t"},"aws":"aws-stg","topic":"t"},
		{"name":"no-type","outbox":{"conn":"shop-stg","table":"t"},"aws":"aws-stg","topic":"t"},
		{"name":"deep","outbox":{"conn":"shop-stg","table":"a.b.c","type":"x"},"aws":"aws-stg","topic":"t"},
		{"name":"ok","aws":"aws-stg","topic":"t"},
		{"aws":"aws-stg","topic":"t"}
	]`)
	fs, err := Load(dir, conns)
	if err != nil {
		t.Fatal(err)
	}
	want := []string{
		"", "",
		"outbox.conn cache: want kind postgresql, got redis",
		"aws shop-stg: want kind aws, got postgresql",
		"aws nope: no such connection",
		"outbox or topic is required",
		"outbox.where must be a single condition without ';'",
		"outbox.type is empty",
		`outbox.table must be table or schema.table: "a.b.c"`,
		"duplicate name ok",
		"name is empty",
	}
	if len(fs) != len(want) {
		t.Fatalf("항목 수: %d", len(fs))
	}
	for i, w := range want {
		if fs[i].Err != w {
			t.Errorf("%d %s: %q, want %q", i, fs[i].Name, fs[i].Err, w)
		}
	}
	if fs[0].Outbox == nil || fs[0].Outbox.Due != "check_needed_at" || fs[1].Outbox != nil {
		t.Fatalf("값: %+v %+v", fs[0].Outbox, fs[1])
	}
	if fs[10].Name != "#11" {
		t.Fatalf("이름 없는 항목은 자리로 부른다: %q", fs[10].Name)
	}
}

func TestCheckWhere_문자열_주석_안의_세미콜론은_받는다(t *testing.T) {
	for where, ok := range map[string]bool{
		"":                              true,
		"event_type like 'a;b'":         true,
		"x = 1 -- 끝;":                   true,
		"x = 1;":                        false,
		"x = 1; select 1":               false,
		"x = 1 /* ; */ and y = $$;$$":   true,
		"x = 1;\n-- trailing comment\n": false,
	} {
		if err := checkWhere(where); (err == nil) != ok {
			t.Errorf("%q: %v", where, err)
		}
	}
}

func intp(n int) *int { return &n }

func TestLoad_토픽_없는_아웃박스_흐름과_한도_설정을_검사한다(t *testing.T) {
	dir := writeFlows(t, `[
		{"name":"outbox-only","outbox":{"conn":"shop-stg","table":"public.t_outbox","type":"event_type","retry":"retry_count","due":"check_needed_at","limit":5,
			"types":{"CREATE_PAYOUT":{"limit":20,"consumer":"consume-outbox-create-payout"},"CLIP":{"consumer":"consume-outbox-clip"}}}},
		{"name":"aws-no-topic","outbox":{"conn":"shop-stg","table":"t","type":"x"},"aws":"aws-stg"},
		{"name":"topic-no-aws","topic":"t"},
		{"name":"zero-limit","outbox":{"conn":"shop-stg","table":"t","type":"x","retry":"r","limit":0}},
		{"name":"type-zero","outbox":{"conn":"shop-stg","table":"t","type":"x","retry":"r","types":{"A":{"limit":0}}}},
		{"name":"empty-type","outbox":{"conn":"shop-stg","table":"t","type":"x","retry":"r","types":{" ":{"consumer":"c"}}}},
		{"name":"limit-no-retry","outbox":{"conn":"shop-stg","table":"t","type":"x","limit":3}},
		{"name":"types-no-retry","outbox":{"conn":"shop-stg","table":"t","type":"x","types":{"A":{"consumer":"c"}}}}
	]`)
	fs, err := Load(dir, conns)
	if err != nil {
		t.Fatal(err)
	}
	want := []string{
		"",
		"aws requires topic",
		"aws is empty",
		"outbox.limit must be >= 1",
		"outbox.types.A.limit must be >= 1",
		"outbox.types has an empty type",
		"outbox.limit requires outbox.retry",
		"outbox.types requires outbox.retry",
	}
	for i, w := range want {
		if fs[i].Err != w {
			t.Errorf("%d %s: %q, want %q", i, fs[i].Name, fs[i].Err, w)
		}
	}
	o := fs[0].Outbox
	if o.limit() != 5 || *o.Types["CREATE_PAYOUT"].Limit != 20 || o.Types["CLIP"].Limit != nil || o.Types["CLIP"].Consumer != "consume-outbox-clip" {
		t.Fatalf("값: %+v", o)
	}
	if (Outbox{}).limit() != DefaultLimit {
		t.Fatal("limit 이 없으면 기본 한도다")
	}
}

func TestCountSQL_종류별로_묶고_식별자를_감싸고_where_를_AND_괄호로_붙인다(t *testing.T) {
	o := Outbox{Table: `public.t"x`, Type: "event_type", Retry: "retry_count", Due: "check_needed_at", Where: "event_type like 'DATA_%' -- 주석"}
	r, live := `coalesce("retry_count", 0)`, `coalesce("retry_count", 0) < 5`
	want := `select "event_type"::text as type, count(*) as rows, count(*) filter (where ` + live + `) as waiting, ` +
		`count(*) filter (where ` + r + ` > 0 and ` + live + `) as retrying, count(*) filter (where ` + r + ` >= 5) as exhausted, ` +
		`count(*) filter (where "check_needed_at" < now() - interval '5 minutes' and ` + live + `) as stale, ` +
		`min("check_needed_at") filter (where ` + live + `) as oldest_due` +
		`, "event_type"::text is null as type_null, grouping("event_type"::text) as total from "public"."t""x" where (` +
		"\nevent_type like 'DATA_%' -- 주석\n)" + ` group by grouping sets (("event_type"::text), ()) order by 9 desc, 5 desc nulls last, 3 desc, 1`
	if got := CountSQL(o); got != want {
		t.Fatalf("got  %s\nwant %s", got, want)
	}
	// retry·due 가 없으면 대기는 모든 행이고 나머지는 null 이며 where 도 없다
	min := `select "x"::text as type, count(*) as rows, count(*) as waiting, null::bigint as retrying, null::bigint as exhausted, null::bigint as stale, null::timestamptz as oldest_due, "x"::text is null as type_null, grouping("x"::text) as total from "t_outbox" group by grouping sets (("x"::text), ()) order by 9 desc, 5 desc nulls last, 3 desc, 1`
	if got := CountSQL(Outbox{Table: "t_outbox", Type: "x"}); got != min {
		t.Fatalf("최소: %s", got)
	}
	if !db.IsReadOnlySQL(CountSQL(o)) || !db.IsReadOnlySQL(RetrySQL(o, TypeStat{Name: "A"})) {
		t.Fatal("읽기 문장이어야 한다")
	}
}

func TestCountSQL_종류별_한도는_리터럴을_감싼_CASE_다(t *testing.T) {
	o := Outbox{Table: "t", Type: "event_type", Retry: "retry_count", Limit: intp(3),
		Types: map[string]TypeConf{"O'BRIEN": {Limit: intp(20)}, `A\B`: {Limit: intp(7)}, "CLIP": {Consumer: "c"}}}
	want := `case "event_type"::text when E'A\\B' then 7 when 'O''BRIEN' then 20 else 3 end`
	if got := limitExpr(o); got != want {
		t.Fatalf("got  %s\nwant %s", got, want)
	}
	// due 가 없으면 대기·재시도·한도 소진 세 곳에 쓰인다
	if got := CountSQL(o); strings.Count(got, want) != 3 {
		t.Fatalf("CASE 자리: %s", got)
	}
	// 종류별 한도가 없으면 기본 한도 숫자다
	if got := limitExpr(Outbox{Retry: "r", Types: map[string]TypeConf{"CLIP": {Consumer: "c"}}}); got != "5" {
		t.Fatalf("기본: %s", got)
	}
}

func TestRetrySQL_종류_하나의_재시도_행을_where_와_AND_로_묻는다(t *testing.T) {
	o := Outbox{Table: "public.t_outbox", Type: "event_type", Retry: "retry_count", Due: "check_needed_at", Where: "x = 1"}
	want := `select * from "public"."t_outbox" where "event_type"::text = 'O''B' and coalesce("retry_count", 0) > 0 and (` +
		"\nx = 1\n)" + ` order by coalesce("retry_count", 0) desc, "check_needed_at" limit 50`
	if got := RetrySQL(o, TypeStat{Name: "O'B"}); got != want {
		t.Fatalf("got  %s\nwant %s", got, want)
	}
	if got := RetrySQL(Outbox{Table: "t", Type: "x"}, TypeStat{Name: "A"}); got != `select * from "t" where "x"::text = 'A' limit 50` {
		t.Fatalf("최소: %s", got)
	}
	// NULL 종류는 글자 'NULL' 과 섞이지 않게 is null 로 묻는다
	if got := RetrySQL(Outbox{Table: "t", Type: "x"}, TypeStat{Name: "NULL", Null: true}); got != `select * from "t" where "x"::text is null limit 50` {
		t.Fatalf("NULL 종류: %s", got)
	}
}

func TestStat_Metrics_는_준_열만_보이고_재시도_한도_소진_오래됨이_있으면_경고다(t *testing.T) {
	cases := []struct {
		s    Stat
		want string
		warn bool
	}{
		{Stat{Pending: 120, Retry: 3, Stale: 1, Exhausted: 2}, "대기 120 · 재시도 3 · 한도 소진 2 · 오래됨 1", true},
		{Stat{Pending: 5, Retry: 0, Stale: 0, Exhausted: 0}, "대기 5 · 재시도 0 · 한도 소진 0 · 오래됨 0", false},
		{Stat{Pending: 5, Retry: -1, Stale: -1, Exhausted: -1}, "대기 5", false},
		{Stat{Pending: 5, Retry: -1, Stale: 2, Exhausted: -1}, "대기 5 · 오래됨 2", true},
		{Stat{Pending: 5, Retry: 0, Stale: 0, Exhausted: 1}, "대기 5 · 재시도 0 · 한도 소진 1 · 오래됨 0", true},
	}
	for _, c := range cases {
		if got, warn := c.s.Metrics(); got != c.want || warn != c.warn {
			t.Errorf("%+v: %q %v", c.s, got, warn)
		}
	}
	for _, c := range []struct {
		t    TypeStat
		want string
		warn bool
	}{
		{TypeStat{Waiting: 3, Retrying: 1, Exhausted: 2}, "대기 3 · 재시도 1 · 한도 소진 2", true},
		{TypeStat{Waiting: 3, Retrying: 1, Exhausted: 0}, "대기 3 · 재시도 1 · 한도 소진 0", false},
		{TypeStat{Waiting: 3, Retrying: 0, Exhausted: 0, Stale: 1}, "대기 3 · 재시도 0 · 한도 소진 0 · 오래됨 1", true},
		{TypeStat{Waiting: 3, Retrying: -1, Exhausted: -1, Stale: -1}, "대기 3", false},
	} {
		if got, warn := c.t.Metrics(); got != c.want || warn != c.warn {
			t.Errorf("%+v: %q %v", c.t, got, warn)
		}
	}
}

// ─── 통합 (DBTUI_TEST_PG_URL 이 있을 때만) ───

func testPG(t *testing.T) (*db.PGConn, context.Context) {
	t.Helper()
	raw := os.Getenv("DBTUI_TEST_PG_URL")
	if raw == "" {
		t.Skip("DBTUI_TEST_PG_URL not set")
	}
	u, err := url.Parse(raw)
	if err != nil {
		t.Fatal(err)
	}
	port, _ := strconv.Atoi(u.Port())
	pw, _ := u.User.Password()
	c := db.Conn{Name: "flows-it", UUID: "flows-it", Kind: db.KindPG, Host: u.Hostname(), Port: port, DB: strings.TrimPrefix(u.Path, "/"),
		User: u.User.Username(), Password: pw}
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	t.Cleanup(cancel)
	cs := db.NewClients()
	t.Cleanup(cs.Close)
	pg, err := cs.PG(ctx, c)
	if err != nil {
		t.Fatal(err)
	}
	return pg, ctx
}

func TestIntegration_아웃박스_수와_종류별_표를_READ_ONLY_로_읽는다(t *testing.T) {
	pg, ctx := testPG(t)
	for _, sql := range []string{
		`drop schema if exists flows_it cascade`,
		`create schema flows_it`,
		`create table flows_it."T_outbox" (id serial, event_type text, retry_count int, check_needed_at timestamptz)`,
		`insert into flows_it."T_outbox" (event_type, retry_count, check_needed_at) values
			('DATA_A', 0, now()), ('DATA_A', 2, now() - interval '1 hour'), ('DATA_B', 0, now() - interval '10 minutes'), ('OTHER', 5, now() - interval '1 day')`,
	} {
		if _, err := pg.Run(ctx, sql); err != nil {
			t.Fatal(err)
		}
	}
	t.Cleanup(func() { _, _ = pg.Run(context.Background(), `drop schema if exists flows_it cascade`) })
	o := Outbox{Table: `flows_it.T_outbox`, Type: "event_type", Retry: "retry_count", Due: "check_needed_at", Where: "event_type like 'DATA_%'"}
	s, err := Count(ctx, pg, o)
	// DATA_A: 0(대기) · 2(재시도, 1시간 지남 → 오래됨) · DATA_B: 0(10분 지남 → 오래됨) — OTHER 는 where 밖
	if err != nil || s.Pending != 3 || s.Retry != 1 || s.Stale != 2 || s.Exhausted != 0 || len(s.Types) != 2 {
		t.Fatalf("수: %+v %v", s, err)
	}
	s, err = Count(ctx, pg, Outbox{Table: o.Table, Type: o.Type})
	if err != nil || s.Pending != 4 || s.Retry != -1 || s.Stale != -1 || s.Exhausted != -1 {
		t.Fatalf("열 없는 수: %+v %v", s, err)
	}
	res, err := pg.RunReadOnly(ctx, CountSQL(o))
	if err != nil || strings.Join(res.Cols, ",") != "type,rows,waiting,retrying,exhausted,stale,oldest_due,type_null,total" {
		t.Fatalf("열: %+v %v", res, err)
	}
	// where 로 쓰기를 숨겨도 READ ONLY 가 막는다
	bad := o
	bad.Where = "event_type in (select 'x' from (with d as (delete from flows_it.\"T_outbox\" returning 1) select * from d) z)"
	if _, err := Count(ctx, pg, bad); err == nil {
		t.Fatal("쓰기는 막혀야 한다")
	}
	if s, _ := Count(ctx, pg, Outbox{Table: o.Table, Type: o.Type}); s.Pending != 4 {
		t.Fatalf("행이 지워졌다: %+v", s)
	}
}

func TestIntegration_종류별_한도로_세고_한도_소진은_오래됨이_아니다(t *testing.T) {
	pg, ctx := testPG(t)
	for _, sql := range []string{
		`drop schema if exists flows_it2 cascade`,
		`create schema flows_it2`,
		`create table flows_it2.t_outbox (id serial, event_type text, retry_count int, check_needed_at timestamptz)`,
		// EARN 한도 20: 0·7(재시도)·20(소진, 오래된 due). CLIP 한도 5: 5(소진, 오래된 due)·NULL(대기, 오래됨). O'B 한도 5: 4(재시도)
		`insert into flows_it2.t_outbox (event_type, retry_count, check_needed_at) values
			('EARN', 0, now()), ('EARN', 7, now()), ('EARN', 20, now() - interval '1 day'),
			('CLIP', 5, now() - interval '1 day'), ('CLIP', null, now() - interval '1 hour'),
			('O''B', 4, now())`,
	} {
		if _, err := pg.Run(ctx, sql); err != nil {
			t.Fatal(err)
		}
	}
	t.Cleanup(func() { _, _ = pg.Run(context.Background(), `drop schema if exists flows_it2 cascade`) })
	o := Outbox{Table: "flows_it2.t_outbox", Type: "event_type", Retry: "retry_count", Due: "check_needed_at",
		Types: map[string]TypeConf{"EARN": {Limit: intp(20)}, "O'B": {Consumer: "c"}}}
	s, err := Count(ctx, pg, o)
	if err != nil {
		t.Fatal(err)
	}
	got := map[string]TypeStat{}
	for _, ts := range s.Types {
		got[ts.Name] = ts
	}
	for name, w := range map[string][5]int64{ // rows·waiting·retrying·exhausted·stale
		"EARN": {3, 2, 1, 1, 0},
		"CLIP": {2, 1, 0, 1, 1},
		"O'B":  {1, 1, 1, 0, 0},
	} {
		g := got[name]
		if [5]int64{g.Rows, g.Waiting, g.Retrying, g.Exhausted, g.Stale} != w {
			t.Errorf("%s: %+v, want %v", name, g, w)
		}
	}
	// 한도 소진 많은 순 → 대기 많은 순 → 이름 순
	if len(s.Types) != 3 || s.Types[0].Name != "EARN" || s.Types[1].Name != "CLIP" || s.Types[2].Name != "O'B" {
		t.Fatalf("순서: %+v", s.Types)
	}
	if s.Pending != 4 || s.Retry != 2 || s.Exhausted != 2 || s.Stale != 1 {
		t.Fatalf("합: %+v", s)
	}
	// 종류 상세는 재시도 중·한도 소진 행만 retry 큰 순이다
	res, err := pg.RunReadOnly(ctx, RetrySQL(o, TypeStat{Name: "EARN"}))
	if err != nil || len(res.Rows) != 2 || res.Rows[0][2] != "20" || res.Rows[1][2] != "7" {
		t.Fatalf("종류 상세: %+v %v", res, err)
	}
	// 행이 없으면 열이 있어도 0 이다
	empty := o
	empty.Where = "false"
	if s, err := Count(ctx, pg, empty); err != nil || s.Pending != 0 || s.Retry != 0 || s.Exhausted != 0 || s.Stale != 0 || len(s.Types) != 0 {
		t.Fatalf("빈 표: %+v %v", s, err)
	}
}

func TestIntegration_종류가_상한을_넘어도_합은_서버에서_정확히_세고_잘림을_알린다(t *testing.T) {
	pg, ctx := testPG(t)
	for _, sql := range []string{
		`drop schema if exists flows_it3 cascade`,
		`create schema flows_it3`,
		`create table flows_it3.t_outbox (event_type text, retry_count int)`,
		// 종류 1,200개(행마다 retry 0), 그 가운데 T0007 만 한도 소진 행 하나를 더 든다
		`insert into flows_it3.t_outbox select 'T' || lpad(g::text, 4, '0'), 0 from generate_series(1, 1200) g`,
		`insert into flows_it3.t_outbox values ('T0007', 9)`,
	} {
		if _, err := pg.Run(ctx, sql); err != nil {
			t.Fatal(err)
		}
	}
	t.Cleanup(func() { _, _ = pg.Run(context.Background(), `drop schema if exists flows_it3 cascade`) })
	s, err := Count(ctx, pg, Outbox{Table: "flows_it3.t_outbox", Type: "event_type", Retry: "retry_count"})
	if err != nil {
		t.Fatal(err)
	}
	if s.Pending != 1200 || s.Exhausted != 1 || s.Retry != 0 {
		t.Fatalf("합: %+v", Stat{Pending: s.Pending, Retry: s.Retry, Exhausted: s.Exhausted, Stale: s.Stale})
	}
	if !s.Truncated || len(s.Types) != MaxTypes || s.Types[0].Name != "T0007" {
		t.Fatalf("잘림: truncated=%v 종류 %d 첫째 %+v", s.Truncated, len(s.Types), s.Types[0])
	}
	if m, _ := s.Metrics(); !strings.HasSuffix(m, " · 종류 1,000개만") {
		t.Fatalf("잘림 표시: %q", m)
	}
}

func TestIntegration_NULL_종류와_글자_NULL_종류를_가르고_상세는_IS_NULL_로_묻는다(t *testing.T) {
	pg, ctx := testPG(t)
	for _, sql := range []string{
		`drop schema if exists flows_it4 cascade`,
		`create schema flows_it4`,
		`create table flows_it4.t_outbox (id int, event_type text, retry_count int)`,
		`insert into flows_it4.t_outbox values (1, null, 3), (2, null, 0), (3, 'NULL', 4)`,
	} {
		if _, err := pg.Run(ctx, sql); err != nil {
			t.Fatal(err)
		}
	}
	t.Cleanup(func() { _, _ = pg.Run(context.Background(), `drop schema if exists flows_it4 cascade`) })
	o := Outbox{Table: "flows_it4.t_outbox", Type: "event_type", Retry: "retry_count"}
	s, err := Count(ctx, pg, o)
	if err != nil || len(s.Types) != 2 {
		t.Fatalf("종류: %+v %v", s.Types, err)
	}
	var null, text TypeStat
	for _, ts := range s.Types {
		if ts.Null {
			null = ts
		} else {
			text = ts
		}
	}
	if !null.Null || null.Rows != 2 || text.Name != "NULL" || text.Rows != 1 || null.Label() != "(NULL)" || text.Label() != "NULL" {
		t.Fatalf("NULL 가르기: null=%+v text=%+v", null, text)
	}
	for _, c := range []struct {
		ts TypeStat
		id string
	}{{null, "1"}, {text, "3"}} {
		res, err := pg.RunReadOnly(ctx, RetrySQL(o, c.ts))
		if err != nil || len(res.Rows) != 1 || res.Rows[0][0] != c.id {
			t.Fatalf("%+v 상세: %+v %v", c.ts, res, err)
		}
	}
}
