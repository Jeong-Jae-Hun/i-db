//! Amazon Athena 읽기 전용 저장소(R60·R68). 트리는 데이터 카탈로그 AwsDataCatalog 의 데이터베이스 → 테이블 → 열이고,
//! 편집기는 읽기 SQL 만 StartQueryExecution 으로 보낸 뒤 끝날 때까지 기다려 결과를 받는다. Go `internal/athena` 다.
//! 목록도 Glue 카탈로그 요청이라 요금이 붙어(R72) 트리·미리 보기·질의 모두 source::billed 를 먼저 본다. 확인(GetWorkGroup)만 요금이 없다.

use std::future::Future;
use std::time::{Duration, Instant};

use aws_sdk_athena::types::{
    QueryExecution, QueryExecutionContext, QueryExecutionState, QueryExecutionStatistics, QueryExecutionStatus, ResultConfiguration,
    ResultReuseByAgeConfiguration, ResultReuseConfiguration, ResultSet, StatementType, TableMetadata, WorkGroup, WorkGroupState,
};
use idb_core::project::Conn;
use idb_core::result::{MAX_ROWS, NULL_TEXT, QueryResult, cap_cell};
use idb_core::sqlsafe::quote_ident;
use idb_core::textutil::human_bytes;

use crate::Error;
use crate::aws::{AwsErr, sdk_err};
use crate::health::{Health, Metric, failed};
use crate::source::{self, BoxFut, Node, Query, sql_words};

const CATALOG: &str = "AwsDataCatalog";
const DEFAULT_WORKGROUP: &str = "primary";
/// 이 안에 같은 질의를 다시 보내면 Athena 가 이전 결과를 돌려주고 다시 스캔하지 않는다(요금 아낌)
const REUSE_MINUTES: i32 = 60;
/// GetQueryResults 한 번의 상한
const PAGE_ROWS: usize = 1000;
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
const FULL_SCAN: &str = "전체 스캔";
const NO_PARTITION: &str = "파티션 조건 없음";

/// StartQueryExecution 에 싣는 값이다. 시험이 그대로 들여다본다.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StartInput {
    pub query: String,
    pub workgroup: String,
    /// 결과 위치(s3://…). None 이면 작업 그룹 설정을 따른다
    pub output: Option<String>,
    /// `-- database:` 로 고른 데이터베이스(카탈로그 AwsDataCatalog)
    pub database: Option<String>,
    /// 결과 재사용(REUSE_MINUTES)을 켠다
    pub reuse: bool,
}

/// Backend 가 부르는 Athena 작업이다. 시험은 가짜로 바꿔 끼운다(네트워크 없음).
/// stop_query_execution 만 읽기가 아니다: 취소한 질의가 서버에서 계속 돌며 스캔 요금을 쌓지 않게 멈춘다(R68.3).
pub trait Api: Send + Sync + 'static {
    fn list_databases(&self, next: Option<String>) -> impl Future<Output = Result<(Vec<String>, Option<String>), AwsErr>> + Send;
    fn list_table_metadata(
        &self,
        db: &str,
        next: Option<String>,
    ) -> impl Future<Output = Result<(Vec<TableMetadata>, Option<String>), AwsErr>> + Send;
    fn get_table_metadata(&self, db: &str, table: &str) -> impl Future<Output = Result<Option<TableMetadata>, AwsErr>> + Send;
    fn start_query_execution(&self, input: &StartInput) -> impl Future<Output = Result<String, AwsErr>> + Send;
    fn get_query_execution(&self, id: &str) -> impl Future<Output = Result<Option<QueryExecution>, AwsErr>> + Send;
    fn get_query_results(
        &self,
        id: &str,
        max: i32,
        next: Option<String>,
    ) -> impl Future<Output = Result<(Option<ResultSet>, Option<String>), AwsErr>> + Send;
    fn stop_query_execution(&self, id: &str) -> impl Future<Output = Result<(), AwsErr>> + Send;
    fn get_work_group(&self, wg: &str) -> impl Future<Output = Result<Option<WorkGroup>, AwsErr>> + Send;
}

pub struct Sdk(aws_sdk_athena::Client);

impl Api for Sdk {
    async fn list_databases(&self, next: Option<String>) -> Result<(Vec<String>, Option<String>), AwsErr> {
        let out = self
            .0
            .list_databases()
            .catalog_name(CATALOG)
            .set_next_token(next)
            .send()
            .await
            .map_err(sdk_err)?;
        let names = out.database_list().iter().map(|d| d.name().to_string()).collect();
        Ok((names, out.next_token))
    }

