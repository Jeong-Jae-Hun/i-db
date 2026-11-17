//! 새 쿼리 파일 이름(R45.1)·충돌 사본 이름(R45.3)과 `.idea/data_source_mapping.xml` 항목 더하기. Go `internal/db/mapping.go` 다.
//! 매핑은 DataGrip 이 다시 쓸 때 diff 가 커지지 않게 XML 을 다시 찍지 않고 글자로 끼운다.
//! 단, Go 는 XML 토큰 자리로 끼우고 여기서는 roxmltree 의 노드 범위로 끼운다. 같은 입력에 같은 바이트를 내는 경우를 시험으로 고정했다.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local};

use crate::atomicfile;

const COMPONENT: &str = "DataSourcePerFileMappings";
const URL_PREFIX: &str = "file://$PROJECT_DIR$/";
const MAX_SUFFIX: usize = 1000;
/// 새로 짓는 파일·폴더 이름 한 칸의 상한(흔한 파일 시스템 한계 255바이트보다 작게)
const MAX_NAME_BYTES: usize = 200;
const TIME_LAYOUT: &str = "%Y%m%d-%H%M%S";

/// s 를 n 바이트 안으로 글자 경계에서 자른다.
pub fn cut_bytes(s: &str, mut n: usize) -> &str {
    if s.len() <= n {
        return s;
    }
    while n > 0 && !s.is_char_boundary(n) {
        n -= 1;
    }
    &s[..n]
}

/// 글자·숫자·-·_ 밖의 글자 덩이를 - 하나로 바꾸고 앞뒤 - 를 지운다(한글 그대로).
pub fn path_safe(s: &str) -> String {
    let mut b = String::new();
    let mut dash = false;
    for r in s.chars() {
        if r.is_alphanumeric() || r == '-' || r == '_' {
            b.push(r);
            dash = false;
        } else if !dash {
            b.push('-');
            dash = true;
        }
    }
    b.trim_matches('-').to_string()
}

/// 새 쿼리를 저장할 프로젝트 상대 경로다: queries/<그룹>/<접속>-<YYYYMMDD-HHmmss>.sql, 있으면 -2, -3 ….
pub fn unique_query_path(project: &Path, group: &str, conn: &str, now: DateTime<Local>) -> io::Result<String> {
    let g = path_safe(group);
    let g = cut_bytes(&g, MAX_NAME_BYTES);
    let g = if g.is_empty() { "etc" } else { g };
    let c = path_safe(conn);
    let c = if c.is_empty() { "query".to_string() } else { c };
    unique_path(project, &format!("queries/{g}"), &c, &format!("-{}", now.format(TIME_LAYOUT)))
}

/// rel 옆 충돌 사본 경로 <이름>-conflict-<YYYYMMDD-HHmmss>.sql 이다.
pub fn conflict_path(project: &Path, rel: &str, now: DateTime<Local>) -> io::Result<String> {
    let (dir, base) = rel.rsplit_once('/').unwrap_or((".", rel));
    unique_path(
        project,
        dir,
        base.trim_end_matches(".sql"),
        &format!("-conflict-{}", now.format(TIME_LAYOUT)),
    )
}

fn unique_path(project: &Path, dir: &str, name: &str, suffix: &str) -> io::Result<String> {
    let tail = format!("-{MAX_SUFFIX}.sql");
    let keep = MAX_NAME_BYTES.saturating_sub(suffix.len() + tail.len());
    let base = format!("{dir}/{}{suffix}", cut_bytes(name, keep));
    for n in 1..=MAX_SUFFIX {
        let rel = if n == 1 { format!("{base}.sql") } else { format!("{base}-{n}.sql") };
        match fs::symlink_metadata(project.join(&rel)) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(rel),
            Err(e) => return Err(e),
            Ok(_) => {}
        }
    }
    Err(io::Error::other(format!("too many query files named {base}")))
}

fn xml_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&#34;")
        .replace('\'', "&#39;")
        .replace('\t', "&#x9;")
        .replace('\n', "&#xA;")
        .replace('\r', "&#xD;")
}

