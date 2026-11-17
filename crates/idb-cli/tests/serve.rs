#![allow(non_snake_case)]
//! `db serve` 프로토콜 시험. 표준 입출력으로 요청을 보내 안전장치가 서버에서 걸리는지, 취소가 되는지,
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
        let mut child = Command::new(env!("CARGO_BIN_EXE_db"))
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
        program: env!("CARGO_BIN_EXE_db").into(),
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

#[test]
fn live_결과_편집_반영과_되돌리기와_운영_조회를_프로토콜로_돈다() {
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
    let run = |s: &mut Serve, sql: &str| s.call(json!({"op": "run", "conn": conn, "sql": sql, "confirm_danger": true}));
    for sql in [
        "drop schema if exists serve_e cascade",
        "create schema serve_e",
        "create type serve_e.mood as enum ('happy', 'sad')",
        "create table serve_e.p (id int generated always as identity primary key, name text not null, note text, twice int generated always as (length(name) * 2) stored, m serve_e.mood)",
        "create table serve_e.c (id int primary key, p_id int references serve_e.p(id))",
        "insert into serve_e.p (name, note) values ('a', null), ('bb', 'x')",
        "create function serve_e.f() returns int language sql as 'select 1'",
    ] {
        ok(&run(&mut s, sql));
    }
    // 출처: PK 가 다 있으면 편집할 수 있고, 생성 열·식 열은 이름이 빈다
    let r = ok(&run(&mut s, "select id, name, note, twice, 1 as k from serve_e.p order by id"));
    let origin = r["origin"].clone();
    assert_eq!(origin["table"], json!({"schema": "serve_e", "table": "p"}), "{r}");
    assert_eq!(origin["pk"], json!(["id"]));
    assert_eq!(origin["cols"], json!(["id", "name", "note", "", ""]));
    assert_eq!(origin["always"], json!(["id"]));
    assert!(
        ok(&run(&mut s, "select name from serve_e.p")).get("origin").is_none(),
        "PK 없는 결과는 읽기 전용"
    );
    assert!(
        ok(&run(&mut s, "select a.id, b.name from serve_e.p a, serve_e.p b limit 1"))
            .get("origin")
            .is_some()
    );
    // 반영: UPDATE·DELETE 는 정확히 한 행, 옛값 확인
    let cs = json!({"table": {"schema": "serve_e", "table": "p"}, "pk": ["id"], "changes": [
        {"op": "update", "key": {"id": "1"}, "values": {"name": "q"}, "old": {"name": "a"}},
        {"op": "delete", "key": {"id": "2"}, "old": {"name": "bb", "note": "x"}},
        {"op": "insert", "values": {"name": "new"}},
    ]});
    let a = ok(&s.call(json!({"op": "apply_changes", "conn": conn, "changes": cs})));
    assert_eq!(a["result"]["text"], "UPDATE 1 · INSERT 1 · DELETE 1", "{a}");
    assert_eq!(a["result"]["rows"][0][1], "new");
    assert_eq!(a["applied"]["generated"], json!(["twice"]));
    assert_eq!(a["applied"]["rows"][1]["note"], "x");
    assert!(
        a["sql"][0]
            .as_str()
            .unwrap()
            .starts_with(r#"UPDATE "serve_e"."p" SET "name" = 'q' WHERE "id" = '1'"#)
    );
    let names = |s: &mut Serve| ok(&run(s, "select id, name from serve_e.p order by id"))["rows"].clone();
    let new_id = a["applied"]["rows"][2]["id"].as_str().unwrap().to_string();
    assert_eq!(names(&mut s), json!([["1", "q"], [new_id.clone(), "new"]]));
    // 옛값이 그새 바뀌었으면 묶음째 되돌린다
    let stale = json!({"table": {"schema": "serve_e", "table": "p"}, "pk": ["id"], "changes": [
        {"op": "update", "key": {"id": "1"}, "values": {"name": "z"}, "old": {"name": "a"}}]});
    let r = s.call(json!({"op": "apply_changes", "conn": conn, "changes": stale}));
    assert!(err(&r).1.starts_with("UPDATE failed: row (id=1) changed since it was read"), "{r}");
    // PK 가 아닌 열로 행을 찾는 묶음은 거절한다
    let fake = json!({"table": {"schema": "serve_e", "table": "p"}, "pk": ["name"], "changes": [
        {"op": "delete", "key": {"name": "q"}}]});
    assert_eq!(err(&s.call(json!({"op": "apply_changes", "conn": conn, "changes": fake}))).0, 2);
    // 되돌리기: 미리 보기와 실행, 지운 행은 IDENTITY 키 그대로 되살린다
    let p = ok(&s.call(json!({"op": "changes_sql", "changes": cs, "applied": a["applied"]})));
    assert!(p["sql"][1].as_str().unwrap().contains("OVERRIDING SYSTEM VALUE"), "{p}");
    let u = ok(&s.call(json!({"op": "undo_changes", "conn": conn, "changes": cs, "applied": a["applied"]})));
    assert_eq!(u["result"]["text"], "UPDATE 1 · INSERT 1 · DELETE 1", "{u}");
    assert_eq!(names(&mut s), json!([["1", "a"], ["2", "bb"]]));
    // 열린 트랜잭션 안의 반영은 SAVEPOINT 라 트랜잭션이 이어진다
    ok(&s.call(json!({"op": "tx_begin", "conn": conn})));
    let one = json!({"table": {"schema": "serve_e", "table": "p"}, "pk": ["id"], "changes": [
        {"op": "update", "key": {"id": "1"}, "values": {"note": "t"}, "old": {"note": null}}]});
    let a = ok(&s.call(json!({"op": "apply_changes", "conn": conn, "changes": one})));
    assert_eq!(a["result"]["in_tx"], true);
    ok(&s.call(json!({"op": "tx_rollback", "conn": conn})));
    assert_eq!(
        ok(&run(&mut s, "select note from serve_e.p where id = 1"))["rows"],
        json!([["NULL"]])
    );
    // search_path·정의로 이동·FK·목록·enum·추정
    let st = ok(&s.call(json!({"op": "set_search_path", "conn": conn, "schema": "serve_e"})));
    assert_eq!(st["search_path"], "serve_e");
    assert_eq!(ok(&run(&mut s, "select count(*) from p"))["rows"], json!([["2"]]));
    let o = ok(&s.call(json!({"op": "resolve", "conn": conn, "name": "P"})));
    assert_eq!(o, json!({"kind": "table", "schema": "serve_e", "name": "p"}));
    let o = ok(&s.call(json!({"op": "resolve", "conn": conn, "name": "f"})));
    assert_eq!(o["kind"], "function");
    assert!(
        ok(&s.call(json!({"op": "function_def", "conn": conn, "schema": "serve_e", "name": "f"})))
            .as_str()
            .unwrap()
            .contains("select 1")
    );
    assert!(
        err(&s.call(json!({"op": "resolve", "conn": conn, "name": "nope_zz"})))
            .1
            .starts_with("definition not found")
    );
    let fks = ok(&s.call(json!({"op": "foreign_keys", "conn": conn, "schema": "serve_e", "table": "c"})));
    assert_eq!(fks[0]["ref"], json!({"schema": "serve_e", "table": "p"}), "{fks}");
    assert_eq!(
        (fks[0]["cols"].clone(), fks[0]["ref_cols"].clone()),
        (json!(["p_id"]), json!(["id"]))
    );
    let fks = ok(&s.call(json!({"op": "foreign_keys", "conn": conn, "schema": "serve_e", "table": "p", "incoming": true})));
    assert_eq!(fks[0]["table"]["table"], "c");
    let cat = ok(&s.call(json!({"op": "catalog", "conn": conn})));
    let se = cat["schemas"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["name"] == "serve_e")
        .unwrap()
        .clone();
    assert!(
        se["tables"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "p" && t["columns"][0]["name"] == "id"),
        "{se}"
    );
    let en = ok(&s.call(json!({"op": "enum_labels", "conn": conn, "types": ["serve_e.mood", "int4"]})));
    assert_eq!(en, json!({"serve_e.mood": ["happy", "sad"], "int4": []}));
    let est =
        ok(&s.call(json!({"op": "estimate_rows", "conn": conn, "sql": "delete from serve_e.p where id = :id", "params": {"id": "1"}})));
    assert!(est["rows"].as_i64().unwrap() >= 0, "{est}");
    assert_eq!(err(&s.call(json!({"op": "estimate_rows", "conn": conn, "sql": "select 1"}))).0, 1);
    // CSV 가져오기는 COPY 한 번이고 실패하면 한 행도 남지 않는다
    let imp = ok(&s.call(
        json!({"op": "import_csv", "conn": conn, "schema": "serve_e", "table": "c", "columns": ["id", "p_id"],
        "rows": [["1", "1"], ["2", null]]}),
    ));
    assert_eq!(imp["rows"], 2);
    let bad = s.call(
        json!({"op": "import_csv", "conn": conn, "schema": "serve_e", "table": "c", "columns": ["id", "p_id"],
        "rows": [["3", "1"], ["4", "999"]]}),
    );
    assert_eq!(err(&bad).0, 1);
    assert_eq!(ok(&run(&mut s, "select count(*) from serve_e.c"))["rows"], json!([["2"]]));
    // 운영 화면과 백엔드 신호
    let act = ok(&s.call(json!({"op": "activity", "conn": conn})));
    assert_eq!(act["cols"][0], "pid");
    let locks = ok(&s.call(json!({"op": "locks", "conn": conn})));
    assert!(locks["text"].as_str().unwrap().starts_with("pg_locks: "), "{locks}");
    let sizes = ok(&s.call(json!({"op": "table_sizes", "conn": conn})));
    assert!(sizes["rows"].as_array().unwrap().iter().any(|r| r[0] == "serve_e"), "{sizes}");
    let r = s.call(json!({"op": "signal_backend", "conn": conn, "pid": 1}));
    assert_eq!(err(&r), (3, "golden-local pid 1 쿼리 취소: y/N", "confirm"));
    let r = s.call(json!({"op": "signal_backend", "conn": conn, "pid": 2147483000, "confirm": true}));
    assert!(err(&r).1.contains("was not signaled"), "{r}");
    // DB 바꾸기: 다른 DB 로 열고 돌아온다
    let dbs = ok(&s.call(json!({"op": "databases", "conn": conn})));
    assert!(dbs.as_array().unwrap().contains(&json!("postgres")), "{dbs}");
    let st = ok(&s.call(json!({"op": "switch_db", "conn": conn, "db": "template1_missing_zz"})));
    assert_eq!(
        (st["db"].clone(), st["search_path"].clone()),
        (json!("template1_missing_zz"), json!(""))
    );
    assert_eq!(err(&s.call(json!({"op": "schemas", "conn": conn}))).0, 1);
    ok(&s.call(json!({"op": "switch_db", "conn": conn, "db": ""})));
    // 여러 접속 조회도 op 로
    let l = ok(&s.call(json!({"op": "linked", "conn": conn, "column": "p_id", "value": "1"})));
    assert!(l["result"]["text"].as_str().unwrap().starts_with("연결된 곳"), "{l}");
    let d = ok(&s.call(json!({"op": "schema_diff", "conn": conn, "to": conn, "schema": "serve_e"})));
    assert!(d["rows"].as_array().unwrap().is_empty(), "{d}");
    ok(&run(&mut s, "drop schema serve_e cascade"));
    // Redis: 원값·값 편집·모니터·패턴 삭제
    let rc = "golden-local-redis";
    ok(&s.call(json!({"op": "redis_run", "conn": rc, "args": ["RPUSH", "serve:e:l", "a", "b", "a"]})));
    ok(&s.call(json!({"op": "redis_run", "conn": rc, "args": ["SET", "serve:e:s", "v"]})));
    let raw = ok(&s.call(json!({"op": "redis_raw_value", "conn": rc, "key": "serve:e:l"})));
    assert_eq!(
        (raw["type"].clone(), raw["items"].clone(), raw["ttl_secs"].clone()),
        (json!("list"), json!(["a", "b", "a"]), json!(-1))
    );
    let e = ok(&s.call(
        json!({"op": "redis_edit", "conn": rc, "edit": {"key": "serve:e:l", "type": "list", "op": "delete", "index": 2, "old": "a"}}),
    ));
    assert_eq!(e["status"], "반영함", "{e}");
    let raw = ok(&s.call(json!({"op": "redis_raw_value", "conn": rc, "key": "serve:e:l"})));
    assert_eq!(raw["items"], json!(["a", "b"]));
    let e = ok(
        &s.call(json!({"op": "redis_edit", "conn": rc, "edit": {"key": "serve:e:l", "type": "list", "op": "rename", "text": "serve:e:s"}}))
    );
    assert_eq!(
        (e["key"].clone(), e["status"].clone()),
        (json!("serve:e:l"), json!("같은 이름의 키가 있어 바꾸지 않음"))
    );
    let e = ok(&s.call(json!({"op": "redis_edit", "conn": rc, "edit": {"key": "serve:e:l", "type": "list", "op": "ttl", "text": "100"}})));
    assert_eq!(e["results"][0]["text"], "1");
    assert_eq!(
        err(&s.call(json!({"op": "redis_raw_value", "conn": rc, "key": "serve:e:none"}))).1,
        "key does not exist"
    );
    let info = ok(&s.call(json!({"op": "redis_info", "conn": rc})));
    assert_eq!(info["rows"][0][0], "버전");
    let sl = ok(&s.call(json!({"op": "redis_slowlog", "conn": rc})));
    assert_eq!(sl["cols"], json!(["시각", "걸린 시간", "명령", "클라이언트"]));
    let big = ok(&s.call(json!({"op": "redis_big_keys", "conn": rc})));
    assert!(big["text"].as_str().unwrap().starts_with("표본 "), "{big}");
    let keys = ok(&s.call(json!({"op": "redis_pattern_keys", "conn": rc, "pattern": "serve:e:*"})));
    let mut got: Vec<String> = serde_json::from_value(keys["keys"].clone()).unwrap();
    got.sort();
    assert_eq!(got, ["serve:e:l", "serve:e:s"]);
    let r = s.call(json!({"op": "redis_unlink", "conn": rc, "keys": got}));
    assert_eq!(err(&r), (3, "golden-local-redis 키 2개 지움: y/N", "confirm"));
    let n = ok(&s.call(json!({"op": "redis_unlink", "conn": rc, "keys": got, "confirm": true})));
    assert_eq!(n["rows"], 2);
    s.finish(&secrets(tmp.path()));
}

#[test]
fn prod_편집_반영_가져오기_신호_일괄_삭제는_잠금과_확인을_거친다() {
    let tmp = offline_project(1);
    let mut s = Serve::start(tmp.path());
    s.hello();
    let cs = json!({"table": {"schema": "public", "table": "t"}, "pk": ["id"], "changes": [
        {"op": "delete", "key": {"id": "1"}}]});
    for req in [
        json!({"op": "apply_changes", "conn": "app-prod", "changes": cs, "confirm_prod": true}),
        json!({"op": "undo_changes", "conn": "app-prod", "changes": cs, "applied": {"rows": [{"id": "1"}]}, "confirm_prod": true}),
        json!({"op": "import_csv", "conn": "app-prod", "schema": "public", "table": "t", "columns": ["id"], "rows": [["1"]], "confirm_prod": true}),
        json!({"op": "signal_backend", "conn": "app-prod", "pid": 1, "confirm": true, "confirm_prod": true}),
        json!({"op": "redis_unlink", "conn": "app-prod-redis", "keys": ["a"], "confirm": true, "confirm_prod": true}),
        json!({"op": "redis_edit", "conn": "app-prod-redis", "edit": {"key": "a", "type": "string", "op": "delete_key"}, "confirm_prod": true}),
    ] {
        let r = s.call(req.clone());
        assert_eq!(err(&r).2, "prod_locked", "{req} → {r}");
    }
    s.call(json!({"op": "prod_lock", "conn": "app-prod", "unlock": true}));
    let r = s.call(json!({"op": "apply_changes", "conn": "app-prod", "changes": cs}));
    assert_eq!(err(&r), (3, "prod(app-prod)에 제출 1건: y/N", "confirm_prod"));
    let r = s.call(json!({"op": "signal_backend", "conn": "app-prod", "pid": 7, "terminate": true}));
    assert_eq!(err(&r), (3, "prod(app-prod) pid 7 연결 종료: y/N", "confirm_prod"));
    let r = s.call(json!({"op": "changes_sql", "changes": cs}));
    assert_eq!(
        r["result"]["sql"],
        json!([r#"DELETE FROM "public"."t" WHERE "id" = '1' RETURNING *;"#])
    );
    // user360 op: 닿지 않는 접속의 오류도 가린다
    let r = s.call(json!({"op": "user360", "query": "a@example.com"}));
    assert!(r["result"].as_array().is_some_and(|a| !a.is_empty()), "{r}");
    s.finish(&secrets(tmp.path()));
}

#[test]
fn 저장소_op_는_쓰기_비싼_질의를_코드_3_까닭으로_거절하고_보드는_패널마다_결과다() {
    let tmp = offline_project(1);
    let mut s = Serve::start(tmp.path());
    s.hello();
    let r = s.call(json!({"op": "source_run", "conn": "search-stg", "text": "DELETE logs"}));
    assert_eq!(err(&r), (3, "읽기 전용 저장소라 보내지 않음", "read_only"));
    let r = s.call(json!({"op": "source_run", "conn": "search-stg", "text": "GET logs/_search?size=20000"}));
    let (code, _, reason) = err(&r);
    assert_eq!((code, reason), (3, "confirm_expensive"), "{r}");
    // Loki 7일 넘는 범위(R60.2)
    let r = s.call(json!({"op": "source_run", "conn": "logs-stg", "text": "{a=\"1\"}", "since_ns": 8i64 * 24 * 3_600_000_000_000}));
    assert_eq!(err(&r).2, "confirm_expensive", "{r}");
    let r = s.call(json!({"op": "source_run", "conn": "aws-stg", "text": "x"}));
    assert_eq!(err(&r).0, 2, "{r}");
    let r = s.call(json!({"op": "source_children", "conn": "app-stg"}));
    assert_eq!(err(&r).0, 2, "{r}");
    // 닿지 않는 저장소는 실행 오류다
    let r = s.call(json!({"op": "source_children", "conn": "search-stg", "path": ""}));
    assert_eq!(err(&r).0, 1, "{r}");
    let r = s.call(json!({"op": "health", "conn": "logs-stg"}));
    assert_eq!(r["result"]["ok"], false, "{r}");
    assert!(!r["result"]["err"].as_str().unwrap().is_empty(), "{r}");
    let r = s.call(json!({"op": "flows", "names": ["nope"]}));
    assert_eq!(err(&r), (2, "흐름 없음: nope (.dbtui/flows.json)", ""));
    let r = s.call(json!({"op": "board_list"}));
    assert_eq!(r["result"], json!([{"name": "app-stg", "title": "app stg", "panels": 1}]), "{r}");
    let r = s.call(json!({"op": "board_run", "name": "app-stg"}));
    let p = &r["result"]["panels"][0];
    assert_eq!(r["result"]["title"], "app stg", "{r}");
    assert_eq!(p["error"], "설정 오류: view must be one of number|table|chart|flow: \"\"", "{r}");
    let r = s.call(json!({"op": "board_run", "name": "nope"}));
    assert_eq!(err(&r), (2, "보드 없음: nope (.dbtui/boards)", ""));
    s.finish(&secrets(tmp.path()));
}

#[test]
fn EXPLAIN_ANALYZE_는_속_문장으로_쓰기와_위험을_가른다() {
    let tmp = offline_project(1);
    let mut s = Serve::start(tmp.path());
    s.hello();
    let explain = |sql: &str, conn: &str, extra: Value| {
        let mut req = json!({"op": "explain", "conn": conn, "sql": sql, "analyze": true, "buffers": true, "format": "JSON"});
        for (k, v) in extra.as_object().unwrap() {
            req[k] = v.clone();
        }
        req
    };
    // 쓰기 문장의 ANALYZE 는 환경과 상관없이 실행 확인(R9.2), 그다음 위험 확인(R25)
    let r = s.call(explain("delete from t", "app-stg", json!({})));
    assert_eq!(err(&r), (3, "EXPLAIN ANALYZE 는 문장을 실제로 실행합니다: y/N", "confirm_analyze"));
    let r = s.call(explain("delete from t", "app-stg", json!({"confirm_analyze": true})));
    assert_eq!(err(&r), (3, "위험: WHERE 없는 DELETE", "confirm_danger"));
    // 잠긴 prod: 쓰기의 ANALYZE 는 확인 없이 거절, 열면 실행 확인 → prod 확인 순
    let r = s.call(explain(
        "delete from t where id = 1",
        "app-prod",
        json!({"confirm_analyze": true, "confirm_prod": true}),
    ));
    assert_eq!(err(&r).2, "prod_locked", "{r}");
    s.call(json!({"op": "prod_lock", "conn": "app-prod", "unlock": true}));
    let r = s.call(explain("delete from t where id = 1", "app-prod", json!({"confirm_prod": true})));
    assert_eq!(err(&r).2, "confirm_analyze", "{r}");
    let r = s.call(explain("delete from t where id = 1", "app-prod", json!({"confirm_analyze": true})));
    assert_eq!(err(&r).2, "confirm_prod", "{r}");
    s.call(json!({"op": "prod_lock", "conn": "app-prod", "unlock": false}));
    // 읽기의 ANALYZE 는 잠긴 prod 에서도 쓰기로 보지 않는다(접속이 없어 실행 오류로 끝난다)
    let r = s.call(explain("select 1", "app-prod", json!({})));
    assert_eq!((err(&r).0, err(&r).2), (1, ""), "{r}");
    // ANALYZE 없는 EXPLAIN 은 속 문장을 실행하지 않아 묻지 않는다(R9.1)
    let r = s.call(json!({"op": "explain", "conn": "app-prod", "sql": "delete from t", "format": "JSON"}));
    assert_eq!((err(&r).0, err(&r).2), (1, ""), "{r}");
    s.finish(&secrets(tmp.path()));
}

#[test]
fn Redis_KEYS_는_접속하기_전에_CLI_와_같은_글로_거절한다() {
    let tmp = offline_project(1);
    let mut s = Serve::start(tmp.path());
    s.hello();
    let want = (
        3,
        "KEYS 는 CLI 에서 하지 않음 (서버를 멈춤): SCAN 0 MATCH * COUNT 1000 을 쓰세요",
        "keys",
    );
    for conn in ["app-prod-redis", "app-stg-redis"] {
        let r = s.call(json!({"op": "redis_run", "conn": conn, "args": ["keys", "*"], "confirm_prod": true}));
        assert_eq!(err(&r), want, "{conn}");
    }
    s.finish(&secrets(tmp.path()));
}

#[test]
fn live_편집_반영의_PK_는_세션_트랜잭션_안에서_찾는다() {
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
    ok(&s.call(json!({"op": "tx_begin", "conn": conn})));
    // 아직 커밋하지 않은 테이블·PK 는 풀 연결에서 보이지 않는다
    for sql in [
        "create temp table pk_t (id int, v text)",
        "alter table pk_t add primary key (id)",
        "insert into pk_t values (1, 'a')",
    ] {
        ok(&s.call(json!({"op": "run", "conn": conn, "sql": sql})));
    }
    let cs = json!({"table": {"schema": "pg_temp", "table": "pk_t"}, "pk": ["id"], "changes": [
        {"op": "update", "key": {"id": "1"}, "values": {"v": "b"}, "old": {"v": "a"}}]});
    let a = ok(&s.call(json!({"op": "apply_changes", "conn": conn, "changes": cs})));
    assert_eq!(a["result"]["text"], "UPDATE 1", "{a}");
    // PK 가 아닌 열이면 여전히 거절하고, 찾다 실패해도 트랜잭션은 이어진다
    let bad = json!({"table": {"schema": "pg_temp", "table": "pk_t"}, "pk": ["v"], "changes": [
        {"op": "delete", "key": {"v": "b"}}]});
    assert_eq!(err(&s.call(json!({"op": "apply_changes", "conn": conn, "changes": bad}))).0, 2);
    let none = json!({"table": {"schema": "public", "table": "no_such_t"}, "pk": ["id"], "changes": [
        {"op": "delete", "key": {"id": "1"}}]});
    assert_eq!(err(&s.call(json!({"op": "apply_changes", "conn": conn, "changes": none}))).0, 1);
    let r = ok(&s.call(json!({"op": "run", "conn": conn, "sql": "select v from pk_t"})));
    assert_eq!((r["rows"].clone(), r["in_tx"].clone()), (json!([["b"]]), json!(true)), "{r}");
    ok(&s.call(json!({"op": "tx_rollback", "conn": conn})));
    s.finish(&[]);
}

#[test]
fn 잘못된_퍼센트_뒤_한글이_있는_저장소_질의도_오류로_답한다() {
    let tmp = offline_project(1);
    let mut s = Serve::start(tmp.path());
    s.hello();
    let r = s.call(json!({"op": "source_run", "conn": "search-stg", "text": "GET logs/_search?x=%한"}));
    assert_eq!(r["ok"], false, "{r}");
    assert!(r["error"].as_str().unwrap().contains(r#"invalid URL escape "%\xed\x95""#), "{r}");
    // 같은 serve 가 다음 요청에도 답한다
    let r = s.call(json!({"op": "project"}));
    assert_eq!(r["ok"], true, "{r}");
    s.finish(&secrets(tmp.path()));
}

#[test]
fn 복호화_내보내기_접속_편집은_serve_에서도_같은_모양이고_키와_비밀번호를_내지_않는다() {
    let tmp = offline_project(1);
    let mut s = Serve::start(tmp.path());
    s.hello();
    let key = "0123456789abcdef0123456789abcdef";
    let enc = idb_core::statcrypt::encrypt("홍길동", key, "00112233445566778899aabbccddeeff").unwrap();
    let r = s.call(json!({"op": "decrypt", "conn": "app-stg", "cells": [enc, "plain"]}));
    assert_eq!(r["result"], json!({"values": ["홍길동", null], "failed": 0}), "{r}");
    let r = s.call(json!({"op": "decrypt", "conn": "nogroup-stg", "cells": []}));
    assert_eq!(err(&r), (2, "접속 그룹을 몰라 복호화 키를 고를 수 없음", ""));
    // 내보내기: 거절은 접속 전, 닿지 않는 접속은 시작에서 실행 오류
    let r = s.call(json!({"op": "export_start", "conn": "app-prod", "sql": "delete from t"}));
    assert_eq!(err(&r), (2, "읽기 문장만 전체 내보내기", ""));
    let r = s.call(json!({"op": "export_start", "conn": "app-stg", "sql": "select 1"}));
    assert_eq!(err(&r).0, 1);
    let r = s.call(json!({"op": "export_read", "export": 99}));
    assert_eq!(err(&r), (2, "전체 내보내기 없음: 99", ""));
    let r = s.call(json!({"op": "export_cancel", "export": 99}));
    assert_eq!(r["result"], json!({"canceled": false}), "{r}");
    // 접속 파일 쓰기와 비밀번호는 원격에서 받지 않는다
    let conn = json!({"name": "db-x", "kind": "postgresql", "host": "127.0.0.1", "port": 1, "db": "d", "password": "golden-secret-new"});
    for req in [
        json!({"op": "conn_save", "conn": conn}),
        json!({"op": "conn_delete", "name": "logs-stg"}),
        json!({"op": "conn_get", "name": "logs-stg"}),
        json!({"op": "conn_test", "conn": conn}),
    ] {
        let r = s.call(req);
        assert_eq!(err(&r).2, "remote_conn_edit", "{r}");
    }
    let r = s.call(json!({"op": "conn_test", "name": "app-stg"}));
    assert!(err(&r).1.starts_with("접속 실패: "), "{r}");
    let r = s.call(json!({"op": "conn_folder", "names": ["app-stg"], "folder": "ops"}));
    assert_eq!(r["result"], json!({}), "{r}");
    let r = s.call(json!({"op": "project"}));
    let conns = r["result"]["conns"].as_array().unwrap();
    assert!(conns.iter().any(|c| c["name"] == "app-stg" && c["folder"] == "ops"), "{r}");
    assert!(!fs::read_to_string(tmp.path().join(".env")).unwrap().contains("golden-secret-new"));
    let mut sec = secrets(tmp.path());
    sec.push("golden-secret-new".into());
    s.finish(&sec);
}

#[tokio::test]
async fn 원격_엔진으로_복호화와_접속_시험이_로컬과_같은_답을_낸다() {
    use idb_engine::{Engine, LocalEngine, RemoteEngine, RemoteSpec};
    use std::sync::Arc;
    let tmp = offline_project(1);
    let spec = RemoteSpec {
        name: "loop".into(),
        program: env!("CARGO_BIN_EXE_db").into(),
        args: vec!["serve".into(), "-p".into(), tmp.path().to_string_lossy().into_owned()],
    };
    let remote = Engine::Remote(Arc::new(RemoteEngine::connect(&spec).await.unwrap()));
    let proj = idb_core::project::load_project(tmp.path()).unwrap();
    let local = Engine::Local(Arc::new(LocalEngine::new(proj, None)));
    let enc = idb_core::statcrypt::encrypt("a@b.c", "0123456789abcdef0123456789abcdef", "00112233445566778899aabbccddeeff").unwrap();
    for e in [&remote, &local] {
        let d = e.decrypt("app-prod", vec![enc.clone(), "1".into()]).await.unwrap();
        assert_eq!((d.values, d.failed), (vec![Some("a@b.c".to_string()), None], 0));
        let r = e.export_start("app-stg", "show all").await.unwrap_err();
        assert_eq!(r.error, "SELECT 문만 전체 내보내기");
        assert!(!e.export_cancel(7).await.unwrap());
        assert!(e.conn_test_saved("app-stg").await.unwrap_err().error.starts_with("접속 실패: "));
    }
    // 원격만 접속 편집을 거절한다
    let r = remote.conn_get("logs-stg").await.unwrap_err();
    assert_eq!((r.code, r.reason.as_deref()), (3, Some("remote_conn_edit")));
    assert_eq!(local.conn_get("logs-stg").await.unwrap().user, "loki_user");
    remote.close().await;
}