    async fn list_table_metadata(&self, db: &str, next: Option<String>) -> Result<(Vec<TableMetadata>, Option<String>), AwsErr> {
        let out = self
            .0
            .list_table_metadata()
            .catalog_name(CATALOG)
            .database_name(db)
            .set_next_token(next)
            .send()
            .await
            .map_err(sdk_err)?;
        Ok((out.table_metadata_list.unwrap_or_default(), out.next_token))
    }

    async fn get_table_metadata(&self, db: &str, table: &str) -> Result<Option<TableMetadata>, AwsErr> {
        let out = self
            .0
            .get_table_metadata()
            .catalog_name(CATALOG)
            .database_name(db)
            .table_name(table)
            .send()
            .await
            .map_err(sdk_err)?;
        Ok(out.table_metadata)
    }

    async fn start_query_execution(&self, input: &StartInput) -> Result<String, AwsErr> {
        let mut req = self
            .0
            .start_query_execution()
            .query_string(&input.query)
            .work_group(&input.workgroup);
        if input.reuse {
            req = req.result_reuse_configuration(
                ResultReuseConfiguration::builder()
                    .result_reuse_by_age_configuration(
                        ResultReuseByAgeConfiguration::builder()
                            .enabled(true)
                            .max_age_in_minutes(REUSE_MINUTES)
                            .build(),
                    )
                    .build(),
            );
        }
        if let Some(o) = &input.output {
            req = req.result_configuration(ResultConfiguration::builder().output_location(o).build());
        }
        if let Some(d) = &input.database {
            req = req.query_execution_context(QueryExecutionContext::builder().catalog(CATALOG).database(d).build());
        }
        let out = req.send().await.map_err(sdk_err)?;
        Ok(out.query_execution_id.unwrap_or_default())
    }

    async fn get_query_execution(&self, id: &str) -> Result<Option<QueryExecution>, AwsErr> {
        let out = self.0.get_query_execution().query_execution_id(id).send().await.map_err(sdk_err)?;
        Ok(out.query_execution)
    }

    async fn get_query_results(&self, id: &str, max: i32, next: Option<String>) -> Result<(Option<ResultSet>, Option<String>), AwsErr> {
        let out = self
            .0
            .get_query_results()
            .query_execution_id(id)
            .max_results(max)
            .set_next_token(next)
            .send()
            .await
            .map_err(sdk_err)?;
        Ok((out.result_set, out.next_token))
    }

    async fn stop_query_execution(&self, id: &str) -> Result<(), AwsErr> {
        self.0.stop_query_execution().query_execution_id(id).send().await.map_err(sdk_err)?;
        Ok(())
    }

    async fn get_work_group(&self, wg: &str) -> Result<Option<WorkGroup>, AwsErr> {
        let out = self.0.get_work_group().work_group(wg).send().await.map_err(sdk_err)?;
        Ok(out.work_group)
    }
}

/// Athena 접속 하나다.
pub struct Backend<A: Api = Sdk> {
    cl: A,
    workgroup: String,
    output: String,
    /// 상태 묻기 간격. 처음 짧게 묻고 두 배씩 늘려 max_poll 에서 멈춘다. 시험이 줄인다
    min_poll: Duration,
    max_poll: Duration,
}

impl Backend<Sdk> {
    pub async fn new(c: &Conn) -> Result<Backend<Sdk>, Error> {
        let cfg = crate::aws::sdk_config(c).await;
        Ok(Backend::with_api(Sdk(aws_sdk_athena::Client::new(&cfg)), c))
    }
}

fn wrap(op: &str, e: AwsErr) -> Error {
    Error::msg(format!("athena {op}: {}", e.text()))
}

/// fut 를 deadline 까지 기다린다. 넘으면 Error::Deadline 이다.
async fn at<T>(deadline: tokio::time::Instant, fut: impl Future<Output = T>) -> Result<T, Error> {
    tokio::time::timeout_at(deadline, fut).await.map_err(|_| Error::Deadline)
}

/// 마감이 없을 때 쓰는 먼 시각이다(Go 의 기한 없는 ctx).
fn far() -> tokio::time::Instant {
    tokio::time::Instant::now() + Duration::from_secs(365 * 24 * 3600)
}

