//! Go `internal/mcpserver/server_test.go`·`source_test.go` 를 옮겼다. 접속 시험은 로컬 엔진(닿지 않는 포트 1)으로,
//! 저장소 시험은 Engine 요청을 적고 정한 값을 돌려주는 가짜로 한다. 통합 시험은 DBTUI_TEST_PG_URL·DBTUI_TEST_REDIS_ADDR 가 있을 때만 돈다.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use idb_core::project::{Conn, Kind, Project};
use idb_engine::{EngineError, LocalEngine, REASON_BILLED, REASON_CONFIRM_EXPENSIVE, Request};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use super::*;

// ─── 돕는 것 ───

fn conn(name: &str, kind: Kind) -> Conn {
    Conn {
        name: name.into(),
        uuid: format!("u-{name}"),
        kind: Some(kind),
        host: "127.0.0.1".into(),
        port: 1,
        ..Default::default()
    }
}

/// 닿지 않는 주소(포트 1)라 도구가 거부하지 않고 접속을 시도하면 접속 오류가 나 구분된다.
fn unit_conns() -> Vec<Conn> {
    let mut app = conn("app-local", Kind::Pg);
    (app.group, app.db, app.user, app.password) = ("app".into(), "app".into(), "u".into(), "s3cr3t-pw".into());
    let mut prod = conn("app-prod", Kind::Pg);
    (prod.db, prod.user, prod.password) = ("app".into(), "u".into(), "prod-pw".into());
    let mut cache = conn("cache-local", Kind::Redis);
    cache.password = "redis-pw".into();
    vec![app, prod, cache, conn("cache-prod", Kind::Redis)]
}

/// 로컬 엔진 위의 서버다. 시험이 끝나도 닫지 않는다(연 접속이 없거나 포트 1 이다).
async fn local(conns: Vec<Conn>, opt: Options) -> McpServer {
    let engine = Engine::Local(Arc::new(LocalEngine::new(
        Project {
            dir: Default::default(),
            conns,
            mapping: Default::default(),
            warnings: Vec::new(),
            aes_env: Default::default(),
        },
        None,
    )));
    let summaries = engine.project().await.unwrap().conns;
    let call: CallFn = Arc::new(move |req| {
        let engine = engine.clone();
        Box::pin(async move { engine.call(req).await })
    });
    McpServer::new(call, summaries, opt)
}

/// (본문 텍스트, 오류 여부)
async fn call(s: &McpServer, name: &str, args: Value) -> (String, bool) {
    let r = match s.call_tool_json(name, args).await {
        Ok(r) => r,
        Err(e) => return (e, true),
    };
    let text = r
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<String>();
    (text, r.is_error == Some(true))
}

fn src_conn(name: &str, group: &str, kind: Kind, region: &str, profile: &str) -> Conn {
    Conn {
        name: name.into(),
        uuid: format!("s-{name}"),
        group: group.into(),
        kind: Some(kind),
        region: region.into(),
        profile: profile.into(),
        ..Default::default()
    }
}

fn source_conns() -> Vec<Conn> {
    let mut logs = src_conn("logs-local", "", Kind::Loki, "", "");
    (logs.url, logs.auth, logs.user, logs.password) = ("https://loki.example".into(), "basic".into(), "u".into(), "loki-pw".into());
    vec![
        src_conn("athena-local", "data", Kind::Athena, "ap-northeast-2", "data-ro"),
        src_conn("athena-prod", "", Kind::Athena, "ap-northeast-2", ""),
        src_conn("aws-local", "", Kind::Aws, "us-east-1", "infra"),
        src_conn("aws-prod", "", Kind::Aws, "ap-northeast-2", ""),
        logs,
    ]
}

