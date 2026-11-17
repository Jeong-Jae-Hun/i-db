#![allow(non_snake_case)]
//! `db-rs serve` 프로토콜 시험. 표준 입출력으로 요청을 보내 안전장치가 서버에서 걸리는지, 취소가 되는지,
//! 어느 응답에도 .env 의 비밀번호·AES 키가 나오지 않는지 본다. live 시험은 DBTUI_TEST_PG_URL·DBTUI_TEST_REDIS_ADDR 가 있을 때만 돈다.

mod common;

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};

struct Serve {
    child: Child,
    stdin: ChildStdin,
    out: BufReader<ChildStdout>,
    /// 받은 모든 줄(비밀 검사용)
    lines: Vec<String>,
    early: HashMap<u64, Value>,
    next: u64,
}

impl Serve {
    fn start(dir: &Path) -> Serve {
        let mut child = Command::new(env!("CARGO_BIN_EXE_db-rs"))
            .args(["serve", "-p"])
            .arg(dir)
            .env("XDG_STATE_HOME", dir.join("state"))
            .env("HOME", dir.join("state"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let (stdin, out) = (child.stdin.take().unwrap(), BufReader::new(child.stdout.take().unwrap()));
        Serve {
            child,
            stdin,
            out,
            lines: Vec::new(),
            early: HashMap::new(),
            next: 1,
        }
    }

    fn send(&mut self, mut req: Value) -> u64 {
        let id = self.next;
        self.next += 1;
        req["id"] = json!(id);
        writeln!(self.stdin, "{req}").unwrap();
        self.stdin.flush().unwrap();
        id
    }

    fn wait(&mut self, id: u64) -> Value {
        if let Some(v) = self.early.remove(&id) {
            return v;
        }
        loop {
            let mut line = String::new();
            assert!(self.out.read_line(&mut line).unwrap() > 0, "serve closed before answering {id}");
            self.lines.push(line.clone());
            let v: Value = serde_json::from_str(&line).unwrap();
            let got = v["id"].as_u64().unwrap();
            if got == id {
                return v;
            }
            self.early.insert(got, v);
        }
    }

    fn call(&mut self, req: Value) -> Value {
        let id = self.send(req);
        self.wait(id)
    }

    fn hello(&mut self) {
        let r = self.call(json!({"op": "hello", "version": 1}));
        assert_eq!(r["result"]["version"], 1, "{r}");
    }

    /// 입력을 닫아 끝내고 남은 줄까지 모은 뒤 비밀이 하나도 없는지 본다.
    fn finish(mut self, secrets: &[String]) {
        drop(self.stdin);
        for line in self.out.lines() {
            self.lines.push(line.unwrap());
        }
        assert!(self.child.wait().unwrap().success());
        for line in &self.lines {
            for s in secrets {
                assert!(!line.contains(s.as_str()), "secret leaked in {line}");
            }
        }
    }
}

/// .env 의 비밀번호·AES 키다. IV 는 staticEncrypt 암호문 앞에 그대로 붙는 값이라(`IV^|^…`) 결과에 나와도 된다.
fn secrets(dir: &Path) -> Vec<String> {
    fs::read_to_string(dir.join(".env"))
        .unwrap()
        .lines()
        .filter_map(|l| l.split_once('='))
        .filter(|(k, v)| (k.contains("PASSWORD") || k.ends_with("_AES_KEY")) && v.len() >= 6)
        .map(|(_, v)| v.to_string())
        .collect()
}

fn copy_dir(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for e in fs::read_dir(src).unwrap() {
        let e = e.unwrap();
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &dst.join(e.file_name()));
        } else {
            fs::copy(e.path(), dst.join(e.file_name())).unwrap();
        }
    }
}

/// 골든 offline 프로젝트에 답하지 않는 PostgreSQL 접속(hang-stg)을 더한 것이다.
fn offline_project(hang_port: u16) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    copy_dir(&common::root().join("testdata/golden/offline"), tmp.path());
    let path = tmp.path().join(".dbtui/connections.json");
    let mut conns: Vec<Value> = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    conns.push(
        json!({"uuid": "10000000-0000-0000-0000-0000000000ff", "name": "hang-stg", "group": "grp", "kind": "postgresql",
        "host": "127.0.0.1", "port": hang_port, "user": "u", "db": "d", "sslmode": "disable"}),
    );
    fs::write(&path, serde_json::to_vec(&conns).unwrap()).unwrap();
    let mut env = fs::read_to_string(tmp.path().join(".env")).unwrap();
    env.push_str("PG_HANG_STG_PASSWORD=golden-secret-hang\n");
    fs::write(tmp.path().join(".env"), env).unwrap();
    tmp
}