/// 테이블 Enter 가 보내는 미리 보기 질의다(R68.1). path 는 "<db>/<테이블>" 이다.
pub fn open_sql(path: &str, limit: usize) -> String {
    let (db, table) = path.split_once('/').unwrap_or((path, ""));
    format!("SELECT * FROM {}.{} LIMIT {limit}", quote_ident(db), quote_ident(table))
}

impl<A: Api> Backend<A> {
    pub fn with_api(cl: A, c: &Conn) -> Backend<A> {
        Backend {
            cl,
            workgroup: if c.workgroup.is_empty() {
                DEFAULT_WORKGROUP.into()
            } else {
                c.workgroup.clone()
            },
            output: c.output.clone(),
            min_poll: Duration::from_millis(200),
            max_poll: Duration::from_secs(2),
        }
    }

    /// "" 이면 데이터베이스, "<db>" 면 테이블(info 는 테이블 종류), "<db>/<테이블>" 이면 열이다.
    pub async fn children(&self, path: &str) -> Result<Vec<Node>, Error> {
        source::billed()?;
        if path.is_empty() {
            let mut out = Vec::new();
            let mut next = None;
            loop {
                let (names, token) = self.cl.list_databases(next).await.map_err(|e| wrap("ListDatabases", e))?;
                out.extend(names.into_iter().map(|n| Node {
                    name: n.clone(),
                    path: n,
                    has_children: true,
                    ..Default::default()
                }));
                match token {
                    Some(t) => next = Some(t),
                    None => return Ok(out),
                }
            }
        }
        let Some((db, table)) = path.split_once('/') else {
            let mut out = Vec::new();
            let mut next = None;
            loop {
                let (tables, token) = self
                    .cl
                    .list_table_metadata(path, next)
                    .await
                    .map_err(|e| wrap("ListTableMetadata", e))?;
                out.extend(tables.iter().map(|t| Node {
                    name: t.name().to_string(),
                    path: format!("{path}/{}", t.name()),
                    info: t.table_type().unwrap_or_default().to_string(),
                    has_children: true,
                    openable: true,
                    ..Default::default()
                }));
                match token {
                    Some(t) => next = Some(t),
                    None => return Ok(out),
                }
            }
        };
        // 열 줄은 잎이다
        if table.contains('/') {
            return Ok(Vec::new());
        }
        let md = self.table(db, table).await?;
        Ok(column_nodes(path, &md))
    }

    async fn table(&self, db: &str, table: &str) -> Result<TableMetadata, Error> {
        self.cl
            .get_table_metadata(db, table)
            .await
            .map_err(|e| wrap("GetTableMetadata", e))?
            .ok_or_else(|| Error::msg(format!("athena: table {db}.{table} has no metadata")))
    }

    /// 테이블 미리 보기다. 파티션 테이블은 파티션 조건 없이 여러 파티션을 열 수 있어 확인을 받는다(R68.1).
    pub async fn open(&self, path: &str, limit: usize) -> Result<QueryResult, Error> {
        source::billed()?;
        let Some((db, table)) = path.split_once('/').filter(|(_, t)| !t.contains('/')) else {
            return Err(Error::msg(format!("athena: not a table: {path}")));
        };
        let md = self.table(db, table).await?;
        if !md.partition_keys().is_empty() {
            return Err(Error::NeedsConfirm(NO_PARTITION.into()));
        }
        self.run(&Query {
            text: open_sql(path, limit),
            limit,
            force: true,
            ..Default::default()
        })
        .await
    }

    /// 읽기 SQL 한 문장만 보낸다(R68.2). 끝날 때까지 상태를 묻고 결과를 q.limit(0 이면 1,000)행까지 받는다.
    /// 마감이 지나면 StopQueryExecution 으로 서버의 질의를 멈춘다. 결과 text 는 스캔 크기·엔진 시간이다.
    pub async fn run(&self, q: &Query) -> Result<QueryResult, Error> {
        source::billed()?;
        let (db, text) = database_directive(&q.text);
        let (stmt, words) = read_only(text)?;
        let reason = expensive(&words);
        if !reason.is_empty() && !q.force {
            return Err(Error::NeedsConfirm(reason.into()));
        }
        let limit = if q.limit == 0 { MAX_ROWS } else { q.limit };
        let deadline = q.deadline.unwrap_or_else(far);
        let start = Instant::now();
        let id = self.start(&stmt, db, deadline).await?;
        let qe = self.wait(&id, deadline).await?;
        let st = qe.statement_type().cloned();
        let mut res = self.results(&id, st, limit, deadline).await?;
        res.text = stats_note(qe.statistics());
        res.elapsed = start.elapsed();
        Ok(res)
    }

