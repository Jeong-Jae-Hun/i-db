//! `db update`(R65). Go `update.go`·`internal/selfupdate` 를 옮긴다.
//! GitHub 릴리스에서 이 OS·CPU 의 묶음을 받아 checksums.txt 의 sha256 으로 검증하고, 묶음 안의 실행 파일 하나만 꺼내
//! 지금 실행 중인 바이너리를 바꿔 끼운다. 켤 때 하루 한 번 묻는 알림(R65.2)은 화면 쪽이라 여기 없다.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use http_body_util::{BodyExt, Empty, Limited};
use hyper::body::Bytes;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use idb_core::atomicfile;
use idb_core::project::go_quote;
use sha2::{Digest, Sha256};

use crate::cli::{EXIT_ERR, EXIT_OK, EXIT_REFUSED, EXIT_USAGE};
use crate::goflag::{FlagDef, FlagSet, FlagType, ParseError, Parsed};
use crate::usage_text::UPDATE_USAGE;

/// update --check 가 새 판을 찾았다
const EXIT_NEWER: i32 = 10;
pub const DEFAULT_API: &str = "https://api.github.com";
pub const DEFAULT_REPO: &str = "Jeong-Jae-Hun/i-db";
/// 릴리스 묶음과 그 안 실행 파일의 이름이다(묶음은 `<이름>_<os>_<arch>.tar.gz`). v0.x Go 판 묶음과 같은 꼴이라
/// `db update --version v0.<판> --force` 로 Go 판으로 되돌릴 수도 있다.
pub const RELEASE_BIN: &str = "db";
/// 받는 묶음과 꺼낸 바이너리의 상한이다
const MAX_ARCHIVE: u64 = 100 << 20;
const MAX_METADATA: u64 = 4 << 20;
const MAX_REDIRECTS: usize = 10;
const USER_AGENT: &str = "db-selfupdate";
/// Go url.PathEscape 가 그대로 두는 글자를 뺀 집합이다
const PATH_SEGMENT: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~')
    .remove(b'$')
    .remove(b'&')
    .remove(b'+')
    .remove(b',')
    .remove(b':')
    .remove(b';')
    .remove(b'=')
    .remove(b'@');

/// 이 빌드의 판이다. 릴리스 빌드는 IDB_VERSION 환경 변수로 넣고, 없으면 소스 빌드(dev)다.
pub fn version() -> &'static str {
    option_env!("IDB_VERSION").unwrap_or("dev")
}

// ─── 판 비교 ───

struct Semver {
    core: [u64; 3],
    pre: String,
}

fn digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// 점으로 나눈 식별자가 모두 비지 않은 [0-9A-Za-z-] 인지다. pre-release 의 숫자 식별자는 0 으로 시작하면 안 된다.
fn valid_idents(s: &str, pre: bool) -> bool {
    s.split('.').all(|id| {
        !id.is_empty()
            && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !(pre && id.len() > 1 && id.starts_with('0') && digits(id))
    })
}

/// v1.2.3[-pre][+build] 을 semver 대로 읽는다.
fn parse(v: &str) -> Option<Semver> {
    let rest = v.strip_prefix('v')?;
    let (rest, build) = match rest.split_once('+') {
        Some((r, b)) => (r, Some(b)),
        None => (rest, None),
    };
    let (core, pre) = match rest.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (rest, None),
    };
    if build.is_some_and(|b| !valid_idents(b, false)) || pre.is_some_and(|p| !valid_idents(p, true)) {
        return None;
    }
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let mut s = Semver {
        core: [0; 3],
        pre: pre.unwrap_or("").to_string(),
    };
    for (i, p) in parts.iter().enumerate() {
        if !digits(p) || (p.len() > 1 && p.starts_with('0')) {
            return None;
        }
        s.core[i] = p.parse().ok()?;
    }
    Some(s)
}

/// v 가 비교할 수 있는 판(vX.Y.Z)인지다. 소스 빌드(dev)는 false 다.
pub fn valid(v: &str) -> bool {
    parse(v).is_some()
}

