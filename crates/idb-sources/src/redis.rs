//! Redis 연결 하나와 명령 실행. Go `internal/db/backend.go` 의 Clients.Redis·RedisConn.Run 과 go-redis v9 의 접속 절차를 맞춘다.
//! 접속은 HELLO 3(RESP3)을 먼저 보내고, 서버가 오류로 답하면 RESP2 로 보고 AUTH·SELECT 한다. 값 글은 go-redis 가 돌려주는
//! Go 값을 fmt.Print 한 것과 같다(정수·실수·불리언·배열·맵).

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use idb_core::project::{Conn as ConnInfo, go_atoi, go_quote};
use idb_core::result::{MAX_CELL_BYTES, MAX_ROWS, QueryResult, to_valid_utf8};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::Error;
use crate::net::{Stream, dial, io_reason, resolve};
use crate::tls::{Verify, handshake};

/// Redis string 값은 앞 1MiB 만 보여 준다(E63)
const MAX_STRING_BYTES: usize = 1 << 20;
const DIAL_TIMEOUT: Duration = Duration::from_secs(3);
const READ_TIMEOUT: Duration = Duration::from_secs(15);
/// 평문·TLS 시도 하나의 한도(접속·HELLO·PING 까지)
const PING_TIMEOUT: Duration = Duration::from_secs(3);

/// go-redis 가 돌려주는 값이다. 상태 줄·bulk·verbatim 은 모두 Str 이다.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Nil,
    Str(Vec<u8>),
    Int(i64),
    Float(f64),
    Bool(bool),
    BigInt(String),
    Array(Vec<Value>),
    Map(Vec<(Value, Value)>),
    /// 배열·맵 안에 든 오류 응답이다. 맨 위 오류는 Reply::Error 다
    Error(String),
}

/// 답 하나다. 맨 위 오류 응답은 글 그대로(`ERR …`) 싣는다.
enum Reply {
    Value(Value),
    Error(String),
}

/// 접속 시도의 실패다. Reply 는 서버가 RESP 로 답했다는 뜻이라 평문 서버로 본다(NOAUTH 등).
enum PingError {
    Io(Error),
    Reply(String),
}

impl PingError {
    fn into_error(self) -> Error {
        match self {
            PingError::Io(e) => e,
            PingError::Reply(s) => Error::Other(s),
        }
    }
}

/// 연결 설정이다. 비밀번호가 있어 Debug 를 두지 않는다.
#[derive(Clone)]
struct Options {
    host: String,
    port: u16,
    user: String,
    password: String,
    db: i64,
}

type ReplyFuture<'a> = Pin<Box<dyn Future<Output = Result<Reply, Error>> + Send + 'a>>;

pub struct RedisConn {
    stream: Stream,
    rbuf: Vec<u8>,
    pos: usize,
}

impl RedisConn {
    /// 평문 PING 을 먼저 하고, 3초 안에 답이 없으면 TLS 로 다시 붙는다.
    /// ElastiCache 인증서는 *.cache.amazonaws.com 이라 .local 별칭과 맞지 않으므로 서버 이름은 CNAME 대상을 쓴다(명세 R4.3).
    pub(crate) async fn open(c: &ConnInfo) -> Result<RedisConn, Error> {
        let port = if c.port == 0 { 6379 } else { c.port };
        let db = if c.db.is_empty() {
            0
        } else {
            go_atoi(&c.db).ok_or_else(|| Error::msg(format!("invalid redis db {} for {}", go_quote(&c.db), c.name)))?
        };
        let opts = Options {
            host: c.host.clone(),
            port,
            user: c.user.clone(),
            password: c.password.clone(),
            db,
        };
        // 평문 시도는 비밀번호 없이 한다. TLS 전용 서버에 비밀번호를 평문으로 흘리지 않기 위해서다
        let probe = Options {
            user: String::new(),
            password: String::new(),
            ..opts.clone()
        };
        let plain_err = match ping(&probe, None).await {
            // 평문으로 답했다(인증 불필요 또는 NOAUTH). 이제 비밀번호를 넣어 평문으로 붙는다(명세 R4.3 예외)
            Ok(c) => {
                c.close().await;
                return ping(&opts, None).await.map_err(PingError::into_error);
            }
            Err(PingError::Reply(_)) => return ping(&opts, None).await.map_err(PingError::into_error),
            Err(PingError::Io(e)) => e,
        };
        // CNAME 이 없으면 호스트 이름 그대로 검증한다
        let name = crate::dns::lookup_cname(&c.host).await.unwrap_or_else(|| c.host.clone());
        ping(&opts, Some(&name))
            .await
            .map_err(|e| Error::msg(format!("plain: {plain_err} / tls: {}", e.into_error())))
    }

