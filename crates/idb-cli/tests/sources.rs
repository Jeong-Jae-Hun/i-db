#![allow(non_snake_case)]
//! db 의 저장소 명령(query·logs·metrics·tables, R60~R63) 시험. Go `sources_test.go` 의 CLI 시험을 옮겼다.
//! Go 판은 Backend 를 가짜로 바꿔 끼우지만 여기서는 실제 클라이언트를 가짜 HTTP 서버에 붙여 보낸 요청까지 본다.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

const PASSWORD: &str = "loki-pw-7";

/// 받은 요청 하나다(경로·질의 문자열·본문).
#[derive(Clone, Debug)]
struct Seen {
    method: String,
    path: String,
    query: String,
    body: String,
}

type Handler = Box<dyn Fn(&Seen) -> (u16, String) + Send + Sync>;

/// 경로마다 정한 답을 돌려주는 가짜 HTTP 서버다. 없는 경로는 404 다.
struct Fake {
    url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Fake {
    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

fn serve(routes: Vec<(&str, Handler)>) -> Fake {
    let routes: HashMap<String, Handler> = routes.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut r = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if r.read_line(&mut line).is_err() {
                continue;
            }
            let mut parts = line.split_whitespace();
            let (method, target) = (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("").to_string());
            let mut len = 0;
            loop {
                let mut h = String::new();
                if r.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                    break;
                }
                if let Some((k, v)) = h.split_once(':')
                    && k.eq_ignore_ascii_case("content-length")
                {
                    len = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0; len];
            let _ = r.read_exact(&mut body);
            let (path, query) = target.split_once('?').unwrap_or((&target, ""));
            let s = Seen {
                method,
                path: unescape(path),
                query: query.to_string(),
                body: String::from_utf8_lossy(&body).into_owned(),
            };
            log.lock().unwrap().push(s.clone());
            let (code, out) = match routes.get(&s.path) {
                Some(h) => h(&s),
                None => (404, "not found".into()),
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {code} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}",
                out.len()
            );
        }
    });
    Fake { url, seen }
}

/// Go `r.URL.Path` 처럼 %XX 를 푼다.
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(v) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())
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

fn ok(body: &str) -> Handler {
    let body = body.to_string();
    Box::new(move |_| (200, body.clone()))
}

/// PostgreSQL 접속 하나(app-stg)와 저장소 db 전용 접속을 가진 프로젝트다. logs-prod 는 prod, orders 는 prod 프로필이다.
fn project(search: &str, loki: &str, prom: &str, graf: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".idea")).unwrap();
    fs::create_dir_all(dir.path().join(".dbtui")).unwrap();
    fs::write(
        dir.path().join(".idea/dataSources.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?><project version="4"><component name="DataSourceManagerImpl"><data-source name="app-stg" group="grp" uuid="u-app"><driver-ref>postgresql</driver-ref><jdbc-url>jdbc:postgresql://127.0.0.1:1/app</jdbc-url></data-source></component></project>"#,
    )
    .unwrap();
    let conns = format!(
        r#"[
  {{"uuid": "s1", "name": "search-stg", "kind": "opensearch", "url": "{search}"}},
  {{"uuid": "s2", "name": "logs-stg", "kind": "loki", "url": "{loki}", "auth": "basic", "user": "mon"}},
  {{"uuid": "s3", "name": "logs-prod", "kind": "loki", "url": "{loki}"}},
  {{"uuid": "s4", "name": "prom", "kind": "prometheus", "url": "{prom}"}},
  {{"uuid": "s5", "name": "graf", "kind": "grafana", "url": "{graf}", "auth": "token"}},
  {{"uuid": "s6", "name": "ddb", "kind": "dynamodb", "region": "ap-northeast-2"}},
  {{"uuid": "s7", "name": "orders", "kind": "dynamodb", "region": "ap-northeast-2", "profile": "prod-admin"}}
]"#
    );
    fs::write(dir.path().join(".dbtui/connections.json"), conns).unwrap();
    fs::write(
        dir.path().join(".env"),
        format!("LOKI_LOGS_STG_PASSWORD={PASSWORD}\nGRAFANA_GRAF_TOKEN=glsa_x\n"),
    )
    .unwrap();
    dir
}

