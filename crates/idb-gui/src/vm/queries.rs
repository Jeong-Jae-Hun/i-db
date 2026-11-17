//! 프로젝트 `queries/` 파일(R43)과 새 쿼리 자동 저장(R45). 로컬이든 원격이든 엔진 op(`query_*`)로만 읽고 쓴다.
//! 원격이면 serve 쪽 프로젝트의 파일이고, 경계(queries/ 밖·링크 탈출·1MiB)와 매핑은 엔진이 지킨다.
//! ⌘O 로 고른 이 Mac 의 파일(프로젝트 밖)만 여기서 직접 쓴다.

use std::path::Path;
use std::time::Duration;

use idb_engine::{ConnSummary, Engine, QueryText, QueryWritten, Request};
use serde::de::DeserializeOwned;

/// 쿼리 파일 요청 하나를 기다리는 한도다. 닫을 때 저장을 기다리는 한도도 같다(TUI 와 같다).
pub const SAVE_TIMEOUT: Duration = Duration::from_secs(20);

pub const AUTOSAVE_STOPPED: &str = "파일이 밖에서 바뀌어 자동 저장 멈춤 (⌘S 로 덮어쓰기)";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryFile {
    /// 프로젝트 상대 경로(`queries/app/a.sql`). 매핑 열쇠와 같다
    pub rel: String,
    /// `queries/` 뒤 경로(화면 글)
    pub name: String,
    /// 붙은 접속 이름. 매핑이 없거나 그 uuid 의 접속이 없으면 None
    pub conn: Option<String>,
    /// 유닉스 초
    pub mtime: i64,
}

/// query_list 의 답을 화면 줄로 바꾼다. 순서(최근 수정 순)는 엔진 것을 그대로 둔다.
pub fn from_engine(files: Vec<idb_engine::QueryFile>) -> Vec<QueryFile> {
    files
        .into_iter()
        .map(|f| QueryFile {
            name: f.path.strip_prefix("queries/").unwrap_or(&f.path).to_string(),
            conn: (!f.conn.is_empty()).then_some(f.conn),
            rel: f.path,
            mtime: f.mtime,
        })
        .collect()
}

/// 접속별 묶음이다. 접속 순서는 conns 순서, 붙지 않은 파일은 맨 뒤(None)다.
pub fn group<'a>(files: &'a [QueryFile], conns: &[ConnSummary]) -> Vec<(Option<String>, Vec<&'a QueryFile>)> {
    let mut out: Vec<(Option<String>, Vec<&QueryFile>)> = Vec::new();
    for c in conns {
        let v: Vec<&QueryFile> = files.iter().filter(|f| f.conn.as_deref() == Some(c.name.as_str())).collect();
        if !v.is_empty() {
            out.push((Some(c.name.clone()), v));
        }
    }
    let none: Vec<&QueryFile> = files.iter().filter(|f| f.conn.is_none()).collect();
    if !none.is_empty() {
        out.push((None, none));
    }
    out
}

// ─── 엔진 op ───

async fn call<T: DeserializeOwned>(e: &Engine, req: Request) -> Result<T, String> {
    let v = tokio::time::timeout(SAVE_TIMEOUT, e.call(req))
        .await
        .map_err(|_| "쿼리 파일 요청이 20초 안에 끝나지 않음".to_string())?
        .map_err(|e| e.error)?;
    serde_json::from_value(v).map_err(|e| format!("bad result: {e}"))
}

pub async fn list(e: &Engine) -> Result<Vec<QueryFile>, String> {
    call::<Vec<idb_engine::QueryFile>>(e, Request::QueryList).await.map(from_engine)
}

pub async fn read(e: &Engine, rel: &str) -> Result<String, String> {
    call::<QueryText>(e, Request::QueryRead { path: rel.into() })
        .await
        .map(|t| t.content)
}

