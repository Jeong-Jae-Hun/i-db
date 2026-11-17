//! PostgreSQL·Redis 밖의 읽기 전용 저장소를 같은 트리·결과 표로 다루는 공통 틀이다(명세 R60~R63). Go `internal/source` 를 옮긴다.
//! 종류마다 모듈이 같은 모양의 children·run·open·probe 를 두고, 통합(CLI)은 `Backend` 하나로 부른다.

use std::fmt;
use std::time::Duration;

use idb_core::project::{Conn, Kind};
use idb_core::redact::redact;
use idb_core::result::QueryResult;
use idb_core::sqlsafe::{is_ident_byte, mask_sql};

use crate::obs::{Grafana, Loki, Prometheus};
use crate::opensearch::OpenSearch;

/// 트리 한 줄이다. path 는 children·open 에 그대로 돌려줄 종류별 경로다(예: "index/field").
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Node {
    pub name: String,
    pub path: String,
    /// 타입·개수 같은 오른쪽 흐린 글
    pub info: String,
    pub has_children: bool,
    /// Enter 로 open 할 수 있다(인덱스·테이블·라벨 값)
    pub openable: bool,
    /// info 를 경고 색으로 그린다(예: DLQ 에 메시지가 있는 SQS 큐, R67.2)
    pub warn: bool,
}

/// 편집기 실행 한 번이다. since·until 은 Loki·Prometheus 범위(나노초)이고 0 이면 종류 기본값(1시간)을 쓴다.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Query {
    pub text: String,
    pub since_ns: i64,
    pub until_ns: i64,
    /// 행 상한. 0 이면 1,000
    pub limit: usize,
    /// NeedsConfirm 을 확인받은 뒤 다시 보낼 때
    pub force: bool,
    /// Loki 로그 질의 방향이다. "forward" 면 오래된 것부터, 비거나 "backward" 면 최신부터다(R62.3)
    pub direction: String,
}

// ─── 오류 ───

const ERR_WRITE: &str = "read-only source: write requests are not sent";
const ERR_NEEDS_CONFIRM: &str = "expensive query needs confirmation";

/// 요금이 붙는 조회를 여는 환경 변수다. 기본은 막음이다(R72).
pub const ALLOW_BILLED_ENV: &str = "DB_ALLOW_BILLED";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// 읽기 전용 저장소에 쓰기로 보이는 질의다. 보내기 전에 거절한다. 글은 까닭(낱말·경로)이고 비어도 된다
    Write(String),
    /// 전체 스캔·큰 범위처럼 비싼 질의다. 통합이 확인받은 뒤 Query.force 로 다시 보낸다. 글은 한국어 까닭이다(R60.2)
    NeedsConfirm(String),
    /// 요청·스캔마다 요금이 붙는 AWS 조회를 허용 없이 부르려 했다(R72). 보내기 전에 돌려준다
    Billed,
    Other(String),
}

impl Error {
    pub fn msg(s: impl Into<String>) -> Error {
        Error::Other(s.into())
    }

