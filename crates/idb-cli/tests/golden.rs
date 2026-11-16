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
    // Go url.QueryEscape 와 같게 비밀번호를 다시 인코딩한다(파일에는 인코딩된 값이 든다)
    let pw = u.password().unwrap_or_default().to_string();
    Some(HashMap::from([
        ("PG_HOST", u.host_str()?.to_string()),
        ("PG_PORT", u.port().map(|p| p.to_string()).unwrap_or_default()),
        ("PG_DB", u.path().trim_start_matches('/').to_string()),
        ("PG_USER", u.username().to_string()),
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

#[test]
fn golden() {
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
