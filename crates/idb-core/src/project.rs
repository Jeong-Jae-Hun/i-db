//! DataGrip `.idea` 와 `.env`, `.dbtui/connections.json`·`folders.json` 을 읽어 접속 목록을 만든다(R1·R59·R66).
//! Go `internal/db/project.go`·`connstore.go`·`folders.go` 의 읽기 쪽과 같은 결과를 낸다.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

// ─── 종류 ───

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Pg,
    Redis,
    OpenSearch,
    Loki,
    Prometheus,
    Grafana,
    DynamoDb,
    Athena,
    Aws,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Kind> {
        Some(match s {
            "postgresql" => Kind::Pg,
            "redis" => Kind::Redis,
            "opensearch" => Kind::OpenSearch,
            "loki" => Kind::Loki,
            "prometheus" => Kind::Prometheus,
            "grafana" => Kind::Grafana,
            "dynamodb" => Kind::DynamoDb,
            "athena" => Kind::Athena,
            "aws" => Kind::Aws,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Pg => "postgresql",
            Kind::Redis => "redis",
            Kind::OpenSearch => "opensearch",
            Kind::Loki => "loki",
            Kind::Prometheus => "prometheus",
            Kind::Grafana => "grafana",
            Kind::DynamoDb => "dynamodb",
            Kind::Athena => "athena",
            Kind::Aws => "aws",
        }
    }

    /// 읽기 전용 저장소(R60) 종류인지다.
    pub fn is_source(self) -> bool {
        !matches!(self, Kind::Pg | Kind::Redis)
    }

    /// url 칸이 필요한 HTTP 저장소(OpenSearch·Loki·Prometheus·Grafana)인지다.
    pub fn is_http(self) -> bool {
        self.is_source() && !self.uses_region()
    }

    /// region·profile 칸이 늘 필요한 AWS 종류인지다. OpenSearch 는 sigv4 일 때만 필요하다.
    pub fn uses_region(self) -> bool {
        matches!(self, Kind::DynamoDb | Kind::Athena | Kind::Aws)
    }

    pub(crate) fn auth_options(self) -> &'static [&'static str] {
        match self {
            Kind::OpenSearch => &["", "basic", "sigv4"],
            Kind::Loki | Kind::Prometheus => &["", "basic"],
            Kind::Grafana => &["token"],
            _ => &[""],
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ─── 접속 ───

/// db 전용 접속 파일에서 읽은 접속 표시다(R59.1).
pub const SOURCE_DB: &str = "db";

#[derive(Clone, Default)]
pub struct Conn {
    pub name: String,
    pub uuid: String,
    pub group: String,
    pub kind: Option<Kind>,
    pub host: String,
    pub port: u16,
    pub db: String,
    pub user: String,
    pub password: String,
    pub url: String,
    pub auth: String,
    pub region: String,
    pub profile: String,
    pub workgroup: String,
    pub output: String,
    pub sslmode: String,
    pub folder: String,
    /// 접속을 읽은 곳이다. `SOURCE_DB` 면 `.dbtui/connections.json`, 비면 DataGrip `.idea` 다
    pub source: String,
}

// 디버그 출력에도 비밀번호가 나오지 않게 한다
impl fmt::Debug for Conn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = self.kind.map(Kind::as_str).unwrap_or("");
        write!(
            f,
            "{}({} {}:{}/{} user={})",
            self.name, kind, self.host, self.port, self.db, self.user
        )
    }
}

