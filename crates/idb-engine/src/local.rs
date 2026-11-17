//! 이 기계의 프로젝트·접속으로 요청을 처리한다. serve 는 이것을 표준 입출력에 잇는다.
//! 안전장치(읽기 전용·prod 확인·잠금 R54·위험 문장 R25·가리기)는 모두 여기서 정한다. 원격 클라이언트는 확인 여부만 보낸다.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use idb_core::project::{Conn, Kind, Project, SOURCE_DB, folder_of};
use idb_core::redact::redact;
use idb_core::result::{MAX_ROWS, QueryResult, cap_cell};
use idb_core::sqlsafe::{
    ExplainOpt, danger_of, danger_of_redis, explain_sql, first_sql_word, is_read_only_redis, is_read_only_sql, mask_sql, quote_ident,
    quote_literal, split_statements,
};
use idb_core::textutil::printable;
use idb_sources::redis::{RedisConn, Value as RValue};
use idb_sources::{Clients, Error, Session, TableRef};
use serde::Serialize;
use serde_json::Value;

use crate::protocol::*;

/// 테이블 보기 기본 페이지(R16.1)
const DEFAULT_PAGE: u64 = 200;
const DEFAULT_UNLOCK_MIN: u64 = 10;
const MAX_UNLOCK_MIN: u64 = 24 * 60;
/// Redis string 값은 앞 1MiB 만 보여 준다(E63)
const MAX_STRING_BYTES: i64 = 1 << 20;
const TX_END_WORDS: &[&str] = &["commit", "rollback", "end", "abort"];

/// serve 가 CLI 하위 명령을 돌리는 함수다. idb-cli 가 넘긴다(엔진이 CLI 에 기대지 않게).
pub type CliHandler = Arc<dyn Fn(CliRequest) -> CliResponse + Send + Sync>;

type RedisSlot = Arc<tokio::sync::Mutex<RedisState>>;

#[derive(Default)]
struct RedisState {
    conn: Option<RedisConn>,
    /// 명령 중에 퓨처가 버려졌다(취소). 연결은 함께 버려졌다
    running: bool,
}

pub struct LocalEngine {
    proj: Project,
    clients: Clients,
    sessions: tokio::sync::Mutex<HashMap<String, Arc<Session>>>,
    redis: Mutex<HashMap<String, RedisSlot>>,
    /// prod 접속마다 열린 끝 시각(R54.2). 없으면 잠김
    unlocked: Mutex<HashMap<String, Instant>>,
    cli: Option<CliHandler>,
}

impl LocalEngine {
    pub fn new(proj: Project, cli: Option<CliHandler>) -> LocalEngine {
        LocalEngine {
            proj,
            clients: Clients::new(),
            sessions: Default::default(),
            redis: Default::default(),
            unlocked: Default::default(),
            cli,
        }
    }

    /// 세션(열린 트랜잭션은 서버가 되돌린다)과 풀을 닫는다.
    pub async fn close(&self) {
        let sessions: Vec<Arc<Session>> = self.sessions.lock().await.drain().map(|(_, s)| s).collect();
        for s in sessions {
            s.close().await;
        }
        self.clients.close().await;
    }

