//! PostgreSQL 접속 풀과 CLI 가 쓰는 조회. Go `internal/db` 의 backend·readonly·meta·export(SearchData)·schemadiff·linked 와 같다.

mod meta;
mod types;
mod wire;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use idb_core::project::{Conn as ConnInfo, Kind};
use idb_core::result::{MAX_ROWS, NULL_TEXT, QueryResult, cap_cell};
use idb_core::sqlsafe::{check_read_only_sql, first_sql_word, is_read_only_sql, quote_literal, trim_statement};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::linked::{self, LinkedColumn};
use crate::schemadiff::{SnapColumn, SnapTable, Snapshot, column_def};
use crate::{Error, redis};
pub use meta::TableRef;
use wire::{Config, Rows};

/// 풀 하나가 여는 연결 수 상한(pgxpool MaxConns)
const MAX_CONNS: usize = 4;
/// 읽기 전용 실행의 서버 시간 제한
const READ_ONLY_TIMEOUT: &str = "30s";
const SEARCH_ROWS_PER_TABLE: usize = 5;
const SEARCH_MAX_TABLES: usize = 200;
const SEARCH_TIMEOUT: &str = "5s";

// ─── 접속 모음 ───

/// 접속 하나의 칸이다. 처음 쓸 때 채운다.
type Slot<T> = Arc<tokio::sync::Mutex<Option<T>>>;
/// 명령을 하나씩 보내는 Redis 연결이다.
pub type SharedRedis = Arc<tokio::sync::Mutex<redis::RedisConn>>;

/// 접속을 처음 쓸 때 연다. 같은 접속은 한 번만 열고(칸마다 잠금), 다른 접속은 동시에 연다.
#[derive(Default)]
pub struct Clients {
    slots: Mutex<HashMap<String, Slot<Pg>>>,
    redis_slots: Mutex<HashMap<String, Slot<SharedRedis>>>,
}

impl Clients {
    pub fn new() -> Clients {
        Clients::default()
    }

    /// 접속 c 의 풀이다. 처음이면 연결 하나를 열어 확인한다.
    pub async fn pg(&self, c: &ConnInfo) -> Result<Pg, Error> {
        let slot = self.slots.lock().unwrap().entry(c.uuid.clone()).or_default().clone();
        let mut slot = slot.lock().await;
        if let Some(pg) = slot.as_ref() {
            return Ok(pg.clone());
        }
        let pg = Pg::open(c).await?;
        *slot = Some(pg.clone());
        Ok(pg)
    }

    /// 접속 c 의 Redis 연결이다. 처음이면 평문을 먼저 시도하고 안 되면 TLS 로 붙는다(R4.3).
    pub async fn redis(&self, c: &ConnInfo) -> Result<SharedRedis, Error> {
        let slot = self.redis_slots.lock().unwrap().entry(c.uuid.clone()).or_default().clone();
        let mut slot = slot.lock().await;
        if let Some(r) = slot.as_ref() {
            return Ok(r.clone());
        }
        let r = Arc::new(tokio::sync::Mutex::new(redis::RedisConn::open(c).await?));
        *slot = Some(r.clone());
        Ok(r)
    }

    /// 풀에 남은 연결을 Terminate 로 닫는다. 소켓만 끊으면 서버 로그에 비정상 종료가 남는다.
    pub async fn close(&self) {
        let slots: Vec<Slot<Pg>> = self.slots.lock().unwrap().values().cloned().collect();
        for slot in slots {
            let Some(pg) = slot.lock().await.take() else { continue };
            let idle: Vec<wire::Conn> = std::mem::take(&mut *pg.0.idle.lock().unwrap());
            for c in idle {
                c.close().await;
            }
        }
    }
}

// ─── 풀 ───

/// 접속 하나의 연결 풀이다. 복사하면 같은 풀을 가리킨다.
#[derive(Clone)]
pub struct Pg(Arc<Inner>);