impl Conn {
    pub fn kind_str(&self) -> &'static str {
        self.kind.map(Kind::as_str).unwrap_or("")
    }

    /// 환경 판정에 쓸 호스트다. 읽기 전용 저장소는 Host 가 비어 URL 의 호스트를 본다(R60.2).
    fn host_of(&self) -> String {
        if self.host.is_empty()
            && !self.url.is_empty()
            && let Ok(u) = url::Url::parse(&self.url)
        {
            return u.host_str().unwrap_or("").trim_matches(['[', ']']).to_string();
        }
        self.host.clone()
    }

    /// 접속 환경이다(R2.2·R6). 이름·AWS 프로필에 prod·prd 가 들거나 호스트의 `.` `-` 마디가 prod·prd 면 prod 다.
    pub fn env(&self) -> &'static str {
        let name = self.name.to_lowercase();
        let profile = self.profile.to_lowercase();
        let host = self.host_of().to_lowercase();
        let host_prod = host.split(['.', '-']).any(|t| t == "prod" || t == "prd");
        if name.contains("prod") || name.contains("prd") || host_prod || profile.contains("prod") || profile.contains("prd") {
            "prod"
        } else if name.contains("stg") {
            "stg"
        } else {
            "local"
        }
    }
}

/// 그룹 없는 접속은 "기타" 로 묶는다.
pub fn group_of(c: &Conn) -> &str {
    if c.group.is_empty() { "기타" } else { &c.group }
}

/// 트리에 보일 폴더다. 옮긴 폴더가 없으면 그룹(없으면 기타)이다.
pub fn folder_of(c: &Conn) -> &str {
    if c.folder.is_empty() { group_of(c) } else { &c.folder }
}

/// 접속 이름을 `.env` 키 조각으로 바꾼다: 영숫자 외 문자는 `_`, 대문자.
pub fn env_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' })
        .collect()
}

/// 접속 비밀값을 두는 `.env` 키다. 비밀값이 없는 종류(DynamoDB·Athena·aws)는 빈 글이다.
pub fn password_key(kind: Option<Kind>, name: &str) -> String {
    if kind.is_some_and(Kind::uses_region) {
        return String::new();
    }
    let prefix = match kind {
        Some(Kind::Redis) => "REDIS_",
        Some(Kind::OpenSearch) => "OS_",
        Some(Kind::Loki) => "LOKI_",
        Some(Kind::Prometheus) => "PROM_",
        Some(Kind::Grafana) => "GRAFANA_",
        _ => "PG_",
    };
    let suffix = if kind == Some(Kind::Grafana) { "_TOKEN" } else { "_PASSWORD" };
    format!("{prefix}{}{suffix}", env_name(name))
}

fn same_secret_key(a: &Conn, b: &Conn) -> bool {
    let k = password_key(a.kind, &a.name);
    !k.is_empty() && k == password_key(b.kind, &b.name)
}

/// `.env` 값은 URL 인코딩돼 있다(.lazysql.toml 시절 형식). 인코딩이 아닌 원문이면 그대로 쓴다.
fn env_password(env: &HashMap<String, String>, kind: Option<Kind>, name: &str) -> String {
    let key = password_key(kind, name);
    if key.is_empty() {
        return String::new();
    }
    let pw = env.get(&key).cloned().unwrap_or_default();
    query_unescape(&pw).unwrap_or(pw)
}

/// Go `url.QueryUnescape` 와 같다: `+` 는 공백, `%XX` 는 바이트. 잘못된 `%` 나 UTF-8 이 아닌 결과는 None 이다.
pub fn query_unescape(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                let hex = |c: u8| (c as char).to_digit(16);
                let (Some(h), Some(l)) = (b.get(i + 1).and_then(|&c| hex(c)), b.get(i + 2).and_then(|&c| hex(c))) else {
                    return None;
                };
                out.push((h * 16 + l) as u8);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    // Go 는 바이트를 그대로 string 으로 둔다. Rust String 은 UTF-8 이어야 해 깨진 결과는 원문을 쓴다
    String::from_utf8(out).ok()
}

// ─── 프로젝트 ───

