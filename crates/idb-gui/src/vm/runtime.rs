//! 엔진을 고르고 띄우는 일과, GPUI 쪽에서 엔진 퓨처를 tokio 위에서 돌리는 다리다.
//! 엔진(PG 와이어·RESP·ssh 자식)은 tokio 위에서만 돌고 GPUI 는 자기 실행기를 쓴다. 그래서 요청은 tokio 에 띄우고
//! 결과는 oneshot 으로 받는다. tokio 의 oneshot 은 어느 실행기에서 기다려도 된다.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use idb_engine::{Engine, EngineError, LocalEngine, RemoteEngine, RemoteSpec};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

/// 열 곳이다. 로컬 프로젝트 폴더이거나 `remote.json` 의 이름이다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    Local(PathBuf),
    Remote(String),
}

impl Target {
    pub fn label(&self) -> String {
        match self {
            Target::Local(p) => p.display().to_string(),
            Target::Remote(n) => format!("{n} (원격)"),
        }
    }
}

/// `remote.json` 의 이름들이다. 파일이 없으면 빈 목록이다.
pub fn remote_names() -> Result<Vec<String>, String> {
    let Some(path) = idb_engine::remote::config_path() else {
        return Ok(Vec::new());
    };
    match std::fs::read(&path) {
        Ok(raw) => parse_remote_names(&raw).map_err(|e| format!("remote config {}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("remote config {}: {e}", path.display())),
    }
}

fn parse_remote_names(raw: &[u8]) -> Result<Vec<String>, String> {
    let m: serde_json::Map<String, serde_json::Value> = serde_json::from_slice(raw).map_err(|e| e.to_string())?;
    let mut names: Vec<String> = m.keys().cloned().collect();
    names.sort();
    Ok(names)
}

/// 엔진을 띄운다. 로컬은 프로젝트를 읽고, 원격은 ssh 로 `db serve` 를 띄워 판 인사까지 마친다.
pub async fn connect(target: &Target) -> Result<Engine, EngineError> {
    match target {
        Target::Local(dir) => {
            let abs = std::path::absolute(dir).map_err(|e| EngineError::run(e.to_string()))?;
            let proj = idb_core::project::load_project(&abs).map_err(|e| EngineError::run(e.to_string()))?;
            Ok(Engine::Local(Arc::new(LocalEngine::new(proj, None))))
        }
        Target::Remote(name) => {
            let spec = RemoteSpec::resolve(name).map_err(EngineError::run)?;
            Ok(Engine::Remote(Arc::new(RemoteEngine::connect(&spec).await?)))
        }
    }
}

// ─── tokio 다리 ───

/// 띄운 요청이다. `rx` 로 결과를 기다리고 `cancel` 로 멈춘다. 멈추면 엔진 퓨처가 버려져 원격이면 cancel 이 간다.
pub struct Job<T> {
    pub rx: oneshot::Receiver<T>,
    abort: tokio::task::AbortHandle,
}

impl<T> Job<T> {
    pub fn canceler(&self) -> Canceler {
        Canceler(self.abort.clone())
    }
}

#[derive(Clone)]
pub struct Canceler(tokio::task::AbortHandle);

impl Canceler {
    pub fn cancel(&self) {
        self.0.abort();
    }
}

pub struct Runtime {
    rt: tokio::runtime::Runtime,
}

impl Runtime {
    pub fn new() -> std::io::Result<Runtime> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("idb-engine")
            .enable_all()
            .build()?;
        Ok(Runtime { rt })
    }

    pub fn spawn<F, T>(&self, fut: F) -> Job<T>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let h = self.rt.spawn(async move {
            let _ = tx.send(fut.await);
        });
        Job {
            rx,
            abort: h.abort_handle(),
        }
    }
}

// ─── 기억 ───

/// GUI 설정과 최근 연 곳이다. `~/.config/db/gui.json` 에 둔다. 비밀값은 싣지 않는다.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Prefs {
    #[serde(default)]
    pub recent: Vec<Target>,
    /// 화면 전체(글·편집기·결과 표) 글꼴. 없으면 넣어 둔 D2Coding 을 쓴다
    #[serde(default = "default_font")]
    pub font: String,
    #[serde(default = "default_font_size")]
    pub font_size: f32,
}

