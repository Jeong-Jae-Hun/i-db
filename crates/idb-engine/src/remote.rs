//! 원격 엔진: `ssh <호스트> <db> serve -p <프로젝트>` 를 띄워 표준 입출력으로 serve 프로토콜을 주고받는다.
//! 시스템 ssh 를 써서 ~/.ssh/config·키·ProxyJump 가 그대로 듣는다. 비밀번호·.env·VPN 경로는 원격에만 있다.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};

use crate::protocol::*;

/// 시험 전용: 설정 파일과 ssh 대신 이 명령을 띄운다(`db-rs serve -p 폴더` 처럼 공백으로 나눈다).
pub const REMOTE_CMD_ENV: &str = "DB_REMOTE_CMD";
/// 응답 한 줄의 상한. 결과는 1,000행 × 64KiB 칸이 넘지 않는다
const MAX_RESPONSE_LINE: usize = 256 << 20;
const HELLO_TIMEOUT: Duration = Duration::from_secs(30);
/// ssh 가 남긴 오류 글에서 보일 끝부분
const STDERR_TAIL: usize = 2048;

/// 원격 하나의 띄울 명령이다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteSpec {
    pub name: String,
    pub program: String,
    pub args: Vec<String>,
}

/// `remote.json` 의 항목이다.
#[derive(Deserialize)]
struct RemoteEntry {
    ssh: String,
    #[serde(default)]
    db: String,
    project: String,
}

impl RemoteSpec {
    /// 이름의 원격을 찾는다. DB_REMOTE_CMD 가 있으면 그것을 띄운다(시험 전용).
    pub fn resolve(name: &str) -> Result<RemoteSpec, String> {
        if let Some(cmd) = std::env::var(REMOTE_CMD_ENV).ok().filter(|c| !c.trim().is_empty()) {
            let mut parts = cmd.split_whitespace().map(String::from);
            let program = parts.next().unwrap_or_default();
            return Ok(RemoteSpec {
                name: name.into(),
                program,
                args: parts.collect(),
            });
        }
        let path = config_path().ok_or("remote config: no home directory")?;
        let raw = std::fs::read(&path).map_err(|e| format!("remote config {}: {e}", path.display()))?;
        Self::from_config(name, &path, &raw)
    }

    fn from_config(name: &str, path: &std::path::Path, raw: &[u8]) -> Result<RemoteSpec, String> {
        let mut all: HashMap<String, RemoteEntry> =
            serde_json::from_slice(raw).map_err(|e| format!("remote config {}: {e}", path.display()))?;
        let e = all
            .remove(name)
            .ok_or_else(|| format!("remote {name} is not in {}", path.display()))?;
        Self::ssh(name, &e.ssh, if e.db.is_empty() { "db" } else { &e.db }, &e.project)
    }

    /// ssh 로 원격 db 의 serve 를 띄우는 명령이다. BatchMode 라 비밀번호·호스트 키 묻기에서 멈추지 않고 실패한다.
    pub fn ssh(name: &str, host: &str, db: &str, project: &str) -> Result<RemoteSpec, String> {
        // - 로 시작하는 호스트는 ssh 옵션으로 읽힌다
        if host.is_empty() || host.starts_with('-') {
            return Err(format!("remote {name}: invalid ssh host"));
        }
        let remote = [shell_arg(db), "serve".into(), "-p".into(), shell_arg(project)].join(" ");
        Ok(RemoteSpec {
            name: name.into(),
            program: "ssh".into(),
            args: vec![
                "-o".into(),
                "BatchMode=yes".into(),
                "-o".into(),
                "ConnectTimeout=15".into(),
                "-T".into(),
                host.into(),
                remote,
            ],
        })
    }
}

/// `$XDG_CONFIG_HOME/db/remote.json`, 없으면 `~/.config/db/remote.json` 이다.
pub fn config_path() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(std::env::var_os("HOME").filter(|v| !v.is_empty())?).join(".config"),
    };
    Some(base.join("db").join("remote.json"))
}

/// 원격 셸에 넘길 낱말이다. 앞의 `~/` 는 원격 셸이 펼치게 따옴표 밖에 둔다.
fn shell_arg(s: &str) -> String {
    let (home, rest) = match s.strip_prefix("~/") {
        Some(rest) => ("~/", rest),
        None => ("", s),
    };
    if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_alphanumeric() || b"/._-+=:@,".contains(&b)) {
        return format!("{home}{rest}");
    }
    format!("{home}'{}'", rest.replace('\'', r"'\''"))
}