fn run(dir: &tempfile::TempDir, args: &[&str]) -> (i32, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_db"))
        .args(args)
        .arg("-p")
        .arg(dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn param<'a>(query: &'a str, key: &str) -> &'a str {
    query
        .split('&')
        .find_map(|kv| kv.strip_prefix(key).and_then(|v| v.strip_prefix('=')))
        .unwrap_or("")
}

const STREAMS_EMPTY: &str = r#"{"status":"success","data":{"resultType":"streams","result":[]}}"#;

#[test]
fn query_logs_metrics_는_범위와_한도를_싣고_결과를_찍는다() {
    let search = serve(vec![(
        "/logs-*/_search",
        ok(r#"{"hits":{"hits":[{"_index":"logs-1","_id":"a","_score":1,"_source":{"msg":"hi"}}]}}"#),
    )]);
    let loki = serve(vec![("/loki/api/v1/query_range", ok(STREAMS_EMPTY))]);
    let graf = serve(vec![
        ("/api/datasources", ok(r#"[{"uid":"pr1","name":"Prom","type":"prometheus"}]"#)),
        (
            "/api/datasources/proxy/uid/pr1/api/v1/query_range",
            ok(r#"{"status":"success","data":{"resultType":"matrix","result":[]}}"#),
        ),
    ]);
    let dir = project(&search.url, &loki.url, "http://127.0.0.1:1", &graf.url);

    let (code, out, err) = run(&dir, &["query", "-c", "search-stg", "GET logs-*/_search"]);
    assert_eq!(
        (code, out.as_str(), err.as_str()),
        (0, "_index\t_id\t_score\tmsg\nlogs-1\ta\t1\thi\n", "")
    );
    let r = &search.seen()[0];
    assert_eq!((r.method.as_str(), r.body.as_str()), ("GET", ""));

    let (code, _, err) = run(
        &dir,
        &[
            "logs",
            "-c",
            "logs-stg",
            r#"{app="api"}"#,
            "--since",
            "6h",
            "--until",
            "1h",
            "--max-rows",
            "5",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let q = loki.seen()[0].query.clone();
    let (start, end): (i64, i64) = (param(&q, "start").parse().unwrap(), param(&q, "end").parse().unwrap());
    assert_eq!(end - start, 5 * 3_600_000_000_000, "{q}");
    assert_eq!(param(&q, "limit"), "5");

    let (code, out, err) = run(&dir, &["metrics", "-c", "graf", "up", "--since", "7d", "--format", "json"]);
    assert!(
        code == 0 && out.contains(r#""conn":"graf""#) && out.contains(r#""cols":["time","value"]"#),
        "{code} {out} {err}"
    );
    let q = graf.seen().last().unwrap().query.clone();
    let (start, end): (f64, f64) = (param(&q, "start").parse().unwrap(), param(&q, "end").parse().unwrap());
    assert_eq!((end - start).round() as i64, 7 * 24 * 3600, "{q}");
}

#[test]
fn 쓰기와_비싼_질의는_3이고_force_면_보낸다() {
    let search = serve(vec![(
        "/logs/_search",
        ok(r#"{"hits":{"hits":[{"_index":"logs","_id":"1","_score":null,"_source":{"pk":1}}]}}"#),
    )]);
    let dir = project(&search.url, "http://127.0.0.1:1", "http://127.0.0.1:1", "http://127.0.0.1:1");
    let (code, _, err) = run(&dir, &["query", "-c", "search-stg", "DELETE logs"]);
    assert!(code == 3 && err.contains("읽기 전용 저장소라 보내지 않음"), "{code} {err}");
    let (code, _, err) = run(&dir, &["query", "-c", "search-stg", "GET logs/_search?size=20000"]);
    assert!(code == 3 && err.contains("비싼 질의: size 1만 초과 (--force)"), "{code} {err}");
    assert!(search.seen().is_empty(), "거절한 질의가 나갔다");
    let (code, out, err) = run(&dir, &["query", "-c", "search-stg", "--force", "GET logs/_search?size=20000"]);
    assert!(code == 0 && out.contains("pk\nlogs\t1\tNULL\t1\n"), "{code} {out} {err}");
    assert_eq!(search.seen()[0].query, "size=20000");
}

#[test]
fn prod_는_prod_플래그가_필요하고_열기_전에_거절한다() {
    let loki = serve(vec![("/loki/api/v1/query_range", ok(STREAMS_EMPTY))]);
    let dir = project("http://127.0.0.1:1", &loki.url, "http://127.0.0.1:1", "http://127.0.0.1:1");
    let (code, _, err) = run(&dir, &["logs", "-c", "logs-prod", r#"{a="b"}"#]);
    assert!(code == 3 && err.contains("--prod"), "{code} {err}");
    assert!(loki.seen().is_empty(), "prod 거절은 접속을 열기 전이다");
    let (code, _, err) = run(&dir, &["logs", "-c", "logs-prod", "--prod", r#"{a="b"}"#]);
    assert_eq!(code, 0, "{err}");
    // AWS 프로필이 prod 여도 --prod 가 필요하다
    let (code, _, err) = run(&dir, &["query", "-c", "orders", r#"SELECT * FROM "t" WHERE pk = 'a'"#]);
    assert!(code == 3 && err.contains("--prod"), "{code} {err}");
}

#[test]
fn tables_는_맨_위_목록이고_basic_비밀번호는_오류에_나오지_않는다() {
    let search = serve(vec![
        (
            "/_cat/indices",
            ok(r#"[{"index":"logs-2026","docs.count":"1234","health":"green"}]"#),
        ),
        ("/_cat/aliases", ok(r#"[{"alias":"app","index":"logs-2026"}]"#)),
    ]);
    let loki = serve(vec![(
        "/loki/api/v1/labels",
        Box::new(|_: &Seen| (401u16, format!("bad password {PASSWORD}"))) as Handler,
    )]);
    let dir = project(&search.url, &loki.url, "http://127.0.0.1:1", "http://127.0.0.1:1");
    let (code, out, err) = run(&dir, &["tables", "-c", "search-stg"]);
    assert_eq!(
        (code, out.as_str(), err.as_str()),
        (0, "name\tinfo\napp → logs-2026\talias\nlogs-2026\t1234 green\n", "")
    );
    let (code, _, err) = run(&dir, &["tables", "-c", "logs-stg"]);
    assert!(code == 1 && err.contains("loki 401") && !err.contains(PASSWORD), "{code} {err}");
}

#[test]
fn 저장소에서_안_되는_명령과_플래그는_사용법_오류이고_열지_않는다() {
    let any = serve(vec![]);
    let dir = project(&any.url, &any.url, &any.url, &any.url);
    for args in [
        &["describe", "-c", "search-stg", "t"][..],
        &["ddl", "-c", "search-stg", "t"],
        &["explain", "-c", "ddb", "select 1"],
        &["redis", "-c", "logs-stg", "GET", "k"],
        &["search", "-c", "search-stg", "x"],
        &["schemas", "-c", "prom"],
        &["schema-diff", "-c", "search-stg", "--to", "app-stg"],
        &["schema-diff", "-c", "app-stg", "--to", "search-stg"],
    ] {
        let (code, _, err) = run(&dir, args);
        assert!(code == 2 && err.contains("이 접속 종류에서는 안 됨"), "{args:?}: {code} {err}");
    }
    for args in [
        &["query", "-c", "logs-stg", "--write", "x"][..],
        &["query", "-c", "logs-stg", "--param", "a=1", "x"],
        &["logs", "-c", "prom", "up"],
        &["metrics", "-c", "logs-stg", "x"],
        &["logs", "-c", "app-stg", "x"],
        &["logs", "-c", "logs-stg", "--since", "x", "y"],
        &["logs", "-c", "logs-stg", "--since", "3651d", "y"],
    ] {
        let (code, _, err) = run(&dir, args);
        assert_eq!(code, 2, "{args:?}: 사용법 오류(2)여야 한다: {err}");
    }
    assert!(any.seen().is_empty(), "사용법 오류는 접속을 열지 않는다");
    let (code, _, err) = run(&dir, &["logs", "-c", "logs-stg", "--since", "3650d", "--force", "{a=\"1\"}"]);
    assert_eq!(code, 1, "3650d 는 받는다(서버가 404): {err}");
}
