//! 프로젝트의 쿼리 파일(queries/, R5·R43·R45)과 즐겨찾기(R29) op. 원격이면 파일은 serve 쪽 프로젝트에 있고
//! 화면은 이 op 로만 읽고 쓴다(로컬도 같은 길). 사용자 자신의 쿼리 파일이라 원격에서도 쓰기를 받는다(DB 쓰기가 아님).
//! 경계: `queries/` 아래 `.sql` 만, `..`·절대 경로를 받지 않고, 읽고 쓰기 전마다 실제 경로(링크를 따라간 곳)가
//! 프로젝트의 queries 안인지 본다. 한 파일은 1MiB 까지이고, 쓰기는 원자적이며 권한은 원본을 따른다.
//! 해시 확인과 쓰기, 새 이름 고르기, 매핑 고치기는 queries 폴더의 flock 하나로 줄 세운다. flock 은 연 파일마다
//! 걸려 같은 프로세스의 두 스레드도 서로 기다리므로 프로세스 안 Mutex 를 따로 두지 않는다(프로젝트마다 따로 잠긴다).

use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

use idb_core::mapping::{WriteError, add_mapping_checked, conflict_path, real_path, unique_query_path, write_project_file};
use idb_core::project::Project;

use idb_core::favorites::{Fav, Store, normalize_name};

use crate::protocol::{EngineError, FavoriteSaved, QueryFile, QueryText, QueryWritten};

/// 한 쿼리 파일의 상한이다
pub const MAX_QUERY_BYTES: usize = 1 << 20;

pub fn hash(text: &str) -> String {
    idb_core::session::disk_hash(text.as_bytes())
}

/// 받을 수 있는 상대 경로인지 본다: `queries/…/<이름>.sql`, 빈 마디·`.`·`..`·절대 경로·역슬래시 없음.
/// 제어 문자와 XML 에 못 넣는 글자(U+FFFE·U+FFFF)도 받지 않는다. 매핑 XML 에 들어가면 다음 프로젝트 읽기가 깨진다.
pub fn check_rel(rel: &str) -> Result<(), EngineError> {
    let bad = || EngineError::usage(format!("쿼리 파일 경로가 아님: {}", idb_core::textutil::printable(rel, false)));
    let illegal = |c: char| c == '\\' || c.is_control() || c == '\u{FFFE}' || c == '\u{FFFF}';
    if !rel.starts_with("queries/") || !rel.ends_with(".sql") || rel.contains(illegal) {
        return Err(bad());
    }
    if rel.split('/').any(|p| p.is_empty() || p == "." || p == "..") {
        return Err(bad());
    }
    Ok(())
}

/// rel 의 실제 경로다. 링크를 따라간 곳이 프로젝트의 queries 밖이면 오류다.
fn real(dir: &Path, rel: &str) -> Result<PathBuf, EngineError> {
    check_rel(rel)?;
    let outside = || EngineError::usage(format!("쿼리 파일의 실제 경로가 queries 밖: {rel}"));
    let p = real_path(dir, rel).map_err(|_| outside())?;
    let root = real_path(dir, "queries").map_err(|_| outside())?;
    if !p.starts_with(&root) {
        return Err(outside());
    }
    Ok(p)
}

/// 잠금을 기다리는 상한이다(시험은 짧게)
const LOCK_WAIT: Duration = if cfg!(test) {
    Duration::from_millis(300)
} else {
    Duration::from_secs(10)
};

/// 잡고 있는 동안 다른 쓰기(이 프로세스의 다른 스레드와 다른 프로세스)가 기다린다. 닫으면 풀린다.
struct Locked {
    _file: File,
}