    async fn start(&self, stmt: &str, db: &str, deadline: tokio::time::Instant) -> Result<String, Error> {
        let mut input = StartInput {
            query: stmt.into(),
            workgroup: self.workgroup.clone(),
            output: (!self.output.is_empty()).then(|| self.output.clone()),
            database: (!db.is_empty()).then(|| db.to_string()),
            reuse: true,
        };
        let mut r = at(deadline, self.cl.start_query_execution(&input)).await?;
        if let Err(e) = &r
            && e.code == "InvalidRequestException"
            && e.message.to_lowercase().contains("reuse")
        {
            // 결과 재사용은 엔진 v3 작업 그룹만 받는다. 거절하면 재사용 없이 한 번 더 보낸다
            input.reuse = false;
            r = at(deadline, self.cl.start_query_execution(&input)).await?;
        }
        r.map_err(|e| wrap("StartQueryExecution", e))
    }

    /// 질의가 끝날 때까지 GetQueryExecution 으로 묻는다. 간격은 min_poll 에서 두 배씩 max_poll 까지 늘린다.
    async fn wait(&self, id: &str, deadline: tokio::time::Instant) -> Result<QueryExecution, Error> {
        let mut delay = self.min_poll;
        loop {
            let qe = match at(deadline, self.cl.get_query_execution(id)).await {
                Err(e) => {
                    self.stop(id).await;
                    return Err(e);
                }
                Ok(Err(e)) => return Err(wrap("GetQueryExecution", e)),
                Ok(Ok(qe)) => qe,
            };
            if let Some(qe) = qe
                && let Some(st) = qe.status()
            {
                match st.state() {
                    Some(QueryExecutionState::Succeeded) => return Ok(qe),
                    Some(QueryExecutionState::Failed) => return Err(Error::msg(format!("athena: query failed: {}", fail_reason(st)))),
                    Some(QueryExecutionState::Cancelled) => return Err(Error::msg("athena: query cancelled")),
                    _ => {}
                }
            }
            if at(deadline, tokio::time::sleep(delay)).await.is_err() {
                self.stop(id).await;
                return Err(Error::Deadline);
            }
            delay = (delay * 2).min(self.max_poll);
        }
    }

    /// 지난 마감 대신 새 기한으로 StopQueryExecution 을 보낸다. 실패해도 취소 결과는 그대로라 오류를 버린다.
    async fn stop(&self, id: &str) {
        let _ = tokio::time::timeout(STOP_TIMEOUT, self.cl.stop_query_execution(id)).await;
    }