type Reply = Result<Value, EngineError>;

#[derive(Default)]
struct Pending {
    waiters: HashMap<u64, oneshot::Sender<Reply>>,
    /// 연결이 끊긴 까닭. 있으면 새 요청도 바로 이 오류다
    closed: Option<String>,
}

pub struct RemoteEngine {
    name: String,
    tx: Mutex<Option<mpsc::UnboundedSender<String>>>,
    pending: Arc<Mutex<Pending>>,
    next: AtomicU64,
    child: Arc<tokio::sync::Mutex<Option<Child>>>,
}

impl RemoteEngine {
    /// 원격 serve 를 띄우고 판 인사까지 마친다.
    pub async fn connect(spec: &RemoteSpec) -> Result<RemoteEngine, EngineError> {
        let fail = |e: String| EngineError::run(format!("remote {}: {e}", spec.name));
        let mut child = Command::new(&spec.program)
            .args(&spec.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| fail(format!("start {}: {e}", spec.program)))?;
        let (mut stdin, stdout, mut stderr) = (
            child.stdin.take().expect("piped stdin"),
            child.stdout.take().expect("piped stdout"),
            child.stderr.take().expect("piped stderr"),
        );
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            while let Some(line) = rx.recv().await {
                if stdin.write_all(line.as_bytes()).await.is_err() || stdin.write_all(b"\n").await.is_err() || stdin.flush().await.is_err()
                {
                    break;
                }
            }
            // 보낼 쪽이 모두 닫히면 stdin 도 닫혀 serve 가 끝난다
        });
        let tail = Arc::new(Mutex::new(String::new()));
        let tail2 = tail.clone();
        let stderr_done = tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            while let Ok(n) = stderr.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                let mut t = tail2.lock().unwrap();
                t.push_str(&String::from_utf8_lossy(&buf[..n]));
                if t.len() > STDERR_TAIL * 2 {
                    let cut = t.len() - STDERR_TAIL;
                    let cut = (cut..t.len()).find(|&i| t.is_char_boundary(i)).unwrap_or(t.len());
                    t.drain(..cut);
                }
            }
        });
        let pending: Arc<Mutex<Pending>> = Arc::default();
        let child = Arc::new(tokio::sync::Mutex::new(Some(child)));
        let (pending2, child2, name) = (pending.clone(), child.clone(), spec.name.clone());
        tokio::spawn(async move {
            let mut out = BufReader::new(stdout);
            let why = loop {
                let mut line = Vec::new();
                match (&mut out).take(MAX_RESPONSE_LINE as u64 + 1).read_until(b'\n', &mut line).await {
                    Ok(0) => break "connection closed".to_string(),
                    Ok(_) if line.len() > MAX_RESPONSE_LINE => break "response line too long".to_string(),
                    Ok(_) => {}
                    Err(e) => break e.to_string(),
                }
                let Ok((id, res)) = parse_response(String::from_utf8_lossy(&line).trim_end()) else {
                    break "bad response from remote".to_string();
                };
                if let Some(w) = pending2.lock().unwrap().waiters.remove(&id) {
                    let _ = w.send(res);
                }
            };
            // 끝난 까닭에 종료 코드와 ssh 가 남긴 글(인증 실패 등)을 붙인다
            let _ = tokio::time::timeout(Duration::from_secs(3), stderr_done).await;
            let mut why = format!("remote {name}: {why}");
            if let Some(mut c) = child2.lock().await.take()
                && let Ok(Ok(st)) = tokio::time::timeout(Duration::from_secs(3), c.wait()).await
                && let Some(code) = st.code()
                && code != 0
            {
                why.push_str(&format!(" (exit status {code})"));
            }
            let t = tail.lock().unwrap().trim().to_string();
            if !t.is_empty() {
                let last: Vec<&str> = t.lines().rev().take(3).collect();
                why.push_str(": ");
                why.push_str(&last.into_iter().rev().collect::<Vec<_>>().join(" / "));
            }
            let mut p = pending2.lock().unwrap();
            p.closed = Some(why.clone());
            for (_, w) in p.waiters.drain() {
                let _ = w.send(Err(EngineError::run(why.clone())));
            }
        });
        let eng = RemoteEngine {
            name: spec.name.clone(),
            tx: Mutex::new(Some(tx)),
            pending,
            next: AtomicU64::new(1),
            child,
        };
        let hello = tokio::time::timeout(HELLO_TIMEOUT, eng.call(Request::Hello { version: PROTOCOL_VERSION }))
            .await
            .map_err(|_| fail("no answer from remote serve".into()))??;
        let h: Hello = serde_json::from_value(hello).map_err(|e| fail(format!("bad hello: {e}")))?;
        if h.version != PROTOCOL_VERSION {
            return Err(fail(format!("protocol version {} (want {PROTOCOL_VERSION})", h.version)));
        }
        Ok(eng)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    fn send(&self, line: String) -> bool {
        self.tx.lock().unwrap().as_ref().is_some_and(|tx| tx.send(line).is_ok())
    }

    /// 요청 하나를 보내고 응답을 기다린다. 기다리는 퓨처를 버리면 원격에 cancel 을 보낸다.
    pub async fn call(&self, req: Request) -> Reply {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (wtx, wrx) = oneshot::channel();
        {
            let mut p = self.pending.lock().unwrap();
            if let Some(why) = &p.closed {
                return Err(EngineError::run(why.clone()));
            }
            p.waiters.insert(id, wtx);
        }
        let mut guard = CancelOnDrop {
            eng: self,
            id,
            armed: true,
        };
        if !self.send(request_line(id, &req)) {
            guard.armed = false;
            self.pending.lock().unwrap().waiters.remove(&id);
            return Err(EngineError::run(format!("remote {}: connection closed", self.name)));
        }
        let res = wrx.await;
        guard.armed = false;
        match res {
            Ok(r) => r,
            Err(_) => Err(EngineError::run(
                self.pending
                    .lock()
                    .unwrap()
                    .closed
                    .clone()
                    .unwrap_or_else(|| format!("remote {}: connection closed", self.name)),
            )),
        }
    }

    /// 표준 입력을 닫아 원격 serve 가 세션을 닫고 끝나게 하고 잠시 기다린다.
    pub async fn close(&self) {
        self.tx.lock().unwrap().take();
        if let Some(mut c) = self.child.lock().await.take()
            && tokio::time::timeout(Duration::from_secs(5), c.wait()).await.is_err()
        {
            let _ = c.kill().await;
        }
    }
}

