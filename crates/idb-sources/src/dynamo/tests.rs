use std::sync::atomic::{AtomicUsize, Ordering};

use aws_sdk_dynamodb::types::{AttributeDefinition, GlobalSecondaryIndexDescription, LocalSecondaryIndexDescription, ScalarAttributeType};
use aws_smithy_types::Blob;

use super::*;
use crate::source::set_billed_for_test;

fn s(v: &str) -> AttributeValue {
    AttributeValue::S(v.into())
}

fn n(v: &str) -> AttributeValue {
    AttributeValue::N(v.into())
}

#[test]
fn 타입마다_보일_글자로_푼다() {
    let m = AttributeValue::M(HashMap::from([
        ("b".to_string(), n("1.50")),
        ("a".to_string(), s("<x&y>")),
        ("n".to_string(), AttributeValue::Null(true)),
    ]));
    let l = AttributeValue::L(vec![
        s("a"),
        AttributeValue::Bool(true),
        AttributeValue::L(vec![n("2")]),
        AttributeValue::B(Blob::new("hi")),
    ]);
    let cases = [
        (s("안녕"), "S", "안녕"),
        (n("12345678901234567890.10"), "N", "12345678901234567890.10"),
        (AttributeValue::Bool(true), "BOOL", "true"),
        (AttributeValue::Bool(false), "BOOL", "false"),
        (AttributeValue::Null(true), "NULL", "NULL"),
        (AttributeValue::B(Blob::new("hi")), "B", "aGk="),
        (m, "M", r#"{"a":"<x&y>","b":1.50,"n":null}"#),
        (l, "L", r#"["a",true,[2],"aGk="]"#),
        (AttributeValue::Ss(vec!["x".into(), "y".into()]), "SS", r#"["x","y"]"#),
        (AttributeValue::Ns(vec!["1".into(), "2.0".into()]), "NS", "[1,2.0]"),
        (AttributeValue::Bs(vec![Blob::new("hi")]), "BS", r#"["aGk="]"#),
    ];
    for (v, typ, text) in cases {
        assert_eq!(cell(&v), (typ, text.to_string()), "{v:?}");
    }
    assert_eq!(cell(&AttributeValue::L(vec![n("1x")])).1, r#"<json: invalid number literal "1x">"#);
}

#[test]
fn 키_열_먼저_나머지는_이름순이고_없는_속성은_NULL() {
    let items = vec![
        HashMap::from([
            ("zeta".into(), s("z")),
            ("sk".into(), n("1")),
            ("pk".into(), s("a")),
            ("alpha".into(), n("3")),
        ]),
        HashMap::from([
            ("pk".into(), s("b")),
            ("sk".into(), n("2")),
            ("alpha".into(), s("문자")),
            ("gsiPK".into(), s("g")),
        ]),
    ];
    let keys: Vec<String> = ["pk", "sk", "gsiPK", ""].map(String::from).to_vec();
    let res = to_result(items, 10, &keys);
    assert_eq!(res.cols, ["pk", "sk", "gsiPK", "alpha", "zeta"]);
    assert_eq!(res.types.unwrap(), ["S", "N", "S", "N", "S"]);
    assert_eq!(res.rows, [["a", "1", "NULL", "3", "z"], ["b", "2", "g", "문자", "NULL"]]);
    assert!(!res.truncated);
}

#[test]
fn 항목에_없는_키_열은_빼고_한도를_넘으면_자른다() {
    let items = (1..=3).map(|i| HashMap::from([("a".to_string(), s(&i.to_string()))])).collect();
    let res = to_result(items, 2, &["pk".into(), "sk".into()]);
    assert_eq!(res.cols, ["a"]);
    assert_eq!(res.rows.len(), 2);
    assert!(res.truncated);
    let empty = to_result(Vec::new(), 10, &["pk".into()]);
    assert!(empty.cols.is_empty() && empty.rows.is_empty() && empty.types == Some(vec![]));
}

#[test]
fn 항목_수_키_인덱스를_보인다() {
    let ks = |h: &str, r: &str| {
        let mut out = vec![
            KeySchemaElement::builder()
                .attribute_name(h)
                .key_type(KeyType::Hash)
                .build()
                .unwrap(),
        ];
        if !r.is_empty() {
            out.push(
                KeySchemaElement::builder()
                    .attribute_name(r)
                    .key_type(KeyType::Range)
                    .build()
                    .unwrap(),
            );
        }
        out
    };
    let attr = |n: &str, t| AttributeDefinition::builder().attribute_name(n).attribute_type(t).build().unwrap();
    let d = TableDescription::builder()
        .table_name("orders")
        .item_count(42)
        .set_key_schema(Some(ks("pk", "sk")))
        .attribute_definitions(attr("pk", ScalarAttributeType::S))
        .attribute_definitions(attr("sk", ScalarAttributeType::N))
        .global_secondary_indexes(
            GlobalSecondaryIndexDescription::builder()
                .index_name("byStatus")
                .set_key_schema(Some(ks("status", "")))
                .build(),
        )
        .local_secondary_indexes(
            LocalSecondaryIndexDescription::builder()
                .index_name("byDate")
                .set_key_schema(Some(ks("pk", "date")))
                .build(),
        )
        .build();
    let got: Vec<String> = table_nodes(&d).iter().map(|n| format!("{}|{}", n.name, n.info)).collect();
    assert_eq!(
        got,
        ["항목 수|≈42", "PK pk|S", "SK sk|N", "gsi byStatus|status", "lsi byDate|pk/date"]
    );
}

// ─── 가짜 API ───

/// ExecuteStatement 마다 항목 per_page 개와 늘 NextToken 을 돌려준다(거르기로 거의 안 맞는 큰 테이블).
/// block_from 번째 호출부터는 답하지 않고 기다린다. pages 는 ListTables 의 페이지이고 list_err 가 있으면 끝 페이지 뒤에 실패한다.
#[derive(Default)]
struct Fake {
    per_page: usize,
    calls: AtomicUsize,
    block_from: Option<usize>,
    range_key: String,
    pages: Vec<Vec<String>>,
    list_err: Option<AwsErr>,
    list_calls: AtomicUsize,
}

impl Api for Fake {
    async fn list_tables(&self, _start: Option<String>) -> Result<(Vec<String>, Option<String>), AwsErr> {
        let call = self.list_calls.fetch_add(1, Ordering::SeqCst) + 1;
        if call > self.pages.len() {
            return match &self.list_err {
                Some(e) => Err(e.clone()),
                None => Ok((Vec::new(), None)),
            };
        }
        let more = call < self.pages.len() || self.list_err.is_some();
        Ok((self.pages[call - 1].clone(), more.then(|| "next".to_string())))
    }

    async fn describe_table(&self, table: &str) -> Result<Option<TableDescription>, AwsErr> {
        let mut b = TableDescription::builder().table_name(table).key_schema(
            KeySchemaElement::builder()
                .attribute_name("pk")
                .key_type(KeyType::Hash)
                .build()
                .unwrap(),
        );
        if !self.range_key.is_empty() {
            b = b.key_schema(
                KeySchemaElement::builder()
                    .attribute_name(&self.range_key)
                    .key_type(KeyType::Range)
                    .build()
                    .unwrap(),
            );
        }
        Ok(Some(b.build()))
    }

    async fn execute_statement(
        &self,
        _stmt: &str,
        _limit: i32,
        _next: Option<String>,
    ) -> Result<(Vec<HashMap<String, AttributeValue>>, Option<String>), AwsErr> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if self.block_from.is_some_and(|b| call >= b) {
            std::future::pending::<()>().await;
        }
        let items = (0..self.per_page)
            .map(|i| HashMap::from([("pk".to_string(), s(&format!("{call}-{i}")))]))
            .collect();
        Ok((items, Some("next".into())))
    }
}

fn q(text: &str, force: bool, limit: usize) -> Query {
    Query {
        text: text.into(),
        force,
        limit,
        ..Default::default()
    }
}

#[tokio::test]
async fn 쓰기_문장은_보내지_않고_Write() {
    set_billed_for_test(true);
    let b = Backend::with_api(Fake::default(), false);
    for text in [
        r#"DELETE FROM "t" WHERE pk='a'"#,
        "UPDATE t SET a=1",
        "INSERT INTO t VALUE {'pk':'a'}",
    ] {
        assert!(matches!(b.run(&q(text, true, 0)).await, Err(Error::Write(_))), "{text}");
    }
    assert_eq!(b.cl.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn 테이블_Enter_는_스캔이라_늘_확인을_받는다() {
    set_billed_for_test(true);
    let b = Backend::with_api(Fake::default(), false);
    assert_eq!(b.open().unwrap_err(), Error::NeedsConfirm("전체 스캔".into()));
}

#[tokio::test]
async fn 페이지가_끝없이_이어지면_상한에서_멈추고_잘림으로_알린다() {
    set_billed_for_test(true);
    let b = Backend::with_api(Fake::default(), false);
    let res = b.run(&q(r#"SELECT * FROM "t" WHERE status = 'x'"#, true, 10)).await.unwrap();
    assert_eq!(b.cl.calls.load(Ordering::SeqCst), MAX_PAGES);
    assert!(res.truncated && res.text.contains("페이지"), "{}", res.text);
}

#[tokio::test(start_paused = true)]
async fn 마감이_지나면_받은_데까지_잘림으로_돌려준다() {
    set_billed_for_test(true);
    let b = Backend::with_api(
        Fake {
            per_page: 1,
            block_from: Some(3),
            ..Default::default()
        },
        false,
    );
    let mut query = q(r#"SELECT * FROM "t""#, true, 100);
    query.deadline = Some(tokio::time::Instant::now() + Duration::from_millis(50));
    let res = b.run(&query).await.unwrap();
    assert_eq!(res.rows.len(), 2);
    assert!(res.truncated && res.text.contains("시간"), "{}", res.text);
}

#[tokio::test(start_paused = true)]
async fn 받은_것_없이_마감이_지나면_오류다() {
    set_billed_for_test(true);
    let b = Backend::with_api(
        Fake {
            block_from: Some(1),
            ..Default::default()
        },
        false,
    );
    // 마감이 없으면 기본 60초 상한이 걸린다(시간이 멈춘 시험이라 바로 지난다)
    assert!(b.run(&q(r#"SELECT * FROM "t""#, true, 0)).await.is_err());
}

#[tokio::test]
async fn 정렬_키는_파티션_키_같음_조건이면_run_이_읽은_키다() {
    set_billed_for_test(true);
    let b = Backend::with_api(
        Fake {
            per_page: 1,
            range_key: "sk".into(),
            ..Default::default()
        },
        false,
    );
    let text = r#"SELECT * FROM "t" WHERE pk = 'a'"#;
    assert_eq!(b.sort_key(text), Err("키 정보 없음"));
    b.run(&q(text, false, 1)).await.unwrap();
    let cases = [
        (text, Ok("sk")),
        (r#"SELECT * FROM "t" WHERE "pk" = 'a' AND sk > 3"#, Ok("sk")),
        (r#"SELECT * FROM "t" WHERE pk IN ['a','b']"#, Err("파티션 키 = 조건 없음")),
        (r#"SELECT * FROM "t" WHERE status = 'x'"#, Err("파티션 키 = 조건 없음")),
        (r#"SELECT * FROM "other" WHERE pk = 'a'"#, Err("키 정보 없음")),
        (r#"UPDATE "t" SET a = 1"#, Err("SELECT 가 아님")),
    ];
    for (text, want) in cases {
        assert_eq!(b.sort_key(text).as_deref().map_err(|e| *e), want, "{text}");
    }
    let nosk = Backend::with_api(
        Fake {
            per_page: 1,
            ..Default::default()
        },
        false,
    );
    nosk.run(&q(text, false, 1)).await.unwrap();
    assert_eq!(nosk.sort_key(text), Err("정렬 키 없는 테이블"));
    assert_eq!(
        sorted(text, "sk", true).as_deref(),
        Some(r#"SELECT * FROM "t" WHERE pk = 'a' ORDER BY "sk" DESC"#)
    );
}

#[tokio::test]
async fn 요금이_막혀_있으면_Enter_와_PartiQL_은_보내지_않고_목록과_확인은_된다() {
    set_billed_for_test(false);
    let b = Backend::with_api(
        Fake {
            per_page: 1,
            pages: vec![vec!["t".into()]],
            ..Default::default()
        },
        false,
    );
    assert_eq!(b.open().unwrap_err(), Error::Billed, "Enter 는 확인보다 요금 막음이 먼저다");
    for query in [
        q(r#"SELECT * FROM "t" WHERE pk = 'a'"#, false, 0),
        q(r#"SELECT * FROM "t""#, true, 0),
    ] {
        assert_eq!(b.run(&query).await.unwrap_err(), Error::Billed);
    }
    assert_eq!(b.cl.calls.load(Ordering::SeqCst), 0);
    assert_eq!(b.children("").await.unwrap().len(), 1, "ListTables 는 요금이 없다");
    assert!(b.probe().await.ok);
    set_billed_for_test(true);
    b.run(&q(r#"SELECT * FROM "t" WHERE pk = 'a'"#, false, 0)).await.unwrap();
    assert!(b.cl.calls.load(Ordering::SeqCst) > 0);
}

#[tokio::test]
async fn 이_머신의_DynamoDB_Local_은_요금_막음을_건너뛴다() {
    set_billed_for_test(false);
    for (url, local) in [
        ("http://localhost:8000", true),
        ("http://127.0.0.1:8000", true),
        ("http://[::1]:8000", true),
        ("", false),
        ("https://dynamodb.ap-northeast-2.amazonaws.com", false),
        ("http://localhost.evil.example", false),
    ] {
        assert_eq!(is_local_url(url), local, "{url}");
    }
    let b = Backend::with_api(
        Fake {
            per_page: 1,
            ..Default::default()
        },
        true,
    );
    assert!(b.run(&q(r#"SELECT * FROM "t" WHERE pk = 'a'"#, false, 0)).await.is_ok());
}

#[tokio::test]
async fn 확인은_ListTables_페이지를_이어_테이블_수를_센다() {
    let b = Backend::with_api(
        Fake {
            pages: vec![vec!["a".into(), "b".into()], vec!["c".into()]],
            ..Default::default()
        },
        false,
    );
    let h = b.probe().await;
    assert!(h.ok && h.version.is_empty());
    assert_eq!(h.metrics, [Metric::new("테이블", "3", false)]);
    assert_eq!(b.cl.list_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn 확인의_AWS_오류는_작업_이름과_코드로_실패다() {
    let b = Backend::with_api(
        Fake {
            pages: vec![vec!["a".into()]],
            list_err: Some(AwsErr::api("AccessDeniedException", "not authorized")),
            ..Default::default()
        },
        false,
    );
    let h = b.probe().await;
    assert!(!h.ok);
    assert_eq!(h.err, "dynamodb ListTables: AccessDeniedException: not authorized");
}