    /// 요청 하나를 처리한다. hello·cancel 은 serve 가 처리한다.
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
                let names = self.pg(c, |pg| async move { pg.find_schemas().await }).await?;
                json(names)
            }
            Request::Tables { conn, schema } => {
                let c = self.conn(&conn, Kind::Pg)?;
                let schema = if schema.is_empty() { "public".to_string() } else { schema };
                json(self.pg(c, |pg| async move { pg.find_tables(&schema).await }).await?)
            }
            Request::Describe { conn, schema, table } => {
                let c = self.conn(&conn, Kind::Pg)?;
                let t = table_ref(schema, table);
                let r = self.pg(c, |pg| async move { pg.describe_table(&t).await }).await?;
                json(ResultSet::from_query(r))
            }
            Request::Ddl { conn, schema, table } => {
                let c = self.conn(&conn, Kind::Pg)?;
                let t = table_ref(schema, table);
                json(self.pg(c, |pg| async move { pg.table_ddl(&t).await }).await?)
            }
            Request::Search { conn, schema, text } => {
                let c = self.conn(&conn, Kind::Pg)?;
                if text.is_empty() {
                    return Err(EngineError::usage("찾을 글이 필요함"));
                }
                let (r, skipped) = self.pg(c, |pg| async move { pg.search_data(&schema, &text).await }).await?;
                let mut rs = ResultSet::from_query(r);
                if !skipped.is_empty() {
                    rs.note = self.clean(c, &format!("건너뛴 테이블: {}", skipped.join(", ")));
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
                    in_tx: self.in_tx(c).await,
                    text: String::new(),
                })
            }
            Request::ProdLock { conn, unlock } => json(self.prod_lock(&conn, unlock)?),
            Request::RedisScan { conn, pattern } => {
                let c = self.conn(&conn, Kind::Redis)?;
                let pattern = if pattern.is_empty() { "*".to_string() } else { pattern };
                json(self.redis_do(c, |r| Box::pin(scan_keys(r, pattern))).await?)
            }
            Request::RedisValue { conn, key } => {
                let c = self.conn(&conn, Kind::Redis)?;
                json(self.redis_do(c, |r| Box::pin(get_value(r, key))).await?)
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
        }
    }

    fn project(&self) -> ProjectInfo {
        ProjectInfo {
            dir: self.proj.dir.to_string_lossy().into_owned(),
            conns: self
                .proj
                .conns
                .iter()
                .map(|c| ConnSummary {
                    name: c.name.clone(),
                    group: c.group.clone(),
                    folder: folder_of(c).into(),
                    kind: c.kind_str().into(),
                    env: c.env().into(),
                    host: if c.host.is_empty() { c.url.clone() } else { c.host.clone() },
                    db: c.db.clone(),
                    source: if c.source == SOURCE_DB { SOURCE_DB } else { "datagrip" }.into(),
                })
                .collect(),
            warnings: self.proj.warnings.iter().map(|w| self.clean_all(w)).collect(),
        }
    }

    // ─── 접속·오류 ───

    fn conn(&self, name: &str, kind: Kind) -> Result<&Conn, EngineError> {
        if name.is_empty() {
            return Err(EngineError::usage("접속 이름이 필요함"));
        }
        let c = self
            .proj
            .conn_by_name(name)
            .ok_or_else(|| EngineError::usage(format!("접속 없음: {name}")))?;
        if c.kind != Some(kind) {
            return Err(EngineError::usage(format!("{name} 는 {kind} 접속이 아님")));
        }
        Ok(c)
    }

    /// 내보낼 글로 만든다. 서버가 비밀번호·키를 되돌려줘도 나오지 않게 그 접속의 비밀번호와 모든 AES 키·IV 를 지운다(CLI err_text 와 같다).
    fn clean(&self, c: &Conn, msg: &str) -> String {
        let mut msg = redact(msg, &c.password);
        for v in self.proj.aes_env.values() {
            msg = redact(&msg, v);
        }
        printable(&msg, true)
    }

    /// 접속을 모를 때(프로젝트 경고)는 모든 비밀번호를 지운다.
    fn clean_all(&self, msg: &str) -> String {
        let mut msg = msg.to_string();
        for c in &self.proj.conns {
            msg = redact(&msg, &c.password);
        }
        for v in self.proj.aes_env.values() {
            msg = redact(&msg, v);
        }
        printable(&msg, true)
    }

    fn run_err(&self, c: &Conn, e: &Error) -> EngineError {
        EngineError::run(self.clean(c, &e.to_string()))
    }

    async fn pg<T, F, Fut>(&self, c: &Conn, f: F) -> Result<T, EngineError>
    where
        F: FnOnce(idb_sources::Pg) -> Fut,
        Fut: std::future::Future<Output = Result<T, Error>>,
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
            .proj
            .conn_by_name(name)
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
        Ok(self.lock_state(c))
    }

    fn lock_refusal(&self, c: &Conn, tx_hint: bool) -> EngineError {
        let mut msg = format!("prod({}) 쓰기 잠김 — {}분 열기", c.name, Self::unlock_min());
        if tx_hint {
            msg.push_str(" · 트랜잭션 끝내기는 F10/F11");
        }
        EngineError::refused(REASON_PROD_LOCKED, msg)
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
        let (mut sent, note) = self.proj.expand_enc(c, stmts[0]).map_err(EngineError::usage)?;
        if let Some(opt) = &explain {
            sent = explain_sql(&sent, opt);
        }
        let (sql, names) = idb_core::params::bind(&sent);
        let mut args = Vec::with_capacity(names.len());
        for n in &names {
            match r.params.get(n) {
                Some(v) => args.push(Some(v.clone())),
                None => return Err(EngineError::usage(format!("파라미터 값이 없음: {n}"))),
            }
        }
        let read_only = is_read_only_sql(&sent);
        // 거절은 접속을 열기 전에 한다. 그래야 거절할 prod 에 연결 자체를 하지 않는다
        if c.env() == "prod" && !read_only {
            let w = first_sql_word(&sent);
            let tx_end = TX_END_WORDS.contains(&w.as_str()) && !mask_sql(&sent).to_lowercase().split_whitespace().any(|t| t == "prepared");
            if !tx_end && self.locked(c) {
                let hint = matches!(w.as_str(), "begin" | "start" | "set") || self.in_tx(c).await;
                return Err(self.lock_refusal(c, hint));
            }
            if !r.confirm_prod {
                return Err(EngineError::refused(
                    REASON_CONFIRM_PROD,
                    format!("prod({})에 쓰기 실행: y/N", c.name),
                ));
            }
        }
        let danger = danger_of(&sent);
        if !danger.is_empty() && !r.confirm_danger {
            return Err(EngineError::refused(REASON_CONFIRM_DANGER, format!("위험: {danger}")));
        }
        let s = self.session(c).await?;
        let ran = s.run(&sql, &args, read_only && self.locked(c)).await;
        let mut rs = ResultSet::from_query(ran.res.map_err(|e| {
            let mut e = self.run_err(c, &e);
            // 실패해도 트랜잭션이 열려 있으면 화면이 알게 한다(R3.6)
            if ran.in_tx {
                e.reason = Some("in_tx".into());
            }
            e
        })?);
        rs.in_tx = Some(ran.in_tx);
        rs.note = note;
        Ok(rs)
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
        let in_tx = self.in_tx(c).await;
        match verb {
            "BEGIN" if c.env() == "prod" && self.locked(c) => return Err(self.lock_refusal(c, true)),
            "COMMIT" | "ROLLBACK" if !in_tx => return Err(EngineError::usage("열린 트랜잭션 없음")),
            "COMMIT" if c.env() == "prod" && !confirm_prod => {
                return Err(EngineError::refused(
                    REASON_CONFIRM_PROD,
                    format!("prod({})에 쓰기 실행: y/N", c.name),
                ));
            }
            _ => {}
        }
        let ran = self.session(c).await?.run(verb, &[], false).await;
        let res = ran.res.map_err(|e| self.run_err(c, &e))?;
        if verb == "COMMIT" && res.text == "ROLLBACK" {
            return Err(EngineError::run("COMMIT 이 ROLLBACK 으로 끝남(트랜잭션 오류 상태)"));
        }
        Ok(TxState {
            in_tx: ran.in_tx,
            text: res.text,
        })
    }

    // ─── Redis ───

    /// 접속의 Redis 연결로 f 를 돌린다. 한 번에 하나다. 실행 중 취소되면 연결을 버리고 다음 요청에서 다시 연다.
    async fn redis_do<T>(
        &self,
        c: &Conn,
        f: impl for<'a> FnOnce(&'a mut RedisConn) -> std::pin::Pin<Box<dyn Future<Output = Result<T, Error>> + Send + 'a>>,
    ) -> Result<T, EngineError> {
        let slot = self.redis.lock().unwrap().entry(c.uuid.clone()).or_default().clone();
        let mut st = slot.lock().await;
        if std::mem::take(&mut st.running) {
            st.conn = None;
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
        }
        res.map_err(|e| self.run_err(c, &e))
    }

    async fn redis_run(&self, name: &str, args: Vec<String>, confirm_prod: bool, confirm_danger: bool) -> Result<ResultSet, EngineError> {
        let c = self.conn(name, Kind::Redis)?;
        if args.is_empty() {
            return Err(EngineError::usage("실행할 명령이 없음"));
        }
        if c.env() == "prod" && !is_read_only_redis(&args) {
            if self.locked(c) {
                return Err(self.lock_refusal(c, false));
            }
            if !confirm_prod {
                return Err(EngineError::refused(
                    REASON_CONFIRM_PROD,
                    format!("prod({})에 쓰기 실행: y/N", c.name),
                ));
            }
        }
        let danger = danger_of_redis(&args);
        if !danger.is_empty() && !confirm_danger {
            return Err(EngineError::refused(REASON_CONFIRM_DANGER, format!("위험: {danger}")));
        }
        let r = self.redis_do(c, |r| Box::pin(async move { r.run(&args).await })).await?;
        Ok(ResultSet::from_query(r))
    }
}

