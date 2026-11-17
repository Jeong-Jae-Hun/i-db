//! db 전용 접속 쓰기(R59)와 트리 폴더 쓰기(R66.2). Go `internal/db/connstore.go`·`folders.go` 의 쓰기 쪽이다.
//! DataGrip .idea 는 고치지 않고 `.dbtui/connections.json` 에 접속을, `.env` 에 비밀번호를 둔다.
//! 파일 잠금은 하지 않는다(Go 판과 같은 한계). 화면과 CLI 가 같은 프로젝트를 동시에 고치면 나중 쓰기가 앞 쓰기를 덮을 수 있다.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::atomicfile;
use crate::project::{self, Conn, ConnEntry, Kind, SOURCE_DB, group_of, load_project, password_key, valid_folder};

pub const DEFAULT_PG_PORT: u16 = 5432;
pub const DEFAULT_REDIS_PORT: u16 = 6379;

/// 쓰기 오류다. env_not_updated 면 connections.json 은 썼지만 .env 비밀번호 줄을 바꾸지 못했다(접속은 저장됨).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreError {
    pub msg: String,
    pub env_not_updated: bool,
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

impl From<String> for StoreError {
    fn from(msg: String) -> StoreError {
        StoreError {
            msg,
            env_not_updated: false,
        }
    }
}

impl From<io::Error> for StoreError {
    fn from(e: io::Error) -> StoreError {
        StoreError::from(e.to_string())
    }
}

/// UpdateConn 이 connections.json 은 썼지만 .env 를 고치지 못했을 때 글의 머리다.
pub const ENV_NOT_UPDATED: &str = "connection saved but .env was not updated";

/// 종류의 기본 포트다. 읽기 전용 저장소는 포트를 URL 에 두어 0 이다.
pub fn default_port(k: Kind) -> u16 {
    match k {
        k if k.is_source() => 0,
        Kind::Redis => DEFAULT_REDIS_PORT,
        _ => DEFAULT_PG_PORT,
    }
}

/// 종류 k 가 받는 auth 값이다. 첫 값이 기본값이고 "" 는 인증 없음이다.
pub fn auth_options(k: Kind) -> &'static [&'static str] {
    k.auth_options()
}

pub fn valid_conn_name(name: &str) -> bool {
    project::valid_conn_name(name)
}

/// 저장할 접속 값을 검사한다(Go `ValidateConn`).
pub fn validate_conn(c: &Conn) -> Result<(), String> {
    project::validate_conn(c, c.kind_str(), c.port as i64)
}

fn conns_path(dir: &Path) -> PathBuf {
    dir.join(".dbtui").join("connections.json")
}

fn read_entries(dir: &Path) -> Result<Vec<ConnEntry>, String> {
    let path = conns_path(dir);
    match fs::read(&path) {
        Ok(b) => serde_json::from_slice::<Option<Vec<ConnEntry>>>(&b)
            .map(Option::unwrap_or_default)
            .map_err(|e| format!("read {}: {}", path.display(), crate::gojson::json_error(&e))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("read {}: {e}", path.display())),
    }
}

/// Go json.Marshal(HTML 이스케이프 켬) 의 문자열이다.
fn json_str(out: &mut String, s: &str) {
    let mut tmp = String::new();
    crate::gojson::write_str(&mut tmp, s);
    out.push_str(&tmp.replace('<', "\\u003c").replace('>', "\\u003e").replace('&', "\\u0026"));
}