    /// 명령 하나를 보내고 결과를 표·글로 만든다.
    /// 사용자가 직접 친 명령이라 서버 응답 크기는 막지 않고, 화면에 싣는 줄 수와 칸 크기만 제한한다(E63).
    pub async fn run(&mut self, args: &[String]) -> Result<QueryResult, Error> {
        let start = std::time::Instant::now();
        let v = match self.command(args).await? {
            Reply::Value(v) => v,
            Reply::Error(e) => return Err(Error::Other(e)),
        };
        let mut res = QueryResult::default();
        match v {
            Value::Array(items) => {
                res.cols = vec!["#".into(), "value".into()];
                for (i, e) in items.iter().enumerate() {
                    if i >= MAX_ROWS {
                        res.truncated = true;
                        break;
                    }
                    res.rows.push(vec![i.to_string(), format_cell(e)]);
                }
            }
            Value::Map(pairs) => {
                res.cols = vec!["key".into(), "value".into()];
                // Go map 은 같은 키를 하나로 접는다(뒤 값이 이긴다)
                let mut rows: Vec<Vec<String>> = Vec::new();
                for (k, e) in &pairs {
                    let key = format_cell(k);
                    match rows.iter_mut().find(|r| r[0] == key) {
                        Some(r) => r[1] = format_cell(e),
                        None => rows.push(vec![key, format_cell(e)]),
                    }
                }
                if rows.len() > MAX_ROWS {
                    // 맵은 순서가 없어 앞 1,000개가 정렬상 앞이라는 보장은 없다
                    rows.truncate(MAX_ROWS);
                    res.truncated = true;
                }
                rows.sort_by(|a, b| a[0].cmp(&b[0]));
                res.rows = rows;
            }
            other => {
                // string 은 칸 상한(64KiB)이 아니라 1MiB 까지 보여 준다
                // 다른 값은 format_cell 이 64KiB 에서 자른다
                let v = match &other {
                    Value::Str(b) if b.len() > MAX_STRING_BYTES => {
                        res.truncated = true;
                        format!(
                            "{}\n\n… {}바이트 중 앞 {MAX_STRING_BYTES}바이트만 보여 줌",
                            to_valid_utf8(&b[..MAX_STRING_BYTES]),
                            b.len()
                        )
                    }
                    Value::Str(b) => String::from_utf8_lossy(b).into_owned(),
                    _ => format_cell(&other),
                };
                res.text = pretty_json(&v);
            }
        }
        res.elapsed = start.elapsed();
        Ok(res)
    }

