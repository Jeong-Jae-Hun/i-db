#![allow(non_snake_case)]
//! PostgreSQL·Redis 클라이언트. Go `internal/db` 의 CLI 가 쓰는 쪽(backend·readonly·meta·export·schemadiff·linked)을 옮긴다.
//! 결과 글은 Go 판과 같아야 해서 PostgreSQL 은 텍스트 형식으로 받고 명령 태그를 그대로 싣는 작은 프로토콜 구현을 둔다
//! (tokio-postgres 는 결과를 바이너리로만 받고 명령 태그를 내주지 않는다).

mod dns;
pub mod linked;
#[cfg(test)]
mod live_tests;
mod net;
pub mod pg;
pub mod redis;
pub mod schemadiff;
mod tls;

use std::fmt;
use std::future::Future;

use tokio::time::Instant;

pub use pg::{Clients, Pg, TableRef};

/// 서버가 돌려준 오류다. 글은 pgx `PgError.Error()` 와 같다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PgError {
    pub severity: String,
    pub code: String,
    pub message: String,
}

impl fmt::Display for PgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {} (SQLSTATE {})", self.severity, self.message, self.code)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    Pg(PgError),
    /// 마감(--timeout 등)이 지났다. 글은 Go `context.DeadlineExceeded` 와 같다
    Deadline,
    Other(String),
}

impl Error {
    pub fn msg(s: impl Into<String>) -> Error {
        Error::Other(s.into())
    }

    /// 서버 statement_timeout 으로 멈춘 오류(57014)인지다.
    pub fn is_statement_timeout(&self) -> bool {
        matches!(self, Error::Pg(e) if e.code == "57014")
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Pg(e) => e.fmt(f),
            Error::Deadline => f.write_str("context deadline exceeded"),
            Error::Other(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for Error {}

/// fut 를 deadline 까지만 기다린다. 넘으면 Error::Deadline 이고 fut 는 버린다(연결도 함께 닫힌다).
pub async fn with_deadline<T>(deadline: Instant, fut: impl Future<Output = Result<T, Error>>) -> Result<T, Error> {
    match tokio::time::timeout_at(deadline, fut).await {
        Ok(r) => r,
        Err(_) => Err(Error::Deadline),
    }
}