/// a 와 b 를 비교한다(-1·0·1). 둘 중 하나라도 판이 아니면 None 이다. 같은 X.Y.Z 면 정식 판이 pre-release 보다 높다.
/// pre-release 는 점으로 나눈 식별자를 앞에서부터 비교한다: 숫자끼리는 수로, 숫자는 글자보다 낮고, 앞이 같으면 짧은 쪽이 낮다.
pub fn compare(a: &str, b: &str) -> Option<i32> {
    use std::cmp::Ordering;
    let (x, y) = (parse(a)?, parse(b)?);
    let ord = |o: Ordering| o as i32;
    for i in 0..3 {
        if x.core[i] != y.core[i] {
            return Some(ord(x.core[i].cmp(&y.core[i])));
        }
    }
    if x.pre == y.pre {
        return Some(0);
    }
    if x.pre.is_empty() {
        return Some(1);
    }
    if y.pre.is_empty() {
        return Some(-1);
    }
    let (xs, ys): (Vec<&str>, Vec<&str>) = (x.pre.split('.').collect(), y.pre.split('.').collect());
    for (a, b) in xs.iter().zip(&ys) {
        let c = match (digits(a), digits(b)) {
            // 0 으로 시작하지 않으므로 길이가 길면 큰 수다(넘침 없이)
            (true, true) => a.len().cmp(&b.len()).then_with(|| a.cmp(b)),
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            _ => a.cmp(b),
        };
        if c != Ordering::Equal {
            return Some(ord(c));
        }
    }
    Some(ord(xs.len().cmp(&ys.len())))
}

// ─── 릴리스 ───

/// 릴리스 하나다. assets 는 파일 이름 → 내려받기 주소다.
#[derive(Debug, Default)]
pub struct Release {
    pub tag: String,
    pub assets: std::collections::HashMap<String, String>,
}

/// 릴리스를 읽어 올 곳이다. api 가 비면 DEFAULT_API 이고 그때 넘어가기·내려받기는 https 의 GitHub 호스트만 받는다.
/// api 를 주입하면(시험) 그 주소 아래의 내려받기는 https·GitHub 호스트가 아니어도 받고 넘어가기도 막지 않는다.
/// token 은 API 요청에만 붙이고, 처음과 다른 호스트(포트 포함)로 넘어가면 지운다.
#[derive(Clone, Default)]
pub struct Source {
    pub api: String,
    pub token: String,
}

/// 릴리스 API·내려받기(github.com → *.githubusercontent.com)가 쓰는 https 주소인지다.
fn github_url(u: &url::Url) -> bool {
    let h = u.host_str().unwrap_or("");
    u.scheme() == "https" && (h == "api.github.com" || h == "github.com" || h.ends_with(".githubusercontent.com"))
}

/// 주소의 호스트:포트(Go URL.Host)다.
fn host_port(u: &url::Url) -> String {
    match u.port() {
        Some(p) => format!("{}:{p}", u.host_str().unwrap_or("")),
        None => u.host_str().unwrap_or("").to_string(),
    }
}

/// 비밀값이 없는 주소 글(Go URL.Redacted)이다.
fn redacted(u: &url::Url) -> String {
    let mut u = u.clone();
    if u.password().is_some() {
        let _ = u.set_password(Some("xxxxx"));
    }
    u.to_string()
}

struct Response {
    status: u16,
    body: Vec<u8>,
}

/// Go resp.Status 꼴(`404 Not Found`)이다.
fn status_text(code: u16) -> String {
    let reason = hyper::StatusCode::from_u16(code)
        .ok()
        .and_then(|s| s.canonical_reason())
        .unwrap_or("");
    format!("{code} {reason}").trim_end().to_string()
}

impl Source {
    fn injected(&self) -> bool {
        !self.api.is_empty()
    }

    fn api(&self) -> &str {
        if self.api.is_empty() { DEFAULT_API } else { &self.api }
    }

