#![allow(non_snake_case)]
//! db-rs 를 띄워 보는 conn·status·flows·board·저장소(DynamoDB) 명령 시험. Go `conncli_test.go`·`folders_test.go`·
//! `board_test.go`·`flow_test.go` 의 CLI 쪽을 옮겼다. 통합 시험은 DBTUI_TEST_PG_URL·DBTUI_TEST_DYNAMO_URL 이 있을 때만 돈다.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

const PW: &str = "Cli-Sekrit-42";

/// DataGrip 접속 dg-stg(127.0.0.1:1, 닿지 않음) 하나와 GRP 키를 가진 프로젝트다.
fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".idea")).unwrap();
    fs::write(
        dir.path().join(".idea/dataSources.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?><project version="4"><component name="DataSourceManagerImpl">
<data-source name="dg-stg" group="grp" uuid="u-dg"><driver-ref>postgresql</driver-ref><jdbc-url>jdbc:postgresql://127.0.0.1:1/app</jdbc-url></data-source>
</component></project>"#,
    )
    .unwrap();
    fs::write(
        dir.path().join(".env"),
        "GRP_AES_KEY=0123456789abcdef0123456789abcdef\nGRP_AES_IV=00112233445566778899aabbccddeeff\n",
    )
    .unwrap();
    dir
}

