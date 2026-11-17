//! PostgreSQL 연결 하나. 접속(TLS·인증)과 두 가지 실행만 있다.
//! - `exec`: simple protocol. pgx 가 인자 없는 Exec 를 보내는 방식이다. db 가 만든 고정 문장(BEGIN·SET 등)에만 쓴다
//! - `query`: extended protocol(Parse·Bind·Describe·Execute·Sync). 여러 문장을 서버가 거부하고 값은 텍스트로 받는다
//!
//! 오류 글은 pgx 와 같게 둔다: 서버 오류는 `ERROR: … (SQLSTATE …)`, 접속 실패는 pgconn.ConnectError 꼴이다.

use std::net::SocketAddr;
use std::time::Duration;

use bytes::BytesMut;
use fallible_iterator::FallibleIterator;
use postgres_protocol::authentication::md5_hash;
use postgres_protocol::authentication::sasl::{ChannelBinding, SCRAM_SHA_256, ScramSha256};
use postgres_protocol::message::backend::{DataRowBody, ErrorResponseBody, Message};
use postgres_protocol::message::frontend;
use postgres_protocol::{IsNull, Oid};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use idb_core::result::cap_cell;

use crate::net::{Stream, dial, io_reason, resolve};
use crate::tls::{Verify, handshake};
use crate::{Error, PgError};

/// 접속 하나를 여는 데 쓰는 한도(pgx connect_timeout=8)
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
/// 서버 메시지 하나의 길이 상한. 서버는 메시지 버퍼를 MaxAllocSize(1GB-1) 넘게 만들지 못해 정상 서버는 넘지 않는다.
/// pgx 는 상한이 없지만, 깨졌거나 악의적인 서버가 길이 칸 하나로 4GiB 를 잡게 두지 않는다
const MAX_FRAME_BYTES: usize = 1 << 30;
/// 결과 한 벌에 싣는 칸 글자의 합. 행(--max-rows 1만)·칸(64KiB) 상한만으로는 수 GiB 까지 쌓일 수 있어 따로 둔다.
/// 넘으면 행 상한처럼 거기서 자르고 truncated 를 세운다
pub(crate) const MAX_RESULT_BYTES: usize = 256 << 20;

/// 접속 설정이다. 비밀번호가 있어 Debug 를 두지 않는다.
#[derive(Clone)]
pub(crate) struct Config {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub database: String,
    pub sslmode: String,
}

/// 실행 결과다. 칸은 서버 텍스트이고 None 은 NULL 이다. limit 행이나 결과 예산을 넘으면 버리고 truncated 를 세운다.
pub(crate) struct Rows {
    pub fields: Vec<(String, Oid)>,
    /// 열마다 출처 테이블 OID 와 열 번호(RowDescription). 식 열은 (0, 0) 이다
    pub sources: Vec<(Oid, i16)>,
    pub rows: Vec<Vec<Option<String>>>,
    pub truncated: bool,
    pub tag: String,
}

impl Rows {
    /// row 행 i 칸의 글이다. NULL 은 빈 글이다(카탈로그 조회는 coalesce 로 NULL 을 막는다).
    pub fn text(&self, row: usize, i: usize) -> String {
        self.rows[row][i].clone().unwrap_or_default()
    }

    pub fn flag(&self, row: usize, i: usize) -> bool {
        self.rows[row][i].as_deref() == Some("t")
    }
}

pub(crate) struct Conn {
    stream: Stream,
    rbuf: BytesMut,
    tx_status: u8,
    broken: bool,
    /// 결과 한 벌의 칸 글자 예산(MAX_RESULT_BYTES). 시험이 줄인다
    result_budget: usize,
}

/// 한 번의 접속 시도(주소·TLS 조합)의 실패다. fatal 이면 다음 시도를 하지 않는다(pgx connectPreferred).
struct AttemptError {
    msg: String,
    fatal: bool,
}