    async fn connect(o: &Options, tls_name: Option<&str>) -> Result<RedisConn, PingError> {
        let io = |e: String| PingError::Io(Error::msg(e));
        let addrs = resolve(&o.host, o.port).await.map_err(|e| io(format!("dial tcp: {e}")))?;
        let mut last = String::new();
        let mut tcp = None;
        for addr in addrs {
            match tokio::time::timeout(DIAL_TIMEOUT, dial(addr)).await {
                Ok(Ok(s)) => {
                    tcp = Some(s);
                    break;
                }
                Ok(Err(e)) => last = e,
                Err(_) => last = format!("dial tcp {addr}: i/o timeout"),
            }
        }
        let tcp = tcp.ok_or_else(|| io(last))?;
        let stream = match tls_name {
            None => Stream::Plain(tcp),
            Some(name) => Stream::Tls(Box::new(handshake(tcp, name, Verify::Full).await.map_err(io)?)),
        };
        let mut c = RedisConn {
            stream,
            rbuf: Vec::with_capacity(4096),
            pos: 0,
        };
        c.init(o).await?;
        Ok(c)
    }

    /// go-redis initConn: HELLO 3 [AUTH] 가 서버 오류면 HELLO 를 모르는 서버로 보고 AUTH·SELECT 를 보낸다.
    async fn init(&mut self, o: &Options) -> Result<(), PingError> {
        let mut hello = vec!["hello".to_string(), "3".to_string()];
        if !o.password.is_empty() {
            let user = if o.user.is_empty() { "default" } else { o.user.as_str() };
            hello.extend(["auth".to_string(), user.to_string(), o.password.clone()]);
        }
        let auth = matches!(self.command(&hello).await.map_err(PingError::Io)?, Reply::Value(_));
        if !auth && !o.password.is_empty() {
            let mut cmd = vec!["auth".to_string()];
            if !o.user.is_empty() {
                cmd.push(o.user.clone());
            }
            cmd.push(o.password.clone());
            if let Reply::Error(e) = self.command(&cmd).await.map_err(PingError::Io)? {
                return Err(PingError::Reply(e));
            }
        }
        if o.db > 0
            && let Reply::Error(e) = self
                .command(&["select".to_string(), o.db.to_string()])
                .await
                .map_err(PingError::Io)?
        {
            return Err(PingError::Reply(e));
        }
        Ok(())
    }

    async fn close(mut self) {
        let _ = tokio::time::timeout(Duration::from_secs(1), self.stream.shutdown()).await;
    }

    /// 명령 하나를 보내고 답 하나를 읽는다. 입출력 실패만 Err 다.
    async fn command(&mut self, args: &[String]) -> Result<Reply, Error> {
        let mut buf = format!("*{}\r\n", args.len()).into_bytes();
        for a in args {
            buf.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
            buf.extend_from_slice(a.as_bytes());
            buf.extend_from_slice(b"\r\n");
        }
        self.stream.write_all(&buf).await.map_err(|e| Error::msg(io_reason(&e)))?;
        self.stream.flush().await.map_err(|e| Error::msg(io_reason(&e)))?;
        tokio::time::timeout(READ_TIMEOUT, self.read_reply())
            .await
            .map_err(|_| Error::msg("i/o timeout"))?
    }

    // ─── RESP 읽기 (go-redis proto.Reader) ───

    async fn fill(&mut self) -> Result<(), Error> {
        if self.pos > 0 && self.pos == self.rbuf.len() {
            self.rbuf.clear();
            self.pos = 0;
        }
        let mut chunk = [0u8; 8192];
        match self.stream.read(&mut chunk).await {
            Ok(0) => Err(Error::msg("EOF")),
            Ok(n) => {
                self.rbuf.extend_from_slice(&chunk[..n]);
                Ok(())
            }
            Err(e) => Err(Error::msg(io_reason(&e))),
        }
    }

    /// \r\n 을 뗀 한 줄이다.
    async fn read_line(&mut self) -> Result<Vec<u8>, Error> {
        loop {
            if let Some(i) = self.rbuf[self.pos..].iter().position(|&b| b == b'\n') {
                let line = self.rbuf[self.pos..self.pos + i + 1].to_vec();
                self.pos += i + 1;
                if line.len() <= 2 || line[line.len() - 2] != b'\r' {
                    return Err(Error::msg(format!(
                        "redis: invalid reply: {}",
                        go_quote(&String::from_utf8_lossy(&line))
                    )));
                }
                return Ok(line[..line.len() - 2].to_vec());
            }
            self.fill().await?;
        }
    }