pub struct Project {
    pub dir: PathBuf,
    pub conns: Vec<Conn>,
    /// queries 상대 경로 → 접속 uuid
    pub mapping: HashMap<String, String>,
    /// 읽지 못하고 건너뛴 접속
    pub warnings: Vec<String>,
    /// `.env` 의 `<그룹>_AES_KEY`·`<그룹>_AES_IV` 값(R37·R52). 비밀값이라 상태 줄·기록·로그에 내지 않는다
    pub aes_env: HashMap<String, String>,
}

impl Project {
    /// 접속 c 그룹의 클라이언트 암호화 키·IV 다(R37·R52). 그룹이 없거나 `.env` 에 없으면 빈 글이다.
    pub fn aes_key_iv(&self, c: &Conn) -> (String, String) {
        if c.group.is_empty() {
            return (String::new(), String::new());
        }
        let n = env_name(&c.group);
        let get = |k: String| self.aes_env.get(&k).cloned().unwrap_or_default();
        (get(format!("{n}_AES_KEY")), get(format!("{n}_AES_IV")))
    }

    pub fn conn_by_name(&self, name: &str) -> Option<&Conn> {
        self.conns.iter().find(|c| c.name == name)
    }
}

#[derive(Debug)]
pub struct LoadError(pub String);

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for LoadError {}

#[derive(Default)]
struct XmlSource {
    name: String,
    uuid: String,
    group: String,
    driver: String,
    jdbc_url: String,
    user: String,
}