/// Go `json.MarshalIndent(es, "", "  ")` 와 같은 글이다. 빈 칸은 omitempty 칸만 뺀다.
fn entries_json(es: &[ConnEntry]) -> String {
    if es.is_empty() {
        return "[]".into();
    }
    let mut out = String::from("[\n");
    for (i, e) in es.iter().enumerate() {
        out.push_str("  {\n");
        let mut fields: Vec<(&str, String)> = Vec::new();
        let mut s = |k: &'static str, v: &str, omit: bool| {
            if !(omit && v.is_empty()) {
                let mut q = String::new();
                json_str(&mut q, v);
                fields.push((k, q));
            }
        };
        s("uuid", &e.uuid, false);
        s("name", &e.name, false);
        s("group", &e.group, false);
        s("kind", &e.kind, false);
        s("host", &e.host, false);
        fields.push(("port", e.port.to_string()));
        let mut s = |k: &'static str, v: &str, omit: bool| {
            if !(omit && v.is_empty()) {
                let mut q = String::new();
                json_str(&mut q, v);
                fields.push((k, q));
            }
        };
        s("db", &e.db, false);
        s("user", &e.user, false);
        s("url", &e.url, true);
        s("auth", &e.auth, true);
        s("region", &e.region, true);
        s("profile", &e.profile, true);
        s("sslmode", &e.sslmode, true);
        s("workgroup", &e.workgroup, true);
        s("output", &e.output, true);
        let n = fields.len();
        for (j, (k, v)) in fields.into_iter().enumerate() {
            out.push_str(&format!("    \"{k}\": {v}"));
            out.push_str(if j + 1 < n { ",\n" } else { "\n" });
        }
        out.push_str(if i + 1 < es.len() { "  },\n" } else { "  }\n" });
    }
    out.push(']');
    out
}

fn write_entries(dir: &Path, es: &[ConnEntry]) -> io::Result<()> {
    let mut b = entries_json(es).into_bytes();
    b.push(b'\n');
    let path = conns_path(dir);
    fs::create_dir_all(path.parent().unwrap_or(dir))?;
    let (target, perm) = write_target(&path, 0o644)?;
    atomicfile::write(&target, ".connections-*.json", &b, perm)
}

/// path 를 바꿔 끼워 쓸 실제 경로와 권한이다. 심볼릭 링크면 가리키는 파일에 쓴다(.env 를 다른 곳에 두고 링크로 쓰는 경우).
/// 실제 경로가 폴더거나 가리키는 파일이 없는 링크면 거절한다. 없으면 path 에 new_perm 으로 만들고, 있으면 권한을 그대로 둔다.
fn write_target(path: &Path, new_perm: u32) -> io::Result<(PathBuf, u32)> {
    use std::os::unix::fs::PermissionsExt;
    let real = match fs::canonicalize(path) {
        Ok(r) => r,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
                return Err(io::Error::other(format!("{} is a symlink to a missing file", path.display())));
            }
            return Ok((path.to_path_buf(), new_perm));
        }
        Err(e) => return Err(e),
    };
    let st = fs::metadata(&real)?;
    if st.is_dir() {
        return Err(io::Error::other(format!("{} is a directory", real.display())));
    }
    Ok((real, st.permissions().mode() & 0o777))
}

fn entry_of(c: &Conn) -> ConnEntry {
    ConnEntry {
        uuid: c.uuid.clone(),
        name: c.name.clone(),
        group: c.group.clone(),
        kind: c.kind_str().into(),
        host: c.host.clone(),
        port: c.port as i64,
        db: c.db.clone(),
        user: c.user.clone(),
        url: c.url.clone(),
        auth: c.auth.clone(),
        region: c.region.clone(),
        profile: c.profile.clone(),
        sslmode: c.sslmode.clone(),
        workgroup: c.workgroup.clone(),
        output: c.output.clone(),
    }
}

/// 파일 줄의 접속 값이다(겹침 검사용). 종류를 모르는 줄은 kind 가 None 이다.
fn conn_of(e: &ConnEntry) -> Conn {
    Conn {
        uuid: e.uuid.clone(),
        name: e.name.clone(),
        group: e.group.clone(),
        kind: Kind::parse(&e.kind),
        host: e.host.clone(),
        port: u16::try_from(e.port).unwrap_or(0),
        db: e.db.clone(),
        user: e.user.clone(),
        url: e.url.clone(),
        auth: e.auth.clone(),
        region: e.region.clone(),
        profile: e.profile.clone(),
        sslmode: e.sslmode.clone(),
        workgroup: e.workgroup.clone(),
        output: e.output.clone(),
        source: SOURCE_DB.into(),
        ..Default::default()
    }
}

/// 새 이름이 다른 접속(.idea 포함)의 이름·.env 비밀번호 키와 겹치지 않는지 본다. self_uuid 는 고치는 접속의 uuid 다.
fn check_name(conns: &[Conn], c: &Conn, self_uuid: &str) -> Result<(), String> {
    for o in conns {
        if o.uuid == self_uuid {
            continue;
        }
        if o.name == c.name || o.uuid == c.uuid {
            return Err(format!("connection already exists: {}", c.name));
        }
        let k = password_key(o.kind, &o.name);
        if !k.is_empty() && k == password_key(c.kind, &c.name) {
            return Err(format!("{} uses the same .env key as {}", c.name, o.name));
        }
    }
    Ok(())
}

