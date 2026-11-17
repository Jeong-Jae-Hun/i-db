#![allow(non_snake_case)]
//! TUI·GUI·CLI 가 같이 쓰는 데이터 쪽 API. 화면은 이 Engine 만 부르고, 접속이 이 기계에 있든(Local)
//! ssh 너머 `db serve` 에 있든(Remote) 같은 요청·응답 모양으로 돈다. Local 도 요청을 JSON 값으로 거쳐
//! 두 길의 뜻이 갈리지 않게 한다. 안전장치와 가리기는 늘 접속이 있는 쪽(serve)이 정한다.

pub mod local;
pub mod lookup;
pub mod protocol;
mod redis;
pub mod remote;
pub mod serve;

use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde_json::Value;

pub use idb_sources::fk_where;
pub use local::{CliHandler, LocalEngine};
pub use protocol::*;
pub use remote::{RemoteEngine, RemoteSpec};

#[derive(Clone)]
pub enum Engine {
    Local(Arc<LocalEngine>),
    Remote(Arc<RemoteEngine>),
}

impl Engine {
    /// 요청 하나를 처리한다. 돌려받는 퓨처를 버리면 취소된다(원격이면 cancel 을 보낸다).
    pub async fn call(&self, req: Request) -> Result<Value, EngineError> {
        match self {
            Engine::Local(l) => l.handle(req).await,
            Engine::Remote(r) => r.call(req).await,
        }
    }

    async fn typed<T: DeserializeOwned>(&self, req: Request) -> Result<T, EngineError> {
        let v = self.call(req).await?;
        serde_json::from_value(v).map_err(|e| EngineError::run(format!("bad result: {e}")))
    }

    pub async fn close(&self) {
        match self {
            Engine::Local(l) => l.close().await,
            Engine::Remote(r) => r.close().await,
        }
    }

    // ─── 프로젝트·구조 ───

    pub async fn project(&self) -> Result<ProjectInfo, EngineError> {
        self.typed(Request::Project).await
    }

    pub async fn schemas(&self, conn: &str) -> Result<Vec<String>, EngineError> {
        self.typed(Request::Schemas { conn: conn.into() }).await
    }

    pub async fn tables(&self, conn: &str, schema: &str) -> Result<Vec<String>, EngineError> {
        self.typed(Request::Tables {
            conn: conn.into(),
            schema: schema.into(),
        })
        .await
    }

    pub async fn describe(&self, conn: &str, schema: &str, table: &str) -> Result<ResultSet, EngineError> {
        self.typed(Request::Describe {
            conn: conn.into(),
            schema: schema.into(),
            table: table.into(),
        })
        .await
    }

    pub async fn ddl(&self, conn: &str, schema: &str, table: &str) -> Result<String, EngineError> {
        self.typed(Request::Ddl {
            conn: conn.into(),
            schema: schema.into(),
            table: table.into(),
        })
        .await
    }

    pub async fn search(&self, conn: &str, schema: &str, text: &str) -> Result<ResultSet, EngineError> {
        self.typed(Request::Search {
            conn: conn.into(),
            schema: schema.into(),
            text: text.into(),
        })
        .await
    }

    // ─── 편집기 세션 ───

    pub async fn run(&self, req: RunRequest) -> Result<ResultSet, EngineError> {
        self.typed(Request::Run(req)).await
    }

    pub async fn explain(&self, run: RunRequest, analyze: bool, buffers: bool, format: &str) -> Result<ResultSet, EngineError> {
        self.typed(Request::Explain {
            run,
            analyze,
            buffers,
            format: format.into(),
        })
        .await
    }

    pub async fn table_page(&self, req: TablePageRequest) -> Result<TablePage, EngineError> {
        self.typed(Request::TablePage(req)).await
    }

    pub async fn tx_begin(&self, conn: &str) -> Result<TxState, EngineError> {
        self.typed(Request::TxBegin { conn: conn.into() }).await
    }

    pub async fn tx_commit(&self, conn: &str, confirm_prod: bool) -> Result<TxState, EngineError> {
        self.typed(Request::TxCommit {
            conn: conn.into(),
            confirm_prod,
        })
        .await
    }