struct Inner {
    cfg: Config,
    idle: Mutex<Vec<wire::Conn>>,
    sem: Arc<Semaphore>,
}

/// 풀에서 빌린 연결이다. release 하지 않고 버리면 연결도 닫힌다(실패·취소 뒤 상태를 모르는 연결을 다시 쓰지 않는다).
pub(crate) struct Pooled {
    conn: Option<wire::Conn>,
    pg: Pg,
    _permit: OwnedSemaphorePermit,
}

impl Pooled {
    pub(crate) fn conn(&mut self) -> &mut wire::Conn {
        self.conn.as_mut().expect("pooled connection")
    }

    /// 트랜잭션 밖의 성한 연결만 풀에 돌려준다.
    pub(crate) fn release(mut self) {
        if let Some(c) = self.conn.take()
            && c.reusable()
        {
            self.pg.0.idle.lock().unwrap().push(c);
        }
    }

    /// 풀에 돌려주지 않고 세션째 닫는다(advisory lock·SET 같은 세션 상태를 남기지 않는다).
    async fn close(mut self) {
        if let Some(c) = self.conn.take() {
            c.close().await;
        }
    }
}

impl Pg {
    async fn open(c: &ConnInfo) -> Result<Pg, Error> {
        if c.kind != Some(Kind::Pg) {
            return Err(Error::msg(format!("{} is not a postgresql connection", c.name)));
        }
        let port = if c.port == 0 { 5432 } else { c.port };
        // 원격은 TLS 를 강제해 평문으로 떨어지지 않게 한다. 로컬 컨테이너는 보통 TLS 가 없어 prefer 로 둔다
        let local = c.host == "localhost" || c.host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback());
        let mut sslmode = if local { "prefer" } else { "require" }.to_string();
        // TLS 를 받지 않는 사내 서버는 접속 파일의 sslmode 로 낮춘다(R59.1)
        if !c.sslmode.is_empty() {
            sslmode = c.sslmode.clone();
        }
        if !matches!(
            sslmode.as_str(),
            "disable" | "allow" | "prefer" | "require" | "verify-ca" | "verify-full"
        ) {
            // pgx 는 설정 오류 글에 접속 URL(비밀번호)을 실어 원문 대신 이 글을 낸다
            return Err(Error::msg(format!("invalid connection config for {}", c.name)));
        }
        let pg = Pg(Arc::new(Inner {
            cfg: Config {
                host: c.host.clone(),
                port,
                user: c.user.clone(),
                password: c.password.clone(),
                database: c.db.clone(),
                sslmode,
            },
            idle: Mutex::new(Vec::new()),
            sem: Arc::new(Semaphore::new(MAX_CONNS)),
        }));
        // pgxpool.Ping 처럼 연결 하나를 열어 확인하고 풀에 둔다
        let mut p = pg.acquire().await?;
        p.conn().exec("-- ping").await?;
        p.release();
        Ok(pg)
    }

    /// 연결을 열지 않은 풀이다. 접속 전 거부를 보는 시험이 쓴다.
    #[cfg(test)]
    pub(crate) fn unopened(host: &str, port: u16) -> Pg {
        Pg(Arc::new(Inner {
            cfg: Config {
                host: host.into(),
                port,
                user: "u".into(),
                password: String::new(),
                database: "d".into(),
                sslmode: "disable".into(),
            },
            idle: Mutex::new(Vec::new()),
            sem: Arc::new(Semaphore::new(MAX_CONNS)),
        }))
    }

    pub(crate) async fn acquire(&self) -> Result<Pooled, Error> {
        let permit = self
            .0
            .sem
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| Error::msg("connections are closed"))?;
        let idle = self.0.idle.lock().unwrap().pop();
        let conn = match idle {
            Some(c) => c,
            None => wire::Conn::connect(&self.0.cfg).await?,
        };
        Ok(Pooled {
            conn: Some(conn),
            pg: self.clone(),
            _permit: permit,
        })
    }

    /// 풀 연결에서 조회 하나를 하고 연결을 돌려준다.
    async fn pool_query(&self, sql: &str, params: &[Option<&str>]) -> Result<Rows, Error> {
        let mut p = self.acquire().await?;
        let rows = p.conn().query(sql, params, usize::MAX).await?;
        p.release();
        Ok(rows)
    }

    pub async fn find_schemas(&self) -> Result<Vec<String>, Error> {
        let rows = self
            .pool_query(
                r"select nspname from pg_namespace
		where nspname not like 'pg\_%' and nspname <> 'information_schema' order by 1",
                &[],
            )
            .await?;
        Ok((0..rows.rows.len()).map(|r| rows.text(r, 0)).collect())
    }

    pub async fn find_tables(&self, schema: &str) -> Result<Vec<String>, Error> {
        let rows = self
            .pool_query(
                "select c.relname from pg_class c join pg_namespace n on n.oid = c.relnamespace
		where n.nspname = $1 and c.relkind in ('r','p','v','m','f') order by 1",
                &[Some(schema)],
            )
            .await?;
        Ok((0..rows.rows.len()).map(|r| rows.text(r, 0)).collect())
    }

    // ─── 실행 ───

    /// 문장 하나를 풀 연결의 읽기 전용 트랜잭션으로 실행하고 결과를 limit 행까지 싣는다.
    /// 사람이 확인하지 않는 호출을 위한 것이라 check_read_only_sql 에 걸리는 문장은 여기서도 거부하고,
    /// 첫 단어 판정이 놓치는 CTE 안 쓰기·select ... into 는 서버의 READ ONLY 가 막게 한다.
    /// 연결은 풀에 돌려주지 않고 닫는다. ROLLBACK 해도 advisory lock·LISTEN·SET 같은 세션 상태가 남아서다.
    pub async fn run_read_only_rows(&self, sql: &str, limit: usize) -> Result<QueryResult, Error> {
        check_read_only_sql(sql).map_err(|e| Error::msg(e.to_string()))?;
        let start = Instant::now();
        let mut p = self.acquire().await?;
        let res = read_only(p.conn(), sql, limit).await;
        p.close().await;
        let mut res = res?;
        res.elapsed = start.elapsed();
        Ok(res)
    }

    /// 쓰기 허용 실행(Go RunArgs, CLI --write). 값은 텍스트로 바인딩해 서버가 타입을 정한다.
    /// select·values·table 읽기 문장은 커서로 MAX_ROWS+1 행만 받고, 그 밖의 문장(쓰기·RETURNING·SHOW·EXPLAIN)은 끝까지 받되 MAX_ROWS 행만 싣는다.
    pub async fn run_args(&self, sql: &str, args: &[String]) -> Result<QueryResult, Error> {
        let start = Instant::now();
        let mut p = self.acquire().await?;
        let params: Vec<Option<&str>> = args.iter().map(|a| Some(a.as_str())).collect();
        let c = p.conn();
        let w = first_sql_word(sql);
        let res = if !matches!(w.as_str(), "select" | "values" | "table") || !is_read_only_sql(sql) {
            c.query(sql, &params, MAX_ROWS).await.map(to_result)
        } else {
            // 트랜잭션 밖이면 커서용 트랜잭션을 직접 열고, 실패하면 직접 닫는다
            let began = !c.in_tx();
            let r = async {
                if began {
                    c.exec("BEGIN").await?;
                }
                c.query(
                    &format!("DECLARE dbtui_cursor NO SCROLL CURSOR FOR {}", trim_statement(sql)),
                    &params,
                    0,
                )
                .await?;
                let rows = c
                    .query(&format!("FETCH FORWARD {} FROM dbtui_cursor", MAX_ROWS + 1), &[], MAX_ROWS)
                    .await?;
                c.exec("CLOSE dbtui_cursor").await?;
                if began {
                    c.exec("COMMIT").await?;
                }
                Ok(to_result(rows))
            }
            .await;
            if r.is_err() && began {
                let _ = c.exec("ROLLBACK").await;
            }
            r
        };
        let mut res = res?;
        p.release();
        res.elapsed = start.elapsed();
        Ok(res)
    }

    // ─── 구조 ───

    /// 열마다 이름·타입·NULL 허용·기본값·주석을 행으로, 제약·인덱스를 text 로 돌려준다.
    pub async fn describe_table(&self, t: &TableRef) -> Result<QueryResult, Error> {
        let m = self.load_meta(t).await?;
        Ok(meta::describe(&m))
    }

    /// 카탈로그로 CREATE 문을 다시 만든다.
    pub async fn table_ddl(&self, t: &TableRef) -> Result<String, Error> {
        let m = self.load_meta(t).await?;
        meta::ddl(t, &m)
    }

    async fn load_meta(&self, t: &TableRef) -> Result<meta::TableMeta, Error> {
        let mut p = self.acquire().await?;
        let m = meta::load(p.conn(), t).await?;
        p.release();
        Ok(m)
    }

    // ─── 검색 ───

    /// 스키마의 테이블마다 글자 열을 ::text ILIKE '%text%' 로 찾는다. 테이블당 5행, 테이블 200개까지,
    /// 문장마다 statement_timeout 5초, 읽기 전용 트랜잭션이다. 결과 열은 테이블·열·PK·값이고 PK 칸은 `id=3` 꼴이다.
    /// 시간이 넘었거나 서버 오류가 난 테이블은 SAVEPOINT 로 되돌리고 skipped 에 넣은 뒤 다음 테이블로 간다.
    /// schema 가 비었으면 연결의 current_schema() 다(CLI 는 search_path 를 고르지 않는다).
    pub async fn search_data(&self, schema: &str, text: &str) -> Result<(QueryResult, Vec<String>), Error> {
        if text.is_empty() {
            return Err(Error::msg("empty search text"));
        }
        let mut p = self.acquire().await?;
        let c = p.conn();
        c.exec("BEGIN READ ONLY").await?;
        // SET LOCAL 은 뒤의 ROLLBACK TO SAVEPOINT 로도 풀리지 않는다(SAVEPOINT 전에 정했다)
        c.exec(&format!("SET LOCAL statement_timeout = '{SEARCH_TIMEOUT}'")).await?;
        let tables = c
            .query(
                &format!("{SEARCH_TABLES_QUERY} limit {}", SEARCH_MAX_TABLES + 1),
                &[Some(schema)],
                usize::MAX,
            )
            .await?;
        let mut res = QueryResult {
            cols: ["테이블", "열", "PK", "값"].map(String::from).to_vec(),
            ..Default::default()
        };
        let mut list = Vec::new();
        for r in 0..tables.rows.len() {
            let arr = |i: usize| serde_json::from_str::<Vec<String>>(&tables.text(r, i)).unwrap_or_default();
            list.push((tables.text(r, 0), tables.text(r, 1), arr(2), arr(3)));
        }
        if list.len() > SEARCH_MAX_TABLES {
            list.truncate(SEARCH_MAX_TABLES);
            res.truncated = true;
        }
        // LIKE 의 기본 이스케이프 글자는 백슬래시다. 패턴은 인자로 보내 standard_conforming_strings 와 무관하다
        let pattern = text.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        let mut skipped = Vec::new();
        for (schema, name, cols, pk) in &list {
            c.exec("SAVEPOINT dbtui_search").await?;
            match c.query(&search_sql(schema, name, cols, pk), &[Some(&pattern)], usize::MAX).await {
                Ok(hits) => {
                    for h in 0..hits.rows.len() {
                        let val = hits.rows[h][2].as_deref().map(cap_cell).unwrap_or_default();
                        res.rows.push(vec![name.clone(), hits.text(h, 0), hits.text(h, 1), val]);
                    }
                }
                Err(Error::Pg(e)) => {
                    c.exec("ROLLBACK TO SAVEPOINT dbtui_search").await?;
                    // query_canceled 는 statement_timeout 이다. 그 밖의 오류는 까닭을 붙인다
                    if e.code == "57014" {
                        skipped.push(name.clone());
                    } else {
                        skipped.push(format!("{name} ({})", e.message));
                    }
                }
                Err(e) => return Err(e),
            }
        }
        c.exec("ROLLBACK").await?;
        p.release();
        Ok((res, skipped))
    }

    // ─── 스키마 비교 ───

    /// 스키마 구조를 읽기 전용 트랜잭션 하나에서 읽는다. 세 조회가 같은 스냅숏을 보도록 REPEATABLE READ 로 연다.
    /// search_path 를 그 스키마로 두고 읽어 정의 글에 스키마 이름이 붙지 않게 한다. 없는 스키마는 빈 스냅숏이다.
    pub async fn schema_snapshot(&self, schema: &str) -> Result<Snapshot, Error> {
        let mut p = self.acquire().await?;
        let c = p.conn();
        c.exec("begin isolation level repeatable read read only").await?;
        let snap = async {
            c.query(
                "select set_config('search_path', $1, true)",
                &[Some(&idb_core::sqlsafe::quote_ident(schema))],
                usize::MAX,
            )
            .await?;
            read_snapshot(c, schema).await
        }
        .await;
        // 읽기만 하므로 끝은 늘 되돌리기다. set_config(…, true) 도 트랜잭션과 함께 사라진다
        let _ = c.exec("rollback").await;
        p.release();
        snap
    }

    // ─── 연결된 곳 ───

    /// 이 DB 에서 col·base 규칙과 값 타입에 맞는 열이다. LINKED_MAX_COLUMNS 를 넘으면 truncated 다.
    /// col 이 id 면 같은 이름 규칙은 쓰지 않는다. 모든 테이블의 id 가 걸려서다.
    pub async fn linked_columns(&self, col: &str, base: &str, value: &str) -> Result<(Vec<LinkedColumn>, bool), Error> {
        let mut same = col.to_lowercase();
        if same == "id" {
            same.clear();
        }
        let owners = pg_array(&linked::owner_names(base));
        let types = pg_array(&linked::linked_types(value));
        let max = (linked::LINKED_MAX_COLUMNS + 1).to_string();
        let rows = self
            .pool_query(
                linked::COLUMNS_QUERY,
                &[Some(&same), Some(base), Some(&owners), Some(&types), Some(&max)],
            )
            .await?;
        let mut cols: Vec<LinkedColumn> = (0..rows.rows.len())
            .map(|r| LinkedColumn {
                table: TableRef {
                    schema: rows.text(r, 0),
                    table: rows.text(r, 1),
                },
                column: rows.text(r, 2),
                typ: rows.text(r, 3),
                owner: rows.flag(r, 4),
                indexed: rows.flag(r, 5),
            })
            .collect();
        if cols.len() > linked::LINKED_MAX_COLUMNS {
            cols.truncate(linked::LINKED_MAX_COLUMNS);
            return Ok((cols, true));
        }
        Ok((cols, false))
    }

    /// cols 가운데 indexed 인 열만 workers 개씩 동시에 센다. 세지 않은 열은 counts[i] 가 -1 이다.
    pub async fn count_linked(&self, cols: &[LinkedColumn], value: &str, workers: usize) -> (Vec<i64>, Vec<Option<Error>>) {
        let mut counts = vec![-1i64; cols.len()];
        let mut errs: Vec<Option<Error>> = vec![None; cols.len()];
        let slots = Arc::new(Semaphore::new(workers.max(1)));
        let mut set = tokio::task::JoinSet::new();
        for (i, c) in cols.iter().enumerate().filter(|(_, c)| c.indexed) {
            let (pg, slots, sql) = (self.clone(), slots.clone(), linked::count_sql(c, value));
            set.spawn(async move {
                let _slot = slots.acquire_owned().await;
                (i, pg.count_one(&sql).await)
            });
        }
        while let Some(done) = set.join_next().await {
            match done {
                Ok((i, Ok(n))) => counts[i] = n,
                Ok((i, Err(e))) => {
                    counts[i] = 0;
                    errs[i] = Some(e);
                }
                Err(_) => {}
            }
        }
        (counts, errs)
    }

    /// count 하나를 풀 연결의 READ ONLY 트랜잭션에서 statement_timeout 을 걸고 돌린다.
    /// db 가 만든 문장이라 세션째 버리지 않고 풀에 돌려준다. 후보마다 새 연결을 열면 prod 에 부담이 커서다.
    pub(crate) async fn count_one(&self, sql: &str) -> Result<i64, Error> {
        let mut p = self.acquire().await?;
        let c = p.conn();
        c.exec("begin read only").await?;
        let r = async {
            c.exec(&format!("SET LOCAL statement_timeout = '{}'", linked::STATEMENT_TIMEOUT))
                .await?;
            let rows = c.query(sql, &[], 1).await?;
            if rows.rows.is_empty() {
                return Err(Error::msg("no rows in result set"));
            }
            rows.text(0, 0).parse::<i64>().map_err(|e| Error::msg(e.to_string()))
        }
        .await;
        let _ = c.exec("rollback").await;
        p.release();
        r
    }
}