/// Go `url.QueryEscape`.
pub fn query_escape(s: &str) -> String {
    let mut out = String::new();
    for &c in s.as_bytes() {
        match c {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(c as char),
            b' ' => out.push('+'),
            c => out.push_str(&format!("%{c:02X}")),
        }
    }
    out
}

/// 무작위(v4) uuid 다.
pub fn new_uuid() -> String {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&atomicfile::random_u64().to_le_bytes());
    b[8..].copy_from_slice(&atomicfile::random_u64().to_le_bytes());
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = |r: std::ops::Range<usize>| b[r].iter().map(|x| format!("{x:02x}")).collect::<String>();
    format!("{}-{}-{}-{}-{}", h(0..4), h(4..6), h(6..8), h(8..10), h(10..16))
}

fn all_conns(dir: &Path, es: &[ConnEntry]) -> Result<Vec<Conn>, String> {
    let p = load_project(dir).map_err(|e| e.to_string())?;
    // 겹쳐서 건너뛴 db 전용 접속도 파일에는 있으니 같이 본다
    let mut all = p.conns;
    all.extend(es.iter().map(conn_of));
    Ok(all)
}

/// db 전용 접속을 connections.json 끝에 더하고 비밀번호가 있으면 .env 에 쓴다. uuid 가 비면 만든다.
/// .env 를 먼저 쓴다. connections.json 쓰기가 실패해도 남는 것은 쓰이지 않는 비밀번호 한 줄이다.
pub fn add_conn(dir: &Path, mut c: Conn) -> Result<Conn, StoreError> {
    validate_conn(&c)?;
    let mut es = read_entries(dir)?;
    let all = all_conns(dir, &es)?;
    if c.uuid.is_empty() {
        c.uuid = new_uuid();
    }
    check_name(&all, &c, "")?;
    let key = password_key(c.kind, &c.name);
    if !c.password.is_empty() && !key.is_empty() {
        set_env(dir, &[&key], &format!("{key}={}", query_escape(&c.password)))?;
    }
    c.source = SOURCE_DB.into();
    es.push(entry_of(&c));
    write_entries(dir, &es)?;
    Ok(c)
}

/// db 전용 접속 old_name 을 c 로 바꾼다. uuid 는 그대로 둔다. c.password 가 비면 비밀번호를 그대로 두고,
/// 이름·종류가 바뀌어 .env 키가 달라지면 값을 새 키로 옮긴다(R59.3).
pub fn update_conn(dir: &Path, old_name: &str, mut c: Conn) -> Result<Conn, StoreError> {
    validate_conn(&c)?;
    let mut es = read_entries(dir)?;
    let all = all_conns(dir, &es)?;
    let Some(i) = es.iter().position(|e| e.name == old_name) else {
        return Err(db_only_err(old_name).into());
    };
    let old = conn_of(&es[i]);
    c.uuid = old.uuid.clone();
    c.source = SOURCE_DB.into();
    check_name(&all, &c, &old.uuid)?;
    // connections.json 을 먼저 쓴다. .env 쓰기가 실패해도 옛 키 줄은 지우지 않아 비밀번호를 잃지 않는다
    es[i] = entry_of(&c);
    write_entries(dir, &es)?;
    let (old_key, new_key) = (password_key(old.kind, &old.name), password_key(c.kind, &c.name));
    let r = if !c.password.is_empty() && !new_key.is_empty() {
        set_env(dir, &[&old_key, &new_key], &format!("{new_key}={}", query_escape(&c.password)))
    } else if old_key != new_key && old_key.is_empty() {
        // 비밀값이 없던 종류(DynamoDB)에서 바꾸면 옮길 값이 없다
        Ok(())
    } else if old_key != new_key {
        // 비밀번호를 그대로 두고 키만 옮긴다. 값은 인코딩된 원문 그대로 옮긴다. 비밀값이 없는 종류로 바꾸면 옛 줄만 지운다
        env_value(dir, &old_key).and_then(|(raw, found)| {
            let line = if found && !new_key.is_empty() {
                format!("{new_key}={raw}")
            } else {
                String::new()
            };
            set_env(dir, &[&old_key, &new_key], &line)
        })
    } else {
        Ok(())
    };
    if let Err(e) = r {
        return Err(StoreError {
            msg: format!("{ENV_NOT_UPDATED}: move {old_key} to {new_key} by hand: {e}"),
            env_not_updated: true,
        });
    }
    Ok(c)
}