/// `.idea/data_source_mapping.xml` 에 rel → uuid 를 붙인다. 같은 url 이 있으면 그 항목만 바꾸고, 없으면
/// 컴포넌트 닫는 줄 앞에 같은 들여쓰기로 더한다. 파일이 없으면 DataGrip 형식으로 만든다. 권한은 원본을 따른다.
pub fn add_mapping(project: &Path, rel: &str, uuid: &str) -> io::Result<()> {
    if rel.starts_with('/') || rel.split('/').any(|p| p == "..") {
        return Err(io::Error::other(format!("mapping path is not inside the project: {rel}")));
    }
    let file = project.join(".idea").join("data_source_mapping.xml");
    let (data, perm) = match fs::read(&file) {
        Ok(d) => (d, fs::metadata(&file)?.permissions().mode() & 0o777),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(file.parent().unwrap())?;
            (Vec::new(), 0o644)
        }
        Err(e) => return Err(e),
    };
    let text = String::from_utf8(data).map_err(|e| io::Error::other(e.to_string()))?;
    let out =
        insert_mapping(&text, &format!("{URL_PREFIX}{rel}"), uuid).map_err(|e| io::Error::other(format!("{}: {e}", file.display())))?;
    if out == text {
        return Ok(());
    }
    atomicfile::write(&file, ".data_source_mapping-*.xml", out.as_bytes(), perm)
}

/// at 이 든 줄의 앞 공백이다. at 앞에 공백 아닌 글이 있으면 false 다.
fn line_indent(data: &str, at: usize) -> (String, bool) {
    let ls = data[..at].rfind('\n').map_or(0, |i| i + 1);
    let head = &data[ls..at];
    let ind = &head[..head.len() - head.trim_start_matches([' ', '\t']).len()];
    (ind.to_string(), ind.len() == head.len())
}

fn insert_line(data: &str, at: usize, line: &str, nl: &str) -> String {
    let ls = data[..at].rfind('\n').map_or(0, |i| i + 1);
    if data[ls..at].trim_start_matches([' ', '\t']).is_empty() {
        return format!("{}{line}{nl}{}", &data[..ls], &data[ls..]);
    }
    let (ind, _) = line_indent(data, ls);
    format!("{}{nl}{line}{nl}{ind}{}", &data[..at], &data[at..])
}

pub fn insert_mapping(data: &str, url: &str, uuid: &str) -> Result<String, String> {
    let nl = if data.contains("\r\n") { "\r\n" } else { "\n" };
    let entry = format!("<file url=\"{}\" value=\"{}\" />", xml_attr(url), xml_attr(uuid));
    if data.trim().is_empty() {
        return Ok(format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>{nl}<project version=\"4\">{nl}  <component name=\"{COMPONENT}\">{nl}    {entry}{nl}  </component>{nl}</project>{nl}"
        ));
    }
    let doc = roxmltree::Document::parse(data).map_err(|e| e.to_string())?;
    let root = doc.root_element();
    if root.tag_name().name() != "project" {
        return Err(format!("root element is {}, not project", root.tag_name().name()));
    }
    let mut unit = "  ".to_string();
    if let Some(first) = root.children().find(|n| n.is_element()) {
        let (ind, ok) = line_indent(data, first.range().start);
        if ok {
            unit = ind;
        }
    }
    let comp = root
        .children()
        .find(|n| n.is_element() && n.tag_name().name() == "component" && n.attribute("name") == Some(COMPONENT));
    let Some(comp) = comp else {
        let r = root.range();
        let close = data[..r.end].rfind("</").ok_or("project element is empty and self-closing")?;
        if close < r.start {
            return Err("project element is empty and self-closing".into());
        }
        let block = format!("{unit}<component name=\"{COMPONENT}\">{nl}{unit}{unit}{entry}{nl}{unit}</component>");
        return Ok(insert_line(data, close, &block, nl));
    };
    let files: Vec<_> = comp
        .children()
        .filter(|n| n.is_element() && n.tag_name().name() == "file")
        .collect();
    let matches: Vec<_> = files.iter().filter(|f| f.attribute("url") == Some(url)).collect();
    if !matches.is_empty() {
        if matches.iter().all(|f| f.attribute("value") == Some(uuid)) {
            return Ok(data.to_string());
        }
        // 같은 url 이 여럿이면 프로젝트 읽기가 마지막 것을 쓰므로 모두 바꾼다
        let mut out = data.to_string();
        for f in matches.iter().rev() {
            let r = f.range();
            out.replace_range(r, &entry);
        }
        return Ok(out);
    }
    let cr = comp.range();
    let body = &data[cr.clone()];
    if body.trim_end().ends_with("/>") && !body.contains("</") {
        let (ind, _) = line_indent(data, cr.start);
        let open = body.trim_end().trim_end_matches("/>").trim_end();
        let repl = format!("{open}>{nl}{ind}{unit}{entry}{nl}{ind}</component>");
        let mut out = data.to_string();
        out.replace_range(cr, &repl);
        return Ok(out);
    }
    let close = cr.start + body.rfind("</").unwrap();
    let ind = match files.last() {
        Some(f) => {
            let (ind, ok) = line_indent(data, f.range().start);
            if ok { ind } else { line_indent(data, cr.start).0 + &unit }
        }
        None => line_indent(data, cr.start).0 + &unit,
    };
    Ok(insert_line(data, close, &format!("{ind}{entry}"), nl))
}

