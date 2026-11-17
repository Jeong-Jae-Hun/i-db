use std::collections::HashMap;
use std::sync::Mutex;

use aws_sdk_athena::types::{AthenaError, Column, ColumnInfo, Datum, EngineVersion, ResultSetMetadata, Row, WorkGroupConfiguration};

use super::*;
use crate::source::set_billed_for_test;

// ─── 가짜 Athena ───

#[derive(Default)]
struct Fake {
    dbs: Vec<Vec<String>>,
    tables: HashMap<String, Vec<TableMetadata>>,
    meta: HashMap<String, TableMetadata>,
    /// StartQueryExecution 호출마다 차례로 돌려줄 오류
    start_err: Vec<Option<AwsErr>>,
    /// GetQueryExecution 이 차례로 돌려줄 상태, 다 쓰면 마지막을 되풀이
    states: Vec<QueryExecutionState>,
    status: Option<QueryExecutionStatus>,
    stmt_type: Option<StatementType>,
    stats: Option<QueryExecutionStatistics>,
    pages: Vec<ResultSet>,
    wg: Option<WorkGroup>,
    wg_err: Option<AwsErr>,
    log: Mutex<Log>,
}

#[derive(Default)]
struct Log {
    started: Vec<StartInput>,
    get_calls: usize,
    asked: Vec<i32>,
    stopped: Vec<String>,
}

impl Api for Fake {
    async fn list_databases(&self, next: Option<String>) -> Result<(Vec<String>, Option<String>), AwsErr> {
        let i: usize = next.map_or(0, |n| n.parse().unwrap());
        Ok((self.dbs[i].clone(), (i + 1 < self.dbs.len()).then(|| (i + 1).to_string())))
    }

    async fn list_table_metadata(&self, db: &str, _next: Option<String>) -> Result<(Vec<TableMetadata>, Option<String>), AwsErr> {
        Ok((self.tables.get(db).cloned().unwrap_or_default(), None))
    }

    async fn get_table_metadata(&self, db: &str, table: &str) -> Result<Option<TableMetadata>, AwsErr> {
        match self.meta.get(&format!("{db}.{table}")) {
            Some(m) => Ok(Some(m.clone())),
            None => Err(AwsErr::api("MetadataException", "not found")),
        }
    }

    async fn start_query_execution(&self, input: &StartInput) -> Result<String, AwsErr> {
        let mut log = self.log.lock().unwrap();
        log.started.push(input.clone());
        if let Some(Some(e)) = self.start_err.get(log.started.len() - 1) {
            return Err(e.clone());
        }
        Ok("q-1".into())
    }

    async fn get_query_execution(&self, id: &str) -> Result<Option<QueryExecution>, AwsErr> {
        let n = {
            let mut log = self.log.lock().unwrap();
            log.get_calls += 1;
            log.get_calls
        };
        let state = self.states[n.min(self.states.len()) - 1].clone();
        let mut st = QueryExecutionStatus::builder().state(state);
        if let Some(s) = &self.status {
            st = st
                .set_athena_error(s.athena_error().cloned())
                .set_state_change_reason(s.state_change_reason().map(String::from));
        }
        Ok(Some(
            QueryExecution::builder()
                .query_execution_id(id)
                .status(st.build())
                .set_statement_type(self.stmt_type.clone())
                .set_statistics(self.stats.clone())
                .build(),
        ))
    }

    async fn get_query_results(&self, _id: &str, max: i32, _next: Option<String>) -> Result<(Option<ResultSet>, Option<String>), AwsErr> {
        let mut log = self.log.lock().unwrap();
        log.asked.push(max);
        let i = log.asked.len() - 1;
        Ok((Some(self.pages[i].clone()), (i + 1 < self.pages.len()).then(|| "next".to_string())))
    }

    async fn stop_query_execution(&self, id: &str) -> Result<(), AwsErr> {
        self.log.lock().unwrap().stopped.push(id.into());
        Ok(())
    }

    async fn get_work_group(&self, _wg: &str) -> Result<Option<WorkGroup>, AwsErr> {
        match &self.wg_err {
            Some(e) => Err(e.clone()),
            None => Ok(self.wg.clone()),
        }
    }
}

fn backend(f: Fake, c: Conn) -> Backend<Fake> {
    let mut b = Backend::with_api(f, &c);
    b.min_poll = Duration::from_millis(1);
    b.max_poll = Duration::from_millis(2);
    b
}