/// db 전용 접속을 connections.json 에서 빼고 .env 비밀번호 줄을 지운다.
pub fn remove_conn(dir: &Path, name: &str) -> Result<(), StoreError> {
    let mut es = read_entries(dir)?;
    let Some(i) = es.iter().position(|e| e.name == name) else {
        return Err(db_only_err(name).into());
    };
    let key = password_key(Kind::parse(&es[i].kind), &es[i].name);
    es.remove(i);
    write_entries(dir, &es)?;
    if key.is_empty() {
        return Ok(());
    }
    Ok(set_env(dir, &[&key], "")?)
}

fn db_only_err(name: &str) -> String {
    format!("no such db connection: {name}")
}

// ─── .env ───

/// .env 한 줄의 키다. KEY=VALUE 가 아니거나 주석이면 빈 글이다(load_project 와 같은 규칙).
fn env_key_of(line: &str) -> &str {
    let line = line.trim();
    match line.split_once('=') {
        Some((k, _)) if !line.starts_with('#') => k.trim(),
        _ => "",
    }
}

/// .env 에서 key 의 원문 값(인코딩된 채)이다. 같은 키가 여러 줄이면 마지막 것이다.
fn env_value(dir: &Path, key: &str) -> io::Result<(String, bool)> {
    let b = match fs::read(dir.join(".env")) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((String::new(), false)),
        Err(e) => return Err(e),
    };
    let text = String::from_utf8_lossy(&b);
    let mut out = (String::new(), false);
    for line in text.split('\n') {
        if env_key_of(line) == key {
            let v = line.split_once('=').map_or("", |(_, v)| v);
            out = (v.trim().trim_matches(['"', '\'']).to_string(), true);
        }
    }
    Ok(out)
}

/// .env 에서 keys 의 줄을 모두 빼고, line 이 있으면 그 첫 줄 자리(없으면 끝)에 둔다. 다른 줄·주석·순서는 그대로 둔다.
/// 파일이 없으면 0600 으로 만들고, 있으면 권한을 그대로 둔다(R59.2). 심볼릭 링크면 가리키는 파일을 고친다.
/// 파일이 CRLF 줄바꿈이면 새 줄도 CRLF 로 쓴다.
pub fn set_env(dir: &Path, keys: &[&str], line: &str) -> io::Result<()> {
    // 빈 키는 주석·빈 줄(env_key_of "")과 맞아 그 줄들을 지우게 되므로 뺀다
    let keys: Vec<&str> = keys.iter().copied().filter(|k| !k.is_empty()).collect();
    let (path, perm) = write_target(&dir.join(".env"), 0o600)?;
    let b = match fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e),
    };
    let text = String::from_utf8_lossy(&b);
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut line = line.to_string();
    let mut buf = String::new();
    if !text.is_empty() {
        for ln in text.strip_suffix('\n').unwrap_or(&text).split('\n') {
            if !keys.contains(&env_key_of(ln)) {
                buf.push_str(ln);
                buf.push('\n');
            } else if !line.is_empty() {
                buf.push_str(&line);
                buf.push_str(eol);
                line.clear();
            }
        }
    }
    if !line.is_empty() {
        buf.push_str(&line);
        buf.push_str(eol);
    }
    atomicfile::write(&path, ".env-*", buf.as_bytes(), perm)
}

// ─── URL ───

/// 접속 URL 을 읽지 못했을 때다. URL 에 비밀번호가 들 수 있어 원문을 싣지 않는다.
pub const ERR_CONN_URL: &str = "invalid connection url (postgres://user:pw@host:5432/db, redis://:pw@host:6379/0)";

fn decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned()
}

