#![allow(non_snake_case)]
//! TUI·GUI·CLI 가 같이 쓰는 데이터 쪽 API. 화면은 이 Engine 만 부르고, 접속이 이 기계에 있든(Local)
//! ssh 너머 `db serve` 에 있든(Remote) 같은 요청·응답 모양으로 돈다. Local 도 요청을 JSON 값으로 거쳐
//! 두 길의 뜻이 갈리지 않게 한다. 안전장치와 가리기는 늘 접속이 있는 쪽(serve)이 정한다.

pub mod local;
pub mod protocol;
pub mod remote;
pub mod serve;

use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde_json::Value;

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
}