fn err(r: &Value) -> (i64, &str, &str) {
    assert_eq!(r["ok"], false, "{r}");
    (
        r["code"].as_i64().unwrap(),
        r["error"].as_str().unwrap(),
        r["reason"].as_str().unwrap_or(""),
    )
}

#[test]
fn 인사_전_요청과_다른_판은_사용법_오류다() {
    let tmp = offline_project(1);
    let mut s = Serve::start(tmp.path());
    let r = s.call(json!({"op": "project"}));
    assert_eq!(err(&r), (2, "hello first", ""));
    let r = s.call(json!({"op": "hello", "version": 99}));
    assert_eq!(err(&r).0, 2);
    s.hello();
    let r = s.call(json!({"op": "nope"}));
    assert_eq!(err(&r).0, 2);
    s.finish(&secrets(tmp.path()));
}

#[test]
fn 원격_prod_쓰기는_서버가_코드_3_으로_거절한다() {
    let tmp = offline_project(1);
    let mut s = Serve::start(tmp.path());
    s.hello();
    // CLI 길: prod 쓰기는 --prod 가 있어도 거절(R58.2)
    let r = s.call(json!({"op": "cli", "args": ["query", "-c", "app-prod", "--prod", "--write", "delete from t where id = 1"]}));
    assert_eq!(r["result"]["code"], 3, "{r}");
    let r = s.call(json!({"op": "cli", "args": ["redis", "-c", "app-prod-redis", "--prod", "--write", "SET", "a", "b"]}));
    assert_eq!(r["result"]["code"], 3, "{r}");
    // 화면 길: 잠긴 prod 는 확인을 보내도 거절(R54.1), 열면 확인을 요구(R6)
    let r = s.call(json!({"op": "run", "conn": "app-prod", "sql": "delete from t where id = 1", "confirm_prod": true}));
    assert_eq!(err(&r), (3, "prod(app-prod) 쓰기 잠김 — 10분 열기", "prod_locked"));
    let r = s.call(json!({"op": "redis_run", "conn": "app-prod-redis", "args": ["SET", "a", "b"], "confirm_prod": true}));
    assert_eq!(err(&r).2, "prod_locked");
    let r = s.call(json!({"op": "tx_begin", "conn": "app-prod"}));
    assert_eq!(err(&r).1, "prod(app-prod) 쓰기 잠김 — 10분 열기 · 트랜잭션 끝내기는 F10/F11");
    let r = s.call(json!({"op": "prod_lock", "conn": "app-prod", "unlock": true}));
    assert_eq!(r["result"]["locked"], false, "{r}");
    let r = s.call(json!({"op": "run", "conn": "app-prod", "sql": "delete from t where id = 1"}));
    assert_eq!(err(&r), (3, "prod(app-prod)에 쓰기 실행: y/N", "confirm_prod"));
    let r = s.call(json!({"op": "prod_lock", "conn": "app-prod", "unlock": false}));
    assert_eq!(r["result"]["locked"], true, "{r}");
    // 위험 문장은 환경과 상관없이 확인(R25)
    let r = s.call(json!({"op": "run", "conn": "app-stg", "sql": "delete from t"}));
    assert_eq!(err(&r), (3, "위험: WHERE 없는 DELETE", "confirm_danger"));
    let r = s.call(json!({"op": "prod_lock", "conn": "app-stg", "unlock": true}));
    assert_eq!(err(&r), (2, "prod 접속이 아니라 잠그지 않음", ""));
    s.finish(&secrets(tmp.path()));
}