/// queries 폴더(없으면 만든다)에 배타 flock 을 건다. 커밋 폴더에 잠금 파일을 남기지 않으려고 폴더 자체에 건다.
fn lock(dir: &Path) -> Result<Locked, EngineError> {
    // LOCK_WAIT 안에 못 얻으면 끝없이 서지 않고 오류다
    let deadline = Instant::now() + LOCK_WAIT;
    let timeout = || EngineError::run("queries 잠금을 시간 안에 얻지 못함");
    let outside = || EngineError::usage("queries 폴더의 실제 경로가 프로젝트 밖");
    let root = real_path(dir, "queries").map_err(|_| outside())?;
    std::fs::create_dir_all(&root).map_err(|e| EngineError::run(format!("쿼리 파일을 쓰지 못함: {e}")))?;
    if real_path(dir, "queries").map_err(|_| outside())? != root {
        return Err(outside());
    }
    let file = File::open(&root).map_err(|e| EngineError::run(format!("queries 잠금: {e}")))?;
    loop {
        match file.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Err(std::fs::TryLockError::WouldBlock) => return Err(timeout()),
            Err(std::fs::TryLockError::Error(e)) => return Err(EngineError::run(format!("queries 잠금: {e}"))),
        }
    }
    Ok(Locked { _file: file })
}

/// 고른 새 이름을 create_new 로 먼저 잡는다. 이미 있으면 false 다(다른 쪽이 그사이 만들었다).
fn claim(dir: &Path, rel: &str) -> Result<bool, EngineError> {
    let p = real(dir, rel)?;
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(|e| EngineError::run(format!("쿼리 파일을 쓰지 못함: {e}")))?;
    }
    let p = real(dir, rel)?;
    match OpenOptions::new().write(true).create_new(true).mode(0o644).open(&p) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(EngineError::run(format!("쿼리 파일을 쓰지 못함: {e}"))),
    }
}

/// pick 이 고른 새 이름을 잡아 content 를 쓴다. 쓰지 못하면 잡아 둔 빈 파일을 지운다.
fn write_new(dir: &Path, content: &str, pick: impl Fn() -> std::io::Result<String>) -> Result<String, EngineError> {
    if content.len() > MAX_QUERY_BYTES {
        return Err(EngineError::usage("쿼리 파일은 1MiB 까지만 저장"));
    }
    for _ in 0..5 {
        let rel = pick().map_err(|e| EngineError::run(e.to_string()))?;
        check_rel(&rel)?;
        if !claim(dir, &rel)? {
            continue;
        }
        if let Err(e) = write_file(dir, &rel, content) {
            if let Ok(p) = real(dir, &rel) {
                let _ = std::fs::remove_file(p);
            }
            return Err(e);
        }
        return Ok(rel);
    }
    Err(EngineError::run("새 쿼리 파일 이름을 고르지 못함"))
}

fn read_file(dir: &Path, rel: &str) -> Result<Option<String>, EngineError> {
    let p = real(dir, rel)?;
    let meta = match std::fs::metadata(&p) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(EngineError::run(e.to_string())),
    };
    if !meta.is_file() {
        return Err(EngineError::usage(format!("일반 파일이 아님: {rel}")));
    }
    if meta.len() as usize > MAX_QUERY_BYTES {
        return Err(EngineError::usage(format!("쿼리 파일이 1MiB 를 넘음: {rel}")));
    }
    std::fs::read_to_string(&p).map(Some).map_err(|e| EngineError::run(e.to_string()))
}

fn write_file(dir: &Path, rel: &str, content: &str) -> Result<(), EngineError> {
    if content.len() > MAX_QUERY_BYTES {
        return Err(EngineError::usage("쿼리 파일은 1MiB 까지만 저장"));
    }
    real(dir, rel)?;
    write_project_file(dir, rel, content).map_err(|e| match e {
        WriteError::Outside(_) => EngineError::usage(format!("쿼리 파일의 실제 경로가 queries 밖: {rel}")),
        // 읽기 전용 디스크·권한 없음은 그대로 알린다
        WriteError::Io(e) => EngineError::run(format!("쿼리 파일을 쓰지 못함: {e}")),
    })?;
    // 쓴 뒤에도 다시 본다(그사이 바뀐 링크를 따라 밖에 쓰지 않았는지)
    real(dir, rel).map(|_| ())
}