fn default_font() -> String {
    super::font::DEFAULT_FONT.into()
}

fn default_font_size() -> f32 {
    super::font::DEFAULT_SIZE
}

impl Default for Prefs {
    fn default() -> Prefs {
        Prefs {
            recent: Vec::new(),
            font: default_font(),
            font_size: default_font_size(),
        }
    }
}

const MAX_RECENT: usize = 8;

impl Prefs {
    pub fn path() -> Option<PathBuf> {
        idb_engine::remote::config_path().map(|p| p.with_file_name("gui.json"))
    }

    pub fn load(path: &Path) -> Prefs {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?)
    }

    /// 맨 앞으로 올리고 중복·넘침을 뗀다.
    pub fn touch(&mut self, t: Target) {
        self.recent.retain(|r| r != &t);
        self.recent.insert(0, t);
        self.recent.truncate(MAX_RECENT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 원격_이름은_정렬해서_돌려준다() {
        let raw = br#"{"zeta":{"ssh":"z","project":"p"},"devbox":{"ssh":"devbox","project":"~/p"}}"#;
        assert_eq!(parse_remote_names(raw).unwrap(), vec!["devbox", "zeta"]);
        assert!(parse_remote_names(b"[1]").is_err());
    }

    #[test]
    fn 최근_목록은_앞으로_올리고_여덟_개까지_둔다() {
        let mut p = Prefs::default();
        for i in 0..10 {
            p.touch(Target::Remote(format!("r{i}")));
        }
        p.touch(Target::Remote("r5".into()));
        assert_eq!(p.recent.len(), MAX_RECENT);
        assert_eq!(p.recent[0], Target::Remote("r5".into()));
        assert_eq!(p.recent.iter().filter(|t| **t == Target::Remote("r5".into())).count(), 1);
    }

    #[test]
    fn 글꼴_설정을_읽고_없으면_기본값이다() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gui.json");
        std::fs::write(&path, r#"{"font": "Menlo", "font_size": 14}"#).unwrap();
        let p = Prefs::load(&path);
        assert_eq!((p.font.as_str(), p.font_size), ("Menlo", 14.0));
        std::fs::write(&path, r#"{"recent": []}"#).unwrap();
        let p = Prefs::load(&path);
        assert_eq!((p.font.as_str(), p.font_size), ("D2Coding", 13.0));
    }

    #[test]
    fn 기억은_파일로_저장하고_다시_읽는다() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db").join("gui.json");
        let mut p = Prefs::default();
        p.touch(Target::Local("/tmp/proj".into()));
        p.save(&path).unwrap();
        assert_eq!(Prefs::load(&path), p);
        assert_eq!(Prefs::load(&dir.path().join("none.json")), Prefs::default());
    }

    #[test]
    fn 띄운_요청은_결과를_돌려주고_멈추면_답이_없다() {
        let rt = Runtime::new().unwrap();
        let job = rt.spawn(async { 1 + 1 });
        assert_eq!(rt.rt.block_on(job.rx).unwrap(), 2);

        let job = rt.spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            1
        });
        job.canceler().cancel();
        assert!(rt.rt.block_on(job.rx).is_err());
    }

    #[test]
    fn 로컬_프로젝트를_열면_접속_목록과_트리가_나온다() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/golden/offline");
        let rt = Runtime::new().unwrap();
        let job = rt.spawn(async move {
            let e = connect(&Target::Local(dir)).await?;
            let p = e.project().await;
            e.close().await;
            p
        });
        let info = rt.rt.block_on(job.rx).unwrap().unwrap();
        assert!(!info.conns.is_empty());
        let tree = crate::vm::tree::ConnTree::new(info.conns);
        assert!(tree.rows().iter().any(|r| matches!(r.kind, crate::vm::tree::RowKind::Conn(_))));
    }

    #[test]
    fn 없는_프로젝트는_실행_오류다() {
        let rt = Runtime::new().unwrap();
        let job = rt.spawn(async { connect(&Target::Local("/nonexistent/i-db-gui".into())).await.map(|_| ()) });
        let err = rt.rt.block_on(job.rx).unwrap().unwrap_err();
        assert_eq!(err.code, idb_engine::CODE_RUN);
    }
}