    pub async fn tx_rollback(&self, conn: &str) -> Result<TxState, EngineError> {
        self.typed(Request::TxRollback { conn: conn.into() }).await
    }

    pub async fn tx_state(&self, conn: &str) -> Result<TxState, EngineError> {
        self.typed(Request::TxState { conn: conn.into() }).await
    }

    pub async fn prod_lock(&self, conn: &str, unlock: Option<bool>) -> Result<LockState, EngineError> {
        self.typed(Request::ProdLock { conn: conn.into(), unlock }).await
    }

    // ─── Redis ───

    pub async fn redis_scan(&self, conn: &str, pattern: &str) -> Result<ScanResult, EngineError> {
        self.typed(Request::RedisScan {
            conn: conn.into(),
            pattern: pattern.into(),
        })
        .await
    }

    pub async fn redis_value(&self, conn: &str, key: &str) -> Result<ResultSet, EngineError> {
        self.typed(Request::RedisValue {
            conn: conn.into(),
            key: key.into(),
        })
        .await
    }

    pub async fn redis_run(
        &self,
        conn: &str,
        args: Vec<String>,
        confirm_prod: bool,
        confirm_danger: bool,
    ) -> Result<ResultSet, EngineError> {
        self.typed(Request::RedisRun {
            conn: conn.into(),
            args,
            confirm_prod,
            confirm_danger,
        })
        .await
    }

    // ─── CLI ───

    pub async fn cli(&self, req: CliRequest) -> Result<CliResponse, EngineError> {
        self.typed(Request::Cli(req)).await
    }
    // ─── 판 1 에 더한 op ───

    pub async fn databases(&self, conn: &str) -> Result<Vec<String>, EngineError> {
        self.typed(Request::Databases { conn: conn.into() }).await
    }

    pub async fn switch_db(&self, conn: &str, db: &str) -> Result<SessionState, EngineError> {
        self.typed(Request::SwitchDb {
            conn: conn.into(),
            db: db.into(),
        })
        .await
    }

    pub async fn set_search_path(&self, conn: &str, schema: &str) -> Result<SessionState, EngineError> {
        self.typed(Request::SetSearchPath {
            conn: conn.into(),
            schema: schema.into(),
        })
        .await
    }

    pub async fn activity(&self, conn: &str) -> Result<ResultSet, EngineError> {
        self.typed(Request::Activity { conn: conn.into() }).await
    }

    pub async fn locks(&self, conn: &str) -> Result<ResultSet, EngineError> {
        self.typed(Request::Locks { conn: conn.into() }).await
    }

    pub async fn table_sizes(&self, conn: &str) -> Result<ResultSet, EngineError> {
        self.typed(Request::TableSizes { conn: conn.into() }).await
    }

    pub async fn signal_backend(
        &self,
        conn: &str,
        pid: i64,
        terminate: bool,
        confirm: bool,
        confirm_prod: bool,
    ) -> Result<(), EngineError> {
        self.call(Request::SignalBackend {
            conn: conn.into(),
            pid,
            terminate,
            confirm,
            confirm_prod,
        })
        .await
        .map(|_| ())
    }

    pub async fn foreign_keys(&self, conn: &str, schema: &str, table: &str, incoming: bool) -> Result<Vec<Fk>, EngineError> {
        self.typed(Request::ForeignKeys {
            conn: conn.into(),
            schema: schema.into(),
            table: table.into(),
            incoming,
        })
        .await
    }

    pub async fn resolve(&self, conn: &str, name: &str) -> Result<Object, EngineError> {
        self.typed(Request::Resolve {
            conn: conn.into(),
            name: name.into(),
        })
        .await
    }

    pub async fn function_def(&self, conn: &str, schema: &str, name: &str) -> Result<String, EngineError> {
        self.typed(Request::FunctionDef {
            conn: conn.into(),
            schema: schema.into(),
            name: name.into(),
        })
        .await
    }

    pub async fn catalog(&self, conn: &str) -> Result<Catalog, EngineError> {
        self.typed(Request::Catalog { conn: conn.into() }).await
    }

    pub async fn enum_labels(&self, conn: &str, types: Vec<String>) -> Result<std::collections::HashMap<String, Vec<String>>, EngineError> {
        self.typed(Request::EnumLabels { conn: conn.into(), types }).await
    }

