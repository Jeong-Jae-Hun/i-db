//! PostgreSQL·Redis 밖의 읽기 전용 저장소(R60~R63, R67·R68)를 같은 트리·결과로 다루는 틀이다. Go `internal/source` 와
//! 루트 `sources.go` 의 newSource·testConn 이다. 종류마다 모듈이 Backend 를 구현하고 CLI 는 이 트레이트만 본다.

use std::future::Future;
use std::pin::Pin;

use idb_core::project::{Conn, Kind};
use idb_core::redact::redact;
use idb_core::result::QueryResult;
use idb_core::sqlsafe::{is_ident_byte, mask_sql};

use crate::Error;
use crate::health::Health;

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
#[derive(Clone, Debug, Default)]
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
    /// 호출부 마감(Go ctx 기한). 받은 데까지 돌려줄 수 있는 종류(DynamoDB 페이지 잇기)가 본다. None 이면 종류 기본 상한이다
    pub deadline: Option<tokio::time::Instant>,
}

/// 트리 Enter 미리 보기 행 수다(R61.1·R63.1). Loki 만 LOKI_OPEN_LIMIT 줄이다(R62.1)
pub const OPEN_LIMIT: usize = 50;
pub const LOKI_OPEN_LIMIT: usize = 100;

/// 요금이 붙는 조회를 여는 환경 변수다. 기본은 막음이다(R72).
pub const ALLOW_BILLED_ENV: &str = "DB_ALLOW_BILLED";

/// 요금이 붙는 조회(SQS·SNS·S3·Athena·DynamoDB 데이터 읽기)를 보내기 전에 부른다. DB_ALLOW_BILLED=1 이면 Ok, 아니면 Error::Billed 다.
/// 호출마다 환경을 읽는다.
pub fn billed() -> Result<(), Error> {
    #[cfg(test)]
    if let Some(allow) = BILLED_FOR_TEST.with(|c| c.get()) {
        return if allow { Ok(()) } else { Err(Error::Billed) };
    }
    if std::env::var(ALLOW_BILLED_ENV).is_ok_and(|v| v == "1") {
        Ok(())
    } else {
        Err(Error::Billed)
    }
}

#[cfg(test)]
thread_local! {
    /// 시험이 환경 변수(프로세스 전체) 대신 스레드마다 요금 허용을 정한다. 시험들이 동시에 돌아서다
    static BILLED_FOR_TEST: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// 이 스레드의 요금 허용을 정한다(시험 전용).
#[cfg(test)]
pub(crate) fn set_billed_for_test(allow: bool) {
    BILLED_FOR_TEST.with(|c| c.set(Some(allow)));
}

/// err 에 실린 확인 까닭이다. NeedsConfirm 이 아니거나 까닭이 없으면 빈 글이다.
pub fn confirm_reason(err: &Error) -> String {
    match err {
        Error::NeedsConfirm(r) => r.clone(),
        _ => String::new(),
    }
}

/// SQL 한 문장을 보낼 글과 문자열·주석·"따옴표 이름" 밖의 낱말(소문자)로 나눈다. 끝의 ;·공백·주석은 떼고,
/// 그 뒤에도 ; 가 남으면 여러 문장이라 Error::Write 다(R61.2·R68.2).
pub fn sql_words(text: &str) -> Result<(String, Vec<String>), Error> {
    let masked = mask_sql(text);
    let n = masked.trim_end_matches([';', ' ', '\t', '\r', '\n', '\x0c', '\x0b']).len();
    let masked = &masked[..n];
    if masked.contains(';') {
        return Err(Error::Write("multiple statements".into()));
    }
    let words = masked
        .to_ascii_lowercase()
        .split(|c: char| (c as u32) > 0x7f || !is_ident_byte(c as u8))
        .filter(|w| !w.is_empty())
        .map(String::from)
        .collect();
    Ok((text[..n].trim().to_string(), words))
}

pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// 한 접속이다. 모두 읽기 전용이고, 쓰기로 보이는 질의는 run 이 보내기 전에 Error::Write 로 거절한다.
pub trait Backend: Send + Sync {
    /// path "" 는 맨 위다
    fn children<'a>(&'a self, path: &'a str) -> BoxFut<'a, Result<Vec<Node>, Error>>;
    fn run<'a>(&'a self, q: &'a Query) -> BoxFut<'a, Result<QueryResult, Error>>;
    /// 트리 Enter 미리 보기
    fn open<'a>(&'a self, path: &'a str, limit: usize) -> BoxFut<'a, Result<QueryResult, Error>>;
    /// 대시보드(R64.2)용 가벼운 확인이다. 없는 종류는 None 이다. 제한 시간은 호출부가 건다.
    fn probe(&self) -> Option<BoxFut<'_, Health>> {
        None
    }
}