/// 저장소 serve 를 흉내 낸다. athena 처럼 force 없는 "select" 는 보내기 전에 비싼 질의 거절이다.
fn fake_source(req: &Request) -> Result<Value, EngineError> {
    Ok(match req {
        Request::SourceChildren { path, .. } if path.is_empty() => {
            json!([{"name": "SQS 큐", "path": "sqs", "info": "", "has_children": true, "openable": false, "warn": false}])
        }
        Request::SourceChildren { .. } => json!([{
            "name": "orders", "path": "sqs/orders", "info": "대기 12 · DLQ orders-dlq 3",
            "has_children": false, "openable": true, "warn": true
        }]),
        Request::SourceRun(r) if r.text.starts_with("select") && !r.force => {
            return Err(EngineError::refused(REASON_CONFIRM_EXPENSIVE, "비싼 질의: 전체 스캔"));
        }
        Request::SourceRun(_) => {
            let rows: Vec<Vec<String>> = (0..10).map(|i| vec![i.to_string()]).collect();
            json!({"cols": ["n"], "rows": rows, "text": "스캔 1.2GB", "truncated": false, "elapsed_ms": 0})
        }
        Request::SourceOpen { .. } => json!({
            "cols": ["attribute", "value"], "rows": [["QueueUrl", "https://sqs/orders"]],
            "text": "", "truncated": false, "elapsed_ms": 0
        }),
        r => return Err(EngineError::usage(format!("unexpected {r:?}"))),
    })
}

/// 가짜 serve 위의 서버와 받은 요청 목록이다.
async fn source_server(opt: Options) -> (McpServer, Arc<Mutex<Vec<Request>>>) {
    let summaries = local(source_conns(), Options::default()).await.conns;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let call: CallFn = Arc::new(move |req| {
        log.lock().unwrap().push(req.clone());
        let r = fake_source(&req);
        Box::pin(async move { r })
    });
    (McpServer::new(call, summaries, opt), seen)
}

// ─── 접속 도구 ───

#[tokio::test]
async fn list_connections_는_비밀번호_없이_이름_종류_환경을_준다() {
    let s = local(unit_conns(), Options::default()).await;
    let (text, is_err) = call(&s, "list_connections", json!({})).await;
    assert!(!is_err, "{text}");
    for pw in ["s3cr3t-pw", "prod-pw", "redis-pw", "Password"] {
        assert!(!text.contains(pw), "비밀번호가 나가면 안 된다: {text}");
    }
    let out: ListConnectionsOutput = serde_json::from_str(&text).unwrap();
    let want = ConnInfo {
        name: "app-prod".into(),
        group: "기타".into(),
        kind: "postgresql".into(),
        env: "prod".into(),
        region: String::new(),
        profile: String::new(),
    };
    assert!(
        out.connections.len() == 4 && out.connections[1] == want && out.connections[0].group == "app",
        "{:?}",
        out.connections
    );
}

#[tokio::test]
async fn 도구_prod_접속은_허용하지_않으면_접속도_하지_않고_거부한다() {
    let s = local(unit_conns(), Options::default()).await;
    for (name, args) in [
        ("list_schemas", json!({"connection": "app-prod"})),
        ("list_tables", json!({"connection": "app-prod", "schema": "public"})),
        (
            "describe_table",
            json!({"connection": "app-prod", "schema": "public", "table": "t"}),
        ),
        ("query", json!({"connection": "app-prod", "sql": "select 1"})),
        ("redis_scan", json!({"connection": "cache-prod"})),
        ("redis_get", json!({"connection": "cache-prod", "key": "k"})),
        ("redis_command", json!({"connection": "cache-prod", "args": ["GET", "k"]})),
    ] {
        let (text, is_err) = call(&s, name, args).await;
        assert!(is_err && text.contains("is prod"), "{name}: prod 거부여야 한다: {text}");
    }
    // 허용하면 거부하지 않고 접속을 시도한다(포트 1 이라 접속 오류)
    let s = local(
        unit_conns(),
        Options {
            allow_prod: true,
            ..Default::default()
        },
    )
    .await;
    let (text, is_err) = call(&s, "query", json!({"connection": "app-prod", "sql": "select 1"})).await;
    assert!(is_err && !text.contains("is prod"), "allow_prod 면 접속을 시도해야 한다: {text}");
}

#[tokio::test]
async fn 도구_쓰기_SQL과_쓰기_Redis_명령과_KEYS_는_접속_전에_거부한다() {
    let s = local(unit_conns(), Options::default()).await;
    for (name, args) in [
        ("query", json!({"connection": "app-local", "sql": "delete from t"})),
        ("query", json!({"connection": "app-local", "sql": "explain analyze delete from t"})),
        ("query", json!({"connection": "app-local", "sql": "set role postgres"})),
        ("redis_command", json!({"connection": "cache-local", "args": ["SET", "k", "v"]})),
        ("redis_command", json!({"connection": "cache-local", "args": ["SLOWLOG", "RESET"]})),
        ("redis_command", json!({"connection": "cache-local", "args": []})),
        ("redis_command", json!({"connection": "cache-local", "args": null})),
        ("redis_command", json!({"connection": "cache-local", "args": ["keys", "*"]})),
        ("query", json!({"connection": "app-local", "sql": "select pg_terminate_backend(1)"})),
        ("redis_command", json!({"connection": "cache-local", "args": ["HGETALL", "k"]})),
    ] {
        let (text, is_err) = call(&s, name, args.clone()).await;
        assert!(
            is_err && (text.contains("read-only") || text.contains("redis_scan") || text.contains("whole collection")),
            "{name} {args}: 접속 전 거부여야 한다: {text}"
        );
    }
}