    async fn read_n(&mut self, n: usize) -> Result<Vec<u8>, Error> {
        while self.rbuf.len() - self.pos < n + 2 {
            self.fill().await?;
        }
        let v = self.rbuf[self.pos..self.pos + n].to_vec();
        self.pos += n + 2;
        Ok(v)
    }

    fn read_reply(&mut self) -> ReplyFuture<'_> {
        Box::pin(async move {
            let line = self.read_line().await?;
            let body = String::from_utf8_lossy(&line[1..]).into_owned();
            let len = || -> Result<Option<usize>, Error> {
                if body == "-1" {
                    return Ok(None);
                }
                body.parse::<usize>()
                    .map(Some)
                    .map_err(|_| Error::msg(format!("redis: invalid reply: {}", go_quote(&body))))
            };
            let v = match line[0] {
                b'-' => return Ok(Reply::Error(body)),
                b'+' => Value::Str(line[1..].to_vec()),
                b':' => Value::Int(body.parse().map_err(|e: std::num::ParseIntError| Error::msg(e.to_string()))?),
                b',' => Value::Float(match body.as_str() {
                    "inf" => f64::INFINITY,
                    "-inf" => f64::NEG_INFINITY,
                    "nan" | "-nan" => f64::NAN,
                    s => s.parse().map_err(|e: std::num::ParseFloatError| Error::msg(e.to_string()))?,
                }),
                b'#' => match body.as_str() {
                    "t" => Value::Bool(true),
                    "f" => Value::Bool(false),
                    _ => return Err(Error::msg(format!("redis: can't parse bool reply: {}", go_quote(&body)))),
                },
                b'(' => Value::BigInt(body),
                b'_' => Value::Nil,
                b'$' | b'=' | b'!' => match len()? {
                    None => Value::Nil,
                    Some(n) => {
                        let s = self.read_n(n).await?;
                        match line[0] {
                            b'!' => return Ok(Reply::Error(String::from_utf8_lossy(&s).into_owned())),
                            // verbatim 은 "txt:" 같은 형식 머리를 뗀다
                            b'=' if s.len() >= 4 && s[3] == b':' => Value::Str(s[4..].to_vec()),
                            b'=' => return Err(Error::msg("redis: can't parse verbatim string reply")),
                            _ => Value::Str(s),
                        }
                    }
                },
                b'*' | b'~' | b'>' => match len()? {
                    None => Value::Nil,
                    Some(n) => {
                        let mut items = Vec::with_capacity(n.min(1024));
                        for _ in 0..n {
                            items.push(match self.read_reply().await? {
                                Reply::Value(v) => v,
                                Reply::Error(e) => Value::Error(e),
                            });
                        }
                        Value::Array(items)
                    }
                },
                b'%' | b'|' => {
                    let n = len()?.unwrap_or(0);
                    let mut pairs = Vec::with_capacity(n.min(1024));
                    for _ in 0..n {
                        let k = match self.read_reply().await? {
                            Reply::Value(v) => v,
                            Reply::Error(e) => return Ok(Reply::Error(e)),
                        };
                        let v = match self.read_reply().await? {
                            Reply::Value(v) => v,
                            Reply::Error(e) => Value::Error(e),
                        };
                        pairs.push((k, v));
                    }
                    if line[0] == b'|' {
                        // 속성은 버리고 뒤따르는 답을 읽는다
                        return self.read_reply().await;
                    }
                    Value::Map(pairs)
                }
                _ => {
                    return Err(Error::msg(format!(
                        "redis: can't parse {}",
                        go_quote(&String::from_utf8_lossy(&line))
                    )));
                }
            };
            Ok(Reply::Value(v))
        })
    }
}