/// queries/ 아래 .sql 파일을 최근 수정 순(같으면 이름 순)으로 돌려준다. 밖을 가리키는 링크·읽지 못하는 곳은 뺀다.
pub fn list(proj: &Project) -> Vec<QueryFile> {
    let dir = &proj.dir;
    let mut out: Vec<QueryFile> = Vec::new();
    let Ok(root) = real_path(dir, "queries") else { return out };
    let mut stack = vec![dir.join("queries")];
    let mut seen = 0usize;
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            seen += 1;
            if seen > 100_000 {
                break;
            }
            let p = e.path();
            let Ok(rel) = p.strip_prefix(dir) else { continue };
            let rel = rel.to_string_lossy().replace('\\', "/");
            let Ok(rp) = real_path(dir, &rel) else { continue };
            if !rp.starts_with(&root) {
                continue;
            }
            let Ok(meta) = std::fs::metadata(&p) else { continue };
            if meta.is_dir() {
                stack.push(p);
            } else if check_rel(&rel).is_ok() && meta.is_file() {
                let uuid = proj.mapping.get(&rel).cloned().unwrap_or_default();
                let conn = proj
                    .conns
                    .iter()
                    .find(|c| !uuid.is_empty() && c.uuid == uuid)
                    .map(|c| c.name.clone())
                    .unwrap_or_default();
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_secs() as i64);
                out.push(QueryFile {
                    path: rel,
                    conn_uuid: uuid,
                    conn,
                    mtime,
                    size: meta.len(),
                });
            }
        }
    }
    out.sort_by(|a, b| b.mtime.cmp(&a.mtime).then(a.path.cmp(&b.path)));
    out
}

pub fn read(proj: &Project, rel: &str) -> Result<QueryText, EngineError> {
    match read_file(&proj.dir, rel)? {
        Some(content) => Ok(QueryText {
            hash: hash(&content),
            content,
        }),
        None => Err(EngineError::run(format!("쿼리 파일이 없음: {rel}"))),
    }
}

/// base_hash 는 화면이 마지막으로 읽거나 쓴 내용의 해시다(비면 없던 파일). 지금 내용이 그것과 다르면 쓰지 않고 conflict 와
/// 지금 해시(지워졌으면 빈 글)를 돌려준다. force 는 그래도 덮는다(Ctrl+S). 내용이 이미 같으면 쓰지 않는다.
pub fn write(proj: &Project, rel: &str, content: &str, base_hash: &str, force: bool) -> Result<QueryWritten, EngineError> {
    check_rel(rel)?;
    let _lock = lock(&proj.dir)?;
    let cur = read_file(&proj.dir, rel)?;
    let cur_hash = cur.as_deref().map(hash).unwrap_or_default();
    if cur.as_deref() == Some(content) {
        return Ok(QueryWritten {
            path: rel.into(),
            hash: cur_hash,
            ..Default::default()
        });
    }
    if !force && cur_hash != base_hash {
        return Ok(QueryWritten {
            path: rel.into(),
            hash: cur_hash,
            conflict: true,
            ..Default::default()
        });
    }
    write_file(&proj.dir, rel, content)?;
    Ok(QueryWritten {
        path: rel.into(),
        hash: hash(content),
        created: cur.is_none(),
        ..Default::default()
    })
}

/// 매핑을 붙인다. 실패해도 파일은 남기고 그 까닭을 돌려준다(R45.1).
fn map(proj: &Project, rel: &str, uuid: &str) -> String {
    if uuid.is_empty() {
        return String::new();
    }
    match add_mapping_checked(&proj.dir, rel, uuid) {
        Ok(()) => String::new(),
        Err(e) => format!("saved {rel} but mapping failed: {e}"),
    }
}

/// 새 쿼리를 `queries/<그룹>/<접속>-<YYYYMMDD-HHmmss>.sql` 에 쓰고 그 접속을 매핑에 붙인다(R45.1).
pub fn new_file(proj: &Project, conn: &str, content: &str) -> Result<QueryWritten, EngineError> {
    let c = proj
        .conn_by_name(conn)
        .ok_or_else(|| EngineError::usage(format!("접속 없음: {conn}")))?;
    let _lock = lock(&proj.dir)?;
    let rel = write_new(&proj.dir, content, || {
        unique_query_path(&proj.dir, &c.group, &c.name, chrono::Local::now())
    })?;
    let mapping_error = map(proj, &rel, &c.uuid);
    Ok(QueryWritten {
        hash: hash(content),
        created: true,
        mapping_error,
        uuid: c.uuid.clone(),
        path: rel,
        ..Default::default()
    })
}