/// 읽기 전용 트랜잭션 안에서 sql 을 실행한다. select·values·table·with 는 커서로 limit+1 행만 받는다.
/// 실행 취소로 끊으면 연결이 닫혀서다. 커서에 못 넣는 show·explain 은 결과가 작아 끝까지 읽고 버린다.
async fn read_only(c: &mut wire::Conn, sql: &str, limit: usize) -> Result<QueryResult, Error> {
    c.exec("BEGIN READ ONLY").await?;
    c.exec(&format!("SET LOCAL statement_timeout = '{READ_ONLY_TIMEOUT}'")).await?;
    // check_read_only_sql 의 함수 차단은 표준 문자열 규칙(백슬래시가 이스케이프가 아님)으로 문장을 읽는다.
    // 서버 설정이 off 면 같은 문장을 다르게 읽어 차단을 비켜 갈 수 있어 고정한다
    c.exec("SET LOCAL standard_conforming_strings = on").await?;
    let fetch;
    let run = match first_sql_word(sql).as_str() {
        "select" | "values" | "table" | "with" => {
            // 두 경로 모두 extended protocol 이라 여러 문장은 서버가 거부한다
            c.query(
                &format!("DECLARE dbtui_ro_cursor NO SCROLL CURSOR FOR {}", trim_statement(sql)),
                &[],
                0,
            )
            .await?;
            fetch = format!("FETCH FORWARD {} FROM dbtui_ro_cursor", limit + 1);
            fetch.as_str()
        }
        "" => return Err(Error::msg("empty statement")),
        _ => sql,
    };
    let rows = c.query(run, &[], limit).await?;
    Ok(to_result(rows))
}

