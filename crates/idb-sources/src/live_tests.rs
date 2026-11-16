//! 실제 서버 시험. DBTUI_TEST_PG_URL=postgres://user:pass@host:port/db·DBTUI_TEST_REDIS_ADDR=host:port 가 있을 때만 돈다.
//! Go `internal/db` 의 backend·readonly·meta·linked·export(SearchData)·schemadiff 시험 가운데 CLI 가 쓰는 쪽을 옮겼다.
//! 같은 서버에서 Go 시험과 함께 돌아도 겹치지 않게 스키마·키 이름에 rs_ 를 붙인다.

use std::time::{Duration, Instant};

use idb_core::project::{Conn, Kind};
use idb_core::result::MAX_ROWS;

use crate::linked::{LINKED_CAP, LinkedColumn};
use crate::pg::{Pg, TableRef};
use crate::redis::RedisConn;
use crate::schemadiff::diff_snapshots;
use crate::{Clients, Error};

fn pg_conn() -> Option<Conn> {
    let raw = std::env::var("DBTUI_TEST_PG_URL").ok().filter(|s| !s.is_empty())?;
    let rest = raw.strip_prefix("postgres://")?;
    let (cred, hostdb) = rest.split_once('@')?;
    let (user, pass) = cred.split_once(':').unwrap_or((cred, ""));
    let (hp, db) = hostdb.split_once('/').unwrap_or((hostdb, ""));
    let (host, port) = hp.rsplit_once(':')?;
    Some(Conn {
        name: "it-local".into(),
        uuid: "it-pg".into(),
        kind: Some(Kind::Pg),
        host: host.into(),
        port: port.parse().ok()?,
        db: db.into(),
        user: user.into(),
        password: pass.into(),
        ..Default::default()
    })
}

fn redis_conn() -> Option<Conn> {
    let addr = std::env::var("DBTUI_TEST_REDIS_ADDR").ok().filter(|s| !s.is_empty())?;
    let (host, port) = addr.rsplit_once(':')?;
    Some(Conn {
        name: "it-redis".into(),
        uuid: "it-redis".into(),
        kind: Some(Kind::Redis),
        host: host.into(),
        port: port.parse().ok()?,
        db: "0".into(),
        ..Default::default()
    })
}

macro_rules! need {
    ($e:expr) => {
        match $e {
            Some(v) => v,
            None => {
                eprintln!("DBTUI_TEST_PG_URL / DBTUI_TEST_REDIS_ADDR not set");
                return;
            }
        }
    };
}

async fn pg() -> Option<Pg> {
    let c = pg_conn()?;
    Some(Clients::new().pg(&c).await.expect("connect"))
}

/// 여러 문장을 simple protocol 로 한 번에 실행한다(시험 준비용).
async fn exec(pg: &Pg, sql: &str) {
    let mut p = pg.acquire().await.unwrap();
    p.conn().exec(sql).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    p.release();
}

async fn run(pg: &Pg, sql: &str) -> Result<idb_core::result::QueryResult, Error> {
    pg.run_args(sql, &[]).await
}

// ─── 실행 ───