    /// NeedsConfirm 에 실린 까닭이다. 아니면 빈 글이다(Go `ConfirmReason`).
    pub fn confirm_reason(&self) -> &str {
        match self {
            Error::NeedsConfirm(r) => r,
            _ => "",
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (base, detail) = match self {
            Error::Write(d) => (ERR_WRITE, d.as_str()),
            Error::NeedsConfirm(r) => (ERR_NEEDS_CONFIRM, r.as_str()),
            Error::Billed => return write!(f, "요금이 붙는 조회라 막음 ({ALLOW_BILLED_ENV}=1 로 허용)"),
            Error::Other(s) => return f.write_str(s),
        };
        if detail.is_empty() {
            f.write_str(base)
        } else {
            write!(f, "{base}: {detail}")
        }
    }
}

impl std::error::Error for Error {}

/// 요금이 붙는 조회(SQS·SNS·S3·Athena·DynamoDB 데이터 읽기)를 보내기 전에 부른다. DB_ALLOW_BILLED=1 이면 Ok 다.
/// 호출마다 환경을 읽어 시험이 바꿀 수 있다.
pub fn billed() -> Result<(), Error> {
    if std::env::var(ALLOW_BILLED_ENV).is_ok_and(|v| v == "1") {
        Ok(())
    } else {
        Err(Error::Billed)
    }
}

// ─── 종류 공통 값 (R60~R63) ───

/// HTTP 응답 본문 상한이다. 결과 표는 MAX_ROWS 행이라 이보다 큰 응답은 질의가 잘못된 것으로 본다
pub const MAX_BODY: usize = 16 << 20;
/// 트리 Enter 미리 보기 행 수다(R61.1·R63.1). Loki 만 LOKI_OPEN_LIMIT 줄이다(R62.1)
pub const OPEN_LIMIT: usize = 50;
pub const LOKI_OPEN_LIMIT: usize = 100;
const HOUR_NS: i64 = 3_600_000_000_000;
/// Loki·Prometheus 범위·라벨 값 기본이다(R62)
pub const DEFAULT_RANGE_NS: i64 = HOUR_NS;
/// 이 범위를 넘는 Loki·Prometheus 질의는 확인을 받는다(R60.2)
pub const CONFIRM_RANGE_NS: i64 = 7 * 24 * HOUR_NS;

/// SQL 한 문장을 보낼 글과 문자열·주석·"따옴표 이름" 밖의 낱말(소문자)로 나눈다. 끝의 ;·공백·주석은 떼고,
/// 그 뒤에도 ; 가 남으면 여러 문장이라 Write 다(R61.2·R68.2). 어휘는 mask_sql 로 읽는다. 종류마다 낱말로 쓰기를 가른다.
/// 단, mask_sql 은 PostgreSQL 의 $태그$ 도 문자열로 본다. OpenSearch·Athena SQL 에는 없는 꼴이라 그 안에 숨긴 글은 서버가 문법 오류로 거절한다.
pub fn sql_words(text: &str) -> Result<(String, Vec<String>), Error> {
    let masked = mask_sql(text);
    let n = masked.trim_end_matches([';', ' ', '\t', '\r', '\n', '\x0c', '\x0b']).len();
    let masked = &masked[..n];
    if masked.contains(';') {
        return Err(Error::Write("multiple statements".into()));
    }
    let words = masked
        .to_lowercase()
        .split(|c: char| c as u32 > 0x7f || !is_ident_byte(c as u8))
        .filter(|w| !w.is_empty())
        .map(String::from)
        .collect();
    // 가린 글은 원문과 길이·글자 경계가 같아 n 이 원문에서도 글자 경계다
    let stmt = text.get(..n).unwrap_or(text).trim().to_string();
    Ok((stmt, words))
}

// ─── 대시보드 확인 (R64.2) ───

/// 대시보드 한 칸이다. warn 이면 눈에 띄게 그린다(임계치를 넘음). name 이 비면 값만 보인다(`green`·`ready`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Metric {
    pub name: String,
    pub value: String,
    pub warn: bool,
}

impl Metric {
    pub fn new(name: &str, value: impl Into<String>, warn: bool) -> Metric {
        Metric {
            name: name.into(),
            value: value.into(),
            warn,
        }
    }
}

impl fmt::Display for Metric {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(format!("{} {}", self.name, self.value).trim())
    }
}

/// 접속 하나를 가볍게 살펴본 결과다. 읽기 전용 메타데이터만 묻는다(Go `db.Health`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Health {
    pub ok: bool,
    pub latency: Duration,
    pub version: String,
    pub metrics: Vec<Metric>,
    /// 비밀값을 지운 한 줄
    pub err: String,
}

impl Health {
    /// 주요 수치를 ` · ` 로 이은 한 줄이다. 예: `green · 문서 120만 · 인덱스 37`.
    pub fn summary(&self) -> String {
        self.metrics.iter().map(Metric::to_string).collect::<Vec<_>>().join(" · ")
    }

    /// 확인 실패 결과다. 오류에서 secret 을 지우고 여러 줄이면 한 줄로 붙인다.
    pub fn failed(latency: Duration, err: &str, secret: &str) -> Health {
        Health {
            latency,
            err: redact(err, secret).split_whitespace().collect::<Vec<_>>().join(" "),
            ..Default::default()
        }
    }
}

// ─── 접속 하나 ───

