//! `db serve`: 줄 JSON 요청을 받아 LocalEngine 으로 처리하고 응답을 줄로 낸다. 포트를 열지 않고 표준 입출력만 쓴다.
//! 요청마다 작업 하나라 동시에 돈다. 응답은 끝난 순서다. 입력이 닫히면 남은 작업을 멈추고 세션을 닫는다.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures_util::FutureExt;
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

use crate::local::LocalEngine;
use crate::protocol::*;

/// 요청 한 줄의 상한. CLI 표준 입력 16MiB 가 JSON 이스케이프로 늘어도 들어간다
pub const MAX_REQUEST_LINE: usize = 64 << 20;

/// 입력이 닫힌 뒤 남은 작업을 기다리는 시간. CLI 기본 --timeout(30s)보다 길다
const DRAIN_GRACE: std::time::Duration = std::time::Duration::from_secs(60);

type Tasks = Arc<Mutex<HashMap<u64, AbortHandle>>>;

/// 입력이 닫힐 때까지 요청을 처리한다.
pub async fn serve<R, W>(engine: Arc<LocalEngine>, input: R, output: W) -> std::io::Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    engine.set_serving();
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        let mut output = output;
        while let Some(line) = rx.recv().await {
            if output.write_all(line.as_bytes()).await.is_err() || output.write_all(b"\n").await.is_err() || output.flush().await.is_err() {
                break;
            }
        }
    });
    let tasks: Tasks = Arc::default();
    let mut input = input;
    let mut hello = false;
    let result = loop {
        let line = match read_line(&mut input, MAX_REQUEST_LINE).await {
            Ok(Some(Ok(line))) => line,
            Ok(Some(Err(()))) => {
                let _ = tx.send(response_line(0, &Err(EngineError::usage("request line too long"))));
                continue;
            }
            Ok(None) => break Ok(()),
            Err(e) => break Err(e),
        };
        if line.trim().is_empty() {
            continue;
        }
        let (id, req) = parse_request(&line);
        let req = match req {
            Ok(r) => r,
            Err(e) => {
                let _ = tx.send(response_line(id, &Err(e)));
                continue;
            }
        };
        match req {
            Request::Hello { version } => {
                let res = if version == PROTOCOL_VERSION {
                    hello = true;
                    engine.handle(Request::Hello { version }).await
                } else {
                    Err(EngineError::usage(format!(
                        "unsupported protocol version {version} (server speaks {PROTOCOL_VERSION})"
                    )))
                };
                let _ = tx.send(response_line(id, &res));
            }
            _ if !hello => {
                let _ = tx.send(response_line(id, &Err(EngineError::usage("hello first"))));
            }
            Request::Cancel { target } => {
                // 응답 보내기와 지우기를 같은 잠금 안에서 해 이미 답한 요청에 canceled 를 또 보내지 않는다
                let canceled = {
                    let mut t = tasks.lock().unwrap();
                    match t.remove(&target) {
                        Some(h) => {
                            h.abort();
                            let _ = tx.send(response_line(target, &Err(EngineError::run("canceled"))));
                            true
                        }
                        None => false,
                    }
                };
                let _ = tx.send(response_line(id, &Ok(serde_json::json!({ "canceled": canceled }))));
            }
            req => {
                let mut t = tasks.lock().unwrap();
                if t.contains_key(&id) {
                    let _ = tx.send(response_line(id, &Err(EngineError::usage(format!("duplicate id {id}")))));
                    continue;
                }
                let (engine, tx2, tasks2) = (engine.clone(), tx.clone(), tasks.clone());
                // 잠금을 쥔 채 띄우고 넣어야 작업이 끝나 지우는 일이 넣기보다 앞서지 않는다
                let h = tokio::spawn(async move {
                    let res = guarded(engine.handle(req)).await;
                    if tasks2.lock().unwrap().remove(&id).is_some() {
                        let _ = tx2.send(response_line(id, &res));
                    }
                });
                t.insert(id, h.abort_handle());
            }
        }
    };
    // 입력을 다 보내고 닫은 클라이언트도 답을 받게 남은 작업을 잠시 기다린다. 끊긴 연결의 긴 쿼리는 그 뒤 멈춘다
    let grace = tokio::time::Instant::now() + DRAIN_GRACE;
    while !tasks.lock().unwrap().is_empty() && tokio::time::Instant::now() < grace {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let pending: Vec<AbortHandle> = tasks.lock().unwrap().drain().map(|(_, h)| h).collect();
    for h in pending {
        h.abort();
    }
    engine.close().await;
    drop(tx);
    let _ = writer.await;
    result
}

/// op 처리기가 패닉해도 그 요청에 `internal error` 로 답한다. 답이 없으면 클라이언트가 그 id 를 끝없이 기다린다.
async fn guarded(fut: impl std::future::Future<Output = Result<Value, EngineError>>) -> Result<Value, EngineError> {
    std::panic::AssertUnwindSafe(fut)
        .catch_unwind()
        .await
        .unwrap_or_else(|_| Err(EngineError::run("internal error")))
}

/// 줄 하나를 읽는다. 상한을 넘으면 그 줄을 끝까지 버리고 Some(Err) 다. 끝이면 None 이다.
async fn read_line<R: AsyncBufRead + Unpin>(r: &mut R, max: usize) -> std::io::Result<Option<Result<String, ()>>> {
    let mut buf = Vec::new();
    let mut over = false;
    loop {
        let chunk = r.fill_buf().await?;
        if chunk.is_empty() {
            if buf.is_empty() && !over {
                return Ok(None);
            }
            break;
        }
        let (take, done) = match chunk.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (chunk.len(), false),
        };
        if !over {
            if buf.len() + take > max + 1 {
                over = true;
                buf = Vec::new();
            } else {
                buf.extend_from_slice(&chunk[..take]);
            }
        }
        r.consume(take);
        if done {
            break;
        }
    }
    if over {
        return Ok(Some(Err(())));
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
    }
    Ok(Some(Ok(String::from_utf8_lossy(&buf).into_owned())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn 처리기가_패닉하면_internal_error_로_답한다() {
        let res = guarded(async { panic!("boom") }).await;
        assert_eq!(res.unwrap_err().error, "internal error");
        assert_eq!(guarded(async { Ok(Value::from(1)) }).await.unwrap(), Value::from(1));
    }

    #[tokio::test]
    async fn 긴_줄은_버리고_다음_줄을_읽는다() {
        let data = b"aaaaaaaaaa\nbb\n".to_vec();
        let mut r = tokio::io::BufReader::new(&data[..]);
        assert_eq!(read_line(&mut r, 4).await.unwrap(), Some(Err(())));
        assert_eq!(read_line(&mut r, 4).await.unwrap(), Some(Ok("bb".into())));
        assert_eq!(read_line(&mut r, 4).await.unwrap(), None);
    }
}
