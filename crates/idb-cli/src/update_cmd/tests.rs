use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};

use super::*;

// ─── 가짜 HTTP 서버 ───

#[derive(Clone, Debug, Default)]
struct Req {
    path: String,
    headers: HashMap<String, String>,
}

struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn ok(body: impl Into<Vec<u8>>) -> Resp {
    Resp {
        status: 200,
        headers: Vec::new(),
        body: body.into(),
    }
}

fn status(code: u16) -> Resp {
    Resp {
        status: code,
        headers: Vec::new(),
        body: Vec::new(),
    }
}

type Handler = Arc<dyn Fn(&Req, &str) -> Resp + Send + Sync>;

/// 요청마다 연결 하나로 답하고 닫는 작은 HTTP/1.1 서버다. handler 는 (요청, 서버 주소) 를 받는다. 받은 요청을 log 에 남긴다.
struct Server {
    url: String,
    log: Arc<Mutex<Vec<Req>>>,
}

fn serve(handler: Handler) -> Server {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    let log = Arc::new(Mutex::new(Vec::new()));
    let (log2, base) = (log.clone(), url.clone());
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { continue };
            let (handler, log, base) = (handler.clone(), log2.clone(), base.clone());
            std::thread::spawn(move || {
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut line = String::new();
                if r.read_line(&mut line).is_err() {
                    return;
                }
                let path = line.split(' ').nth(1).unwrap_or("").to_string();
                let mut req = Req {
                    path,
                    ..Default::default()
                };
                loop {
                    let mut h = String::new();
                    if r.read_line(&mut h).is_err() || h == "\r\n" || h.is_empty() {
                        break;
                    }
                    if let Some((k, v)) = h.trim_end().split_once(':') {
                        req.headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                    }
                }
                log.lock().unwrap().push(req.clone());
                let resp = handler(&req, &base);
                let mut head = format!(
                    "HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n",
                    status_text(resp.status),
                    resp.body.len()
                );
                for (k, v) in &resp.headers {
                    head.push_str(&format!("{k}: {v}\r\n"));
                }
                head.push_str("\r\n");
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(&resp.body);
            });
        }
    });
    Server { url, log }
}

fn tar_gz(entries: &[(&str, &str, tar::EntryType)]) -> Vec<u8> {
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut tw = tar::Builder::new(gz);
    for (name, body, typ) in entries {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(*typ);
        h.set_mode(0o755);
        if *typ == tar::EntryType::Symlink {
            h.set_size(0);
            h.set_link_name("/bin/sh").unwrap();
            tw.append_data(&mut h, name, std::io::empty()).unwrap();
        } else {
            h.set_size(body.len() as u64);
            // set_path 는 .. 를 거절해 머리에 바로 쓴다
            let raw = h.as_old_mut();
            raw.name[..name.len()].copy_from_slice(name.as_bytes());
            h.set_cksum();
            tw.append(&h, body.as_bytes()).unwrap();
        }
    }
    tw.into_inner().unwrap().finish().unwrap()
}

fn reg(name: &'static str, body: &'static str) -> (&'static str, &'static str, tar::EntryType) {
    (name, body, tar::EntryType::Regular)
}

const ARCHIVE: &str = "db_linux_amd64.tar.gz";

fn sha_hex(b: &[u8]) -> String {
    Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect()
}