#[tokio::test]
async fn 도구_접속_이름과_종류가_맞지_않으면_거부한다() {
    let s = local(unit_conns(), Options::default()).await;
    let (text, is_err) = call(&s, "query", json!({"connection": "nope", "sql": "select 1"})).await;
    assert!(is_err && text.contains("unknown connection"), "없는 접속: {text}");
    let (text, is_err) = call(&s, "redis_get", json!({"connection": "app-local", "key": "k"})).await;
    assert!(is_err && text.contains("not redis"), "종류 불일치: {text}");
}

#[tokio::test]
async fn 도구_스키마에_없는_인자와_빠진_필수_인자는_거부한다() {
    let s = local(unit_conns(), Options::default()).await;
    let (text, is_err) = call(&s, "query", json!({"connection": "app-local", "sql": "select 1", "write": true})).await;
    assert!(is_err && text.contains("invalid params"), "{text}");
    let (text, is_err) = call(&s, "describe_table", json!({"connection": "app-local", "schema": "public"})).await;
    assert!(is_err && text.contains("invalid params"), "{text}");
}

#[tokio::test]
async fn 도구_접속_오류에_실린_비밀번호를_지운다() {
    // 호스트 이름이 비밀번호를 담고 있어 DNS 오류 글에 비밀번호가 실린다. 끝 점은 검색 도메인을 붙여 느려지는 것을 막는다
    let mut c = conn("leak-local", Kind::Pg);
    (c.host, c.port, c.user, c.password) = ("leaky-pw.invalid.".into(), 5432, "u".into(), "leaky-pw".into());
    let s = local(vec![c], Options::default()).await;
    let (text, is_err) = call(&s, "list_schemas", json!({"connection": "leak-local"})).await;
    assert!(
        is_err && !text.contains("leaky-pw") && text.contains("***"),
        "비밀번호를 지워야 한다: {text}"
    );
}

#[test]
fn bound_result_는_행수_칸_전체_크기를_자르고_잘림을_알린다() {
    let rows = |n: usize, cell: &str| ResultSet {
        cols: vec!["a".into()],
        rows: (0..n)
            .map(|i| vec![if cell.is_empty() { i.to_string() } else { cell.to_string() }])
            .collect(),
        ..Default::default()
    };
    let res = rows(50, "");
    let out = bound_result(&res, 10);
    assert!(out.rows.len() == 10 && out.truncated, "행 수: {} {}", out.rows.len(), out.truncated);
    let out = bound_result(&res, 50);
    assert!(out.rows.len() == 50 && !out.truncated, "상한 안이면 자르지 않는다");

    // 3바이트 글자라 바이트 경계에서 글자가 깨진다
    let big = "가".repeat(MAX_CELL_BYTES);
    let out = bound_result(&rows(1, &big), 10);
    let c = &out.rows[0][0];
    assert!(
        out.truncated && c.len() <= MAX_CELL_BYTES + CLIP_MARK.len() && c.ends_with(CLIP_MARK),
        "칸 자르기: {}",
        c.len()
    );

    let out = bound_result(&rows(1000, &"x".repeat(MAX_CELL_BYTES)), 1000);
    assert!(
        out.truncated && out.rows.len() <= MAX_OUTPUT_BYTES / MAX_CELL_BYTES + 1,
        "전체 크기: {}",
        out.rows.len()
    );

    let text = ResultSet {
        text: "y".repeat(MAX_OUTPUT_BYTES + 10),
        ..Default::default()
    };
    let out = bound_result(&text, 10);
    assert!(
        out.truncated && out.text.len() <= MAX_OUTPUT_BYTES + CLIP_MARK.len(),
        "텍스트: {}",
        out.text.len()
    );
}

