//! TCP·TLS 연결. 오류 글은 Go `net` 패키지 꼴(`dial tcp 127.0.0.1:1: connect: connection refused`)로 맞춘다.

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;

/// 평문 또는 TLS 연결이다.
pub(crate) enum Stream {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

impl AsyncRead for Stream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_flush(cx),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

/// host 의 주소들이다. 실패 글은 Go 의 `lookup <host>: no such host` 꼴이다.
pub(crate) async fn resolve(host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
    match tokio::net::lookup_host((host, port)).await {
        Ok(addrs) => {
            let v: Vec<SocketAddr> = addrs.collect();
            if v.is_empty() {
                Err(format!("lookup {host}: no such host"))
            } else {
                Ok(v)
            }
        }
        Err(_) => Err(format!("lookup {host}: no such host")),
    }
}

/// addr 로 TCP 를 연다. 실패 글은 `dial tcp <addr>: connect: <까닭>` 이다.
pub(crate) async fn dial(addr: SocketAddr) -> Result<TcpStream, String> {
    let s = TcpStream::connect(addr)
        .await
        .map_err(|e| format!("dial tcp {addr}: connect: {}", io_reason(&e)))?;
    let _ = s.set_nodelay(true);
    Ok(s)
}

/// 입출력 오류의 Go 식 까닭 글이다(syscall.Errno 글과 같다).
pub(crate) fn io_reason(e: &io::Error) -> String {
    match e.kind() {
        io::ErrorKind::ConnectionRefused => "connection refused".into(),
        io::ErrorKind::ConnectionReset => "connection reset by peer".into(),
        io::ErrorKind::ConnectionAborted => "software caused connection abort".into(),
        io::ErrorKind::TimedOut => "i/o timeout".into(),
        io::ErrorKind::NetworkUnreachable => "network is unreachable".into(),
        io::ErrorKind::HostUnreachable => "no route to host".into(),
        io::ErrorKind::BrokenPipe => "broken pipe".into(),
        io::ErrorKind::UnexpectedEof => "EOF".into(),
        _ => {
            // "Connection refused (os error 111)" 의 꼬리는 Go 글에 없다
            let s = e.to_string();
            let s = s.split(" (os error").next().unwrap_or(&s).to_string();
            let mut c = s.chars();
            match c.next() {
                Some(f) => f.to_lowercase().chain(c).collect(),
                None => s,
            }
        }
    }
}