struct CancelOnDrop<'a> {
    eng: &'a RemoteEngine,
    id: u64,
    armed: bool,
}

impl Drop for CancelOnDrop<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.eng.pending.lock().unwrap().waiters.remove(&self.id);
        let cid = self.eng.next.fetch_add(1, Ordering::Relaxed);
        self.eng.send(request_line(cid, &Request::Cancel { target: self.id }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 원격_셸_낱말은_물결표를_펼치고_나머지는_따옴표로_싼다() {
        assert_eq!(shell_arg("~/.local/bin/db"), "~/.local/bin/db");
        assert_eq!(shell_arg("~/My Projects/db"), "~/'My Projects/db'");
        assert_eq!(shell_arg("/a/b;rm -rf x"), "'/a/b;rm -rf x'");
        assert_eq!(shell_arg("it's"), r"'it'\''s'");
    }

    #[test]
    fn ssh_명령은_BatchMode_이고_옵션처럼_보이는_호스트는_거절한다() {
        let s = RemoteSpec::ssh("devbox", "devbox", "~/.local/bin/db", "~/work/db").unwrap();
        assert_eq!(s.program, "ssh");
        assert_eq!(
            s.args,
            [
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=15",
                "-T",
                "devbox",
                "~/.local/bin/db serve -p ~/work/db"
            ]
        );
        assert!(RemoteSpec::ssh("x", "-oProxyCommand=evil", "db", "p").is_err());
    }

    #[test]
    fn 설정_파일의_이름으로_ssh_명령을_만든다() {
        let raw =
            br#"{"devbox":{"ssh":"devbox","db":"~/.local/bin/db","project":"~/work/db"},"bare":{"ssh":"h","project":"/p"}}"#;
        let p = std::path::Path::new("/x/remote.json");
        let s = RemoteSpec::from_config("devbox", p, raw).unwrap();
        assert_eq!(s.args.last().unwrap(), "~/.local/bin/db serve -p ~/work/db");
        assert_eq!(
            RemoteSpec::from_config("bare", p, raw).unwrap().args.last().unwrap(),
            "db serve -p /p"
        );
        assert_eq!(
            RemoteSpec::from_config("nope", p, raw).unwrap_err(),
            "remote nope is not in /x/remote.json"
        );
    }
}
