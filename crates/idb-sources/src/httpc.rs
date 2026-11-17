//! HTTP 저장소(OpenSearch·Loki·Prometheus·Grafana) 공용 요청. Go `net/http`·`net/url` 의 쓰임을 맞춘다.
//! 리다이렉트는 따라가지 않는다(R61.2): 따라가면 basic·토큰 인증 헤더를 다른 주소로 다시 보내거나 허용 경로 검사를 우회한다.
//! 오류 글은 Go `*url.Error` 꼴(`Get "http://…": dial tcp 127.0.0.1:1: connect: connection refused`)로 맞춘다.

use std::collections::BTreeMap;
use std::error::Error as _;
use std::time::Duration;

use idb_core::project::go_quote;
use reqwest::header::HeaderMap;

use crate::net::io_reason;
use crate::source::MAX_BODY;

/// Go `source.HTTPClient` 의 2분 제한이다
const HTTP_TIMEOUT: Duration = Duration::from_secs(120);

/// 저장소 HTTP 클라이언트다. 인증서는 시스템 루트로 이름까지 검증한다(Go 기본 전송과 같다).
pub(crate) fn client() -> Result<reqwest::Client, String> {
    let tls = crate::tls::verified_config()?;
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(HTTP_TIMEOUT)
        .tls_backend_preconfigured(tls)
        .build()
        .map_err(|e| e.to_string())
}

/// 받은 응답이다. 3xx 면 본문은 읽지 않는다. 본문은 MAX_BODY+1 바이트까지만 읽어 넘침을 알 수 있게 한다.
pub(crate) struct Response {
    pub status: u16,
    /// Go `resp.Status` 꼴(`404 Not Found`)
    pub status_text: String,
    pub body: Vec<u8>,
}

impl Response {
    /// 3xx 면 Go `source.RedirectError` 의 글이다.
    pub fn redirect_error(&self) -> Option<String> {
        (self.status / 100 == 3).then(|| format!("redirect not followed: {}", self.status_text))
    }
}

pub(crate) enum SendError {
    /// 연결·요청 실패. 글은 Go `*url.Error` 꼴이다
    Transport(String),
    /// 본문을 읽다 끊겼다
    Read(String),
}

/// method 로 target 에 요청을 보낸다. body 가 비면 본문을 싣지 않는다.
pub(crate) async fn send(
    client: &reqwest::Client,
    method: &str,
    target: &str,
    headers: HeaderMap,
    body: Vec<u8>,
) -> Result<Response, SendError> {
    let transport = |reason: String| SendError::Transport(format!("{} {}: {reason}", url_error_op(method), go_quote(target)));
    let url = reqwest::Url::parse(target).map_err(|e| transport(e.to_string()))?;
    let m = reqwest::Method::from_bytes(method.as_bytes()).map_err(|e| transport(e.to_string()))?;
    let mut req = client.request(m, url.clone()).headers(headers);
    if !body.is_empty() {
        req = req.body(body);
    }
    let mut resp = req.send().await.map_err(|e| transport(reason(&e, &url)))?;
    let status = resp.status();
    let status_text = format!("{} {}", status.as_u16(), status.canonical_reason().unwrap_or(""));
    let mut out = Response {
        status: status.as_u16(),
        status_text: status_text.trim_end().to_string(),
        body: Vec::new(),
    };
    if out.redirect_error().is_some() {
        return Ok(out);
    }
    while let Some(chunk) = resp.chunk().await.map_err(|e| SendError::Read(reason(&e, &url)))? {
        let room = MAX_BODY + 1 - out.body.len();
        out.body.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if out.body.len() > MAX_BODY {
            break;
        }
    }
    Ok(out)
}

/// Go `http.Client` 의 urlErrorOp: 메서드 첫 글자만 대문자다(GET → Get).
fn url_error_op(method: &str) -> String {
    let mut c = method.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c.flat_map(char::to_lowercase)).collect(),
        None => String::new(),
    }
}

/// reqwest 오류를 Go 전송 오류의 까닭 글로 바꾼다.
/// 단, 이름으로 연 연결이 거절되면 Go 는 푼 IP 를 찍지만 여기서는 이름:포트를 찍는다(reqwest 가 푼 주소를 내주지 않는다).
fn reason(e: &reqwest::Error, url: &reqwest::Url) -> String {
    if e.is_timeout() {
        return "context deadline exceeded (Client.Timeout exceeded while awaiting headers)".into();
    }
    let host = url.host_str().unwrap_or("");
    let ip_host = matches!(url.host(), Some(url::Host::Ipv4(_) | url::Host::Ipv6(_)));
    let port = url.port_or_known_default().unwrap_or(0);
    let mut src = e.source();
    while let Some(s) = src {
        if let Some(io) = s.downcast_ref::<std::io::Error>() {
            if e.is_connect() && io.kind() != std::io::ErrorKind::Other {
                return format!("dial tcp {host}:{port}: connect: {}", io_reason(io));
            }
            if e.is_connect() && !ip_host {
                return format!("dial tcp: lookup {}: no such host", host.trim_matches(['[', ']']));
            }
            return io_reason(io);
        }
        if let Some(te) = s.downcast_ref::<rustls::Error>() {
            return te.to_string();
        }
        src = s.source();
    }
    // 꼬리의 원인까지 이어 쓴다(reqwest 의 맨 위 글은 "error sending request" 뿐이다)
    let mut msg = e.to_string();
    let mut src = e.source();
    while let Some(s) = src {
        msg = format!("{msg}: {s}");
        src = s.source();
    }
    msg
}

// ─── Go net/url 이스케이프 ───

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Path,
    PathSegment,
    QueryComponent,
}

