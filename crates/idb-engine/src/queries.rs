//! 프로젝트의 쿼리 파일(queries/, R5·R43·R45)과 즐겨찾기(R29) op. 원격이면 파일은 serve 쪽 프로젝트에 있고
//! 화면은 이 op 로만 읽고 쓴다(로컬도 같은 길). 사용자 자신의 쿼리 파일이라 원격에서도 쓰기를 받는다(DB 쓰기가 아님).
//! 경계: `queries/` 아래 `.sql` 만, `..`·절대 경로를 받지 않고, 읽고 쓰기 전마다 실제 경로(링크를 따라간 곳)가
//! 프로젝트의 queries 안인지 본다. 한 파일은 1MiB 까지이고, 쓰기는 원자적이며 권한은 원본을 따른다.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use idb_core::mapping::{WriteError, add_mapping_checked, conflict_path, real_path, unique_query_path, write_project_file};
use idb_core::project::Project;

use crate::protocol::{EngineError, QueryFile, QueryText, QueryWritten};

/// 한 쿼리 파일의 상한이다
pub const MAX_QUERY_BYTES: usize = 1 << 20;

pub fn hash(text: &str) -> String {
    idb_core::session::disk_hash(text.as_bytes())
}

/// 받을 수 있는 상대 경로인지 본다: `queries/…/<이름>.sql`, 빈 마디·`.`·`..`·절대 경로·역슬래시·NUL 없음.
pub fn check_rel(rel: &str) -> Result<(), EngineError> {
    let bad = || EngineError::usage(format!("쿼리 파일 경로가 아님: {}", idb_core::textutil::printable(rel, false)));
    if !rel.starts_with("queries/") || !rel.ends_with(".sql") || rel.contains(['\\', '\0']) {
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
    let rel = unique_query_path(&proj.dir, &c.group, &c.name, chrono::Local::now()).map_err(|e| EngineError::run(e.to_string()))?;
    write_file(&proj.dir, &rel, content)?;
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
    let copy = conflict_path(&proj.dir, rel, chrono::Local::now()).map_err(|e| EngineError::run(e.to_string()))?;
    write_file(&proj.dir, &copy, content)?;
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
}
