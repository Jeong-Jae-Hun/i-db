//! 이 기계의 프로젝트·접속으로 요청을 처리한다. serve 는 이것을 표준 입출력에 잇는다.
//! 안전장치(읽기 전용·prod 확인·잠금 R54·위험 문장 R25·가리기)는 모두 여기서 정한다. 원격 클라이언트는 확인 여부만 보낸다.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use idb_core::params::inline_args;
use idb_core::project::{Conn, Kind, Project, SOURCE_DB, folder_of};
use idb_core::redact::redact;
use idb_core::result::MAX_ROWS;
use idb_core::sqlsafe::{
    ExplainOpt, check_read_only_sql, danger_of, danger_of_redis, explain_sql, first_sql_word, is_read_only_redis, is_read_only_sql,
    keys_refusal, mask_sql, quote_ident, quote_literal, split_statements,
};
use idb_core::textutil::printable;
use idb_core::{connstore, statcrypt};
use idb_sources::redis::RedisConn;
use idb_sources::source::{self, Backend, LOKI_OPEN_LIMIT, OPEN_LIMIT, Query};
use idb_sources::{Clients, Error, Ran, Session, TableRef, changes_sql, undo_changes};
use serde::Serialize;
use serde_json::Value;

use crate::lookup;
use crate::protocol::*;
use crate::redis;

/// 테이블 보기 기본 페이지(R16.1)
const DEFAULT_PAGE: u64 = 200;
const DEFAULT_UNLOCK_MIN: u64 = 10;
const MAX_UNLOCK_MIN: u64 = 24 * 60;
const TX_END_WORDS: &[&str] = &["commit", "rollback", "end", "abort"];
/// 여러 접속을 거치는 조회(user360·schema-diff)의 시간 제한. CLI 기본 --timeout 과 같다
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(30);
/// Redis 모니터 기본값(Go redisSlowLogN·redisBigKeySample·redisBigKeyTop)
const SLOWLOG_N: i64 = 50;
const BIGKEY_SAMPLE: usize = 1000;
const BIGKEY_TOP: usize = 50;
/// 끊긴 Redis 세션을 한 번 알리는 글이다(R4.2, Go RedisConn.Run)
pub const REDIS_SESSION_LOST: &str = "이전 Redis 세션 연결이 끊겨 SELECT 한 DB 같은 세션 상태가 사라졌다. 다시 실행하면 새 연결에서 돈다";

/// serve 가 CLI 하위 명령을 돌리는 함수다. idb-cli 가 넘긴다(엔진이 CLI 에 기대지 않게).
/// 흐름·보드 한 번의 상한이다. 패널·흐름마다 15초 상한이 따로 있다
const DASH_TIMEOUT: Duration = Duration::from_secs(60);
/// 전체 내보내기가 serve 에 쌓아 두는 조각 수(조각마다 1MiB). 클라이언트가 읽지 않으면 COPY 가 멈춘다
const EXPORT_BUFFER: usize = 4;

pub type CliHandler = Arc<dyn Fn(CliRequest) -> CliResponse + Send + Sync>;

type RedisSlot = Arc<tokio::sync::Mutex<RedisState>>;
type RedisJob<'a, T> = Pin<Box<dyn Future<Output = Result<T, Error>> + Send + 'a>>;

#[derive(Default)]
struct RedisState {
    conn: Option<RedisConn>,
    /// 명령 중에 퓨처가 버려졌다(취소). 연결은 함께 버려졌다
    running: bool,
    /// 세션 연결이 끊겨 SELECT 같은 세션 상태를 잃었고 아직 알리지 않았다(세션 칸만)
    lost: bool,
}

/// Redis 연결 칸이다. 편집기 명령·값 편집은 세션, 트리·값 보기·모니터·일괄 삭제는 풀이다(Go 와 같다).
#[derive(Clone, Copy, PartialEq)]
enum RedisUse {
    Session,
    Pool,
}

pub struct LocalEngine {
    /// 접속을 고치면(R59) 다시 읽어 바꾼다. 요청은 시작할 때의 사본을 끝까지 쓴다
    proj: std::sync::RwLock<Arc<Project>>,
    clients: Arc<Clients>,
    sessions: tokio::sync::Mutex<HashMap<String, Arc<Session>>>,
    redis_session: Mutex<HashMap<String, RedisSlot>>,
    redis_pool: Mutex<HashMap<String, RedisSlot>>,
    /// prod 접속마다 열린 끝 시각(R54.2). 없으면 잠김
    unlocked: Mutex<HashMap<String, Instant>>,
    /// switch_db 로 바꾼 데이터베이스(R27.1). 프로젝트 파일은 고치지 않는다
    db_override: Mutex<HashMap<String, String>>,
    cli: Option<CliHandler>,
    /// 연 저장소 접속(uuid). AWS 설정 읽기·서명 준비를 트리 펼칠 때마다 하지 않게 둔다
    sources: tokio::sync::Mutex<HashMap<String, Arc<dyn Backend>>>,
    /// serve 로 원격 클라이언트에 답하는 중이다. 접속 파일 쓰기와 비밀번호 받기를 막는다(R59)
    serving: AtomicBool,
    /// 진행 중인 전체 내보내기(R33.1)
    exports: Mutex<HashMap<u64, Arc<ExportJob>>>,
    next_export: AtomicU64,
}

/// 전체 내보내기 하나다. COPY 는 따로 도는 작업이고 조각을 data 채널(EXPORT_BUFFER 개까지)에 넣는다.
struct ExportJob {
    conn: Conn,
    rx: tokio::sync::Mutex<ExportRx>,
    task: tokio::task::AbortHandle,
}

struct ExportRx {
    data: tokio::sync::mpsc::Receiver<Vec<u8>>,
    /// COPY 가 끝나면 행 수나 오류다
    end: Option<tokio::sync::oneshot::Receiver<Result<i64, Error>>>,
    bytes: u64,
}

impl LocalEngine {
    pub fn new(proj: Project, cli: Option<CliHandler>) -> LocalEngine {
        LocalEngine {
            proj: std::sync::RwLock::new(Arc::new(proj)),
            clients: Arc::new(Clients::new()),
            sessions: Default::default(),
            redis_session: Default::default(),
            redis_pool: Default::default(),
            unlocked: Default::default(),
            db_override: Default::default(),
            cli,
            sources: Default::default(),
            serving: AtomicBool::new(false),
            exports: Default::default(),
            next_export: AtomicU64::new(1),
        }
    }

    /// serve 가 부른다. 그 뒤로 접속 파일 쓰기(conn_save·conn_delete·conn_get)와 비밀번호가 실린 conn_test 를 거절한다.
    pub fn set_serving(&self) {
        self.serving.store(true, Ordering::Relaxed);
    }

    /// 지금 프로젝트다. 잠금은 사본을 꺼내는 동안만 쥔다.
    fn proj(&self) -> Arc<Project> {
        self.proj.read().unwrap().clone()
    }

    /// 세션(열린 트랜잭션은 서버가 되돌린다)과 풀을 닫는다.
    pub async fn close(&self) {
        for (_, job) in self.exports.lock().unwrap().drain() {
            job.task.abort();
        }
        let sessions: Vec<Arc<Session>> = self.sessions.lock().await.drain().map(|(_, s)| s).collect();
        for s in sessions {
            s.close().await;
        }
        self.sources.lock().await.clear();
        self.clients.close().await;
    }