use std::future::Future;

fn json<T: Serialize>(v: T) -> Result<Value, EngineError> {
    serde_json::to_value(v).map_err(|e| EngineError::run(e.to_string()))
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

// ─── Redis 읽기 (Go GetKeys·GetValue) ───

fn text(v: &RValue) -> String {
    match v {
        RValue::Str(b) => String::from_utf8_lossy(b).into_owned(),
        RValue::Int(n) => n.to_string(),
        other => idb_sources::redis::format_cell(other),
    }
}

fn cell(v: &RValue) -> String {
    match v {
        RValue::Str(b) => cap_cell(b),
        other => idb_sources::redis::format_cell(other),
    }
}

fn int(v: &RValue) -> i64 {
    match v {
        RValue::Int(n) => *n,
        other => text(other).parse().unwrap_or(0),
    }
}

/// SCAN 답(커서, 원소들)을 읽는다.
fn scan_reply(v: RValue) -> Result<(String, Vec<RValue>), Error> {
    match v {
        RValue::Array(mut a) if a.len() == 2 => {
            let items = match a.pop() {
                Some(RValue::Array(items)) => items,
                Some(RValue::Map(pairs)) => pairs.into_iter().flat_map(|(k, v)| [k, v]).collect(),
                _ => Vec::new(),
            };
            Ok((text(&a[0]), items))
        }
        _ => Err(Error::msg("unexpected SCAN reply")),
    }
}

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

async fn scan_keys(r: &mut RedisConn, pattern: String) -> Result<ScanResult, Error> {
    let (mut keys, mut more, mut cursor) = (Vec::new(), false, "0".to_string());
    loop {
        let (next, batch) = scan_reply(r.value(&args(&["SCAN", &cursor, "MATCH", &pattern, "COUNT", "500"])).await?)?;
        keys.extend(batch.iter().map(text));
        cursor = next;
        if cursor == "0" {
            break;
        }
        if keys.len() >= MAX_ROWS {
            more = true;
            break;
        }
    }
    if keys.len() > MAX_ROWS {
        keys.truncate(MAX_ROWS);
        more = true;
    }
    // SCAN 은 리해시 중 같은 키를 두 번 줄 수 있다
    keys.sort();
    keys.dedup();
    Ok(ScanResult { keys, more })
}

/// Go HumanBytes.
fn human_bytes(n: i64) -> String {
    if n < 1024 {
        return format!("{n}B");
    }
    let mut f = n as f64;
    for u in ["KB", "MB", "GB", "TB"] {
        f /= 1024.0;
        if f < 1024.0 || u == "TB" {
            return format!("{f:.1}{u}");
        }
    }
    String::new()
}

async fn get_value(r: &mut RedisConn, key: String) -> Result<ResultSet, Error> {
    let start = Instant::now();
    let typ = text(&r.value(&args(&["TYPE", &key])).await?);
    let ttl = int(&r.value(&args(&["TTL", &key])).await?);
    let ttl_text = if ttl >= 0 {
        idb_core::gostrconv::duration_string(ttl.saturating_mul(1_000_000_000))
    } else {
        "∞".into()
    };
    let mut res = QueryResult {
        text: format!("{key} · {typ} · TTL {ttl_text}"),
        ..Default::default()
    };
    match r.value(&args(&["MEMORY", "USAGE", &key])).await {
        Ok(RValue::Nil) => {}
        Ok(v) => res.text.push_str(&format!(" · 메모리 {}", human_bytes(int(&v)))),
        // 관리형 서버가 명령을 막았거나 모른다
        Err(Error::Other(e)) if e.contains("unknown command") || e.contains("unknown subcommand") || e.starts_with("NOPERM") => {}
        Err(e) => return Err(e),
    }
    let max = MAX_ROWS.to_string();
    match typ.as_str() {
        "string" => {
            let size = int(&r.value(&args(&["STRLEN", &key])).await?);
            let v = text(
                &r.value(&args(&["GETRANGE", &key, "0", &(MAX_STRING_BYTES - 1).to_string()]))
                    .await?,
            );
            res.text.push_str(&format!("\n\n{}", idb_sources::redis::pretty_json(&v)));
            if size > MAX_STRING_BYTES {
                res.truncated = true;
                res.text
                    .push_str(&format!("\n\n… {size}바이트 중 앞 {MAX_STRING_BYTES}바이트만 보여 줌"));
            }
        }
        "hash" | "set" => {
            // HGETALL·SMEMBERS 는 큰 값에서 상한이 없어 HSCAN·SSCAN 으로 MAX_ROWS 근처까지만 읽는다
            let hash = typ == "hash";
            res.cols = if hash { args(&["field", "value"]) } else { args(&["member"]) };
            let mut cursor = "0".to_string();
            loop {
                let cmd = if hash { "HSCAN" } else { "SSCAN" };
                let (next, items) = scan_reply(r.value(&args(&[cmd, &key, &cursor, "MATCH", "*", "COUNT", "500"])).await?)?;
                if hash {
                    res.rows
                        .extend(items.as_chunks::<2>().0.iter().map(|[k, v]| vec![cell(k), cell(v)]));
                } else {
                    res.rows.extend(items.iter().map(|v| vec![cell(v)]));
                }
                cursor = next;
                if cursor == "0" || res.rows.len() >= MAX_ROWS {
                    res.truncated = cursor != "0";
                    break;
                }
            }
            res.rows.sort_by(|a, b| a[0].cmp(&b[0]));
        }
        "list" => {
            // 하나 더 받아 정확히 1,000개와 잘림을 가른다
            let RValue::Array(mut vs) = r.value(&args(&["LRANGE", &key, "0", &max])).await? else {
                return Err(Error::msg("unexpected LRANGE reply"));
            };
            if vs.len() > MAX_ROWS {
                vs.truncate(MAX_ROWS);
                res.truncated = true;
            }
            res.cols = args(&["#", "value"]);
            res.rows = vs.iter().enumerate().map(|(i, v)| vec![i.to_string(), cell(v)]).collect();
        }
        "zset" => {
            let RValue::Array(vs) = r.value(&args(&["ZRANGE", &key, "0", &max, "WITHSCORES"])).await? else {
                return Err(Error::msg("unexpected ZRANGE reply"));
            };
            // RESP2 는 [멤버, 점수, …], RESP3 는 [[멤버, 점수], …] 다
            let mut pairs: Vec<(RValue, RValue)> = Vec::new();
            let mut flat = Vec::new();
            for v in vs {
                match v {
                    RValue::Array(mut p) if p.len() == 2 => {
                        let s = p.pop().unwrap_or(RValue::Nil);
                        pairs.push((p.pop().unwrap_or(RValue::Nil), s));
                    }
                    v => flat.push(v),
                }
            }
            let mut it = flat.into_iter();
            while let (Some(m), Some(s)) = (it.next(), it.next()) {
                pairs.push((m, s));
            }
            if pairs.len() > MAX_ROWS {
                pairs.truncate(MAX_ROWS);
                res.truncated = true;
            }
            res.cols = args(&["member", "score"]);
            res.rows = pairs
                .iter()
                .map(|(m, s)| {
                    let score = match s {
                        RValue::Float(f) => *f,
                        other => text(other).parse().unwrap_or(0.0),
                    };
                    // Go strconv.FormatFloat(f, 'f', -1, 64): 지수 없이 가장 짧은 자릿수다
                    vec![cell(m), format!("{score}")]
                })
                .collect();
        }
        "stream" => {
            let RValue::Array(xs) = r.value(&args(&["XRANGE", &key, "-", "+", "COUNT", "200"])).await? else {
                return Err(Error::msg("unexpected XRANGE reply"));
            };
            res.cols = args(&["id", "fields"]);
            for x in xs {
                let RValue::Array(parts) = x else { continue };
                let (Some(id), Some(RValue::Array(fv))) = (parts.first(), parts.get(1)) else {
                    continue;
                };
                // Go json.Marshal(map[string]any): 키 순서로 정렬한다
                let mut kv: Vec<(String, String)> = fv.as_chunks::<2>().0.iter().map(|[k, v]| (text(k), text(v))).collect();
                kv.sort();
                kv.dedup_by(|a, b| a.0 == b.0);
                let mut obj = String::from("{");
                for (i, (k, v)) in kv.iter().enumerate() {
                    if i > 0 {
                        obj.push(',');
                    }
                    idb_core::gojson::write_str(&mut obj, k);
                    obj.push(':');
                    idb_core::gojson::write_str(&mut obj, v);
                }
                obj.push('}');
                res.rows.push(vec![text(id), cap_cell(obj.as_bytes())]);
            }
        }
        "none" => res.text.push_str("\n\n(키 없음)"),
        _ => res.text.push_str("\n\n(보기 미지원 타입)"),
    }
    res.elapsed = start.elapsed();
    Ok(ResultSet::from_query(res))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 바이트_크기는_Go_HumanBytes_와_같다() {
        assert_eq!(human_bytes(1023), "1023B");
        assert_eq!(human_bytes(1536), "1.5KB");
        assert_eq!(human_bytes(5 << 20), "5.0MB");
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