pub fn load_project(dir: &Path) -> Result<Project, LoadError> {
    let ds_path = dir.join(".idea").join("dataSources.xml");
    let ds = match fs::read(&ds_path) {
        Ok(b) => parse_data_sources(&b).map_err(|e| LoadError(format!("read {}: {e}", ds_path.display())))?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(LoadError(format!("dataSources.xml not found: {}", ds_path.display())));
        }
        Err(e) => return Err(LoadError(format!("read {}: {e}", ds_path.display()))),
    };

    let local = read_optional(&dir.join(".idea").join("dataSources.local.xml"), parse_data_sources)?;
    let users: HashMap<String, String> = local.into_iter().map(|s| (s.uuid, s.user)).collect();

    // .env: KEY=VALUE 줄만 읽는다. 없으면 비밀번호 없이 진행한다(R1.1)
    let mut env = HashMap::new();
    let env_path = dir.join(".env");
    match fs::read(&env_path) {
        Ok(b) => {
            for line in String::from_utf8_lossy(&b).split('\n') {
                let line = line.trim();
                let Some((k, v)) = line.split_once('=') else { continue };
                if line.starts_with('#') {
                    continue;
                }
                env.insert(k.trim().to_string(), v.trim().trim_matches(['"', '\'']).to_string());
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(LoadError(format!("read {}: {e}", env_path.display()))),
    }

    let aes_env = env
        .iter()
        .filter(|(k, _)| k.ends_with("_AES_KEY") || k.ends_with("_AES_IV"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let mut p = Project {
        dir: dir.to_path_buf(),
        conns: Vec::new(),
        mapping: HashMap::new(),
        warnings: Vec::new(),
        aes_env,
    };

    for s in ds {
        let kind = match s.driver.as_str() {
            "postgresql" => Kind::Pg,
            "redis" => Kind::Redis,
            _ => continue,
        };
        let Some((host, port, db)) = parse_jdbc(&s.jdbc_url) else {
            p.warnings.push(format!("{}: unsupported jdbc url", s.name));
            continue;
        };
        let port = match port {
            None => 0,
            Some(raw) => match raw.parse::<u32>() {
                Ok(n) if (1..=65535).contains(&n) => n as u16,
                _ => {
                    p.warnings.push(format!("{}: invalid port {}", s.name, go_quote(&raw)));
                    continue;
                }
            },
        };
        let password = env_password(&env, Some(kind), &s.name);
        p.conns.push(Conn {
            user: users.get(&s.uuid).cloned().unwrap_or_default(),
            name: s.name,
            uuid: s.uuid,
            group: s.group,
            kind: Some(kind),
            host,
            port,
            db,
            password,
            ..Default::default()
        });
    }
    load_db_conns(&mut p, &env)?;
    load_folders(&mut p);

    let mapping = read_optional(&dir.join(".idea").join("data_source_mapping.xml"), parse_mapping)?;
    for (url, value) in mapping {
        let key = url.strip_prefix("file://$PROJECT_DIR$/").unwrap_or(&url).to_string();
        p.mapping.insert(key, value);
    }
    Ok(p)
}

fn read_optional<T: Default>(path: &Path, parse: fn(&[u8]) -> Result<T, String>) -> Result<T, LoadError> {
    match fs::read(path) {
        Ok(b) => parse(&b).map_err(|e| LoadError(format!("read {}: {e}", path.display()))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(LoadError(format!("read {}: {e}", path.display()))),
    }
}

/// `jdbc:<드라이버>://호스트[:포트][/DB][?…]` 를 나눈다. Go `jdbcRe` 와 같은 꼴만 받는다.
fn parse_jdbc(raw: &str) -> Option<(String, Option<String>, String)> {
    let rest = raw.strip_prefix("jdbc:")?;
    let (driver, rest) = rest.split_once("://")?;
    if driver.is_empty() || !driver.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    let host_end = rest.find([':', '/', '@', '?']).unwrap_or(rest.len());
    let host = &rest[..host_end];
    if host.is_empty() {
        return None;
    }
    let mut rest = &rest[host_end..];
    let mut port = None;
    if let Some(r) = rest.strip_prefix(':') {
        let end = r.find(|c: char| !c.is_ascii_digit()).unwrap_or(r.len());
        if end == 0 {
            return None;
        }
        port = Some(r[..end].to_string());
        rest = &r[end..];
    }
    let mut db = String::new();
    if let Some(r) = rest.strip_prefix('/') {
        let end = r.find('?').unwrap_or(r.len());
        db = r[..end].to_string();
        rest = &r[end..];
    }
    if !rest.is_empty() && !rest.starts_with('?') {
        return None;
    }
    Some((host.to_string(), port, db))
}

/// XML 글을 연다. Go encoding/xml 처럼 UTF-8 BOM·앞 공백을 견딘다.
fn xml_doc(b: &[u8]) -> Result<roxmltree::Document<'_>, String> {
    let text = std::str::from_utf8(b).map_err(|e| e.to_string())?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    roxmltree::Document::parse_with_options(
        text,
        roxmltree::ParsingOptions {
            allow_dtd: true,
            ..Default::default()
        },
    )
    .map_err(|e| e.to_string())
}

/// `project>component>child` 꼴 원소들이다(Go `xml:"component>data-source"` 경로).
fn component_children<'a>(doc: &'a roxmltree::Document<'a>, child: &'a str) -> impl Iterator<Item = roxmltree::Node<'a, 'a>> {
    doc.root_element()
        .children()
        .filter(|n| n.has_tag_name("component"))
        .flat_map(move |c| c.children().filter(move |n| n.is_element() && n.tag_name().name() == child))
}

/// 원소 안 첫 child 원소의 글이다. 없으면 빈 글이다.
fn child_text(n: roxmltree::Node<'_, '_>, name: &str) -> String {
    n.children()
        .find(|c| c.is_element() && c.tag_name().name() == name)
        .map(|c| c.descendants().filter(|d| d.is_text()).filter_map(|d| d.text()).collect())
        .unwrap_or_default()
}

/// `project>component>data-source` 의 속성과 `driver-ref`·`jdbc-url`·`user-name` 글을 읽는다.
fn parse_data_sources(b: &[u8]) -> Result<Vec<XmlSource>, String> {
    let doc = xml_doc(b)?;
    Ok(component_children(&doc, "data-source")
        .map(|n| XmlSource {
            name: n.attribute("name").unwrap_or_default().to_string(),
            uuid: n.attribute("uuid").unwrap_or_default().to_string(),
            group: n.attribute("group").unwrap_or_default().to_string(),
            driver: child_text(n, "driver-ref"),
            jdbc_url: child_text(n, "jdbc-url"),
            user: child_text(n, "user-name"),
        })
        .collect())
}

/// `project>component>file` 의 url·value 속성을 읽는다.
fn parse_mapping(b: &[u8]) -> Result<Vec<(String, String)>, String> {
    let doc = xml_doc(b)?;
    Ok(component_children(&doc, "file")
        .map(|n| {
            (
                n.attribute("url").unwrap_or_default().to_string(),
                n.attribute("value").unwrap_or_default().to_string(),
            )
        })
        .collect())
}

// ─── db 전용 접속 (R59) ───

#[derive(Deserialize, Default)]
#[serde(default)]
pub(crate) struct ConnEntry {
    pub(crate) uuid: String,
    pub(crate) name: String,
    pub(crate) group: String,
    pub(crate) kind: String,
    pub(crate) host: String,
    pub(crate) port: i64,
    pub(crate) db: String,
    pub(crate) user: String,
    pub(crate) url: String,
    pub(crate) auth: String,
    pub(crate) region: String,
    pub(crate) profile: String,
    pub(crate) sslmode: String,
    pub(crate) workgroup: String,
    pub(crate) output: String,
}

fn load_db_conns(p: &mut Project, env: &HashMap<String, String>) -> Result<(), LoadError> {
    let path = p.dir.join(".dbtui").join("connections.json");
    let entries: Vec<ConnEntry> = match fs::read(&path) {
        Ok(b) => serde_json::from_slice::<Option<Vec<ConnEntry>>>(&b)
            .map_err(|e| LoadError(format!("read {}: {}", path.display(), crate::gojson::json_error(&e))))?
            .unwrap_or_default(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(LoadError(format!("read {}: {e}", path.display()))),
    };
    for e in entries {
        let kind = Kind::parse(&e.kind);
        let port_ok = (0..=65535).contains(&e.port);
        let mut c = Conn {
            uuid: e.uuid,
            name: e.name,
            group: e.group,
            kind,
            host: e.host,
            port: if port_ok { e.port as u16 } else { 0 },
            db: e.db,
            user: e.user,
            url: e.url,
            auth: e.auth,
            region: e.region,
            profile: e.profile,
            sslmode: e.sslmode,
            workgroup: e.workgroup,
            output: e.output,
            source: SOURCE_DB.into(),
            ..Default::default()
        };
        if c.uuid.is_empty() {
            p.warnings.push(format!("{}: missing uuid (connections.json)", c.name));
            continue;
        }
        if p.conns.iter().any(|o| o.name == c.name || o.uuid == c.uuid) {
            p.warnings.push(format!("{}: name or uuid already used (connections.json)", c.name));
            continue;
        }
        if p.conns.iter().any(|o| same_secret_key(o, &c)) {
            p.warnings.push(format!(
                "{}: same .env password key as another connection (connections.json)",
                c.name
            ));
            continue;
        }
        if let Err(err) = validate_conn(&c, &e.kind, e.port) {
            p.warnings.push(format!("{}: {err} (connections.json)", c.name));
            continue;
        }
        c.password = env_password(env, c.kind, &c.name);
        p.conns.push(c);
    }
    Ok(())
}

pub(crate) fn valid_conn_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Go `ValidateConn` 과 같은 순서로 같은 오류 글을 낸다. raw_kind·raw_port 는 파일에 적힌 그대로다.
pub(crate) fn validate_conn(c: &Conn, raw_kind: &str, raw_port: i64) -> Result<(), String> {
    if !valid_conn_name(&c.name) {
        return Err("name must be letters, digits, - or _".into());
    }
    match c.kind {
        Some(k) if k.is_source() => return validate_source(c, k),
        Some(_) => {}
        None => {
            return Err(format!(
                "kind must be postgresql, redis, opensearch, loki, prometheus, grafana, dynamodb, athena or aws: {}",
                go_quote(raw_kind)
            ));
        }
    }
    if c.host.is_empty() {
        return Err("host is required".into());
    }
    if !(0..=65535).contains(&raw_port) {
        return Err(format!("invalid port {raw_port}"));
    }
    if c.kind == Some(Kind::Redis) && !c.db.is_empty() && go_atoi(&c.db).is_none_or(|n| n < 0) {
        return Err(format!("redis db must be a number: {}", go_quote(&c.db)));
    }
    Ok(())
}

fn validate_source(c: &Conn, k: Kind) -> Result<(), String> {
    let opts = k.auth_options();
    if !opts.contains(&c.auth.as_str()) {
        let list = opts.iter().map(|o| go_quote(o)).collect::<Vec<_>>().join(" ");
        return Err(format!("{k} auth must be one of [{list}]: {}", go_quote(&c.auth)));
    }
    if k.is_http() && c.url.is_empty() {
        return Err("url is required".into());
    } else if k == Kind::Aws && !c.url.is_empty() {
        return Err("aws connection has no url".into());
    } else if !c.url.is_empty() {
        check_http_url(&c.url)?;
    }
    if (k.uses_region() || c.auth == "sigv4") && c.region.is_empty() {
        return Err("region is required (dynamodb, athena, aws, sigv4)".into());
    }
    if k == Kind::Athena && !c.url.is_empty() {
        return Err("athena does not use url (region, profile, workgroup, output)".into());
    }
    if k == Kind::Athena && !c.output.is_empty() && !c.output.starts_with("s3://") {
        return Err("athena output must be s3://bucket/prefix/".into());
    }
    Ok(())
}

fn check_http_url(raw: &str) -> Result<(), String> {
    match url::Url::parse(raw) {
        Ok(u) if (u.scheme() == "http" || u.scheme() == "https") && u.host_str().is_some_and(|h| !h.is_empty()) => {
            if !u.username().is_empty() || u.password().is_some() {
                Err("url must not contain user or password (use user and .env)".into())
            } else {
                Ok(())
            }
        }
        _ => Err("url must be http(s)://host".into()),
    }
}

// ─── 트리 폴더 (R66.2) ───

fn load_folders(p: &mut Project) {
    let path = p.dir.join(".dbtui").join("folders.json");
    let m: HashMap<String, String> = match fs::read(&path) {
        Ok(b) => match serde_json::from_slice::<Option<HashMap<String, String>>>(&b) {
            Ok(m) => m.unwrap_or_default(),
            Err(e) => {
                p.warnings.push(format!("folders.json: {}", crate::gojson::json_error(&e)));
                return;
            }
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => return,
        Err(e) => {
            p.warnings.push(format!("folders.json: {e}"));
            return;
        }
    };
    let mut warnings = Vec::new();
    for c in &mut p.conns {
        let key = if c.uuid.is_empty() {
            format!("name:{}", c.name)
        } else {
            c.uuid.clone()
        };
        let Some(f) = m.get(&key) else { continue };
        if let Err(e) = valid_folder(f) {
            warnings.push(format!("folders.json: {}: {e}", c.name));
            continue;
        }
        c.folder = f.clone();
    }
    p.warnings.extend(warnings);
}

pub fn valid_folder(f: &str) -> Result<(), &'static str> {
    if f.is_empty() {
        return Err("folder name is empty");
    }
    if f.chars().count() > 64 {
        return Err("folder name is longer than 64");
    }
    // 줄 순서를 뒤집어 보이게 하는 방향 문자(Cf)·줄 구분 문자도 막는다
    if f.chars().any(|r| r.is_control() || is_format_or_separator(r)) {
        return Err("folder name has control characters");
    }
    Ok(())
}

/// 유니코드 Cf(형식)·Zl·Zp 문자 가운데 이름에 끼어들 만한 것이다. Go `unicode.Cf` 전체 표 대신 실제로 쓰이는 범위만 둔다.
/// 한계: 드문 Cf(몽골 모음 구분자 등)는 놓친다. 표가 필요해지면 unicode-general-category 크레이트로 바꾼다.
fn is_format_or_separator(r: char) -> bool {
    matches!(r as u32,
        0x00AD | 0x0600..=0x0605 | 0x061C | 0x06DD | 0x070F | 0x180E | 0x200B..=0x200F | 0x2028..=0x202E
        | 0x2060..=0x2064 | 0x2066..=0x206F | 0xFEFF | 0xFFF9..=0xFFFB | 0x110BD | 0x1D173..=0x1D17A | 0xE0001 | 0xE0020..=0xE007F)
}

// ─── Go 와 같은 글 꼴 ───

/// Go `strconv.Quote`(`%q`) 와 같은 따옴표 글이다. 출력 글이 Go 판과 바이트로 같아야 해 직접 만든다.
pub fn go_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\x07' => out.push_str("\\a"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x0b' => out.push_str("\\v"),
            c if is_go_printable(c) => out.push(c),
            c if (c as u32) < 0x80 => out.push_str(&format!("\\x{:02x}", c as u32)),
            c if (c as u32) < 0x10000 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push_str(&format!("\\U{:08x}", c as u32)),
        }
    }
    out.push('"');
    out
}

