#![allow(non_snake_case)]
//! Go 판이 만든 골든 출력(testdata/golden/out)과 db-rs 출력을 바이트로 비교한다.
//! 형식·자리 규칙은 레포 루트 golden_test.go 머리 주석과 같다. rust-ported.txt 에 적은 케이스만 통과를 요구하고,
//! 적지 않은 케이스가 이미 맞으면 옮겨 적으라고 알린다.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Deserialize;

#[derive(Deserialize)]
struct Case {
    name: String,
    project: String,
    args: Vec<String>,
    #[serde(default)]
    stdin: String,
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

fn live_vars() -> Option<HashMap<&'static str, String>> {
    let raw = std::env::var("DBTUI_TEST_PG_URL").ok()?;
    let addr = std::env::var("DBTUI_TEST_REDIS_ADDR").ok()?;
    let u = url::Url::parse(&raw).ok()?;
    let (rhost, rport) = addr.rsplit_once(':')?;
    // Go 러너처럼 URL 의 값을 풀었다가 비밀번호만 url.QueryEscape 로 다시 싼다(.env 로더가 QueryUnescape 로 읽는다)
    let unescape = |s: &str| percent_decode(s);
    let pw = go_query_escape(&unescape(u.password().unwrap_or_default()));
    Some(HashMap::from([
        ("PG_HOST", u.host_str()?.to_string()),
        ("PG_PORT", u.port().map(|p| p.to_string()).unwrap_or_default()),
        ("PG_DB", unescape(u.path().trim_start_matches('/'))),
        ("PG_USER", unescape(u.username())),
        ("PG_PASSWORD", pw),
        ("REDIS_HOST", rhost.to_string()),
        ("REDIS_PORT", rport.to_string()),
    ]))
}

fn copy_project(src: &Path, dst: &Path, vars: &HashMap<&'static str, String>) {
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let target = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            fs::create_dir_all(&target).unwrap();
            copy_project(&entry.path(), &target, vars);
            continue;
        }
        let mut text = fs::read_to_string(entry.path()).unwrap();
        // ${NAME} 꼴만 채운다. DataGrip 의 $PROJECT_DIR$ 는 그대로 둔다
        for (k, v) in vars {
            text = text.replace(&format!("${{{k}}}"), v);
        }
        fs::write(target, text).unwrap();
    }
}

fn normalize(s: &str, dir: &str) -> String {
    let s = s.replace(dir, "{P}");
    // "elapsed_ms":N → 0
    let mut out = String::with_capacity(s.len());
    let mut rest = s.as_str();
    while let Some(i) = rest.find("\"elapsed_ms\":") {
        let after = &rest[i + 13..];
        let digits = after.bytes().take_while(u8::is_ascii_digit).count();
        out.push_str(&rest[..i + 13]);
        out.push('0');
        rest = &after[digits..];
    }
    out.push_str(rest);
    out
}

/// URL 의 %XX 를 푼다(Go url.URL 의 Path·Username 처럼 이미 풀린 값을 얻는다).
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Go url.QueryEscape: 영숫자와 -_.~ 만 두고 공백은 +, 나머지는 %XX.
fn go_query_escape(s: &str) -> String {
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

#[test]
fn 골든_케이스의_출력이_Go_판과_바이트로_같다() {
    let root = root();
    let golden = root.join("testdata/golden");
    let cases: Vec<Case> = serde_json::from_slice(&fs::read(golden.join("cases.json")).unwrap()).unwrap();
    let ported: Vec<String> = fs::read_to_string(golden.join("rust-ported.txt"))
        .unwrap()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect();
    for p in &ported {
        assert!(cases.iter().any(|c| &c.name == p), "rust-ported.txt names unknown case {p}");
    }
    let live = live_vars();
    let tmp = std::env::temp_dir().join(format!("db-rs-golden-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    let state = tmp.join("state");
    fs::create_dir_all(&state).unwrap();
    let mut dirs: HashMap<String, String> = HashMap::new();
    let (mut failures, mut extra) = (Vec::new(), Vec::new());
    for c in &cases {
        let listed = ported.contains(&c.name);
        if c.project == "live" && live.is_none() {
            continue;
        }
        let dir = dirs
            .entry(c.project.clone())
            .or_insert_with(|| {
                let d = tmp.join(&c.project);
                fs::create_dir_all(&d).unwrap();
                copy_project(&golden.join(&c.project), &d, live.as_ref().unwrap_or(&HashMap::new()));
                d.to_string_lossy().into_owned()
            })
            .clone();
        let args: Vec<String> = c.args.iter().map(|a| a.replace("{P}", &dir)).collect();
        let mut child = Command::new(env!("CARGO_BIN_EXE_db-rs"))
            .args(&args)
            .env("XDG_STATE_HOME", &state)
            .env("HOME", &state)
            .env("DBTUI_PROJECT", "/golden/project")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(c.stdin.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        let got = format!(
            "args: {}\nexit: {}\n--- stdout\n{}--- stderr\n{}",
            go_json_args(&c.args),
            out.status.code().unwrap_or(-1),
            normalize(&String::from_utf8_lossy(&out.stdout), &dir),
            normalize(&String::from_utf8_lossy(&out.stderr), &dir),
        );
        let want = fs::read_to_string(golden.join("out").join(format!("{}.txt", c.name))).unwrap();
        match (got == want, listed) {
            (false, true) => failures.push(format!("{}\n--- want\n{want}\n--- got\n{got}", c.name)),
            (true, false) => extra.push(c.name.clone()),
            _ => {}
        }
    }
    let _ = fs::remove_dir_all(&tmp);
    if !extra.is_empty() {
        eprintln!("already matching, add to rust-ported.txt: {}", extra.join(" "));
    }
    assert!(
        failures.is_empty(),
        "{} golden mismatches:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

/// Go json.Marshal([]string) 와 같은 글(golden 첫 줄).
fn go_json_args(args: &[String]) -> String {
    let mut s = String::from("[");
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(
            &serde_json::to_string(a)
                .unwrap()
                .replace('<', "\\u003c")
                .replace('>', "\\u003e")
                .replace('&', "\\u0026"),
        );
    }
    s.push(']');
    s
}