/// 접속하고 PING 한다. 전체를 PING_TIMEOUT 안에 끝낸다(go-redis pingRedis 의 3초 컨텍스트).
async fn ping(o: &Options, tls_name: Option<&str>) -> Result<RedisConn, PingError> {
    let attempt = async {
        let mut c = RedisConn::connect(o, tls_name).await?;
        match c.command(&["ping".to_string()]).await.map_err(PingError::Io)? {
            Reply::Value(_) => Ok(c),
            Reply::Error(e) => Err(PingError::Reply(e)),
        }
    };
    match tokio::time::timeout(PING_TIMEOUT, attempt).await {
        Ok(r) => r,
        Err(_) => Err(PingError::Io(Error::msg("context deadline exceeded"))),
    }
}

// ─── 값 글 ───

/// 값을 글로 바꾸되 MAX_CELL_BYTES 를 넘으면 더 만들지 않는다(E63).
pub fn format_cell(v: &Value) -> String {
    let mut b = Vec::new();
    write_value(&mut b, v);
    if b.len() > MAX_CELL_BYTES {
        return format!("{}…(잘림)", to_valid_utf8(&b[..MAX_CELL_BYTES]));
    }
    String::from_utf8_lossy(&b).into_owned()
}

fn write_value(b: &mut Vec<u8>, v: &Value) {
    if b.len() > MAX_CELL_BYTES {
        return;
    }
    match v {
        Value::Nil => b.extend_from_slice(b"(nil)"),
        // 남은 예산만 복사한다
        Value::Str(s) => b.extend_from_slice(&s[..s.len().min(MAX_CELL_BYTES + 1 - b.len())]),
        Value::Map(pairs) => {
            b.push(b'{');
            for (k, e) in pairs {
                write_value(b, k);
                b.extend_from_slice(b": ");
                write_value(b, e);
                b.extend_from_slice(b", ");
                if b.len() > MAX_CELL_BYTES {
                    return;
                }
            }
            b.push(b'}');
        }
        Value::Array(items) => {
            b.push(b'[');
            for (i, e) in items.iter().enumerate() {
                if i > 0 {
                    b.extend_from_slice(b", ");
                }
                write_value(b, e);
                if b.len() > MAX_CELL_BYTES {
                    return;
                }
            }
            b.push(b']');
        }
        Value::Int(n) => b.extend_from_slice(n.to_string().as_bytes()),
        Value::Float(f) => b.extend_from_slice(go_float(*f).as_bytes()),
        Value::Bool(t) => b.extend_from_slice(if *t { b"true" } else { b"false" }),
        Value::BigInt(s) | Value::Error(s) => b.extend_from_slice(s.as_bytes()),
    }
}

/// Go fmt %v 의 float64 글이다: 가장 짧은 자릿수, 지수가 -4 미만이거나 21 이상이면 e 표기(e+21, e-05).
fn go_float(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "+Inf".into() } else { "-Inf".into() };
    }
    if f == 0.0 {
        return if f.is_sign_negative() { "-0".into() } else { "0".into() };
    }
    // {:e} 는 가장 짧은 자릿수의 과학 표기다(1.5e-5)
    let sci = format!("{f:e}");
    let (mant, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    if !(-4..21).contains(&exp) {
        let sign = if exp < 0 { '-' } else { '+' };
        return format!("{mant}e{sign}{:02}", exp.abs());
    }
    let (neg, mant) = mant.strip_prefix('-').map_or((false, mant), |m| (true, m));
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let point = exp + 1; // 소수점 앞 자릿수
    let s = if point <= 0 {
        format!("0.{}{digits}", "0".repeat((-point) as usize))
    } else if point as usize >= digits.len() {
        format!("{digits}{}", "0".repeat(point as usize - digits.len()))
    } else {
        format!("{}.{}", &digits[..point as usize], &digits[point as usize..])
    };
    if neg { format!("-{s}") } else { s }
}

