//! 시험용 가짜 HTTP 서버. Go `httptest.NewServer` 처럼 받은 요청을 적어 두고 처리 함수의 답을 돌려준다.
//! 요청마다 연결을 닫아(Connection: close) 순서가 분명하다.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use crate::httpc::{Values, parse_query};

/// 받은 요청 하나다. path 는 Go `r.URL.Path` 처럼 퍼센트를 푼 경로다.
#[derive(Clone, Debug, Default)]
pub(crate) struct Seen {
    pub method: String,
    pub path: String,
    pub raw_query: String,
    pub query: Values,
    /// 이름은 소문자다
    pub headers: HashMap<String, String>,
    pub body: String,
}

impl Seen {
    pub fn header(&self, name: &str) -> &str {
        self.headers.get(&name.to_lowercase()).map_or("", String::as_str)
    }

    pub fn q(&self, key: &str) -> &str {
        self.query.get(key).and_then(|v| v.first()).map_or("", String::as_str)
    }

    /// Authorization: Basic 을 풀어 "사용자:비밀번호" 로 돌려준다.
    pub fn basic(&self) -> String {
        let Some(b64) = self.header("authorization").strip_prefix("Basic ") else {
            return String::new();
        };
        String::from_utf8_lossy(&unbase64(b64)).into_owned()
    }
}

/// 답이다: 상태, 더할 헤더, 본문.
pub(crate) struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

pub(crate) fn reply(status: u16, body: impl Into<String>) -> Reply {
    Reply {
        status,
        headers: Vec::new(),
        body: body.into(),
    }
}

pub(crate) fn redirect(location: &str) -> Reply {
    Reply {
        status: 302,
        headers: vec![("Location".into(), location.into())],
        body: String::new(),
    }
}

type Handler = Arc<dyn Fn(&Seen) -> Reply + Send + Sync>;

pub(crate) struct Fake {
    pub url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Fake {
    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    pub fn last(&self) -> Seen {
        self.seen().pop().expect("no request")
    }

    pub fn count(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

/// 127.0.0.1 의 빈 포트에서 듣는다. 서버 스레드는 시험 프로세스가 끝날 때 함께 끝난다.
pub(crate) fn serve(handler: impl Fn(&Seen) -> Reply + Send + Sync + 'static) -> Fake {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let handler: Handler = Arc::new(handler);
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let Some(req) = read_request(&mut stream) else { continue };
            log.lock().unwrap().push(req.clone());
            let r = handler(&req);
            let mut head = format!(
                "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
                r.status,
                reason(r.status),
                r.body.len()
            );
            for (k, v) in &r.headers {
                head.push_str(&format!("{k}: {v}\r\n"));
            }
            head.push_str("\r\n");
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(r.body.as_bytes());
        }
    });
    Fake { url, seen }
}

fn read_request(stream: &mut std::net::TcpStream) -> Option<Seen> {
    let mut r = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    r.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let mut headers = HashMap::new();
    loop {
        let mut h = String::new();
        if r.read_line(&mut h).ok()? == 0 || h.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_lowercase(), v.trim().to_string());
        }
    }
    let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut body = vec![0; len];
    r.read_exact(&mut body).ok()?;
    let (path, raw_query) = target.split_once('?').unwrap_or((&target, ""));
    Some(Seen {
        method,
        path: percent_encoding::percent_decode_str(path).decode_utf8_lossy().into_owned(),
        raw_query: raw_query.to_string(),
        query: parse_query(raw_query).0,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        503 => "Service Unavailable",
        _ => "",
    }
}

fn unbase64(s: &str) -> Vec<u8> {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes().filter(|&c| c != b'=') {
        let Some(v) = T.iter().position(|&t| t == c) else { continue };
        acc = acc << 6 | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}