impl Conn {
    /// cfg 로 접속한다. sslmode 에 따라 TLS·평문 시도를 pgx 순서로 하고, 모두 실패하면 시도별 까닭을 모은 오류다.
    pub async fn connect(cfg: &Config) -> Result<Conn, Error> {
        // pgx configTLS: prefer·allow·require 는 인증서를 검증하지 않는다(sslrootcert 없음)
        let modes: &[Option<Verify>] = match cfg.sslmode.as_str() {
            "disable" => &[None],
            "allow" => &[None, Some(Verify::None)],
            "prefer" => &[Some(Verify::None), None],
            "require" => &[Some(Verify::None)],
            "verify-ca" => &[Some(Verify::Chain)],
            "verify-full" => &[Some(Verify::Full)],
            _ => return Err(Error::msg("sslmode is invalid")),
        };
        let addrs = resolve(&cfg.host, cfg.port)
            .await
            .map_err(|e| connect_error(cfg, &format!("hostname resolving error: {e}")))?;
        let mut errs = Vec::new();
        'modes: for &tls in modes {
            for &addr in &addrs {
                match tokio::time::timeout(CONNECT_TIMEOUT, Conn::attempt(cfg, addr, tls)).await {
                    Ok(Ok(c)) => return Ok(c),
                    Ok(Err(e)) => {
                        errs.push(format!("{addr} ({}): {}", cfg.host, e.msg));
                        if e.fatal {
                            break 'modes;
                        }
                    }
                    Err(_) => errs.push(format!("{addr} ({}): timeout: context deadline exceeded", cfg.host)),
                }
            }
        }
        Err(connect_error(cfg, &errs.join("\n")))
    }

    async fn attempt(cfg: &Config, addr: SocketAddr, tls: Option<Verify>) -> Result<Conn, AttemptError> {
        let soft = |msg: String| AttemptError { msg, fatal: false };
        let tcp = dial(addr).await.map_err(|e| soft(format!("dial error: {e}")))?;
        let stream = match tls {
            None => Stream::Plain(tcp),
            Some(verify) => {
                let mut tcp = tcp;
                let mut buf = BytesMut::new();
                frontend::ssl_request(&mut buf);
                let mut answer = [0u8; 1];
                let io = async {
                    tcp.write_all(&buf).await?;
                    tcp.read_exact(&mut answer).await
                };
                io.await.map_err(|e| soft(format!("tls error: {}", io_reason(&e))))?;
                if answer[0] != b'S' {
                    return Err(soft("tls error: server refused TLS connection".into()));
                }
                // 이름이 SNI 로 못 쓰는 꼴이면(밑줄 등) 검증하지 않는 모드에서는 주소로 대신한다
                let name = if verify == Verify::None && rustls::pki_types::ServerName::try_from(cfg.host.as_str()).is_err() {
                    addr.ip().to_string()
                } else {
                    cfg.host.clone()
                };
                let s = handshake(tcp, &name, verify).await.map_err(|e| soft(format!("tls error: {e}")))?;
                Stream::Tls(Box::new(s))
            }
        };
        let mut c = Conn {
            stream,
            rbuf: BytesMut::with_capacity(8192),
            tx_status: b'I',
            broken: false,
            result_budget: MAX_RESULT_BYTES,
        };
        c.startup(cfg, tls.is_some()).await?;
        Ok(c)
    }

    async fn startup(&mut self, cfg: &Config, tls: bool) -> Result<(), AttemptError> {
        let soft = |msg: String| AttemptError { msg, fatal: false };
        let mut buf = BytesMut::new();
        let params = [
            ("user", cfg.user.as_str()),
            ("database", cfg.database.as_str()),
            ("application_name", "db"),
        ];
        frontend::startup_message(params, &mut buf).map_err(|e| soft(e.to_string()))?;
        self.send(&buf)
            .await
            .map_err(|e| soft(format!("failed to write startup message: {e}")))?;
        let mut scram: Option<ScramSha256> = None;
        // SCRAM 은 서버 서명(AuthenticationSaslFinal)을 확인해야 끝난다. 그 전의 AuthenticationOk·ReadyForQuery 는
        // 서버가 비밀번호를 모르는 채 통과시킨 것일 수 있어 받지 않는다
        let (mut scram_verified, mut authed) = (false, false);
        loop {
            let msg = self.read().await.map_err(|e| soft(format!("failed to receive message: {e}")))?;
            let mut out = BytesMut::new();
            match msg {
                Message::AuthenticationOk => {
                    if scram.is_some() && !scram_verified {
                        return Err(soft("failed SASL auth: server skipped SCRAM verification".into()));
                    }
                    authed = true;
                }
                Message::AuthenticationCleartextPassword => {
                    frontend::password_message(cfg.password.as_bytes(), &mut out).map_err(|e| soft(e.to_string()))?;
                }
                Message::AuthenticationMd5Password(body) => {
                    let hash = md5_hash(cfg.user.as_bytes(), cfg.password.as_bytes(), body.salt());
                    frontend::password_message(hash.as_bytes(), &mut out).map_err(|e| soft(e.to_string()))?;
                }
                Message::AuthenticationSasl(body) => {
                    let mut mechs = body.mechanisms();
                    let mut ok = false;
                    while let Ok(Some(m)) = mechs.next() {
                        ok |= m == SCRAM_SHA_256;
                    }
                    if !ok {
                        return Err(soft("failed SASL auth: no SCRAM-SHA-256 mechanism".into()));
                    }
                    // pgx 처럼 채널 바인딩 없이(n,,) 한다
                    let s = ScramSha256::new(cfg.password.as_bytes(), ChannelBinding::unsupported());
                    frontend::sasl_initial_response(SCRAM_SHA_256, s.message(), &mut out).map_err(|e| soft(e.to_string()))?;
                    scram = Some(s);
                }
                Message::AuthenticationSaslContinue(body) => {
                    let s = scram.as_mut().ok_or_else(|| soft("failed SASL auth: unexpected message".into()))?;
                    s.update(body.data()).map_err(|e| soft(format!("failed SASL auth: {e}")))?;
                    frontend::sasl_response(s.message(), &mut out).map_err(|e| soft(e.to_string()))?;
                }
                Message::AuthenticationSaslFinal(body) => {
                    let s = scram.as_mut().ok_or_else(|| soft("failed SASL auth: unexpected message".into()))?;
                    s.finish(body.data()).map_err(|e| soft(format!("failed SASL auth: {e}")))?;
                    scram_verified = true;
                }
                Message::ErrorResponse(body) => {
                    let e = pg_error(&body);
                    // pgx connectPreferred: 비밀번호·DB 없음·권한은 다른 시도로 바뀌지 않는다
                    let fatal = matches!(e.code.as_str(), "28P01" | "3D000" | "42501") || e.code == "28000" && tls;
                    return Err(AttemptError {
                        msg: format!("server error: {e}"),
                        fatal,
                    });
                }
                Message::ReadyForQuery(_) if !authed => return Err(soft("received unexpected message".into())),
                Message::ReadyForQuery(body) => {
                    self.tx_status = body.status();
                    return Ok(());
                }
                Message::ParameterStatus(_) | Message::BackendKeyData(_) | Message::NoticeResponse(_) => {}
                _ => return Err(soft("received unexpected message".into())),
            }
            if !out.is_empty() {
                self.send(&out)
                    .await
                    .map_err(|e| soft(format!("failed to write password message: {e}")))?;
            }
        }
    }

    /// 트랜잭션 상태다(I 밖, T 안, E 실패한 트랜잭션 안).
    pub fn in_tx(&self) -> bool {
        matches!(self.tx_status, b'T' | b'E')
    }

    /// 풀에 돌려줘도 되는 연결인지다. 트랜잭션이 열린 채면 버린다(pgxpool 과 같다).
    pub fn reusable(&self) -> bool {
        !self.broken && self.tx_status == b'I'
    }

    /// 오류가 난 트랜잭션 안이다(ROLLBACK 만 받는다).
    pub fn tx_failed(&self) -> bool {
        self.tx_status == b'E'
    }

    /// 읽다·쓰다 끊겨 다시 쓸 수 없는 연결인지다.
    pub fn is_broken(&self) -> bool {
        self.broken
    }

    /// simple protocol 로 sql 을 보낸다. 결과 행은 버리고 첫 오류를 돌려준다.
    pub async fn exec(&mut self, sql: &str) -> Result<(), Error> {
        let mut buf = BytesMut::new();
        frontend::query(sql, &mut buf).map_err(|e| Error::msg(e.to_string()))?;
        self.send_or_break(&buf).await?;
        let mut first: Option<Error> = None;
        loop {
            match self.read_or_break().await? {
                Message::ErrorResponse(body) => {
                    first.get_or_insert(Error::Pg(pg_error(&body)));
                }
                Message::ReadyForQuery(body) => {
                    self.tx_status = body.status();
                    return first.map_or(Ok(()), Err);
                }
                Message::CopyInResponse(_) => {
                    let mut b = BytesMut::new();
                    let _ = frontend::copy_fail("COPY is not supported", &mut b);
                    self.send_or_break(&b).await?;
                }
                _ => {}
            }
        }
    }

    /// extended protocol 로 sql 하나를 실행하고 결과를 텍스트로 받는다. params 는 텍스트로 보내 서버가 타입을 정한다.
    #[cfg(test)]
    pub fn set_result_budget(&mut self, n: usize) {
        self.result_budget = n;
    }

    /// 카탈로그 조회용이다. 칸을 자르지 않고, 결과 예산을 넘으면 자르지 않고 오류다.
    pub async fn query(&mut self, sql: &str, params: &[Option<&str>], limit: usize) -> Result<Rows, Error> {
        self.query_inner(sql, params, limit, false).await
    }

    /// 사용자 문장용이다. 칸을 받는 대로 cap_cell(64KiB)로 잘라 원값 전체를 쌓아 두지 않는다.
    pub async fn query_cells(&mut self, sql: &str, params: &[Option<&str>], limit: usize) -> Result<Rows, Error> {
        self.query_inner(sql, params, limit, true).await
    }

    async fn query_inner(&mut self, sql: &str, params: &[Option<&str>], limit: usize, cap: bool) -> Result<Rows, Error> {
        let mut buf = BytesMut::new();
        let enc = |e: std::io::Error| Error::msg(e.to_string());
        frontend::parse("", sql, std::iter::empty::<Oid>(), &mut buf).map_err(enc)?;
        let formats: Vec<i16> = if params.is_empty() { vec![] } else { vec![0] };
        frontend::bind(
            "",
            "",
            formats,
            params.iter().copied(),
            |v: Option<&str>, b: &mut BytesMut| match v {
                Some(s) => {
                    b.extend_from_slice(s.as_bytes());
                    Ok(IsNull::No)
                }
                None => Ok(IsNull::Yes),
            },
            [0i16],
            &mut buf,
        )
        .map_err(|_| Error::msg("failed to encode arguments"))?;
        frontend::describe(b'P', "", &mut buf).map_err(enc)?;
        frontend::execute("", 0, &mut buf).map_err(enc)?;
        frontend::sync(&mut buf);
        self.send_or_break(&buf).await?;

        let mut out = Rows {
            fields: Vec::new(),
            sources: Vec::new(),
            rows: Vec::new(),
            truncated: false,
            tag: String::new(),
        };
        let mut first: Option<Error> = None;
        let mut left = self.result_budget;
        let mut over_budget = false;
        loop {
            match self.read_or_break().await? {
                Message::RowDescription(body) => {
                    let mut fields = body.fields();
                    while let Ok(Some(f)) = fields.next() {
                        out.fields.push((f.name().to_string(), f.type_oid()));
                        out.sources.push((f.table_oid(), f.column_id()));
                    }
                }
                Message::DataRow(body) => {
                    if first.is_some() {
                        continue;
                    }
                    if out.truncated || out.rows.len() >= limit {
                        out.truncated = true;
                        continue;
                    }
                    let row = row_values(&body, cap).map_err(|e| Error::msg(e.to_string()))?;
                    let size: usize = row.iter().flatten().map(String::len).sum();
                    match left.checked_sub(size) {
                        Some(l) => {
                            left = l;
                            out.rows.push(row);
                        }
                        None => {
                            out.truncated = true;
                            over_budget = true;
                        }
                    }
                }
                Message::CommandComplete(body) => out.tag = body.tag().unwrap_or_default().to_string(),
                Message::ErrorResponse(body) => {
                    first.get_or_insert(Error::Pg(pg_error(&body)));
                }
                Message::CopyInResponse(_) => {
                    let mut b = BytesMut::new();
                    let _ = frontend::copy_fail("COPY is not supported", &mut b);
                    self.send_or_break(&b).await?;
                }
                Message::ReadyForQuery(body) => {
                    self.tx_status = body.status();
                    // 카탈로그 결과는 잘리면 틀린 DDL·스키마 비교가 성공처럼 나가서 오류로 낸다. 남은 행은 위에서 읽어 버렸다
                    if first.is_none() && over_budget && !cap {
                        return Err(Error::msg(format!("catalog result exceeds {} bytes", self.result_budget)));
                    }
                    return first.map_or(Ok(out), Err);
                }
                _ => {}
            }
        }
    }

    /// simple protocol 로 `COPY … FROM STDIN` 을 보내고 data 를 한 번에 넣는다. 돌려주는 것은 명령 태그다.
    pub async fn copy_in(&mut self, sql: &str, data: &[u8]) -> Result<String, Error> {
        let mut buf = BytesMut::new();
        frontend::query(sql, &mut buf).map_err(|e| Error::msg(e.to_string()))?;
        self.send_or_break(&buf).await?;
        let (mut first, mut tag): (Option<Error>, String) = (None, String::new());
        loop {
            match self.read_or_break().await? {
                Message::CopyInResponse(_) => {
                    let mut b = BytesMut::new();
                    // 큰 메시지 하나로 보내지 않고 1MiB 조각으로 나눈다
                    for chunk in data.chunks(1 << 20) {
                        frontend::CopyData::new(chunk).map_err(|e| Error::msg(e.to_string()))?.write(&mut b);
                    }
                    frontend::copy_done(&mut b);
                    self.send_or_break(&b).await?;
                }
                Message::CommandComplete(body) => tag = body.tag().unwrap_or_default().to_string(),
                Message::ErrorResponse(body) => {
                    first.get_or_insert(Error::Pg(pg_error(&body)));
                }
                Message::ReadyForQuery(body) => {
                    self.tx_status = body.status();
                    return first.map_or(Ok(tag), Err);
                }
                _ => {}
            }
        }
    }

    /// 연결을 정중히 닫는다(Terminate). 실패해도 할 일이 없다.
    pub async fn close(mut self) {
        let mut buf = BytesMut::new();
        frontend::terminate(&mut buf);
        let _ = tokio::time::timeout(Duration::from_secs(5), async {
            let _ = self.stream.write_all(&buf).await;
            let _ = self.stream.shutdown().await;
        })
        .await;
    }

    async fn send(&mut self, buf: &[u8]) -> Result<(), String> {
        self.stream.write_all(buf).await.map_err(|e| io_reason(&e))?;
        self.stream.flush().await.map_err(|e| io_reason(&e))
    }

    async fn read(&mut self) -> Result<Message, String> {
        loop {
            // 머리(종류 1 + 길이 4)를 보고 길이부터 막는다. postgres-protocol 은 길이만큼 버퍼를 먼저 잡는다
            if self.rbuf.len() >= 5 {
                let len = u32::from_be_bytes([self.rbuf[1], self.rbuf[2], self.rbuf[3], self.rbuf[4]]) as usize;
                if len > MAX_FRAME_BYTES {
                    return Err(format!("message length {len} exceeds {MAX_FRAME_BYTES} bytes"));
                }
            }
            if let Some(m) = Message::parse(&mut self.rbuf).map_err(|e| e.to_string())? {
                return Ok(m);
            }
            match self.stream.read_buf(&mut self.rbuf).await {
                Ok(0) => return Err("unexpected EOF".into()),
                Ok(_) => {}
                Err(e) => return Err(io_reason(&e)),
            }
        }
    }

    /// 실행 중 입출력이 끊기면 연결을 버린다고 표시한다. 글은 pgx 처럼 까닭만 싣는다.
    async fn send_or_break(&mut self, buf: &[u8]) -> Result<(), Error> {
        self.send(buf).await.map_err(|e| {
            self.broken = true;
            Error::msg(format!("write failed: {e}"))
        })
    }

    async fn read_or_break(&mut self) -> Result<Message, Error> {
        self.read().await.map_err(|e| {
            self.broken = true;
            Error::msg(e)
        })
    }
}