fn run(stdin: &str, args: &[&str]) -> (i32, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_db-rs"))
        .args(args)
        .env("DBTUI_PROJECT", "/nonexistent")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
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

fn env_file(dir: &Path) -> String {
    fs::read_to_string(dir.join(".env")).unwrap_or_default()
}

/// 출력 어디에도 비밀번호(원문·인코딩)가 없는지 본다.
fn no_secret(outs: &[&str]) {
    for s in outs {
        assert!(!s.contains(PW), "출력에 비밀번호: {s}");
    }
}

// ─── db conn ───

#[test]
fn conn_add_는_시험이_실패하면_force_없이_저장하지_않는다() {
    let d = project();
    let u = format!("postgres://ro:{PW}@127.0.0.1:1/app");
    let (code, out, err) = run("", &with_p(&["conn", "add", "app-stg2", "--url", &u, "--group", "app"], d.path()));
    assert!(code == 1 && err.contains("접속 시험 실패"), "{code} {err}");
    no_secret(&[&out, &err]);
    assert!(!d.path().join(".dbtui/connections.json").exists(), "저장하지 않는다");
    let (code, out, err) = run(
        "",
        &with_p(&["conn", "add", "app-stg2", "--url", &u, "--group", "app", "--force"], d.path()),
    );
    assert!(
        code == 0 && out.contains("추가함: app-stg2 (db) · PostgreSQL ro@127.0.0.1:1/app"),
        "{code} {out} {err}"
    );
    no_secret(&[&out, &err]);
    assert!(env_file(d.path()).contains(&format!("PG_APP_STG2_PASSWORD={PW}\n")));
    let (_, out, _) = run("", &with_p(&["conns", "--format", "tsv"], d.path()));
    assert!(out.contains("app-stg2\tapp\tpostgresql\tstg\t127.0.0.1\tapp\tdb\tapp\n"), "{out}");
    let (code, _, err) = run("", &with_p(&["conn", "add", "app-stg2", "--url", &u, "--force"], d.path()));
    assert!(code == 2 && err.contains("이미 있음"), "{err}");
}

#[test]
fn conn_사용법_오류는_종료_코드_2() {
    let d = project();
    let mysql = format!("mysql://u:{PW}@h/d");
    let cases: Vec<Vec<&str>> = vec![
        vec!["conn"],
        vec!["conn", "bogus"],
        vec!["conn", "add", "x"],
        vec!["conn", "add", "bad name", "--url", "postgres://h/d"],
        vec!["conn", "add", "x", "--url", &mysql],
        vec!["conn", "add", "x", "--url", "redis://h:1/0", "--kind", "postgresql"],
        vec!["conn", "add", "x", "--url", "postgres://h/d", "--password", "pw"],
        vec!["conn", "edit", "nope", "--host", "h"],
        vec!["conn", "edit", "dg-stg", "--host", "h"],
        vec!["conn", "rm", "dg-stg", "--yes"],
        vec!["conn", "test"],
        vec!["conn", "test", "dg-stg", "--url", "postgres://h/d"],
    ];
    for args in cases {
        let (code, out, err) = run("", &with_p(&args, d.path()));
        assert_eq!(code, 2, "{args:?}: {err}");
        no_secret(&[&out, &err]);
    }
    let (_, _, err) = run("", &with_p(&["conn", "edit", "dg-stg", "--host", "h"], d.path()));
    assert!(err.contains("DataGrip 접속은 DataGrip 에서 고침"), "{err}");
}

#[test]
fn conn_edit_는_password_stdin_으로_비밀번호를_바꾸고_주지_않으면_그대로_둔다() {
    let d = project();
    let u = format!("redis://:{PW}@127.0.0.1:1/0");
    assert_eq!(run("", &with_p(&["conn", "add", "mine", "--url", &u, "--force"], d.path())).0, 0);
    let (code, out, err) = run("", &with_p(&["conn", "edit", "mine", "--db", "3", "--force"], d.path()));
    assert!(
        code == 0 && out.contains("고침: mine (db) · Redis 127.0.0.1:1/3"),
        "{code} {out} {err}"
    );
    assert!(
        env_file(d.path()).contains(&format!("REDIS_MINE_PASSWORD={PW}\n")),
        "비밀번호를 그대로 둔다"
    );
    let (code, out, err) = run(
        "new-Pw-9\n",
        &with_p(&["conn", "edit", "mine", "--password-stdin", "--force"], d.path()),
    );
    assert!(code == 0 && !(out.clone() + &err).contains("new-Pw-9"), "{code} {out} {err}");
    let env = env_file(d.path());
    assert!(env.contains("REDIS_MINE_PASSWORD=new-Pw-9\n") && !env.contains(PW), "{env}");
    let (code, _, err) = run("", &with_p(&["conn", "edit", "mine"], d.path()));
    assert!(code == 2 && err.contains("고칠 값이 없음"), "{err}");
    assert_eq!(
        run("", &with_p(&["conn", "edit", "mine", "--password-stdin", "--force"], d.path())).0,
        2,
        "빈 표준 입력은 2"
    );
    let (code, _, err) = run(
        "",
        &with_p(&["conn", "edit", "mine", "--host", "db.prod.internal", "--force"], d.path()),
    );
    assert_eq!(code, 3, "prod 호스트는 --prod 가 필요: {err}");
}

#[test]
fn conn_rm_은_터미널이_아니면_yes_가_필요하다() {
    let d = project();
    let u = format!("postgres://u:{PW}@127.0.0.1:1/d");
    assert_eq!(run("", &with_p(&["conn", "add", "mine", "--url", &u, "--force"], d.path())).0, 0);
    let (code, _, err) = run("y\n", &with_p(&["conn", "rm", "mine"], d.path()));
    assert!(code == 2 && err.contains("--yes"), "{err}");
    let (code, out, _) = run("", &with_p(&["conn", "rm", "mine", "--yes"], d.path()));
    assert!(code == 0 && out.contains("지움: mine (db)"), "{out}");
    assert!(!env_file(d.path()).contains("PG_MINE_PASSWORD"));
}

#[test]
fn conn_test_는_실패하면_1_이고_비밀번호를_찍지_않는다() {
    let d = project();
    let u = format!("postgres://u:{PW}@127.0.0.1:1/d");
    let (code, out, err) = run("", &with_p(&["conn", "test", "--url", &u], d.path()));
    assert!(code == 1 && err.contains("접속 실패"), "{code} {err}");
    no_secret(&[&out, &err]);
    assert_eq!(
        run("", &with_p(&["conn", "test", "dg-stg"], d.path())).0,
        1,
        "DataGrip 접속도 시험한다"
    );
    assert_eq!(
        run("", &with_p(&["conn", "test", "--url", "postgres://db.prod.internal/d"], d.path())).0,
        3
    );
}

#[test]
fn conn_이름_자리의_URL_은_찾거나_되풀이하기_전에_거절한다() {
    let d = project();
    let u = format!("postgres://u:{PW}@127.0.0.1:1/d");
    for sub in ["test", "edit", "rm", "add"] {
        let (code, out, err) = run("", &with_p(&["conn", sub, &u], d.path()));
        assert!(code == 2 && err.contains("접속 이름은 영숫자·-·_ 만 받음"), "{sub}: {err}");
        no_secret(&[&out, &err]);
        assert!(!(out + &err).contains("127.0.0.1"), "{sub}: URL 을 되풀이한다");
    }
}

#[test]
fn conn_add_는_URL_의_sslmode_를_접속_파일에_남긴다() {
    let d = project();
    let u = format!("postgres://ro:{PW}@10.0.0.9:1/app?sslmode=prefer");
    let (code, _, err) = run(
        "",
        &with_p(&["conn", "add", "app-stg3", "--url", &u, "--force", "--timeout", "2s"], d.path()),
    );
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("경고: TLS 없이 붙으면 비밀번호가 평문으로 감"), "{err}");
    let raw = fs::read_to_string(d.path().join(".dbtui/connections.json")).unwrap();
    assert!(raw.contains(r#""sslmode": "prefer""#), "{raw}");
}

#[test]
fn conn_저장소는_리전으로_더하고_aws_는_url_을_받지_않는다() {
    let d = project();
    let (code, out, err) = run(
        "",
        &with_p(
            &[
                "conn",
                "add",
                "aws-stg",
                "--kind",
                "aws",
                "--region",
                "ap-northeast-2",
                "--profile",
                "nope-golden",
                "--force",
                "--timeout",
                "3s",
            ],
            d.path(),
        ),
    );
    assert!(
        code == 0 && out.contains("추가함: aws-stg (db) · AWS ap-northeast-2 nope-golden"),
        "{code} {out} {err}"
    );
    let (code, _, err) = run(
        "",
        &with_p(
            &["conn", "add", "aws2", "--kind", "aws", "--region", "r", "--url", "https://x"],
            d.path(),
        ),
    );
    assert!(code == 2 && err.contains("aws connection has no url"), "{err}");
    let (code, out, _) = run(
        "",
        &with_p(
            &[
                "conn",
                "add",
                "ath",
                "--kind",
                "athena",
                "--region",
                "ap-northeast-2",
                "--workgroup",
                "wg",
                "--output",
                "s3://b/p/",
                "--force",
                "--timeout",
                "3s",
            ],
            d.path(),
        ),
    );
    assert!(code == 0 && out.contains("Athena ap-northeast-2 wg s3://b/p/"), "{out}");
}

#[test]
fn conn_folder_는_옮기고_이름을_바꾸고_되돌린다() {
    let d = project();
    let (code, out, _) = run("", &with_p(&["conn", "folder", "dg-stg", "moved"], d.path()));
    assert!(code == 0 && out == "옮김: dg-stg → moved\n", "{out}");
    let (_, out, _) = run("", &with_p(&["conns", "--format", "tsv"], d.path()));
    assert!(out.contains("\tdatagrip\tmoved\n"), "{out}");
    let (code, out, _) = run("", &with_p(&["conn", "folder", "--rename", "moved", "renamed"], d.path()));
    assert!(code == 0 && out == "폴더 이름 바꿈: moved → renamed (접속 1)\n", "{out}");
    let (code, _, err) = run("", &with_p(&["conn", "folder", "--rename", "renamed", " "], d.path()));
    assert!(
        code == 2 && err.contains("--rename <옛 폴더> <새 폴더>"),
        "빈 새 이름은 되돌리지 않고 거절: {err}"
    );
    let (code, out, _) = run("", &with_p(&["conn", "folder", "dg-stg", "--reset"], d.path()));
    assert!(code == 0 && out == "되돌림: dg-stg → grp\n", "{out}");
    assert_eq!(fs::read_to_string(d.path().join(".dbtui/folders.json")).unwrap(), "{}\n");
    assert!(
        !fs::read_to_string(d.path().join(".idea/dataSources.xml"))
            .unwrap()
            .contains("moved"),
        ".idea 는 고치지 않는다"
    );
    fs::write(d.path().join(".dbtui/folders.json"), "{broken").unwrap();
    assert_eq!(run("", &with_p(&["conn", "folder", "dg-stg", "x"], d.path())).0, 1);
    assert_eq!(
        fs::read_to_string(d.path().join(".dbtui/folders.json")).unwrap(),
        "{broken",
        "깨진 파일은 덮어쓰지 않는다"
    );
}

// ─── status·flows·board ───

#[test]
fn status_는_prod_를_묻지_않고_닿지_않는_접속은_실패다() {
    let d = project();
    let (code, out, err) = run("", &with_p(&["status", "--format", "json", "--timeout", "5s"], d.path()));
    assert_eq!(code, 1, "{err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v[0]["name"], "dg-stg");
    assert_eq!(v[0]["ok"], false);
    assert!(v[0]["error"].as_str().unwrap().contains("127.0.0.1"), "{out}");
}

#[test]
fn flows_는_설정_오류가_있으면_1_이고_상자마다_한_줄이다() {
    let d = project();
    fs::create_dir_all(d.path().join(".dbtui")).unwrap();
    fs::write(
        d.path().join(".dbtui/flows.json"),
        r#"[{"name":"bad","aws":"dg-stg","queues":[{"name":"x"}]},{"name":"topic","topic":"arn:aws:sns:ap-northeast-2:1:t"}]"#,
    )
    .unwrap();
    let (code, out, err) = run("", &with_p(&["flows", "--format", "json"], d.path()));
    assert_eq!(code, 1, "{err}");
    assert_eq!(
        out,
        "[{\"flow\":\"bad\",\"stage\":\"config\",\"name\":\"bad\",\"metrics\":\"\",\"warn\":false,\"error\":\"설정 오류: aws dg-stg: want kind aws, got postgresql\"},{\"flow\":\"topic\",\"stage\":\"sns\",\"name\":\"t\",\"metrics\":\"\",\"warn\":false}]\n"
    );
    let (code, out, _) = run("", &with_p(&["flows", "-f", "topic", "--format", "tsv"], d.path()));
    assert_eq!(
        (code, out.as_str()),
        (0, "flow\tstage\tname\tmetrics\twarn\ntopic\tsns\tt\t\tfalse\n")
    );
    fs::remove_file(d.path().join(".dbtui/flows.json")).unwrap();
    let (code, out, err) = run("", &with_p(&["flows", "--format", "json"], d.path()));
    assert!(
        code == 0 && out == "[]\n" && err.contains("흐름 없음 (.dbtui/flows.json)"),
        "{code} {out} {err}"
    );
}

#[test]
fn board_실패한_패널이_있으면_1_없는_보드는_2_이름이_없으면_목록이다() {
    let d = project();
    fs::create_dir_all(d.path().join(".dbtui/boards")).unwrap();
    fs::write(
        d.path().join(".dbtui/boards/b.json"),
        r#"{"title":"b","panels":[{"title":"n","conn":"dg-stg","view":"number","query":"select 1"},{"title":"bad","conn":"dg-stg","view":"table","query":"delete from t"}]}"#,
    )
    .unwrap();
    let (code, out, _) = run("", &with_p(&["board", "b", "--format", "json", "--timeout", "5s"], d.path()));
    assert_eq!(code, 1);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(v[0]["error"].as_str().unwrap().contains("127.0.0.1"), "{out}");
    assert!(
        v[1]["error"].as_str().unwrap().starts_with("설정 오류: query is not read-only"),
        "{out}"
    );
    assert_eq!(run("", &with_p(&["board", "nope"], d.path())).0, 2);
    let (code, out, _) = run("", &with_p(&["board", "--format", "tsv"], d.path()));
    assert_eq!((code, out.as_str()), (0, "name\ttitle\tpanels\terror\nb\tb\t2\t\n"));
}

// ─── 통합: 로컬 PostgreSQL ───

fn live_pg_project() -> Option<tempfile::TempDir> {
    let raw = std::env::var("DBTUI_TEST_PG_URL").ok()?;
    let u = url::Url::parse(&raw).ok()?;
    let d = project();
    fs::create_dir_all(d.path().join(".dbtui")).unwrap();
    fs::write(
        d.path().join(".dbtui/connections.json"),
        format!(
            r#"[{{"uuid":"live","name":"live-pg","group":"live","kind":"postgresql","host":"{}","port":{},"db":"{}","user":"{}"}}]"#,
            u.host_str()?,
            u.port().unwrap_or(5432),
            u.path().trim_start_matches('/'),
            u.username()
        ),
    )
    .unwrap();
    fs::write(
        d.path().join(".env"),
        format!("PG_LIVE_PG_PASSWORD={}\n", u.password().unwrap_or("")),
    )
    .unwrap();
    Some(d)
}

#[test]
fn 통합_status_flows_board_는_로컬_PG_를_READ_ONLY_로_본다() {
    let Some(d) = live_pg_project() else { return };
    let (code, out, err) = run("", &with_p(&["status", "-c", "live-pg", "--format", "json"], d.path()));
    assert_eq!(code, 0, "{err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v[0]["ok"], true);
    assert!(v[0]["version"].as_str().unwrap().starts_with("PostgreSQL "), "{out}");
    assert_eq!(v[0]["metrics"][0]["name"], "세션");

    // 아웃박스 테이블은 실제로 만들지 않고 값 목록으로 흉내 낸다: VALUES 서브쿼리를 테이블 자리에 둘 수 없어 임시 뷰 대신 pg 카탈로그를 센다
    fs::write(
        d.path().join(".dbtui/flows.json"),
        r#"[{"name":"cat","outbox":{"conn":"live-pg","table":"pg_catalog.pg_class","type":"relkind"}}]"#,
    )
    .unwrap();
    let (code, out, err) = run("", &with_p(&["flows", "--format", "json"], d.path()));
    assert_eq!(code, 0, "{err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v[0]["stage"], "outbox");
    assert!(v[0]["metrics"].as_str().unwrap().starts_with("대기 "), "{out}");
    assert!(
        v.as_array()
            .unwrap()
            .iter()
            .any(|r| r["stage"] == "outbox-type" && r["name"] == "r"),
        "{out}"
    );

    fs::create_dir_all(d.path().join(".dbtui/boards")).unwrap();
    fs::write(
        d.path().join(".dbtui/boards/b.json"),
        r#"{"panels":[{"title":"one","conn":"live-pg","view":"number","query":"select 1000","warn":">=1,000"},
        {"title":"rows","conn":"live-pg","view":"table","query":"select g from generate_series(1, 60) g"},
        {"title":"flow","view":"flow","flow":"cat"}]}"#,
    )
    .unwrap();
    let (code, out, err) = run("", &with_p(&["board", "b", "--format", "json"], d.path()));
    assert_eq!(code, 0, "{err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!((v[0]["value"].as_str(), v[0]["warn"].as_bool()), (Some("1000"), Some(true)));
    assert_eq!(v[1]["rows"].as_array().unwrap().len(), 50);
    assert_eq!(v[1]["truncated"], true);
    assert_eq!(v[2]["columns"][0], "stage");
    let (_, out, _) = run("", &with_p(&["board", "b", "--format", "tsv"], d.path()));
    assert!(
        out.contains("one\tlive-pg\tnumber\t1000 !\t\n") && out.contains("rows\tlive-pg\ttable\t50행+\t\n"),
        "{out}"
    );
}

// ─── 통합: DynamoDB Local ───

#[test]
fn 통합_DynamoDB_Local_은_요금_막음_없이_트리와_PartiQL_을_본다() {
    let Ok(url) = std::env::var("DBTUI_TEST_DYNAMO_URL") else { return };
    let d = project();
    fs::create_dir_all(d.path().join(".dbtui")).unwrap();
    fs::write(
        d.path().join(".dbtui/connections.json"),
        format!(r#"[{{"uuid":"ddb","name":"ddb-local","kind":"dynamodb","url":"{url}","region":"us-east-1"}}]"#),
    )
    .unwrap();
    let run_aws = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_db-rs"))
            .args(with_p(args, d.path()))
            .env("AWS_ACCESS_KEY_ID", "x")
            .env("AWS_SECRET_ACCESS_KEY", "x")
            .env_remove("DB_ALLOW_BILLED")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    let (code, _, err) = run_aws(&["tables", "-c", "ddb-local", "--format", "tsv"]);
    assert_eq!(code, 0, "{err}");
    let (code, out, err) = run_aws(&["status", "-c", "ddb-local", "--format", "tsv"]);
    assert!(
        code == 0 && out.contains("\tdynamodb\ttrue\t") && out.contains("테이블 "),
        "{out} {err}"
    );
    let (code, _, err) = run_aws(&["query", "-c", "ddb-local", "DELETE FROM \"t\" WHERE pk = 'a'"]);
    assert!(code == 3 && err.contains("읽기 전용 저장소라 보내지 않음"), "{err}");
    let (code, out, _) = run_aws(&["conn", "test", "ddb-local"]);
    assert!(code == 0 && out.starts_with("접속됨 (DynamoDB · "), "{out}");
}

#[test]
fn 통합_conn_은_로컬_컨테이너에_추가하고_시험한다() {
    let (Ok(raw), Ok(addr)) = (std::env::var("DBTUI_TEST_PG_URL"), std::env::var("DBTUI_TEST_REDIS_ADDR")) else {
        return;
    };
    let d = project();
    let pw = url::Url::parse(&raw).unwrap().password().unwrap_or("").to_string();
    let (code, out, err) = run("", &with_p(&["conn", "add", "local-pg", "--url", &raw], d.path()));
    assert!(code == 0 && out.contains("접속됨 (PostgreSQL "), "{code} {out} {err}");
    assert!(!out.contains(&pw) && !err.contains(&pw));
    let redis = format!("redis://{addr}/0");
    let (code, out, err) = run("", &with_p(&["conn", "add", "local-redis", "--url", &redis], d.path()));
    assert!(code == 0 && out.contains("접속됨 ("), "{code} {out} {err}");
    let (code, out, _) = run("", &with_p(&["conn", "test", "local-pg"], d.path()));
    assert!(code == 0 && out.starts_with("접속됨 (PostgreSQL "), "{out}");
    let (code, out, _) = run("", &with_p(&["query", "-c", "local-pg", "select 1 as one"], d.path()));
    assert!(code == 0 && out.contains("one"), "저장한 접속으로 query 가 돈다: {out}");
    // 틀린 비밀번호로 바꾸면 시험이 실패해 저장하지 않는다
    let (code, _, err) = run("wrong-pw\n", &with_p(&["conn", "edit", "local-pg", "--password-stdin"], d.path()));
    assert!(code == 1 && !err.contains("wrong-pw"), "{err}");
    assert!(env_file(d.path()).contains(&format!("PG_LOCAL_PG_PASSWORD={pw}")), "그대로");
}