/// 접속 c 의 Backend 를 연다. 아직 옮기지 않은 종류는 `<종류> source is not built in` 오류다(Go 의 등록 안 된 종류와 같은 글).
pub async fn open(c: &Conn) -> Result<Box<dyn Backend>, Error> {
    match c.kind {
        Some(Kind::DynamoDb) => Ok(Box::new(crate::dynamo::Backend::new(c).await?)),
        Some(Kind::Athena) => Ok(Box::new(crate::athena::Backend::new(c).await?)),
        Some(Kind::Aws) => Ok(Box::new(crate::awsres::Backend::new(c).await?)),
        _ => Err(Error::msg(format!("{} source is not built in", c.kind_str()))),
    }
}

/// 종류의 보일 이름이다(Go connKinds).
pub fn kind_label(k: Option<Kind>) -> &'static str {
    match k {
        Some(Kind::Pg) => "PostgreSQL",
        Some(Kind::Redis) => "Redis",
        Some(Kind::OpenSearch) => "OpenSearch",
        Some(Kind::Loki) => "Loki",
        Some(Kind::Prometheus) => "Prometheus",
        Some(Kind::Grafana) => "Grafana",
        Some(Kind::DynamoDb) => "DynamoDB",
        Some(Kind::Athena) => "Athena",
        Some(Kind::Aws) => "AWS",
        None => "",
    }
}

/// 접속 창·db conn 의 시험이다(Go main testConn). 읽기 전용 저장소는 맨 위 children("") 가 되면 접속된 것으로 본다.
/// 맨 위 목록부터 요금이 붙는 종류(Athena 는 Glue 카탈로그, R72)는 막혀 있으면 목록을 보내지 않고, 요금 없는 확인
/// (Athena GetWorkGroup)으로 자격 증명만 본다. 확인도 없는 종류는 시험을 건너뛰었다고 알린다. 오류에서 비밀값을 지운다.
pub async fn test_conn(c: &Conn) -> Result<String, String> {
    if !c.kind.is_some_and(Kind::is_source) {
        return crate::health::test_db_conn(c).await;
    }
    let label = kind_label(c.kind);
    let err = match open(c).await {
        Ok(b) => match b.children("").await {
            Ok(nodes) => return Ok(format!("{label} · {}개", nodes.len())),
            Err(Error::Billed) => match b.probe() {
                None => return Ok(format!("{label} · 시험 건너뜀 ({})", Error::Billed)),
                Some(p) => {
                    let h = p.await;
                    if h.ok {
                        return Ok(format!("{label} · 목록은 요금이 붙어 시험 안 함"));
                    }
                    h.err
                }
            },
            Err(e) => e.to_string(),
        },
        Err(e) => e.to_string(),
    };
    Err(redact(&err, &c.password))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 한_문장만_낱말로_나누고_끝_세미콜론은_뗀다() {
        let (stmt, words) = sql_words("SELECT a FROM \"T\" -- x;\n ;").unwrap();
        assert_eq!(stmt, "SELECT a FROM \"T\"");
        assert_eq!(words, ["select", "a", "from"]);
        assert_eq!(
            sql_words("select 1; drop t").unwrap_err(),
            Error::Write("multiple statements".into())
        );
    }
}