/// 저장소 접속 하나다. 모두 읽기 전용이고, 쓰기로 보이는 질의는 run 이 보내기 전에 Error::Write 로 거절한다.
pub enum Backend {
    OpenSearch(OpenSearch),
    Loki(Loki),
    Prometheus(Prometheus),
    Grafana(Grafana),
}

impl Backend {
    /// c 의 종류에 맞는 접속을 만든다. 네트워크는 첫 요청 때 쓴다. 아직 옮기지 않은 종류는 오류다.
    pub async fn open(c: &Conn) -> Result<Backend, Error> {
        Ok(match c.kind {
            Some(Kind::OpenSearch) => Backend::OpenSearch(OpenSearch::new(c).await?),
            Some(Kind::Loki) => Backend::Loki(Loki::new(c)?),
            Some(Kind::Prometheus) => Backend::Prometheus(Prometheus::new(c)?),
            Some(Kind::Grafana) => Backend::Grafana(Grafana::new(c)?),
            _ => return Err(Error::msg(format!("{} source is not built in", c.kind_str()))),
        })
    }

    /// path "" 는 맨 위다.
    pub async fn children(&self, path: &str) -> Result<Vec<Node>, Error> {
        match self {
            Backend::OpenSearch(b) => b.children(path).await,
            Backend::Loki(b) => b.children(path).await,
            Backend::Prometheus(b) => b.children(path).await,
            Backend::Grafana(b) => b.children(path).await,
        }
    }

    pub async fn run(&self, q: &Query) -> Result<QueryResult, Error> {
        match self {
            Backend::OpenSearch(b) => b.run(q).await,
            Backend::Loki(b) => b.run(q).await,
            Backend::Prometheus(b) => b.run(q).await,
            Backend::Grafana(b) => b.run(q).await,
        }
    }

    /// 트리 Enter 미리 보기다.
    pub async fn open_path(&self, path: &str, limit: usize) -> Result<QueryResult, Error> {
        match self {
            Backend::OpenSearch(b) => b.open(path, limit).await,
            Backend::Loki(b) => b.open(path, limit).await,
            Backend::Prometheus(b) => b.open(path, limit).await,
            Backend::Grafana(b) => b.open(path, limit).await,
        }
    }

    /// 대시보드 확인이다. 제한 시간은 호출부가 건다.
    pub async fn probe(&self) -> Health {
        match self {
            Backend::OpenSearch(b) => b.probe().await,
            Backend::Loki(b) => b.probe().await,
            Backend::Prometheus(b) => b.probe().await,
            Backend::Grafana(b) => b.probe().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 오류_글과_확인_까닭은_Go_와_같다() {
        assert_eq!(Error::Write(String::new()).to_string(), ERR_WRITE);
        assert_eq!(Error::Write("delete".into()).to_string(), format!("{ERR_WRITE}: delete"));
        let e = Error::NeedsConfirm("범위 7일 초과".into());
        assert_eq!(e.to_string(), format!("{ERR_NEEDS_CONFIRM}: 범위 7일 초과"));
        assert_eq!(e.confirm_reason(), "범위 7일 초과");
        assert_eq!(Error::msg("x").confirm_reason(), "");
        assert_eq!(Error::Billed.to_string(), "요금이 붙는 조회라 막음 (DB_ALLOW_BILLED=1 로 허용)");
    }

    #[test]
    fn SQL_낱말은_문자열_주석_밖만_세고_끝_세미콜론은_뗀다() {
        let (stmt, words) = sql_words("SELECT a FROM t WHERE b = 'delete' -- drop\n ;; ").unwrap();
        assert_eq!(stmt, "SELECT a FROM t WHERE b = 'delete'");
        assert_eq!(words, ["select", "a", "from", "t", "where", "b"]);
        assert_eq!(sql_words("select 1; select 2"), Err(Error::Write("multiple statements".into())));
    }

    #[test]
    fn 확인_실패는_비밀값을_지우고_한_줄로_붙인다() {
        let h = Health::failed(Duration::ZERO, "bad\n  pw hunter2", "hunter2");
        assert_eq!(h.err, "bad pw ***");
        assert!(!h.ok);
        let h = Health {
            metrics: vec![Metric::new("", "green", false), Metric::new("문서", "3", false)],
            ..Default::default()
        };
        assert_eq!(h.summary(), "green · 문서 3");
    }
}