#[tokio::test]
async fn PG_스키마_테이블_쿼리() {
    let pg = need!(pg().await);
    exec(
        &pg,
        "drop schema if exists rs_it cascade; create schema rs_it; create table rs_it.t (id int, name text)",
    )
    .await;
    let res = run(
        &pg,
        "insert into rs_it.t select g, case when g % 2 = 0 then null else 'n' || g end from generate_series(1, 1500) g",
    )
    .await
    .unwrap();
    assert!(res.text == "INSERT 0 1500" && res.cols.is_empty(), "{res:?}");

    let schemas = pg.find_schemas().await.unwrap();
    assert!(
        schemas.contains(&"rs_it".into()) && !schemas.contains(&"pg_catalog".into()) && !schemas.contains(&"information_schema".into())
    );
    assert_eq!(pg.find_tables("rs_it").await.unwrap(), ["t"]);

    let res = run(&pg, "select id, name from rs_it.t order by id").await.unwrap();
    assert!(res.rows.len() == MAX_ROWS && res.truncated && res.cols[1] == "name");
    assert_eq!(
        (res.rows[0][0].as_str(), res.rows[0][1].as_str(), res.rows[1][1].as_str()),
        ("1", "n1", "NULL")
    );
    // 잘라낸 뒤에도 같은 풀로 다음 쿼리가 된다
    assert_eq!(run(&pg, "select count(*) from rs_it.t").await.unwrap().rows[0][0], "1500");
    let err = run(&pg, "select * from rs_it.nope").await.unwrap_err();
    assert_eq!(err.to_string(), r#"ERROR: relation "rs_it.nope" does not exist (SQLSTATE 42P01)"#);
    exec(&pg, "drop schema rs_it cascade").await;
}

#[tokio::test]
async fn PG_비밀번호가_틀리면_오류에_비밀번호를_싣지_않는다() {
    let mut c = need!(pg_conn());
    c.password = "wrong-secret-xyz".into();
    let err = Clients::new().pg(&c).await.err().expect("wrong password must fail").to_string();
    assert!(
        !err.contains("wrong-secret-xyz") && err.starts_with("failed to connect to `user="),
        "{err}"
    );
}

#[tokio::test]
async fn PG_큰_결과는_1000행에서_끊고_남은_행을_기다리지_않는다() {
    let pg = need!(pg().await);
    let start = Instant::now();
    let res = run(&pg, "select g, repeat('x', 200) from generate_series(1, 20000000) g")
        .await
        .unwrap();
    assert!(res.truncated && start.elapsed() < Duration::from_secs(3), "{:?}", start.elapsed());
    run(&pg, "select 1").await.unwrap();
}

#[tokio::test]
async fn PG_결과는_칸_글자_예산을_넘으면_거기서_자른다() {
    let pg = need!(pg().await);
    let mut p = pg.acquire().await.unwrap();
    p.conn().set_result_budget(100);
    let rows = p
        .conn()
        .query_cells("select repeat('x', 40) from generate_series(1, 10)", &[], usize::MAX)
        .await
        .unwrap();
    assert!(rows.rows.len() == 2 && rows.truncated, "{} {}", rows.rows.len(), rows.truncated);
    // 남은 행은 끝까지 읽어 버려 연결을 이어 쓴다
    assert_eq!(p.conn().query_cells("select 1", &[], 10).await.unwrap().rows.len(), 1);
    p.release();
}

#[tokio::test]
async fn PG_여러_문장은_실행하지_않는다() {
    let pg = need!(pg().await);
    exec(&pg, "create table if not exists rs_multi(a int)").await;
    // 커서 경로(select)와 일반 경로(update) 둘 다 막아야 한다
    for q in ["select 1; drop table rs_multi", "update rs_multi set a = 1; drop table rs_multi"] {
        assert!(run(&pg, q).await.is_err(), "{q}");
        assert!(pg.run_read_only_rows(q, 10).await.is_err(), "{q}");
    }
    let res = run(&pg, "select count(*) from pg_tables where tablename = 'rs_multi'")
        .await
        .unwrap();
    assert_eq!(res.rows[0][0], "1", "뒤 문장(drop)이 실행됐다");
}

#[tokio::test]
async fn PG_RETURNING_과_WITH_쓰기는_1000행을_넘어도_끝까지_반영한다() {
    let pg = need!(pg().await);
    exec(&pg, "drop table if exists rs_ret; create table rs_ret(a int)").await;
    let res = run(&pg, "insert into rs_ret select generate_series(1, 1500) returning a")
        .await
        .unwrap();
    assert!(res.truncated && res.rows.len() == MAX_ROWS);
    run(
        &pg,
        "with src as (select generate_series(1, 1500) a) insert into rs_ret select a from src returning a",
    )
    .await
    .unwrap();
    assert_eq!(run(&pg, "select count(*) from rs_ret").await.unwrap().rows[0][0], "3000");
    exec(&pg, "drop table rs_ret").await;
}

#[tokio::test]
async fn PG_열_개수가_다른_select를_이어_실행해도_열_정보가_맞다() {
    let pg = need!(pg().await);
    for q in ["select 1 as a", "select 'x' as b, 2 as c", "select 1 as d, 2 as e, 3 as f"] {
        let res = run(&pg, q).await.unwrap();
        assert!(res.rows.len() == 1 && res.cols.len() == res.rows[0].len(), "{q}");
    }
    assert_eq!(run(&pg, "select 'x' as b, 2 as c").await.unwrap().cols, ["b", "c"]);
}

#[tokio::test]
async fn PG_결과에_열_타입_이름을_싣고_모르는_타입은_OID로_싣는다() {
    let pg = need!(pg().await);
    exec(&pg, "drop type if exists rs_mood; create type rs_mood as enum ('ok')").await;
    let oid = run(&pg, "select 'rs_mood'::regtype::oid::text").await.unwrap().rows[0][0].clone();
    // select 는 커서 경로, show 는 끝까지 받는 경로다
    let res = pg
        .run_read_only_rows(
            "select 1::int4 as a, 'x'::text as b, true as c, now() as d, '{}'::jsonb as e, 'ok'::rs_mood as f, '1:00'::timetz as g",
            10,
        )
        .await
        .unwrap();
    let want = format!("int4,text,bool,timestamptz,jsonb,{oid},1266");
    assert_eq!(res.types.unwrap().join(","), want);
    let res = pg.run_read_only_rows("show server_version", 10).await.unwrap();
    assert_eq!(res.types.unwrap(), ["text"]);
    exec(&pg, "drop type rs_mood").await;
}

// ─── 읽기 전용 ───

#[tokio::test]
async fn RunReadOnly_읽기는_돌고_쓰기_여러문장은_서버가_거부한다() {
    let pg = need!(pg().await);
    exec(
        &pg,
        "drop table if exists rs_ro_t; create table rs_ro_t (id int); insert into rs_ro_t select generate_series(1, 1500)",
    )
    .await;
    let res = pg
        .run_read_only_rows("select id from rs_ro_t order by id;", MAX_ROWS)
        .await
        .unwrap();
    assert!(res.rows.len() == MAX_ROWS && res.truncated && res.rows[0][0] == "1");
    let res = pg.run_read_only_rows("show statement_timeout", MAX_ROWS).await.unwrap();
    assert_eq!(res.rows[0][0], "30s", "SET LOCAL 시간 제한이 걸려야 한다");
    pg.run_read_only_rows("explain select * from rs_ro_t", MAX_ROWS).await.unwrap();
    for sql in [
        "delete from rs_ro_t",
        "with d as (delete from rs_ro_t returning id) select * from d",
        "select 1; delete from rs_ro_t",
        "show timezone; delete from rs_ro_t",
        "select * into rs_ro_copy from rs_ro_t",
    ] {
        assert!(pg.run_read_only_rows(sql, MAX_ROWS).await.is_err(), "{sql}");
    }
    assert_eq!(
        pg.run_read_only_rows("select count(*) from rs_ro_t", 10).await.unwrap().rows[0][0],
        "1500"
    );
    let err = pg.run_read_only_rows("-- 주석만", 10).await.unwrap_err();
    assert!(err.to_string().contains("empty"), "{err}");
    exec(&pg, "drop table rs_ro_t").await;
}

#[tokio::test]
async fn RunReadOnly_읽기_전용이_아닌_문장은_접속_전에_거부한다() {
    // 닿지 않는 주소의 풀이라 접속을 시도하면 접속 오류가 난다. 거부는 그 전에 해야 한다
    let pg = Pg::unopened("127.0.0.1", 1);
    for sql in [
        "copy (select 'x') to program 'touch /tmp/dbtui_pwn'",
        "commit",
        "set transaction read write",
        "call some_proc()",
        "do $$ begin perform 1; end $$",
    ] {
        let err = pg.run_read_only_rows(sql, 10).await.unwrap_err();
        assert!(err.to_string().starts_with("only read-only statements are allowed"), "{sql}: {err}");
    }
}

#[tokio::test]
async fn RunReadOnly_붙여_쓴_select도_커서로_행수를_자른다() {
    let pg = need!(pg().await);
    let res = pg
        .run_read_only_rows("select*from generate_series(1, 3000)", MAX_ROWS)
        .await
        .unwrap();
    assert!(res.rows.len() == MAX_ROWS && res.truncated);
}

#[tokio::test]
async fn RunReadOnly_세션_상태를_남기지_않는다() {
    let pg = need!(pg().await);
    // 이름 차단을 비켜 가는 사용자 함수로 잠근다. 차단은 최선일 뿐이라 연결 폐기가 따로 막아야 한다
    exec(
        &pg,
        "create or replace function rs_ro_lock() returns boolean language sql as 'select pg_try_advisory_lock(424243)'",
    )
    .await;
    assert_eq!(pg.run_read_only_rows("select rs_ro_lock()", 10).await.unwrap().rows[0][0], "t");
    let res = pg.run_read_only_rows("show standard_conforming_strings", 10).await.unwrap();
    assert_eq!(res.rows[0][0], "on");
    // 연결을 닫은 뒤 백엔드가 끝나며 잠금을 푸는 것은 비동기라 잠깐 기다린다
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let res = run(&pg, "select count(*) from pg_locks where locktype = 'advisory' and objid = 424243")
            .await
            .unwrap();
        if res.rows[0][0] == "0" {
            break;
        }
        assert!(Instant::now() < deadline, "읽기 전용 실행이 잡은 advisory lock 이 남아 있다");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ─── 구조 ───

const META_FIXTURE: &str = r#"
drop schema if exists rs_ddl_src cascade;
drop schema if exists rs_ddl_dst cascade;
create schema rs_ddl_src;
create table rs_ddl_src.parent (id int primary key);
create table rs_ddl_src."Child Tbl" (
	id bigint generated always as identity primary key,
	parent_id int not null references rs_ddl_src.parent(id) on delete cascade,
	code text not null default 'x''y',
	qty int check (qty > 0),
	total numeric generated always as (qty * 2) stored,
	"Weird ""Col""" varchar(20),
	unique (parent_id, code)
);
create index child_code_idx on rs_ddl_src."Child Tbl" (lower(code)) where qty > 1;
comment on table rs_ddl_src."Child Tbl" is 'child''s table';
comment on column rs_ddl_src."Child Tbl".code is 'c\d';
create view rs_ddl_src.v as select id, code from rs_ddl_src."Child Tbl" where qty > 0;
comment on view rs_ddl_src.v is '뷰';
create table rs_ddl_src.events (id int not null, at date not null, note text, primary key (id, at)) partition by range (at);
create index events_note_idx on rs_ddl_src.events (note);
create table rs_ddl_src.events_2026 partition of rs_ddl_src.events (constraint note_len check (length(note) < 100))
	for values from ('2026-01-01') to ('2027-01-01') partition by list (id);
create table rs_ddl_src.events_2026_1 partition of rs_ddl_src.events_2026 for values in (1);
create index events_2026_at_idx on rs_ddl_src.events_2026_1 (at);
create table rs_ddl_src.events_def partition of rs_ddl_src.events default;
comment on table rs_ddl_src.events_def is '나머지';
"#;

fn tref(schema: &str, table: &str) -> TableRef {
    TableRef {
        schema: schema.into(),
        table: table.into(),
    }
}

/// 두 시험이 같은 스키마를 쓰므로 하나로 묶는다(cargo 는 한 바이너리의 시험을 동시에 돌린다).
#[tokio::test]
async fn 구조_보기와_DDL_은_다른_스키마에서_같은_테이블과_뷰를_만든다() {
    let pg = need!(pg().await);
    exec(&pg, META_FIXTURE).await;

    let res = pg.describe_table(&tref("rs_ddl_src", "Child Tbl")).await.unwrap();
    let want: Vec<Vec<String>> = [
        ["id", "bigint", "NO", "GENERATED ALWAYS AS IDENTITY", ""],
        ["parent_id", "integer", "NO", "", ""],
        ["code", "text", "NO", "DEFAULT 'x''y'::text", r"c\d"],
        ["qty", "integer", "YES", "", ""],
        ["total", "numeric", "YES", "GENERATED ALWAYS AS ((qty * 2)) STORED", ""],
        [r#"Weird "Col""#, "character varying(20)", "YES", "", ""],
    ]
    .iter()
    .map(|r| r.iter().map(|s| s.to_string()).collect())
    .collect();
    assert_eq!(res.rows, want);
    for s in [
        "comment: child's table",
        "PRIMARY KEY (id)",
        "FOREIGN KEY (parent_id) REFERENCES rs_ddl_src.parent(id) ON DELETE CASCADE",
        "UNIQUE (parent_id, code)",
        "CHECK ((qty > 0))",
        "child_code_idx  CREATE INDEX child_code_idx",
    ] {
        assert!(res.text.contains(s), "{s}:\n{}", res.text);
    }
    let err = pg.describe_table(&tref("rs_ddl_src", "nope")).await.unwrap_err();
    assert_eq!(err.to_string(), r#"relation "rs_ddl_src"."nope" not found"#);

    exec(&pg, "create schema rs_ddl_dst").await;
    // 부모 → 자식 → 뷰 순서로 만들어야 FK·뷰가 참조할 대상이 있다
    let refs = ["parent", "Child Tbl", "v", "events", "events_2026", "events_2026_1", "events_def"];
    let mut src = std::collections::HashMap::new();
    for name in refs {
        let ddl = pg.table_ddl(&tref("rs_ddl_src", name)).await.unwrap();
        exec(&pg, &ddl.replace("rs_ddl_src", "rs_ddl_dst")).await;
        src.insert(name, ddl);
    }
    for s in [
        "GENERATED ALWAYS AS IDENTITY",
        "GENERATED ALWAYS AS ((qty * 2)) STORED",
        r#""Weird ""Col""" character varying(20)"#,
        r#"COMMENT ON COLUMN "rs_ddl_src"."Child Tbl"."code" IS E'c\\d';"#,
        "CREATE INDEX child_code_idx",
        r#"CONSTRAINT "Child Tbl_pkey" PRIMARY KEY (id)"#,
    ] {
        assert!(src["Child Tbl"].contains(s), "{s}:\n{}", src["Child Tbl"]);
    }
    assert_eq!(
        src["events_2026"],
        "CREATE TABLE \"rs_ddl_src\".\"events_2026\" PARTITION OF \"rs_ddl_src\".\"events\" (\n    CONSTRAINT \"note_len\" CHECK ((length(note) < 100))\n) FOR VALUES FROM ('2026-01-01') TO ('2027-01-01') PARTITION BY LIST (id);\n"
    );
    assert_eq!(
        src["events_2026_1"],
        "CREATE TABLE \"rs_ddl_src\".\"events_2026_1\" PARTITION OF \"rs_ddl_src\".\"events_2026\" FOR VALUES IN (1);\nCREATE INDEX events_2026_at_idx ON rs_ddl_src.events_2026_1 USING btree (at);\n"
    );
    assert_eq!(
        src["events_def"],
        "CREATE TABLE \"rs_ddl_src\".\"events_def\" PARTITION OF \"rs_ddl_src\".\"events\" DEFAULT;\nCOMMENT ON TABLE \"rs_ddl_src\".\"events_def\" IS '나머지';\n"
    );
    assert!(
        src["v"].starts_with("CREATE VIEW \"rs_ddl_src\".\"v\" AS") && src["v"].contains("COMMENT ON VIEW \"rs_ddl_src\".\"v\" IS '뷰';")
    );
    for name in refs {
        let dst = pg.table_ddl(&tref("rs_ddl_dst", name)).await.unwrap();
        assert_eq!(dst, src[name].replace("rs_ddl_src", "rs_ddl_dst"), "{name}");
    }
    exec(&pg, "drop schema rs_ddl_src cascade; drop schema rs_ddl_dst cascade").await;
}

// ─── 스키마 비교 ───

const SCHEMA_DIFF_FIXTURE: &str = "drop schema if exists rs_snap_a cascade; drop schema if exists rs_snap_b cascade;
create schema rs_snap_a; create schema rs_snap_b;
create table rs_snap_a.same (id serial primary key, name text not null default 'n');
create table rs_snap_b.same (id serial primary key, name text not null default 'n');
create table rs_snap_a.child (same_id int references rs_snap_a.same (id) check (same_id > 0));
create table rs_snap_b.child (same_id int references rs_snap_b.same (id) check (same_id > 0));
create table rs_snap_a.t (id int primary key, a int, b text, c int);
create index t_a_idx on rs_snap_a.t (a);
create index t_b_idx on rs_snap_a.t (b);
create table rs_snap_b.t (id int primary key, a bigint, c int not null, d text);
create index t_b_idx on rs_snap_b.t (d);
create table rs_snap_a.only_a (x int);
create table rs_snap_b.only_b (y int);
create view rs_snap_a.v as select id from rs_snap_a.t;
create view rs_snap_b.v as select id, d from rs_snap_b.t;";

/// same·child 는 시퀀스 기본값·FK·PK 인덱스에 스키마 이름이 들어가도 같은 정의로 보여야 한다.
#[tokio::test]
async fn 스키마_비교는_다른_점만_정해진_순서로_보이고_없는_스키마는_비어_있다() {
    let pg = need!(pg().await);
    exec(&pg, SCHEMA_DIFF_FIXTURE).await;
    let a = pg.schema_snapshot("rs_snap_a").await.unwrap();
    let b = pg.schema_snapshot("rs_snap_b").await.unwrap();
    assert!(diff_snapshots(&a, &a).rows.is_empty());
    let got = diff_snapshots(&a, &b);
    assert_eq!(got.cols, ["구분", "대상", "왼쪽", "오른쪽"]);
    let want = [
        ["−", "테이블 only_a", "TABLE", ""],
        ["+", "테이블 only_b", "", "TABLE"],
        ["~", "열 t.a", "integer", "bigint"],
        ["−", "열 t.b", "text", ""],
        ["~", "열 t.c", "integer", "integer NOT NULL"],
        ["+", "열 t.d", "", "text"],
        ["−", "인덱스 t.t_a_idx", "CREATE INDEX t_a_idx ON t USING btree (a)", ""],
        [
            "~",
            "인덱스 t.t_b_idx",
            "CREATE INDEX t_b_idx ON t USING btree (b)",
            "CREATE INDEX t_b_idx ON t USING btree (d)",
        ],
        [
            "~",
            "테이블 v",
            "VIEW AS  SELECT id\n   FROM t;",
            "VIEW AS  SELECT id,\n    d\n   FROM t;",
        ],
        ["+", "열 v.d", "", "text"],
    ];
    assert_eq!(got.rows, want.map(|r| r.map(String::from).to_vec()).to_vec());
    assert!(pg.schema_snapshot("rs_snap_none").await.unwrap().tables.is_empty());
    exec(&pg, "drop schema rs_snap_a cascade; drop schema rs_snap_b cascade").await;
}

// ─── 검색 ───

const SEARCH_FIXTURE: &str = r#"drop schema if exists rs_search cascade; create schema rs_search;
create table rs_search.users (id int primary key, name text, email varchar(100), code char(6), meta jsonb, uid uuid, age int);
insert into rs_search.users values
	(1, 'alice', 'alice@x.io', 'A100', '{"tag": "needle"}', '00000000-0000-0000-0000-000000000001', 30),
	(2, 'needle bob', 'bob@x.io', '100%', null, null, 40),
	(3, 'carol', 'NEEDLE@x.io', '100x', null, null, 50);
create table rs_search.multi (a int, b text, note text, primary key (a, b));
insert into rs_search.multi values (1, 'k', 'needle');
create table rs_search.nopk (note text);
insert into rs_search.nopk select 'needle ' || g from generate_series(1, 9) g;
create table rs_search.nums (n int);
create table rs_search.zlocked (note text);
insert into rs_search.zlocked values ('needle');
create view rs_search.v_users as select * from rs_search.users;"#;

/// 세 시험이 같은 스키마를 쓰므로 하나로 묶는다.
#[tokio::test]
async fn 검색은_글자_열을_찾고_와일드카드는_글자로_보며_시간이_넘은_테이블은_건너뛴다() {
    let pg = need!(pg().await);
    exec(&pg, SEARCH_FIXTURE).await;
    let (res, skipped) = pg.search_data("rs_search", "needle").await.unwrap();
    assert!(res.cols == ["테이블", "열", "PK", "값"] && skipped.is_empty());
    let count = |t: &str| res.rows.iter().filter(|r| r[0] == t).count();
    // nopk 는 5행 상한
    assert_eq!(
        (count("users"), count("multi"), count("nopk"), count("zlocked")),
        (3, 1, 5, 1),
        "{:?}",
        res.rows
    );
    for want in [
        ["users", "meta", "id=1", r#"{"tag": "needle"}"#],
        ["users", "name", "id=2", "needle bob"],
        ["users", "email", "id=3", "NEEDLE@x.io"],
        ["multi", "note", "a=1, b=k", "needle"],
        ["nopk", "note", "", "needle 1"],
    ] {
        assert!(res.rows.iter().any(|r| r == &want), "{want:?}: {:?}", res.rows);
    }

    for (text, want) in [
        ("100%", "100%"),
        ("10_x", ""),
        ("-000000000001", "00000000-0000-0000-0000-000000000001"),
    ] {
        let (res, _) = pg.search_data("rs_search", text).await.unwrap();
        let vals: Vec<&str> = res.rows.iter().map(|r| r[3].as_str()).collect();
        if want.is_empty() {
            assert!(vals.is_empty(), "{text}: {vals:?}");
        } else {
            assert_eq!(vals, [want], "{text}");
        }
    }

    // 다른 세션이 잠근 테이블은 statement_timeout(5초)에 건너뛴다
    let locker = Clients::new().pg(&need!(pg_conn())).await.unwrap();
    let mut held = locker.acquire().await.unwrap();
    held.conn()
        .exec("begin; lock table rs_search.zlocked in access exclusive mode")
        .await
        .unwrap();
    let start = Instant::now();
    let (res, skipped) = pg.search_data("rs_search", "needle").await.unwrap();
    assert!(
        skipped == ["zlocked"] && res.rows.len() == 9 && start.elapsed() < Duration::from_secs(15),
        "{skipped:?} {} {:?}",
        res.rows.len(),
        start.elapsed()
    );
    held.conn().exec("rollback").await.unwrap();
    held.release();
    exec(&pg, "drop schema rs_search cascade").await;
}

// ─── 연결된 곳 ───

async fn linked_fixture(pg: &Pg) {
    exec(
        pg,
        "drop schema if exists rs_lnk cascade;
        create schema rs_lnk;
        create table rs_lnk.lnk_members (id bigint primary key);
        create table rs_lnk.lnk_orders (id serial primary key, lnk_member_id bigint);
        create index on rs_lnk.lnk_orders (lnk_member_id);
        create table rs_lnk.lnk_msgs (id serial primary key, sender_lnk_member_id int4, target_lnk_member_id bigint);
        create index on rs_lnk.lnk_msgs (sender_lnk_member_id);
        create table rs_lnk.lnk_small (lnk_member_id int2);
        create index on rs_lnk.lnk_small (lnk_member_id);
        create table rs_lnk.lnk_multi (a int, lnk_member_id bigint);
        create index on rs_lnk.lnk_multi (a, lnk_member_id);
        create table rs_lnk.lnk_tags (lnk_member_id uuid);
        create index on rs_lnk.lnk_tags (lnk_member_id);
        create view rs_lnk.lnk_view as select lnk_member_id from rs_lnk.lnk_orders;
        create table rs_lnk.lnk_part (lnk_member_id bigint);
        create index on rs_lnk.lnk_part (lnk_member_id) where lnk_member_id > 0;
        create table rs_lnk.lnk_inval (lnk_member_id bigint);
        create index lnk_inval_idx on rs_lnk.lnk_inval (lnk_member_id);
        update pg_index set indisvalid = false where indexrelid = 'rs_lnk.lnk_inval_idx'::regclass;
        create table rs_lnk.lnk_expr (lnk_member_id bigint);
        create index on rs_lnk.lnk_expr ((lnk_member_id + 0));
        insert into rs_lnk.lnk_members values (1315), (1316);
        insert into rs_lnk.lnk_orders (lnk_member_id) select 1315 from generate_series(1, 150);
        insert into rs_lnk.lnk_msgs (sender_lnk_member_id, target_lnk_member_id) values (1315, 1315), (1316, 1315);",
    )
    .await;
}

/// 다른 시험이 만든 테이블을 빼고 rs_lnk 스키마 열만 남긴다.
fn lnk_only(cols: Vec<LinkedColumn>) -> Vec<LinkedColumn> {
    cols.into_iter().filter(|c| c.table.schema == "rs_lnk").collect()
}

/// 세 시험이 같은 스키마를 쓰므로 하나로 묶는다.
#[tokio::test]
async fn 연결된_곳은_이름_규칙_타입_인덱스로_후보를_고르고_인덱스_열만_101행까지_센다() {
    let pg = need!(pg().await);
    linked_fixture(&pg).await;
    let (cols, truncated) = pg.linked_columns("lnk_member_id", "lnk_member", "1315").await.unwrap();
    let cols = lnk_only(cols);
    let got: std::collections::HashMap<String, (bool, bool)> = cols
        .iter()
        .map(|c| (format!("{}.{}", c.table.table, c.column), (c.owner, c.indexed)))
        .collect();
    let want = std::collections::HashMap::from([
        ("lnk_members.id".to_string(), (true, true)),
        ("lnk_orders.lnk_member_id".to_string(), (false, true)),
        ("lnk_msgs.sender_lnk_member_id".to_string(), (false, true)),
        ("lnk_msgs.target_lnk_member_id".to_string(), (false, false)),
        ("lnk_small.lnk_member_id".to_string(), (false, true)),
        // 인덱스 둘째 열이라 세지 않는다
        ("lnk_multi.lnk_member_id".to_string(), (false, false)),
        ("lnk_part.lnk_member_id".to_string(), (false, false)),
        ("lnk_inval.lnk_member_id".to_string(), (false, false)),
        ("lnk_expr.lnk_member_id".to_string(), (false, false)),
    ]);
    assert!(!truncated);
    assert_eq!(got, want);
    assert_eq!(cols[0].table.table, "lnk_members", "주인 테이블이 먼저");

    // int2 를 넘는 값은 int2 열을 빼고, uuid 열은 정수 값에서 늘 빠진다
    let (cols70k, _) = pg.linked_columns("lnk_member_id", "lnk_member", "70000").await.unwrap();
    assert!(
        lnk_only(cols70k)
            .iter()
            .all(|c| c.table.table != "lnk_small" && c.table.table != "lnk_tags")
    );
    // id 열은 같은 이름 규칙을 쓰지 않아 다른 테이블의 id 가 들지 않는다
    let (ids, _) = pg.linked_columns("id", "lnk_member", "1315").await.unwrap();
    assert!(lnk_only(ids).iter().all(|c| c.column != "id" || c.owner));

    // 인덱스가 없는 열은 묻지 않는다. 없는 테이블을 가리켜도 오류가 나지 않아야 한다
    let mut cols = cols;
    cols.push(LinkedColumn {
        table: tref("rs_lnk", "no_such_table"),
        column: "x".into(),
        typ: String::new(),
        owner: false,
        indexed: false,
    });
    let (counts, errs) = pg.count_linked(&cols, "1315", 3).await;
    let mut got = std::collections::HashMap::new();
    for (i, c) in cols.iter().enumerate() {
        assert!(errs[i].is_none(), "{:?}", errs[i]);
        got.insert(format!("{}.{}", c.table.table, c.column), counts[i]);
    }
    for (k, w) in [
        ("lnk_members.id", 1),
        ("lnk_orders.lnk_member_id", LINKED_CAP + 1),
        ("lnk_msgs.sender_lnk_member_id", 1),
        ("lnk_msgs.target_lnk_member_id", -1),
        ("lnk_small.lnk_member_id", 0),
        ("lnk_multi.lnk_member_id", -1),
        ("no_such_table.x", -1),
        ("lnk_part.lnk_member_id", -1),
        ("lnk_inval.lnk_member_id", -1),
        ("lnk_expr.lnk_member_id", -1),
    ] {
        assert_eq!(got[k], w, "{k}");
    }

    // 한 테이블은 READ ONLY 트랜잭션에서 2초 제한으로 돈다
    let n = pg
        .count_one("select extract(epoch from current_setting('statement_timeout')::interval)::int")
        .await
        .unwrap();
    assert_eq!(n, 2);
    let start = Instant::now();
    let err = pg.count_one("select count(*) from (select pg_sleep(5)) s").await.unwrap_err();
    assert!(err.is_statement_timeout() && start.elapsed() < Duration::from_secs(4), "{err}");
    let err = pg
        .count_one("with x as (insert into rs_lnk.lnk_members values (9) returning 1) select count(*) from x")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("read-only"), "{err}");
    exec(&pg, "drop schema rs_lnk cascade").await;
}

// ─── Redis ───

async fn redis_run(r: &mut RedisConn, line: &str) -> idb_core::result::QueryResult {
    r.run(&idb_core::sqlsafe::split_args(line)).await.unwrap()
}

#[tokio::test]
async fn Redis_명령은_값을_표와_글로_보인다() {
    let c = need!(redis_conn());
    let mut r = RedisConn::open(&c).await.unwrap();
    redis_run(&mut r, "DEL rs:s rs:l rs:h").await;
    redis_run(&mut r, r#"SET rs:s "{\"a\":1}""#).await;
    // JSON 글은 들여 쓴다
    assert_eq!(redis_run(&mut r, "GET rs:s").await.text, "{\n  \"a\": 1\n}");
    redis_run(&mut r, "RPUSH rs:l x y").await;
    let res = redis_run(&mut r, "LRANGE rs:l 0 -1").await;
    assert_eq!(res.rows, [["0", "x"], ["1", "y"]]);
    redis_run(&mut r, "HSET rs:h f2 v2 f1 v1").await;
    // RESP3 맵은 키 순서로 줄을 세운다
    assert_eq!(redis_run(&mut r, "HGETALL rs:h").await.rows, [["f1", "v1"], ["f2", "v2"]]);
    assert_eq!(redis_run(&mut r, "GET rs:nope").await.text, "(nil)");
    assert_eq!(redis_run(&mut r, "EXISTS rs:s").await.text, "1");
    let err = r.run(&["NOPE_CMD".to_string()]).await.unwrap_err().to_string();
    assert!(err.starts_with("ERR unknown command"), "{err}");
    let err = r
        .run(&["LRANGE".to_string(), "rs:s".into(), "0".into(), "1".into()])
        .await
        .unwrap_err()
        .to_string();
    assert!(err.starts_with("WRONGTYPE"), "{err}");
    redis_run(&mut r, "DEL rs:s rs:l rs:h").await;
}

#[tokio::test]
async fn Redis_평문이_안_되면_TLS_까지_시도하고_둘_다_실패를_알린다() {
    // 아무도 듣지 않는 포트: 평문·TLS 모두 실패해야 한다
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let c = Conn {
        name: "x".into(),
        kind: Some(Kind::Redis),
        host: "127.0.0.1".into(),
        port,
        ..Default::default()
    };
    let err = RedisConn::open(&c).await.err().unwrap().to_string();
    assert!(
        err.contains("plain:") && err.contains("tls:") && err.contains("connection refused"),
        "{err}"
    );
}

/// 접속은 받지만 아무 말도 하지 않는 가짜 서버다. 받은 바이트를 연결마다 보낸다.
async fn silent_server() -> (u16, tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>) {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            let tx = tx.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut buf = vec![0u8; 4096];
                let n = tokio::time::timeout(Duration::from_secs(2), s.read(&mut buf))
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .unwrap_or(0);
                let _ = tx.send(buf[..n].to_vec());
                tokio::time::sleep(Duration::from_secs(5)).await;
            });
        }
    });
    (port, rx)
}

#[tokio::test]
async fn Redis_응답_없는_서버는_평문_3초_TLS_3초_안에_포기하고_평문에는_비밀번호를_보내지_않는다() {
    let (port, mut got) = silent_server().await;
    let c = Conn {
        name: "silent".into(),
        kind: Some(Kind::Redis),
        host: "127.0.0.1".into(),
        port,
        password: "s3cret-xyz".into(),
        ..Default::default()
    };
    let start = Instant::now();
    assert!(RedisConn::open(&c).await.is_err());
    assert!(start.elapsed() < Duration::from_secs(8), "{:?}", start.elapsed());
    // 평문 시도와 TLS 시도, 두 연결이 받은 내용을 본다
    for _ in 0..2 {
        let b = tokio::time::timeout(Duration::from_secs(3), got.recv()).await.unwrap().unwrap();
        assert!(!String::from_utf8_lossy(&b).contains("s3cret-xyz"), "비밀번호가 평문으로 나갔다");
    }
}
