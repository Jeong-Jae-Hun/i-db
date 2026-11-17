#![allow(non_snake_case)]
//! TUI·GUI·CLI 가 같이 쓰는 데이터 쪽 API. 화면은 이 Engine 만 부르고, 접속이 이 기계에 있든(Local)
//! ssh 너머 `db serve` 에 있든(Remote) 같은 요청·응답 모양으로 돈다. Local 도 요청을 JSON 값으로 거쳐
//! 두 길의 뜻이 갈리지 않게 한다. 안전장치와 가리기는 늘 접속이 있는 쪽(serve)이 정한다.

pub mod board;
pub mod flows;
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

/// 묶음의 미리 보기 문장이다(`changes_sql` op 와 같은 글). applied 가 있으면 되돌리기 묶음의 문장이다.
/// 접속을 쓰지 않는 계산이라 화면이 원격에서도 요청 없이 바로 부른다(fk_where 와 같다).
pub fn changes_preview(cs: &ChangeSet, applied: Option<&Applied>) -> Result<Vec<String>, String> {
    match applied {
        Some(a) => idb_sources::undo_changes(cs, a).map(|u| idb_sources::changes_sql(&u)),
        None => Ok(idb_sources::changes_sql(cs)),
    }
}

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

    // ─── 저장소·대시보드 ───

    pub async fn source_children(&self, conn: &str, path: &str) -> Result<Vec<Node>, EngineError> {
        self.typed(Request::SourceChildren {
            conn: conn.into(),
            path: path.into(),
        })
        .await
    }

    pub async fn source_open(&self, conn: &str, path: &str) -> Result<ResultSet, EngineError> {
        self.source_open_rows(conn, path, 0).await
    }

    /// limit 행까지의 미리 보기다. 0 이면 source_open 과 같다
    pub async fn source_open_rows(&self, conn: &str, path: &str, limit: usize) -> Result<ResultSet, EngineError> {
        self.typed(Request::SourceOpen {
            conn: conn.into(),
            path: path.into(),
            limit,
        })
        .await
    }

    pub async fn source_run(&self, req: SourceRunRequest) -> Result<ResultSet, EngineError> {
        self.typed(Request::SourceRun(req)).await
    }

    pub async fn health(&self, conn: &str) -> Result<HealthInfo, EngineError> {
        self.typed(Request::Health { conn: conn.into() }).await
    }

    pub async fn flows(&self, names: Vec<String>, prod: bool) -> Result<Vec<FlowResult>, EngineError> {
        self.typed(Request::Flows { names, prod }).await
    }

    pub async fn board_list(&self) -> Result<Vec<BoardSummary>, EngineError> {
        self.typed(Request::BoardList).await
    }

    /// 흐름 아웃박스 한도 소진 수(대시보드 카드, R64)
    pub async fn outbox_exhausted(&self) -> Result<Vec<OutboxExhausted>, EngineError> {
        self.typed(Request::OutboxExhausted).await
    }

    pub async fn board_run(&self, name: &str, prod: bool) -> Result<BoardResult, EngineError> {
        self.typed(Request::BoardRun { name: name.into(), prod }).await
    }

    // ─── 복호화·전체 내보내기·접속 편집 ───

    /// 칸들 중 staticEncrypt 꼴을 그 접속 그룹 키로 serve 에서 푼다(R37). 화면은 `idb_core::statcrypt::looks_encrypted` 로
    /// 고른 칸만 보내면 된다. 키가 없거나 그룹을 모르면 code 2 이고 글은 상태 줄 글 그대로다.
    pub async fn decrypt(&self, conn: &str, cells: Vec<String>) -> Result<Decrypted, EngineError> {
        self.typed(Request::Decrypt { conn: conn.into(), cells }).await
    }

    /// 전체 내보내기(R33.1)를 시작한다. 파일은 화면 쪽이 쓴다(원격이어도 받은 바이트를 이 기계에 쓴다).
    pub async fn export_start(&self, conn: &str, sql: &str) -> Result<ExportStarted, EngineError> {
        self.typed(Request::ExportStart {
            conn: conn.into(),
            sql: sql.into(),
        })
        .await
    }

    /// 다음 조각이다. done 이 올 때까지 부른다. 퓨처를 버려도 조각은 잃지 않고 내보내기도 이어진다(멈추려면 export_cancel).
    pub async fn export_read(&self, export: u64) -> Result<ExportChunk, EngineError> {
        self.typed(Request::ExportRead { export }).await
    }

    pub async fn export_cancel(&self, export: u64) -> Result<bool, EngineError> {
        self.typed::<Canceled>(Request::ExportCancel { export }).await.map(|c| c.canceled)
    }

    /// db 전용 접속의 고칠 값이다(비밀번호 없음). 원격이면 code 3 remote_conn_edit 다.
    pub async fn conn_get(&self, name: &str) -> Result<ConnFields, EngineError> {
        self.typed(Request::ConnGet { name: name.into() }).await
    }

    /// 저장된 접속 name 을 시험한다.
    pub async fn conn_test_saved(&self, name: &str) -> Result<ConnTested, EngineError> {
        self.typed(Request::ConnTest {
            name: name.into(),
            conn: None,
            old_name: String::new(),
        })
        .await
    }

    /// 창의 값으로 시험한다. 비밀번호가 비면 old_name(없으면 conn.name)의 저장된 비밀번호를 쓴다.
    pub async fn conn_test(&self, conn: ConnFields, old_name: &str) -> Result<ConnTested, EngineError> {
        self.typed(Request::ConnTest {
            name: String::new(),
            conn: Some(conn),
            old_name: old_name.into(),
        })
        .await
    }

    /// old_name 이 비면 더하고 아니면 고친다. 성공하면 엔진의 접속 목록이 바뀌므로 화면은 project 를 다시 읽는다.
    pub async fn conn_save(&self, old_name: &str, conn: ConnFields) -> Result<ConnSaved, EngineError> {
        self.typed(Request::ConnSave {
            old_name: old_name.into(),
            conn,
        })
        .await
    }

    pub async fn conn_delete(&self, name: &str) -> Result<(), EngineError> {
        self.call(Request::ConnDelete { name: name.into() }).await.map(|_| ())
    }

    pub async fn conn_folder(&self, names: Vec<String>, folder: &str) -> Result<(), EngineError> {
        self.call(Request::ConnFolder {
            names,
            folder: folder.into(),
        })
        .await
        .map(|_| ())
    }

    pub async fn read_query(&self, conn: &str, sql: &str, limit: usize) -> Result<ResultSet, EngineError> {
        self.typed(Request::ReadQuery {
            conn: conn.into(),
            sql: sql.into(),
            limit,
        })
        .await
    }
}