    /// 요청 하나를 처리한다. cancel 은 serve 가 처리한다.
    pub async fn handle(&self, req: Request) -> Result<Value, EngineError> {
        match req {
            Request::Hello { .. } => json(Hello {
                version: PROTOCOL_VERSION,
                server: "db-rs".into(),
            }),
            Request::Cancel { .. } => Err(EngineError::usage("cancel is handled by serve")),
            Request::Project => json(self.project()),
            Request::Schemas { conn } => {
                let c = self.conn(&conn, Kind::Pg)?;
                json(self.pg(&c, |pg| async move { pg.find_schemas().await }).await?)
            }
            Request::Tables { conn, schema } => {
                let c = self.conn(&conn, Kind::Pg)?;
                let schema = if schema.is_empty() { "public".to_string() } else { schema };
                json(self.pg(&c, |pg| async move { pg.find_tables(&schema).await }).await?)
            }
            Request::Describe { conn, schema, table } => {
                let c = self.conn(&conn, Kind::Pg)?;
                let t = table_ref(schema, table);
                let r = self.pg(&c, |pg| async move { pg.describe_table(&t).await }).await?;
                json(ResultSet::from_query(r))
            }
            Request::Ddl { conn, schema, table } => {
                let c = self.conn(&conn, Kind::Pg)?;
                let t = table_ref(schema, table);
                json(self.pg(&c, |pg| async move { pg.table_ddl(&t).await }).await?)
            }
            Request::Search { conn, schema, text } => {
                let c = self.conn(&conn, Kind::Pg)?;
                if text.is_empty() {
                    return Err(EngineError::usage("찾을 글이 필요함"));
                }
                let (r, skipped) = self.pg(&c, |pg| async move { pg.search_data(&schema, &text).await }).await?;
                let mut rs = ResultSet::from_query(r);
                if !skipped.is_empty() {
                    rs.note = self.clean(&c, &format!("건너뛴 테이블: {}", skipped.join(", ")));
                }
                json(rs)
            }
            Request::Run(r) => json(self.run(&r, None).await?),
            Request::Explain {
                run,
                analyze,
                buffers,
                format,
            } => json(self.run(&run, Some(ExplainOpt { analyze, buffers, format })).await?),
            Request::TablePage(r) => json(self.table_page(&r).await?),
            Request::TxBegin { conn } => json(self.tx(&conn, "BEGIN", false).await?),
            Request::TxCommit { conn, confirm_prod } => json(self.tx(&conn, "COMMIT", confirm_prod).await?),
            Request::TxRollback { conn } => json(self.tx(&conn, "ROLLBACK", false).await?),
            Request::TxState { conn } => {
                let c = self.conn(&conn, Kind::Pg)?;
                json(TxState {
                    in_tx: self.in_tx(&c).await,
                    text: String::new(),
                })
            }
            Request::ProdLock { conn, unlock } => json(self.prod_lock(&conn, unlock)?),
            Request::RedisScan { conn, pattern } => {
                let c = self.conn(&conn, Kind::Redis)?;
                let pattern = if pattern.is_empty() { "*".to_string() } else { pattern };
                json(
                    self.redis_do(&c, RedisUse::Pool, |r| Box::pin(redis::scan_keys(r, pattern)))
                        .await?,
                )
            }
            Request::RedisValue { conn, key } => {
                let c = self.conn(&conn, Kind::Redis)?;
                json(self.redis_do(&c, RedisUse::Pool, |r| Box::pin(redis::get_value(r, key))).await?)
            }
            Request::RedisRun {
                conn,
                args,
                confirm_prod,
                confirm_danger,
            } => json(self.redis_run(&conn, args, confirm_prod, confirm_danger).await?),
            Request::Cli(r) => {
                let Some(cli) = self.cli.clone() else {
                    return Err(EngineError::usage("cli op is not available here"));
                };
                let out = tokio::task::spawn_blocking(move || cli(r))
                    .await
                    .map_err(|_| EngineError::run("internal error"))?;
                json(out)
            }
            Request::Databases { conn } => {
                let c = self.conn(&conn, Kind::Pg)?;
                json(self.pg(&c, |pg| async move { pg.databases().await }).await?)
            }
            Request::SwitchDb { conn, db } => json(self.switch_db(&conn, db).await?),
            Request::SetSearchPath { conn, schema } => {
                let c = self.conn(&conn, Kind::Pg)?;
                let ran = self.session(&c).await?.set_search_path(&schema).await;
                ran.res.map_err(|e| self.run_err(&c, &e))?;
                json(self.session_state(&c, ran.in_tx).await)
            }
            Request::Activity { conn } => {
                let c = self.conn(&conn, Kind::Pg)?;
                json(ResultSet::from_query(self.pg(&c, |pg| async move { pg.activity().await }).await?))
            }
            Request::Locks { conn } => {
                let c = self.conn(&conn, Kind::Pg)?;
                json(ResultSet::from_query(self.pg(&c, |pg| async move { pg.locks().await }).await?))
            }
            Request::TableSizes { conn } => {
                let c = self.conn(&conn, Kind::Pg)?;
                json(ResultSet::from_query(
                    self.pg(&c, |pg| async move { pg.table_sizes().await }).await?,
                ))
            }
            Request::SignalBackend {
                conn,
                pid,
                terminate,
                confirm,
                confirm_prod,
            } => {
                let c = self.conn(&conn, Kind::Pg)?;
                let what = if terminate { "연결 종료" } else { "쿼리 취소" };
                self.always_confirm(&c, confirm, confirm_prod, &format!("pid {pid} {what}"))?;
                self.pg(&c, |pg| async move { pg.signal_backend(pid, terminate).await }).await?;
                json(serde_json::json!({}))
            }
            Request::ForeignKeys {
                conn,
                schema,
                table,
                incoming,
            } => {
                let c = self.conn(&conn, Kind::Pg)?;
                let t = TableRef { schema, table };
                json(self.pg(&c, |pg| async move { pg.foreign_keys(&t, incoming).await }).await?)
            }
            Request::Resolve { conn, name } => {
                let c = self.conn(&conn, Kind::Pg)?;
                let sp = self.search_path(&c).await;
                json(self.pg(&c, |pg| async move { pg.resolve(&name, &sp).await }).await?)
            }
            Request::FunctionDef { conn, schema, name } => {
                let c = self.conn(&conn, Kind::Pg)?;
                json(self.pg(&c, |pg| async move { pg.function_def(&schema, &name).await }).await?)
            }
            Request::Catalog { conn } => {
                let c = self.conn(&conn, Kind::Pg)?;
                json(self.pg(&c, |pg| async move { pg.catalog().await }).await?)
            }
            Request::EnumLabels { conn, types } => {
                let c = self.conn(&conn, Kind::Pg)?;
                let out = self
                    .pg(&c, |pg| async move {
                        let mut out = HashMap::new();
                        for t in types {
                            let labels = pg.enum_labels(&t).await?;
                            out.insert(t, labels);
                        }
                        Ok(out)
                    })
                    .await?;
                json(out)
            }
            Request::EstimateRows { conn, sql, params } => {
                let c = self.conn(&conn, Kind::Pg)?;
                let (sent, _) = self.proj().expand_enc(&c, &sql).map_err(EngineError::usage)?;
                let (bound, args) = bind_params(&sent, &params)?;
                let sql = inline_args(&bound, &args);
                let sp = self.search_path(&c).await;
                let rows = self.pg(&c, |pg| async move { pg.estimate_rows(&sql, &sp).await }).await?;
                json(Count { rows })
            }
            Request::ApplyChanges {
                conn,
                changes,
                confirm_prod,
            } => json(self.apply(&conn, changes, confirm_prod, "제출").await?),
            Request::UndoChanges {
                conn,
                changes,
                applied,
                confirm_prod,
            } => {
                let undo = undo_changes(&changes, &applied).map_err(EngineError::usage)?;
                json(self.apply(&conn, undo, confirm_prod, "제출 되돌리기").await?)
            }
            Request::ChangesSql { changes, applied } => {
                let changes = match applied {
                    Some(a) => undo_changes(&changes, &a).map_err(EngineError::usage)?,
                    None => changes,
                };
                json(ChangesPreview {
                    sql: changes_sql(&changes),
                    changes,
                })
            }
            Request::ImportCsv {
                conn,
                schema,
                table,
                columns,
                rows,
                confirm_prod,
            } => {
                let c = self.conn(&conn, Kind::Pg)?;
                self.write_guard(
                    &c,
                    confirm_prod,
                    &format!("prod({})에 CSV {}행 가져오기: y/N", c.name, rows.len()),
                    false,
                )?;
                let t = TableRef { schema, table };
                let n = self.pg(&c, |pg| async move { pg.import_csv(&t, &columns, &rows).await }).await?;
                json(Count { rows: n })
            }
            Request::Linked {
                conn,
                column,
                value,
                table,
            } => {
                let c = self.conn(&conn, Kind::Pg)?;
                let err_text = |c: &Conn, e: &Error| self.clean(c, &e.to_string());
                let deadline = tokio::time::Instant::now() + LOOKUP_TIMEOUT;
                let (res, failed) = lookup::linked(&self.proj(), self.clients.clone(), &c, &column, &table, &value, deadline, &err_text)
                    .await
                    .map_err(EngineError::usage)?;
                json(LinkedResult {
                    result: ResultSet::from_query(res),
                    failed,
                })
            }
            Request::SchemaDiff {
                conn,
                to,
                schema,
                to_schema,
            } => {
                let (a, b) = (self.conn(&conn, Kind::Pg)?, self.conn(&to, Kind::Pg)?);
                let schema = if schema.is_empty() { "public".to_string() } else { schema };
                let to_schema = if to_schema.is_empty() { schema.clone() } else { to_schema };
                let deadline = tokio::time::Instant::now() + LOOKUP_TIMEOUT;
                let res = lookup::schema_diff(self.clients.clone(), &a, &b, &schema, &to_schema, deadline)
                    .await
                    .map_err(|e| EngineError::run(self.clean(&a, &self.clean(&b, &e.to_string()))))?;
                json(ResultSet::from_query(res))
            }
            Request::User360 { query } => {
                if query.trim().is_empty() {
                    return Err(EngineError::usage("찾을 이메일·uid·id·닉네임이 필요함"));
                }
                let qs = lookup::user360_plan(&self.proj(), &query).map_err(|e| EngineError::run(self.clean_all(&e.to_string())))?;
                let deadline = tokio::time::Instant::now() + LOOKUP_TIMEOUT;
                let rows = lookup::user360_run(&self.proj(), self.clients.clone(), &qs, deadline).await;
                let items: Vec<User360Item> = rows
                    .into_iter()
                    .map(|r| User360Item {
                        error: r.err.map(|e| match self.proj().conn_by_name(&r.conn) {
                            Some(c) => self.clean(c, &e),
                            None => self.clean_all(&e),
                        }),
                        title: r.title,
                        conn: r.conn,
                        result: r.res.map(ResultSet::from_query),
                    })
                    .collect();
                json(items)
            }
            Request::RedisRawValue { conn, key } => {
                let c = self.conn(&conn, Kind::Redis)?;
                json(self.redis_do(&c, RedisUse::Pool, |r| Box::pin(redis::raw_value(r, key))).await?)
            }
            Request::RedisEdit {
                conn,
                edit,
                confirm_prod,
                confirm_danger,
            } => json(self.redis_edit(&conn, edit, confirm_prod, confirm_danger).await?),
            Request::RedisInfo { conn } => {
                let c = self.conn(&conn, Kind::Redis)?;
                json(self.redis_do(&c, RedisUse::Pool, |r| Box::pin(redis::info_summary(r))).await?)
            }
            Request::RedisSlowlog { conn, n } => {
                let c = self.conn(&conn, Kind::Redis)?;
                let n = if n <= 0 { SLOWLOG_N } else { n };
                json(self.redis_do(&c, RedisUse::Pool, |r| Box::pin(redis::slow_log(r, n))).await?)
            }
            Request::RedisBigKeys { conn, sample, top } => {
                let c = self.conn(&conn, Kind::Redis)?;
                let sample = if sample == 0 { BIGKEY_SAMPLE } else { sample.min(100_000) };
                let top = if top == 0 { BIGKEY_TOP } else { top.min(MAX_ROWS) };
                json(
                    self.redis_do(&c, RedisUse::Pool, |r| Box::pin(redis::big_keys(r, sample, top)))
                        .await?,
                )
            }
            Request::RedisPatternKeys { conn, pattern } => {
                let c = self.conn(&conn, Kind::Redis)?;
                if pattern.is_empty() {
                    return Err(EngineError::usage("패턴이 없어 지우지 않음"));
                }
                json(
                    self.redis_do(&c, RedisUse::Pool, |r| {
                        Box::pin(redis::pattern_keys(r, pattern, redis::DELETE_LIMIT))
                    })
                    .await?,
                )
            }
            Request::RedisUnlink {
                conn,
                keys,
                confirm,
                confirm_prod,
            } => {
                let c = self.conn(&conn, Kind::Redis)?;
                if keys.len() > redis::DELETE_LIMIT {
                    return Err(EngineError::usage(format!(
                        "키가 {}개를 넘어 지우지 않음 (패턴을 좁히세요)",
                        redis::DELETE_LIMIT
                    )));
                }
                self.always_confirm(&c, confirm, confirm_prod, &format!("키 {}개 지움", keys.len()))?;
                let n = self.redis_do(&c, RedisUse::Pool, |r| Box::pin(redis::unlink_keys(r, keys))).await?;
                json(Count { rows: n })
            }
            Request::SourceChildren { conn, path } => {
                let c = self.source_conn(&conn)?;
                let b = self.source(&c).await?;
                json(b.children(&path).await.map_err(|e| self.source_err(&c, e))?)
            }
            Request::SourceOpen { conn, path, limit } => {
                let c = self.source_conn(&conn)?;
                let b = self.source(&c).await?;
                let limit = match limit {
                    0 if c.kind == Some(Kind::Loki) => LOKI_OPEN_LIMIT,
                    0 => OPEN_LIMIT,
                    n => n.min(MAX_ROWS),
                };
                let r = b.open(&path, limit).await.map_err(|e| self.source_err(&c, e))?;
                json(ResultSet::from_query(r))
            }
            Request::SourceRun(r) => {
                let c = self.source_conn(&r.conn)?;
                if c.kind == Some(Kind::Aws) {
                    return Err(EngineError::usage("aws 접속은 트리로 본다"));
                }
                if r.text.trim().is_empty() {
                    return Err(EngineError::usage("실행할 글이 없음"));
                }
                let b = self.source(&c).await?;
                let q = Query {
                    text: r.text.trim().to_string(),
                    since_ns: r.since_ns,
                    until_ns: r.until_ns,
                    limit: r.limit,
                    force: r.force,
                    direction: r.direction,
                    deadline: None,
                };
                json(ResultSet::from_query(b.run(&q).await.map_err(|e| self.source_err(&c, e))?))
            }
            Request::Health { conn } => {
                let c = self.conn_any(&conn)?;
                let h = idb_sources::health::probe_any(&self.clients, &c).await;
                json(HealthInfo {
                    ok: h.ok,
                    latency_ms: h.latency.as_millis() as u64,
                    summary: h.summary(),
                    version: h.version,
                    metrics: h.metrics,
                    err: self.clean(&c, &h.err),
                })
            }
            Request::Flows { names, prod } => {
                let all = idb_core::flows::load(&self.proj().dir, &self.proj().conns).map_err(|e| EngineError::run(self.clean_all(&e)))?;
                let list = if names.is_empty() {
                    all
                } else {
                    let mut list = Vec::new();
                    for n in &names {
                        let f = all.iter().find(|f| &f.name == n);
                        list.push(
                            f.cloned()
                                .ok_or_else(|| EngineError::usage(format!("흐름 없음: {n} (.dbtui/flows.json)")))?,
                        );
                    }
                    list
                };
                let deadline = tokio::time::Instant::now() + DASH_TIMEOUT;
                let boxes = crate::flows::resolve_flows(&self.clients, &self.proj().conns, &list, prod, deadline).await;
                json(
                    list.iter()
                        .zip(boxes)
                        .map(|(f, bs)| FlowResult {
                            flow: f.name.clone(),
                            opens: crate::flows::box_opens(f, &bs),
                            boxes: bs.into_iter().map(|b| self.clean_box(b)).collect(),
                        })
                        .collect::<Vec<_>>(),
                )
            }
            Request::BoardList => {
                use crate::board::{BOARD_DIR, LoadErr, list_boards, load_board};
                let items: Vec<BoardSummary> = list_boards(&self.proj().dir)
                    .into_iter()
                    .map(|n| match load_board(&self.proj(), &n) {
                        Ok(b) => BoardSummary {
                            title: b.title,
                            panels: b.panels.len(),
                            error: self.clean_all(&b.err),
                            name: n,
                        },
                        Err(LoadErr::NotFound) => BoardSummary {
                            error: format!("open {BOARD_DIR}/{n}.json: no such file or directory"),
                            name: n,
                            ..Default::default()
                        },
                        Err(LoadErr::Other(e)) => BoardSummary {
                            name: n,
                            error: self.clean_all(&e),
                            ..Default::default()
                        },
                    })
                    .collect();
                json(items)
            }
            Request::BoardRun { name, prod } => json(self.board_run(&name, prod).await?),
            Request::OutboxExhausted => {
                let all = idb_core::flows::load(&self.proj().dir, &self.proj().conns).map_err(|e| EngineError::run(self.clean_all(&e)))?;
                let deadline = tokio::time::Instant::now() + DASH_TIMEOUT;
                json(crate::flows::outbox_exhausted(&self.clients, &self.proj().conns, &all, deadline).await)
            }
            Request::Decrypt { conn, cells } => json(self.decrypt(&conn, cells)?),
            Request::ExportStart { conn, sql } => json(self.export_start(&conn, &sql).await?),
            Request::ExportRead { export } => json(self.export_read(export).await?),
            Request::ExportCancel { export } => {
                let job = self.exports.lock().unwrap().remove(&export);
                if let Some(j) = &job {
                    j.task.abort();
                }
                json(Canceled { canceled: job.is_some() })
            }
            Request::ConnGet { name } => json(self.conn_get(&name)?),
            Request::ConnTest { name, conn, old_name } => json(self.conn_test(&name, conn, &old_name).await?),
            Request::ConnSave { old_name, conn } => json(self.conn_save(&old_name, conn).await?),
            Request::ConnDelete { name } => {
                self.conn_delete(&name).await?;
                json(serde_json::json!({}))
            }
            Request::ConnFolder { names, folder } => {
                self.conn_folder(&names, &folder).await?;
                json(serde_json::json!({}))
            }
            Request::ReadQuery { conn, sql, limit } => {
                let c = self.conn(&conn, Kind::Pg)?;
                // 접속을 열기 전에 거른다. 놓친 쓰기는 run_read_only_rows 의 READ ONLY 트랜잭션이 막는다
                check_read_only_sql(&sql).map_err(|e| EngineError::refused(REASON_READ_ONLY, e.to_string()))?;
                let limit = if limit == 0 { MAX_ROWS } else { limit.min(MAX_ROWS) };
                let r = self.pg(&c, |pg| async move { pg.run_read_only_rows(&sql, limit).await }).await?;
                json(ResultSet::from_query(r))
            }
        }
    }