/// 텍스트 결과를 표로 만든다. NULL 은 "NULL", 긴 칸은 cap_cell 로 자른다. 열이 없으면 명령 태그를 text 에 싣는다.
fn to_result(rows: Rows) -> QueryResult {
    let mut res = QueryResult {
        cols: rows.fields.iter().map(|(n, _)| n.clone()).collect(),
        types: Some(rows.fields.iter().map(|&(_, oid)| types::type_name(oid)).collect()),
        truncated: rows.truncated,
        ..Default::default()
    };
    res.rows = rows
        .rows
        .into_iter()
        .map(|r| {
            r.into_iter()
                .map(|v| v.as_deref().map_or_else(|| NULL_TEXT.to_string(), cap_cell))
                .collect()
        })
        .collect();
    if res.cols.is_empty() {
        res.text = rows.tag;
        res.types = None;
    }
    res
}

/// 글 배열의 PostgreSQL 텍스트 표기({"a","b"})다. 바인딩 인자로 보낸다.
fn pg_array(items: &[String]) -> String {
    let quoted: Vec<String> = items
        .iter()
        .map(|s| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect();
    format!("{{{}}}", quoted.join(","))
}

// ─── 검색 문장 ───

/// 검색할 테이블과 글자 열·PK 열이다. 배열은 JSON 으로 받아 텍스트 배열 표기를 풀지 않는다.
const SEARCH_TABLES_QUERY: &str = "select n.nspname::text, c.relname::text,
			array_to_json(array_agg(a.attname::text order by a.attnum))::text,
			coalesce((select array_to_json(array_agg(pa.attname::text order by k.ord))
				from pg_index i cross join unnest(i.indkey) with ordinality as k(attnum, ord)
				join pg_attribute pa on pa.attrelid = i.indrelid and pa.attnum = k.attnum
				where i.indrelid = c.oid and i.indisprimary), '[]'::json)::text
		from pg_class c join pg_namespace n on n.oid = c.relnamespace
		join pg_attribute a on a.attrelid = c.oid and a.attnum > 0 and not a.attisdropped
		join pg_type t on t.oid = a.atttypid
		where n.nspname = coalesce(nullif($1, ''), current_schema()) and c.relkind in ('r', 'p') and not c.relispartition
			and t.typname in ('text', 'varchar', 'bpchar', 'citext', 'json', 'jsonb', 'uuid')
			and has_table_privilege(c.oid, 'SELECT')
		group by n.nspname, c.oid, c.relname order by c.relname";

/// 테이블 하나에서 맞는 (행, 열) 쌍을 5개까지 찾는 문장이다. 결과는 열 이름, `a=1, b=x` 꼴 PK(없으면 빈 글), 값 순서다.
fn search_sql(schema: &str, table: &str, cols: &[String], pk: &[String]) -> String {
    use idb_core::sqlsafe::quote_ident;
    let vals: Vec<String> = cols
        .iter()
        .map(|c| format!("({}, s.{}::text)", quote_literal(c), quote_ident(c)))
        .collect();
    let pk = if pk.is_empty() {
        "''".to_string()
    } else {
        let parts: Vec<String> = pk
            .iter()
            .map(|c| format!("{} || s.{}::text", quote_literal(&format!("{c}=")), quote_ident(c)))
            .collect();
        format!("concat_ws(', ', {})", parts.join(", "))
    };
    format!(
        "select m.col, {pk}, m.val from {}.{} as s cross join lateral (values {}) as m(col, val) where m.val ilike '%' || $1 || '%' limit {SEARCH_ROWS_PER_TABLE}",
        quote_ident(schema),
        quote_ident(table),
        vals.join(", ")
    )
}

// ─── 스키마 비교 조회 ───

const SNAP_REL_QUERY: &str = "select c.relname, c.relkind::text,
		coalesce(case when c.relkind in ('v', 'm') then pg_get_viewdef(c.oid, true) end, '')
		from pg_class c join pg_namespace n on n.oid = c.relnamespace
		where n.nspname = $1 and c.relkind in ('r', 'p', 'v', 'm', 'f')";
const SNAP_COL_QUERY: &str = "select c.relname, a.attname, format_type(a.atttypid, a.atttypmod), a.attnotnull,
		coalesce(pg_get_expr(d.adbin, d.adrelid), ''), a.attidentity::text, a.attgenerated::text
		from pg_attribute a join pg_class c on c.oid = a.attrelid join pg_namespace n on n.oid = c.relnamespace
		left join pg_attrdef d on d.adrelid = a.attrelid and d.adnum = a.attnum
		where n.nspname = $1 and c.relkind in ('r', 'p', 'v', 'm', 'f') and a.attnum > 0 and not a.attisdropped
		order by c.relname, a.attnum";
// 인덱스(true)와 제약(false)은 같은 꼴(테이블, 이름, 정의)이라 한 번에 읽는다.
// pg_get_indexdef 는 search_path 와 무관하게 테이블을 늘 스키마로 한정해서 ' 스키마.' 를 지운다
const SNAP_OBJ_QUERY: &str =
    "select c.relname, true, ic.relname, replace(pg_get_indexdef(i.indexrelid), ' ' || quote_ident(n.nspname) || '.', ' ')
		from pg_index i join pg_class c on c.oid = i.indrelid join pg_class ic on ic.oid = i.indexrelid
		join pg_namespace n on n.oid = c.relnamespace where n.nspname = $1
		union all
		select c.relname, false, k.conname, pg_get_constraintdef(k.oid)
		from pg_constraint k join pg_class c on c.oid = k.conrelid join pg_namespace n on n.oid = c.relnamespace
		where n.nspname = $1 and k.contype in ('p', 'u', 'c', 'f', 'x')";

/// 세 조회를 읽어 합친다. 첫 조회에 없던 테이블의 열·인덱스·제약은 버린다(같은 스냅숏이면 생기지 않는다).
async fn read_snapshot(c: &mut wire::Conn, schema: &str) -> Result<Snapshot, Error> {
    let mut snap = Snapshot {
        schema: schema.to_string(),
        tables: Default::default(),
    };
    let rels = c.query(SNAP_REL_QUERY, &[Some(schema)], usize::MAX).await?;
    for r in 0..rels.rows.len() {
        snap.tables.insert(
            rels.text(r, 0),
            SnapTable {
                kind: crate::schemadiff::kind_word(&rels.text(r, 1)).to_string(),
                view: rels.text(r, 2),
                ..Default::default()
            },
        );
    }
    let cols = c.query(SNAP_COL_QUERY, &[Some(schema)], usize::MAX).await?;
    for r in 0..cols.rows.len() {
        let def = column_def(
            &cols.text(r, 2),
            cols.flag(r, 3),
            &cols.text(r, 4),
            &cols.text(r, 5),
            &cols.text(r, 6),
        );
        if let Some(t) = snap.tables.get_mut(&cols.text(r, 0)) {
            t.columns.push(SnapColumn {
                name: cols.text(r, 1),
                def,
            });
        }
    }
    let objs = c.query(SNAP_OBJ_QUERY, &[Some(schema)], usize::MAX).await?;
    for r in 0..objs.rows.len() {
        if let Some(t) = snap.tables.get_mut(&objs.text(r, 0)) {
            let map = if objs.flag(r, 1) { &mut t.indexes } else { &mut t.constraints };
            map.insert(objs.text(r, 2), objs.text(r, 3));
        }
    }
    Ok(snap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 배열_인자는_원소마다_따옴표로_감싼다() {
        assert_eq!(pg_array(&["a".into(), "b\"c\\".into()]), r#"{"a","b\"c\\"}"#);
        assert_eq!(pg_array(&[]), "{}");
    }

    #[test]
    fn 검색_문장은_PK_가_없으면_빈_글이다() {
        let sql = search_sql("s", "t", &["a".into(), "b".into()], &[]);
        assert_eq!(
            sql,
            r#"select m.col, '', m.val from "s"."t" as s cross join lateral (values ('a', s."a"::text), ('b', s."b"::text)) as m(col, val) where m.val ilike '%' || $1 || '%' limit 5"#
        );
        let sql = search_sql("s", "t", &["a".into()], &["id".into(), "k".into()]);
        assert!(
            sql.contains(r#"concat_ws(', ', 'id=' || s."id"::text, 'k=' || s."k"::text)"#),
            "{sql}"
        );
    }
}