    /// GET 을 보내고 넘어가기를 손으로 따라간다. 본문은 limit+1 바이트까지만 읽는다(넘으면 호출부가 오류로 본다).
    async fn get(&self, raw: &str, api: bool, limit: u64) -> Result<Response, String> {
        let tls = hyper_rustls::HttpsConnectorBuilder::new()
            .with_provider_and_native_roots(rustls::crypto::ring::default_provider())
            .map_err(|e| e.to_string())?
            .https_or_http()
            .enable_http1()
            .build();
        let client: Client<_, Empty<Bytes>> = Client::builder(TokioExecutor::new()).build(tls);
        let mut u = url::Url::parse(raw).map_err(|e| e.to_string())?;
        let first = host_port(&u);
        for hop in 0.. {
            let mut req = hyper::Request::get(u.as_str()).header("User-Agent", USER_AGENT);
            if api {
                req = req.header("Accept", "application/vnd.github+json");
                if !self.token.is_empty() && host_port(&u) == first {
                    req = req.header("Authorization", format!("Bearer {}", self.token));
                }
            }
            let req = req.body(Empty::new()).map_err(|e| e.to_string())?;
            let resp = client
                .request(req)
                .await
                .map_err(|e| format!("Get {}: {e}", go_quote(u.as_str())))?;
            let status = resp.status().as_u16();
            if resp.status().is_redirection()
                && let Some(loc) = resp.headers().get("Location").and_then(|v| v.to_str().ok())
            {
                if hop + 1 >= MAX_REDIRECTS {
                    return Err("stopped after 10 redirects".into());
                }
                let next = u.join(loc).map_err(|e| e.to_string())?;
                // 기본 클라이언트는 https 의 GitHub 호스트로만 넘어간다
                if !self.injected() && !github_url(&next) {
                    return Err(format!("refuse redirect to {}", redacted(&next)));
                }
                u = next;
                continue;
            }
            let body = Limited::new(resp.into_body(), (limit + 1) as usize).collect().await;
            let body = match body {
                Ok(b) => b.to_bytes().to_vec(),
                // 상한을 넘으면 넘은 것만 알면 되니 limit+1 바이트로 본다
                Err(e) if e.downcast_ref::<http_body_util::LengthLimitError>().is_some() => vec![0; (limit + 1) as usize],
                Err(e) => return Err(e.to_string()),
            };
            return Ok(Response { status, body });
        }
        unreachable!()
    }

    /// repo 의 tag 릴리스다. tag 가 비면 최신 릴리스다.
    pub async fn release(&self, repo: &str, tag: &str) -> Result<Release, String> {
        if !tag.is_empty() && !valid(tag) {
            return Err(format!("tag {} is not a version", go_quote(tag)));
        }
        let base = self.api().trim_end_matches('/');
        let u = if tag.is_empty() {
            format!("{base}/repos/{repo}/releases/latest")
        } else {
            format!(
                "{base}/repos/{repo}/releases/tags/{}",
                percent_encoding::utf8_percent_encode(tag, PATH_SEGMENT)
            )
        };
        let resp = self.get(&u, true, MAX_METADATA).await?;
        match resp.status {
            404 if !tag.is_empty() => return Err(format!("릴리스가 없음: {tag}")),
            403 | 429 => {
                return Err(format!(
                    "GitHub API {} (요청 한도면 GITHUB_TOKEN 을 두고 다시)",
                    status_text(resp.status)
                ));
            }
            200 => {}
            s => return Err(format!("GitHub API {}", status_text(s))),
        }
        #[derive(serde::Deserialize)]
        struct Asset {
            #[serde(default)]
            name: String,
            #[serde(default)]
            browser_download_url: String,
        }
        #[derive(serde::Deserialize)]
        struct Body {
            #[serde(default)]
            tag_name: String,
            #[serde(default)]
            assets: Option<Vec<Asset>>,
        }
        let body: Body = serde_json::Deserializer::from_slice(&resp.body)
            .into_iter::<Body>()
            .next()
            .unwrap_or_else(|| Err(serde::de::Error::custom("EOF")))
            .map_err(|e| format!("read release: {}", idb_core::gojson::json_error(&e)))?;
        // 태그는 화면·상태 파일에 그대로 나가므로 판 꼴만 받는다(제어 문자 차단)
        if !valid(&body.tag_name) {
            return Err(format!("release tag {} is not a version", go_quote(&body.tag_name)));
        }
        if !tag.is_empty() && body.tag_name != tag {
            return Err(format!("release tag {} does not match requested {tag}", go_quote(&body.tag_name)));
        }
        Ok(Release {
            tag: body.tag_name,
            assets: body
                .assets
                .unwrap_or_default()
                .into_iter()
                .map(|a| (a.name, a.browser_download_url))
                .collect(),
        })
    }