#[tokio::test]
async fn max_rows_기본값과_상한() {
    for (input, want) in [(0, DEFAULT_MAX_ROWS), (50, 50), (5000, MAX_ROWS)] {
        let s = local(
            Vec::new(),
            Options {
                max_rows: input,
                ..Default::default()
            },
        )
        .await;
        assert_eq!(s.opt.max_rows, want, "{input}");
    }
}

/// SCAN 에 늘 커서 1·빈 결과로 답해 키 공간이 끝없이 이어지는 Redis 를 흉내 낸다. HELLO 는 모르는 서버로 답한다.
async fn serve_endless_scan(ln: tokio::net::TcpListener) {
    while let Ok((sock, _)) = ln.accept().await {
        tokio::spawn(async move {
            let (r, mut w) = sock.into_split();
            let mut r = BufReader::new(r);
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).await.unwrap_or(0) == 0 {
                    return;
                }
                let n: usize = line.trim().trim_start_matches('*').parse().unwrap_or(0);
                let mut args = Vec::new();
                for _ in 0..n {
                    let mut len = String::new();
                    if r.read_line(&mut len).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let size: usize = len.trim().trim_start_matches('$').parse().unwrap_or(0);
                    let mut buf = vec![0; size + 2];
                    if r.read_exact(&mut buf).await.is_err() {
                        return;
                    }
                    args.push(String::from_utf8_lossy(&buf[..size]).to_uppercase());
                }
                let reply: &[u8] = match args.first().map(String::as_str) {
                    Some("HELLO") => b"-ERR unknown command 'HELLO'\r\n",
                    Some("PING") => b"+PONG\r\n",
                    Some("SCAN") => b"*2\r\n$1\r\n1\r\n*0\r\n",
                    _ => b"+OK\r\n",
                };
                if w.write_all(reply).await.is_err() {
                    return;
                }
            }
        });
    }
}

#[tokio::test]
async fn redis_scan_끝나지_않는_SCAN은_시간_제한으로_끊는다() {
    let ln = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = ln.local_addr().unwrap().port();
    tokio::spawn(serve_endless_scan(ln));
    let mut c = conn("slow-local", Kind::Redis);
    c.port = port;
    let s = local(
        vec![c],
        Options {
            scan_timeout: Duration::from_millis(300),
            ..Default::default()
        },
    )
    .await;
    let start = Instant::now();
    let (text, is_err) = call(&s, "redis_scan", json!({"connection": "slow-local", "pattern": "none:*"})).await;
    assert!(
        is_err && text.contains("stopped after 300ms") && text.contains("narrower pattern") && start.elapsed() < Duration::from_secs(5),
        "시간 제한으로 끊어야 한다 ({:?}): {text}",
        start.elapsed()
    );
}

// ─── 저장소 도구 ───