fn row_values(body: &DataRowBody, cap: bool) -> std::io::Result<Vec<Option<String>>> {
    let buf = body.buffer();
    body.ranges()
        .map(|r| {
            Ok(r.map(|r| {
                if cap {
                    cap_cell(&buf[r])
                } else {
                    String::from_utf8_lossy(&buf[r]).into_owned()
                }
            }))
        })
        .collect()
}

fn pg_error(body: &ErrorResponseBody) -> PgError {
    let mut e = PgError {
        severity: String::new(),
        code: String::new(),
        message: String::new(),
    };
    let mut fields = body.fields();
    while let Ok(Some(f)) = fields.next() {
        let v = String::from_utf8_lossy(f.value_bytes()).into_owned();
        match f.type_() {
            b'S' => e.severity = v,
            b'C' => e.code = v,
            b'M' => e.message = v,
            _ => {}
        }
    }
    e
}

/// pgconn.ConnectError 의 글이다. 까닭이 여러 줄이면 줄마다 탭을 넣는다.
fn connect_error(cfg: &Config, details: &str) -> Error {
    let prefix = format!("failed to connect to `user={} database={}`:", cfg.user, cfg.database);
    if details.contains('\n') {
        Error::msg(format!("{prefix}\n\t{}", details.replace('\n', "\n\t")))
    } else {
        Error::msg(format!("{prefix} {details}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut m = vec![tag];
        m.extend_from_slice(&(body.len() as u32 + 4).to_be_bytes());
        m.extend_from_slice(body);
        m
    }

    /// 시작 메시지를 받은 뒤 replies 를 차례로 보내는 가짜 서버다. 각 답 앞에 클라이언트 메시지 하나를 읽는다(첫 답 제외).
    async fn fake_server(replies: Vec<Vec<u8>>) -> u16 {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut len = [0u8; 4];
            s.read_exact(&mut len).await.unwrap();
            let mut rest = vec![0u8; u32::from_be_bytes(len) as usize - 4];
            s.read_exact(&mut rest).await.unwrap();
            for (i, r) in replies.into_iter().enumerate() {
                if i > 0 {
                    let mut head = [0u8; 5];
                    if s.read_exact(&mut head).await.is_err() {
                        return;
                    }
                    let mut body = vec![0u8; u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize - 4];
                    let _ = s.read_exact(&mut body).await;
                }
                let _ = s.write_all(&r).await;
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        port
    }

    fn cfg(port: u16) -> Config {
        Config {
            host: "127.0.0.1".into(),
            port,
            user: "u".into(),
            password: "pw".into(),
            database: "d".into(),
            sslmode: "disable".into(),
        }
    }

    async fn connect_err(replies: Vec<Vec<u8>>) -> String {
        let port = fake_server(replies).await;
        match tokio::time::timeout(Duration::from_secs(3), Conn::connect(&cfg(port))).await {
            Ok(Ok(_)) => "connected".into(),
            Ok(Err(e)) => e.to_string(),
            Err(_) => "timed out".into(),
        }
    }

    #[tokio::test]
    async fn SCRAM_서버_서명_확인_없이는_인증_성공으로_보지_않는다() {
        let sasl = msg(b'R', b"\0\0\0\x0aSCRAM-SHA-256\0\0");
        let ok = msg(b'R', &0u32.to_be_bytes());
        let ready = msg(b'Z', b"I");
        let err = connect_err(vec![sasl, [ok.clone(), ready.clone()].concat()]).await;
        assert!(err.ends_with("failed SASL auth: server skipped SCRAM verification"), "{err}");
        // AuthenticationOk 없이 ReadyForQuery 만 와도 받지 않는다
        let err = connect_err(vec![ready]).await;
        assert!(err.ends_with("received unexpected message"), "{err}");
    }

    #[tokio::test]
    async fn 길이_칸이_너무_큰_메시지는_버퍼를_잡기_전에_거절한다() {
        let mut huge = vec![b'R'];
        huge.extend_from_slice(&u32::MAX.to_be_bytes());
        let err = connect_err(vec![huge]).await;
        assert!(
            err.ends_with("failed to receive message: message length 4294967295 exceeds 1073741824 bytes"),
            "{err}"
        );
    }
}