/// rel 의 실제 경로다. 심볼릭 링크를 따라간 경로가 프로젝트 밖이면 오류다(R45.2).
pub fn real_path(dir: &Path, rel: &str) -> Result<PathBuf, String> {
    let root = fs::canonicalize(dir).map_err(|e| e.to_string())?;
    let mut p = dir.join(rel);
    let mut rest = PathBuf::new();
    let real = loop {
        match fs::canonicalize(&p) {
            // 빈 rest 를 join 하면 끝에 / 가 붙어 있는 파일이 폴더로 읽힌다(쓰기가 ENOTDIR 로 실패했다)
            Ok(r) if rest.as_os_str().is_empty() => break r,
            Ok(r) => break r.join(&rest),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let Some(name) = p.file_name().map(PathBuf::from) else {
                    return Err(e.to_string());
                };
                // 빈 PathBuf 를 join 하면 끝에 / 가 붙어 폴더로 읽힌다
                rest = if rest.as_os_str().is_empty() { name } else { name.join(rest) };
                if !p.pop() {
                    return Err(e.to_string());
                }
            }
            Err(e) => return Err(e.to_string()),
        }
    };
    if !real.starts_with(&root) {
        return Err(format!("real path is outside the project: {rel}"));
    }
    Ok(real)
}

#[derive(Debug)]
pub enum WriteError {
    /// 실제 경로가 프로젝트 밖이다(R45.2)
    Outside(String),
    Io(String),
}

/// 프로젝트 안 rel 을 원자적으로 쓴다(R5.2·R45.2). 실제 경로(심볼릭 링크를 따라간 경로)가 프로젝트 밖이면 쓰지 않고,
/// 폴더는 그 확인 뒤에만 만든다. 권한은 원본을 따르고 새 파일은 0644 다.
pub fn write_project_file(dir: &Path, rel: &str, text: &str) -> Result<(), WriteError> {
    let p = real_path(dir, rel).map_err(WriteError::Outside)?;
    let perm = fs::metadata(&p).map(|m| m.permissions().mode() & 0o777).unwrap_or(0o644);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).map_err(|e| WriteError::Io(e.to_string()))?;
        // 만든 뒤에도 다시 본다(그사이 바뀐 링크를 따라 밖에 쓰지 않게)
        real_path(dir, rel).map_err(WriteError::Outside)?;
    }
    atomicfile::write(&p, ".db-*", text.as_bytes(), perm).map_err(|e| WriteError::Io(e.to_string()))
}