    // ─── 복호화 보기 (R37) ───

    /// 암호문 꼴 칸만 그 접속 그룹 키로 푼다. 풀지 못한 칸은 null 로 두고 센다. 키는 답·오류 어디에도 싣지 않는다.
    fn decrypt(&self, name: &str, cells: Vec<String>) -> Result<Decrypted, EngineError> {
        let c = self.conn_any(name)?;
        if c.group.is_empty() {
            return Err(EngineError::usage("접속 그룹을 몰라 복호화 키를 고를 수 없음"));
        }
        let (key, _) = self.proj().aes_key_iv(&c);
        if key.is_empty() {
            return Err(EngineError::usage(format!(
                "{}_AES_KEY 가 .env 에 없음",
                idb_core::project::env_name(&c.group)
            )));
        }
        let mut failed = 0;
        let values = cells
            .iter()
            .map(|v| {
                if !statcrypt::looks_encrypted(v) {
                    return None;
                }
                let p = statcrypt::decrypt(v, &key).ok();
                failed += usize::from(p.is_none());
                p
            })
            .collect();
        Ok(Decrypted { values, failed })
    }

    // ─── 전체 내보내기 (R33.1) ───

    /// 문장을 거른 뒤 COPY 를 따로 띄운다. 거절 글은 화면 글과 같다. 편집기 세션이 아니라 풀 연결의 READ ONLY 트랜잭션에서
    /// 돌아(Go CopyOut) prod 잠금과 상관없이 읽기만 하고, 편집기에서 고른 데이터베이스(R27.1)·search_path(R27.2)를 따른다.
    async fn export_start(&self, name: &str, sql: &str) -> Result<ExportStarted, EngineError> {
        let c = self.conn(name, Kind::Pg)?;
        let stmts = split_statements(sql);
        match stmts.len() {
            0 => return Err(EngineError::usage("전체 내보낼 문장이 없음")),
            1 => {}
            _ => return Err(EngineError::usage("문장 하나만 전체 내보내기 (커서 문장으로 다시 실행)")),
        }
        // read_query 와 같은 검사다. 첫 단어만 보면 `select pg_terminate_backend(…)` 같은 부수 효과 함수가 지나가고
        // 그런 함수는 READ ONLY 트랜잭션 안에서도 돈다. 그래서 prod 잠금(R54)과 상관없이 늘 막는다
        match check_read_only_sql(stmts[0]) {
            Err(idb_core::sqlsafe::ReadOnlyError::NotReadOnly) => return Err(EngineError::usage("읽기 문장만 전체 내보내기")),
            Err(e) => return Err(EngineError::refused(REASON_READ_ONLY, e.to_string())),
            Ok(()) => {}
        }
        if !matches!(first_sql_word(stmts[0]).as_str(), "select" | "with" | "values" | "table") {
            return Err(EngineError::usage("SELECT 문만 전체 내보내기"));
        }
        let danger = danger_of(stmts[0]);
        if !danger.is_empty() {
            return Err(EngineError::usage(format!("위험 문장은 전체 내보내지 않음: {danger}")));
        }
        if !idb_core::params::bind(stmts[0]).1.is_empty() {
            return Err(EngineError::usage("파라미터(:name·$n)가 있는 문장은 전체 내보내지 않음"));
        }
        let (stmt, note) = self.proj().expand_enc(&c, stmts[0]).map_err(EngineError::usage)?;
        check_read_only_sql(&stmt).map_err(|e| EngineError::refused(REASON_READ_ONLY, e.to_string()))?;
        let pg = self.clients.pg(&c).await.map_err(|e| self.run_err(&c, &e))?;
        let sp = self.search_path(&c).await;
        let (data_tx, data) = tokio::sync::mpsc::channel(EXPORT_BUFFER);
        let (end_tx, end) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = end_tx.send(pg.copy_out(&stmt, &sp, &data_tx).await);
        });
        let id = self.next_export.fetch_add(1, Ordering::Relaxed);
        let job = ExportJob {
            conn: c,
            rx: tokio::sync::Mutex::new(ExportRx {
                data,
                end: Some(end),
                bytes: 0,
            }),
            task: task.abort_handle(),
        };
        self.exports.lock().unwrap().insert(id, Arc::new(job));
        Ok(ExportStarted { export: id, note })
    }

    /// 다음 조각을 기다려 돌려준다. 채널이 비고 COPY 가 끝났으면 done 이고 그 id 는 지운다. 기다리다 버려도 조각은 잃지 않는다.
    async fn export_read(&self, id: u64) -> Result<ExportChunk, EngineError> {
        let job = self
            .exports
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or_else(|| EngineError::usage(format!("전체 내보내기 없음: {id}")))?;
        let mut rx = job.rx.lock().await;
        if let Some(data) = rx.data.recv().await {
            rx.bytes += data.len() as u64;
            return Ok(ExportChunk {
                data,
                done: false,
                bytes: rx.bytes,
                rows: 0,
            });
        }
        let end = match rx.end.take() {
            Some(end) => end.await,
            None => return Err(EngineError::usage(format!("전체 내보내기 없음: {id}"))),
        };
        self.exports.lock().unwrap().remove(&id);
        match end {
            Ok(Ok(rows)) => Ok(ExportChunk {
                data: Vec::new(),
                done: true,
                bytes: rx.bytes,
                rows,
            }),
            Ok(Err(e)) => Err(self.run_err(&job.conn, &e)),
            Err(_) => Err(EngineError::run("canceled")),
        }
    }

    /// 접속 uuid 를 쓰는 연결·세션·저장소·내보내기를 닫는다(R59.3 고치기 전 Forget). 다음 요청은 새 설정으로 연다.
    async fn forget_conn(&self, uuid: &str) {
        let jobs: Vec<u64> = self
            .exports
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, j)| j.conn.uuid == uuid)
            .map(|(id, _)| *id)
            .collect();
        for id in jobs {
            if let Some(j) = self.exports.lock().unwrap().remove(&id) {
                j.task.abort();
            }
        }
        let old = self.sessions.lock().await.remove(uuid);
        if let Some(s) = old {
            s.close().await;
        }
        self.clients.forget(uuid).await;
        self.redis_session.lock().unwrap().remove(uuid);
        self.redis_pool.lock().unwrap().remove(uuid);
        self.sources.lock().await.remove(uuid);
    }

    // ─── 접속 편집 (R59·R66.2) ───

    /// 원격 serve 는 접속 파일을 고치지 않는다. 비밀번호가 ssh 너머로 오가고, 그 기계의 프로젝트를 다른 클라이언트도 같이 쓴다.
    fn local_only(&self) -> Result<(), EngineError> {
        if self.serving.load(Ordering::Relaxed) {
            return Err(EngineError::refused(
                REASON_REMOTE_CONN_EDIT,
                "원격 serve 에서는 접속을 고치지 않음 (그 기계에서 db conn 으로 고침)",
            ));
        }
        Ok(())
    }

    /// 고칠 수 있는 db 전용 접속이다. DataGrip 접속은 DataGrip 에서 고친다.
    fn db_only_conn(&self, name: &str) -> Result<Conn, EngineError> {
        let c = self
            .proj()
            .conn_by_name(name)
            .cloned()
            .ok_or_else(|| EngineError::usage(format!("접속 없음: {name}")))?;
        if c.source != SOURCE_DB {
            return Err(EngineError::usage(connstore::DATAGRIP_REFUSAL));
        }
        Ok(c)
    }

    fn conn_get(&self, name: &str) -> Result<ConnFields, EngineError> {
        self.local_only()?;
        let c = self.db_only_conn(name)?;
        Ok(ConnFields {
            name: c.name,
            group: c.group,
            kind: c.kind.map(Kind::as_str).unwrap_or("").into(),
            host: c.host,
            port: c.port,
            db: c.db,
            user: c.user,
            password: String::new(),
            url: c.url,
            auth: c.auth,
            region: c.region,
            profile: c.profile,
            workgroup: c.workgroup,
            output: c.output,
            sslmode: c.sslmode,
            has_password: !c.password.is_empty(),
        })
    }

    async fn conn_test(&self, name: &str, fields: Option<ConnFields>, old_name: &str) -> Result<ConnTested, EngineError> {
        let c = match fields {
            None => self.conn_any(name)?,
            Some(f) => {
                // 창의 값으로 시험하는 것은 이 기계에서만이다. 원격에서 받으면 빈 비밀번호 + 저장된 이름 + 고른 호스트로
                // 저장된 비밀번호를 남의 호스트에 보낼 수 있다. serve 는 저장된 접속 이름(conn_test{name})만 시험한다
                self.local_only()?;
                let stored = if old_name.is_empty() {
                    f.name.clone()
                } else {
                    old_name.to_string()
                };
                let mut c = conn_of_fields(f);
                if c.password.is_empty()
                    && let Some(old) = self.proj().conn_by_name(&stored)
                {
                    c.password = old.password.clone();
                }
                connstore::validate_conn(&c).map_err(|e| EngineError::usage(self.clean_with(&c, &e)))?;
                c
            }
        };
        let warning = connstore::conn_warning(&c).to_string();
        let version = match tokio::time::timeout(LOOKUP_TIMEOUT, source::test_conn(&c)).await {
            Ok(r) => r,
            Err(_) => Err("context deadline exceeded".into()),
        }
        .map_err(|e| EngineError::run(format!("접속 실패: {}", self.clean_with(&c, &e))))?;
        Ok(ConnTested { version, warning })
    }

    async fn conn_save(&self, old_name: &str, f: ConnFields) -> Result<ConnSaved, EngineError> {
        self.local_only()?;
        if !connstore::valid_conn_name(&f.name) {
            return Err(EngineError::usage("접속 이름은 영숫자·-·_ 만 받음"));
        }
        let old = if old_name.is_empty() {
            None
        } else {
            Some(self.db_only_conn(old_name)?)
        };
        let c = conn_of_fields(f);
        connstore::validate_conn(&c).map_err(|e| EngineError::usage(self.clean_with(&c, &e)))?;
        let warning = connstore::conn_warning(&c).to_string();
        let dir = self.proj().dir.clone();
        let r = match &old {
            Some(o) => connstore::update_conn(&dir, &o.name, c.clone()),
            None => connstore::add_conn(&dir, c.clone()),
        };
        let (saved, env_error) = match r {
            Ok(saved) => (saved, String::new()),
            Err(e) if e.env_not_updated => (c.clone(), self.clean_with(&c, &e.msg)),
            Err(e) => return Err(EngineError::run(self.clean_with(&c, &e.msg))),
        };
        if let Some(o) = &old {
            self.forget_conn(&o.uuid).await;
            // 파일의 DB 를 고쳤으면 B 로 고른 데이터베이스를 풀고, 그대로면 지킨다(R59.3)
            if o.db != saved.db {
                self.db_override.lock().unwrap().remove(&o.uuid);
            }
        }
        self.reload()?;
        let summary = match self.proj().conn_by_name(&saved.name) {
            Some(now) => self.summary(now),
            None => self.summary(&saved),
        };
        Ok(ConnSaved {
            conn: summary,
            warning,
            env_error,
        })
    }

    async fn conn_delete(&self, name: &str) -> Result<(), EngineError> {
        self.local_only()?;
        let c = self.db_only_conn(name)?;
        connstore::remove_conn(&self.proj().dir, name).map_err(|e| EngineError::run(self.clean_all(&e.msg)))?;
        self.forget_conn(&c.uuid).await;
        self.db_override.lock().unwrap().remove(&c.uuid);
        self.unlocked.lock().unwrap().remove(&c.uuid);
        self.reload()
    }

    /// 폴더는 보이는 묶음이라(R66.2) 비밀값이 없어 원격에서도 옮긴다. 연결은 닫지 않는다(R66.3).
    async fn conn_folder(&self, names: &[String], folder: &str) -> Result<(), EngineError> {
        let p = self.proj();
        let mut conns = Vec::with_capacity(names.len());
        for n in names {
            conns.push(p.conn_by_name(n).ok_or_else(|| EngineError::usage(format!("접속 없음: {n}")))?);
        }
        connstore::set_folder(&p.dir, &conns, folder).map_err(|e| EngineError::usage(self.clean_all(&e)))?;
        self.reload()
    }

    /// 접속 파일을 다시 읽어 바꾼다. 진행 중인 요청은 옛 사본으로 끝난다.
    fn reload(&self) -> Result<(), EngineError> {
        let dir = self.proj().dir.clone();
        let p = idb_core::project::load_project(&dir)
            .map_err(|e| EngineError::run(format!("접속을 저장했지만 다시 읽지 못함: {}", self.clean_all(&e.to_string()))))?;
        *self.proj.write().unwrap() = Arc::new(p);
        Ok(())
    }

    // ─── 저장소 ───

    /// 읽기 전용 저장소 접속이다(PostgreSQL·Redis 가 아닌 것).
    fn source_conn(&self, name: &str) -> Result<Conn, EngineError> {
        let c = self.conn_any(name)?;
        if !c.kind.is_some_and(Kind::is_source) {
            return Err(EngineError::usage(format!("{name} 는 저장소 접속이 아님 ({})", c.kind_str())));
        }
        Ok(c)
    }

    /// 종류를 가리지 않는 이름의 접속이다.
    fn conn_any(&self, name: &str) -> Result<Conn, EngineError> {
        if name.is_empty() {
            return Err(EngineError::usage("접속 이름이 필요함"));
        }
        self.proj()
            .conn_by_name(name)
            .filter(|c| c.kind.is_some())
            .cloned()
            .ok_or_else(|| EngineError::usage(format!("접속 없음: {name}")))
    }

    /// 연 저장소 접속을 돌려주고, 없으면 연다. 여는 동안 잠그지 않아 다른 접속의 요청을 막지 않는다.
    async fn source(&self, c: &Conn) -> Result<Arc<dyn Backend>, EngineError> {
        if let Some(b) = self.sources.lock().await.get(&c.uuid) {
            return Ok(b.clone());
        }
        let b: Arc<dyn Backend> = source::open(c).await.map_err(|e| self.source_err(c, e))?.into();
        Ok(self.sources.lock().await.entry(c.uuid.clone()).or_insert(b).clone())
    }

    /// 저장소 오류다. 쓰기·비싼 질의·요금 조회는 거절(3)이고 글은 CLI 와 같은 안내다(R60.2·R72).
    fn source_err(&self, c: &Conn, e: Error) -> EngineError {
        let text = self.clean(c, &source::err_text(&e));
        match e {
            Error::Write(_) => EngineError::refused(REASON_READ_ONLY, text),
            Error::NeedsConfirm(_) => EngineError::refused(REASON_CONFIRM_EXPENSIVE, text),
            Error::Billed => EngineError::refused(REASON_BILLED, text),
            _ => EngineError::run(text),
        }
    }

    fn clean_box(&self, mut b: crate::flows::FlowBox) -> crate::flows::FlowBox {
        b.err = self.clean_all(&b.err);
        b
    }

    async fn board_run(&self, name: &str, prod: bool) -> Result<BoardResult, EngineError> {
        use crate::board::{BOARD_DIR, LoadErr, load_board, panel_prod, panel_value, run_panels};
        let b = match load_board(&self.proj(), name) {
            Ok(b) => b,
            Err(LoadErr::NotFound) => return Err(EngineError::usage(format!("보드 없음: {name} ({BOARD_DIR})"))),
            Err(LoadErr::Other(e)) => return Err(EngineError::usage(self.clean_all(&e))),
        };
        let deadline = tokio::time::Instant::now() + DASH_TIMEOUT;
        let results = run_panels(&self.clients, &self.proj(), &b, prod, deadline).await;
        let panels = b
            .panels
            .iter()
            .zip(results)
            .map(|(p, r)| {
                let mut out = PanelOut {
                    title: p.title.clone(),
                    conn: p.conn.clone(),
                    flow: p.flow.clone(),
                    view: p.view.clone(),
                    prod: panel_prod(&self.proj(), p),
                    note: r.note,
                    query: p.query.clone(),
                    since: p.since.clone(),
                    ..Default::default()
                };
                if !p.err.is_empty() {
                    out.error = format!("설정 오류: {}", self.clean_all(&p.err));
                } else if !r.err.is_empty() {
                    out.error = self.clean_all(&r.err);
                } else if p.view == "flow" {
                    out.boxes = r.boxes.into_iter().map(|b| self.clean_box(b)).collect();
                } else if p.view == "number" {
                    let v = panel_value(r.res.as_ref()).unwrap_or_default();
                    out.warn = p.warn_hit(&v);
                    out.value = Some(v);
                } else {
                    out.result = r.res.map(ResultSet::from_query);
                }
                out
            })
            .collect();
        Ok(BoardResult {
            title: b.title,
            error: self.clean_all(&b.err),
            panels,
            every_secs: (b.every_ns / 1_000_000_000).max(0) as u64,
        })
    }

    fn project(&self) -> ProjectInfo {
        let p = self.proj();
        ProjectInfo {
            dir: p.dir.to_string_lossy().into_owned(),
            conns: p.conns.iter().map(|c| self.summary(c)).collect(),
            warnings: p.warnings.iter().map(|w| self.clean_all(w)).collect(),
        }
    }

    fn summary(&self, c: &Conn) -> ConnSummary {
        ConnSummary {
            name: c.name.clone(),
            group: c.group.clone(),
            folder: folder_of(c).into(),
            kind: c.kind_str().into(),
            env: c.env().into(),
            host: if c.host.is_empty() { c.url.clone() } else { c.host.clone() },
            db: self
                .db_override
                .lock()
                .unwrap()
                .get(&c.uuid)
                .cloned()
                .unwrap_or_else(|| c.db.clone()),
            source: if c.source == SOURCE_DB { SOURCE_DB } else { "datagrip" }.into(),
            uuid: c.uuid.clone(),
            region: c.region.clone(),
            profile: c.profile.clone(),
        }
    }

    // ─── 접속·오류 ───

    /// 이름의 접속이다. switch_db 로 바꾼 DB 를 입힌 사본이다.
    fn conn(&self, name: &str, kind: Kind) -> Result<Conn, EngineError> {
        if name.is_empty() {
            return Err(EngineError::usage("접속 이름이 필요함"));
        }
        let mut c = self
            .proj()
            .conn_by_name(name)
            .cloned()
            .ok_or_else(|| EngineError::usage(format!("접속 없음: {name}")))?;
        if c.kind != Some(kind) {
            return Err(EngineError::usage(format!("{name} 는 {kind} 접속이 아님")));
        }
        if let Some(db) = self.db_override.lock().unwrap().get(&c.uuid) {
            c.db = db.clone();
        }
        Ok(c)
    }

    /// 내보낼 글로 만든다. 서버가 비밀번호·키를 되돌려줘도 나오지 않게 그 접속의 비밀번호와 모든 AES 키·IV 를 지운다(CLI err_text 와 같다).
    fn clean(&self, c: &Conn, msg: &str) -> String {
        let mut msg = redact(msg, &c.password);
        for v in self.proj().aes_env.values() {
            msg = redact(&msg, v);
        }
        printable(&msg, true)
    }

    /// 접속을 모를 때(프로젝트 경고·여러 접속 조회)는 모든 비밀번호를 지운다.
    fn clean_all(&self, msg: &str) -> String {
        let mut msg = msg.to_string();
        for c in &self.proj().conns {
            msg = redact(&msg, &c.password);
        }
        for v in self.proj().aes_env.values() {
            msg = redact(&msg, v);
        }
        printable(&msg, true)
    }

    /// 저장 전 접속(프로젝트에 아직 없음)의 오류 글이다. 받은 비밀번호와 프로젝트의 비밀번호·키를 모두 지운다.
    fn clean_with(&self, c: &Conn, msg: &str) -> String {
        self.clean_all(&redact(msg, &c.password))
    }

    fn run_err(&self, c: &Conn, e: &Error) -> EngineError {
        EngineError::run(self.clean(c, &e.to_string()))
    }

    async fn pg<T, F, Fut>(&self, c: &Conn, f: F) -> Result<T, EngineError>
    where
        F: FnOnce(idb_sources::Pg) -> Fut,
        Fut: Future<Output = Result<T, Error>>,
    {
        let r = async { f(self.clients.pg(c).await?).await }.await;
        r.map_err(|e| self.run_err(c, &e))
    }

    /// 이미 연 세션이 트랜잭션 안인지다. 세션을 새로 열지 않는다.
    async fn in_tx(&self, c: &Conn) -> bool {
        let s = self.sessions.lock().await.get(&c.uuid).cloned();
        match s {
            Some(s) => s.in_tx().await,
            None => false,
        }
    }

    /// 이미 연 세션에서 고른 search_path 다.
    async fn search_path(&self, c: &Conn) -> String {
        let s = self.sessions.lock().await.get(&c.uuid).cloned();
        match s {
            Some(s) => s.search_path().await,
            None => String::new(),
        }
    }

    async fn session_state(&self, c: &Conn, in_tx: bool) -> SessionState {
        SessionState {
            conn: c.name.clone(),
            db: c.db.clone(),
            search_path: self.search_path(c).await,
            in_tx,
        }
    }

    async fn session(&self, c: &Conn) -> Result<Arc<Session>, EngineError> {
        let mut map = self.sessions.lock().await;
        if let Some(s) = map.get(&c.uuid) {
            return Ok(s.clone());
        }
        let pg = self.clients.pg(c).await.map_err(|e| self.run_err(c, &e))?;
        let s = Arc::new(Session::new(pg));
        map.insert(c.uuid.clone(), s.clone());
        Ok(s)
    }

    /// R27.1: 세션과 풀을 닫고 다음 연결부터 db 로 연다. 열린 트랜잭션이 있으면 바꾸지 않는다.
    async fn switch_db(&self, name: &str, db: String) -> Result<SessionState, EngineError> {
        let c = self.conn(name, Kind::Pg)?;
        if self.in_tx(&c).await {
            return Err(EngineError::usage(
                "열린 트랜잭션이 있어 데이터베이스를 바꾸지 않음 (COMMIT·ROLLBACK 먼저)",
            ));
        }
        let old = self.sessions.lock().await.remove(&c.uuid);
        if let Some(s) = old {
            s.close().await;
        }
        self.clients.forget(&c.uuid).await;
        let base = self.proj().conn_by_name(name).map(|c| c.db.clone()).unwrap_or_default();
        {
            let mut m = self.db_override.lock().unwrap();
            if db.is_empty() || db == base {
                m.remove(&c.uuid);
            } else {
                m.insert(c.uuid.clone(), db);
            }
        }
        let c = self.conn(name, Kind::Pg)?;
        Ok(self.session_state(&c, false).await)
    }

    // ─── prod 잠금 (R54) ───

    fn unlock_min() -> u64 {
        match std::env::var("DB_PROD_UNLOCK_MIN").ok().and_then(|v| v.parse::<u64>().ok()) {
            Some(n) if n > 0 => n.min(MAX_UNLOCK_MIN),
            _ => DEFAULT_UNLOCK_MIN,
        }
    }

    fn locked(&self, c: &Conn) -> bool {
        c.env() == "prod" && self.unlocked.lock().unwrap().get(&c.uuid).is_none_or(|t| Instant::now() >= *t)
    }

    fn lock_state(&self, c: &Conn) -> LockState {
        let remaining = self
            .unlocked
            .lock()
            .unwrap()
            .get(&c.uuid)
            .map_or(0, |t| t.saturating_duration_since(Instant::now()).as_secs());
        LockState {
            conn: c.name.clone(),
            prod: c.env() == "prod",
            locked: self.locked(c),
            remaining_secs: if self.locked(c) { 0 } else { remaining },
            unlock_min: Self::unlock_min(),
        }
    }

    fn prod_lock(&self, name: &str, unlock: Option<bool>) -> Result<LockState, EngineError> {
        let c = self
            .proj()
            .conn_by_name(name)
            .cloned()
            .ok_or_else(|| EngineError::usage(format!("접속 없음: {name}")))?;
        if let Some(open) = unlock {
            if c.env() != "prod" {
                return Err(EngineError::usage("prod 접속이 아니라 잠그지 않음"));
            }
            let mut m = self.unlocked.lock().unwrap();
            if open {
                m.insert(c.uuid.clone(), Instant::now() + Duration::from_secs(Self::unlock_min() * 60));
            } else {
                m.remove(&c.uuid);
            }
        }
        Ok(self.lock_state(&c))
    }

    fn lock_refusal(&self, c: &Conn, tx_hint: bool) -> EngineError {
        let mut msg = format!("prod({}) 쓰기 잠김 — {}분 열기", c.name, Self::unlock_min());
        if tx_hint {
            msg.push_str(" · 트랜잭션 끝내기는 F10/F11");
        }
        EngineError::refused(REASON_PROD_LOCKED, msg)
    }

    /// prod 쓰기의 문이다. 잠겨 있으면 확인 없이 거절(R54.1), 열려 있으면 confirm_prod(R6)를 요구한다. prod 가 아니면 통과다.
    fn write_guard(&self, c: &Conn, confirm_prod: bool, ask: &str, tx_hint: bool) -> Result<(), EngineError> {
        if c.env() != "prod" {
            return Ok(());
        }
        if self.locked(c) {
            return Err(self.lock_refusal(c, tx_hint));
        }
        if !confirm_prod {
            return Err(EngineError::refused(REASON_CONFIRM_PROD, ask));
        }
        Ok(())
    }

    /// 환경과 상관없이 늘 묻는 쓰기(백엔드 신호·패턴 일괄 삭제)다. prod 는 잠금과 confirm_prod 를, 아니면 confirm 을 요구한다.
    fn always_confirm(&self, c: &Conn, confirm: bool, confirm_prod: bool, what: &str) -> Result<(), EngineError> {
        if c.env() == "prod" {
            return self.write_guard(c, confirm_prod, &format!("prod({}) {what}: y/N", c.name), false);
        }
        if !confirm {
            return Err(EngineError::refused(REASON_CONFIRM, format!("{} {what}: y/N", c.name)));
        }
        Ok(())
    }

    // ─── 편집기 세션 ───

    /// 편집기 문장 하나(explain 이면 EXPLAIN 으로 감싼 것)를 안전장치를 거쳐 세션에서 실행한다.
    /// prod 쓰기는 잠금(R54) → 확인(R6) 순, 위험 문장(R25)은 환경과 상관없이 확인을 요구한다.
    /// 잠긴 prod 의 읽기는 READ ONLY 로 감싸 첫 단어 판정이 놓친 쓰기(CTE 안 쓰기·select into)를 서버가 거절하게 한다.
    async fn run(&self, r: &RunRequest, explain: Option<ExplainOpt>) -> Result<ResultSet, EngineError> {
        let c = self.conn(&r.conn, Kind::Pg)?;
        let stmts = split_statements(&r.sql);
        if stmts.len() != 1 {
            return Err(EngineError::usage(format!("문장 하나만 실행함 ({}개)", stmts.len())));
        }
        let (orig, note) = self.proj().expand_enc(&c, stmts[0]).map_err(EngineError::usage)?;
        // 판정은 감싸기 전 문장으로 한다(Go ui.go ctrl+e·alt+e). ANALYZE 없이 감싼 EXPLAIN 은 속 문장을 실행하지 않아 늘 읽기이고,
        // ANALYZE 로 감쌌거나 직접 쓴 EXPLAIN 이면 속 문장의 읽기·쓰기와 위험(R25)이 그대로다. 감싼 글로 가르면 ANALYZE 낱말에 걸린다
        let analyze = explain.as_ref().is_some_and(|o| o.analyze);
        let judged = explain.is_none() || analyze || first_sql_word(&orig) == "explain";
        let read_only = !judged || is_read_only_sql(&orig);
        let danger = if explain.is_none() || analyze {
            danger_of(&orig)
        } else {
            String::new()
        };
        let sent = match &explain {
            Some(opt) => explain_sql(&orig, opt),
            None => orig.clone(),
        };
        let (sql, args) = bind_params(&sent, &r.params)?;
        // 거절은 접속을 열기 전에 한다. 그래야 거절할 prod 에 연결 자체를 하지 않는다
        if c.env() == "prod" && !read_only {
            let w = first_sql_word(&orig);
            let tx_end = explain.is_none()
                && TX_END_WORDS.contains(&w.as_str())
                && !mask_sql(&orig).to_lowercase().split_whitespace().any(|t| t == "prepared");
            if !tx_end && self.locked(&c) {
                let hint = matches!(w.as_str(), "begin" | "start" | "set") || self.in_tx(&c).await;
                return Err(self.lock_refusal(&c, hint));
            }
        }
        // ANALYZE 는 문장을 실제로 실행하므로 읽기 전용이 아니면 모든 환경에서 먼저 묻는다(R9.2). prod 는 그다음 R6 확인이다
        if analyze && !read_only && !r.confirm_analyze {
            return Err(EngineError::refused(
                REASON_CONFIRM_ANALYZE,
                "EXPLAIN ANALYZE 는 문장을 실제로 실행합니다: y/N",
            ));
        }
        if c.env() == "prod" && !read_only && !r.confirm_prod {
            return Err(EngineError::refused(
                REASON_CONFIRM_PROD,
                format!("prod({})에 쓰기 실행: y/N", c.name),
            ));
        }
        if !danger.is_empty() && !r.confirm_danger {
            return Err(EngineError::refused(REASON_CONFIRM_DANGER, format!("위험: {danger}")));
        }
        let s = self.session(&c).await?;
        let ran = s.run(&sql, &args, read_only && self.locked(&c)).await;
        let mut rs = self.ran(&c, ran)?;
        rs.note = note;
        Ok(rs)
    }

    /// 세션 결과를 ResultSet 으로 바꾼다. 실패해도 트랜잭션이 열려 있으면 reason in_tx 로 화면이 알게 한다(R3.6).
    fn ran(&self, c: &Conn, ran: Ran) -> Result<ResultSet, EngineError> {
        let in_tx = ran.in_tx;
        let mut rs = ResultSet::from_query(ran.res.map_err(|e| {
            let mut e = self.run_err(c, &e);
            if in_tx {
                e.reason = Some("in_tx".into());
            }
            e
        })?);
        rs.in_tx = Some(in_tx);
        rs.origin = ran.origin;
        Ok(rs)
    }

    /// 편집 묶음을 반영한다(R15.2·R50). pk 가 테이블의 진짜 PK 가 아니면 거절한다(ctid·다른 열로 행을 찾지 않는다).
    async fn apply(&self, name: &str, cs: ChangeSet, confirm_prod: bool, what: &str) -> Result<AppliedResult, EngineError> {
        let c = self.conn(name, Kind::Pg)?;
        if cs.changes.is_empty() {
            return Err(EngineError::usage("no changes to apply"));
        }
        let ask = format!("prod({})에 {what} {}건: y/N", c.name, cs.changes.len());
        self.write_guard(&c, confirm_prod, &ask, false)?;
        // 반영과 같은 세션에서 찾아야 그 트랜잭션에서 만든 테이블·바꾼 PK 를 본다
        let session = self.session(&c).await?;
        let pk = session.primary_key(&cs.table).await.map_err(|e| self.run_err(&c, &e))?;
        if pk.is_empty() || pk != cs.pk {
            return Err(EngineError::usage(format!(
                "편집 묶음의 PK({})가 {} 의 PK({})와 다름",
                cs.pk.join(", "),
                cs.table.quoted(),
                pk.join(", ")
            )));
        }
        let ran = session.apply(&cs).await;
        let applied = ran.applied.clone().unwrap_or_default();
        Ok(AppliedResult {
            result: self.ran(&c, ran)?,
            applied,
            sql: changes_sql(&cs),
        })
    }

    async fn table_page(&self, r: &TablePageRequest) -> Result<TablePage, EngineError> {
        self.conn(&r.conn, Kind::Pg)?;
        if r.table.is_empty() {
            return Err(EngineError::usage("테이블 이름이 필요함"));
        }
        let schema = if r.schema.is_empty() { "public" } else { &r.schema };
        let rel = format!("{}.{}", quote_ident(schema), quote_ident(&r.table));
        let filter = r.filter.trim();
        let order = r.order_by.trim();
        check_single(filter)?;
        check_single(order)?;
        // 끝의 -- 주석이 뒤에 붙는 ORDER BY·LIMIT 을 삼키지 않도록 줄을 바꾼다(Go tableview)
        let line = |s: &str| if s.contains("--") { "\n" } else { "" };
        let where_clause = if filter.is_empty() {
            String::new()
        } else {
            format!(" WHERE {filter}{}", line(filter))
        };
        let limit = if r.limit == 0 { DEFAULT_PAGE } else { r.limit.min(MAX_ROWS as u64) };
        let mut sql = format!("SELECT * FROM {rel}{where_clause}");
        if !order.is_empty() {
            sql.push_str(&format!(" ORDER BY {order}{}", line(order)));
        }
        sql.push_str(&format!(" LIMIT {limit} OFFSET {}", r.offset));
        let count_sql = format!("SELECT count(*) FROM {rel}{where_clause}");
        // 칸마다는 문장 하나여도 합치면 둘이 될 수 있다(R16.4)
        if split_statements(&sql).len() != 1 || split_statements(&count_sql).len() != 1 {
            return Err(EngineError::usage("WHERE must be a single expression without a top-level ';'"));
        }
        let page = |sql: String| RunRequest {
            conn: r.conn.clone(),
            sql,
            ..Default::default()
        };
        let result = self.run(&page(sql.clone()), None).await?;
        let first_int = |rs: &ResultSet| rs.rows.first().and_then(|row| row.first()).and_then(|v| v.parse::<i64>().ok());
        let count = if r.count {
            first_int(&self.run(&page(count_sql), None).await?)
        } else {
            None
        };
        let estimate = if r.estimate {
            let est = format!(
                "SELECT reltuples::bigint FROM pg_class WHERE oid = {}::regclass",
                quote_literal(&rel)
            );
            first_int(&self.run(&page(est), None).await?)
        } else {
            None
        };
        Ok(TablePage {
            result,
            sql,
            count,
            estimate,
        })
    }

    /// F9·F10·F11(R17). COMMIT 은 prod 면 확인을 받고 잠금에는 막히지 않는다(R54.2 단서). BEGIN 은 잠긴 prod 에서 막는다.
    async fn tx(&self, name: &str, verb: &str, confirm_prod: bool) -> Result<TxState, EngineError> {
        let c = self.conn(name, Kind::Pg)?;
        let in_tx = self.in_tx(&c).await;
        match verb {
            "BEGIN" if c.env() == "prod" && self.locked(&c) => return Err(self.lock_refusal(&c, true)),
            "COMMIT" | "ROLLBACK" if !in_tx => return Err(EngineError::usage("열린 트랜잭션 없음")),
            "COMMIT" if c.env() == "prod" && !confirm_prod => {
                return Err(EngineError::refused(
                    REASON_CONFIRM_PROD,
                    format!("prod({})에 쓰기 실행: y/N", c.name),
                ));
            }
            _ => {}
        }
        let ran = self.session(&c).await?.run(verb, &[], false).await;
        let res = ran.res.map_err(|e| self.run_err(&c, &e))?;
        if verb == "COMMIT" && res.text == "ROLLBACK" {
            return Err(EngineError::run("COMMIT 이 ROLLBACK 으로 끝남(트랜잭션 오류 상태)"));
        }
        Ok(TxState {
            in_tx: ran.in_tx,
            text: res.text,
        })
    }

    // ─── Redis ───

    /// 접속의 Redis 연결로 f 를 돌린다. 칸마다 한 번에 하나다. 실행 중 취소되면 연결을 버리고 다음 요청에서 다시 연다.
    /// 세션 칸은 연결이 끊기면 SELECT 같은 상태를 잃은 것이라 다음 요청 한 번은 실행하지 않고 알린다(R4.2). 취소는 사용자가 한 일이라 알리지 않는다.
    async fn redis_do<T>(
        &self,
        c: &Conn,
        which: RedisUse,
        f: impl for<'a> FnOnce(&'a mut RedisConn) -> RedisJob<'a, T>,
    ) -> Result<T, EngineError> {
        let slots = if which == RedisUse::Session {
            &self.redis_session
        } else {
            &self.redis_pool
        };
        let slot = slots.lock().unwrap().entry(c.uuid.clone()).or_default().clone();
        let mut st = slot.lock().await;
        if std::mem::take(&mut st.running) {
            st.conn = None;
        }
        if std::mem::take(&mut st.lost) {
            return Err(EngineError::run(REDIS_SESSION_LOST));
        }
        let mut conn = match st.conn.take() {
            Some(r) => r,
            None => RedisConn::open(c).await.map_err(|e| self.run_err(c, &e))?,
        };
        st.running = true;
        let res = f(&mut conn).await;
        // 오류가 연결이 끊긴 탓이면 버린다. 서버 오류 응답이면 PING 이 되고 연결을 다시 쓴다
        let keep = res.is_ok() || conn.value(&["PING".into()]).await.is_ok();
        st.running = false;
        if keep {
            st.conn = Some(conn);
        } else if which == RedisUse::Session {
            st.lost = true;
        }
        res.map_err(|e| self.run_err(c, &e))
    }

    /// Redis 쓰기의 문이다. prod 는 잠금·확인(R54·R6), 위험 명령(R25)은 환경과 상관없이 확인이다.
    fn redis_guard(&self, c: &Conn, cmds: &[Vec<String>], confirm_prod: bool, confirm_danger: bool) -> Result<(), EngineError> {
        if let Some(msg) = cmds.iter().find_map(|a| keys_refusal(a)) {
            return Err(EngineError::refused(REASON_KEYS, msg));
        }
        if !cmds.iter().all(|a| is_read_only_redis(a)) {
            self.write_guard(c, confirm_prod, &format!("prod({})에 쓰기 실행: y/N", c.name), false)?;
        }
        let danger: Vec<String> = cmds.iter().map(|a| danger_of_redis(a)).filter(|d| !d.is_empty()).collect();
        if !danger.is_empty() && !confirm_danger {
            return Err(EngineError::refused(REASON_CONFIRM_DANGER, format!("위험: {}", danger.join(", "))));
        }
        Ok(())
    }

    async fn redis_run(&self, name: &str, args: Vec<String>, confirm_prod: bool, confirm_danger: bool) -> Result<ResultSet, EngineError> {
        let c = self.conn(name, Kind::Redis)?;
        if args.is_empty() {
            return Err(EngineError::usage("실행할 명령이 없음"));
        }
        self.redis_guard(&c, std::slice::from_ref(&args), confirm_prod, confirm_danger)?;
        let r = self
            .redis_do(&c, RedisUse::Session, |r| Box::pin(async move { r.run(&args).await }))
            .await?;
        Ok(ResultSet::from_query(r))
    }

    /// 값 편집 하나를 명령으로 만들어 세션에서 첫 오류까지 차례로 실행한다(Go redisEditKey·afterRedisWrite).
    async fn redis_edit(&self, name: &str, e: RedisEdit, confirm_prod: bool, confirm_danger: bool) -> Result<RedisEditResult, EngineError> {
        let c = self.conn(name, Kind::Redis)?;
        let cmds = redis::edit_commands(&e).map_err(EngineError::usage)?;
        if cmds.is_empty() {
            return Ok(RedisEditResult {
                results: Vec::new(),
                key: e.key,
                status: "바뀐 것이 없음".into(),
            });
        }
        self.redis_guard(&c, &cmds, confirm_prod, confirm_danger)?;
        let n = cmds.len();
        let results = self
            .redis_do(&c, RedisUse::Session, |r| {
                Box::pin(async move {
                    let mut out = Vec::with_capacity(n);
                    for (i, a) in cmds.iter().enumerate() {
                        match r.run(a).await {
                            Ok(res) => out.push(ResultSet::from_query(res)),
                            Err(e) if n > 1 => return Err(Error::msg(format!("statement {} of {n}: {e}", i + 1))),
                            Err(e) => return Err(e),
                        }
                    }
                    Ok(out)
                })
            })
            .await?;
        let (key, status) = match e.op.as_str() {
            "delete_key" => (String::new(), "키를 지움"),
            // RENAMENX 가 0 이면 같은 이름의 키가 있어 바뀌지 않았다
            "rename" if results.first().is_some_and(|r| r.text.trim() == "0") => (e.key, "같은 이름의 키가 있어 바꾸지 않음"),
            "rename" => (e.text, "반영함"),
            _ => (e.key, "반영함"),
        };
        Ok(RedisEditResult {
            results,
            key,
            status: status.into(),
        })
    }
}