#[test]
fn 접속_목록과_오류에는_비밀번호와_키가_없다() {
    let tmp = offline_project(1);
    let mut s = Serve::start(tmp.path());
    s.hello();
    let r = s.call(json!({"op": "project"}));
    let conns = r["result"]["conns"].as_array().unwrap();
    assert!(conns.iter().any(|c| c["name"] == "app-prod" && c["env"] == "prod"), "{r}");
    assert!(conns.iter().all(|c| c.get("password").is_none() && c.get("user").is_none()));
    // 닿지 않는 접속의 오류·CLI 의 enc()·json 출력도 같은 검사를 거친다
    let r = s.call(json!({"op": "schemas", "conn": "app-stg"}));
    assert_eq!(err(&r).0, 1);
    let r = s.call(json!({"op": "cli", "args": ["query", "-c", "app-stg", "--format", "json"], "input": {"text": "select enc('x')"}}));
    assert_eq!(r["result"]["code"], 1, "{r}");
    let r = s.call(json!({"op": "cli", "args": ["conns", "--format", "json"]}));
    assert_eq!(r["result"]["code"], 0, "{r}");
    // 서버의 -f 파일은 읽지 않는다(클라이언트가 읽어 input 으로 보낸다)
    let r = s.call(json!({"op": "cli", "args": ["query", "-c", "app-stg", "-f", tmp.path().join(".env")]}));
    assert_eq!(r["result"]["stderr"], "실행할 글이 없음 (인자, -f 파일, 표준 입력)\n", "{r}");
    let r = s.call(json!({"op": "cli", "args": ["update"]}));
    assert_eq!(r["result"]["code"], 2, "{r}");
    s.finish(&secrets(tmp.path()));
}

#[test]
fn 진행_중인_요청은_id_로_취소한다() {
    // 연결은 받되 답하지 않는 서버라 접속 시작에서 멈춘다
    let hang = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let tmp = offline_project(hang.local_addr().unwrap().port());
    let mut s = Serve::start(tmp.path());
    s.hello();
    let slow = s.send(json!({"op": "run", "conn": "hang-stg", "sql": "select 1"}));
    // 그동안 다른 요청은 따로 돈다
    let r = s.call(json!({"op": "prod_lock", "conn": "app-prod"}));
    assert_eq!(r["result"]["locked"], true, "{r}");
    std::thread::sleep(std::time::Duration::from_millis(200));
    let c = s.call(json!({"op": "cancel", "target": slow}));
    assert_eq!(c["result"]["canceled"], true, "{c}");
    let r = s.wait(slow);
    assert_eq!(err(&r), (1, "canceled", ""));
    let c = s.call(json!({"op": "cancel", "target": slow}));
    assert_eq!(c["result"]["canceled"], false, "{c}");
    s.finish(&secrets(tmp.path()));
    drop(hang);
}