/// Go `url.shouldEscape` 의 쓰는 갈래(경로·경로 마디·질의 값)다.
fn should_escape(c: u8, mode: Mode) -> bool {
    if c.is_ascii_alphanumeric() {
        return false;
    }
    match c {
        b'-' | b'_' | b'.' | b'~' => false,
        b'$' | b'&' | b'+' | b',' | b'/' | b':' | b';' | b'=' | b'?' | b'@' => match mode {
            Mode::Path => c == b'?',
            Mode::PathSegment => matches!(c, b'/' | b';' | b',' | b'?'),
            Mode::QueryComponent => true,
        },
        _ => true,
    }
}

fn escape(s: &str, mode: Mode) -> String {
    let mut out = String::with_capacity(s.len());
    for &c in s.as_bytes() {
        if !should_escape(c, mode) {
            out.push(c as char);
        } else if c == b' ' && mode == Mode::QueryComponent {
            out.push('+');
        } else {
            out.push_str(&format!("%{c:02X}"));
        }
    }
    out
}

/// Go `url.PathEscape`(경로 한 마디)다.
pub(crate) fn path_escape(s: &str) -> String {
    escape(s, Mode::PathSegment)
}

/// Go `url.URL.EscapedPath` 가 RawPath 없이 Path 를 쓰는 꼴이다.
pub(crate) fn escape_path(s: &str) -> String {
    escape(s, Mode::Path)
}

/// Go `url.Values`: 키 이름순, 같은 키 안은 넣은 순서다.
pub(crate) type Values = BTreeMap<String, Vec<String>>;

/// Go `url.Values.Encode` 다.
pub(crate) fn encode(v: &Values) -> String {
    let mut parts = Vec::new();
    for (k, vs) in v {
        for val in vs {
            parts.push(format!("{}={}", escape(k, Mode::QueryComponent), escape(val, Mode::QueryComponent)));
        }
    }
    parts.join("&")
}

/// 키·값 쌍으로 Values 를 만든다(값 하나씩).
pub(crate) fn values<const N: usize>(pairs: [(&str, String); N]) -> Values {
    pairs.into_iter().map(|(k, v)| (k.to_string(), vec![v])).collect()
}

/// Go `url.ParseQuery` 다. 읽지 못한 쌍은 건너뛰고 첫 오류를 같이 돌려준다.
pub(crate) fn parse_query(raw: &str) -> (Values, Option<String>) {
    let mut m = Values::new();
    let mut err = None;
    for pair in raw.split('&') {
        if pair.contains(';') {
            err.get_or_insert_with(|| "invalid semicolon separator in query".to_string());
            continue;
        }
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        match (query_unescape(k), query_unescape(v)) {
            (Ok(k), Ok(v)) => m.entry(k).or_default().push(v),
            (Err(e), _) | (_, Err(e)) => {
                err.get_or_insert(e);
            }
        }
    }
    (m, err)
}

/// Go `url.QueryUnescape` 다. 잘못된 `%` 는 Go `EscapeError` 글이다.
fn query_unescape(s: &str) -> Result<String, String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                if i + 2 >= b.len() || !b[i + 1].is_ascii_hexdigit() || !b[i + 2].is_ascii_hexdigit() {
                    // Go 처럼 바이트 3개로 자른다. 글자 중간에서 잘릴 수 있어 남은 바이트는 \xNN 으로 적는다(strconv.Quote)
                    return Err(format!("invalid URL escape {}", go_quote_bytes(&b[i..b.len().min(i + 3)])));
                }
                let hex = |c: u8| (c as char).to_digit(16).unwrap_or(0) as u8;
                out.push(hex(b[i + 1]) << 4 | hex(b[i + 2]));
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// Go `strconv.Quote(string(b))` 다. 깨진 UTF-8 바이트는 `\xNN` 이다.
fn go_quote_bytes(b: &[u8]) -> String {
    let mut out = String::from("\"");
    for chunk in b.utf8_chunks() {
        let q = go_quote(chunk.valid());
        out.push_str(&q[1..q.len() - 1]);
        for x in chunk.invalid() {
            out.push_str(&format!("\\x{x:02x}"));
        }
    }
    out.push('"');
    out
}

/// Go `strings.ToValidUTF8(s[:n], "")` 꼴로 앞 n 바이트만 남긴다.
pub(crate) fn head(s: &str, n: usize) -> Option<String> {
    (s.len() > n).then(|| idb_core::result::to_valid_utf8(&s.as_bytes()[..n]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 이스케이프와_질의_문자열은_Go_url_과_같다() {
        assert_eq!(path_escape("logs-* a/b"), "logs-%2A%20a%2Fb");
        assert_eq!(escape_path("/a b/c?d"), "/a%20b/c%3Fd");
        let mut v = values([("b", "x y".into()), ("a", "1&2".into())]);
        v.entry("a".into()).or_default().push("~*".into());
        assert_eq!(encode(&v), "a=1%262&a=~%2A&b=x+y");
        let (m, err) = parse_query("v&format=text&size=1&size=2");
        assert!(err.is_none());
        assert_eq!(m["v"], [""]);
        assert_eq!(m["size"], ["1", "2"]);
        assert_eq!(parse_query("a=%zz&b=1").1.as_deref(), Some(r#"invalid URL escape "%zz""#));
        assert_eq!(parse_query("a=1;b=2").1.as_deref(), Some("invalid semicolon separator in query"));
        assert_eq!(url_error_op("POST"), "Post");
    }

    #[test]
    fn 잘못된_퍼센트_뒤_여러_바이트_글자도_Go_처럼_바이트로_자른다() {
        assert_eq!(parse_query("x=%한").1.as_deref(), Some(r#"invalid URL escape "%\xed\x95""#));
        assert_eq!(parse_query("x=%a한").1.as_deref(), Some(r#"invalid URL escape "%a\xed""#));
    }
}