/// releases API 와 내려받기를 흉내 낸다. tags 는 판 → 묶음 바이트, sums 가 비면 바른 checksums.txt 를 만든다.
fn fake_github(latest: &str, tags: HashMap<String, Vec<u8>>, sums: Option<String>) -> Server {
    let latest = latest.to_string();
    serve(Arc::new(move |r: &Req, base: &str| {
        let release = |tag: &str| {
            if r.headers.get("accept").map(String::as_str) != Some("application/vnd.github+json")
                || r.headers.get("user-agent").map(String::as_str) != Some("db-selfupdate")
            {
                return status(400);
            }
            if !tags.contains_key(tag) {
                return status(404);
            }
            let dl = format!("{base}/dl/{tag}/");
            ok(serde_json::json!({"tag_name": tag, "assets": [
                {"name": ARCHIVE, "browser_download_url": format!("{dl}{ARCHIVE}")},
                {"name": "checksums.txt", "browser_download_url": format!("{dl}checksums.txt")},
            ]})
            .to_string())
        };
        if r.path == "/repos/o/r/releases/latest" {
            return release(&latest);
        }
        if let Some(tag) = r.path.strip_prefix("/repos/o/r/releases/tags/") {
            return release(tag);
        }
        if let Some(rest) = r.path.strip_prefix("/dl/") {
            let (tag, name) = rest.split_once('/').unwrap();
            let data = tags.get(tag).cloned().unwrap_or_default();
            return match name {
                ARCHIVE => ok(data),
                "checksums.txt" => ok(sums.clone().unwrap_or_else(|| {
                    format!(
                        "0000000000000000000000000000000000000000000000000000000000000000  ./db_darwin_arm64.tar.gz\n{}  ./{ARCHIVE}\n",
                        sha_hex(&data)
                    )
                })),
                _ => status(404),
            };
        }
        status(404)
    }))
}

fn source(s: &Server, token: &str) -> Source {
    Source {
        api: s.url.clone(),
        token: token.into(),
    }
}

fn write_exe(dir: &Path, perm: u32) -> PathBuf {
    let p = dir.join("db");
    std::fs::write(&p, "old").unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(perm)).unwrap();
    p
}

fn block<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(f)
}

fn tags(list: &[(&str, Vec<u8>)]) -> HashMap<String, Vec<u8>> {
    list.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
}

// ─── 판 비교 ───

#[test]
fn 판_비교표() {
    let cases = [
        ("v0.12.0", "v0.11.0", Some(1)),
        ("v0.9.0", "v0.12.0", Some(-1)),
        ("v1.0.0", "v0.99.99", Some(1)),
        ("v0.12.0", "v0.12.0", Some(0)),
        ("v0.12.0", "v0.12.0-rc1", Some(1)),
        ("v0.12.0-rc1", "v0.12.0-rc2", Some(-1)),
        ("v0.12.0+meta", "v0.12.0", Some(0)),
        ("v0.0.0-20261115000000-abcdef", "v0.1.0", Some(-1)),
        ("dev", "v0.1.0", None),
        ("0.12.0", "v0.1.0", None),
        ("v1.2", "v1.2.0", None),
        ("v1.x.0", "v1.2.0", None),
        ("v1.0.0-rc2", "v1.0.0-rc10", Some(1)),
        ("v1.0.0-rc.2", "v1.0.0-rc.10", Some(-1)),
        ("v1.0.0-1", "v1.0.0-alpha", Some(-1)),
        ("v1.0.0-alpha", "v1.0.0-alpha.1", Some(-1)),
        ("v1.0.0-alpha.1", "v1.0.0-alpha.beta", Some(-1)),
        ("v1.0.0-beta.11", "v1.0.0-rc.1", Some(-1)),
        ("v1.0.0-2", "v1.0.0-10", Some(-1)),
        ("v1.0.0-01", "v1.0.0", None),
        ("v01.0.0", "v1.0.0", None),
        ("v1.0.0-", "v1.0.0", None),
        ("v1.0.0-a..b", "v1.0.0", None),
        ("v1.0.0-a\x1b", "v1.0.0", None),
        ("v1.0.0+b\x1b", "v1.0.0", None),
        ("v1.0.0+001", "v1.0.0", Some(0)),
        ("v+1.0.0", "v1.0.0", None),
    ];
    for (a, b, want) in cases {
        assert_eq!(compare(a, b), want, "{a} {b}");
    }
}

// ─── 릴리스·바꾸기 ───