    pub async fn estimate_rows(
        &self,
        conn: &str,
        sql: &str,
        params: std::collections::BTreeMap<String, String>,
    ) -> Result<Count, EngineError> {
        self.typed(Request::EstimateRows {
            conn: conn.into(),
            sql: sql.into(),
            params,
        })
        .await
    }

    pub async fn apply_changes(&self, conn: &str, changes: ChangeSet, confirm_prod: bool) -> Result<AppliedResult, EngineError> {
        self.typed(Request::ApplyChanges {
            conn: conn.into(),
            changes,
            confirm_prod,
        })
        .await
    }

    pub async fn undo_changes(
        &self,
        conn: &str,
        changes: ChangeSet,
        applied: Applied,
        confirm_prod: bool,
    ) -> Result<AppliedResult, EngineError> {
        self.typed(Request::UndoChanges {
            conn: conn.into(),
            changes,
            applied,
            confirm_prod,
        })
        .await
    }

    pub async fn changes_sql(&self, changes: ChangeSet, applied: Option<Applied>) -> Result<ChangesPreview, EngineError> {
        self.typed(Request::ChangesSql { changes, applied }).await
    }

    pub async fn import_csv(
        &self,
        conn: &str,
        table: TableRef,
        columns: Vec<String>,
        rows: Vec<Vec<Option<String>>>,
        confirm_prod: bool,
    ) -> Result<Count, EngineError> {
        self.typed(Request::ImportCsv {
            conn: conn.into(),
            schema: table.schema,
            table: table.table,
            columns,
            rows,
            confirm_prod,
        })
        .await
    }

    pub async fn linked(&self, conn: &str, column: &str, value: &str, table: &str) -> Result<LinkedResult, EngineError> {
        self.typed(Request::Linked {
            conn: conn.into(),
            column: column.into(),
            value: value.into(),
            table: table.into(),
        })
        .await
    }

    pub async fn schema_diff(&self, conn: &str, to: &str, schema: &str, to_schema: &str) -> Result<ResultSet, EngineError> {
        self.typed(Request::SchemaDiff {
            conn: conn.into(),
            to: to.into(),
            schema: schema.into(),
            to_schema: to_schema.into(),
        })
        .await
    }

    pub async fn user360(&self, query: &str) -> Result<Vec<User360Item>, EngineError> {
        self.typed(Request::User360 { query: query.into() }).await
    }

    pub async fn redis_raw_value(&self, conn: &str, key: &str) -> Result<RawValue, EngineError> {
        self.typed(Request::RedisRawValue {
            conn: conn.into(),
            key: key.into(),
        })
        .await
    }

    pub async fn redis_edit(
        &self,
        conn: &str,
        edit: RedisEdit,
        confirm_prod: bool,
        confirm_danger: bool,
    ) -> Result<RedisEditResult, EngineError> {
        self.typed(Request::RedisEdit {
            conn: conn.into(),
            edit,
            confirm_prod,
            confirm_danger,
        })
        .await
    }

    pub async fn redis_info(&self, conn: &str) -> Result<ResultSet, EngineError> {
        self.typed(Request::RedisInfo { conn: conn.into() }).await
    }

    pub async fn redis_slowlog(&self, conn: &str, n: i64) -> Result<ResultSet, EngineError> {
        self.typed(Request::RedisSlowlog { conn: conn.into(), n }).await
    }

    pub async fn redis_big_keys(&self, conn: &str, sample: usize, top: usize) -> Result<ResultSet, EngineError> {
        self.typed(Request::RedisBigKeys {
            conn: conn.into(),
            sample,
            top,
        })
        .await
    }

    pub async fn redis_pattern_keys(&self, conn: &str, pattern: &str) -> Result<PatternKeys, EngineError> {
        self.typed(Request::RedisPatternKeys {
            conn: conn.into(),
            pattern: pattern.into(),
        })
        .await
    }

    pub async fn redis_unlink(&self, conn: &str, keys: Vec<String>, confirm: bool, confirm_prod: bool) -> Result<Count, EngineError> {
        self.typed(Request::RedisUnlink {
            conn: conn.into(),
            keys,
            confirm,
            confirm_prod,
        })
        .await
    }
}
