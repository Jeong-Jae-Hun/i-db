#![allow(non_snake_case)]
//! `db mcp` 표준 입출력 왕복 시험. JSON-RPC 줄을 보내 initialize·tools/list·tools/call 이 MCP 클라이언트(Claude)가
//! 보는 꼴로 오는지 본다. tools/list 는 Go 판이 내던 목록(src/mcp/tools.json)과 같아야 한다.
//! Go `billed_test.go`(요금 막음, R72)도 여기서 옮긴다. 실제 AWS 저장소를 열지만 요청을 만들기 전에 막혀 네트워크에 나가지 않는다.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};

const PG_PW: &str = "Mcp-Sekrit-77";
const BILLED: &str = "요금이 붙는 조회라 막음 (DB_ALLOW_BILLED=1 로 허용)";

/// 닿지 않는 PostgreSQL 둘(local·prod)과 AWS 저장소 셋을 가진 db 전용 접속 프로젝트다.
fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".idea")).unwrap();
    fs::write(
        dir.path().join(".idea/dataSources.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?><project version="4"><component name="DataSourceManagerImpl"></component></project>"#,
    )
    .unwrap();
    fs::create_dir_all(dir.path().join(".dbtui")).unwrap();
    fs::write(
        dir.path().join(".dbtui/connections.json"),
        r#"[
{"uuid":"p1","name":"app-local","group":"app","kind":"postgresql","host":"127.0.0.1","port":1,"db":"app","user":"u"},
{"uuid":"p2","name":"app-prod","kind":"postgresql","host":"127.0.0.1","port":1,"db":"app","user":"u"},
{"uuid":"a","name":"athena-x","kind":"athena","region":"ap-northeast-2"},
{"uuid":"d","name":"ddb-x","kind":"dynamodb","region":"ap-northeast-2"},
{"uuid":"w","name":"aws-x","kind":"aws","region":"ap-northeast-2","profile":"infra"}
]"#,
    )
    .unwrap();
    fs::write(
        dir.path().join(".env"),
        format!("PG_APP_LOCAL_PASSWORD={PG_PW}\nPG_APP_PROD_PASSWORD={PG_PW}\n"),
    )
    .unwrap();
    dir
}

struct Mcp {
    child: Child,
    stdin: Option<ChildStdin>,
    out: BufReader<ChildStdout>,
    next: u64,
    /// 받은 모든 줄(비밀 검사용)
    lines: Vec<String>,
}

impl Mcp {
    /// 자격 증명 파일은 비워 SDK 가 이 기계 설정을 읽지 않게 한다(막힘 판정은 요청을 만들기 전이다).
    fn start(dir: &Path, args: &[&str], remote: bool) -> Mcp {
        let bin = env!("CARGO_BIN_EXE_db");
        let mut cmd = Command::new(bin);
        cmd.arg("mcp")
            .args(args)
            .env_remove("DB_ALLOW_BILLED")
            .env("AWS_ACCESS_KEY_ID", "x")
            .env("AWS_SECRET_ACCESS_KEY", "x")
            .env_remove("AWS_PROFILE")
            .env("AWS_CONFIG_FILE", dir.join("none"))
            .env("AWS_SHARED_CREDENTIALS_FILE", dir.join("none"))
            .env("AWS_EC2_METADATA_DISABLED", "true")
            .env("XDG_STATE_HOME", dir.join("state"))
            .env("HOME", dir.join("state"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if remote {
            cmd.env("DB_REMOTE_CMD", format!("{bin} serve -p {}", dir.display()));
        } else {
            cmd.args(["-p", dir.to_str().unwrap()]);
        }
        let mut child = cmd.spawn().unwrap();
        let (stdin, out) = (child.stdin.take().unwrap(), BufReader::new(child.stdout.take().unwrap()));
        let mut m = Mcp {
            child,
            stdin: Some(stdin),
            out,
            next: 1,
            lines: Vec::new(),
        };
        let init = m.request(
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "1"}}),
        );
        assert_eq!(init["result"]["serverInfo"], json!({"name": "db", "version": "v2"}), "{init}");
        assert!(init["result"]["capabilities"]["tools"].is_object(), "{init}");
        m.write(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        m
    }

    fn write(&mut self, v: Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{v}").unwrap();
        stdin.flush().unwrap();
    }

    /// 요청 하나를 보내고 같은 id 의 응답을 기다린다. 사이에 온 알림은 건너뛴다.
    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        self.write(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let mut line = String::new();
            assert!(self.out.read_line(&mut line).unwrap() > 0, "{method}: 응답 전에 끝났다");
            self.lines.push(line.clone());
            let v: Value = serde_json::from_str(&line).unwrap();
            if v["id"] == json!(id) {
                return v;
            }
        }
    }

    /// (본문 텍스트, isError)
    fn call(&mut self, name: &str, args: Value) -> (String, bool) {
        let r = self.request("tools/call", json!({"name": name, "arguments": args}));
        let res = &r["result"];
        let text = res["content"]
            .as_array()
            .unwrap_or_else(|| panic!("{name}: {r}"))
            .iter()
            .filter_map(|c| c["text"].as_str())
            .collect::<String>();
        (text, res["isError"] == json!(true))
    }

    /// 입력을 닫으면 끝나야 한다.
    fn finish(mut self) -> Vec<String> {
        drop(self.stdin.take());
        let status = self.child.wait().unwrap();
        assert!(status.success(), "입력을 닫으면 0 으로 끝나야 한다: {status}");
        self.lines
    }
}