    async fn download(&self, raw: &str, limit: u64) -> Result<Vec<u8>, String> {
        let u = url::Url::parse(raw).map_err(|e| e.to_string())?;
        let injected = self.injected() && raw.starts_with(&format!("{}/", self.api.trim_end_matches('/')));
        if !github_url(&u) && !injected {
            return Err(format!("refuse download from {}", redacted(&u)));
        }
        let resp = self.get(raw, false, limit).await?;
        if resp.status != 200 {
            return Err(format!("download {}", status_text(resp.status)));
        }
        if resp.body.len() as u64 > limit {
            return Err(format!("download exceeds {limit} bytes"));
        }
        Ok(resp.body)
    }

    /// rel 의 os/arch 묶음을 받아 검증하고 exe 를 바꿔 끼운다. exe 가 심볼릭 링크면 가리키는 파일을 바꾼다.
    /// 어느 단계에서 실패해도 원래 파일은 그대로다. 쓸 권한이 없으면 Err 의 bool 이 참이다(sudo 안내).
    pub async fn apply(&self, rel: &Release, os: &str, arch: &str, exe: &Path) -> Result<(), (String, bool)> {
        use std::os::unix::fs::PermissionsExt;
        let name = archive_name(os, arch);
        let plain = |s: String| (s, false);
        let archive_url = rel
            .assets
            .get(&name)
            .ok_or_else(|| plain(format!("{} 에 {name} 가 없음", rel.tag)))?;
        let sums_url = rel
            .assets
            .get("checksums.txt")
            .ok_or_else(|| plain(format!("{} 에 checksums.txt 가 없음", rel.tag)))?;
        let target = std::fs::canonicalize(exe).map_err(|e| plain(e.to_string()))?;
        let perm = std::fs::metadata(&target)
            .map(|m| m.permissions().mode() & 0o777)
            .ok()
            .filter(|p| *p != 0)
            .unwrap_or(0o755);
        let sums = self
            .download(sums_url, MAX_METADATA)
            .await
            .map_err(|e| plain(format!("checksums.txt: {e}")))?;
        let want = checksum_of(&sums, &name).map_err(plain)?;
        let archive = self
            .download(archive_url, MAX_ARCHIVE)
            .await
            .map_err(|e| plain(format!("{name}: {e}")))?;
        let got: String = Sha256::digest(&archive).iter().map(|b| format!("{b:02x}")).collect();
        if got != want {
            return Err(plain(format!("checksum mismatch: {name}")));
        }
        let bin = extract_bin(&archive).map_err(|e| plain(format!("{name}: {e}")))?;
        atomicfile::write(&target, ".db-update-*", &bin, perm).map_err(|e| {
            let ro = e.kind() == std::io::ErrorKind::PermissionDenied || e.kind() == std::io::ErrorKind::ReadOnlyFilesystem;
            if ro {
                (format!("쓸 수 없음: {} (sudo 로 다시)", target.display()), true)
            } else {
                plain(e.to_string())
            }
        })
    }
}

/// release.yml 이 올리는 묶음 이름이다.
fn archive_name(os: &str, arch: &str) -> String {
    format!("{RELEASE_BIN}_{os}_{arch}.tar.gz")
}

/// sha256sum 출력에서 name 의 해시다. release.yml 은 `sha256sum ./*.tar.gz` 로 만들어 이름 앞에 ./ 가 붙는다.
fn checksum_of(sums: &[u8], name: &str) -> Result<String, String> {
    for line in String::from_utf8_lossy(sums).lines() {
        let Some((sum, file)) = line.trim().split_once(' ') else { continue };
        // 두 칸 띄움과 바이너리 표시(*)를 걷어 낸다
        let file = file.trim();
        let file = file.strip_prefix('*').unwrap_or(file);
        let file = file.strip_prefix("./").unwrap_or(file);
        if file == name && sum.len() == 64 {
            return Ok(sum.to_lowercase());
        }
    }
    Err(format!("checksums.txt 에 {name} 가 없음"))
}

/// 읽은 바이트를 세는 읽개다. 압축이 풀린 전체 크기 상한을 본다. 남은 양은 tar 가 읽개를 쥔 동안에도 보게 나눠 든다.
struct Counted<R> {
    inner: R,
    left: std::rc::Rc<std::cell::Cell<u64>>,
}

impl<R: Read> Read for Counted<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let left = self.left.get();
        if left == 0 {
            return Ok(0);
        }
        let n = buf.len().min(left as usize);
        let got = self.inner.read(&mut buf[..n])?;
        self.left.set(left - got as u64);
        Ok(got)
    }
}