/// Go `strconv.IsPrint` 의 근사다: 제어 문자·형식 문자·줄 구분자·비공백 공백류가 아니면 찍는다.
fn is_go_printable(c: char) -> bool {
    if c == ' ' {
        return true;
    }
    !(c.is_control() || is_format_or_separator(c) || c.is_whitespace())
}

/// Go `strconv.Atoi` 와 같다: 앞 부호 하나와 10진 숫자만.
pub fn go_atoi(s: &str) -> Option<i64> {
    let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jdbc_주소를_호스트_포트_DB_로_나눈다() {
        assert_eq!(
            parse_jdbc("jdbc:postgresql://h:5432/app?sslmode=require"),
            Some(("h".into(), Some("5432".into()), "app".into()))
        );
        assert_eq!(parse_jdbc("jdbc:redis://h/0"), Some(("h".into(), None, "0".into())));
        assert_eq!(parse_jdbc("jdbc:postgresql://h"), Some(("h".into(), None, String::new())));
        assert_eq!(parse_jdbc("jdbc:postgresql://u@h/app"), None);
        assert_eq!(parse_jdbc("postgresql://h/app"), None);
    }

    #[test]
    fn 환경은_이름_호스트_프로필_순으로_prod_를_먼저_본다() {
        let c = |name: &str, host: &str, profile: &str| {
            Conn {
                name: name.into(),
                host: host.into(),
                profile: profile.into(),
                ..Default::default()
            }
            .env()
        };
        assert_eq!(c("Shop-PROD", "", ""), "prod");
        assert_eq!(c("app-stg", "redis01.prod.local", ""), "prod");
        assert_eq!(c("ddb", "", "prod-admin"), "prod");
        assert_eq!(c("app-stg", "", ""), "stg");
        assert_eq!(c("live", "productive.local", ""), "local");
    }

    #[test]
    fn env_이름과_비밀값_키() {
        assert_eq!(env_name("한글-stg.1"), "___STG_1");
        assert_eq!(password_key(Some(Kind::Grafana), "g-1"), "GRAFANA_G_1_TOKEN");
        assert_eq!(password_key(Some(Kind::DynamoDb), "d"), "");
        assert_eq!(query_unescape("a%2Bb+c").as_deref(), Some("a+b c"));
        assert_eq!(query_unescape("100%"), None);
    }
}