#[test]
fn 최신과_태그를_읽고_토큰은_API_에만_붙인다() {
    let gh = fake_github(
        "v0.12.0",
        tags(&[("v0.12.0", tar_gz(&[reg("db", "new")])), ("v0.11.0", tar_gz(&[reg("db", "older")]))]),
        None,
    );
    let rel = block(source(&gh, "tok").release("o/r", "")).unwrap();
    assert_eq!(rel.tag, "v0.12.0");
    assert!(rel.assets.contains_key(ARCHIVE) && rel.assets.contains_key("checksums.txt"));
    assert_eq!(block(source(&gh, "").release("o/r", "v0.11.0")).unwrap().tag, "v0.11.0");
    let err = block(source(&gh, "").release("o/r", "v9.9.9")).unwrap_err();
    assert!(err.contains("릴리스가 없음: v9.9.9"), "{err}");
    {
        let log = gh.log.lock().unwrap();
        assert_eq!(log[0].headers.get("authorization").map(String::as_str), Some("Bearer tok"));
        assert_eq!(log[1].headers.get("authorization"), None);
    }
    let dir = tempfile::tempdir().unwrap();
    let exe = write_exe(dir.path(), 0o755);
    let rel = block(source(&gh, "tok").release("o/r", "")).unwrap();
    block(source(&gh, "tok").apply(&rel, "linux", "amd64", &exe)).unwrap();
    for r in gh.log.lock().unwrap().iter().filter(|r| r.path.starts_with("/dl/")) {
        assert_eq!(r.headers.get("authorization"), None, "내려받기에 토큰이 붙음");
    }
}

