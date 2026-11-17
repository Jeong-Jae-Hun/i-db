//! Loki·Prometheus·Grafana 를 읽기 전용 저장소로 다룬다(명세 R60·R62). Go `internal/obs` 를 옮긴다.
//! 질의 API(labels·values·query·query_range)와 Grafana 데이터 소스 목록만 GET 으로 부르므로
//! 쓰기 요청은 만들 수 없다. 그래서 Error::Write 로 거를 문장도 없다.

mod grafana;
mod loki;
mod prom;
mod result;
#[cfg(test)]
mod tests;

use idb_core::gojson::json_error;
use idb_core::gostrconv::duration_string;
use idb_core::project::{Conn, go_quote};
use idb_core::redact::redact;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::value::RawValue;

pub use grafana::Grafana;
pub use loki::{Loki, open_query};
pub use prom::Prometheus;

use crate::httpc::{self, SendError, Values, encode};
use crate::opensearch::base64;
use crate::source::{CONFIRM_RANGE_NS, DEFAULT_RANGE_NS, Error, MAX_BODY, Query};

const MAX_ERR_BYTES: usize = 512;
const SECOND_NS: i64 = 1_000_000_000;
const EMPTY_QUERY: &str = "empty query";

/// 지금(유닉스 나노초)이다. 시험은 고정한 값을 쓰는 함수로 바꾼다.
pub(crate) type Now = fn() -> i64;

pub(crate) fn system_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as i64)
}

#[derive(Clone, Copy, PartialEq)]
enum Auth {
    Bearer,
    Basic,
    None,
}

/// 저장소 하나로 가는 HTTP GET 이다. base 는 끝 '/' 없는 주소이고,
/// Grafana 프록시 아래 Loki·Prometheus 는 같은 연결·인증에 base 만 다른 Client 를 쓴다.
#[derive(Clone)]
pub(crate) struct Client {
    /// 오류 앞머리(loki·prometheus·grafana)
    kind: String,
    base: String,
    hc: reqwest::Client,
    auth: Auth,
    /// basic 사용자
    user: String,
    /// basic 비밀번호·토큰. 오류에서 지운다
    secret: String,
}

impl Client {
    /// 접속 주소·인증을 확인한다. 주소에 user:pass 가 붙어 있으면 떼어 basic 인증으로 쓴다
    /// (오류 문구에 주소가 찍혀도 비밀번호가 나오지 않게).
    fn new(kind: &str, c: &Conn, bearer_only: bool) -> Result<Client, Error> {
        let bad = || Error::msg(format!("{kind}: url must be http(s)://host[:port][/path]"));
        let mut u = url::Url::parse(c.url.trim()).map_err(|_| bad())?;
        if !matches!(u.scheme(), "http" | "https") || u.host_str().is_none_or(str::is_empty) {
            return Err(bad());
        }
        let (mut user, mut pass) = (c.user.clone(), c.password.clone());
        let has_user = !u.username().is_empty() || u.password().is_some();
        if has_user && c.auth.is_empty() && !bearer_only {
            // 인증 없음인데 주소에 user:pass 가 있으면 조용히 버리지 않고 알린다(무엇을 보낼지 모호하다)
            return Err(Error::msg(format!("{kind}: url has user info but auth is none (set auth basic)")));
        }
        if has_user {
            let decode = |s: &str| percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned();
            if user.is_empty() {
                user = decode(u.username());
            }
            if let Some(p) = u.password()
                && pass.is_empty()
            {
                pass = decode(p);
            }
            let _ = u.set_username("");
            let _ = u.set_password(None);
        }
        let auth = match c.auth.as_str() {
            _ if bearer_only => Auth::Bearer,
            "token" => Auth::Bearer,
            "basic" => Auth::Basic,
            "" => Auth::None,
            other => {
                return Err(Error::msg(format!(
                    "{kind}: unsupported auth {} (basic, token or none)",
                    go_quote(other)
                )));
            }
        };
        Ok(Client {
            kind: kind.into(),
            base: u.as_str().trim_end_matches('/').to_string(),
            hc: httpc::client().map_err(|e| Error::msg(format!("{kind}: {e}")))?,
            auth,
            user,
            secret: pass,
        })
    }

    /// 같은 연결·인증으로 base 아래 경로에 붙는 Client 다(Grafana 데이터 소스 프록시).
    fn sub(&self, kind: &str, prefix: &str) -> Client {
        Client {
            kind: kind.into(),
            base: self.base.clone() + prefix,
            ..self.clone()
        }
    }

    /// base+path 를 GET 하고 2xx 본문을 돌려준다. 오류에는 서버 메시지를 담되 비밀값은 지운다.
    async fn get(&self, path: &str, params: &Values) -> Result<Vec<u8>, Error> {
        let mut target = self.base.clone() + path;
        if !params.is_empty() {
            target = format!("{target}?{}", encode(params));
        }
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        let auth = match self.auth {
            Auth::Bearer if !self.secret.is_empty() => Some(format!("Bearer {}", self.secret)),
            Auth::Basic => Some(format!("Basic {}", base64(format!("{}:{}", self.user, self.secret).as_bytes()))),
            _ => None,
        };
        if let Some(a) = auth {
            let mut v = HeaderValue::from_str(&a).map_err(|_| Error::msg(format!("{}: invalid credentials", self.kind)))?;
            v.set_sensitive(true);
            headers.insert(AUTHORIZATION, v);
        }
        let resp = match httpc::send(&self.hc, "GET", &target, headers, Vec::new()).await {
            Ok(r) => r,
            Err(SendError::Transport(e) | SendError::Read(e)) => return Err(self.redact(&e)),
        };
        if let Some(e) = resp.redirect_error() {
            return Err(Error::msg(format!("{}: {e}", self.kind)));
        }
        if resp.status / 100 != 2 {
            return Err(self.redact(&format!("{} {}: {}", self.kind, resp.status_text, server_message(&resp.body))));
        }
        if resp.body.len() > MAX_BODY {
            return Err(Error::msg(format!(
                "{}: response larger than {} MiB, narrow the query",
                self.kind,
                MAX_BODY >> 20
            )));
        }
        Ok(resp.body)
    }

