//! Go `obs_test.go`·`probe_test.go` 를 옮겼다. 가짜 HTTP 서버로 보낸 요청과 결과 표를 본다.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use idb_core::project::{Conn, Kind};
use idb_core::result::QueryResult;

use super::grafana::split_directive;
use super::*;
use crate::fakehttp::{Fake, Reply, Seen, redirect, reply, serve};
use crate::source::{Error, LOKI_OPEN_LIMIT, Query};

const MIN_NS: i64 = 60 * SECOND_NS;
const HOUR_NS: i64 = 60 * MIN_NS;

/// 2026-11-15 12:00:00 KST
fn fixed_now() -> i64 {
    chrono::DateTime::parse_from_rfc3339("2026-11-15T03:00:00Z")
        .unwrap()
        .timestamp_nanos_opt()
        .unwrap()
}

type Route = Box<dyn Fn(&Seen) -> Reply + Send + Sync>;

/// 경로별 응답을 돌려주고 받은 요청을 적어 둔다. 등록 안 된 경로는 404, GET 이 아니면 405 다.
fn fake(routes: Vec<(&str, Route)>) -> Fake {
    let routes: HashMap<String, Route> = routes.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    serve(move |r| {
        if r.method != "GET" {
            return reply(405, "method\n");
        }
        match routes.get(&r.path) {
            Some(h) => h(r),
            None => reply(404, "404 page not found\n"),
        }
    })
}

fn ok(body: &str) -> Route {
    let body = body.to_string();
    Box::new(move |_| reply(200, body.clone()))
}