/// 매핑 파일의 실제 경로를 확인한 뒤 붙인다. `.idea` 가 밖을 가리키면 고치지 않는다(R45.2).
pub fn add_mapping_checked(project: &Path, rel: &str, uuid: &str) -> Result<(), String> {
    real_path(project, ".idea/data_source_mapping.xml")?;
    add_mapping(project, rel, uuid).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 실제_경로가_프로젝트_밖이면_쓰지_않는다() {
        let d = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join("queries")).unwrap();
        fs::write(out.path().join("x.sql"), "outside").unwrap();
        std::os::unix::fs::symlink(out.path().join("x.sql"), d.path().join("queries/a.sql")).unwrap();
        assert!(matches!(
            write_project_file(d.path(), "queries/a.sql", "new"),
            Err(WriteError::Outside(_))
        ));
        assert_eq!(fs::read_to_string(out.path().join("x.sql")).unwrap(), "outside");
        // 밖을 가리키는 폴더 링크 아래 새 파일도, 폴더도 만들지 않는다
        std::os::unix::fs::symlink(out.path(), d.path().join("queries/linked")).unwrap();
        assert!(matches!(
            write_project_file(d.path(), "queries/linked/sub/b.sql", "x"),
            Err(WriteError::Outside(_))
        ));
        assert!(!out.path().join("sub").exists());
    }

    #[test]
    fn 쓰기는_원래_권한을_지키고_새_파일은_0644_다() {
        let d = tempfile::tempdir().unwrap();
        write_project_file(d.path(), "queries/app/new.sql", "a").unwrap();
        let p = d.path().join("queries/app/new.sql");
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o644);
        fs::set_permissions(&p, fs::Permissions::from_mode(0o600)).unwrap();
        write_project_file(d.path(), "queries/app/new.sql", "b").unwrap();
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::read_to_string(&p).unwrap(), "b");
    }

    #[test]
    fn idea_가_밖을_가리키면_매핑을_고치지_않는다() {
        let d = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(out.path(), d.path().join(".idea")).unwrap();
        assert!(add_mapping_checked(d.path(), "queries/a.sql", "u1").is_err());
        assert!(!out.path().join("data_source_mapping.xml").exists());
        let d2 = tempfile::tempdir().unwrap();
        add_mapping_checked(d2.path(), "queries/a.sql", "u1").unwrap();
        assert!(
            fs::read_to_string(d2.path().join(".idea/data_source_mapping.xml"))
                .unwrap()
                .contains("u1")
        );
    }

    #[test]
    fn 없는_파일은_DataGrip_형식으로_만든다() {
        let out = insert_mapping("", "file://$PROJECT_DIR$/queries/a.sql", "u1").unwrap();
        assert_eq!(
            out,
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<project version=\"4\">\n  <component name=\"DataSourcePerFileMappings\">\n    <file url=\"file://$PROJECT_DIR$/queries/a.sql\" value=\"u1\" />\n  </component>\n</project>\n"
        );
    }

    #[test]
    fn 기존_항목_뒤에_같은_들여쓰기로_더하고_같은_url_은_값만_바꾼다() {
        let base = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<project version=\"4\">\n  <component name=\"DataSourcePerFileMappings\">\n    <file url=\"file://$PROJECT_DIR$/q/a.sql\" value=\"u1\" />\n  </component>\n</project>\n";
        let out = insert_mapping(base, "file://$PROJECT_DIR$/q/b.sql", "u2").unwrap();
        assert!(
            out.contains("value=\"u1\" />\n    <file url=\"file://$PROJECT_DIR$/q/b.sql\" value=\"u2\" />\n  </component>"),
            "{out}"
        );
        assert_eq!(insert_mapping(base, "file://$PROJECT_DIR$/q/a.sql", "u1").unwrap(), base);
        let changed = insert_mapping(base, "file://$PROJECT_DIR$/q/a.sql", "u9").unwrap();
        assert_eq!(changed, base.replace("u1", "u9"));
    }

    #[test]
    fn 스스로_닫힌_컴포넌트와_없는_컴포넌트도_채운다() {
        let selfc = "<project version=\"4\">\n  <component name=\"DataSourcePerFileMappings\" />\n</project>\n";
        let out = insert_mapping(selfc, "u", "v").unwrap();
        assert_eq!(
            out,
            "<project version=\"4\">\n  <component name=\"DataSourcePerFileMappings\">\n    <file url=\"u\" value=\"v\" />\n  </component>\n</project>\n"
        );
        let none = "<project version=\"4\">\n  <component name=\"Other\" />\n</project>\n";
        let out = insert_mapping(none, "u", "v").unwrap();
        assert!(
            out.ends_with(
                "  <component name=\"DataSourcePerFileMappings\">\n    <file url=\"u\" value=\"v\" />\n  </component>\n</project>\n"
            ),
            "{out}"
        );
    }

    #[test]
    fn 새_쿼리_이름은_글자만_남기고_있으면_번호를_붙인다() {
        let dir = tempfile::tempdir().unwrap();
        let now = chrono::Local::now();
        let ts = now.format(TIME_LAYOUT).to_string();
        let rel = unique_query_path(dir.path(), "", "app prod/한글", now).unwrap();
        assert_eq!(rel, format!("queries/etc/app-prod-한글-{ts}.sql"));
        fs::create_dir_all(dir.path().join("queries/etc")).unwrap();
        fs::write(dir.path().join(&rel), "x").unwrap();
        assert_eq!(
            unique_query_path(dir.path(), "", "app prod/한글", now).unwrap(),
            format!("queries/etc/app-prod-한글-{ts}-2.sql")
        );
        assert_eq!(
            conflict_path(dir.path(), "queries/a/b.sql", now).unwrap(),
            format!("queries/a/b-conflict-{ts}.sql")
        );
        assert!(add_mapping(dir.path(), "../x.sql", "u").is_err());
    }
}