    /// GetQueryResults 를 이어 limit+1 행(잘림 가르기)까지 받는다. SELECT(DML) 결과는 첫 페이지 첫 줄이
    /// 열 이름 머리라 뺀다. SHOW·DESCRIBE(DDL) 결과에는 머리 줄이 없다.
    async fn results(
        &self,
        id: &str,
        st: Option<StatementType>,
        limit: usize,
        deadline: tokio::time::Instant,
    ) -> Result<QueryResult, Error> {
        let mut res = QueryResult {
            types: Some(Vec::new()),
            ..Default::default()
        };
        let mut next = None;
        let mut first = true;
        loop {
            let mut want = limit as i64 - res.rows.len() as i64 + 1;
            if first {
                want += 1; // 머리 줄
            }
            let max = want.clamp(1, PAGE_ROWS as i64) as i32;
            let (rs, token) = at(deadline, self.cl.get_query_results(id, max, next.take()))
                .await?
                .map_err(|e| wrap("GetQueryResults", e))?;
            let mut rows: &[aws_sdk_athena::types::Row] = &[];
            if let Some(rs) = &rs {
                rows = rs.rows();
                if first {
                    if let Some(md) = rs.result_set_metadata() {
                        for ci in md.column_info() {
                            res.cols.push(ci.name().to_string());
                            res.types.get_or_insert_default().push(ci.r#type().to_string());
                        }
                    }
                    if st == Some(StatementType::Dml) && !rows.is_empty() && row_values(&rows[0]) == res.cols {
                        rows = &rows[1..];
                    }
                    if res.cols.is_empty() && !rows.is_empty() {
                        res.cols = (1..=rows[0].data().len()).map(|i| format!("col{i}")).collect();
                    }
                }
            }
            for r in rows {
                let row = (0..res.cols.len())
                    .map(|i| match r.data().get(i).and_then(|d| d.var_char_value()) {
                        Some(v) => cap_cell(v.as_bytes()),
                        None => NULL_TEXT.into(),
                    })
                    .collect();
                res.rows.push(row);
            }
            if res.rows.len() > limit {
                res.rows.truncate(limit);
                res.truncated = true;
                return Ok(res);
            }
            match token {
                Some(t) => next = Some(t),
                None => return Ok(res),
            }
            first = false;
        }
    }

    /// 대시보드 확인이다(R64.2): GetWorkGroup 한 번으로 작업 그룹 상태(`ENABLED`)와 질의당 스캔 상한을 보인다.
    /// 버전 열은 작업 그룹의 엔진 판이다. ENABLED 가 아니면 경고 색이다.
    pub async fn probe(&self) -> Health {
        let start = Instant::now();
        let r = self.cl.get_work_group(&self.workgroup).await;
        let latency = start.elapsed();
        let wg = match r {
            Err(e) => return failed(latency, &wrap("GetWorkGroup", e).to_string(), ""),
            Ok(wg) => wg,
        };
        let mut h = Health {
            ok: true,
            latency,
            ..Default::default()
        };
        let Some(wg) = wg else { return h };
        let state = wg.state().map(|s| s.as_str()).unwrap_or("");
        h.metrics.push(Metric::new("", state, wg.state() != Some(&WorkGroupState::Enabled)));
        if let Some(cfg) = wg.configuration() {
            if let Some(cut) = cfg.bytes_scanned_cutoff_per_query() {
                h.metrics.push(Metric::new("스캔 상한", human_bytes(cut), false));
            }
            if let Some(ev) = cfg.engine_version() {
                h.version = ev.effective_engine_version().unwrap_or_default().to_string();
            }
        }
        h
    }
}

impl<A: Api> source::Backend for Backend<A> {
    fn children<'a>(&'a self, path: &'a str) -> BoxFut<'a, Result<Vec<Node>, Error>> {
        Box::pin(Backend::children(self, path))
    }

    fn run<'a>(&'a self, q: &'a Query) -> BoxFut<'a, Result<QueryResult, Error>> {
        Box::pin(Backend::run(self, q))
    }

    fn open<'a>(&'a self, path: &'a str, limit: usize) -> BoxFut<'a, Result<QueryResult, Error>> {
        Box::pin(Backend::open(self, path, limit))
    }

    fn probe(&self) -> Option<BoxFut<'_, Health>> {
        Some(Box::pin(Backend::probe(self)))
    }
}