/// disk 는 화면이 마지막으로 읽거나 쓴 내용이다. 그사이 바뀌었으면 쓰지 않고 conflict 다. force(⌘S)는 그래도 덮는다.
pub async fn write(e: &Engine, rel: &str, text: &str, disk: Option<&str>, force: bool) -> Result<QueryWritten, String> {
    call(
        e,
        Request::QueryWrite {
            path: rel.into(),
            content: text.into(),
            base_hash: disk.map(idb_engine::queries::hash).unwrap_or_default(),
            force,
        },
    )
    .await
}

/// 첫 실행 저장(R45.1). 경로는 엔진이 `queries/<그룹>/<접속>-<시각>.sql` 로 정하고 매핑을 붙인다.
pub async fn create(e: &Engine, conn: &str, text: &str) -> Result<QueryWritten, String> {
    call(
        e,
        Request::QueryNew {
            conn: conn.into(),
            content: text.into(),
        },
    )
    .await
}

/// 덮을 수 없는 글을 rel 옆 충돌 사본에 남긴다(R45.3).
pub async fn conflict_copy(e: &Engine, rel: &str, text: &str, conn: &str) -> Result<QueryWritten, String> {
    call(
        e,
        Request::QueryConflict {
            path: rel.into(),
            content: text.into(),
            conn: conn.into(),
        },
    )
    .await
}

/// 저장 답을 상태 줄 글로 바꾼다. 매핑만 실패했으면 그 까닭이 먼저다.
pub fn saved_note(w: &QueryWritten) -> String {
    if !w.mapping_error.is_empty() {
        w.mapping_error.clone()
    } else if w.created {
        format!("새로 만듦: {}", w.path)
    } else {
        format!("저장함: {}", w.path)
    }
}

// ─── 이 Mac 의 파일(⌘O) ───

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SaveDecision {
    /// 파일이 이미 편집기 글과 같다
    Skip,
    Write,
    /// 밖에서 바뀌었거나 지워졌다. 자동 저장을 멈춘다(⌘S 는 덮어쓴다)
    Stop(String),
}

/// ⌘O 로 연 파일의 쓰기 전 판단이다. disk 는 지금 파일 내용(없으면 None), last 는 마지막으로 읽거나 쓴 내용이다.
/// 수정 시각이 아니라 내용으로 가른다(쿼리 파일은 엔진의 base_hash 가 같은 일을 한다).
pub fn before_autosave(disk: Option<&str>, last: &str, editor: &str) -> SaveDecision {
    match disk {
        Some(d) if d == editor => SaveDecision::Skip,
        Some(d) if d == last => SaveDecision::Write,
        Some(_) => SaveDecision::Stop(AUTOSAVE_STOPPED.into()),
        None => SaveDecision::Stop("파일이 지워져 자동 저장 멈춤 (⌘S 로 다시 만들기)".into()),
    }
}

/// ⌘O 로 고른 파일을 그대로 쓴다. 원자적으로 쓰고 원래 권한을 지킨다(R5.2, 새 파일은 0644).
pub fn write_local(path: &Path, text: &str) -> Result<(), String> {
    let real = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let perm = std::fs::metadata(&real)
        .map(|m| std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o777)
        .unwrap_or(0o644);
    idb_core::atomicfile::write(&real, ".db-*", text.as_bytes(), perm).map_err(|e| e.to_string())
}

/// `DB_AUTO_SAVE=0` 이면 자동 저장(첫 실행 저장·탭 옮김·닫기·1초 뒤)을 하지 않는다(TUI 와 같다).
pub fn autosave_enabled(var: Option<&str>) -> bool {
    var != Some("0")
}