/// postgres(ql)://·redis://·rediss:// URL 을 접속 값과 비밀번호로 나눈다. 포트가 없으면 종류의 기본 포트다.
pub fn parse_conn_url(raw: &str) -> Result<(Conn, String), String> {
    let bad = || ERR_CONN_URL.to_string();
    let u = url::Url::parse(raw.trim()).map_err(|_| bad())?;
    let host = u.host_str().unwrap_or("").trim_matches(['[', ']']).to_string();
    if host.is_empty() || u.cannot_be_a_base() {
        return Err(bad());
    }
    let kind = match u.scheme() {
        "postgres" | "postgresql" => Kind::Pg,
        "redis" | "rediss" => Kind::Redis,
        _ => return Err(bad()),
    };
    let mut c = Conn {
        kind: Some(kind),
        host,
        user: decode(u.username()),
        db: decode(u.path().strip_prefix('/').unwrap_or(u.path())),
        port: default_port(kind),
        ..Default::default()
    };
    if let Some(p) = u.port() {
        if p == 0 {
            return Err(bad());
        }
        c.port = p;
    }
    if kind == Kind::Redis && !c.db.is_empty() && project::go_atoi(&c.db).is_none() {
        return Err(bad());
    }
    if kind == Kind::Pg {
        c.sslmode = u
            .query_pairs()
            .find(|(k, _)| k == "sslmode")
            .map(|(_, v)| v.into_owned())
            .unwrap_or_default();
        if !c.sslmode.is_empty() && !matches!(c.sslmode.as_str(), "disable" | "prefer" | "require" | "verify-ca" | "verify-full") {
            return Err(bad());
        }
    }
    let pw = u.password().map(decode).unwrap_or_default();
    Ok((c, pw))
}

/// 접속 창·CLI 에 붙인 http(s) URL 을 사용자 정보를 뺀 URL·사용자·비밀번호로 나눈다.
pub fn parse_http_url(raw: &str) -> Result<(String, String, String), String> {
    let bad = || "invalid url (https://host[:port])".to_string();
    let mut u = url::Url::parse(raw.trim()).map_err(|_| bad())?;
    if !matches!(u.scheme(), "http" | "https") || u.host_str().is_none_or(str::is_empty) {
        return Err(bad());
    }
    let user = decode(u.username());
    let pw = u.password().map(decode).unwrap_or_default();
    let _ = u.set_username("");
    let _ = u.set_password(None);
    Ok((u.as_str().trim_end_matches('/').to_string(), user, pw))
}

// ─── 트리 폴더 (R66.2) ───

fn folders_path(dir: &Path) -> PathBuf {
    dir.join(".dbtui").join("folders.json")
}

/// folders.json 의 키다. 이름을 바꿔도 따라가게 uuid 를 쓰고, uuid 가 없을 때만 이름을 쓴다.
fn folder_key(c: &Conn) -> String {
    if c.uuid.is_empty() {
        format!("name:{}", c.name)
    } else {
        c.uuid.clone()
    }
}