/// 덮을 수 없는 글을 rel 옆 `<이름>-conflict-<시각>.sql` 에 남기고 원래 파일이 붙은 접속(없으면 conn)을 붙인다(R45.3).
pub fn conflict_copy(proj: &Project, rel: &str, content: &str, conn: &str) -> Result<QueryWritten, EngineError> {
    check_rel(rel)?;
    let _lock = lock(&proj.dir)?;
    let copy = write_new(&proj.dir, content, || conflict_path(&proj.dir, rel, chrono::Local::now()))?;
    let uuid = proj
        .mapping
        .get(rel)
        .cloned()
        .or_else(|| proj.conn_by_name(conn).map(|c| c.uuid.clone()))
        .unwrap_or_default();
    let mapping_error = map(proj, &copy, &uuid);
    Ok(QueryWritten {
        hash: hash(content),
        created: true,
        mapping_error,
        uuid,
        path: copy,
        ..Default::default()
    })
}

// ─── 즐겨찾기 (R29) ───

/// 즐겨찾기 한 개 SQL 의 상한이다
pub const MAX_FAV_SQL: usize = 256 << 10;
/// 즐겨찾기 수의 상한이다
pub const MAX_FAVS: usize = 1000;
/// 즐겨찾기 파일의 상한이다. 읽기·쓰기 모두 이것을 넘으면 멈춘다
pub const MAX_FAV_FILE: u64 = 4 << 20;
const FAV_REL: &str = ".dbtui/favorites.json";

/// `.dbtui` 폴더나 즐겨찾기 파일이 링크면 따라가지 않는다. 실제 경로도 프로젝트 안이어야 한다.
fn fav_check(dir: &Path) -> Result<(), EngineError> {
    let refuse = || EngineError::usage("즐겨찾기 파일의 실제 경로가 프로젝트 밖이거나 링크라 쓰지 않음");
    for rel in [".dbtui", FAV_REL] {
        if std::fs::symlink_metadata(dir.join(rel)).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(refuse());
        }
    }
    real_path(dir, FAV_REL).map_err(|_| refuse())?;
    match std::fs::metadata(dir.join(FAV_REL)) {
        Ok(m) if !m.is_file() => Err(EngineError::usage("즐겨찾기 파일이 일반 파일이 아님")),
        Ok(m) if m.len() > MAX_FAV_FILE => Err(EngineError::usage("즐겨찾기 파일이 4MiB 를 넘어 읽지 않음")),
        _ => Ok(()),
    }
}

fn fav_store(dir: &Path) -> Result<Store, EngineError> {
    fav_check(dir)?;
    Store::load(dir).map_err(|e| EngineError::run(e.to_string()))
}

/// 이름순 목록이다. MAX_FAVS 개까지만 돌려준다.
pub fn favorites(dir: &Path) -> Result<Vec<Fav>, EngineError> {
    let mut v = fav_store(dir)?.list();
    v.truncate(MAX_FAVS);
    Ok(v)
}

/// 같은 이름이 있고 overwrite 가 아니면 쓰지 않고 exists 다.
pub fn favorite_add(dir: &Path, name: &str, conn: &str, sql: &str, overwrite: bool) -> Result<FavoriteSaved, EngineError> {
    let name = normalize_name(name).map_err(|e| EngineError::usage(e.to_string()))?;
    if sql.len() > MAX_FAV_SQL {
        return Err(EngineError::usage("즐겨찾기 SQL 은 256KiB 까지만 저장"));
    }
    let st = fav_store(dir)?;
    let list = st.list();
    let old = list.iter().find(|f| f.name == name);
    if old.is_some() && !overwrite {
        return Ok(FavoriteSaved {
            saved: false,
            exists: true,
        });
    }
    if old.is_none() && list.len() >= MAX_FAVS {
        return Err(EngineError::usage(format!("즐겨찾기는 {MAX_FAVS}개까지만 저장")));
    }
    // 제어 문자는 JSON 에서 6배까지 늘어 어림이 틀리므로, 실제로 쓸 바이트로 가른다(넘으면 원본을 그대로 둔다)
    st.put_max(
        Fav {
            name,
            conn: conn.into(),
            sql: sql.into(),
            saved: Default::default(),
        },
        MAX_FAV_FILE as usize,
    )
    .map_err(|e| match e.kind() {
        std::io::ErrorKind::InvalidInput if e.to_string().contains("byte limit") => {
            EngineError::usage("즐겨찾기 파일이 4MiB 를 넘게 되어 저장하지 않음")
        }
        std::io::ErrorKind::InvalidInput => EngineError::usage(e.to_string()),
        _ => EngineError::run(e.to_string()),
    })?;
    fav_check(dir)?;
    Ok(FavoriteSaved {
        saved: true,
        exists: false,
    })
}