/// 접속 창의 칸을 접속으로 바꾼다. 종류 글이 틀리면 validate_conn 이 거절한다.
fn conn_of_fields(f: ConnFields) -> Conn {
    Conn {
        kind: Kind::parse(&f.kind),
        name: f.name,
        group: f.group,
        host: f.host,
        port: f.port,
        db: f.db,
        user: f.user,
        password: f.password,
        url: f.url,
        auth: f.auth,
        region: f.region,
        profile: f.profile,
        workgroup: f.workgroup,
        output: f.output,
        sslmode: f.sslmode,
        ..Default::default()
    }
}

fn json<T: Serialize>(v: T) -> Result<Value, EngineError> {
    serde_json::to_value(v).map_err(|e| EngineError::run(e.to_string()))
}

/// `:이름` 을 $n 으로 바꾸고 값을 순서대로 모은다(R21). 값이 없는 이름은 사용법 오류다.
fn bind_params(sql: &str, params: &std::collections::BTreeMap<String, String>) -> Result<(String, Vec<Option<String>>), EngineError> {
    let (bound, names) = idb_core::params::bind(sql);
    let mut args = Vec::with_capacity(names.len());
    for n in &names {
        match params.get(n) {
            Some(v) => args.push(Some(v.clone())),
            None => return Err(EngineError::usage(format!("파라미터 값이 없음: {n}"))),
        }
    }
    Ok((bound, args))
}

