//! Go `opensearch_test.go`·`probe_test.go` 를 옮겼다. 가짜 HTTP 서버로 보낸 요청과 결과 표를 본다.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use aws_credential_types::Credentials;
use idb_core::result::{MAX_CELL_BYTES, is_capped_cell};

use super::*;
use crate::fakehttp::{Fake, Reply, redirect, reply, serve};

/// 경로별로 정한 응답을 돌려주는 서버다. 없는 경로는 index_not_found 404 다.
fn fake_server(routes: &[(&str, &str)]) -> Fake {
    let routes: HashMap<String, String> = routes.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    serve(move |r| match routes.get(&format!("{} {}", r.method, r.path)) {
        Some(body) => reply(200, body.clone()),
        None => reply(
            404,
            r#"{"error":{"type":"index_not_found_exception","reason":"no such index"},"status":404}"#,
        ),
    })
}

fn conn(url: &str) -> Conn {
    Conn {
        url: url.into(),
        ..Default::default()
    }
}

async fn backend(c: Conn) -> OpenSearch {
    OpenSearch::new(&c).await.unwrap()
}

fn q(text: &str) -> Query {
    Query {
        text: text.into(),
        ..Default::default()
    }
}

const SEARCH_RESP: &str = r#"{"hits":{"total":{"value":2},"hits":[
 {"_index":"logs","_id":"1","_score":1.5,"_source":{"msg":"hi","n":3,"tags":["a","b"]}},
 {"_index":"logs","_id":"2","_score":null,"_source":{"user":{"id":7},"msg":null,"ok":true}}]}}"#;

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[test]
fn 허용_경로는_읽기_경로만이다() {
    let cases: &[(&str, &str, Option<&str>)] = &[
        ("GET", "logs/_search", Some("_search")),
        ("GET", "logs-*/_count", Some("_count")),
        ("GET", "logs/_mapping", Some("_mapping")),
        ("GET", "_search", Some("_search")),
        ("GET", "_cat/indices", Some("_cat")),
        ("GET", "_cat/aliases/foo", Some("_cat")),
        ("GET", "_all/_search", Some("_search")),
        ("POST", "logs/_search", Some("_search")),
        ("POST", "_search", Some("_search")),
        ("POST", "logs/_count", None),
        ("POST", "logs/_doc", None),
        ("PUT", "logs", None),
        ("DELETE", "logs", None),
        ("DELETE", "logs/_doc/1", None),
        ("POST", "_bulk", None),
        ("POST", "logs/_delete_by_query", None),
        ("POST", "logs/_update/1", None),
        ("GET", "logs/_doc/1", None),
        ("GET", "_cluster/settings", None),
        ("GET", "_cat", None),
        ("GET", "_doc/_search", None),
        ("GET", "logs", None),
        ("GET", "/_search", None),
        // 경로 우회: 빈 마디·./..·인코딩·제어 문자·시스템 인덱스
        ("GET", "_cat/a/../..", None),
        ("GET", "_cat/indices/../../_cluster", None),
        ("GET", "_cat/indices/foo/bar", None),
        ("GET", "_cat/in-dices", None),
        ("GET", "../_search", None),
        ("GET", "./_search", None),
        ("GET", "logs//_search", None),
        ("GET", ".opendistro_security/_search", None),
        ("GET", "logs,.opendistro_security/_search", None),
        ("GET", "_cat/indices/.kibana", None),
        ("GET", "%2e%2e/_search", None),
        ("GET", "logs%2F_doc/_search", None),
        ("GET", "lo\tgs/_search", None),
        ("GET", "lo\x00gs/_search", None),
        ("GET", "lo\\gs/_search", None),
        ("GET", "logs,-old/_search", Some("_search")),
    ];
    for (method, path, want) in cases {
        assert_eq!(allowed_path(method, path), *want, "{method} {path}");
    }
}

#[tokio::test]
async fn 쓰기_요청은_보내지_않고_Write_다() {
    let srv = fake_server(&[]);
    let b = backend(conn(&srv.url)).await;
    for text in [
        "DELETE logs",
        "PUT logs/_doc/1\n{\"a\":1}",
        "POST logs/_doc\n{}",
        "POST _bulk",
        "GET logs/_doc/1",
        "UPDATE logs SET a = 1",
        "DELETE FROM logs",
        "",
        "-- 주석만",
    ] {
        let err = b.run(&q(text)).await.unwrap_err();
        assert!(matches!(err, Error::Write(_)), "{text:?}: {err}");
    }
    assert_eq!(srv.count(), 0, "거절한 질의가 나갔다");
}

