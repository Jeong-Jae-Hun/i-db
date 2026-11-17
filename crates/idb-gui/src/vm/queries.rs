//! 프로젝트 `queries/` 파일(R43)과 새 쿼리 자동 저장(R45). 로컬 프로젝트에서만 쓴다(원격은 파일이 serve 쪽에 있다).
//! 파일 → 접속은 `.idea/data_source_mapping.xml`(Project.mapping, 상대 경로 → 접속 uuid)로 잇는다.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use idb_engine::ConnSummary;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryFile {
    /// 프로젝트 상대 경로(`queries/app/a.sql`). 매핑 열쇠와 같다
    pub rel: String,
    /// `queries/` 뒤 경로(화면 글)
    pub name: String,
    /// 붙은 접속 이름. 매핑이 없거나 그 uuid 의 접속이 없으면 None
    pub conn: Option<String>,
    pub mtime: SystemTime,
}

/// `queries/` 아래 .sql 파일을 최근 수정 순으로 모은다. 읽지 못하는 폴더·파일은 건너뛰고,
/// 심볼릭 링크는 실제 경로가 프로젝트 안일 때만 넣는다(R43.1).
pub fn scan(project: &Path, mapping: &HashMap<String, String>, conns: &[ConnSummary]) -> Vec<QueryFile> {
    let Ok(root) = project.canonicalize() else { return Vec::new() };
    let by_uuid: HashMap<&str, &str> = conns
        .iter()
        .filter(|c| !c.uuid.is_empty())
        .map(|c| (c.uuid.as_str(), c.name.as_str()))
        .collect();
    let mut out = Vec::new();
    let mut stack = vec![PathBuf::from("queries")];
    while let Some(rel_dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(project.join(&rel_dir)) else {
            continue;
        };
        for ent in rd.flatten() {
            let rel = rel_dir.join(ent.file_name());
            let full = project.join(&rel);
            let Ok(real) = full.canonicalize() else { continue };
            if !real.starts_with(&root) {
                continue;
            }
            let Ok(meta) = std::fs::metadata(&real) else { continue };
            if meta.is_dir() {
                // 링크가 위 폴더를 가리켜 도는 것을 막는다
                if stack.len() < 64 && !rel.components().skip(1).any(|c| c.as_os_str() == "..") {
                    stack.push(rel);
                }
                continue;
            }
            let rel_s = rel.to_string_lossy().replace('\\', "/");
            if !rel_s.ends_with(".sql") {
                continue;
            }
            let conn = mapping.get(&rel_s).and_then(|u| by_uuid.get(u.as_str())).map(|n| n.to_string());
            out.push(QueryFile {
                name: rel_s.strip_prefix("queries/").unwrap_or(&rel_s).to_string(),
                rel: rel_s,
                conn,
                mtime: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            });
        }
    }
    out.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| a.rel.cmp(&b.rel)));
    out
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

// ─── 자동 저장(R45.2) ───

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SaveDecision {
    /// 파일이 이미 편집기 글과 같다
    Skip,
    Write,
    /// 밖에서 바뀌었거나 지워졌다. 자동 저장을 멈춘다(⌘S 는 덮어쓴다)
    Stop(String),
}

/// 쓰기 전 판단이다. disk 는 지금 파일 내용(없으면 None), last 는 마지막으로 읽거나 쓴 내용이다.
/// 수정 시각이 아니라 내용으로 가른다.
pub fn before_autosave(disk: Option<&str>, last: &str, editor: &str) -> SaveDecision {
    match disk {
        Some(d) if d == editor => SaveDecision::Skip,
        Some(d) if d == last => SaveDecision::Write,
        Some(_) => SaveDecision::Stop("파일이 밖에서 바뀌어 자동 저장 멈춤 (⌘S 로 덮어쓰기)".into()),
        None => SaveDecision::Stop("파일이 지워져 자동 저장 멈춤 (⌘S 로 다시 만들기)".into()),
    }
}

/// 파일 없는 탭의 첫 실행 때 저장할 상대 경로를 짓고 쓴 뒤(R45.1) 매핑에 붙인다. 실제 경로가 프로젝트 밖이면 쓰지 않고,
/// 폴더는 그 확인 뒤에만 만든다(idb-core 가 TUI 와 같이 한다). 매핑만 실패하면 저장은 하고 그 까닭을 돌려준다.
pub fn save_new_query(project: &Path, group: &str, conn: &str, uuid: &str, text: &str) -> Result<(String, Option<String>), String> {
    let rel = idb_core::mapping::unique_query_path(project, group, conn, chrono::Local::now()).map_err(|e| e.to_string())?;
    write_rel(project, &rel, text)?;
    let note = if uuid.is_empty() {
        None
    } else {
        idb_core::mapping::add_mapping_checked(project, &rel, uuid)
            .err()
            .map(|e| format!("{rel} 는 저장했지만 매핑 실패: {e}"))
    };
    Ok((rel, note))
}

const OUTSIDE: &str = "파일의 실제 경로가 프로젝트 밖이라 쓰지 않는다";

fn write_rel(project: &Path, rel: &str, text: &str) -> Result<(), String> {
    idb_core::mapping::write_project_file(project, rel, text).map_err(|e| match e {
        idb_core::mapping::WriteError::Outside(_) => OUTSIDE.to_string(),
        idb_core::mapping::WriteError::Io(e) => e,
    })
}