/// 열(`이름 타입`) 다음에 파티션 키(`타입 (파티션)`)를 보인다.
fn column_nodes(path: &str, md: &TableMetadata) -> Vec<Node> {
    let col = |c: &aws_sdk_athena::types::Column, suffix: &str| Node {
        name: c.name().to_string(),
        path: format!("{path}/{}", c.name()),
        info: format!("{}{suffix}", c.r#type().unwrap_or_default()),
        ..Default::default()
    };
    md.columns()
        .iter()
        .map(|c| col(c, ""))
        .chain(md.partition_keys().iter().map(|c| col(c, " (파티션)")))
        .collect()
}

fn fail_reason(s: &QueryExecutionStatus) -> String {
    if let Some(m) = s.athena_error().and_then(|e| e.error_message()).filter(|m| !m.is_empty()) {
        return m.to_string();
    }
    match s.state_change_reason() {
        Some(r) if !r.is_empty() => r.to_string(),
        _ => "unknown reason".into(),
    }
}

fn row_values(r: &aws_sdk_athena::types::Row) -> Vec<String> {
    r.data()
        .iter()
        .map(|d| d.var_char_value().unwrap_or_default().to_string())
        .collect()
}

/// 결과 위 한 줄이다. 예: `스캔 1.2GB · 엔진 0.6초`, 이전 결과를 다시 쓰면 ` · 이전 결과 재사용`.
fn stats_note(s: Option<&QueryExecutionStatistics>) -> String {
    let Some(s) = s else { return String::new() };
    let mut note = format!(
        "스캔 {} · 엔진 {:.1}초",
        human_bytes(s.data_scanned_in_bytes().unwrap_or(0)),
        s.engine_execution_time_in_millis().unwrap_or(0) as f64 / 1000.0
    );
    if s.result_reuse_information().is_some_and(|r| r.reused_previous_result()) {
        note.push_str(" · 이전 결과 재사용");
    }
    note
}

// ─── 읽기 판정 (R68.2) ───

/// 보낼 수 있는 첫 낱말이다. CTAS·INSERT·UNLOAD·MSCK 같은 쓰기는 첫 낱말에서 걸린다.
const READ_STARTS: [&str; 5] = ["select", "with", "show", "describe", "explain"];

/// SELECT·WITH·EXPLAIN 문장 어디에(하위 질의 포함) 있어도 거절하는 낱말이다. `EXPLAIN ANALYZE INSERT …` 는 실제로 쓰므로 막는다.
/// SHOW·DESCRIBE 는 쓰는 꼴이 없어 보지 않는다(`SHOW CREATE TABLE` 을 받으려고).
const WRITE_WORDS: [&str; 18] = [
    "insert",
    "update",
    "delete",
    "merge",
    "create",
    "drop",
    "alter",
    "truncate",
    "msck",
    "unload",
    "vacuum",
    "optimize",
    "grant",
    "revoke",
    "call",
    "prepare",
    "execute",
    "deallocate",
];

/// text 가 읽기 문장 하나인지 보고 보낼 글과 낱말을 돌려준다. 아니면 Error::Write 다.
fn read_only(text: &str) -> Result<(String, Vec<String>), Error> {
    let (stmt, words) = sql_words(text)?;
    let first = words.first().map(String::as_str).unwrap_or("");
    if !READ_STARTS.contains(&first) {
        return Err(Error::Write(format!("{first} (only SELECT, WITH, SHOW, DESCRIBE, EXPLAIN)")));
    }
    if first != "show"
        && first != "describe"
        && let Some(w) = words.iter().find(|w| WRITE_WORDS.contains(&w.as_str()))
    {
        return Err(Error::Write(w.clone()));
    }
    Ok((stmt, words))
}

/// 확인받을 까닭이다(R60.2). 표를 읽는 문장(SELECT·WITH·EXPLAIN ANALYZE)에 LIMIT 도 WHERE 도 없으면 전체 스캔으로 본다.
/// 문장 어디든 LIMIT·WHERE 낱말이 있으면 넘긴다. 나머지는 작업 그룹의 스캔 상한(BytesScannedCutoffPerQuery)이 막는다.
fn expensive(words: &[String]) -> &'static str {
    let scans = words[0] == "select" || words[0] == "with" || (words[0] == "explain" && words.get(1).is_some_and(|w| w == "analyze"));
    if scans && !words.iter().any(|w| w == "limit" || w == "where") {
        FULL_SCAN
    } else {
        ""
    }
}

fn is_re_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\x0c' | '\r')
}

/// 첫 글줄의 `-- database: <이름>` 을 읽어 질의 맥락 데이터베이스로 쓰고 그 줄을 뗀 글을 돌려준다(Go 정규식
/// `(?i)^--\s*database:\s*(\S+)\s*$`). 없으면 "" 이다. 줄을 떼는 까닭: Athena 는 앞머리 주석이 있으면 DDL 을 읽지 못하는 일이 있다.
fn database_directive(text: &str) -> (&str, &str) {
    let trimmed = text.trim_start_matches([' ', '\t', '\r', '\n']);
    let (line, rest) = trimmed.split_once('\n').unwrap_or((trimmed, ""));
    let line = line.trim_end_matches('\r');
    let parse = || -> Option<&str> {
        let r = line.strip_prefix("--")?.trim_start_matches(is_re_space);
        let head = r.get(..9)?;
        if !head.eq_ignore_ascii_case("database:") {
            return None;
        }
        let r = r[9..].trim_start_matches(is_re_space);
        let end = r.find(is_re_space).unwrap_or(r.len());
        let (name, tail) = r.split_at(end);
        (!name.is_empty() && tail.chars().all(is_re_space)).then_some(name)
    };
    match parse() {
        Some(name) => (name.trim_matches(['"', '`']), rest),
        None => ("", text),
    }
}

#[cfg(test)]
mod tests;