/// tar.gz 맨 위의 일반 파일 RELEASE_BIN 만 꺼낸다. 다른 항목은 디스크에 쓰지 않고 넘긴다.
/// RELEASE_BIN 이 링크·폴더이거나 둘 이상이면 거절한다. 넘기는 항목도 풀어야 하므로 풀린 전체 크기를 2*MAX_ARCHIVE 로 막는다.
fn extract_bin(archive: &[u8]) -> Result<Vec<u8>, String> {
    let gz = flate2::read::GzDecoder::new(archive);
    if gz.header().is_none() {
        return Err("gzip: invalid header".into());
    }
    let limit = 2 * MAX_ARCHIVE;
    let left = std::rc::Rc::new(std::cell::Cell::new(limit));
    // 상한에 닿으면 tar 는 EOF 로 보므로 남은 양으로 가른다(정확히 상한에서 끝나는 묶음도 거절한다)
    let fail = |e: std::io::Error| {
        if left.get() == 0 {
            format!("archive exceeds {limit} bytes uncompressed")
        } else {
            e.to_string()
        }
    };
    let mut tr = tar::Archive::new(Counted {
        inner: gz,
        left: left.clone(),
    });
    let mut bin: Option<Vec<u8>> = None;
    for entry in tr.entries().map_err(fail)? {
        let mut entry = entry.map_err(fail)?;
        let path = entry.path().map_err(fail)?.to_string_lossy().into_owned();
        if path.strip_prefix("./").unwrap_or(&path) != RELEASE_BIN {
            // 넘기는 항목도 읽어야 다음 머리로 간다
            std::io::copy(&mut entry, &mut std::io::sink()).map_err(fail)?;
            if left.get() == 0 {
                return Err(fail(std::io::Error::other("limit")));
            }
            continue;
        }
        if !entry.header().entry_type().is_file() {
            return Err(format!("{RELEASE_BIN} 가 일반 파일이 아님"));
        }
        if bin.is_some() {
            return Err(format!("{RELEASE_BIN} 가 둘 이상"));
        }
        let mut data = Vec::new();
        (&mut entry).take(MAX_ARCHIVE + 1).read_to_end(&mut data).map_err(fail)?;
        if data.len() as u64 > MAX_ARCHIVE {
            return Err(format!("{RELEASE_BIN} exceeds {MAX_ARCHIVE} bytes"));
        }
        if data.is_empty() {
            return Err(format!("{RELEASE_BIN} 가 비었음"));
        }
        bin = Some(data);
    }
    if left.get() == 0 {
        return Err(fail(std::io::Error::other("limit")));
    }
    bin.ok_or_else(|| format!("묶음에 {RELEASE_BIN} 가 없음"))
}

// ─── db update ───

/// db update 가 쓰는 바깥 것이다. 시험은 가짜 서버·임시 파일로 바꾼다.
pub struct Updater {
    pub src: Source,
    pub repo: String,
    pub current: String,
    pub os: String,
    pub arch: String,
    pub exe: Result<PathBuf, String>,
}

/// Go GOOS·GOARCH 이름이다(릴리스 묶음 이름에 쓴다).
fn go_os_arch() -> (String, String) {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        o => o,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        a => a,
    };
    (os.into(), arch.into())
}

pub fn default_updater() -> Updater {
    let (os, arch) = go_os_arch();
    Updater {
        src: Source {
            api: String::new(),
            token: std::env::var("GITHUB_TOKEN").unwrap_or_default(),
        },
        repo: DEFAULT_REPO.into(),
        current: version().into(),
        os,
        arch,
        exe: std::env::current_exe().map_err(|e| e.to_string()),
    }
}

fn flag_set() -> FlagSet {
    let def = |name, ty, usage: &str, default: Option<&str>| FlagDef {
        name,
        usage: usage.into(),
        ty,
        default: default.map(String::from),
    };
    FlagSet {
        defs: vec![
            def("check", FlagType::Bool, "바꾸지 않고 새 판 여부만 보임", None),
            def("version", FlagType::Str, "바꿀 판(기본 최신)", None),
            def("force", FlagType::Bool, "낮은 판·소스 빌드도 바꿈", None),
            def("timeout", FlagType::Duration, "전체 시간 제한", Some("2m0s")),
        ],
        check: |_, _| Ok(()),
    }
}

