#![allow(non_snake_case)]
//! PostgreSQL·Redis 클라이언트. Go `internal/db` 의 CLI 가 쓰는 쪽(backend·readonly·meta·export·schemadiff·linked)을 옮긴다.
//! 결과 글은 Go 판과 같아야 해서 PostgreSQL 은 텍스트 형식으로 받고 명령 태그를 그대로 싣는 작은 프로토콜 구현을 둔다
//! (tokio-postgres 는 결과를 바이너리로만 받고 명령 태그를 내주지 않는다).
//! 읽기 전용 저장소(R60~)는 source 틀 아래 종류마다 모듈을 둔다(dynamo·athena·awsres).

pub mod athena;
mod aws;
pub mod awsres;
mod dns;
pub mod dynamo;
pub mod health;
pub mod linked;
#[cfg(test)]
mod live_tests;
mod net;
pub mod pg;
pub mod redis;
pub mod schemadiff;
pub mod source;
mod tls;

use std::fmt;
use std::future::Future;

use tokio::time::Instant;

pub use pg::{
    Applied, Catalog, CatalogColumn, CatalogSchema, CatalogTable, Change, ChangeOp, ChangeSet, Clients, ERR_NOT_FOUND, Fk, Object, Origin,
    Pg, Ran, Session, TableRef, changes_sql, fk_where, undo_changes,
};

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
    /// 읽기 전용 저장소에 쓰기로 보이는 질의를 보냈다(Go source.ErrWrite). 글은 덧붙일 까닭이다(비면 없음)
    Write(String),
    /// 전체 스캔·큰 범위처럼 비싼 질의다(Go source.ErrNeedsConfirm). 글은 확인 까닭(한국어)이다
    NeedsConfirm(String),
    /// 요청·스캔마다 요금이 붙는 AWS 조회를 허용 없이 부르려 했다(Go source.ErrBilled, R72). 보내기 전에 돌려준다
    Billed,
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

const ERR_WRITE: &str = "read-only source: write requests are not sent";
const ERR_NEEDS_CONFIRM: &str = "expensive query needs confirmation";

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Pg(e) => e.fmt(f),
            Error::Deadline => f.write_str("context deadline exceeded"),
            Error::Other(s) => f.write_str(s),
            Error::Write(d) if d.is_empty() => f.write_str(ERR_WRITE),
            Error::Write(d) => write!(f, "{ERR_WRITE}: {d}"),
            Error::NeedsConfirm(r) if r.is_empty() => f.write_str(ERR_NEEDS_CONFIRM),
            Error::NeedsConfirm(r) => write!(f, "{ERR_NEEDS_CONFIRM}: {r}"),
            Error::Billed => write!(f, "요금이 붙는 조회라 막음 ({}=1 로 허용)", source::ALLOW_BILLED_ENV),
        }
    }
}

impl std::error::Error for Error {}

/// 작업들을 동시에 돌려 (자리, 결과)를 모은다. 순서는 끝난 순서다.
/// 패닉한 작업은 그 자리의 `internal error` 다. 패닉 값에는 SQL·서버 응답이 섞여 비밀번호가 나올 수 있어 싣지 않는다.
pub async fn join_indexed<T, F>(jobs: impl IntoIterator<Item = (usize, F)>) -> Vec<(usize, Result<T, Error>)>
where
    T: Send + 'static,
    F: Future<Output = T> + Send + 'static,
{
    let mut set = tokio::task::JoinSet::new();
    let mut at = std::collections::HashMap::new();
    for (i, f) in jobs {
        at.insert(set.spawn(f).id(), i);
    }
    let mut out = Vec::with_capacity(at.len());
    while let Some(done) = set.join_next_with_id().await {
        match done {
            Ok((id, v)) => out.push((at[&id], Ok(v))),
            Err(e) => out.push((at[&e.id()], Err(Error::msg("internal error")))),
        }
    }
    out
}

/// fut 를 deadline 까지만 기다린다. 넘으면 Error::Deadline 이고 fut 는 버린다(연결도 함께 닫힌다).
pub async fn with_deadline<T>(deadline: Instant, fut: impl Future<Output = Result<T, Error>>) -> Result<T, Error> {
    match tokio::time::timeout_at(deadline, fut).await {
        Ok(r) => r,
        Err(_) => Err(Error::Deadline),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn 패닉한_작업은_그_자리의_오류가_되고_값은_싣지_않는다() {
        let jobs = (0..3).map(|i| {
            (i, async move {
                if i == 1 {
                    panic!("secret-pw-9");
                }
                i * 10
            })
        });
        let mut got = join_indexed(jobs).await;
        got.sort_by_key(|(i, _)| *i);
        assert_eq!(got, [(0, Ok(0)), (1, Err(Error::msg("internal error"))), (2, Ok(20))]);
    }
}