#[test]
fn 검증한_db_로_바꾸고_권한과_링크를_지킨다() {
    let gh = fake_github(
        "v0.12.0",
        tags(&[("v0.12.0", tar_gz(&[reg("README", "x"), reg("./db", "new")]))]),
        None,
    );
    let rel = block(source(&gh, "").release("o/r", "")).unwrap();
    let (real, links) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let target = write_exe(real.path(), 0o750);
    let link = links.path().join("db");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    block(source(&gh, "").apply(&rel, "linux", "amd64", &link)).unwrap();
    assert!(
        std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(),
        "링크가 그대로여야 한다"
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
    assert_eq!(std::fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o750);
    assert_eq!(std::fs::read_dir(real.path()).unwrap().count(), 1, "다른 파일이 남음");
    let err = block(source(&gh, "").apply(&rel, "darwin", "amd64", &link)).unwrap_err();
    assert!(err.0.contains("db_darwin_amd64.tar.gz"), "{err:?}");
}

#[test]
fn 체크섬이_다르거나_db_가_이상하면_아무것도_바꾸지_않는다() {
    let cases: Vec<(&str, Vec<u8>, Option<String>, &str)> = vec![
        (
            "체크섬 다름",
            tar_gz(&[reg("db", "evil")]),
            Some(format!("{}  ./{ARCHIVE}\n", "ab".repeat(32))),
            "checksum mismatch",
        ),
        (
            "체크섬 줄 없음",
            tar_gz(&[reg("db", "new")]),
            Some(format!("{}  ./other.tar.gz\n", "ab".repeat(32))),
            "checksums.txt 에 db_linux_amd64.tar.gz 가 없음",
        ),
        ("db 없음", tar_gz(&[reg("README", "x")]), None, "묶음에 db 가 없음"),
        (
            "다른 경로의 db",
            tar_gz(&[reg("../db", "x"), reg("sub/db", "x")]),
            None,
            "묶음에 db 가 없음",
        ),
        (
            "심볼릭 링크 db",
            tar_gz(&[("db", "", tar::EntryType::Symlink)]),
            None,
            "db 가 일반 파일이 아님",
        ),
        ("db 둘", tar_gz(&[reg("db", "a"), reg("./db", "b")]), None, "db 가 둘 이상"),
        ("빈 db", tar_gz(&[reg("db", "")]), None, "db 가 비었음"),
        (
            "gzip 아님",
            b"this is definitely not a gzip stream".to_vec(),
            None,
            "gzip: invalid header",
        ),
    ];
    for (name, archive, sums, want) in cases {
        let gh = fake_github("v0.12.0", tags(&[("v0.12.0", archive)]), sums);
        let rel = block(source(&gh, "").release("o/r", "")).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let exe = write_exe(dir.path(), 0o755);
        let err = block(source(&gh, "").apply(&rel, "linux", "amd64", &exe)).unwrap_err();
        assert!(err.0.contains(want), "{name}: {err:?}");
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old", "{name}");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1, "{name}");
    }
}

#[test]
fn 쓸_권한이_없으면_경로와_sudo_를_알린다() {
    let gh = fake_github("v0.12.0", tags(&[("v0.12.0", tar_gz(&[reg("db", "new")]))]), None);
    let rel = block(source(&gh, "").release("o/r", "")).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let exe = write_exe(dir.path(), 0o755);
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
    if std::fs::write(dir.path().join("probe"), "").is_ok() {
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        return; // root 는 권한 검사를 받지 않는다
    }
    let err = block(source(&gh, "").apply(&rel, "linux", "amd64", &exe)).unwrap_err();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(err, (format!("쓸 수 없음: {} (sudo 로 다시)", exe.display()), true));
    assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old");
}

#[test]
fn 기본은_https_와_GitHub_호스트만_받는다() {
    for (u, want) in [
        ("https://api.github.com/repos/o/r", true),
        ("https://github.com/o/r/releases/download/v1.0.0/x", true),
        ("https://objects.githubusercontent.com/x", true),
        ("http://github.com/x", false),
        ("https://evil.example/x", false),
        ("https://githubusercontent.com.evil.example/x", false),
    ] {
        assert_eq!(github_url(&url::Url::parse(u).unwrap()), want, "{u}");
    }
}

#[test]
fn 다른_호스트로_넘어가면_토큰을_보내지_않는다() {
    let other = serve(Arc::new(|_: &Req, _: &str| ok(r#"{"tag_name":"v1.0.0"}"#)));
    let target = format!("{}/latest", other.url);
    let api = serve(Arc::new(move |_: &Req, _: &str| Resp {
        status: 302,
        headers: vec![("Location".into(), target.clone())],
        body: Vec::new(),
    }));
    block(source(&api, "tok").release("o/r", "")).unwrap();
    let got = other.log.lock().unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].headers.get("authorization"), None);
}

#[test]
fn https_가_아니거나_주입한_주소_밖이면_받지_않는다() {
    let gh = fake_github("v0.12.0", tags(&[("v0.12.0", tar_gz(&[reg("db", "new")]))]), None);
    let dir = tempfile::tempdir().unwrap();
    let exe = write_exe(dir.path(), 0o755);
    for src in [source(&gh, ""), Source::default()] {
        let rel = Release {
            tag: "v0.12.0".into(),
            assets: HashMap::from([
                (ARCHIVE.to_string(), format!("http://127.0.0.1:1/{ARCHIVE}")),
                ("checksums.txt".to_string(), "http://127.0.0.1:1/checksums.txt".to_string()),
            ]),
        };
        let err = block(src.apply(&rel, "linux", "amd64", &exe)).unwrap_err();
        assert!(err.0.contains("refuse download"), "{err:?}");
    }
}

#[test]
fn 판_꼴이_아니거나_요청과_다른_태그는_거절한다() {
    let tag = Arc::new(Mutex::new("v1.0.0".to_string()));
    let t2 = tag.clone();
    let srv = serve(Arc::new(move |_: &Req, _: &str| {
        ok(serde_json::json!({"tag_name": *t2.lock().unwrap()}).to_string())
    }));
    let src = source(&srv, "");
    assert!(block(src.release("o/r", "v1.1.0")).unwrap_err().contains("does not match"));
    *tag.lock().unwrap() = "v1.0.0-\x1b]0;x\x07".into();
    let err = block(src.release("o/r", "")).unwrap_err();
    assert!(err.contains("not a version") && !err.contains('\x1b'), "{err:?}");
    let before = srv.log.lock().unwrap().len();
    assert!(block(src.release("o/r", "v1.0.0/../x")).is_err());
    assert_eq!(
        srv.log.lock().unwrap().len(),
        before,
        "판 꼴이 아닌 요청은 주소를 만들기 전에 거절한다"
    );
}

#[test]
fn 압축_풀린_전체_크기를_넘으면_다른_항목을_건너뛰다가도_멈춘다() {
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut tw = tar::Builder::new(gz);
    let big = 2 * MAX_ARCHIVE + (1 << 20);
    let mut h = tar::Header::new_gnu();
    h.set_size(big);
    h.set_mode(0o644);
    h.set_cksum();
    tw.append_data(&mut h, "pad", std::io::repeat(0).take(big)).unwrap();
    let mut h = tar::Header::new_gnu();
    h.set_size(3);
    h.set_mode(0o755);
    tw.append_data(&mut h, "db", &b"new"[..]).unwrap();
    let data = tw.into_inner().unwrap().finish().unwrap();
    let err = extract_bin(&data).unwrap_err();
    assert!(err.contains("uncompressed"), "{err}");
}

// ─── db update ───

fn updater(current: &str, bad_sum: bool) -> (Updater, PathBuf, tempfile::TempDir, Server) {
    let archives = tags(&[
        ("v0.11.0", tar_gz(&[reg("db", "bin-v0.11.0")])),
        ("v0.12.0", tar_gz(&[reg("db", "bin-v0.12.0")])),
    ]);
    let srv = serve(Arc::new(move |r: &Req, base: &str| {
        let release = |tag: &str| {
            if !archives.contains_key(tag) {
                return status(404);
            }
            ok(serde_json::json!({"tag_name": tag, "assets": [
                {"name": ARCHIVE, "browser_download_url": format!("{base}/dl/{tag}/a")},
                {"name": "checksums.txt", "browser_download_url": format!("{base}/dl/{tag}/sums")},
            ]})
            .to_string())
        };
        if r.path == "/repos/o/r/releases/latest" {
            return release("v0.12.0");
        }
        if let Some(tag) = r.path.strip_prefix("/repos/o/r/releases/tags/") {
            return release(tag);
        }
        let rest = r.path.strip_prefix("/dl/").unwrap_or("");
        let (tag, name) = rest.split_once('/').unwrap_or(("", ""));
        let data = archives.get(tag).cloned().unwrap_or_default();
        match name {
            "a" => ok(data),
            "sums" => ok(format!(
                "{}  ./{ARCHIVE}\n",
                if bad_sum { sha_hex(b"other") } else { sha_hex(&data) }
            )),
            _ => status(404),
        }
    }));
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("db");
    std::fs::write(&exe, format!("bin-{current}")).unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    let u = Updater {
        src: source(&srv, ""),
        repo: "o/r".into(),
        current: current.into(),
        os: "linux".into(),
        arch: "amd64".into(),
        exe: Ok(exe.clone()),
    };
    (u, exe, dir, srv)
}

fn run(u: &Updater, args: &[&str]) -> (i32, String, String) {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let (mut out, mut err) = (String::new(), String::new());
    let code = run_update(u, &args, &mut out, &mut err);
    (code, out, err)
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap()
}

#[test]
fn check_는_새_판이면_10_최신이면_0() {
    for (current, code, out) in [
        ("v0.11.0", EXIT_NEWER, "새 판 v0.11.0 → v0.12.0\n"),
        ("v0.12.0", EXIT_OK, "이미 최신 (v0.12.0)\n"),
        ("v0.13.0", EXIT_OK, "이미 최신 (v0.13.0)\n"),
    ] {
        let (u, exe, _d, _s) = updater(current, false);
        let got = run(&u, &["--check"]);
        assert_eq!((got.0, got.1.as_str()), (code, out), "{current}: {}", got.2);
        assert_eq!(read(&exe), format!("bin-{current}"), "--check 는 바꾸지 않는다");
    }
}

#[test]
fn check_는_소스_빌드면_새_판이라_하지_않고_오류() {
    let (u, _e, _d, _s) = updater("dev", false);
    let (code, out, err) = run(&u, &["--check"]);
    assert!(
        code == EXIT_ERR && out.is_empty() && err.contains("소스 빌드(dev)는 판을 비교할 수 없음"),
        "{err}"
    );
}

#[test]
fn 받은_태그가_판_꼴이_아니면_소스_빌드_안내가_아닌_오류() {
    let srv = serve(Arc::new(|_: &Req, _: &str| ok("{\"tag_name\":\"latest\\u001b[2J\"}")));
    let (mut u, exe, _d, _s) = updater("v0.11.0", false);
    u.src = source(&srv, "");
    for args in [&[][..], &["--check"]] {
        let (code, out, err) = run(&u, args);
        assert!(
            code == EXIT_ERR && out.is_empty() && err.contains("not a version") && !err.contains("소스 빌드"),
            "{err}"
        );
    }
    assert_eq!(read(&exe), "bin-v0.11.0");
}

#[test]
fn 최신_판으로_바꾸고_전후_판을_보인다() {
    let (u, exe, _d, _s) = updater("v0.11.0", false);
    let (code, out, err) = run(&u, &[]);
    assert_eq!((code, out.as_str()), (EXIT_OK, "v0.11.0 → v0.12.0\n"), "{err}");
    assert_eq!(read(&exe), "bin-v0.12.0");
    let (u, _e, _d, _s) = updater("v0.12.0", false);
    assert_eq!(run(&u, &[]).1, "이미 최신 (v0.12.0)\n");
}

#[test]
fn 내리기와_소스_빌드는_force_가_있어야_바꾼다() {
    for (current, args, want, after) in [
        (
            "v0.12.0",
            vec!["--version", "v0.11.0"],
            "낮은 판으로 내리기(v0.12.0 → v0.11.0)는 --force",
            "bin-v0.11.0",
        ),
        ("dev", vec![], "소스 빌드(dev)는 --force 로 바꿈", "bin-v0.12.0"),
    ] {
        let (u, exe, _d, _s) = updater(current, false);
        let (code, _, err) = run(&u, &args);
        assert!(code == EXIT_REFUSED && err.contains(want), "{current}: {code} {err}");
        assert_eq!(read(&exe), format!("bin-{current}"));
        let mut forced = args.clone();
        forced.push("--force");
        let (code, _, err) = run(&u, &forced);
        assert_eq!(code, EXIT_OK, "{err}");
        assert_eq!(read(&exe), after);
    }
}

#[test]
fn 실패하면_바꾸지_않고_종료_코드를_가른다() {
    let (u, exe, _d, _s) = updater("v0.11.0", true);
    let (code, _, err) = run(&u, &[]);
    assert!(code == EXIT_ERR && err.contains("checksum mismatch"), "{err}");
    assert_eq!(read(&exe), "bin-v0.11.0");
    let (mut u, _e, _d, _s) = updater("v0.11.0", false);
    let (code, _, err) = run(&u, &["--version", "v9.9.9"]);
    assert!(code == EXIT_ERR && err.contains("릴리스가 없음: v9.9.9"), "{err}");
    assert_eq!(run(&u, &["--version", "latest"]).0, EXIT_USAGE);
    assert_eq!(run(&u, &["extra"]).0, EXIT_USAGE);
    u.os = "windows".into();
    let (code, _, err) = run(&u, &[]);
    assert!(code == EXIT_ERR && err.contains("이 OS 는 릴리스가 없음: windows"), "{err}");
}

#[test]
fn 쓸_권한이_없으면_sudo_를_권한다() {
    let (u, exe, dir, _s) = updater("v0.11.0", false);
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
    if std::fs::write(dir.path().join("probe"), "").is_ok() {
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        return; // root
    }
    let (code, _, err) = run(&u, &[]);
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!((code, err), (EXIT_ERR, format!("쓸 수 없음: {} (sudo 로 다시)\n", exe.display())));
}

#[test]
fn 도움말은_stderr_에_사용법이다() {
    let (u, _e, _d, _s) = updater("v0.11.0", false);
    let (code, _, err) = run(&u, &["-h"]);
    assert!(code == EXIT_OK && err.contains("db update [--check]"), "{err}");
}