#[tokio::test]
async fn SELECT_는_SQL_플러그인으로_REST_는_경로로_보낸다() {
    let srv = fake_server(&[
        (
            "POST /_plugins/_sql",
            r#"{"schema":[{"name":"msg","type":"text"},{"name":"count(*)","alias":"c","type":"integer"}],"datarows":[["hi",2],[null,{"x":1}]],"total":2,"size":2,"status":200}"#,
        ),
        ("GET /logs/_search", SEARCH_RESP),
    ]);
    let b = backend(conn(&format!("{}/", srv.url))).await;
    let res = b
        .run(&q("-- 첫 줄 주석\n  select msg, count(*) as c from logs group by msg;"))
        .await
        .unwrap();
    assert_eq!(res.cols, strs(&["msg", "c"]));
    assert_eq!(res.rows, vec![strs(&["hi", "2"]), strs(&["NULL", r#"{"x":1}"#])]);
    b.run(&q("# 주석\nget logs/_search\n{\"query\":{\"match_all\":{}}}")).await.unwrap();
    let reqs = srv.seen();
    assert_eq!(
        (reqs[0].path.as_str(), reqs[0].raw_query.as_str()),
        ("/_plugins/_sql", "format=jdbc")
    );
    let sql: serde_json::Value = serde_json::from_str(&reqs[0].body).unwrap();
    let stmt = sql["query"].as_str().unwrap();
    assert!(!stmt.ends_with(';') && stmt.contains("select msg"), "{}", reqs[0].body);
    assert_eq!(sql["fetch_size"], 1000);
    assert_eq!(
        (reqs[1].method.as_str(), reqs[1].path.as_str(), reqs[1].body.as_str()),
        ("GET", "/logs/_search", r#"{"query":{"match_all":{}}}"#)
    );
}

#[tokio::test]
async fn search_결과는_필드_합집합과_JSON_칸이다() {
    let srv = fake_server(&[("POST /logs/_search", SEARCH_RESP)]);
    let b = backend(conn(&srv.url)).await;
    let res = b.run(&q("POST logs/_search")).await.unwrap();
    assert_eq!(res.cols, strs(&["_index", "_id", "_score", "msg", "n", "tags", "user", "ok"]));
    assert_eq!(res.rows[0], strs(&["logs", "1", "1.5", "hi", "3", r#"["a","b"]"#, "NULL", "NULL"]));
    assert_eq!(
        res.rows[1],
        strs(&["logs", "2", "NULL", "NULL", "NULL", "NULL", r#"{"id":7}"#, "true"])
    );
    assert!(!res.truncated, "잘리지 않았는데 truncated");

    let res = b
        .run(&Query {
            limit: 1,
            ..q("POST logs/_search")
        })
        .await
        .unwrap();
    assert!(
        res.rows.len() == 1 && res.truncated && !res.cols.contains(&"user".into()),
        "{res:?}"
    );
}

#[tokio::test]
async fn 집계는_aggregations_JSON_칸이다() {
    let srv = fake_server(&[
        (
            "GET /a/_search",
            r#"{"hits":{"hits":[]},"aggregations":{"by":{"buckets":[{"key":"x","doc_count":2}]}}}"#,
        ),
        (
            "GET /b/_search",
            r#"{"hits":{"hits":[{"_index":"b","_id":"1","_score":1,"_source":{"k":1}}]},"aggregations":{"m":{"value":1}}}"#,
        ),
    ]);
    let b = backend(conn(&srv.url)).await;
    let res = b
        .run(&q("GET a/_search\n{\"size\":0,\"aggs\":{\"by\":{\"terms\":{\"field\":\"k\"}}}}"))
        .await
        .unwrap();
    assert_eq!(res.cols, strs(&["aggregations"]));
    assert_eq!(res.rows[0][0], r#"{"by":{"buckets":[{"key":"x","doc_count":2}]}}"#);
    let res = b.run(&q("GET b/_search")).await.unwrap();
    assert_eq!(res.cols, strs(&["_index", "_id", "_score", "k", "aggregations"]));
    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[0][4], "NULL");
    assert_eq!(res.rows[1], strs(&["NULL", "NULL", "NULL", "NULL", r#"{"m":{"value":1}}"#]));
}

#[tokio::test]
async fn count_와_cat_과_mapping_은_표다() {
    let srv = fake_server(&[
        ("GET /logs/_count", r#"{"count":42,"_shards":{}}"#),
        (
            "GET /_cat/indices",
            r#"[{"index":"a","health":"green"},{"index":"b","docs.count":"3"}]"#,
        ),
        (
            "GET /logs/_mapping",
            r#"{"logs":{"mappings":{"properties":{"msg":{"type":"text"},"user":{"properties":{"id":{"type":"long"}}}}}}}"#,
        ),
    ]);
    let b = backend(conn(&srv.url)).await;
    let res = b.run(&q("GET logs/_count\n{\"query\":{\"term\":{\"a\":1}}}")).await.unwrap();
    assert_eq!((res.cols.clone(), res.rows[0][0].as_str()), (strs(&["count"]), "42"));
    let res = b.run(&q("GET _cat/indices?v&format=text")).await.unwrap();
    assert_eq!(res.cols, strs(&["index", "health", "docs.count"]));
    assert_eq!(res.rows[1], strs(&["b", "NULL", "3"]));
    let query = &srv.seen()[1].raw_query;
    assert!(query.contains("format=json") && !query.contains("format=text"), "{query}");
    let res = b.run(&q("GET logs/_mapping")).await.unwrap();
    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[1], strs(&["logs", "user.id", "long"]));
}

#[tokio::test]
async fn size_1만_초과는_확인을_받는다() {
    let srv = fake_server(&[("GET /logs/_search", SEARCH_RESP)]);
    let b = backend(conn(&srv.url)).await;
    for text in [
        "GET logs/_search\n{\"size\":10001}",
        "GET logs/_search\n{\"from\":9990,\"size\":20}",
        "GET logs/_search?size=20000",
    ] {
        let err = b.run(&q(text)).await.unwrap_err();
        assert!(matches!(err, Error::NeedsConfirm(_)), "{text:?}: {err}");
    }
    assert_eq!(srv.count(), 0, "확인 전에 나갔다");
    b.run(&q("GET logs/_search\n{\"size\":10000}")).await.expect("경계 1만");
    b.run(&Query {
        force: true,
        ..q("GET logs/_search\n{\"size\":20000}")
    })
    .await
    .expect("force");
    let err = b.run(&q("GET logs/_search\n{\"size\":")).await.unwrap_err();
    assert!(!matches!(err, Error::Write(_)), "깨진 JSON 본문 = {err}");
}

#[tokio::test]
async fn 트리는_인덱스와_별칭과_매핑_필드다() {
    let srv = fake_server(&[
        (
            "GET /_cat/indices",
            r#"[{"index":"logs-2","docs.count":"5","health":"yellow"},{"index":".kibana","docs.count":"1","health":"green"},{"index":"logs-1","docs.count":"3","health":"green"}]"#,
        ),
        (
            "GET /_cat/aliases",
            r#"[{"alias":"logs","index":"logs-2"},{"alias":"logs","index":"logs-1"},{"alias":".sys","index":".kibana"}]"#,
        ),
        (
            "GET /logs/_mapping",
            r#"{"logs-1":{"mappings":{"properties":{"msg":{"type":"text","fields":{"keyword":{"type":"keyword"}}},"user":{"properties":{"id":{"type":"long"},"geo":{"properties":{"lat":{"type":"float"}}}}}}}},
              "logs-2":{"mappings":{"properties":{"extra":{"type":"keyword"},"msg":{"type":"keyword"}}}}}"#,
        ),
    ]);
    let b = backend(conn(&srv.url)).await;
    let nodes = b.children("").await.unwrap();
    let names: Vec<String> = nodes.iter().map(|n| format!("{}|{}|{}", n.name, n.path, n.info)).collect();
    assert_eq!(
        names,
        strs(&[
            "logs → logs-1, logs-2|logs|alias",
            "logs-1|logs-1|3 green",
            "logs-2|logs-2|5 yellow"
        ])
    );
    assert!(nodes[0].has_children && nodes[0].openable, "인덱스·별칭은 펼치고 열 수 있어야 한다");
    let fields = b.children("logs").await.unwrap();
    let names: Vec<String> = fields.iter().map(|n| format!("{} {}", n.name, n.info)).collect();
    assert_eq!(names, strs(&["extra keyword", "msg text", "user.geo.lat float", "user.id long"]));
    assert!(fields[0].path == "logs/extra" && !fields[0].has_children, "{:?}", fields[0]);
    assert!(b.children("logs/extra").await.unwrap().is_empty());
}

#[tokio::test]
async fn 미리_보기는_match_all_size_로_연다() {
    let srv = fake_server(&[("POST /logs/_search", SEARCH_RESP)]);
    let b = backend(conn(&srv.url)).await;
    let res = b.open("logs", 0).await.unwrap();
    assert_eq!(res.rows.len(), 2);
    assert_eq!(srv.seen()[0].body, r#"{"query":{"match_all":{}},"size":50}"#);
}

#[tokio::test]
async fn basic_인증_헤더를_싣고_오류에서_비밀번호를_지운다() {
    const PW: &str = "s3cr:et@pw";
    let srv = serve(|r| {
        if r.basic() != format!("reader:{PW}") {
            return reply(401, "");
        }
        reply(
            400,
            format!(r#"{{"error":{{"root_cause":[],"type":"parsing_exception","reason":"bad token {PW}"}},"status":400}}"#),
        )
    });
    let b = backend(Conn {
        auth: "basic".into(),
        user: "reader".into(),
        password: PW.into(),
        ..conn(&srv.url)
    })
    .await;
    let msg = b.run(&q("GET logs/_search")).await.unwrap_err().to_string();
    assert!(
        msg.contains("400") && msg.contains("parsing_exception: bad token") && !msg.contains(PW),
        "{msg}"
    );
}

#[tokio::test]
async fn sigv4_는_서명한_Authorization_을_싣는다() {
    let srv = fake_server(&[("GET /logs/_count", r#"{"count":1}"#)]);
    let mut b = backend(conn(&srv.url)).await;
    // 자격 증명 체인은 환경을 읽어 시험끼리 섞이므로 정적 자격 증명을 바로 넣는다
    b.sigv4 = Some(SigV4 {
        creds: SharedCredentialsProvider::new(Credentials::new(
            "AKIDEXAMPLE",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            None,
            None,
            "test",
        )),
        region: "ap-northeast-2".into(),
    });
    b.run(&q("GET logs/_count\n{}")).await.unwrap();
    let r = srv.last();
    let auth = r.header("authorization");
    assert!(
        auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/") && auth.contains("/ap-northeast-2/es/aws4_request"),
        "{auth}"
    );
    assert!(!r.header("x-amz-date").is_empty());
}

#[tokio::test]
async fn 잘못된_설정은_거절한다() {
    for (url, auth) in [
        ("", ""),
        ("localhost:9200", ""),
        ("ftp://x", ""),
        ("http://x", "token"),
        ("http://x", "basic"),
    ] {
        let c = Conn {
            auth: auth.into(),
            ..conn(url)
        };
        assert!(OpenSearch::new(&c).await.is_err(), "{url} {auth}: 오류가 나야 한다");
    }
}

#[test]
fn 큰_칸은_잘린_칸_꼴로_자른다() {
    let raw = serde_json::to_string(&"가".repeat(MAX_CELL_BYTES)).unwrap();
    let s = cell(&raw);
    assert!(is_capped_cell(&s), "len={}", s.len());
}

#[tokio::test]
async fn size_from_은_문자열_실수_지수_중복까지_읽어_확인을_받는다() {
    let srv = fake_server(&[("GET /logs/_search", SEARCH_RESP)]);
    let b = backend(conn(&srv.url)).await;
    for (text, reason) in [
        ("GET logs/_search\n{\"size\":\"20000\"}", "size 1만 초과"),
        ("GET logs/_search\n{\"size\":1e5}", "size 1만 초과"),
        ("GET logs/_search\n{\"size\":10000.5}", "size 1만 초과"),
        ("GET logs/_search\n{\"from\":\"9999\",\"size\":2}", "size 1만 초과"),
        ("GET logs/_search?size=20&size=20000", "읽을 수 없음"),
        ("GET logs/_search?size=abc", "읽을 수 없음"),
        ("GET logs/_search\n{\"size\":10,\"size\":20000}", "읽을 수 없음"),
        ("GET logs/_search?size=20\n{\"size\":20}", "읽을 수 없음"),
        ("GET logs/_search\n{\"size\":\"many\"}", "읽을 수 없음"),
        ("GET logs/_search\n{\"size\":\"NaN\"}", "읽을 수 없음"),
        ("GET logs/_search\n[1]", "읽을 수 없음"),
    ] {
        let err = b.run(&q(text)).await.unwrap_err();
        assert!(
            matches!(err, Error::NeedsConfirm(_)) && err.confirm_reason().contains(reason),
            "{text:?}: {err}, want {reason}"
        );
    }
    assert_eq!(srv.count(), 0, "확인 전에 나갔다");
    for text in [
        "GET logs/_search\n{\"size\":\"20\"}",
        "GET logs/_search\n{\"size\":2e3}",
        "GET logs/_search?size=100&from=10",
    ] {
        b.run(&q(text)).await.unwrap_or_else(|e| panic!("{text:?}: {e}, 바로 보내야 한다"));
    }
}

#[tokio::test]
async fn SQL_은_끝_세미콜론_뒤_다른_문장이나_쓰기_낱말을_거절한다() {
    let srv = fake_server(&[("POST /_plugins/_sql", r#"{"schema":[{"name":"a"}],"datarows":[[1]]}"#)]);
    let b = backend(conn(&srv.url)).await;
    for text in [
        "SELECT * FROM logs; DELETE FROM logs",
        "SELECT * FROM logs;; SELECT 1",
        "SELECT * FROM (DELETE FROM logs)",
        "select a from logs where b in (select c from x); drop table x",
        "SELECT * FROM logs WHERE 1=1 /* x */ ; update logs set a=1",
    ] {
        let err = b.run(&q(text)).await.unwrap_err();
        assert!(matches!(err, Error::Write(_)), "{text:?}: {err}");
    }
    assert_eq!(srv.count(), 0, "거절한 SQL 이 나갔다");
    for text in [
        "SELECT * FROM logs WHERE msg = 'a; delete from x';",
        "SELECT * FROM logs -- ; drop table x\n;",
        "SELECT updated_at FROM logs ;  ",
    ] {
        b.run(&q(text)).await.unwrap_or_else(|e| panic!("{text:?}: {e}, 보내야 한다"));
    }
    assert!(
        !srv.seen()[1].body.contains(';'),
        "끝 ;·주석은 떼고 보내야 한다: {}",
        srv.seen()[1].body
    );
}

#[tokio::test]
async fn 리다이렉트를_따라가지_않고_오류로_본다() {
    let second = Arc::new(AtomicUsize::new(0));
    let s2 = second.clone();
    let target = serve(move |_| {
        s2.fetch_add(1, Ordering::SeqCst);
        reply(200, "")
    });
    let turl = target.url.clone();
    let srv = serve(move |r| -> Reply {
        if r.path == "/logs/_search" {
            return redirect("/_all/_delete_by_query"); // 같은 호스트 다른 경로
        }
        redirect(&format!("{turl}/x"))
    });
    let b = backend(Conn {
        auth: "basic".into(),
        user: "reader".into(),
        password: "pw".into(),
        ..conn(&srv.url)
    })
    .await;
    for text in ["GET logs/_search", "GET logs/_count"] {
        let err = b.run(&q(text)).await.unwrap_err().to_string();
        assert!(err.contains("redirect not followed: 302"), "{text:?}: {err}");
    }
    assert_eq!(second.load(Ordering::SeqCst), 0, "리다이렉트를 따라갔다");
    assert_eq!(srv.count(), 2);
}

// ─── R61.3 정렬·거르기 다시 읽기 ───

fn no_info(_: &str) -> (String, String) {
    (String::new(), String::new())
}

fn sort_only(text: &str, field: &str, desc: bool) -> Option<String> {
    rewrite(
        text,
        &Rewrite {
            sort: field.into(),
            desc,
            filter: None,
        },
        &no_info,
    )
}

#[test]
fn 다시_읽기는_본문_sort_만_바꾸고_다른_키는_둔다() {
    for (name, text, field, desc, want) in [
        (
            "본문 없음",
            "GET logs/_search".to_string(),
            "status",
            false,
            "GET logs/_search\n".to_string() + r#"{"sort":[{"status":{"missing":"_last","order":"asc"}}]}"#,
        ),
        (
            "기존 sort 를 바꾼다",
            "GET logs/_search\n".to_string() + r#"{"size":20,"sort":["_doc"]}"#,
            "status",
            true,
            "GET logs/_search\n".to_string() + r#"{"size":20,"sort":[{"status":{"missing":"_last","order":"desc"}}]}"#,
        ),
        (
            "_score 는 missing 이 없다",
            "POST _search\n{}".to_string(),
            "_score",
            true,
            "POST _search\n".to_string() + r#"{"sort":[{"_score":{"order":"desc"}}]}"#,
        ),
        (
            "미리 보기 글",
            open_text("logs", 0),
            "_id",
            false,
            "POST logs/_search\n".to_string()
                + r#"{"query":{"match_all":{}},"size":50,"sort":[{"_id":{"missing":"_last","order":"asc"}}]}"#,
        ),
    ] {
        assert_eq!(sort_only(&text, field, desc), Some(want), "{name}");
    }
}

#[test]
fn search_가_아니면_바꾸지_않는다() {
    for text in [
        "SELECT * FROM logs",
        "GET logs/_count",
        "GET _cat/indices",
        "GET logs/_search\n[1]",
        "GET logs/_search\n{bad",
    ] {
        assert_eq!(sort_only(text, "status", false), None, "{text:?}");
    }
    assert!(is_search("POST logs/_search\n{}") && !is_search("SELECT 1"));
}

/// 거르기 시험의 필드 정보다: msg 는 keyword 하위 필드가 있는 text, body 는 그것이 없는 text, n 은 long 이다.
fn rewrite_info(name: &str) -> (String, String) {
    let (t, k) = match name {
        "msg" => ("text", "msg.keyword"),
        "body" => ("text", ""),
        "n" => ("long", ""),
        _ => ("", ""),
    };
    (t.into(), k.into())
}

fn filter(field: &str, op: &str, value: &str) -> Filter {
    Filter {
        field: field.into(),
        op: op.into(),
        value: value.into(),
    }
}

#[test]
fn 거르기는_원래_query_를_must_에_두고_조건을_더한다() {
    let base = "GET logs/_search\n".to_string() + r#"{"query":{"match":{"app":"api"}},"size":5}"#;
    for (f, not, want) in [
        (filter("status", "=", "500"), false, r#"{"term":{"status":"500"}}"#),
        (filter("msg", "=", "a b"), false, r#"{"term":{"msg.keyword":"a b"}}"#),
        (filter("status", "!=", "500"), true, r#"{"term":{"status":"500"}}"#),
        (filter("n", ">", "5"), false, r#"{"range":{"n":{"gt":5}}}"#),
        (
            filter("day", "<=", "2026-11-01"),
            false,
            r#"{"range":{"day":{"lte":"2026-11-01"}}}"#,
        ),
        (
            filter("msg", ":", "a*b"),
            false,
            r#"{"wildcard":{"msg.keyword":{"case_insensitive":true,"value":"*a\\*b*"}}}"#,
        ),
        (
            filter("body", ":", "err"),
            false,
            r#"{"wildcard":{"body":{"case_insensitive":true,"value":"*err*"}}}"#,
        ),
        (filter("n", ":", "7"), false, r#"{"term":{"n":"7"}}"#),
        (filter("msg", ":", ""), false, r#"{"exists":{"field":"msg"}}"#),
        (filter("msg", "null", ""), true, r#"{"exists":{"field":"msg"}}"#),
        (filter("msg", "notnull", ""), false, r#"{"exists":{"field":"msg"}}"#),
        (
            filter("", ":", r#"a "b" OR c"#),
            false,
            r#"{"query_string":{"lenient":true,"query":"\"a \\\"b\\\" OR c\""}}"#,
        ),
        (filter("", "!:", "x"), true, r#"{"query_string":{"lenient":true,"query":"\"x\""}}"#),
    ] {
        let got = rewrite(
            &base,
            &Rewrite {
                filter: Some(f.clone()),
                ..Default::default()
            },
            &rewrite_info,
        );
        // JSON 키는 이름순이다
        let query = if not {
            format!(r#"{{"bool":{{"must":[{{"match":{{"app":"api"}}}}],"must_not":[{want}]}}}}"#)
        } else {
            format!(r#"{{"bool":{{"filter":[{want}],"must":[{{"match":{{"app":"api"}}}}]}}}}"#)
        };
        let want = format!("GET logs/_search\n{{\"query\":{query},\"size\":5}}");
        assert_eq!(got.as_deref(), Some(want.as_str()), "{f:?}");
    }
    let got = rewrite(
        "GET logs/_search",
        &Rewrite {
            sort: "msg".into(),
            filter: Some(filter("", ":", "")),
            ..Default::default()
        },
        &rewrite_info,
    );
    assert_eq!(
        got.as_deref(),
        Some(concat!(
            "GET logs/_search\n",
            r#"{"sort":[{"msg.keyword":{"missing":"_last","order":"asc"}}]}"#
        )),
        "빈 모든 필드 찾기는 거르기 없음, text 정렬은 keyword"
    );
    let got = rewrite(
        "GET logs/_search",
        &Rewrite {
            filter: Some(filter("a", "=", "1")),
            ..Default::default()
        },
        &rewrite_info,
    )
    .unwrap();
    assert!(got.contains(r#""must":[{"match_all":{}}]"#), "query 가 없으면 match_all: {got}");
}

#[tokio::test]
async fn 다시_읽기의_매핑은_인덱스마다_한_번_읽어_text_정렬을_keyword_로_보낸다() {
    let srv = fake_server(&[
        (
            "GET /logs/_mapping",
            r#"{"logs":{"mappings":{"properties":{"msg":{"type":"text","fields":{"raw":{"type":"keyword"},"keyword":{"type":"keyword"}}},"n":{"type":"long"}}}}}"#,
        ),
        ("GET /logs/_search", SEARCH_RESP),
    ]);
    let b = backend(conn(&srv.url)).await;
    let query = q("GET logs/_search\n{\"size\":2}");
    let r = Rewrite {
        sort: "msg".into(),
        desc: true,
        filter: None,
    };
    for _ in 0..2 {
        let (sent, res) = b.requery(&query, &r).await.unwrap();
        assert_eq!(res.rows.len(), 2);
        assert_eq!(
            sent,
            "GET logs/_search\n".to_string() + r#"{"size":2,"sort":[{"msg.keyword":{"missing":"_last","order":"desc"}}]}"#
        );
    }
    assert_eq!(
        srv.seen().iter().filter(|s| s.path == "/logs/_mapping").count(),
        1,
        "매핑은 한 번만 읽는다"
    );
    let r = Rewrite {
        sort: "n".into(),
        ..Default::default()
    };
    assert!(
        b.requery(&q("GET logs/_count"), &r).await.is_err(),
        "_search 가 아니면 보내지 않는다"
    );
}

#[test]
fn 압축은_문자열_밖_공백만_지우고_html_글자는_Marshal_꼴로_바꾼다() {
    assert_eq!(compact("{ \"a b\" : [1, 2] }", false), r#"{"a b":[1,2]}"#);
    assert_eq!(compact(r#"{"x":"<a\"&>"}"#, true), r#"{"x":"\u003ca\"\u0026\u003e"}"#);
    assert_eq!(go_json_str("a<b"), r#""a\u003cb""#);
    assert_eq!(base64(b"reader:pw"), "cmVhZGVyOnB3");
    assert_eq!(base64(b"ab"), "YWI=");
}

#[test]
fn 오류_본문은_type_reason_이나_앞부분이다() {
    assert_eq!(
        error_text(br#"{"error":{"type":"t","reason":"r","details":"d1\nd2"}}"#),
        "t: r (d1)"
    );
    assert_eq!(error_text(br#"{"error":{"reason":"only"}}"#), "only");
    assert_eq!(error_text(br#"{"error":"plain"}"#), "plain");
    assert_eq!(error_text(b"  <html>  "), "<html>");
    let long = "가".repeat(200);
    assert_eq!(error_text(long.as_bytes()), "가".repeat(100) + "…");
}

// ─── 대시보드 확인 ───

async fn probe(routes: &[(&str, &str)], c: Conn) -> (Health, Vec<crate::fakehttp::Seen>) {
    let srv = fake_server(routes);
    let b = backend(Conn { url: srv.url.clone(), ..c }).await;
    (b.probe().await, srv.seen())
}

#[tokio::test]
async fn 확인은_상태_문서_합_인덱스_수와_버전이고_시스템_인덱스는_뺀다() {
    let (h, seen) = probe(
        &[
            (
                "GET /_cluster/health",
                r#"{"cluster_name":"c","status":"green","number_of_nodes":3}"#,
            ),
            (
                "GET /_cat/indices",
                r#"[{"index":"logs-1","docs.count":"1000000"},{"index":"logs-2","docs.count":"200000"},
                {"index":"closed","docs.count":null},{"index":".kibana_1","docs.count":"99999999"}]"#,
            ),
            ("GET /", r#"{"version":{"distribution":"opensearch","number":"2.17.0"}}"#),
        ],
        Conn {
            auth: "basic".into(),
            user: "u".into(),
            password: "pw".into(),
            ..Default::default()
        },
    )
    .await;
    assert!(h.ok && !h.metrics[0].warn, "{h:?}");
    assert_eq!(h.summary(), "green · 문서 120만 · 인덱스 3");
    assert_eq!(h.version, "OpenSearch 2.17.0");
    for r in seen {
        assert!(
            r.method == "GET" && !r.header("authorization").is_empty(),
            "{r:?}: GET·basic 인증이어야 함"
        );
    }
}

#[tokio::test]
async fn 확인은_green_이_아니면_경고이고_버전을_못_읽어도_OK다() {
    for status in ["yellow", "red"] {
        let health = format!(r#"{{"status":"{status}"}}"#);
        let (h, _) = probe(
            &[("GET /_cluster/health", health.as_str()), ("GET /_cat/indices", "[]")],
            Conn::default(),
        )
        .await;
        assert!(
            h.ok && h.metrics[0].warn && h.metrics[0].value == status && h.version.is_empty(),
            "{status}: {h:?}"
        );
    }
}

#[tokio::test]
async fn 확인_오류는_실패이고_비밀번호를_지운다() {
    // 서버가 비밀번호를 되돌려주는 꼴을 흉내 낸다
    let srv = serve(|_| reply(401, r#"{"error":{"type":"security_exception","reason":"bad password hunter2"}}"#));
    let b = backend(Conn {
        auth: "basic".into(),
        user: "u".into(),
        password: "hunter2".into(),
        ..conn(&srv.url)
    })
    .await;
    let h = b.probe().await;
    assert!(
        !h.ok && h.err.contains("opensearch 401: security_exception") && !h.err.contains("hunter2"),
        "{h:?}"
    );
    let (h, _) = probe(&[("GET /_cluster/health", "<html>")], Conn::default()).await;
    assert!(!h.ok && h.err.contains("_cluster/health"), "깨진 응답: {h:?}");
}

#[tokio::test]
async fn 닿지_않는_주소는_Go_전송_오류_꼴이다() {
    let b = backend(conn("http://127.0.0.1:1")).await;
    let err = b.children("").await.unwrap_err().to_string();
    assert_eq!(
        err,
        r#"opensearch: Get "http://127.0.0.1:1/_cat/indices?format=json&h=index,docs.count,health": dial tcp 127.0.0.1:1: connect: connection refused"#
    );
}

#[tokio::test]
async fn 통합_실제_OpenSearch_를_확인한다() {
    let Ok(base) = std::env::var("DBTUI_TEST_OPENSEARCH_URL") else {
        return;
    };
    let b = backend(conn(&base)).await;
    let h = tokio::time::timeout(std::time::Duration::from_secs(5), b.probe()).await.unwrap();
    assert!(
        h.ok && h.version.starts_with("OpenSearch ") && h.metrics.len() == 3 && !h.latency.is_zero(),
        "{h:?}"
    );
}