#[test]
fn live_세션_트랜잭션과_테이블_보기와_Redis_를_프로토콜로_돈다() {
    let Some(vars) = common::live_vars() else { return };
    let tmp = tempfile::tempdir().unwrap();
    common::copy_project(&common::root().join("testdata/golden/live"), tmp.path(), &vars);
    let mut s = Serve::start(tmp.path());
    s.hello();
    let conn = "golden-local";
    let ok = |r: &Value| -> Value {
        assert_eq!(r["ok"], true, "{r}");
        r["result"].clone()
    };
    assert!(
        ok(&s.call(json!({"op": "schemas", "conn": conn})))
            .as_array()
            .unwrap()
            .contains(&json!("public"))
    );
    let t = ok(&s.call(json!({"op": "tx_begin", "conn": conn})));
    assert_eq!(t["in_tx"], true);
    let r = ok(&s.call(json!({"op": "run", "conn": conn, "sql": "create temp table serve_t (id int primary key, v text)"})));
    assert_eq!(r["in_tx"], true);
    ok(&s.call(json!({"op": "run", "conn": conn, "sql": "insert into serve_t values (1, 'a'), (2, 'b'), (3, 'c')"})));
    let p = ok(
        &s.call(json!({"op": "table_page", "conn": conn, "schema": "pg_temp", "table": "serve_t",
        "where": "id > 1", "order_by": "id DESC", "limit": 1, "count": true})),
    );
    assert_eq!(p["result"]["rows"], json!([["3", "c"]]), "{p}");
    assert_eq!(p["count"], 2, "{p}");
    assert_eq!(
        p["sql"],
        "SELECT * FROM \"pg_temp\".\"serve_t\" WHERE id > 1 ORDER BY id DESC LIMIT 1 OFFSET 0"
    );
    let r = s.call(json!({"op": "table_page", "conn": conn, "schema": "pg_temp", "table": "serve_t", "where": "true; drop table serve_t"}));
    assert_eq!(err(&r).0, 2);
    let e = ok(&s.call(json!({"op": "explain", "conn": conn, "sql": "select * from serve_t", "format": "json"})));
    assert!(e["rows"][0][0].as_str().unwrap().contains("Seq Scan"), "{e}");
    let t = ok(&s.call(json!({"op": "tx_state", "conn": conn})));
    assert_eq!(t["in_tx"], true);
    let t = ok(&s.call(json!({"op": "tx_rollback", "conn": conn})));
    assert_eq!((t["in_tx"].clone(), t["text"].clone()), (json!(false), json!("ROLLBACK")));
    let r = s.call(json!({"op": "tx_commit", "conn": conn}));
    assert_eq!(err(&r), (2, "열린 트랜잭션 없음", ""));
    let r =
        ok(&s.call(json!({"op": "run", "conn": conn, "sql": "select :n::int + 1 as v, enc('a@example.com') as e", "params": {"n": "41"}})));
    assert_eq!(r["rows"][0][0], "42", "{r}");
    assert_eq!(r["note"], "enc() 1개 암호화");
    let r = s.call(json!({"op": "run", "conn": conn, "sql": "select * from serve_t"}));
    assert!(err(&r).1.contains("serve_t"), "{r}");
    // Redis: 쓰기·찾기·값 보기·지우기
    let rc = "golden-local-redis";
    ok(&s.call(json!({"op": "redis_run", "conn": rc, "args": ["SET", "golden:serve", "{\"a\":1}"]})));
    let k = ok(&s.call(json!({"op": "redis_scan", "conn": rc, "pattern": "golden:serve*"})));
    assert_eq!(k["keys"], json!(["golden:serve"]), "{k}");
    let v = ok(&s.call(json!({"op": "redis_value", "conn": rc, "key": "golden:serve"})));
    let text = v["text"].as_str().unwrap();
    assert!(text.starts_with("golden:serve · string · TTL ∞"), "{text}");
    assert!(text.ends_with("{\n  \"a\": 1\n}"), "{text}");
    let r = s.call(json!({"op": "redis_run", "conn": rc, "args": ["FLUSHDB"]}));
    assert_eq!(err(&r).2, "confirm_danger");
    ok(&s.call(json!({"op": "redis_run", "conn": rc, "args": ["DEL", "golden:serve"]})));
    s.finish(&secrets(tmp.path()));
}

#[tokio::test]
async fn 원격_엔진과_로컬_엔진은_같은_답을_내고_버린_요청은_취소된다() {
    use idb_engine::{Engine, LocalEngine, RemoteEngine, RemoteSpec, RunRequest};
    use std::sync::Arc;
    let hang = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let tmp = offline_project(hang.local_addr().unwrap().port());
    let spec = RemoteSpec {
        name: "loop".into(),
        program: env!("CARGO_BIN_EXE_db-rs").into(),
        args: vec!["serve".into(), "-p".into(), tmp.path().to_string_lossy().into_owned()],
    };
    let remote = Engine::Remote(Arc::new(RemoteEngine::connect(&spec).await.unwrap()));
    let proj = idb_core::project::load_project(tmp.path()).unwrap();
    let local = Engine::Local(Arc::new(LocalEngine::new(proj, None)));
    let write = RunRequest {
        conn: "app-prod".into(),
        sql: "update t set a = 1 where id = 1".into(),
        confirm_prod: true,
        ..Default::default()
    };
    for e in [&remote, &local] {
        assert_eq!(e.project().await.unwrap().conns.len(), 12);
        let r = e.run(write.clone()).await.unwrap_err();
        assert_eq!((r.code, r.reason.as_deref()), (3, Some("prod_locked")));
        assert!(!e.tx_state("app-stg").await.unwrap().in_tx);
    }
    // 기다리다 버리면 cancel 이 가고, 연결은 다음 요청에 그대로 쓴다
    let slow = remote.run(RunRequest {
        conn: "hang-stg".into(),
        sql: "select 1".into(),
        ..Default::default()
    });
    assert!(tokio::time::timeout(std::time::Duration::from_millis(200), slow).await.is_err());
    assert!(remote.prod_lock("app-prod", None).await.unwrap().locked);
    remote.close().await;
    let r = remote.project().await.unwrap_err();
    assert!(r.error.starts_with("remote loop: "), "{r:?}");
    drop(hang);
}