fn table_ref(schema: String, table: String) -> TableRef {
    // describe·ddl 은 스키마가 없으면 스키마.테이블 꼴을 나누고, 그것도 없으면 public 이다(CLI table_ref 와 같다)
    if !schema.is_empty() {
        return TableRef { schema, table };
    }
    let (s, t) = table.split_once('.').unwrap_or(("public", &table));
    TableRef {
        schema: s.into(),
        table: t.into(),
    }
}

/// WHERE·ORDER BY 글이 문장 하나 안의 식인지 본다(Go tableview.CheckWhere). 빈 글은 통과다.
fn check_single(s: &str) -> Result<(), EngineError> {
    if s.is_empty() {
        return Ok(());
    }
    let stmts = split_statements(s);
    if stmts.len() != 1 || stmts[0] != s {
        return Err(EngineError::usage("WHERE must be a single expression without a top-level ';'"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// PING·HELLO 에 답하고, 첫 GET 에서는 연결을 끊고 그 뒤 GET 에는 "x" 를 답하는 Redis 흉내다.
    async fn flaky_redis() -> u16 {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let gets = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        tokio::spawn(async move {
            loop {
                let (mut s, _) = l.accept().await.unwrap();
                let gets = gets.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    while let Ok(n) = s.read(&mut buf).await {
                        if n == 0 {
                            return;
                        }
                        let t = String::from_utf8_lossy(&buf[..n]).to_lowercase();
                        let reply: &[u8] = if t.contains("hello") {
                            b"-ERR unknown command 'hello'\r\n"
                        } else if t.contains("ping") {
                            b"+PONG\r\n"
                        } else if gets.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                            return; // 첫 GET: 끊는다
                        } else {
                            b"$1\r\nx\r\n"
                        };
                        if s.write_all(reply).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        port
    }

    #[tokio::test]
    async fn Redis_세션이_끊기면_다음_한_번은_알리고_그다음은_새_연결로_돈다() {
        let port = flaky_redis().await;
        let proj = Project {
            dir: Default::default(),
            conns: vec![Conn {
                name: "r-local".into(),
                uuid: "u1".into(),
                kind: Some(Kind::Redis),
                host: "127.0.0.1".into(),
                port,
                ..Default::default()
            }],
            mapping: Default::default(),
            warnings: vec![],
            aes_env: Default::default(),
        };
        let e = LocalEngine::new(proj, None);
        let get = || e.redis_run("r-local", vec!["GET".into(), "a".into()], false, false);
        assert_eq!(get().await.unwrap_err().code, CODE_RUN, "끊긴 명령 자체는 실행 오류");
        assert_eq!(get().await.unwrap_err().error, REDIS_SESSION_LOST);
        assert_eq!(get().await.unwrap().text, "x");
        // 풀 칸(값 보기·트리)은 알리지 않고 다시 붙는다
        assert!(
            e.redis_do(&e.conn("r-local", Kind::Redis).unwrap(), RedisUse::Pool, |r| Box::pin(async move {
                r.run(&["GET".into(), "a".into()]).await
            }))
            .await
            .is_ok()
        );
    }

    // ─── 복호화·전체 내보내기·접속 편집 ───

    const KEY: &str = "0123456789abcdef0123456789abcdef";

    fn one_conn(c: Conn) -> LocalEngine {
        let proj = Project {
            dir: Default::default(),
            conns: vec![c],
            mapping: Default::default(),
            warnings: vec![],
            aes_env: [("GRP_AES_KEY".to_string(), KEY.to_string())].into(),
        };
        LocalEngine::new(proj, None)
    }

    fn pg_conn(name: &str, group: &str, port: u16) -> Conn {
        Conn {
            name: name.into(),
            uuid: format!("u-{name}"),
            group: group.into(),
            kind: Some(Kind::Pg),
            host: "127.0.0.1".into(),
            port,
            user: "u".into(),
            db: "d".into(),
            sslmode: "disable".into(),
            ..Default::default()
        }
    }

    #[test]
    fn 복호화는_암호문_꼴_칸만_풀고_못_푼_칸은_세고_키는_내지_않는다() {
        let e = one_conn(pg_conn("a", "grp", 1));
        let enc = statcrypt::encrypt("홍길동", KEY, "00112233445566778899aabbccddeeff").unwrap();
        let other = statcrypt::encrypt("x", "another-key", "00112233445566778899aabbccddeeff").unwrap();
        let r = e.decrypt("a", vec![enc, "plain".into(), "NULL".into(), other]).unwrap();
        assert_eq!(r.values, [Some("홍길동".to_string()), None, None, None]);
        assert_eq!(r.failed, 1);
        let e = one_conn(pg_conn("a", "", 1));
        assert_eq!(
            e.decrypt("a", vec![]).unwrap_err().error,
            "접속 그룹을 몰라 복호화 키를 고를 수 없음"
        );
        let e = one_conn(pg_conn("a", "app-x", 1));
        assert_eq!(e.decrypt("a", vec![]).unwrap_err().error, "APP_X_AES_KEY 가 .env 에 없음");
    }

    /// 시작 메시지 뒤 simple query 마다 답하는 PostgreSQL 흉내다. COPY 는 rows 행(각 `n,<pad>\n`)을 보낸다.
    async fn fake_pg(rows: usize, pad: usize) -> (u16, Arc<Mutex<Vec<String>>>) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let seen2 = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = l.accept().await else { return };
                let seen = seen2.clone();
                tokio::spawn(async move {
                    let msg = |tag: u8, body: &[u8]| {
                        let mut m = vec![tag];
                        m.extend_from_slice(&(body.len() as u32 + 4).to_be_bytes());
                        m.extend_from_slice(body);
                        m
                    };
                    let mut len = [0u8; 4];
                    if s.read_exact(&mut len).await.is_err() {
                        return;
                    }
                    let mut rest = vec![0u8; u32::from_be_bytes(len) as usize - 4];
                    let _ = s.read_exact(&mut rest).await;
                    let _ = s.write_all(&[msg(b'R', &0u32.to_be_bytes()), msg(b'Z', b"I")].concat()).await;
                    let mut status = b'I';
                    loop {
                        let mut head = [0u8; 5];
                        if s.read_exact(&mut head).await.is_err() {
                            return;
                        }
                        let mut body = vec![0u8; u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize - 4];
                        if s.read_exact(&mut body).await.is_err() || head[0] != b'Q' {
                            return;
                        }
                        let q = String::from_utf8_lossy(&body[..body.len() - 1]).into_owned();
                        seen.lock().unwrap().push(q.clone());
                        let mut out = Vec::new();
                        if q.starts_with("COPY") {
                            out.extend(msg(b'H', &[0, 0, 0]));
                            out.extend(msg(b'd', b"n,v\n"));
                            for i in 0..rows {
                                out.extend(msg(b'd', format!("{i},{}\n", "x".repeat(pad)).as_bytes()));
                            }
                            out.extend(msg(b'c', b""));
                            out.extend(msg(b'C', format!("COPY {rows}\0").as_bytes()));
                        } else {
                            if q.starts_with("BEGIN") {
                                status = b'T';
                            } else if q.starts_with("ROLLBACK") {
                                status = b'I';
                            }
                            out.extend(msg(b'C', b"OK\0"));
                        }
                        out.extend(msg(b'Z', &[status]));
                        if s.write_all(&out).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        (port, seen)
    }

    #[tokio::test]
    async fn 전체_내보내기는_조각으로_다_받고_행_수로_끝난다() {
        let (port, seen) = fake_pg(3000, 1000).await;
        let e = one_conn(pg_conn("a", "grp", port));
        let st = e.export_start("a", "select * from t;").await.unwrap();
        let (mut data, mut chunks) = (Vec::new(), 0);
        let end = loop {
            let c = e.export_read(st.export).await.unwrap();
            if c.done {
                break c;
            }
            assert!(c.data.len() <= idb_sources::pg::EXPORT_CHUNK + 1100);
            data.extend(c.data);
            chunks += 1;
        };
        assert_eq!(end.rows, 3000);
        assert_eq!(end.bytes, data.len() as u64);
        assert!(chunks >= 3, "{chunks}");
        assert!(data.starts_with(b"n,v\n0,x") && data.ends_with(b"\n"));
        assert_eq!(data.iter().filter(|&&b| b == b'\n').count(), 3001);
        // 끝난 id 는 사라진다
        assert_eq!(e.export_read(st.export).await.unwrap_err().code, CODE_USAGE);
        let q = seen.lock().unwrap().clone();
        assert!(q.contains(&"BEGIN READ ONLY".to_string()), "{q:?}");
        assert!(
            q.contains(&"COPY (select * from t\n) TO STDOUT WITH (FORMAT csv, HEADER)".to_string()),
            "{q:?}"
        );
        assert_eq!(q.last().map(String::as_str), Some("ROLLBACK"));
    }

    async fn cancel(e: &LocalEngine, export: u64) -> Value {
        e.handle(Request::ExportCancel { export }).await.unwrap()
    }

    #[tokio::test]
    async fn 전체_내보내기는_취소하면_id_가_사라진다() {
        let (port, _) = fake_pg(20_000, 1000).await;
        let e = one_conn(pg_conn("a", "grp", port));
        let st = e.export_start("a", "table t").await.unwrap();
        assert!(!e.export_read(st.export).await.unwrap().done);
        assert_eq!(cancel(&e, st.export).await, serde_json::json!({"canceled": true}));
        assert_eq!(e.export_read(st.export).await.unwrap_err().code, CODE_USAGE);
        assert_eq!(cancel(&e, st.export).await, serde_json::json!({"canceled": false}));
    }

    #[tokio::test]
    async fn 전체_내보내기는_읽기_SELECT_문장_하나만_받는다() {
        let e = one_conn(pg_conn("a", "grp", 1));
        let err = |sql: &'static str| {
            let e = &e;
            async move { e.export_start("a", sql).await.unwrap_err().error }
        };
        assert_eq!(err("delete from t").await, "읽기 문장만 전체 내보내기");
        assert_eq!(err("show search_path").await, "SELECT 문만 전체 내보내기");
        assert_eq!(
            err("select 1; select 2").await,
            "문장 하나만 전체 내보내기 (커서 문장으로 다시 실행)"
        );
        assert_eq!(
            err("select * from t where id = :id").await,
            "파라미터(:name·$n)가 있는 문장은 전체 내보내지 않음"
        );
        assert_eq!(err("").await, "전체 내보낼 문장이 없음");
        // 부수 효과 함수는 READ ONLY 트랜잭션 안에서도 돌아 접속하기 전에 막는다(read_query 와 같은 검사)
        let refused = e.export_start("a", "select pg_terminate_backend(1)").await.unwrap_err();
        assert_eq!((refused.code, refused.reason.as_deref()), (CODE_REFUSED, Some(REASON_READ_ONLY)));
        assert!(refused.error.contains("pg_terminate_backend"), "{}", refused.error);
    }

    const DATA_SOURCES: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<project version="4">
  <component name="DataSourceManagerImpl">
    <data-source name="dg-stg" group="grp" uuid="u-dg"><driver-ref>postgresql</driver-ref><jdbc-url>jdbc:postgresql://127.0.0.1:1/app</jdbc-url></data-source>
  </component>
</project>"#;

    fn conn_project() -> (tempfile::TempDir, LocalEngine) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".idea")).unwrap();
        std::fs::write(dir.path().join(".idea/dataSources.xml"), DATA_SOURCES).unwrap();
        let proj = idb_core::project::load_project(dir.path()).unwrap();
        (dir, LocalEngine::new(proj, None))
    }

    fn fields(name: &str, password: &str) -> ConnFields {
        ConnFields {
            name: name.into(),
            group: "grp".into(),
            kind: "postgresql".into(),
            host: "127.0.0.1".into(),
            port: 1,
            db: "app".into(),
            user: "ro".into(),
            password: password.into(),
            sslmode: "disable".into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn 접속을_더하고_고치고_지우면_파일과_접속_목록이_바로_바뀐다() {
        let (dir, e) = conn_project();
        let saved = e.conn_save("", fields("db-stg", "pw-secret-1")).await.unwrap();
        assert_eq!((saved.conn.name.as_str(), saved.conn.source.as_str()), ("db-stg", "db"));
        let env = std::fs::read_to_string(dir.path().join(".env")).unwrap();
        assert!(env.contains("PG_DB_STG_PASSWORD=pw-secret-1"), "{env}");
        assert!(e.project().conns.iter().any(|c| c.name == "db-stg"));
        let got = e.conn_get("db-stg").unwrap();
        assert!(got.has_password && got.password.is_empty() && got.user == "ro");
        assert!(!serde_json::to_string(&got).unwrap().contains("pw-secret-1"));
        // 이름을 바꾸고 비밀번호를 비우면 .env 값을 새 키로 옮긴다
        let mut f = fields("db-stg2", "");
        f.db = "app2".into();
        e.conn_save("db-stg", f).await.unwrap();
        let env = std::fs::read_to_string(dir.path().join(".env")).unwrap();
        assert!(
            env.contains("PG_DB_STG2_PASSWORD=pw-secret-1") && !env.contains("PG_DB_STG_PASSWORD"),
            "{env}"
        );
        let names: Vec<String> = e.project().conns.into_iter().map(|c| c.name).collect();
        assert!(
            names.contains(&"db-stg2".to_string()) && !names.contains(&"db-stg".to_string()),
            "{names:?}"
        );
        // DataGrip 접속은 고치지 않는다
        assert_eq!(
            e.conn_save("dg-stg", fields("dg-stg", "")).await.unwrap_err().error,
            connstore::DATAGRIP_REFUSAL
        );
        assert_eq!(e.conn_delete("dg-stg").await.unwrap_err().error, connstore::DATAGRIP_REFUSAL);
        assert_eq!(
            e.conn_save("", fields("bad name", "")).await.unwrap_err().error,
            "접속 이름은 영숫자·-·_ 만 받음"
        );
        // 폴더만 옮긴다
        e.conn_folder(&["dg-stg".into()], "ops").await.unwrap();
        assert_eq!(e.project().conns.iter().find(|c| c.name == "dg-stg").unwrap().folder, "ops");
        e.conn_delete("db-stg2").await.unwrap();
        assert!(!e.project().conns.iter().any(|c| c.name == "db-stg2"));
        let env = std::fs::read_to_string(dir.path().join(".env")).unwrap();
        assert!(!env.contains("pw-secret-1"), "{env}");
    }

    #[tokio::test]
    async fn 접속_시험_오류에는_받은_비밀번호가_없다() {
        let (_dir, e) = conn_project();
        let err = e
            .conn_test(String::new().as_str(), Some(fields("x", "pw-secret-2")), "")
            .await
            .unwrap_err();
        assert!(err.error.starts_with("접속 실패: "), "{err:?}");
        assert!(!err.error.contains("pw-secret-2"));
        let mut f = fields("x", "");
        f.kind = "mysql".into();
        assert_eq!(e.conn_test("", Some(f), "").await.unwrap_err().code, CODE_USAGE);
    }

    #[tokio::test]
    async fn serve_에서는_접속_파일을_쓰지_않고_비밀번호를_받지_않는다() {
        let (dir, e) = conn_project();
        e.set_serving();
        let refused = |r: EngineError| (r.code, r.reason);
        let want = (CODE_REFUSED, Some(REASON_REMOTE_CONN_EDIT.to_string()));
        assert_eq!(refused(e.conn_save("", fields("db-stg", "pw")).await.unwrap_err()), want);
        assert_eq!(refused(e.conn_delete("dg-stg").await.unwrap_err()), want);
        assert_eq!(refused(e.conn_get("dg-stg").unwrap_err()), want);
        assert_eq!(refused(e.conn_test("", Some(fields("x", "pw")), "").await.unwrap_err()), want);
        // 빈 비밀번호 + 저장된 접속 이름 + 고른 호스트로 저장된 비밀번호를 남의 호스트에 보내지 못한다
        let mut evil = fields("dg-stg", "");
        evil.host = "attacker.example".into();
        assert_eq!(refused(e.conn_test("", Some(evil), "dg-stg").await.unwrap_err()), want);
        // 저장된 접속 이름으로만 시험하고 폴더 옮기기는 된다
        assert_eq!(e.conn_test("dg-stg", None, "").await.unwrap_err().code, CODE_RUN);
        e.conn_folder(&["dg-stg".into()], "ops").await.unwrap();
        assert!(!dir.path().join(".dbtui/connections.json").exists());
    }

    #[tokio::test]
    async fn read_query_는_쓰기를_접속_전에_거절하고_project_는_리전_프로필을_싣는다() {
        let proj = Project {
            dir: Default::default(),
            conns: vec![
                Conn {
                    name: "app-local".into(),
                    uuid: "p".into(),
                    kind: Some(Kind::Pg),
                    host: "127.0.0.1".into(),
                    port: 1,
                    ..Default::default()
                },
                Conn {
                    name: "aws-local".into(),
                    uuid: "w".into(),
                    kind: Some(Kind::Aws),
                    region: "us-east-1".into(),
                    profile: "infra".into(),
                    ..Default::default()
                },
            ],
            mapping: Default::default(),
            warnings: vec![],
            aes_env: Default::default(),
        };
        let e = LocalEngine::new(proj, None);
        let read = |sql: &str| {
            e.handle(Request::ReadQuery {
                conn: "app-local".into(),
                sql: sql.into(),
                limit: 0,
            })
        };
        for sql in ["delete from t", "select pg_terminate_backend(1)"] {
            let err = read(sql).await.unwrap_err();
            assert_eq!((err.code, err.reason.as_deref()), (CODE_REFUSED, Some(REASON_READ_ONLY)), "{sql}");
        }
        // 읽기는 접속을 시도한다(포트 1 이라 실행 오류)
        assert_eq!(read("select 1").await.unwrap_err().code, CODE_RUN);
        let p = e.project();
        assert_eq!((p.conns[1].region.as_str(), p.conns[1].profile.as_str()), ("us-east-1", "infra"));
        assert!(
            !serde_json::to_string(&p.conns[0]).unwrap().contains("region"),
            "빈 리전은 싣지 않는다"
        );
    }

    #[test]
    fn WHERE_와_정렬은_최상위_세미콜론을_거절한다() {
        assert!(check_single("").is_ok());
        assert!(check_single("name = 'a;b'").is_ok());
        assert!(check_single("1=1; drop table t").is_err());
    }

    #[test]
    fn 스키마_없는_테이블은_점으로_나누고_없으면_public_이다() {
        let t = table_ref(String::new(), "s.t".into());
        assert_eq!((t.schema.as_str(), t.table.as_str()), ("s", "t"));
        let t = table_ref(String::new(), "t".into());
        assert_eq!((t.schema.as_str(), t.table.as_str()), ("public", "t"));
    }
}
