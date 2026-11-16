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
		"topic is empty",
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

func TestCountSQL_식별자를_감싸고_where_를_AND_괄호로_붙인다(t *testing.T) {
	o := Outbox{Table: `public.t"x`, Type: "event_type", Retry: "retry_count", Due: "check_needed_at", Where: "event_type like 'DATA_%' -- 주석"}
	want := `select count(*), count(*) filter (where "retry_count" > 0), count(*) filter (where "check_needed_at" < now() - interval '5 minutes') from "public"."t""x" where (` +
		"\nevent_type like 'DATA_%' -- 주석\n)"
	if got := CountSQL(o); got != want {
		t.Fatalf("got  %s\nwant %s", got, want)
	}
	// retry·due 가 없으면 null 이고 where 도 없다
	if got := CountSQL(Outbox{Table: "t_outbox", Type: "x"}); got != `select count(*), null::bigint, null::bigint from "t_outbox"` {
		t.Fatalf("최소: %s", got)
	}
	if !db.IsReadOnlySQL(CountSQL(o)) || !db.IsReadOnlySQL(TypeSQL(o)) {
		t.Fatal("읽기 문장이어야 한다")
	}
}

func TestTypeSQL_종류별_상위_10개(t *testing.T) {
	got := TypeSQL(Outbox{Table: "t", Type: "event_type", Due: "due"})
	want := `select "event_type" as type, count(*) as count, null::bigint as retrying, min("due") as oldest_due from "t" group by 1 order by 2 desc, 1 limit 10`
	if got != want {
		t.Fatalf("got  %s\nwant %s", got, want)
	}
}

func TestStat_Metrics_는_준_열만_보이고_재시도_오래됨이_있으면_경고다(t *testing.T) {
	cases := []struct {
		s    Stat
		want string
		warn bool
	}{
		{Stat{120, 3, 1}, "대기 120 · 재시도 3 · 오래됨 1", true},
		{Stat{5, 0, 0}, "대기 5 · 재시도 0 · 오래됨 0", false},
		{Stat{5, -1, -1}, "대기 5", false},
		{Stat{5, 0, 2}, "대기 5 · 재시도 0 · 오래됨 2", true},
	}
	for _, c := range cases {
		if got, warn := c.s.Metrics(); got != c.want || warn != c.warn {
			t.Errorf("%+v: %q %v", c.s, got, warn)
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
	if err != nil || s != (Stat{3, 1, 2}) {
		t.Fatalf("수: %+v %v", s, err)
	}
	s, err = Count(ctx, pg, Outbox{Table: o.Table, Type: o.Type})
	if err != nil || s != (Stat{4, -1, -1}) {
		t.Fatalf("열 없는 수: %+v %v", s, err)
	}
	res, err := Types(ctx, pg, o)
	if err != nil || len(res.Rows) != 2 || res.Rows[0][0] != "DATA_A" || res.Rows[0][1] != "2" || res.Rows[0][2] != "1" {
		t.Fatalf("종류별: %+v %v", res, err)
	}
	if strings.Join(res.Cols, ",") != "type,count,retrying,oldest_due" {
		t.Fatalf("열: %v", res.Cols)
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