/// 탭의 파일을 쓴다. 프로젝트 안 파일은 실제 경로를 쓰기 바로 전에 확인한다(R45.2). 사용자가 ⌘O 로 고른 프로젝트 밖
/// 파일은 그 파일 그대로 쓴다. 어느 쪽이든 원자적으로 쓰고 원래 권한을 지킨다(R5.2, 새 파일은 0644).
pub fn write_query(project: Option<&Path>, path: &Path, text: &str) -> Result<(), String> {
    if let Some(dir) = project
        && let Ok(rel) = path.strip_prefix(dir)
    {
        return write_rel(dir, &rel.to_string_lossy(), text);
    }
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

    fn touch(p: &Path, rel: &str, body: &str, age_secs: u64) {
        let f = p.join(rel);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(&f, body).unwrap();
        let t = SystemTime::now() - std::time::Duration::from_secs(age_secs);
        std::fs::File::options().write(true).open(&f).unwrap().set_modified(t).unwrap();
    }

    #[test]
    fn 쿼리_파일은_최근_순이고_매핑으로_접속을_잇는다() {
        let d = tempfile::tempdir().unwrap();
        touch(d.path(), "queries/app/old.sql", "select 1", 100);
        touch(d.path(), "queries/app/new.sql", "select 2", 1);
        touch(d.path(), "queries/loose.sql", "select 3", 50);
        touch(d.path(), "queries/notes.txt", "x", 1);
        let mapping = HashMap::from([
            ("queries/app/old.sql".to_string(), "u1".to_string()),
            ("queries/app/new.sql".to_string(), "u1".to_string()),
            ("queries/loose.sql".to_string(), "gone".to_string()),
        ]);
        let conns = vec![conn("app-stg", "u1")];
        let files = scan(d.path(), &mapping, &conns);
        let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["app/new.sql", "loose.sql", "app/old.sql"]);
        assert_eq!(files[0].conn.as_deref(), Some("app-stg"));
        assert_eq!(files[1].conn, None, "없는 uuid 는 접속 없음");
        let g = group(&files, &conns);
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].0.as_deref(), Some("app-stg"));
        assert_eq!(g[0].1.len(), 2);
        assert_eq!(g[1].0, None);
    }

    #[test]
    fn 프로젝트_밖을_가리키는_링크는_뺀다() {
        let d = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.sql"), "x").unwrap();
        std::fs::create_dir_all(d.path().join("queries")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret.sql"), d.path().join("queries/link.sql")).unwrap();
        touch(d.path(), "queries/in.sql", "y", 1);
        std::os::unix::fs::symlink(d.path().join("queries/in.sql"), d.path().join("queries/inner-link.sql")).unwrap();
        let names: Vec<String> = scan(d.path(), &HashMap::new(), &[]).into_iter().map(|f| f.name).collect();
        assert!(names.contains(&"in.sql".to_string()));
        assert!(names.contains(&"inner-link.sql".to_string()));
        assert!(!names.contains(&"link.sql".to_string()));
    }

    #[test]
    fn 프로젝트_안_파일이_밖을_가리키면_덮지_않는다() {
        let d = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("queries")).unwrap();
        std::fs::write(out.path().join("t.sql"), "outside").unwrap();
        std::os::unix::fs::symlink(out.path().join("t.sql"), d.path().join("queries/a.sql")).unwrap();
        let err = write_query(Some(d.path()), &d.path().join("queries/a.sql"), "new").unwrap_err();
        assert!(err.contains("밖"), "{err}");
        assert_eq!(std::fs::read_to_string(out.path().join("t.sql")).unwrap(), "outside");
    }

    #[test]
    fn 저장은_0600_파일을_넓히지_않는다() {
        use std::os::unix::fs::PermissionsExt as _;
        let d = tempfile::tempdir().unwrap();
        touch(d.path(), "queries/a.sql", "x", 1);
        let p = d.path().join("queries/a.sql");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        write_query(Some(d.path()), &p, "y").unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        // 프로젝트 밖(⌘O) 파일도 같다
        let out = tempfile::tempdir().unwrap();
        let o = out.path().join("b.sql");
        std::fs::write(&o, "x").unwrap();
        std::fs::set_permissions(&o, std::fs::Permissions::from_mode(0o600)).unwrap();
        write_query(Some(d.path()), &o, "z").unwrap();
        assert_eq!(std::fs::metadata(&o).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_to_string(&o).unwrap(), "z");
    }

    #[test]
    fn idea_가_밖을_가리키면_쿼리는_저장하되_매핑은_고치지_않는다() {
        let d = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(out.path(), d.path().join(".idea")).unwrap();
        let (rel, note) = save_new_query(d.path(), "app", "app-stg", "u1", "select 1").unwrap();
        assert!(d.path().join(&rel).exists());
        assert!(note.is_some_and(|n| n.contains("매핑 실패")));
        assert!(!out.path().join("data_source_mapping.xml").exists());
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

    #[test]
    fn 새_쿼리는_그룹_폴더에_저장하고_매핑에_붙인다() {
        let d = tempfile::tempdir().unwrap();
        let (rel, note) = save_new_query(d.path(), "app", "app-stg", "u1", "select 1").unwrap();
        assert_eq!(note, None);
        assert!(rel.starts_with("queries/app/app-stg-") && rel.ends_with(".sql"), "{rel}");
        assert_eq!(std::fs::read_to_string(d.path().join(&rel)).unwrap(), "select 1");
        let xml = std::fs::read_to_string(d.path().join(".idea/data_source_mapping.xml")).unwrap();
        assert!(xml.contains(&rel) && xml.contains("u1"), "{xml}");
        let (rel2, _) = save_new_query(d.path(), "", "app-stg", "", "select 2").unwrap();
        assert!(rel2.starts_with("queries/etc/"), "{rel2}");
    }
}