fn success(data: &str) -> Route {
    ok(&format!(r#"{{"status":"success","data":{data}}}"#))
}

fn status(code: u16, body: &str) -> Route {
    let body = body.to_string();
    Box::new(move |_| reply(code, body.clone()))
}

fn with_url(base: &str, c: Conn) -> Conn {
    Conn { url: base.into(), ..c }
}

fn loki_at(base: &str, c: Conn) -> Loki {
    let mut l = Loki::new(&with_url(base, c)).unwrap();
    l.now = fixed_now;
    l
}

fn prom_at(base: &str, c: Conn) -> Prometheus {
    let mut p = Prometheus::new(&with_url(base, c)).unwrap();
    p.now = fixed_now;
    p
}

fn basic(user: &str, pw: &str) -> Conn {
    Conn {
        auth: "basic".into(),
        user: user.into(),
        password: pw.into(),
        ..Default::default()
    }
}

fn token(tok: &str) -> Conn {
    Conn {
        auth: "token".into(),
        password: tok.into(),
        ..Default::default()
    }
}

fn q(text: &str) -> Query {
    Query {
        text: text.into(),
        ..Default::default()
    }
}

fn rows_text(r: &QueryResult) -> String {
    let mut s = r.cols.join("|") + "\n";
    for row in &r.rows {
        s += &(row.join("|") + "\n");
    }
    s
}

// ─── Loki ───

#[tokio::test]
async fn Loki_트리는_최근_1시간_라벨_이름과_값이고_basic_인증을_보낸다() {
    let f = fake(vec![
        ("/loki/api/v1/labels", success(r#"["app","env"]"#)),
        ("/loki/api/v1/label/app/values", success(r#"["api","web/v2"]"#)),
    ]);
    let l = loki_at(&f.url, basic("mon", "pw"));
    let nodes = l.children("").await.unwrap();
    assert!(
        nodes.len() == 2 && nodes[0].path == "app" && nodes[0].has_children && !nodes[0].openable,
        "{nodes:?}"
    );
    let r = f.last();
    assert_eq!(r.q("start"), (fixed_now() - HOUR_NS).to_string());
    assert_eq!(r.q("end"), fixed_now().to_string());
    assert_eq!(r.basic(), "mon:pw");

    let vals = l.children("app").await.unwrap();
    assert!(
        vals.len() == 2 && vals[1].path == "app/web/v2" && vals[1].openable && !vals[1].has_children,
        "{vals:?}"
    );
    assert!(l.children("app/api").await.unwrap().is_empty());
}

#[tokio::test]
async fn Loki_로그_질의는_범위_한도_방향을_보내고_최신_먼저_라벨_합집합으로_편다() {
    // 11:00:00.123 KST
    let t1 = fixed_now() - HOUR_NS + 123_000_000;
    let (t2, t3) = (t1 + MIN_NS, t1 + 2 * MIN_NS);
    let f = fake(vec![(
        "/loki/api/v1/query_range",
        success(&format!(
            r#"{{"resultType":"streams","result":[
            {{"stream":{{"app":"api","pod":"a"}},"values":[["{t3}","third"],["{t1}","first"]]}},
            {{"stream":{{"app":"web","env":"stg"}},"values":[["{t2}","second",{{"trace_id":"x"}}]]}}]}}"#
        )),
    )]);
    let l = loki_at(&f.url, Conn::default());
    let res = l
        .run(&Query {
            since_ns: 2 * HOUR_NS,
            until_ns: 10 * MIN_NS,
            ..q(r#"{app=~".+"} |= "x""#)
        })
        .await
        .unwrap();
    let r = f.last();
    assert_eq!(r.q("query"), r#"{app=~".+"} |= "x""#);
    assert_eq!(r.q("start"), (fixed_now() - 2 * HOUR_NS).to_string());
    assert_eq!(r.q("end"), (fixed_now() - 10 * MIN_NS).to_string());
    assert_eq!(r.q("limit"), "1000");
    assert_eq!(r.q("direction"), "backward");
    assert_eq!(r.header("authorization"), "");
    let want = "time|app|env|pod|line\n\
        2026-11-15 11:02:00.123|api||a|third\n\
        2026-11-15 11:01:00.123|web|stg||second\n\
        2026-11-15 11:00:00.123|api||a|first\n";
    assert_eq!(rows_text(&res), want);
    assert!(!res.truncated);
}

#[tokio::test]
async fn Loki_한도는_5000에서_막고_서버가_한도만큼_주면_잘림으로_표시한다() {
    let f = fake(vec![(
        "/loki/api/v1/query_range",
        success(r#"{"resultType":"streams","result":[{"stream":{"a":"1"},"values":[["2","b"],["1","a"]]}]}"#),
    )]);
    let l = loki_at(&f.url, Conn::default());
    l.run(&Query {
        limit: 9000,
        ..q(r#"{a="1"}"#)
    })
    .await
    .unwrap();
    assert_eq!(f.last().q("limit"), "5000");
    let res = l
        .run(&Query {
            limit: 2,
            ..q(r#"{a="1"}"#)
        })
        .await
        .unwrap();
    assert!(res.truncated);
}

#[tokio::test]
async fn Loki_지표_질의는_시계열별로_묶은_time_라벨_value_표다() {
    let f = fake(vec![(
        "/loki/api/v1/query_range",
        success(
            r#"{"resultType":"matrix","result":[
            {"metric":{"status":"500"},"values":[[1794708000,"3"],[1794708060.5,"4"]]},
            {"metric":{"status":"200"},"values":[[1794708000,"10"]]}]}"#,
        ),
    )]);
    let l = loki_at(&f.url, Conn::default());
    let res = l.run(&q(r#"sum by (status) (count_over_time({a="1"}[1m]))"#)).await.unwrap();
    let want = "time|status|value\n\
        2026-11-15 11:00:00.000|200|10\n\
        2026-11-15 11:00:00.000|500|3\n\
        2026-11-15 11:01:00.500|500|4\n";
    assert_eq!(rows_text(&res), want);
    assert_eq!(f.last().q("step"), "15");
}

#[tokio::test]
async fn Loki_범위가_7일을_넘으면_force_없이는_보내지_않는다() {
    let f = fake(vec![(
        "/loki/api/v1/query_range",
        success(r#"{"resultType":"streams","result":[]}"#),
    )]);
    let l = loki_at(&f.url, Conn::default());
    let mut query = Query {
        since_ns: 8 * 24 * HOUR_NS,
        ..q(r#"{a="1"}"#)
    };
    let err = l.run(&query).await.unwrap_err();
    assert_eq!(err, Error::NeedsConfirm("범위 7일 초과".into()));
    assert_eq!(f.count(), 0);
    // 경계: until 로 줄여 정확히 7일이면 묻지 않는다
    l.run(&Query {
        until_ns: 24 * HOUR_NS,
        ..query.clone()
    })
    .await
    .unwrap();
    query.force = true;
    let res = l.run(&query).await.unwrap();
    assert_eq!(f.count(), 2);
    assert_eq!(res.cols.join("|"), "time|line");
}

#[tokio::test]
async fn Loki_빈_범위와_빈_질의는_거절한다() {
    let l = loki_at("http://127.0.0.1:1", Conn::default());
    let err = l
        .run(&Query {
            since_ns: HOUR_NS,
            until_ns: HOUR_NS,
            ..q(r#"{a="1"}"#)
        })
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "empty range: until 1h0m0s must be shorter than since 1h0m0s");
    assert_eq!(l.run(&q("  \n")).await.unwrap_err(), empty_query());
}

#[tokio::test]
async fn Loki_값_Enter는_라벨_선택자_최근_100줄이다() {
    let f = fake(vec![(
        "/loki/api/v1/query_range",
        success(r#"{"resultType":"streams","result":[]}"#),
    )]);
    let l = loki_at(&f.url, Conn::default());
    l.open(r#"app/we"b/v2"#, 0).await.unwrap();
    let r = f.last();
    assert_eq!(r.q("query"), r#"{app="we\"b/v2"}"#);
    assert_eq!(r.q("limit"), "100");
    assert_eq!(r.q("start"), (fixed_now() - HOUR_NS).to_string());
    assert!(l.open("app", 0).await.is_err(), "label without value must fail");
}

// ─── Prometheus ───

#[tokio::test]
async fn Prometheus_트리는_지표_이름_앞_1000개다() {
    let names: Vec<String> = (0..1200).map(|i| format!(r#""m{i:04}""#)).collect();
    let f = fake(vec![("/api/v1/label/__name__/values", success(&format!("[{}]", names.join(","))))]);
    let p = prom_at(&f.url, Conn::default());
    let nodes = p.children("").await.unwrap();
    assert_eq!(nodes.len(), 1000);
    assert_eq!(nodes[999].path, "m0999");
    assert!(nodes[0].openable && !nodes[0].has_children);
    assert_eq!(f.last().q("limit"), "1000");
    assert!(p.children("m0001").await.unwrap().is_empty());
}

#[tokio::test]
async fn Prometheus_Enter는_즉시값_vector_표다() {
    let f = fake(vec![(
        "/api/v1/query",
        success(
            r#"{"resultType":"vector","result":[
            {"metric":{"__name__":"up","job":"node"},"value":[1794711600.25,"0"]},
            {"metric":{"__name__":"up","instance":"a:9090","job":"api"},"value":[1794711600.25,"1"]}]}"#,
        ),
    )]);
    let p = prom_at(&f.url, token("tok"));
    let res = p.open("up", 0).await.unwrap();
    let r = f.last();
    assert_eq!(r.q("query"), "up");
    assert_eq!(r.q("time"), "1794711600.000");
    assert_eq!(r.header("authorization"), "Bearer tok");
    let want = "time|__name__|instance|job|value\n\
        2026-11-15 12:00:00.250|up|a:9090|api|1\n\
        2026-11-15 12:00:00.250|up||node|0\n";
    assert_eq!(rows_text(&res), want);
}

#[tokio::test]
async fn Prometheus_실행은_범위_질의이고_step은_범위의_250분의_1_최소_15초다() {
    let f = fake(vec![(
        "/api/v1/query_range",
        success(
            r#"{"resultType":"matrix","result":[
            {"metric":{"status":"200"},"values":[[1794708000,"1"],[1794708015,"2"],[1794708030,"3"]]}]}"#,
        ),
    )]);
    let p = prom_at(&f.url, Conn::default());
    let res = p
        .run(&Query {
            limit: 2,
            ..q("sum by (status) (rate(x[5m]))")
        })
        .await
        .unwrap();
    let r = f.last();
    assert_eq!(r.q("start"), "1794708000.000");
    assert_eq!(r.q("end"), "1794711600.000");
    assert_eq!(r.q("step"), "15");
    assert_eq!(res.rows.len(), 2);
    assert!(res.truncated);
    p.run(&Query {
        since_ns: 24 * HOUR_NS,
        ..q("x")
    })
    .await
    .unwrap();
    assert_eq!(f.last().q("step"), "345");
}

#[tokio::test]
async fn Prometheus_scalar_결과는_time_value_한_줄이다() {
    let f = fake(vec![(
        "/api/v1/query_range",
        success(r#"{"resultType":"scalar","result":[1794711600,"2"]}"#),
    )]);
    let p = prom_at(&f.url, Conn::default());
    let res = p.run(&q("1+1")).await.unwrap();
    assert_eq!(rows_text(&res), "time|value\n2026-11-15 12:00:00.000|2\n");
}

// ─── 오류·인증 ───

#[tokio::test]
async fn 오류는_서버_메시지를_담고_비밀값은_지운다() {
    let f = fake(vec![
        (
            "/api/v1/query_range",
            status(
                400,
                r#"{"status":"error","errorType":"bad_data","error":"parse error near s3cr3t!"}"#,
            ),
        ),
        (
            "/loki/api/v1/query_range",
            status(400, "parse error at line 1, col 2: syntax error\n"),
        ),
    ]);
    let p = prom_at(&f.url, basic("u", "s3cr3t!"));
    let err = p.run(&q("x{")).await.unwrap_err().to_string();
    assert!(
        err.contains("400") && err.contains("parse error near ***") && !err.contains("s3cr3t"),
        "{err}"
    );
    let l = loki_at(&f.url, Conn::default());
    let err = l.run(&q("{")).await.unwrap_err().to_string();
    assert!(err.contains("loki 400 Bad Request: parse error at line 1"), "{err}");
}

#[tokio::test]
async fn 주소의_userinfo는_떼어_basic으로_쓰고_오류에_나오지_않는다() {
    let f = fake(vec![("/api/v1/label/__name__/values", success(r#"["up"]"#))]);
    let url = f.url.replace("http://", "http://mon:p%40ss@");
    let p = prom_at(
        &url,
        Conn {
            auth: "basic".into(),
            ..Default::default()
        },
    );
    assert!(!p.c.base.contains("p%40ss") && !p.c.base.contains("mon"), "{}", p.c.base);
    p.children("").await.unwrap();
    assert_eq!(f.last().basic(), "mon:p@ss");
}

#[test]
fn 지원하지_않는_인증과_잘못된_주소는_만들지_않는다() {
    for (url, auth) in [
        ("http://x", "sigv4"),
        ("localhost:3100", ""),
        ("", ""),
        ("http://mon:p@ss@x:3100", ""), // 인증 없음인데 주소에 user:pass 를 조용히 버리지 않는다
    ] {
        let c = Conn {
            url: url.into(),
            auth: auth.into(),
            ..Default::default()
        };
        assert!(Loki::new(&c).is_err(), "Loki::new({url:?}, {auth:?}) succeeded");
    }
}

// ─── Grafana ───

const GRAFANA_DS: &str = r#"[
    {"uid":"lk1","name":"Loki (Legacy)","type":"loki"},
    {"uid":"my1","name":"MySQL","type":"mysql"},
    {"uid":"pr1","name":"Prom","type":"prometheus"},
    {"uid":"lk2","name":"Loki B","type":"loki"}]"#;

fn grafana_fake() -> (Fake, Grafana) {
    let f = fake(vec![
        ("/grafana/api/datasources", ok(GRAFANA_DS)),
        ("/grafana/api/datasources/proxy/uid/lk1/loki/api/v1/labels", success(r#"["app"]"#)),
        (
            "/grafana/api/datasources/proxy/uid/lk1/loki/api/v1/query_range",
            success(r#"{"resultType":"streams","result":[]}"#),
        ),
        (
            "/grafana/api/datasources/proxy/uid/lk2/loki/api/v1/query_range",
            success(r#"{"resultType":"streams","result":[]}"#),
        ),
        (
            "/grafana/api/datasources/proxy/uid/pr1/api/v1/query",
            success(r#"{"resultType":"vector","result":[]}"#),
        ),
        (
            "/grafana/api/datasources/proxy/uid/pr1/api/v1/query_range",
            success(r#"{"resultType":"matrix","result":[]}"#),
        ),
    ]);
    let mut g = Grafana::new(&with_url(&format!("{}/grafana/", f.url), token("glsa_x"))).unwrap();
    g.now = fixed_now;
    (f, g)
}

#[tokio::test]
async fn Grafana_트리는_Loki_Prometheus_데이터_소스이고_아래는_프록시로_보낸다() {
    let (f, g) = grafana_fake();
    let nodes = g.children("").await.unwrap();
    let got: Vec<String> = nodes.iter().map(|n| format!("{}={}:{}", n.name, n.path, n.info)).collect();
    assert_eq!(got.join(","), "Loki (Legacy)=lk1:loki,Prom=pr1:prometheus,Loki B=lk2:loki");
    assert_eq!(f.last().header("authorization"), "Bearer glsa_x");

    let labels = g.children("lk1").await.unwrap();
    assert!(labels.len() == 1 && labels[0].path == "lk1/app", "{labels:?}");
    let r = f.last();
    assert_eq!(r.path, "/grafana/api/datasources/proxy/uid/lk1/loki/api/v1/labels");
    assert_eq!(r.header("authorization"), "Bearer glsa_x");

    g.open("pr1/up", 0).await.unwrap();
    assert_eq!(f.last().path, "/grafana/api/datasources/proxy/uid/pr1/api/v1/query");
    assert!(g.children("my1").await.is_err(), "non loki/prometheus datasource must fail");
}

#[tokio::test]
async fn Grafana_실행은_지시줄의_데이터_소스로_없으면_첫_Loki로_보낸다() {
    let (f, g) = grafana_fake();
    g.run(&q(r#"{app="api"}"#)).await.unwrap();
    assert_eq!(f.last().path, "/grafana/api/datasources/proxy/uid/lk1/loki/api/v1/query_range");

    g.run(&q("\n# datasource: Prom\nsum(up)")).await.unwrap();
    let r = f.last();
    assert_eq!(r.path, "/grafana/api/datasources/proxy/uid/pr1/api/v1/query_range");
    assert_eq!(r.q("query"), "sum(up)");

    g.run(&q("#DataSource:lk2\n{a=\"1\"}")).await.unwrap();
    assert_eq!(f.last().path, "/grafana/api/datasources/proxy/uid/lk2/loki/api/v1/query_range");

    let err = g.run(&q("# datasource: MySQL\nselect 1")).await.unwrap_err().to_string();
    assert!(err.contains(r#""MySQL""#), "{err}");
}

#[test]
fn 지시줄은_첫_비어_있지_않은_줄만_본다() {
    for (input, key, rest) in [
        ("# datasource: Loki B\n{a=\"1\"}", Some("Loki B"), r#"{a="1"}"#),
        ("  #datasource:pr1  \nup", Some("pr1"), "up"),
        ("up\n# datasource: Prom", None, "up\n# datasource: Prom"),
        ("# 주석\nup", None, "# 주석\nup"),
        ("", None, ""),
    ] {
        assert_eq!(split_directive(input), (key.map(String::from), rest.to_string()), "{input:?}");
    }
}

#[tokio::test]
async fn 리다이렉트를_따라가지_않고_토큰을_다시_보내지_않는다() {
    let second = Arc::new(AtomicUsize::new(0));
    let s2 = second.clone();
    let target = serve(move |_| {
        s2.fetch_add(1, Ordering::SeqCst);
        reply(200, "")
    });
    let turl = target.url.clone();
    let srv = serve(move |r| redirect(&format!("{turl}{}", r.path)));
    let p = prom_at(&srv.url, token("glsa_secret"));
    let err = p.children("").await.unwrap_err().to_string();
    assert!(err.contains("redirect not followed: 302") && !err.contains("glsa_secret"), "{err}");
    assert_eq!(second.load(Ordering::SeqCst), 0, "리다이렉트를 따라갔다");
}

// ─── R62.3 Loki time 정렬 방향 ───

#[tokio::test]
async fn Loki_forward_는_방향을_보내고_오래된_것부터_편다() {
    let f = fake(vec![(
        "/loki/api/v1/query_range",
        success(
            r#"{"resultType":"streams","result":[
            {"stream":{"a":"1"},"values":[["3000000000","c"],["1000000000","a"]]},
            {"stream":{"a":"2"},"values":[["2000000000","b"]]}]}"#,
        ),
    )]);
    let l = loki_at(&f.url, Conn::default());
    let res = l
        .run(&Query {
            direction: "forward".into(),
            ..q(r#"{a=~".+"}"#)
        })
        .await
        .unwrap();
    assert_eq!(f.last().q("direction"), "forward");
    let lines: Vec<&str> = res.rows.iter().map(|r| r.last().unwrap().as_str()).collect();
    assert_eq!(lines.join(","), "a,b,c");
    let sideways = Query {
        direction: "sideways".into(),
        ..q(r#"{a="1"}"#)
    };
    assert!(l.run(&sideways).await.is_err(), "모르는 방향은 보내지 않는다");
}

#[tokio::test]
async fn 미리_보기_질의는_같은_질의이고_Grafana_는_지시줄을_싣는다() {
    let lq = open_query(Kind::Loki, "app/a/b", 0).unwrap();
    assert_eq!((lq.text.as_str(), lq.limit), (r#"{app="a/b"}"#, LOKI_OPEN_LIMIT));
    let mut gq = open_query(Kind::Grafana, "lk1/app/api", 30).unwrap();
    assert_eq!((gq.text.as_str(), gq.limit), ("# datasource: lk1\n{app=\"api\"}", 30));
    assert!(open_query(Kind::Grafana, "pr1/up", 0).is_none(), "지표 경로는 Loki 질의가 아니다");

    let (f, g) = grafana_fake();
    gq.direction = "forward".into();
    g.run(&gq).await.unwrap();
    let r = f.last();
    assert_eq!(r.path, "/grafana/api/datasources/proxy/uid/lk1/loki/api/v1/query_range");
    assert_eq!(r.q("direction"), "forward");
}

// ─── 대시보드 확인 ───

#[tokio::test]
async fn Loki_확인은_ready와_최근_1시간_라벨_수와_버전이다() {
    let f = fake(vec![
        ("/ready", ok("ready\n")),
        ("/loki/api/v1/labels", success(r#"["app","job","level"]"#)),
        ("/loki/api/v1/status/buildinfo", ok(r#"{"version":"3.0.0","revision":"x"}"#)),
    ]);
    let h = loki_at(&f.url, Conn::default()).probe().await;
    assert!(h.ok && !h.latency.is_zero(), "{h:?}");
    assert_eq!(h.summary(), "ready · 라벨 3");
    assert_eq!(h.version, "Loki 3.0.0");
    assert_eq!(f.seen()[1].q("start"), (fixed_now() - HOUR_NS).to_string());
}

#[tokio::test]
async fn Loki_확인은_준비되지_않으면_실패이고_비밀번호를_지운다() {
    let f = fake(vec![("/ready", status(503, "Ingester not ready: pw=s3cret"))]);
    let h = loki_at(&f.url, basic("u", "s3cret")).probe().await;
    assert!(!h.ok && h.err.contains("loki 503") && !h.err.contains("s3cret"), "{h:?}");
}

#[tokio::test]
async fn Prometheus_확인은_up_대상_수이고_down_이_있으면_경고다() {
    let vector = |v: &str| format!(r#"{{"resultType":"vector","result":[{{"metric":{{}},"value":[1763175600,"{v}"]}}]}}"#);
    // count(up == 0) 은 down 이 없으면 빈 vector 다
    for (down, want, warn) in [("", "up 380/380", false), ("8", "up 372/380", true)] {
        let (total, down_body) = (vector("380"), if down.is_empty() { None } else { Some(vector(down)) });
        let route: Route = Box::new(move |r| match r.q("query") {
            "count(up)" => reply(200, format!(r#"{{"status":"success","data":{total}}}"#)),
            "count(up == 0)" => match &down_body {
                None => reply(200, r#"{"status":"success","data":{"resultType":"vector","result":[]}}"#),
                Some(b) => reply(200, format!(r#"{{"status":"success","data":{b}}}"#)),
            },
            _ => reply(400, r#"{"status":"error","error":"unexpected query"}"#),
        });
        let f = fake(vec![
            ("/api/v1/query", route),
            ("/api/v1/status/buildinfo", success(r#"{"version":"2.53.0","revision":"x"}"#)),
        ]);
        let h = prom_at(&f.url, Conn::default()).probe().await;
        assert!(
            h.ok && h.metrics[0].warn == warn && h.version == "Prometheus 2.53.0",
            "down {down:?}: {h:?}"
        );
        assert_eq!(h.summary(), want);
    }
}

#[tokio::test]
async fn Prometheus_확인은_buildinfo_가_없어도_OK이고_질의_오류는_토큰을_지운다() {
    let f = fake(vec![("/api/v1/query", success(r#"{"resultType":"vector","result":[]}"#))]);
    let h = prom_at(&f.url, Conn::default()).probe().await;
    assert!(h.ok && h.summary() == "up 0/0" && h.version.is_empty(), "{h:?}");
    let f = fake(vec![(
        "/api/v1/query",
        status(401, r#"{"status":"error","error":"bad token tok-123"}"#),
    )]);
    let h = prom_at(&f.url, token("tok-123")).probe().await;
    assert!(!h.ok && h.err.contains("401") && !h.err.contains("tok-123"), "{h:?}");
}

async fn grafana_probe(health: Route) -> crate::source::Health {
    let f = fake(vec![("/grafana/api/health", health)]);
    let g = Grafana::new(&with_url(&format!("{}/grafana/", f.url), token("glsa_x"))).unwrap();
    g.probe().await
}

#[tokio::test]
async fn Grafana_확인은_database_상태와_버전이고_ok_가_아니면_경고다() {
    let h = grafana_probe(ok(r#"{"commit":"abc","database":"ok","version":"11.4.0"}"#)).await;
    assert!(
        h.ok && h.summary() == "ok" && h.version == "Grafana 11.4.0" && !h.metrics[0].warn,
        "{h:?}"
    );
    let h = grafana_probe(ok(r#"{"database":"failing","version":"11.4.0"}"#)).await;
    assert!(h.ok && h.metrics[0].warn, "failing: {h:?}");
    let h = grafana_probe(status(503, r#"{"message":"db down glsa_x"}"#)).await;
    assert!(!h.ok && h.err.contains("grafana 503") && !h.err.contains("glsa_x"), "503: {h:?}");
}

// ─── 통합(로컬 컨테이너) ───

#[tokio::test]
async fn 통합_Loki_Prometheus_를_확인한다() {
    for (env, pre) in [("DBTUI_TEST_LOKI_URL", "Loki "), ("DBTUI_TEST_PROM_URL", "Prometheus ")] {
        let Ok(base) = std::env::var(env) else { continue };
        let c = with_url(&base, Conn::default());
        let h = if env.contains("LOKI") {
            Loki::new(&c).unwrap().probe().await
        } else {
            Prometheus::new(&c).unwrap().probe().await
        };
        assert!(h.ok && h.version.starts_with(pre) && !h.metrics.is_empty(), "{env}: {h:?}");
    }
}