/// 같은 target 의 `db` 실행 파일이다(원격 serve 시험용). `cargo build -p idb-cli` 뒤에만 있다.
#[cfg(test)]
pub(crate) fn test_db_bin() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let bin = exe.parent()?.parent()?.join("db");
    bin.is_file().then_some(bin)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(name: &str, uuid: &str) -> ConnSummary {
        ConnSummary {
            name: name.into(),
            uuid: uuid.into(),
            ..Default::default()
        }
    }

    fn ef(path: &str, conn: &str, mtime: i64) -> idb_engine::QueryFile {
        idb_engine::QueryFile {
            path: path.into(),
            conn: conn.into(),
            mtime,
            ..Default::default()
        }
    }

    #[test]
    fn 엔진_목록을_순서대로_화면_줄로_바꾸고_접속으로_묶는다() {
        let files = from_engine(vec![
            ef("queries/app/new.sql", "app-stg", 3),
            ef("queries/loose.sql", "", 2),
            ef("queries/app/old.sql", "app-stg", 1),
        ]);
        let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["app/new.sql", "loose.sql", "app/old.sql"]);
        assert_eq!(files[0].conn.as_deref(), Some("app-stg"));
        assert_eq!(files[1].conn, None, "빈 접속은 접속 없음");
        let g = group(&files, &[conn("app-stg", "u1")]);
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].0.as_deref(), Some("app-stg"));
        assert_eq!(g[0].1.len(), 2);
        assert_eq!(g[1].0, None);
    }

    #[test]
    fn 저장_알림은_매핑_실패를_먼저_보인다() {
        let mut w = QueryWritten {
            path: "queries/a.sql".into(),
            ..Default::default()
        };
        assert_eq!(saved_note(&w), "저장함: queries/a.sql");
        w.created = true;
        assert_eq!(saved_note(&w), "새로 만듦: queries/a.sql");
        w.mapping_error = "saved queries/a.sql but mapping failed: x".into();
        assert!(saved_note(&w).contains("mapping failed"));
    }

    #[test]
    fn 이_Mac_파일_저장은_0600_을_넓히지_않는다() {
        use std::os::unix::fs::PermissionsExt as _;
        let out = tempfile::tempdir().unwrap();
        let o = out.path().join("b.sql");
        std::fs::write(&o, "x").unwrap();
        std::fs::set_permissions(&o, std::fs::Permissions::from_mode(0o600)).unwrap();
        write_local(&o, "z").unwrap();
        assert_eq!(std::fs::metadata(&o).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_to_string(&o).unwrap(), "z");
    }

    #[test]
    fn 자동_저장_끄기는_0_일_때만이다() {
        assert!(autosave_enabled(None));
        assert!(autosave_enabled(Some("1")));
        assert!(!autosave_enabled(Some("0")));
    }

    #[test]
    fn 자동_저장은_내용으로_가른다() {
        assert_eq!(before_autosave(Some("a"), "a", "a"), SaveDecision::Skip);
        assert_eq!(before_autosave(Some("a"), "a", "b"), SaveDecision::Write);
        assert!(matches!(before_autosave(Some("x"), "a", "b"), SaveDecision::Stop(m) if m.contains("밖에서 바뀌어")));
        assert!(matches!(before_autosave(None, "a", "b"), SaveDecision::Stop(m) if m.contains("지워져")));
    }

    // ─── 로컬·원격 같음 ───

    fn project() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let src = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/golden/offline");
        copy_dir(&src, d.path());
        d
    }

    fn copy_dir(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for e in std::fs::read_dir(from).unwrap().flatten() {
            let (src, dst) = (e.path(), to.join(e.file_name()));
            if e.file_type().unwrap().is_dir() {
                copy_dir(&src, &dst);
            } else {
                std::fs::copy(&src, &dst).unwrap();
            }
        }
    }

    /// 한 엔진으로 화면이 하는 쿼리 파일 흐름을 돌리고 경로를 뺀 결과를 남긴다(시각이 든 이름은 꼴만 본다).
    async fn scenario(e: &Engine) -> Vec<String> {
        let mut log = Vec::new();
        let files = list(e).await.unwrap();
        let a = files.iter().find(|f| f.rel == "queries/app/a.sql").expect("a.sql");
        log.push(format!("list a conn={:?}", a.conn));
        let disk = read(e, "queries/app/a.sql").await.unwrap();
        let w = write(e, "queries/app/a.sql", "select 42", Some(&disk), false).await.unwrap();
        log.push(format!("autosave conflict={} created={}", w.conflict, w.created));
        // 밖에서(다른 편집기) 바꾼 뒤 자동 저장은 쓰지 않는다
        let w = write(e, "queries/app/a.sql", "-- 다른 편집기", Some("select 42"), false)
            .await
            .unwrap();
        log.push(format!("other conflict={}", w.conflict));
        let w = write(e, "queries/app/a.sql", "select 43", Some("select 42"), false).await.unwrap();
        log.push(format!("stale conflict={}", w.conflict));
        log.push(format!("disk={}", read(e, "queries/app/a.sql").await.unwrap()));
        let c = conflict_copy(e, "queries/app/a.sql", "select 43", "app-stg").await.unwrap();
        log.push(format!(
            "copy {} {}",
            c.path.starts_with("queries/app/a-conflict-"),
            read(e, &c.path).await.unwrap()
        ));
        let w = write(e, "queries/app/a.sql", "select 44", Some("select 42"), true).await.unwrap();
        log.push(format!(
            "force conflict={} disk={}",
            w.conflict,
            read(e, "queries/app/a.sql").await.unwrap()
        ));
        let n = create(e, "app-stg", "select 1").await.unwrap();
        log.push(format!("new {} map={}", n.path.starts_with("queries/"), n.mapping_error.is_empty()));
        let after = list(e).await.unwrap();
        log.push(format!(
            "new listed conn={:?}",
            after.iter().find(|f| f.rel == n.path).map(|f| f.conn.clone())
        ));
        log.push(format!("outside {:?}", read(e, "../etc/passwd.sql").await.err()));
        log
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn 쿼리_파일_흐름은_로컬_엔진에서_자동_저장_충돌_사본_덮기_새_쿼리가_된다() {
        let d = project();
        let rt = rt();
        let log = rt.block_on(async {
            let e = crate::vm::runtime::connect(&crate::vm::runtime::Target::Local(d.path().into()))
                .await
                .unwrap();
            scenario(&e).await
        });
        assert_eq!(log[0], "list a conn=Some(\"app-stg\")");
        assert_eq!(log[1], "autosave conflict=false created=false");
        assert_eq!(log[2], "other conflict=false", "다른 편집기를 흉내 낸 쓰기(base 가 맞다)");
        assert_eq!(log[3], "stale conflict=true", "밖의 바뀜은 덮지 않는다");
        assert_eq!(log[4], "disk=-- 다른 편집기");
        assert_eq!(log[5], "copy true select 43");
        assert_eq!(log[6], "force conflict=false disk=select 44");
        assert_eq!(log[7], "new true map=true");
        assert_eq!(log[8], "new listed conn=Some(Some(\"app-stg\"))");
        assert!(log[9].contains("쿼리 파일 경로가 아님"), "{}", log[9]);
    }

    /// 원격(serve 너머)도 같은 결과여야 devbox 사이드바·저장이 로컬과 같다. `db` 실행 파일이 없으면 건너뛴다.
    #[test]
    fn 쿼리_파일_흐름은_원격_엔진에서도_로컬과_같다() {
        let Some(bin) = test_db_bin() else {
            eprintln!("skip: target 에 db 가 없다 (cargo build -p idb-cli)");
            return;
        };
        let (dl, dr) = (project(), project());
        let rt = rt();
        let (local, remote) = rt.block_on(async {
            let l = crate::vm::runtime::connect(&crate::vm::runtime::Target::Local(dl.path().into()))
                .await
                .unwrap();
            let spec = idb_engine::RemoteSpec {
                name: "test".into(),
                program: bin.display().to_string(),
                args: vec!["serve".into(), "-p".into(), dr.path().display().to_string()],
            };
            let r = Engine::Remote(std::sync::Arc::new(idb_engine::RemoteEngine::connect(&spec).await.unwrap()));
            let out = (scenario(&l).await, scenario(&r).await);
            r.close().await;
            out
        });
        assert_eq!(local, remote);
    }
}