fn tools_json() -> Value {
    serde_json::from_str(include_str!("../src/mcp/tools.json")).unwrap()
}

#[test]
fn 표준_입출력으로_도구_목록과_호출이_오가고_입력을_닫으면_끝난다() {
    let d = project();
    let mut m = Mcp::start(d.path(), &[], false);
    let list = m.request("tools/list", json!({}));
    assert_eq!(list["result"]["tools"], tools_json(), "Go 판 도구 목록과 같아야 한다");

    let (text, is_err) = m.call("list_connections", json!({}));
    assert!(!is_err, "{text}");
    let out: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        out["connections"][4],
        json!({"name": "aws-x", "group": "기타", "kind": "aws", "env": "local", "region": "ap-northeast-2", "profile": "infra"})
    );
    assert_eq!(out["connections"][0]["group"], "app");

    let (text, is_err) = m.call("query", json!({"connection": "app-prod", "sql": "select 1"}));
    assert!(is_err && text.contains("is prod"), "{text}");
    let (text, is_err) = m.call("query", json!({"connection": "app-local", "sql": "delete from t"}));
    assert!(is_err && text.contains("read-only"), "{text}");
    // 닿지 않는 포트라 접속 오류다. 오류 글에도 비밀번호는 없다
    let (text, is_err) = m.call("list_schemas", json!({"connection": "app-local"}));
    assert!(is_err && !text.is_empty(), "{text}");

    // 모르는 도구·스키마에 맞지 않는 인자는 Go SDK 처럼 JSON-RPC invalid params 다
    let r = m.request("tools/call", json!({"name": "drop_table", "arguments": {}}));
    assert_eq!(r["error"]["code"], -32602, "{r}");
    let r = m.request("tools/call", json!({"name": "query", "arguments": {"connection": "app-local"}}));
    assert!(
        r["error"]["code"] == -32602 && r["error"]["message"].as_str().unwrap().contains("sql"),
        "{r}"
    );

    let lines = m.finish();
    assert!(lines.iter().all(|l| !l.contains(PG_PW)), "비밀번호가 나가면 안 된다");
}

/// 요금 막음 판정은 serve 쪽 환경이라 로컬과 원격 둘 다 본다.
fn billed_calls(m: &mut Mcp) {
    for (name, args) in [
        ("query", json!({"connection": "athena-x", "sql": "SELECT 1 LIMIT 1", "force": true})),
        ("query", json!({"connection": "ddb-x", "sql": "SELECT * FROM \"t\" WHERE pk = 'a'"})),
        ("aws_tree", json!({"connection": "aws-x", "path": "sqs"})),
        ("aws_tree", json!({"connection": "aws-x", "path": "s3/logs"})),
        (
            "aws_open",
            json!({"connection": "aws-x", "path": "sns/arn:aws:sns:ap-northeast-2:1:t"}),
        ),
    ] {
        let (text, is_err) = m.call(name, args.clone());
        assert!(is_err && text == BILLED, "{name} {args}: {is_err} {text:?}");
    }
}

#[test]
fn 요금_막음_query_와_요금_aws_경로는_보내지_않고_같은_안내로_거절한다() {
    let d = project();
    let mut m = Mcp::start(d.path(), &[], false);
    billed_calls(&mut m);
    m.finish();
}

#[test]
fn remote_는_원격_serve_의_접속으로_같은_도구를_돌린다() {
    let d = project();
    let mut m = Mcp::start(d.path(), &["--remote", "t", "-max-rows", "5"], true);
    let (text, is_err) = m.call("list_connections", json!({}));
    assert!(
        !is_err && text.contains(r#""name":"app-prod""#) && text.contains(r#""region":"ap-northeast-2""#),
        "{text}"
    );
    let (text, is_err) = m.call("redis_get", json!({"connection": "app-local", "key": "k"}));
    assert!(is_err && text.contains("not redis"), "{text}");
    billed_calls(&mut m);
    let lines = m.finish();
    assert!(lines.iter().all(|l| !l.contains(PG_PW)), "비밀번호가 나가면 안 된다");
}

#[test]
fn 사용법_오류는_stderr_로_알리고_2_로_끝난다() {
    let out = Command::new(env!("CARGO_BIN_EXE_db"))
        .args(["mcp", "-max-rows", "-1"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty() && String::from_utf8_lossy(&out.stderr).contains("max-rows must not be negative: -1"));
    let out = Command::new(env!("CARGO_BIN_EXE_db"))
        .args(["mcp", "extra"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unexpected arguments: [extra]"));
}