    /// Loki·Prometheus 봉투({"status":"success","data":…})에서 data 를 꺼낸다.
    async fn get_data<T: DeserializeOwned>(&self, path: &str, params: &Values) -> Result<T, Error> {
        #[derive(Deserialize)]
        struct Envelope {
            #[serde(default, deserialize_with = "crate::opensearch::nullable")]
            status: String,
            #[serde(default)]
            data: Option<Box<RawValue>>,
            #[serde(default, deserialize_with = "crate::opensearch::nullable")]
            error: String,
        }
        let body = self.get(path, params).await?;
        let env: Envelope =
            serde_json::from_slice(&body).map_err(|e| Error::msg(format!("{}: unexpected response: {}", self.kind, json_error(&e))))?;
        if env.status != "success" {
            let why = if env.error.is_empty() { &env.status } else { &env.error };
            return Err(self.redact(&format!("{}: {why}", self.kind)));
        }
        let raw = env.data.as_ref().map_or("", |r| r.get());
        serde_json::from_str(raw).map_err(|e| Error::msg(format!("{}: unexpected data: {}", self.kind, json_error(&e))))
    }

    fn redact(&self, msg: &str) -> Error {
        Error::msg(redact(msg, &self.secret))
    }

    /// buildinfo 의 version 에 이름을 붙인다. Prometheus 는 봉투({"status","data"}) 안에,
    /// Loki 는 맨 위에 있다. 못 읽으면 빈 글이다.
    async fn build_version(&self, path: &str, name: &str) -> String {
        #[derive(Deserialize, Default)]
        struct Data {
            #[serde(default, deserialize_with = "crate::opensearch::nullable")]
            version: String,
        }
        #[derive(Deserialize)]
        struct Build {
            #[serde(default, deserialize_with = "crate::opensearch::nullable")]
            version: String,
            #[serde(default, deserialize_with = "crate::opensearch::null_default")]
            data: Data,
        }
        let Ok(body) = self.get(path, &Values::new()).await else {
            return String::new();
        };
        match serde_json::from_slice::<Build>(&body) {
            Ok(b) => {
                let v = format!("{}{}", b.version, b.data.version);
                let v = v.trim();
                if v.is_empty() { String::new() } else { format!("{name} {v}") }
            }
            Err(_) => String::new(),
        }
    }
}

/// Prometheus({"error"})·Grafana({"message"}) JSON 이면 그 메시지를, Loki 처럼 평문이면 본문을 줄여 쓴다.
fn server_message(body: &[u8]) -> String {
    #[derive(Deserialize)]
    struct M {
        #[serde(default, deserialize_with = "crate::opensearch::nullable")]
        error: String,
        #[serde(default, deserialize_with = "crate::opensearch::nullable")]
        message: String,
    }
    if let Ok(m) = serde_json::from_slice::<M>(body) {
        if !m.error.is_empty() {
            return m.error;
        }
        if !m.message.is_empty() {
            return m.message;
        }
    }
    let s = String::from_utf8_lossy(body);
    let s = s.trim();
    match httpc::head(s, MAX_ERR_BYTES) {
        Some(h) => h + "…",
        None => s.to_string(),
    }
}

/// Query 의 since·until 을 [start, end](유닉스 나노초)로 바꾼다. 7일을 넘으면 force 없이는 NeedsConfirm 이다.
fn window(q: &Query, now: i64) -> Result<(i64, i64), Error> {
    let since = if q.since_ns <= 0 { DEFAULT_RANGE_NS } else { q.since_ns };
    if q.until_ns < 0 || q.until_ns >= since {
        return Err(Error::msg(format!(
            "empty range: until {} must be shorter than since {}",
            duration_string(q.until_ns),
            duration_string(since)
        )));
    }
    if since - q.until_ns > CONFIRM_RANGE_NS && !q.force {
        return Err(Error::NeedsConfirm("범위 7일 초과".into()));
    }
    Ok((now - since, now - q.until_ns))
}

/// 범위 질의 해상도(초)다. 한 시계열 250점 안팎, 최소 15초(Prometheus 11,000점 상한에서 멀다).
fn step(start: i64, end: i64) -> String {
    ((end - start) / 250).max(15 * SECOND_NS).div_euclid(SECOND_NS).to_string()
}

fn limit_of(n: usize, def: usize, ceiling: usize) -> usize {
    let n = if n == 0 { def } else { n };
    if ceiling > 0 { n.min(ceiling) } else { n }
}

fn empty_query() -> Error {
    Error::msg(EMPTY_QUERY)
}