pub fn favorite_remove(dir: &Path, name: &str) -> Result<(), EngineError> {
    fav_store(dir)?.delete(name).map_err(|e| EngineError::run(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proj(dir: &Path) -> Project {
        Project {
            dir: dir.to_path_buf(),
            conns: vec![idb_core::project::Conn {
                name: "app-stg".into(),
                uuid: "u1".into(),
                group: "app".into(),
                kind: Some(idb_core::project::Kind::Pg),
                ..Default::default()
            }],
            mapping: Default::default(),
            warnings: vec![],
            aes_env: Default::default(),
        }
    }

    #[test]
    fn queries_밖이나_링크로_빠지는_경로는_읽지도_쓰지도_않는다() {
        let d = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("queries")).unwrap();
        std::fs::write(d.path().join(".env"), "PW=secret").unwrap();
        std::fs::write(out.path().join("x.sql"), "outside").unwrap();
        std::os::unix::fs::symlink(out.path().join("x.sql"), d.path().join("queries/link.sql")).unwrap();
        std::os::unix::fs::symlink(d.path().join(".env"), d.path().join("queries/env.sql")).unwrap();
        let p = proj(d.path());
        for rel in [
            "../x.sql",
            "queries/../.env",
            "/etc/passwd",
            ".env",
            "queries/a.txt",
            "queries//a.sql",
            "queries/./a.sql",
        ] {
            assert_eq!(read(&p, rel).unwrap_err().code, idb_engine_usage(), "{rel}");
            assert!(write(&p, rel, "x", "", true).is_err(), "{rel}");
        }
        // 링크가 프로젝트 밖·queries 밖(.env)을 가리키면 막는다
        assert!(read(&p, "queries/link.sql").unwrap_err().error.contains("queries 밖"));
        assert!(read(&p, "queries/env.sql").unwrap_err().error.contains("queries 밖"));
        assert!(write(&p, "queries/link.sql", "new", "", true).is_err());
        assert_eq!(std::fs::read_to_string(out.path().join("x.sql")).unwrap(), "outside");
        assert!(list(&p).is_empty(), "밖을 가리키는 링크는 목록에도 없다");
    }

    fn idb_engine_usage() -> i32 {
        crate::protocol::CODE_USAGE
    }

    #[test]
    fn 쓰기는_마지막_해시가_다르면_충돌이고_새_쿼리는_매핑을_붙인다() {
        let d = tempfile::tempdir().unwrap();
        let p = proj(d.path());
        let n = new_file(&p, "app-stg", "select 1").unwrap();
        assert!(n.path.starts_with("queries/app/app-stg-") && n.mapping_error.is_empty(), "{n:?}");
        let map = std::fs::read_to_string(d.path().join(".idea/data_source_mapping.xml")).unwrap();
        assert!(map.contains(&format!("file://$PROJECT_DIR$/{}", n.path)) && map.contains("value=\"u1\""));
        let r = read(&p, &n.path).unwrap();
        assert_eq!((r.content.as_str(), r.hash.as_str()), ("select 1", n.hash.as_str()));
        // 밖에서 바뀌었으면 쓰지 않고 지금 해시를 알린다
        std::fs::write(d.path().join(&n.path), "select 2").unwrap();
        let w = write(&p, &n.path, "select 3", &n.hash, false).unwrap();
        assert!(w.conflict && w.hash == hash("select 2"));
        assert_eq!(std::fs::read_to_string(d.path().join(&n.path)).unwrap(), "select 2");
        // Ctrl+S 는 덮고, 같은 내용이면 쓰지 않는다
        let w = write(&p, &n.path, "select 3", &n.hash, true).unwrap();
        assert!(!w.conflict && w.hash == hash("select 3"));
        assert!(!write(&p, &n.path, "select 3", "", false).unwrap().conflict);
        // 충돌 사본은 옆에 두고 원래 접속을 붙인다
        let mut p2 = p.clone();
        p2.mapping.insert(n.path.clone(), "u1".into());
        let c = conflict_copy(&p2, &n.path, "mine", "").unwrap();
        assert!(c.path.contains("-conflict-") && c.uuid == "u1");
        assert_eq!(list(&p2).len(), 2);
        assert!(write(&p, &n.path, &"x".repeat(MAX_QUERY_BYTES + 1), "", true).is_err());
    }

    #[test]
    fn 제어_문자가_든_경로는_받지_않고_매핑_XML_을_깨지_않는다() {
        let d = tempfile::tempdir().unwrap();
        let p = proj(d.path());
        new_file(&p, "app-stg", "select 1").unwrap();
        for rel in [
            "queries/a\u{1}.sql",
            "queries/a\u{7f}.sql",
            "queries/a\u{FFFE}.sql",
            "queries/a\nb.sql",
        ] {
            assert_eq!(read(&p, rel).unwrap_err().code, idb_engine_usage(), "{rel:?}");
            assert!(write(&p, rel, "x", "", true).is_err(), "{rel:?}");
            assert!(conflict_copy(&p, rel, "x", "app-stg").is_err(), "{rel:?}");
        }
        // 매핑 파일이 그대로 XML 로 읽힌다(insert_mapping 이 파싱한다)
        let map = std::fs::read_to_string(d.path().join(".idea/data_source_mapping.xml")).unwrap();
        assert!(idb_core::mapping::insert_mapping(&map, "file://x", "u").is_ok());
    }

    #[test]
    fn 같은_해시로_동시에_쓰면_하나만_되고_새_이름은_겹치지_않는다() {
        let d = tempfile::tempdir().unwrap();
        let p = proj(d.path());
        let n = new_file(&p, "app-stg", "base").unwrap();
        for round in 0..20 {
            let base = read(&p, &n.path).unwrap().hash;
            let bar = std::sync::Barrier::new(4);
            let ok: usize = std::thread::scope(|s| {
                let hs: Vec<_> = (0..4)
                    .map(|i| {
                        let (p, n, base, bar) = (&p, &n, &base, &bar);
                        s.spawn(move || {
                            bar.wait();
                            usize::from(!write(p, &n.path, &format!("r{round}-{i}"), base, false).unwrap().conflict)
                        })
                    })
                    .collect();
                hs.into_iter().map(|h| h.join().unwrap()).sum()
            });
            assert_eq!(ok, 1, "round {round}");
        }
        let bar = std::sync::Barrier::new(8);
        let paths: Vec<String> = std::thread::scope(|s| {
            let hs: Vec<_> = (0..8)
                .map(|i| {
                    let (p, bar) = (&p, &bar);
                    s.spawn(move || {
                        bar.wait();
                        new_file(p, "app-stg", &format!("q{i}")).unwrap().path
                    })
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let uniq: std::collections::BTreeSet<_> = paths.iter().collect();
        assert_eq!(uniq.len(), 8);
        let map = std::fs::read_to_string(d.path().join(".idea/data_source_mapping.xml")).unwrap();
        for (i, rel) in paths.iter().enumerate() {
            assert_eq!(std::fs::read_to_string(d.path().join(rel)).unwrap(), format!("q{i}"));
            assert!(map.contains(rel.as_str()), "매핑에서 빠짐: {rel}");
        }
    }

    #[test]
    fn 즐겨찾기는_링크를_따라가지_않고_크기와_수를_넘지_않는다() {
        let d = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        std::fs::write(out.path().join("favorites.json"), "[]").unwrap();
        // 폴더 링크
        std::os::unix::fs::symlink(out.path(), d.path().join(".dbtui")).unwrap();
        assert!(favorites(d.path()).is_err());
        assert!(favorite_add(d.path(), "q", "", "select 1", false).is_err());
        assert!(favorite_remove(d.path(), "q").is_err());
        assert_eq!(std::fs::read_to_string(out.path().join("favorites.json")).unwrap(), "[]");
        // 파일 링크
        std::fs::remove_file(d.path().join(".dbtui")).unwrap();
        std::fs::create_dir(d.path().join(".dbtui")).unwrap();
        std::os::unix::fs::symlink(out.path().join("favorites.json"), d.path().join(FAV_REL)).unwrap();
        assert!(favorites(d.path()).is_err());
        assert!(favorite_add(d.path(), "q", "", "select 1", false).is_err());
        assert_eq!(std::fs::read_to_string(out.path().join("favorites.json")).unwrap(), "[]");
        std::fs::remove_file(d.path().join(FAV_REL)).unwrap();

        assert!(favorite_add(d.path(), "big", "", &"x".repeat(MAX_FAV_SQL + 1), false).is_err());
        let data: Vec<serde_json::Value> = (0..MAX_FAVS)
            .map(|i| serde_json::json!({"name": format!("f{i:04}"), "conn": "", "sql": "select 1", "saved": "2026-11-17T10:00:00+09:00"}))
            .collect();
        std::fs::write(d.path().join(FAV_REL), serde_json::to_vec(&data).unwrap()).unwrap();
        assert_eq!(favorites(d.path()).unwrap().len(), MAX_FAVS);
        assert!(
            favorite_add(d.path(), "one-more", "", "select 1", false)
                .unwrap_err()
                .error
                .contains("1000개")
        );
        assert!(
            favorite_add(d.path(), "f0001", "", "select 2", true).unwrap().saved,
            "있는 이름은 덮을 수 있다"
        );
        // 제어 문자는 JSON 에서 \u0001 처럼 6배로 늘어난다. 쓴 뒤 4MiB 를 넘을 저장은 원본을 두고 거절한다
        std::fs::remove_file(d.path().join(FAV_REL)).unwrap();
        let ctl = "\u{1}".repeat(MAX_FAV_SQL);
        let mut saved = 0;
        let err = loop {
            match favorite_add(d.path(), &format!("c{saved:02}"), "", &ctl, false) {
                Ok(_) => saved += 1,
                Err(e) => break e,
            }
            assert!(saved < 10, "4MiB 를 넘기 전에 멈춰야 한다");
        };
        assert!(err.error.contains("4MiB"), "{err:?}");
        assert!(std::fs::metadata(d.path().join(FAV_REL)).unwrap().len() <= MAX_FAV_FILE);
        assert_eq!(favorites(d.path()).unwrap().len(), saved, "넘기 전 파일은 그대로 읽힌다");
        std::fs::write(d.path().join(FAV_REL), vec![b' '; MAX_FAV_FILE as usize + 1]).unwrap();
        assert!(favorites(d.path()).unwrap_err().error.contains("4MiB"));
    }

    /// 리뷰: 잠금을 async 안에서 그대로 기다려 바깥 시간 제한이 듣지 않았다.
    #[tokio::test(flavor = "current_thread")]
    async fn 잠금을_기다리는_동안에도_바깥_시간_제한이_듣고_끝내_오류다() {
        let d = tempfile::tempdir().unwrap();
        let p = proj(d.path());
        let n = new_file(&p, "app-stg", "a").unwrap();
        // 다른 쪽이 queries 잠금을 쥐고 있다
        let held = File::open(d.path().join("queries")).unwrap();
        held.lock().unwrap();
        let engine = crate::Engine::Local(std::sync::Arc::new(crate::LocalEngine::new(p, None)));
        let req = crate::Request::QueryWrite {
            path: n.path.clone(),
            content: "b".into(),
            base_hash: n.hash.clone(),
            force: false,
        };
        let t0 = Instant::now();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), engine.call(req.clone()))
                .await
                .is_err()
        );
        assert!(t0.elapsed() < Duration::from_millis(250), "{:?}", t0.elapsed());
        let e = engine.call(req).await.unwrap_err();
        assert!(e.error.contains("잠금"), "{e:?}");
        assert_eq!(std::fs::read_to_string(d.path().join(&n.path)).unwrap(), "a");
    }
}