fn runs(seen: &Mutex<Vec<Request>>) -> Vec<SourceRunRequest> {
    seen.lock()
        .unwrap()
        .iter()
        .filter_map(|r| match r {
            Request::SourceRun(q) => Some(q.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn query_Athena_비싼_질의는_force_없이_거부하고_force_면_보낸다() {
    let (s, seen) = source_server(Options::default()).await;
    let (text, is_err) = call(&s, "query", json!({"connection": "athena-local", "sql": "select * from t"})).await;
    assert!(
        is_err && text.contains("전체 스캔") && text.contains("expensive query needs confirmation") && text.contains("force=true"),
        "force 없으면 거부여야 한다: {text}"
    );
    let (text, is_err) = call(
        &s,
        "query",
        json!({"connection": "athena-local", "sql": " select * from t ", "force": true}),
    )
    .await;
    assert!(!is_err, "{text}");
    let out: ResultOutput = serde_json::from_str(&text).unwrap();
    let rs = runs(&seen);
    let last = rs.last().unwrap();
    assert!(
        rs.len() == 2 && last.force && last.text == "select * from t" && out.text == "스캔 1.2GB" && out.columns[0] == "n",
        "force 면 보내야 한다: {rs:?} {out:?}"
    );
}

#[tokio::test]
async fn query_저장소_행수는_max_rows_와_서버_상한으로_자른다() {
    let (s, seen) = source_server(Options {
        max_rows: 5,
        ..Default::default()
    })
    .await;
    for (max_rows, want) in [(0, 5), (3, 3), (50, 5)] {
        let (text, is_err) = call(
            &s,
            "query",
            json!({"connection": "athena-local", "sql": "show databases", "max_rows": max_rows}),
        )
        .await;
        assert!(!is_err, "{text}");
        let out: ResultOutput = serde_json::from_str(&text).unwrap();
        let limit = runs(&seen).last().unwrap().limit;
        assert!(
            out.rows.len() == want && out.truncated && limit == want,
            "max_rows={max_rows}: rows={} limit={limit}",
            out.rows.len()
        );
    }
}

#[tokio::test]
async fn 저장소_도구_prod_접속은_허용하지_않으면_열지도_않고_거부한다() {
    let prod_calls = [
        ("query", json!({"connection": "athena-prod", "sql": "show databases"})),
        ("aws_tree", json!({"connection": "aws-prod"})),
        ("aws_open", json!({"connection": "aws-prod", "path": "sqs/orders"})),
    ];
    let (s, seen) = source_server(Options::default()).await;
    for (name, args) in &prod_calls {
        let (text, is_err) = call(&s, name, args.clone()).await;
        assert!(is_err && text.contains("is prod"), "{name}: prod 거부여야 한다: {text}");
    }
    assert!(seen.lock().unwrap().is_empty(), "prod 접속을 열면 안 된다");

    let (s, seen) = source_server(Options {
        allow_prod: true,
        ..Default::default()
    })
    .await;
    for (name, args) in &prod_calls {
        let (text, is_err) = call(&s, name, args.clone()).await;
        assert!(!is_err, "{name}: allow_prod 면 보내야 한다: {text}");
    }
    assert_eq!(seen.lock().unwrap().len(), prod_calls.len());
}

#[tokio::test]
async fn aws_tree_는_자식_노드를_경로_정보_경고와_함께_준다() {
    let (s, _) = source_server(Options::default()).await;
    let (text, is_err) = call(&s, "aws_tree", json!({"connection": "aws-local"})).await;
    assert!(!is_err, "{text}");
    let out: TreeOutput = serde_json::from_str(&text).unwrap();
    let top = TreeNode {
        name: "SQS 큐".into(),
        path: "sqs".into(),
        info: String::new(),
        warn: false,
        has_children: true,
        openable: false,
    };
    assert!(out.nodes == [top] && !out.truncated, "맨 위: {out:?}");
    let (text, _) = call(&s, "aws_tree", json!({"connection": "aws-local", "path": "sqs"})).await;
    let out: TreeOutput = serde_json::from_str(&text).unwrap();
    let want = TreeNode {
        name: "orders".into(),
        path: "sqs/orders".into(),
        info: "대기 12 · DLQ orders-dlq 3".into(),
        warn: true,
        has_children: false,
        openable: true,
    };
    assert!(out.nodes == [want] && text.contains(r#""has_children":false"#), "sqs: {text}");
}

#[tokio::test]
async fn aws_open_은_노드_표를_query_와_같은_꼴로_준다() {
    let (s, seen) = source_server(Options {
        max_rows: 7,
        ..Default::default()
    })
    .await;
    let (text, is_err) = call(&s, "aws_open", json!({"connection": "aws-local", "path": "sqs/orders"})).await;
    assert!(!is_err, "{text}");
    let out: ResultOutput = serde_json::from_str(&text).unwrap();
    assert!(
        out.columns.join(",") == "attribute,value" && out.rows.len() == 1 && out.rows[0][1] == "https://sqs/orders",
        "표: {text}"
    );
    let seen = seen.lock().unwrap();
    assert!(
        matches!(&seen[..], [Request::SourceOpen { path, limit: 7, .. }] if path == "sqs/orders"),
        "open 인자: {seen:?}"
    );
}

#[tokio::test]
async fn 저장소_도구_종류가_맞지_않으면_열지_않고_거부한다() {
    let (s, seen) = source_server(Options::default()).await;
    let (text, is_err) = call(&s, "query", json!({"connection": "aws-local", "sql": "select 1"})).await;
    assert!(is_err && text.contains("aws_tree"), "aws 질의는 트리 안내로 거부해야 한다: {text}");
    let (text, is_err) = call(&s, "aws_tree", json!({"connection": "athena-local"})).await;
    assert!(is_err && text.contains("not aws"), "athena 는 aws_tree 대상이 아니다: {text}");
    let (text, is_err) = call(&s, "query", json!({"connection": "athena-local", "sql": "  "})).await;
    assert!(is_err && text == "sql is empty", "빈 질의: {text}");
    assert!(seen.lock().unwrap().is_empty(), "열면 안 된다");
}

#[tokio::test]
async fn 저장소_거절_글은_Go_판_MCP_글로_되돌리고_요금_막음은_그대로_둔다() {
    let ro = EngineError::refused(REASON_READ_ONLY, format!("{MSG_READ_ONLY_SOURCE}: delete"));
    assert_eq!(source_text(&ro), "read-only source: write requests are not sent: delete");
    let billed = EngineError::refused(REASON_BILLED, "요금이 붙는 조회라 막음 (DB_ALLOW_BILLED=1 로 허용)");
    assert_eq!(source_text(&billed), billed.error);
}

#[tokio::test]
async fn list_connections_는_저장소_리전_프로필을_주고_비밀값은_주지_않는다() {
    let (s, _) = source_server(Options::default()).await;
    let (text, is_err) = call(&s, "list_connections", json!({})).await;
    assert!(!is_err, "{text}");
    for secret in ["loki-pw", "Password", "password"] {
        assert!(!text.contains(secret), "비밀값이 나가면 안 된다: {text}");
    }
    let out: ListConnectionsOutput = serde_json::from_str(&text).unwrap();
    let athena = ConnInfo {
        name: "athena-local".into(),
        group: "data".into(),
        kind: "athena".into(),
        env: "local".into(),
        region: "ap-northeast-2".into(),
        profile: "data-ro".into(),
    };
    let aws = ConnInfo {
        name: "aws-local".into(),
        group: "기타".into(),
        kind: "aws".into(),
        env: "local".into(),
        region: "us-east-1".into(),
        profile: "infra".into(),
    };
    assert!(out.connections.contains(&athena) && out.connections.contains(&aws), "{text}");
    assert_eq!(out.connections.len(), source_conns().len());
}

#[test]
fn tools_json_은_Go_판_도구_열_개를_이름_순으로_담는다() {
    let tools: Vec<Tool> = serde_json::from_str(TOOLS_JSON).unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    assert_eq!(
        names,
        [
            "aws_open",
            "aws_tree",
            "describe_table",
            "list_connections",
            "list_schemas",
            "list_tables",
            "query",
            "redis_command",
            "redis_get",
            "redis_scan"
        ]
    );
    assert!(
        tools
            .iter()
            .all(|t| t.annotations.as_ref().and_then(|a| a.read_only_hint) == Some(true))
    );
}

// ─── 통합 (DBTUI_TEST_PG_URL·DBTUI_TEST_REDIS_ADDR 가 있을 때만) ───

fn integration_conns() -> Option<Vec<Conn>> {
    let raw = std::env::var("DBTUI_TEST_PG_URL").ok()?;
    let addr = std::env::var("DBTUI_TEST_REDIS_ADDR").ok()?;
    let u = url::Url::parse(&raw).unwrap();
    let (host, port) = addr.rsplit_once(':').unwrap();
    let mut pg = conn("it-local", Kind::Pg);
    (pg.host, pg.port, pg.db, pg.user, pg.password) = (
        u.host_str().unwrap().into(),
        u.port().unwrap_or(5432),
        u.path().trim_start_matches('/').into(),
        u.username().into(),
        u.password().unwrap_or_default().into(),
    );
    let mut r = conn("it-redis", Kind::Redis);
    (r.host, r.port, r.db) = (host.into(), port.parse().unwrap(), "0".into());
    Some(vec![pg, r])
}

#[tokio::test]
async fn 통합_PG_도구가_읽기만_하고_행수를_제한한다() {
    let Some(conns) = integration_conns() else { return };
    let s = local(
        conns.clone(),
        Options {
            max_rows: 20,
            ..Default::default()
        },
    )
    .await;
    let (text, is_err) = call(
        &s,
        "query",
        json!({"connection": "it-local", "sql": "drop schema if exists mcp_it cascade"}),
    )
    .await;
    assert!(is_err, "drop 은 거부해야 한다: {text}");
    // 준비는 MCP 밖에서 한다(도구로는 쓸 수 없다)
    let clients = idb_sources::Clients::new();
    let pg = clients.pg(&conns[0]).await.unwrap();
    for sql in [
        "drop schema if exists mcp_it cascade",
        "create schema mcp_it",
        "create table mcp_it.t (id int not null default 7 primary key, name text)",
        "insert into mcp_it.t select generate_series(1, 100), 'n'",
    ] {
        pg.run_args(sql, &[]).await.unwrap();
    }
    clients.close().await;

    let (text, is_err) = call(&s, "list_schemas", json!({"connection": "it-local"})).await;
    assert!(!is_err && text.contains(r#""mcp_it""#), "list_schemas: {text}");
    let (text, is_err) = call(&s, "list_tables", json!({"connection": "it-local", "schema": "mcp_it"})).await;
    assert!(!is_err && text.contains(r#""t""#), "list_tables: {text}");
    let (text, is_err) = call(
        &s,
        "describe_table",
        json!({"connection": "it-local", "schema": "mcp_it", "table": "t"}),
    )
    .await;
    assert!(
        !is_err && text.contains(r#"["id","integer","NO","DEFAULT 7",""]"#) && text.contains("t_pkey"),
        "describe_table 은 열과 제약을 준다: {text}"
    );
    let (text, is_err) = call(
        &s,
        "describe_table",
        json!({"connection": "it-local", "schema": "mcp_it", "table": "x' or '1'='1"}),
    )
    .await;
    assert!(is_err && text.contains("not found"), "따옴표는 리터럴로 처리해야 한다: {text}");

    let (text, is_err) = call(
        &s,
        "query",
        json!({"connection": "it-local", "sql": "select id from mcp_it.t order by id"}),
    )
    .await;
    let out: ResultOutput = serde_json::from_str(&text).unwrap();
    assert!(!is_err && out.rows.len() == 20 && out.truncated, "max_rows 로 잘라야 한다: {text}");
    let (text, _) = call(
        &s,
        "query",
        json!({"connection": "it-local", "sql": "select id from mcp_it.t order by id", "max_rows": 5}),
    )
    .await;
    let out: ResultOutput = serde_json::from_str(&text).unwrap();
    assert_eq!(out.rows.len(), 5, "max_rows: {text}");
    for sql in [
        "with d as (delete from mcp_it.t returning id) select * from d",
        "select 1; delete from mcp_it.t",
        "select pg_notify('mcp_it', 'x')",
        "select pg_catalog.pg_try_advisory_lock(424242)",
    ] {
        let (text, is_err) = call(&s, "query", json!({"connection": "it-local", "sql": sql})).await;
        assert!(is_err, "서버가 거부해야 한다: {sql} → {text}");
    }
    let (text, _) = call(
        &s,
        "query",
        json!({"connection": "it-local", "sql": "select count(*) from mcp_it.t"}),
    )
    .await;
    assert!(text.contains(r#"[["100"]]"#), "쓰기가 반영되면 안 된다: {text}");
}

#[tokio::test]
async fn 통합_Redis_도구() {
    let Some(conns) = integration_conns() else { return };
    let mut r = idb_sources::redis::RedisConn::open(&conns[1]).await.unwrap();
    for args in [
        vec!["DEL", "mcp:h", "mcp:s"],
        vec!["HSET", "mcp:h", "f", "v"],
        vec!["SET", "mcp:s", "hello"],
    ] {
        let args: Vec<String> = args.into_iter().map(String::from).collect();
        r.run(&args).await.unwrap();
    }
    let s = local(conns, Options::default()).await;
    let (text, is_err) = call(&s, "redis_scan", json!({"connection": "it-redis", "pattern": "mcp:*"})).await;
    assert!(
        !is_err && text.contains(r#""mcp:h""#) && text.contains(r#""mcp:s""#),
        "redis_scan: {text}"
    );
    let (text, is_err) = call(&s, "redis_scan", json!({"connection": "it-redis", "pattern": "mcp:*", "limit": 1})).await;
    assert!(!is_err && text.contains(r#""truncated":true"#), "limit: {text}");
    let (text, is_err) = call(&s, "redis_get", json!({"connection": "it-redis", "key": "mcp:h"})).await;
    assert!(!is_err && text.contains(r#"["f","v"]"#), "redis_get: {text}");
    let (text, is_err) = call(&s, "redis_command", json!({"connection": "it-redis", "args": ["GET", "mcp:s"]})).await;
    assert!(!is_err && text.contains("hello"), "redis_command: {text}");
    let (text, is_err) = call(&s, "redis_command", json!({"connection": "it-redis", "args": ["DEL", "mcp:s"]})).await;
    assert!(is_err, "DEL 은 거부해야 한다: {text}");
}