fn row(vals: &[&str]) -> Row {
    let mut r = Row::builder();
    for v in vals {
        r = r.data(if *v == "<nil>" {
            Datum::builder().build()
        } else {
            Datum::builder().var_char_value(*v).build()
        });
    }
    r.build()
}

fn meta(cols: &[&str]) -> ResultSetMetadata {
    let mut m = ResultSetMetadata::builder();
    for c in cols {
        let (name, typ) = c.split_once(' ').unwrap_or((c, ""));
        m = m.column_info(ColumnInfo::builder().name(name).r#type(typ).build().unwrap());
    }
    m.build()
}

fn page(md: Option<&[&str]>, rows: &[&[&str]]) -> ResultSet {
    let mut rs = ResultSet::builder().set_rows(Some(rows.iter().map(|r| row(r)).collect()));
    if let Some(cols) = md {
        rs = rs.result_set_metadata(meta(cols));
    }
    rs.build()
}

fn succeeded() -> Fake {
    Fake {
        states: vec![QueryExecutionState::Succeeded],
        stmt_type: Some(StatementType::Dml),
        ..Default::default()
    }
}

fn q(text: &str) -> Query {
    Query {
        text: text.into(),
        ..Default::default()
    }
}

fn conn() -> Conn {
    Conn::default()
}

// ─── 읽기 판정 ───

#[test]
fn 읽기_문장만_받고_쓰기와_여러_문장은_Write() {
    let ok = [
        r#"SELECT * FROM "db"."t" LIMIT 10"#,
        "select 'delete from t' as s, \"update\" from t where a = 1;",
        "-- 설명\nWITH x AS (SELECT 1 a) SELECT a FROM x LIMIT 1",
        "SHOW CREATE TABLE db.t",
        "SHOW TABLES IN db",
        "DESCRIBE db.t",
        "EXPLAIN SELECT 1",
        "/* 앞 */ select 1 limit 1 ; ",
    ];
    for text in ok {
        assert!(read_only(text).is_ok(), "{text}");
    }
    let bad = [
        "INSERT INTO t SELECT * FROM s",
        "CREATE TABLE t2 AS SELECT * FROM t",
        "UNLOAD (SELECT * FROM t) TO 's3://b/p/' WITH (format = 'PARQUET')",
        "DROP TABLE t",
        "ALTER TABLE t ADD PARTITION (dt='2026-01-01')",
        "MSCK REPAIR TABLE t",
        "WITH x AS (SELECT 1) INSERT INTO t SELECT * FROM x",
        "EXPLAIN ANALYZE INSERT INTO t VALUES (1)",
        "SELECT * FROM (DELETE FROM t)",
        "SELECT 1; SELECT 2",
        "SELECT 1; DROP TABLE t",
        "VACUUM t",
        "",
    ];
    for text in bad {
        assert!(matches!(read_only(text), Err(Error::Write(_))), "{text}");
    }
}

#[tokio::test]
async fn 쓰기_문장은_Athena_를_부르지_않는다() {
    set_billed_for_test(true);
    let b = backend(succeeded(), conn());
    for text in ["DROP TABLE t", "CREATE TABLE x AS SELECT 1", "SELECT 1; SELECT 2"] {
        let mut query = q(text);
        query.force = true;
        assert!(matches!(b.run(&query).await, Err(Error::Write(_))), "{text}");
    }
    assert!(b.cl.log.lock().unwrap().started.is_empty());
}

#[tokio::test]
async fn LIMIT_도_WHERE_도_없는_SELECT_는_전체_스캔_확인을_받는다() {
    set_billed_for_test(true);
    let mut f = succeeded();
    f.pages = vec![page(Some(&["a"]), &[&["a"]])];
    let b = backend(f, conn());
    assert_eq!(
        b.run(&q(r#"SELECT * FROM "db"."t""#)).await.unwrap_err(),
        Error::NeedsConfirm(FULL_SCAN.into())
    );
    assert!(b.cl.log.lock().unwrap().started.is_empty());
    let words = |text: &str| read_only(text).unwrap().1;
    for text in [
        "SELECT * FROM t LIMIT 5",
        "SELECT * FROM t WHERE dt = '2026-01-01'",
        "SHOW TABLES",
        "EXPLAIN SELECT * FROM t",
    ] {
        assert_eq!(expensive(&words(text)), "", "{text}");
    }
    assert_eq!(expensive(&words("EXPLAIN ANALYZE SELECT * FROM t")), FULL_SCAN);
    let mut forced = q(r#"SELECT * FROM "db"."t""#);
    forced.force = true;
    b.run(&forced).await.unwrap();
    assert_eq!(b.cl.log.lock().unwrap().started.len(), 1);
}

// ─── 실행 ───

#[tokio::test]
async fn 작업_그룹_결과_위치_데이터베이스_주석_재사용을_실어_보낸다() {
    set_billed_for_test(true);
    let mut f = succeeded();
    f.pages = vec![page(Some(&["a"]), &[])];
    let b = backend(
        f,
        Conn {
            output: "s3://bucket/prefix/".into(),
            ..Default::default()
        },
    );
    b.run(&q("-- database: sales\nSELECT 1 LIMIT 1")).await.unwrap();
    assert_eq!(
        b.cl.log.lock().unwrap().started[0],
        StartInput {
            query: "SELECT 1 LIMIT 1".into(),
            workgroup: "primary".into(),
            output: Some("s3://bucket/prefix/".into()),
            database: Some("sales".into()),
            reuse: true,
        }
    );
    let mut f2 = succeeded();
    f2.pages = vec![page(Some(&["a"]), &[])];
    let b2 = backend(
        f2,
        Conn {
            workgroup: "analysts".into(),
            ..Default::default()
        },
    );
    b2.run(&q("SELECT 1 LIMIT 1")).await.unwrap();
    let got = b2.cl.log.lock().unwrap().started[0].clone();
    assert_eq!((got.workgroup.as_str(), got.output, got.database), ("analysts", None, None));
}

#[tokio::test]
async fn 결과_재사용을_거절하면_재사용_없이_한_번_더_보낸다() {
    set_billed_for_test(true);
    let mut f = succeeded();
    f.pages = vec![page(Some(&["a"]), &[])];
    f.start_err = vec![Some(AwsErr::api(
        "InvalidRequestException",
        "Result reuse is not supported for engine version 2",
    ))];
    let b = backend(f, conn());
    b.run(&q("SELECT 1 LIMIT 1")).await.unwrap();
    let started = b.cl.log.lock().unwrap().started.clone();
    assert_eq!(started.len(), 2);
    assert!(!started[1].reuse);

    let mut f2 = succeeded();
    f2.start_err = vec![Some(AwsErr::api("InvalidRequestException", "No output location provided."))];
    let b2 = backend(f2, conn());
    let err = b2.run(&q("SELECT 1 LIMIT 1")).await.unwrap_err();
    assert_eq!(
        err.to_string(),
        "athena StartQueryExecution: InvalidRequestException: No output location provided."
    );
    assert_eq!(b2.cl.log.lock().unwrap().started.len(), 1);
}

#[tokio::test]
async fn SELECT_결과는_첫_줄_머리를_빼고_SHOW_결과는_그대로_둔다() {
    set_billed_for_test(true);
    let mut f = succeeded();
    f.stats = Some(
        QueryExecutionStatistics::builder()
            .data_scanned_in_bytes(1288490189)
            .engine_execution_time_in_millis(612)
            .build(),
    );
    f.pages = vec![page(
        Some(&["id bigint", "name varchar"]),
        &[&["id", "name"], &["1", "가"], &["2", "<nil>"]],
    )];
    let res = backend(f, conn()).run(&q("SELECT id, name FROM t LIMIT 2")).await.unwrap();
    assert_eq!(res.cols, ["id", "name"]);
    assert_eq!(res.types.unwrap(), ["bigint", "varchar"]);
    assert_eq!(res.rows, [["1", "가"], ["2", "NULL"]]);
    assert!(!res.truncated);
    assert_eq!(res.text, "스캔 1.2GB · 엔진 0.6초");

    let ddl = Fake {
        states: vec![QueryExecutionState::Succeeded],
        stmt_type: Some(StatementType::Ddl),
        pages: vec![page(Some(&["tab_name string"]), &[&["tab_name"], &["orders"]])],
        ..Default::default()
    };
    let res = backend(ddl, conn()).run(&q("SHOW TABLES")).await.unwrap();
    assert_eq!(res.rows, [["tab_name"], ["orders"]]);
}

#[tokio::test]
async fn 페이지를_이어_받되_한도에서_자르고_더_묻지_않는다() {
    set_billed_for_test(true);
    let mut f = succeeded();
    f.pages = vec![
        page(Some(&["n"]), &[&["n"], &["1"], &["2"]]),
        page(None, &[&["3"], &["4"]]),
        page(None, &[&["5"]]),
    ];
    let b = backend(f, conn());
    let mut query = q("SELECT n FROM t LIMIT 100");
    query.limit = 3;
    let res = b.run(&query).await.unwrap();
    assert_eq!(res.rows, [["1"], ["2"], ["3"]]);
    assert!(res.truncated);
    assert_eq!(b.cl.log.lock().unwrap().asked, [5, 2]);

    let mut big = succeeded();
    big.pages = vec![page(Some(&["n"]), &[])];
    let b = backend(big, conn());
    let mut query = q("SELECT 1 LIMIT 1");
    query.limit = 5000;
    b.run(&query).await.unwrap();
    assert_eq!(b.cl.log.lock().unwrap().asked[0], PAGE_ROWS as i32);
}

#[tokio::test]
async fn 실패한_질의는_Athena_까닭을_보인다() {
    set_billed_for_test(true);
    let f = Fake {
        states: vec![
            QueryExecutionState::Queued,
            QueryExecutionState::Running,
            QueryExecutionState::Failed,
        ],
        status: Some(
            QueryExecutionStatus::builder()
                .athena_error(
                    AthenaError::builder()
                        .error_message("TABLE_NOT_FOUND: line 1:15: Table 'x' does not exist")
                        .build(),
                )
                .build(),
        ),
        ..Default::default()
    };
    let b = backend(f, conn());
    let err = b.run(&q("SELECT * FROM x LIMIT 1")).await.unwrap_err();
    assert_eq!(
        err.to_string(),
        "athena: query failed: TABLE_NOT_FOUND: line 1:15: Table 'x' does not exist"
    );
    assert_eq!(b.cl.log.lock().unwrap().get_calls, 3);
}

#[tokio::test(start_paused = true)]
async fn 마감이_지나면_StopQueryExecution_으로_서버_질의를_멈춘다() {
    set_billed_for_test(true);
    let f = Fake {
        states: vec![QueryExecutionState::Running],
        ..Default::default()
    };
    let b = backend(f, conn());
    let mut query = q("SELECT * FROM t LIMIT 1");
    query.deadline = Some(tokio::time::Instant::now() + Duration::from_millis(3));
    assert_eq!(b.run(&query).await.unwrap_err(), Error::Deadline);
    let log = b.cl.log.lock().unwrap();
    assert_eq!(log.stopped, ["q-1"]);
    assert!(log.asked.is_empty(), "취소한 뒤 결과를 묻지 않아야 한다");
}

// ─── 트리·미리 보기 ───

async fn child_names(b: &Backend<Fake>, path: &str) -> Vec<String> {
    b.children(path)
        .await
        .unwrap()
        .iter()
        .map(|n| format!("{}|{}|{}", n.name, n.path, n.info))
        .collect()
}

fn col(name: &str, typ: &str) -> Column {
    Column::builder().name(name).r#type(typ).build().unwrap()
}

#[tokio::test]
async fn 데이터베이스_테이블_열과_파티션_키를_보인다() {
    set_billed_for_test(true);
    let tm = |n: &str, t: &str| TableMetadata::builder().name(n).table_type(t).build().unwrap();
    let f = Fake {
        dbs: vec![vec!["logs".into()], vec!["sales".into()]],
        tables: HashMap::from([("sales".into(), vec![tm("orders", "EXTERNAL_TABLE"), tm("v_daily", "VIRTUAL_VIEW")])]),
        meta: HashMap::from([(
            "sales.orders".into(),
            TableMetadata::builder()
                .name("orders")
                .columns(col("id", "bigint"))
                .partition_keys(col("dt", "string"))
                .build()
                .unwrap(),
        )]),
        ..Default::default()
    };
    let b = backend(f, conn());
    assert_eq!(child_names(&b, "").await, ["logs|logs|", "sales|sales|"]);
    assert_eq!(
        child_names(&b, "sales").await,
        ["orders|sales/orders|EXTERNAL_TABLE", "v_daily|sales/v_daily|VIRTUAL_VIEW"]
    );
    assert_eq!(
        child_names(&b, "sales/orders").await,
        ["id|sales/orders/id|bigint", "dt|sales/orders/dt|string (파티션)"]
    );
    assert!(child_names(&b, "sales/orders/id").await.is_empty());
}

#[tokio::test]
async fn 파티션_테이블은_확인을_받고_아니면_LIMIT_50_으로_읽는다() {
    set_billed_for_test(true);
    let mut f = succeeded();
    f.meta = HashMap::from([
        (
            "sales.orders".into(),
            TableMetadata::builder()
                .name("orders")
                .partition_keys(col("dt", ""))
                .build()
                .unwrap(),
        ),
        ("sales.users".into(), TableMetadata::builder().name("users").build().unwrap()),
    ]);
    f.pages = vec![page(Some(&["id"]), &[&["id"], &["7"]])];
    let b = backend(f, conn());
    assert_eq!(
        b.open("sales/orders", 50).await.unwrap_err(),
        Error::NeedsConfirm(NO_PARTITION.into())
    );
    assert!(b.cl.log.lock().unwrap().started.is_empty());
    let res = b.open("sales/users", 50).await.unwrap();
    assert_eq!(res.rows.len(), 1);
    assert_eq!(
        b.cl.log.lock().unwrap().started[0].query,
        r#"SELECT * FROM "sales"."users" LIMIT 50"#
    );
    assert_eq!(open_sql(r#"we"ird/t"#, 50), r#"SELECT * FROM "we""ird"."t" LIMIT 50"#);
}

#[test]
fn 첫_줄_주석만_데이터베이스로_읽는다() {
    let cases = [
        ("-- database: sales\nSELECT 1", "sales", "SELECT 1"),
        ("\n  --DATABASE: \"logs\"\r\nSHOW TABLES", "logs", "SHOW TABLES"),
        ("SELECT 1\n-- database: sales", "", "SELECT 1\n-- database: sales"),
        ("-- 그냥 주석\nSELECT 1", "", "-- 그냥 주석\nSELECT 1"),
    ];
    for (input, db, rest) in cases {
        assert_eq!(database_directive(input), (db, rest), "{input}");
    }
}

// ─── 대시보드 ───

#[tokio::test]
async fn 확인은_작업_그룹_상태와_스캔_상한_엔진_판을_보인다() {
    let f = Fake {
        wg: Some(
            WorkGroup::builder()
                .name("analysts")
                .state(WorkGroupState::Enabled)
                .configuration(
                    WorkGroupConfiguration::builder()
                        .bytes_scanned_cutoff_per_query(10 << 30)
                        .engine_version(EngineVersion::builder().effective_engine_version("Athena engine version 3").build())
                        .build(),
                )
                .build()
                .unwrap(),
        ),
        ..Default::default()
    };
    let h = backend(f, conn()).probe().await;
    assert!(h.ok);
    assert_eq!(
        h.metrics,
        [Metric::new("", "ENABLED", false), Metric::new("스캔 상한", "10.0GB", false)]
    );
    assert_eq!(h.version, "Athena engine version 3");

    let off = Fake {
        wg: Some(WorkGroup::builder().name("w").state(WorkGroupState::Disabled).build().unwrap()),
        ..Default::default()
    };
    assert_eq!(backend(off, conn()).probe().await.metrics, [Metric::new("", "DISABLED", true)]);

    let bad = Fake {
        wg_err: Some(AwsErr::api("AccessDeniedException", "not authorized")),
        ..Default::default()
    };
    let h = backend(bad, conn()).probe().await;
    assert!(!h.ok);
    assert_eq!(h.err, "athena GetWorkGroup: AccessDeniedException: not authorized");
}

#[tokio::test]
async fn 요금이_막혀_있으면_트리_미리_보기_질의를_보내지_않고_작업_그룹_확인만_된다() {
    set_billed_for_test(false);
    let f = Fake {
        wg: Some(WorkGroup::builder().name("w").state(WorkGroupState::Enabled).build().unwrap()),
        ..Default::default()
    };
    let b = backend(f, conn());
    for p in ["", "sales", "sales/orders"] {
        assert_eq!(b.children(p).await.unwrap_err(), Error::Billed, "{p}");
    }
    assert_eq!(b.open("sales/orders", 50).await.unwrap_err(), Error::Billed);
    let mut query = q("SELECT 1 LIMIT 1");
    query.force = true;
    assert_eq!(b.run(&query).await.unwrap_err(), Error::Billed);
    assert!(b.cl.log.lock().unwrap().started.is_empty());
    let h = b.probe().await;
    assert!(h.ok);
    assert_eq!(h.metrics, [Metric::new("", "ENABLED", false)]);
}