/// 접속들을 folder 로 옮긴다. folder 가 비거나 접속의 원래 그룹과 같으면 옮긴 기록을 지워 그룹으로 되돌린다.
/// 깨진 파일은 덮어 다른 기록을 잃지 않게 쓰지 않는다.
pub fn set_folder(dir: &Path, conns: &[&Conn], folder: &str) -> Result<(), String> {
    let folder = folder.trim();
    if !folder.is_empty() {
        valid_folder(folder)?;
    }
    let path = folders_path(dir);
    let mut m: BTreeMap<String, String> = match fs::read(&path) {
        Ok(b) => serde_json::from_slice::<Option<BTreeMap<String, String>>>(&b)
            .map(Option::unwrap_or_default)
            .map_err(|e| format!("read {}: {}", path.display(), crate::gojson::json_error(&e)))?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    let old = m.clone();
    for c in conns {
        if folder.is_empty() || folder == group_of(c) {
            m.remove(&folder_key(c));
        } else {
            m.insert(folder_key(c), folder.to_string());
        }
    }
    if old == m {
        return Ok(());
    }
    let mut out = String::new();
    if m.is_empty() {
        out.push_str("{}");
    } else {
        out.push_str("{\n");
        for (i, (k, v)) in m.iter().enumerate() {
            out.push_str("  ");
            json_str(&mut out, k);
            out.push_str(": ");
            json_str(&mut out, v);
            out.push_str(if i + 1 < m.len() { ",\n" } else { "\n" });
        }
        out.push('}');
    }
    out.push('\n');
    let write = || -> io::Result<()> {
        fs::create_dir_all(path.parent().unwrap_or(dir))?;
        let (target, perm) = write_target(&path, 0o644)?;
        atomicfile::write(&target, ".folders-*.json", out.as_bytes(), perm)
    };
    write().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const DATA_SOURCES: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<project version="4">
  <component name="DataSourceManagerImpl">
    <data-source name="app-stg" group="app" uuid="u-pg"><driver-ref>postgresql</driver-ref><jdbc-url>jdbc:postgresql://db.stg.example.internal:5432/app_stg</jdbc-url></data-source>
    <data-source name="app-stg-redis" group="app" uuid="u-redis"><driver-ref>redis</driver-ref><jdbc-url>jdbc:redis://cache.stg.example.internal:6379/0</jdbc-url></data-source>
  </component>
</project>"#;

    fn fixture(with_env: bool) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".idea")).unwrap();
        fs::write(dir.path().join(".idea/dataSources.xml"), DATA_SOURCES).unwrap();
        if with_env {
            fs::write(
                dir.path().join(".env"),
                "# 주석\nPG_APP_STG_PASSWORD=p%40ss%2Fword\nREDIS_APP_STG_REDIS_PASSWORD=r-secret\n",
            )
            .unwrap();
        }
        dir
    }

    fn conn(name: &str, kind: Kind, host: &str, password: &str) -> Conn {
        Conn {
            name: name.into(),
            kind: Some(kind),
            host: host.into(),
            password: password.into(),
            ..Default::default()
        }
    }

    fn names(dir: &Path) -> String {
        load_project(dir)
            .unwrap()
            .conns
            .iter()
            .map(|c| c.name.clone())
            .collect::<Vec<_>>()
            .join(",")
    }

    fn read(p: impl AsRef<Path>) -> String {
        fs::read_to_string(p).unwrap()
    }

    #[test]
    fn 접속을_json_끝에_더하고_env_를_0600으로_만든다() {
        let dir = fixture(false);
        let d = dir.path();
        let mut a = conn("a1", Kind::Pg, "h1", "p w&/");
        a.group = "g".into();
        a.port = 5432;
        a.db = "d".into();
        a.user = "u".into();
        let c = add_conn(d, a).unwrap();
        assert!(!c.uuid.is_empty() && c.source == SOURCE_DB);
        let mut b = conn("b2", Kind::Redis, "h2", "");
        b.db = "0".into();
        add_conn(d, b).unwrap();
        let json = read(conns_path(d));
        assert!(json.contains("\n  {\n    \"uuid\""), "{json}");
        // uuid 는 무작위 16진수라 "a1"·"b2" 를 품을 수 있어 이름 칸으로 순서를 본다
        let at = |n: &str| json.find(&format!("\"name\": \"{n}\""));
        assert!(at("a1").is_some() && at("a1") < at("b2"), "{json}");
        assert!(!json.contains("p w"), "json 에 비밀번호가 들어갔다");
        assert!(!json.contains("\"url\""), "빈 저장소 칸은 쓰지 않는다");
        assert_eq!(read(d.join(".env")), format!("PG_A1_PASSWORD={}\n", query_escape("p w&/")));
        assert_eq!(fs::metadata(d.join(".env")).unwrap().permissions().mode() & 0o777, 0o600);
        let p = load_project(d).unwrap();
        assert_eq!(p.conns[2].password, "p w&/");
    }

    #[test]
    fn 이름이_겹치거나_env_키가_같으면_거절한다() {
        let dir = fixture(false);
        let d = dir.path();
        assert!(add_conn(d, conn("app-stg", Kind::Pg, "h", "")).is_err(), ".idea 이름과 겹치면 거절");
        add_conn(d, conn("a-b", Kind::Pg, "h", "")).unwrap();
        let err = add_conn(d, conn("a_b", Kind::Pg, "h", "")).unwrap_err();
        assert!(err.msg.contains(".env key"), "{err}");
        assert!(add_conn(d, conn("bad name", Kind::Pg, "h", "")).is_err());
    }

    #[test]
    fn 이름을_바꾸면_env_키를_옮기고_다른_줄과_권한을_지킨다() {
        let dir = fixture(false);
        let d = dir.path();
        add_conn(d, conn("first", Kind::Pg, "h0", "")).unwrap();
        add_conn(d, conn("x", Kind::Pg, "h", "s3")).unwrap();
        add_conn(d, conn("last", Kind::Redis, "h9", "")).unwrap();
        let env = d.join(".env");
        fs::write(&env, "# 머리 주석\nOTHER=1\nPG_X_PASSWORD=s3\n\nAPP_AES_KEY=k\n").unwrap();
        fs::set_permissions(&env, fs::Permissions::from_mode(0o640)).unwrap();
        let mut y = conn("y", Kind::Pg, "h2", "");
        y.port = 6000;
        let c = update_conn(d, "x", y).unwrap();
        assert_eq!(read(&env), "# 머리 주석\nOTHER=1\nPG_Y_PASSWORD=s3\n\nAPP_AES_KEY=k\n");
        assert_eq!(fs::metadata(&env).unwrap().permissions().mode() & 0o777, 0o640);
        assert_eq!(names(d), "app-stg,app-stg-redis,first,y,last");
        let p = load_project(d).unwrap();
        assert_eq!(
            (p.conns[3].uuid.as_str(), p.conns[3].host.as_str(), p.conns[3].password.as_str()),
            (c.uuid.as_str(), "h2", "s3")
        );
        update_conn(d, "y", conn("y", Kind::Pg, "h2", "new")).unwrap();
        let got = read(&env);
        assert!(got.contains("PG_Y_PASSWORD=new\n") && got.matches("PG_Y").count() == 1, "{got}");
    }

    #[test]
    fn DataGrip_접속은_고치거나_지우지_않는다() {
        let dir = fixture(false);
        assert!(update_conn(dir.path(), "app-stg", conn("app-stg", Kind::Pg, "h", "")).is_err());
        assert!(remove_conn(dir.path(), "app-stg").is_err());
    }

    #[test]
    fn env_쓰기가_실패하면_옛_키를_남기고_env_not_updated_다() {
        let dir = fixture(false);
        let d = dir.path();
        add_conn(d, conn("x", Kind::Pg, "h", "keep-me")).unwrap();
        // .env 임시 파일을 못 만든다. .dbtui 폴더는 그대로 쓸 수 있다
        fs::set_permissions(d, fs::Permissions::from_mode(0o555)).unwrap();
        if fs::write(d.join("probe"), "").is_ok() {
            fs::set_permissions(d, fs::Permissions::from_mode(0o755)).unwrap();
            return; // root 는 읽기 전용 폴더에도 쓴다
        }
        let err = update_conn(d, "x", conn("y", Kind::Pg, "h", "")).unwrap_err();
        fs::set_permissions(d, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(err.env_not_updated && err.msg.contains("PG_X_PASSWORD to PG_Y_PASSWORD"), "{err}");
        assert_eq!(read(d.join(".env")), "PG_X_PASSWORD=keep-me\n");
        assert!(read(conns_path(d)).contains("\"name\": \"y\""), "접속은 저장된다");
    }

    #[test]
    fn env_는_링크면_가리키는_파일에_쓰고_폴더면_거절하며_CRLF_를_따른다() {
        let (dir, other) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let real = other.path().join("real.env");
        fs::write(&real, "A=1\r\n").unwrap();
        std::os::unix::fs::symlink(&real, dir.path().join(".env")).unwrap();
        set_env(dir.path(), &["B"], "B=2").unwrap();
        assert!(
            fs::symlink_metadata(dir.path().join(".env")).unwrap().file_type().is_symlink(),
            "링크는 그대로"
        );
        assert_eq!(read(&real), "A=1\r\nB=2\r\n");
        let dir2 = tempfile::tempdir().unwrap();
        fs::create_dir(dir2.path().join("envdir")).unwrap();
        std::os::unix::fs::symlink(dir2.path().join("envdir"), dir2.path().join(".env")).unwrap();
        let err = set_env(dir2.path(), &["B"], "B=2").unwrap_err();
        assert!(err.to_string().contains("is a directory"), "{err}");
    }

    #[test]
    fn 지우면_json_줄과_env_비밀번호를_지우고_나머지는_둔다() {
        let dir = fixture(true);
        let d = dir.path();
        for n in ["a", "b", "c"] {
            add_conn(d, conn(n, Kind::Redis, "h", &format!("pw-{n}"))).unwrap();
        }
        remove_conn(d, "b").unwrap();
        assert_eq!(names(d), "app-stg,app-stg-redis,a,c");
        let env = read(d.join(".env"));
        assert!(
            !env.contains("REDIS_B_PASSWORD") && env.starts_with("# 주석\nPG_APP_STG_PASSWORD=") && env.contains("REDIS_C_PASSWORD=pw-c")
        );
        remove_conn(d, "a").unwrap();
        remove_conn(d, "c").unwrap();
        assert_eq!(read(conns_path(d)).trim(), "[]");
    }

    #[test]
    fn 접속_URL_을_값과_비밀번호로_나눈다() {
        let c = |kind, host: &str, port, db: &str, user: &str, ssl: &str| Conn {
            kind: Some(kind),
            host: host.into(),
            port,
            db: db.into(),
            user: user.into(),
            sslmode: ssl.into(),
            ..Default::default()
        };
        let cases = [
            (
                "postgres://app_ro:pw@db.stg.example.internal:5432/app",
                "pw",
                c(Kind::Pg, "db.stg.example.internal", 5432, "app", "app_ro", ""),
            ),
            ("postgresql://u@h/d", "", c(Kind::Pg, "h", 5432, "d", "u", "")),
            (
                "postgres://u:p%40ss%2Fw%3Ard@h:6543/d?sslmode=require",
                "p@ss/w:rd",
                c(Kind::Pg, "h", 6543, "d", "u", "require"),
            ),
            ("redis://:pw@cache:6380/2", "pw", c(Kind::Redis, "cache", 6380, "2", "", "")),
            (
                "rediss://default:x@cache.amazonaws.com",
                "x",
                c(Kind::Redis, "cache.amazonaws.com", 6379, "", "default", ""),
            ),
            ("redis://[::1]:7000/0", "", c(Kind::Redis, "::1", 7000, "0", "", "")),
        ];
        for (raw, pw, want) in cases {
            let (got, gpw) = parse_conn_url(raw).unwrap_or_else(|e| panic!("{raw}: {e}"));
            assert_eq!(
                format!("{got:?} {} {}", got.sslmode, gpw),
                format!("{want:?} {} {pw}", want.sslmode),
                "{raw}"
            );
        }
        for bad in [
            "mysql://u:pw@h/d",
            "postgres://",
            "postgres://u:sekrit@h:99999/d",
            "redis://h/abc",
            "h:5432",
            "",
            "postgres://u:sekrit@:5432/d",
            "postgres://u@db.internal/app?sslmode=allow",
        ] {
            let err = parse_conn_url(bad).unwrap_err();
            assert!(!err.contains("sekrit"), "{err}");
        }
    }

    #[test]
    fn http_URL_에서_사용자_정보를_뗀다() {
        assert_eq!(
            parse_http_url("https://u:p%40w@search.example.com:9200/").unwrap(),
            ("https://search.example.com:9200".into(), "u".into(), "p@w".into())
        );
        assert!(parse_http_url("ftp://h").is_err());
    }

    #[test]
    fn 폴더를_옮기고_그룹과_같으면_기록을_지운다() {
        let dir = fixture(false);
        let d = dir.path();
        let p = load_project(d).unwrap();
        let c = &p.conns[0];
        set_folder(d, &[c], " moved ").unwrap();
        assert_eq!(read(d.join(".dbtui/folders.json")), "{\n  \"u-pg\": \"moved\"\n}\n");
        assert_eq!(load_project(d).unwrap().conns[0].folder, "moved");
        set_folder(d, &[c], "app").unwrap();
        assert_eq!(read(d.join(".dbtui/folders.json")), "{}\n");
        fs::write(d.join(".dbtui/folders.json"), "{broken").unwrap();
        assert!(set_folder(d, &[c], "x").is_err());
        assert_eq!(read(d.join(".dbtui/folders.json")), "{broken", "깨진 파일은 덮어쓰지 않는다");
    }
}