/// JSON 글이면 Go json.Indent(“”, “  ”)로 들여 쓴다. 토큰 글(숫자·문자열 이스케이프)은 그대로 둔다. JSON 이 아니면 그대로다.
pub fn pretty_json(s: &str) -> String {
    let t = s.trim();
    if !(t.starts_with('{') || t.starts_with('[')) || serde_json::from_str::<serde::de::IgnoredAny>(t).is_err() {
        return s.to_string();
    }
    let mut out = String::with_capacity(t.len() * 2);
    let mut depth = 0usize;
    let mut need_indent = false;
    let (mut in_str, mut esc) = (false, false);
    let newline = |out: &mut String, depth: usize| {
        out.push('\n');
        out.push_str(&"  ".repeat(depth));
    };
    for c in t.chars() {
        if in_str {
            out.push(c);
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        if matches!(c, ' ' | '\t' | '\n' | '\r') {
            continue;
        }
        // 빈 객체·배열은 {}·[] 로 두려고 들여쓰기를 미룬다
        if need_indent && c != '}' && c != ']' {
            need_indent = false;
            depth += 1;
            newline(&mut out, depth);
        }
        match c {
            '"' => {
                in_str = true;
                out.push(c);
            }
            '{' | '[' => {
                need_indent = true;
                out.push(c);
            }
            ',' => {
                out.push(c);
                newline(&mut out, depth);
            }
            ':' => out.push_str(": "),
            '}' | ']' => {
                if need_indent {
                    need_indent = false;
                } else {
                    depth = depth.saturating_sub(1);
                    newline(&mut out, depth);
                }
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 값_글은_go_redis_값을_fmt_Print_한_것과_같다() {
        let v = Value::Array(vec![
            Value::Str(b"one".to_vec()),
            Value::Float(1.0),
            Value::Nil,
            Value::Int(-3),
            Value::Bool(true),
            Value::Error("ERR x".into()),
            Value::Map(vec![(Value::Str(b"k".to_vec()), Value::Float(0.5))]),
        ]);
        assert_eq!(format_cell(&v), "[one, 1, (nil), -3, true, ERR x, {k: 0.5, }]");
    }

    #[test]
    fn 실수는_Go_v_꼴이다() {
        for (f, want) in [
            (1.0, "1"),
            (2.5, "2.5"),
            (-0.000123, "-0.000123"),
            (0.0000123, "1.23e-05"),
            (1e20, "100000000000000000000"),
            (1e21, "1e+21"),
            (123456.789, "123456.789"),
            (f64::INFINITY, "+Inf"),
            (f64::NAN, "NaN"),
        ] {
            assert_eq!(go_float(f), want, "{f}");
        }
    }

    #[test]
    fn 큰_문자열과_중첩_맵은_예산까지만_만든다() {
        let big = vec![b'x'; 8 << 20];
        let s = format_cell(&Value::Array(vec![
            Value::Str(big.clone()),
            Value::Map(vec![(Value::Str(b"k".to_vec()), Value::Str(big))]),
        ]));
        assert!(s.len() <= MAX_CELL_BYTES + 100 && s.ends_with("…(잘림)") && idb_core::result::is_capped_cell(&s));
    }

    #[test]
    fn JSON_은_Go_Indent_처럼_들여_쓰고_아니면_그대로다() {
        assert_eq!(
            pretty_json(r#" {"a":1,"b":[1, 2],"c":{},"d":[],"e":"x\"y, z"}"#),
            "{\n  \"a\": 1,\n  \"b\": [\n    1,\n    2\n  ],\n  \"c\": {},\n  \"d\": [],\n  \"e\": \"x\\\"y, z\"\n}"
        );
        assert_eq!(pretty_json("값 value"), "값 value");
        assert_eq!(pretty_json("{broken"), "{broken");
        assert_eq!(pretty_json("[1.50e3]"), "[\n  1.50e3\n]");
    }
}
