#![allow(non_snake_case)]
//! db 를 실제로 띄워 보는 PostgreSQL·Redis 명령 시험. Go `cli_test.go` 의 안전장치·입력·통합 시험을 옮겼다.
//! 가짜 프로젝트의 접속은 닿지 않는 주소(127.0.0.1:1)라 거절은 접속 전에, 허락된 명령은 접속 오류(1)로 끝난다.
//! 통합 시험은 DBTUI_TEST_PG_URL·DBTUI_TEST_REDIS_ADDR 가 있을 때만 돈다.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const PASSWORD: &str = "sekrit-pw-9";
const KEY: &str = "0123456789abcdef0123456789abcdef";
const IV: &str = "00112233445566778899aabbccddeeff";

struct Source<'a> {
    name: &'a str,
    group: &'a str,
    driver: &'a str,
    host: &'a str,
    port: &'a str,
    db: &'a str,
    user: &'a str,
}

fn env_name(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' })
        .collect()
}

/// .idea·.env 를 가진 가짜 프로젝트 폴더다. 각 접속 비밀번호는 PASSWORD, 그룹 키·IV 는 KEY·IV 다.
fn write_project(srcs: &[Source]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".idea")).unwrap();
    let mut ds = String::from(r#"<?xml version="1.0" encoding="UTF-8"?><project version="4"><component name="DataSourceManagerImpl">"#);
    let mut local = String::from(r#"<?xml version="1.0" encoding="UTF-8"?><project version="4"><component name="dataSourceStorageLocal">"#);
    let mut env = String::new();
    let mut groups = Vec::new();
    for s in srcs {
        ds.push_str(&format!(
            r#"<data-source name="{n}" group="{g}" uuid="u-{n}"><driver-ref>{d}</driver-ref><jdbc-url>jdbc:{d}://{h}:{p}/{db}</jdbc-url></data-source>"#,
            n = s.name,
            g = s.group,
            d = s.driver,
            h = s.host,
            p = s.port,
            db = s.db
        ));
        local.push_str(&format!(
            r#"<data-source name="{}" uuid="u-{}"><user-name>{}</user-name></data-source>"#,
            s.name, s.name, s.user
        ));
        let prefix = if s.driver == "redis" { "REDIS_" } else { "PG_" };
        env.push_str(&format!("{prefix}{}_PASSWORD={PASSWORD}\n", env_name(s.name)));
        if !s.group.is_empty() && !groups.contains(&s.group) {
            groups.push(s.group);
            env.push_str(&format!("{g}_AES_KEY={KEY}\n{g}_AES_IV={IV}\n", g = env_name(s.group)));
        }
    }
    ds.push_str("</component></project>");
    local.push_str("</component></project>");
    fs::write(dir.path().join(".idea/dataSources.xml"), ds).unwrap();
    fs::write(dir.path().join(".idea/dataSources.local.xml"), local).unwrap();
    fs::write(dir.path().join(".env"), env).unwrap();
    dir
}

fn src<'a>(name: &'a str, group: &'a str, driver: &'a str, db: &'a str) -> Source<'a> {
    Source {
        name,
        group,
        driver,
        host: "127.0.0.1",
        port: "1",
        db,
        user: "u",
    }
}

/// 접속이 닿지 않는 주소인 프로젝트다. 안전장치가 접속 전에 거절하는지와 사용법 오류를 본다.
fn fake_project() -> tempfile::TempDir {
    write_project(&[
        src("app-stg", "grp", "postgresql", "app"),
        src("app-prod", "grp", "postgresql", "app"),
        src("app-stg-redis", "grp", "redis", "0"),
        src("app-prod-redis", "grp", "redis", "0"),
        src("nogroup-stg", "", "postgresql", "app"),
    ])
}

fn db_rs(args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_db"));
    cmd.args(args).env_remove("DBTUI_PROJECT");
    cmd
}

/// 표준 입력에 stdin 을 넣고 db 를 돌린다. 터미널이 아닌 stdout 이다.
fn run(stdin: &str, args: &[&str]) -> (i32, String, String) {
    let mut child = db_rs(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    // 16MiB 를 넘는 입력은 db 가 다 읽지 않고 끝내 쓰기가 끊길 수 있다
    let _ = input.write_all(stdin.as_bytes());
    drop(input);
    let out = child.wait_with_output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn with_p<'a>(args: &[&'a str], dir: &'a Path) -> Vec<&'a str> {
    let mut v = args.to_vec();
    v.extend(["-p", dir.to_str().unwrap()]);
    v
}

// ─── 입력 ───

#[test]
fn SQL_은_인자_파일_표준입력_순으로_읽는다() {
    let p = fake_project();
    let f = p.path().join("q.sql");
    let write = "update t set a = 1 where id = 1";
    // 쓰기 문장을 읽으면 --write 없이 거절(3), 읽기 문장이면 접속까지 가서 1 이다
    fs::write(&f, "select 2").unwrap();
    let fp = f.to_str().unwrap();
    assert_eq!(
        run("select 3", &with_p(&["query", "-c", "app-stg", "-f", fp, write], p.path())).0,
        3
    );
    fs::write(&f, write).unwrap();
    assert_eq!(run("select 3", &with_p(&["query", "-c", "app-stg", "-f", fp], p.path())).0, 3);
    assert_eq!(run(write, &with_p(&["query", "-c", "app-stg"], p.path())).0, 3);
    assert_eq!(run("select 3", &with_p(&["query", "-c", "app-stg"], p.path())).0, 1);
    let (code, _, errw) = run("", &with_p(&["query", "-c", "app-stg"], p.path()));
    assert_eq!((code, errw.as_str()), (2, "실행할 글이 없음 (인자, -f 파일, 표준 입력)\n"));
    let none = p.path().join("none.sql");
    let (code, _, errw) = run("", &with_p(&["query", "-c", "app-stg", "-f", none.to_str().unwrap()], p.path()));
    assert!(code == 1 && errw.contains("no such file or directory"), "{code} {errw}");
}

#[test]
fn enc_를_그룹_키로_확장하고_파라미터를_바인딩한다() {
    let p = fake_project();
    // 확장 알림은 접속 전에 stderr 로 가고 키는 싣지 않는다
    let (code, _, errw) = run(
        "",
        &with_p(
            &[
                "query",
                "-c",
                "app-stg",
                "--param",
                "id=7",
                "select * from u where email = enc('a@b.com') and id = :id and n = :id",
            ],
            p.path(),
        ),
    );
    assert!(
        code == 1 && errw.starts_with("enc() 1개 암호화\n") && !errw.contains(KEY),
        "{code} {errw}"
    );
    // 그룹 키가 없으면 서버 staticEncrypt 로 바꾼다
    let (_, _, errw) = run("", &with_p(&["query", "-c", "nogroup-stg", "select enc('a')"], p.path()));
    assert!(errw.starts_with("키·IV 가 없어 서버 staticEncrypt 로 실행\n"), "{errw}");
    for (args, msg) in [
        (vec!["select :x"], "파라미터 값이 없음: x (--param x=값)"),
        (vec!["--param", "x=1", "select 1"], "문장에 없는 파라미터: x"),
        (vec!["select 1; select 2"], "문장 하나만 실행함 (2개)"),
        (vec!["select enc(email) from u"], "argument must be a single string literal"),
    ] {
        let mut a = vec!["query", "-c", "app-stg"];
        a.extend(args.iter());
        let (code, _, errw) = run("", &with_p(&a, p.path()));
        assert!(code == 2 && errw.contains(msg), "{args:?}: {code} {errw}");
    }
}

#[test]
fn 표준입력이_닫히지_않으면_timeout_에_1로_끝난다() {
    let p = fake_project();
    let mut child = db_rs(&with_p(&["query", "-c", "app-stg", "--timeout", "200ms"], p.path()))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let held = child.stdin.take();
    let start = Instant::now();
    let out = child.wait_with_output().unwrap();
    drop(held);
    let errw = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.code() == Some(1) && start.elapsed() < Duration::from_secs(2) && errw.contains("시간 초과(--timeout 200ms)"),
        "{:?} {:?} {errw}",
        out.status,
        start.elapsed()
    );
}

#[test]
fn 표준입력이_16MiB_를_넘으면_오류다() {
    let p = fake_project();
    let big = format!("select '{}'", "x".repeat(16 << 20));
    let (code, _, errw) = run(&big, &with_p(&["query", "-c", "app-stg"], p.path()));
    assert!(code == 1 && errw.contains("16MiB"), "{code} {errw}");
}

// ─── 안전장치·종료 코드 ───

#[test]
fn prod_는_읽기도_prod_플래그가_없으면_3() {
    let p = fake_project();
    for args in [
        vec!["query", "-c", "app-prod", "select 1"],
        vec!["schemas", "-c", "app-prod"],
        vec!["redis", "-c", "app-prod-redis", "GET", "k"],
        vec!["schema-diff", "-c", "app-stg", "--to", "app-prod"],
        vec!["linked", "-c", "app-prod", "--column", "user_id", "--value", "1"],
    ] {
        assert_eq!(
            run("", &with_p(&args, p.path())),
            (3, String::new(), "prod 접속은 --prod 가 필요함\n".into()),
            "{args:?}"
        );
    }
    let (code, _, errw) = run("", &with_p(&["user360", "a@b.com"], p.path()));
    assert!(code == 3 && errw.contains("--prod"), "{errw}");
    // --prod 가 있으면 거절하지 않고 접속까지 간다(닿지 않는 주소라 실행 오류 1)
    let (code, _, errw) = run("", &with_p(&["query", "-c", "app-prod", "--prod", "select 1"], p.path()));
    assert!(
        code == 1 && errw.starts_with("failed to connect to `user=u database=app`:"),
        "{errw}"
    );
    assert!(!errw.contains(PASSWORD), "{errw}");
}

#[test]
fn prod_쓰기는_prod_플래그가_있어도_거절한다() {
    let p = fake_project();
    for args in [
        vec![
            "query",
            "-c",
            "app-prod",
            "--prod",
            "--write",
            "--force",
            "update t set a = 1 where id = 1",
        ],
        vec!["redis", "-c", "app-prod-redis", "--prod", "--write", "SET", "k", "v"],
    ] {
        let (code, _, errw) = run("", &with_p(&args, p.path()));
        assert!(code == 3 && errw.contains("prod 쓰기"), "{args:?}: {code} {errw}");
    }
}

#[test]
fn 쓰기_없는_쓰기_문장과_위험_문장은_force_없이_거절한다() {
    let p = fake_project();
    for (args, msg) in [
        (vec!["query", "-c", "app-stg", "delete from t where id = 1"], "--write"),
        (vec!["query", "-c", "app-stg", "select nextval('s')"], "--write"),
        (
            vec!["query", "-c", "app-stg", "--write", "delete from t"],
            "위험: WHERE 없는 DELETE (--force)",
        ),
        (vec!["query", "-c", "app-stg", "--write", "drop table t"], "위험: DROP (--force)"),
        (vec!["explain", "-c", "app-stg", "explain analyze select 1"], "ANALYZE"),
        (vec!["explain", "-c", "app-stg", "explain (analyze) delete from t"], "ANALYZE"),
        (vec!["redis", "-c", "app-stg-redis", "SET", "k", "v"], "--write"),
        (
            vec!["redis", "-c", "app-stg-redis", "--write", "FLUSHALL"],
            "위험: FLUSHALL (--force)",
        ),
    ] {
        let (code, _, errw) = run("", &with_p(&args, p.path()));
        assert!(code == 3 && errw.contains(msg), "{args:?}: {code} {errw}");
    }
    // 허락되면 거절하지 않고 접속까지 간다
    for args in [
        vec!["query", "-c", "app-stg", "--write", "--force", "delete from t"],
        vec!["query", "-c", "app-stg", "--write", "delete from t where id = 1"],
        vec!["explain", "-c", "app-stg", "delete from t"],
        vec!["redis", "-c", "app-stg-redis", "GET", "k"],
        vec!["redis", "-c", "app-stg-redis", "--write", "SET", "k", "v"],
    ] {
        let (code, _, errw) = run("", &with_p(&args, p.path()));
        assert_eq!(code, 1, "{args:?}: {errw}");
    }
}

#[test]
fn 사용법_오류는_2() {
    let p = fake_project();
    for args in [
        vec!["query", "select 1"],
        vec!["query", "-c", "none", "select 1"],
        vec!["query", "-c", "app-stg-redis", "select 1"],
        vec!["query", "-c", "app-stg"],
        vec!["describe", "-c", "app-stg"],
        vec!["describe", "-c", "app-stg", "a", "b"],
        vec!["search", "-c", "app-stg"],
        vec!["linked", "-c", "app-stg", "--column", "id", "--value", "1"],
        vec!["schema-diff", "-c", "app-stg"],
        vec!["user360", "--prod"],
        vec!["query", "--bogus"],
    ] {
        let (code, _, errw) = run("", &with_p(&args, p.path()));
        assert_eq!(code, 2, "{args:?}: {errw}");
    }
    let (code, _, errw) = run("", &["query", "-h"]);
    assert!(code == 0 && errw.contains("사용법: db query"), "{errw}");
}

#[test]
fn write_force_decrypt_는_허용된_명령에서만_받는다() {
    let p = fake_project();
    for args in [
        vec!["explain", "-c", "app-stg", "--write", "delete from t"],
        vec!["explain", "-c", "app-stg", "--force", "select 1"],
        vec!["schemas", "-c", "app-stg", "--write"],
        vec!["describe", "-c", "app-stg", "--force", "t"],
        vec!["ddl", "-c", "app-stg", "--decrypt", "t"],
        vec!["redis", "-c", "app-stg-redis", "--decrypt", "GET", "k"],
        vec!["explain", "-c", "app-stg", "--decrypt", "select 1"],
        vec!["query", "-c", "app-stg", "--force", "select 1"],
    ] {
        let (code, _, errw) = run("", &with_p(&args, p.path()));
        assert_eq!(code, 2, "{args:?}: {errw}");
    }
    // --prod 는 어디서나 받고, 허용된 조합은 접속까지 간다(닿지 않는 주소라 1)
    for args in [
        vec!["explain", "-c", "app-stg", "--prod", "select 1"],
        vec!["query", "-c", "app-stg", "--decrypt", "select 1"],
        vec!["search", "-c", "app-stg", "--decrypt", "x"],
    ] {
        let (code, _, errw) = run("", &with_p(&args, p.path()));
        assert_eq!(code, 1, "{args:?}: {errw}");
    }
}

#[test]
fn redis_KEYS_는_SCAN_을_권하며_거절한다() {
    let p = fake_project();
    for args in [
        vec!["redis", "-c", "app-stg-redis", "KEYS", "user:*"],
        vec!["redis", "-c", "app-stg-redis", "--write", "keys", "*"],
    ] {
        let (code, _, errw) = run("", &with_p(&args, p.path()));
        assert!(
            code == 3 && errw.contains("SCAN 0 MATCH") && errw.contains("COUNT 1000"),
            "{args:?}: {errw}"
        );
    }
    let (code, _, errw) = run("", &with_p(&["redis", "-c", "app-stg-redis", "SCAN", "0"], p.path()));
    assert!(
        code == 1 && errw.contains("plain:") && errw.contains("tls:"),
        "SCAN 은 접속까지 간다: {errw}"
    );
}

#[test]
fn write_는_쓰기_동사만_받고_숨은_쓰기는_force_가_필요하다() {
    let p = fake_project();
    for (sql, msg) in [
        ("do $$ begin perform 1; end $$", "--write 로 실행하지 않는 문장: DO"),
        ("call p()", "CALL"),
        ("copy t to program 'rm -rf /'", "COPY"),
        ("copy t from '/etc/passwd'", "COPY"),
        ("set role postgres", "SET"),
        (
            "with x as (delete from t returning 1) select * from x",
            "위험: CTE 안 쓰기 (--force)",
        ),
        ("select * into t2 from t", "위험: SELECT INTO (--force)"),
        (
            "update t set a = pg_terminate_backend(1) where id = 1",
            "위험: 부수효과 함수 pg_terminate_backend (--force)",
        ),
    ] {
        let (code, _, errw) = run("", &with_p(&["query", "-c", "app-stg", "--write", sql], p.path()));
        assert!(code == 3 && errw.contains(msg), "{sql}: {code} {errw}");
    }
    // COPY 는 --force 여도 거절하고, 숨은 쓰기는 --force 면 접속까지 간다
    assert_eq!(
        run(
            "",
            &with_p(&["query", "-c", "app-stg", "--write", "--force", "copy t to program 'x'"], p.path())
        )
        .0,
        3
    );
    for args in [
        vec!["--force", "with x as (delete from t returning 1) select * from x"],
        vec!["insert into t values (1)"],
        vec!["create index i on t (a)"],
        vec!["vacuum t"],
        vec!["refresh materialized view v"],
    ] {
        let mut a = vec!["query", "-c", "app-stg", "--write"];
        a.extend(args.iter());
        let (code, _, errw) = run("", &with_p(&a, p.path()));
        assert_eq!(code, 1, "{args:?}: {errw}");
    }
}

// ─── 통합 (DBTUI_TEST_PG_URL·DBTUI_TEST_REDIS_ADDR 가 있을 때만) ───

/// URL 의 %XX 를 푼다.
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
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

fn integration_project() -> Option<tempfile::TempDir> {
    let raw = std::env::var("DBTUI_TEST_PG_URL").ok().filter(|s| !s.is_empty())?;
    let addr = std::env::var("DBTUI_TEST_REDIS_ADDR").ok().filter(|s| !s.is_empty())?;
    let u = url::Url::parse(&raw).ok()?;
    let (rhost, rport) = addr.rsplit_once(':')?;
    let port = u.port().unwrap_or(5432).to_string();
    let db = unescape(u.path().trim_start_matches('/'));
    let user = unescape(u.username());
    let dir = write_project(&[
        Source {
            name: "clitest-local",
            group: "clitest",
            driver: "postgresql",
            host: u.host_str()?,
            port: &port,
            db: &db,
            user: &user,
        },
        Source {
            name: "clitest-local-redis",
            group: "clitest",
            driver: "redis",
            host: rhost,
            port: rport,
            db: "0",
            user: "",
        },
    ]);
    // 시험 PG 비밀번호는 접속 URL 의 것이다(.env 는 URL 인코딩으로 읽는다)
    let env_path = dir.path().join(".env");
    let env = fs::read_to_string(&env_path).ok()?;
    let pw: String = url::form_urlencoded::byte_serialize(unescape(u.password().unwrap_or_default()).as_bytes()).collect();
    fs::write(
        &env_path,
        env.replacen(
            &format!("PG_CLITEST_LOCAL_PASSWORD={PASSWORD}"),
            &format!("PG_CLITEST_LOCAL_PASSWORD={pw}"),
            1,
        ),
    )
    .ok()?;
    Some(dir)
}

fn with<'a>(mut v: Vec<&'a str>, extra: &[&'a str]) -> Vec<&'a str> {
    v.extend(extra);
    v
}

fn json(args: &[&str]) -> serde_json::Value {
    let mut a = args.to_vec();
    a.extend(["--format", "json"]);
    let (code, out, errw) = run("", &a);
    assert_eq!(code, 0, "{args:?}: {errw}");
    serde_json::from_str(&out).unwrap()
}

fn ok(args: &[&str]) {
    let (code, _, errw) = run("", args);
    assert_eq!(code, 0, "{args:?}: {errw}");
}

#[test]
fn 통합_PG_읽기_쓰기_enc_decrypt_행_상한() {
    let Some(dir) = integration_project() else {
        eprintln!("DBTUI_TEST_PG_URL / DBTUI_TEST_REDIS_ADDR not set");
        return;
    };
    let d = dir.path().to_str().unwrap();
    let q = |sql: &'static str| vec!["query", "-p", d, "-c", "clitest-local", sql];
    ok(&with(q("drop schema if exists clitest_rs cascade"), &["--write", "--force"]));
    ok(&with(q("create schema clitest_rs"), &["--write"]));
    ok(&with(q("create table clitest_rs.t (id int primary key, email text)"), &["--write"]));
    // 읽기 경로는 서버가 막는다. 판정이 놓치는 CTE 안 쓰기·select into 도 실행 오류(1)이고 행이 생기지 않는다
    for sql in [
        "with x as (insert into clitest_rs.t values (99, 'x') returning 1) select * from x",
        "select 1 into clitest_rs.t2",
    ] {
        let (code, _, errw) = run("", &q(sql));
        assert!(code == 1 && errw.contains("SQLSTATE"), "{sql}: {code} {errw}");
    }
    let m = json(&with(
        q("insert into clitest_rs.t values (:id, enc('a@b.com'))"),
        &["--write", "--param", "id=1"],
    ));
    assert_eq!(m["text"], "INSERT 0 1");
    let m = json(&q("select id, email from clitest_rs.t where email = enc('a@b.com')"));
    assert_eq!(m["rows"][0][0], "1");
    assert!(m["rows"][0][1].as_str().unwrap().starts_with(IV), "{m}");
    assert_eq!(m["types"], serde_json::json!(["int4", "text"]));
    assert_eq!(m["conn"], "clitest-local");
    let m = json(&with(q("select email from clitest_rs.t"), &["--decrypt"]));
    assert_eq!(m["rows"], serde_json::json!([["a@b.com"]]));

    // 풀지 못한 칸은 decrypt_failed 로 세고 종료 코드 1, 64KiB 를 넘는 칸은 cells_truncated 로 센다
    let bad = format!("v={IV}^|^{}", "00".repeat(16));
    let (code, out, errw) = run(
        "",
        &[
            "query",
            "-p",
            d,
            "-c",
            "clitest-local",
            "select :v as e",
            "--param",
            &bad,
            "--decrypt",
            "--format",
            "json",
        ],
    );
    assert!(
        code == 1 && out.contains(r#""decrypt_failed":1"#) && errw.contains("1칸 풀지 못함"),
        "{code} {out} {errw}"
    );
    let (code, out, errw) = run("", &with(q("select repeat('a', 70000) as v"), &["--format", "json"]));
    assert!(
        code == 0 && out.contains(r#""cells_truncated":1"#) && errw.contains("칸 1개가 64KiB 에서 잘림"),
        "{errw}"
    );

    let m = json(&with(q("select g from generate_series(1, 2000) g"), &["--max-rows", "1500"]));
    assert!(m["rows"].as_array().unwrap().len() == 1500 && m["truncated"] == true);
    let m = json(&with(q("select g from generate_series(1, 3) g"), &["--max-rows", "3"]));
    assert!(m["rows"].as_array().unwrap().len() == 3 && m["truncated"] == false);
    let (code, _, errw) = run("", &q("select * from clitest_rs.none"));
    assert!(code == 1 && !errw.contains(PASSWORD), "{errw}");

    // 표준 입력·explain·구조
    let (code, out, _) = run(
        "select count(*) as n from clitest_rs.t;\n",
        &["query", "-p", d, "-c", "clitest-local"],
    );
    assert_eq!((code, out.as_str()), (0, "n\n1\n"));
    let (code, out, _) = run("", &["explain", "-p", d, "-c", "clitest-local", "delete from clitest_rs.t"]);
    assert!(code == 0 && out.contains("Delete on t"), "{out}");
    assert_eq!(
        json(&q("select count(*) from clitest_rs.t"))["rows"],
        serde_json::json!([["1"]]),
        "explain 이 delete 를 실행했다"
    );
    let (code, out, _) = run("", &["tables", "-p", d, "-c", "clitest-local", "-s", "clitest_rs"]);
    assert_eq!((code, out.as_str()), (0, "table\nt\n"));
    let m = json(&["describe", "-p", d, "-c", "clitest-local", "clitest_rs.t"]);
    assert!(
        m["rows"][1][1] == "text" && m["text"].as_str().unwrap().contains("PRIMARY KEY"),
        "{m}"
    );
    let m = json(&["ddl", "-p", d, "-c", "clitest-local", "-s", "clitest_rs", "t"]);
    assert!(m["text"].as_str().unwrap().contains("CREATE TABLE"));
    let enc_prefix = &IV[..10];
    let m = json(&["search", "-p", d, "-c", "clitest-local", "-s", "clitest_rs", enc_prefix]);
    assert_eq!(m["rows"].as_array().unwrap().len(), 1, "{m}");
    let m = json(&[
        "schema-diff",
        "-p",
        d,
        "-c",
        "clitest-local",
        "--to",
        "clitest-local",
        "-s",
        "clitest_rs",
        "--to-schema",
        "public",
    ]);
    assert!(m["rows"].to_string().contains("테이블 t"), "{m}");

    // user360: 조회마다 한 칸, 그룹 키로 암호화한 :email_enc, 실패한 조회는 오류 칸이고 종료 코드 1
    fs::create_dir_all(dir.path().join(".dbtui")).unwrap();
    fs::write(
        dir.path().join(".dbtui/user360.json"),
        r#"[
		{"title": "사용자", "conn": "clitest-local", "by": ["email", "id"], "sql": "select id from clitest_rs.t where email = :email_enc or id = :id::int"},
		{"title": "없는 접속", "conn": "clitest-none", "by": ["email"], "sql": "select :email"}]"#,
    )
    .unwrap();
    let (code, out, errw) = run("", &["user360", "-p", d, "--prod", "a@b.com", "--format", "json"]);
    let secs: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(
        code == 1
            && secs[0]["title"] == "사용자"
            && secs[0]["rows"] == serde_json::json!([["1"]])
            && secs[1]["error"] == "접속 없음: clitest-none"
            && errw.contains("없는 접속"),
        "{code} {out} {errw}"
    );
    ok(&with(q("drop schema clitest_rs cascade"), &["--write", "--force"]));
}

#[test]
fn 통합_Redis_읽기는_바로_쓰기는_write_로() {
    let Some(dir) = integration_project() else {
        eprintln!("DBTUI_TEST_PG_URL / DBTUI_TEST_REDIS_ADDR not set");
        return;
    };
    let d = dir.path().to_str().unwrap();
    let rc = |extra: &[&'static str]| {
        let mut v = vec!["redis", "-p", d, "-c", "clitest-local-redis"];
        v.extend(extra);
        v
    };
    ok(&rc(&["--write", "SET", "clitest:rs:k", "값 1"]));
    let m = json(&rc(&["GET", "clitest:rs:k"]));
    assert!(m["text"] == "값 1" && m["conn"] == "clitest-local-redis", "{m}");
    // 인자 하나짜리 명령 줄은 나눠 보낸다
    let (code, out, _) = run("", &rc(&["EXISTS clitest:rs:k"]));
    assert_eq!((code, out.as_str()), (0, "1\n"));
    ok(&rc(&["--write", "DEL", "clitest:rs:k"]));
}