pub fn run_update(u: &Updater, args: &[String], stdout: &mut String, stderr: &mut String) -> i32 {
    let fs = flag_set();
    let mut p = Parsed::default();
    let pos = match fs.parse_interleaved(&mut p, args, stderr, &|out: &mut String| out.push_str(UPDATE_USAGE)) {
        Ok(pos) => pos,
        Err(ParseError::Help) => return EXIT_OK,
        Err(ParseError::Failed) => return EXIT_USAGE,
    };
    if let Some(a) = pos.first() {
        stderr.push_str(&format!("db update: 모르는 인자 {}\n", go_quote(a)));
        return EXIT_USAGE;
    }
    let check = p.bools.get("check").copied().unwrap_or(false);
    let force = p.bools.get("force").copied().unwrap_or(false);
    let want = p.strs.get("version").cloned().unwrap_or_default();
    let timeout = p.durs.get("timeout").copied().unwrap_or(120_000_000_000);
    if !want.is_empty() && !valid(&want) {
        stderr.push_str(&format!("db update: --version 은 vX.Y.Z 꼴: {}\n", go_quote(&want)));
        return EXIT_USAGE;
    }
    if u.os != "linux" && u.os != "darwin" {
        stderr.push_str(&format!("db update: 이 OS 는 릴리스가 없음: {} (linux·darwin 만)\n", u.os));
        return EXIT_ERR;
    }
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            stderr.push_str(&format!("{e}\n"));
            return EXIT_ERR;
        }
    };
    // Go 는 0 이하 기간이면 ctx 가 바로 끝난다. 여기서도 바로 시간 초과로 본다
    let timeout = Duration::from_nanos(timeout.max(0) as u64);
    rt.block_on(async {
        match tokio::time::timeout(timeout, update(u, check, force, &want, stdout, stderr)).await {
            Ok(code) => code,
            Err(_) => {
                stderr.push_str("db update: context deadline exceeded\n");
                EXIT_ERR
            }
        }
    })
}

async fn update(u: &Updater, check: bool, force: bool, want: &str, stdout: &mut String, stderr: &mut String) -> i32 {
    let rel = match u.src.release(&u.repo, want).await {
        Ok(r) => r,
        Err(e) => {
            stderr.push_str(&format!("db update: {e}\n"));
            return EXIT_ERR;
        }
    };
    // rel.tag 는 판 꼴만 오므로 비교할 수 없으면 지금 판이 소스 빌드(dev)다
    let c = compare(&rel.tag, &u.current);
    if check {
        return match c {
            None => {
                stderr.push_str(&format!("db update: 소스 빌드({})는 판을 비교할 수 없음\n", u.current));
                EXIT_ERR
            }
            Some(c) if c > 0 => {
                stdout.push_str(&format!("새 판 {} → {}\n", u.current, rel.tag));
                EXIT_NEWER
            }
            Some(_) => {
                stdout.push_str(&format!("이미 최신 ({})\n", u.current));
                EXIT_OK
            }
        };
    }
    if !force {
        match c {
            None => {
                stderr.push_str(&format!("db update: 소스 빌드({})는 --force 로 바꿈\n", u.current));
                return EXIT_REFUSED;
            }
            Some(0) if !want.is_empty() => {
                stdout.push_str(&format!("이미 {}\n", u.current));
                return EXIT_OK;
            }
            Some(c) if c <= 0 && want.is_empty() => {
                stdout.push_str(&format!("이미 최신 ({})\n", u.current));
                return EXIT_OK;
            }
            Some(c) if c < 0 => {
                stderr.push_str(&format!("db update: 낮은 판으로 내리기({} → {})는 --force\n", u.current, rel.tag));
                return EXIT_REFUSED;
            }
            _ => {}
        }
    }
    let exe = match &u.exe {
        Ok(p) => p,
        Err(e) => {
            stderr.push_str(&format!("db update: {e}\n"));
            return EXIT_ERR;
        }
    };
    if let Err((e, not_writable)) = u.src.apply(&rel, &u.os, &u.arch, exe).await {
        if not_writable {
            stderr.push_str(&format!("{e}\n"));
        } else {
            stderr.push_str(&format!("db update: {e}\n"));
        }
        return EXIT_ERR;
    }
    stdout.push_str(&format!("{} → {}\n", u.current, rel.tag));
    EXIT_OK
}

#[cfg(test)]
mod tests;
