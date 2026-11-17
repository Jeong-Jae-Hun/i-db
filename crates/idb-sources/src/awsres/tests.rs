use std::sync::atomic::{AtomicUsize, Ordering};

use chrono::TimeZone;

use super::*;
use crate::source::set_billed_for_test;

// ─── 가짜 AWS ───
// 서비스마다 정한 값을 돌려주고 받은 요청을 센다. trap 이면 요금이 붙는 SNS·SQS·S3 를 부를 때마다 billed_calls 를 세고 실패한다.

fn denied(code: &str) -> AwsErr {
    AwsErr::api(code, "not authorized")
}

#[derive(Default)]
struct Fake {
    trap: bool,
    billed_calls: AtomicUsize,
    sts_err: Option<AwsErr>,
    sts_calls: AtomicUsize,
    topics: Vec<Vec<String>>,
    topic_err: Option<AwsErr>,
    subs: HashMap<String, Vec<Subscription>>,
    sub_attrs: HashMap<String, HashMap<String, String>>,
    urls: Vec<String>,
    qattrs: HashMap<String, HashMap<String, String>>,
    qattr_err: HashMap<String, AwsErr>,
    by_name: HashMap<String, String>,
    fns: Vec<Function>,
    fn_err: Option<AwsErr>,
    /// 함수 이름 → 매핑
    mappings: Mutex<HashMap<String, Vec<SourceMapping>>>,
    map_err: Option<AwsErr>,
    buses: Vec<Vec<String>>,
    rules: HashMap<String, Vec<Rule>>,
    targets: HashMap<String, Vec<Target>>,
    groups: Vec<String>,
    schedules: HashMap<String, Vec<String>>,
    detail: HashMap<String, Schedule>,
    machines: Vec<Machine>,
    execs: Vec<Execution>,
    exec_next: String,
    buckets: Vec<(String, String)>,
    listing: HashMap<String, Listing>,
    location: HashMap<String, String>,
    heads: HashMap<String, Head>,
    bodies: HashMap<String, Vec<u8>>,
    log: Mutex<Log>,
}

#[derive(Default)]
struct Log {
    attr_calls: Vec<String>,
    attr_names: Vec<Vec<String>>,
    url_calls: Vec<String>,
    sources: Vec<String>,
    targets_asked: Vec<String>,
    exec_limit: i32,
    regions: Vec<String>,
    ranges: Vec<String>,
    locations: usize,
}

impl Fake {
    fn billed(&self) -> Result<(), AwsErr> {
        if self.trap {
            self.billed_calls.fetch_add(1, Ordering::SeqCst);
            return Err(AwsErr::other("billed call"));
        }
        Ok(())
    }

    fn page<T: Clone>(pages: &[Vec<T>], next: Option<String>) -> Page<T> {
        let i: usize = next.map_or(0, |n| n.parse().unwrap());
        let items = pages.get(i).cloned().unwrap_or_default();
        Ok((items, (i + 1 < pages.len()).then(|| (i + 1).to_string())))
    }

    fn region(&self, r: Option<&str>) {
        self.log.lock().unwrap().regions.push(r.unwrap_or("ap-northeast-2").to_string());
    }
}

impl Api for Fake {
    async fn sts_caller(&self) -> Result<(String, String), AwsErr> {
        self.sts_calls.fetch_add(1, Ordering::SeqCst);
        match &self.sts_err {
            Some(e) => Err(e.clone()),
            None => Ok(("111122223333".into(), "arn:aws:iam::111122223333:user/dev".into())),
        }
    }

    async fn sns_topics(&self, next: Option<String>) -> Page<String> {
        self.billed()?;
        if let Some(e) = &self.topic_err {
            return Err(e.clone());
        }
        Fake::page(&self.topics, next)
    }

    async fn sns_subscriptions(&self, topic: &str, _next: Option<String>) -> Page<Subscription> {
        self.billed()?;
        Ok((self.subs.get(topic).cloned().unwrap_or_default(), None))
    }

    async fn sns_subscription_attrs(&self, arn: &str) -> Result<HashMap<String, String>, AwsErr> {
        self.billed()?;
        self.log.lock().unwrap().attr_calls.push(arn.into());
        Ok(self.sub_attrs.get(arn).cloned().unwrap_or_default())
    }

    async fn sqs_queues(&self, _next: Option<String>) -> Page<String> {
        self.billed()?;
        Ok((self.urls.clone(), None))
    }

    async fn sqs_queue_attrs(&self, url: &str, names: &[&str]) -> Result<HashMap<String, String>, AwsErr> {
        self.billed()?;
        self.log
            .lock()
            .unwrap()
            .attr_names
            .push(names.iter().map(|n| n.to_string()).collect());
        if let Some(e) = self.qattr_err.get(url) {
            return Err(e.clone());
        }
        Ok(self.qattrs.get(url).cloned().unwrap_or_default())
    }

    async fn sqs_queue_url(&self, name: &str, owner: Option<&str>) -> Result<String, AwsErr> {
        self.billed()?;
        self.log.lock().unwrap().url_calls.push(format!("{name}@{}", owner.unwrap_or("")));
        self.by_name
            .get(name)
            .cloned()
            .ok_or_else(|| AwsErr::api("AWS.SimpleQueueService.NonExistentQueue", "no queue"))
    }

    async fn lambda_functions(&self, _next: Option<String>) -> Page<Function> {
        match &self.fn_err {
            Some(e) => Err(e.clone()),
            None => Ok((self.fns.clone(), None)),
        }
    }

    /// function 으로 고르고, source 를 주면 모든 함수에서 그 소스의 매핑을 모은다. 둘 다 없으면 모든 매핑이다.
    async fn lambda_mappings(&self, function: Option<&str>, source: Option<&str>, _next: Option<String>) -> Page<SourceMapping> {
        if let Some(e) = &self.map_err {
            return Err(e.clone());
        }
        let maps = self.mappings.lock().unwrap();
        let out = match (function, source) {
            (None, None) => maps.values().flatten().cloned().collect(),
            (_, Some(src)) => {
                self.log.lock().unwrap().sources.push(src.into());
                maps.values().flatten().filter(|m| m.source_arn == src).cloned().collect()
            }
            (Some(f), None) => maps.get(f).cloned().unwrap_or_default(),
        };
        Ok((out, None))
    }

    async fn events_buses(&self, next: Option<String>) -> Page<String> {
        Fake::page(&self.buses, next)
    }

    async fn events_rules(&self, bus: &str, _next: Option<String>) -> Page<Rule> {
        Ok((self.rules.get(bus).cloned().unwrap_or_default(), None))
    }

    async fn events_targets(&self, bus: &str, rule: &str, _next: Option<String>) -> Page<Target> {
        let k = format!("{bus}|{rule}");
        self.log.lock().unwrap().targets_asked.push(k.clone());
        Ok((self.targets.get(&k).cloned().unwrap_or_default(), None))
    }

    async fn scheduler_groups(&self, _next: Option<String>) -> Page<String> {
        Ok((self.groups.clone(), None))
    }

    async fn scheduler_schedules(&self, group: &str, _next: Option<String>) -> Page<String> {
        Ok((self.schedules.get(group).cloned().unwrap_or_default(), None))
    }

    async fn scheduler_get(&self, group: &str, name: &str) -> Result<Schedule, AwsErr> {
        self.detail
            .get(&format!("{group}/{name}"))
            .cloned()
            .ok_or_else(|| denied("AccessDeniedException"))
    }

    async fn sfn_machines(&self, _next: Option<String>) -> Page<Machine> {
        Ok((self.machines.clone(), None))
    }

    async fn sfn_executions(&self, _arn: &str, max: i32) -> Page<Execution> {
        self.log.lock().unwrap().exec_limit = max;
        Ok((self.execs.clone(), (!self.exec_next.is_empty()).then(|| self.exec_next.clone())))
    }

    async fn s3_buckets(&self, _next: Option<String>) -> Page<(String, String)> {
        self.billed()?;
        Ok((self.buckets.clone(), None))
    }

    async fn s3_list(&self, bucket: &str, prefix: &str, region: Option<&str>) -> Result<Listing, AwsErr> {
        self.billed()?;
        self.region(region);
        Ok(self.listing.get(&format!("{bucket}|{prefix}")).cloned().unwrap_or_default())
    }

    async fn s3_location(&self, bucket: &str) -> Result<String, AwsErr> {
        self.billed()?;
        self.log.lock().unwrap().locations += 1;
        Ok(self.location.get(bucket).cloned().unwrap_or_default())
    }

    async fn s3_head(&self, _bucket: &str, key: &str, region: Option<&str>) -> Result<Head, AwsErr> {
        self.billed()?;
        self.region(region);
        Ok(self.heads.get(key).cloned().unwrap_or_default())
    }

    async fn s3_get_range(&self, _bucket: &str, key: &str, range: &str, region: Option<&str>) -> Result<Vec<u8>, AwsErr> {
        self.billed()?;
        self.region(region);
        self.log.lock().unwrap().ranges.push(range.into());
        Ok(self.bodies.get(key).cloned().unwrap_or_default())
    }
}

fn backend(f: Fake) -> Backend<Fake> {
    Backend::with_api(f, "")
}

/// SNS·SQS·S3 자리에 덫을 끼운 Backend 다. 요금 없는 sts·lambda 는 보통 가짜다.
fn free_backend(mut f: Fake) -> Backend<Fake> {
    f.trap = true;
    Backend::with_api(f, "ap-northeast-2")
}

fn names(ns: &[Node]) -> Vec<String> {
    ns.iter().map(|n| n.name.clone()).collect()
}

// ─── 공통 ───

#[tokio::test]
async fn 맨_위는_자격_증명을_확인한_뒤_폴더를_정한_순서로_보인다() {
    set_billed_for_test(true);
    let b = backend(Fake::default());
    let ns = b.children("").await.unwrap();
    assert_eq!(
        names(&ns),
        ["SNS 토픽", "SQS 큐", "Lambda", "EventBridge", "Scheduler", "Step Functions", "S3"]
    );
    assert!(ns[0].has_children && !ns[0].openable && ns[1].path == "sqs");
    let b = backend(Fake {
        sts_err: Some(denied("ExpiredToken")),
        ..Default::default()
    });
    assert_eq!(
        b.children("").await.unwrap_err().to_string(),
        "aws sts:GetCallerIdentity: ExpiredToken: not authorized"
    );
}

#[tokio::test]
async fn 권한_없는_폴더는_한_줄로_보이고_다른_폴더는_그대로다() {
    set_billed_for_test(true);
    let u = "https://sqs.ap-northeast-2.amazonaws.com/111122223333/a".to_string();
    let mut f = Fake {
        topic_err: Some(denied("AuthorizationError")),
        urls: vec![u.clone()],
        ..Default::default()
    };
    f.qattrs.insert(
        u,
        HashMap::from([
            (ATTR_VISIBLE.into(), "0".into()),
            (ATTR_IN_FLIGHT.into(), "0".into()),
            (ATTR_DELAYED.into(), "0".into()),
        ]),
    );
    let b = backend(f);
    let ns = b.children("sns").await.unwrap();
    assert_eq!(names(&ns), ["권한 없음: sns:ListTopics"]);
    assert!(!ns[0].has_children && !ns[0].openable);
    assert_eq!(names(&b.children("sqs").await.unwrap()), ["a"]);
    assert!(b.open(&ns[0].path).await.is_err(), "권한 없음 줄은 열 것이 없다");
    let b = backend(Fake {
        topic_err: Some(denied("Throttling")),
        ..Default::default()
    });
    assert!(
        b.children("sns")
            .await
            .unwrap_err()
            .to_string()
            .contains("sns:ListTopics: Throttling")
    );
}

#[test]
fn 서비스마다_다른_거절_코드를_알아본다() {
    for (code, want) in [
        ("AccessDenied", true),
        ("AccessDeniedException", true),
        ("AuthorizationError", true),
        ("AWS.SimpleQueueService.AccessDenied", true),
        ("UnauthorizedOperation", true),
        ("Throttling", false),
        ("ExpiredToken", false),
    ] {
        assert_eq!(denied_action(&op("x:Y")(denied(code))).is_some(), want, "{code}");
    }
}

#[tokio::test]
async fn run_은_늘_읽기_전용_거절이다() {
    let b = backend(Fake::default());
    let q = Query {
        text: "anything".into(),
        ..Default::default()
    };
    assert!(matches!(source::Backend::run(&b, &q).await, Err(Error::Write(_))));
}

// ─── SNS ───

const TOPIC: &str = "arn:aws:sns:ap-northeast-2:111122223333:orders";
const SUB_SAME: &str = "arn:aws:sns:ap-northeast-2:111122223333:orders:aaaa";
const SUB_OTHER: &str = "arn:aws:sns:ap-northeast-2:111122223333:orders:bbbb";

fn sns_fixture() -> Fake {
    let sub = |p: &str, e: &str, a: &str| Subscription {
        protocol: p.into(),
        endpoint: e.into(),
        arn: a.into(),
    };
    Fake {
        topics: vec![vec![TOPIC.into()], vec!["arn:aws:sns:ap-northeast-2:111122223333:alerts".into()]],
        subs: HashMap::from([(
            TOPIC.into(),
            vec![
                sub("sqs", "arn:aws:sqs:ap-northeast-2:111122223333:orders-worker", SUB_SAME),
                sub("sqs", "arn:aws:sqs:ap-northeast-2:999988887777:partner", SUB_OTHER),
                sub("email", "ops@example.com", "PendingConfirmation"),
            ],
        )]),
        sub_attrs: HashMap::from([
            (
                SUB_SAME.into(),
                HashMap::from([
                    ("RawMessageDelivery".into(), "true".into()),
                    ("FilterPolicy".into(), r#"{"type":["paid"]}"#.into()),
                ]),
            ),
            (SUB_OTHER.into(), HashMap::from([("RawMessageDelivery".into(), "false".into())])),
        ]),
        ..Default::default()
    }
}

#[tokio::test]
async fn SNS_토픽은_페이지를_잇고_구독은_같은_계정_SQS_만_큐_이름으로_줄인다() {
    set_billed_for_test(true);
    let b = backend(sns_fixture());
    let ns = b.children("sns").await.unwrap();
    assert_eq!(names(&ns), ["orders", "alerts"]);
    assert!(ns[0].path == format!("sns/{TOPIC}") && ns[0].has_children && ns[0].openable);
    let subs = b.children(&ns[0].path).await.unwrap();
    assert_eq!(
        names(&subs),
        [
            "orders-worker",
            "arn:aws:sqs:ap-northeast-2:999988887777:partner",
            "ops@example.com"
        ]
    );
    assert!(subs[0].info == "sqs" && !subs[0].has_children);
    assert!(b.children(&subs[0].path).await.unwrap().is_empty(), "구독은 잎");
}

#[tokio::test]
async fn SNS_토픽_Enter_는_구독_표이고_확인_전_구독은_속성을_묻지_않는다() {
    set_billed_for_test(true);
    let b = backend(sns_fixture());
    let res = b.open(&format!("sns/{TOPIC}")).await.unwrap();
    assert_eq!(res.cols, ["protocol", "endpoint", "subscription_arn", "raw", "filter"]);
    assert_eq!(
        res.rows,
        [
            vec!["sqs", "orders-worker", SUB_SAME, "true", r#"{"type":["paid"]}"#],
            vec!["sqs", "arn:aws:sqs:ap-northeast-2:999988887777:partner", SUB_OTHER, "false", ""],
            vec!["email", "ops@example.com", "PendingConfirmation", "", ""],
        ]
    );
    let mut calls = b.cl.log.lock().unwrap().attr_calls.clone();
    calls.sort();
    assert_eq!(calls, [SUB_SAME, SUB_OTHER]);
}

// ─── SQS ───

const QBASE: &str = "https://sqs.ap-northeast-2.amazonaws.com/111122223333/";

fn qattrs(name: &str, visible: &str, inflight: &str, delayed: &str, dlq: &str) -> HashMap<String, String> {
    let mut a = HashMap::from([
        (ATTR_ARN.to_string(), format!("arn:aws:sqs:ap-northeast-2:111122223333:{name}")),
        (ATTR_VISIBLE.into(), visible.into()),
        (ATTR_IN_FLIGHT.into(), inflight.into()),
        (ATTR_DELAYED.into(), delayed.into()),
    ]);
    if !dlq.is_empty() {
        a.insert(
            ATTR_REDRIVE.into(),
            format!(r#"{{"deadLetterTargetArn":"{dlq}","maxReceiveCount":5}}"#),
        );
    }
    a
}

#[tokio::test]
async fn SQS_큐_정보는_대기_처리_중_지연이고_DLQ_에_쌓이면_경고다() {
    set_billed_for_test(true);
    let q = |n: &str| format!("{QBASE}{n}");
    let mut f = Fake {
        urls: ["orders", "orders-dlq", "calm", "x-acct", "broken"].map(q).to_vec(),
        ..Default::default()
    };
    f.qattrs.insert(
        q("orders"),
        qattrs("orders", "12", "3", "0", "arn:aws:sqs:ap-northeast-2:111122223333:orders-dlq"),
    );
    f.qattrs.insert(q("orders-dlq"), qattrs("orders-dlq", "2", "0", "0", ""));
    f.qattrs.insert(
        q("calm"),
        qattrs("calm", "0", "0", "1", "arn:aws:sqs:ap-northeast-2:111122223333:calm-dlq"),
    );
    f.qattrs.insert(
        q("x-acct"),
        qattrs("x-acct", "0", "0", "0", "arn:aws:sqs:ap-northeast-2:999988887777:shared-dlq"),
    );
    f.qattr_err.insert(q("broken"), denied("AccessDenied"));
    f.by_name.insert("calm-dlq".into(), q("calm-dlq"));
    f.qattrs.insert(q("calm-dlq"), HashMap::from([(ATTR_VISIBLE.into(), "0".into())]));
    let b = backend(f);
    let ns = b.children("sqs").await.unwrap();
    let want = [
        ("대기 12 · 처리 중 3 · 지연 0 · DLQ orders-dlq 2", true),
        ("대기 2 · 처리 중 0 · 지연 0", false),
        ("대기 0 · 처리 중 0 · 지연 1 · DLQ calm-dlq 0", false),
        ("대기 0 · 처리 중 0 · 지연 0 · DLQ shared-dlq ?", false),
        ("권한 없음: sqs:GetQueueAttributes", false),
    ];
    for (n, (info, warn)) in ns.iter().zip(want) {
        assert_eq!((n.info.as_str(), n.warn), (info, warn), "{}", n.name);
        assert!(n.path == format!("sqs/{}", n.name) && n.openable);
    }
    let log = b.cl.log.lock().unwrap();
    let mut urls = log.url_calls.clone();
    urls.sort();
    assert_eq!(urls, ["calm-dlq@111122223333", "shared-dlq@999988887777"]);
    assert_eq!(log.attr_names[0], ["All"]);
}

#[tokio::test]
async fn SQS_큐_Enter_는_QueueUrl_먼저_속성_이름순이고_JSON_은_원문이다() {
    set_billed_for_test(true);
    let u = format!("{QBASE}orders");
    let redrive = r#"{"deadLetterTargetArn":"arn:aws:sqs:ap-northeast-2:111122223333:orders-dlq","maxReceiveCount":5}"#;
    let mut f = Fake::default();
    f.by_name.insert("orders".into(), u.clone());
    f.qattrs.insert(
        u.clone(),
        HashMap::from([
            ("VisibilityTimeout".into(), "30".into()),
            (ATTR_REDRIVE.into(), redrive.into()),
            (ATTR_VISIBLE.into(), "12".into()),
        ]),
    );
    let res = backend(f).open("sqs/orders").await.unwrap();
    assert_eq!(res.cols, ["attribute", "value"]);
    assert_eq!(
        res.rows,
        [
            vec!["QueueUrl", u.as_str()],
            vec![ATTR_VISIBLE, "12"],
            vec![ATTR_REDRIVE, redrive],
            vec!["VisibilityTimeout", "30"]
        ]
    );
}

#[tokio::test]
async fn SQS_ARN_으로_열면_그_계정의_큐를_열고_다른_리전은_안내만_둔다() {
    set_billed_for_test(true);
    let u = "https://sqs.ap-northeast-2.amazonaws.com/999988887777/shared-dlq";
    let mut f = Fake::default();
    f.by_name.insert("shared-dlq".into(), u.into());
    f.qattrs.insert(u.into(), HashMap::from([(ATTR_VISIBLE.into(), "3".into())]));
    let b = Backend::with_api(f, "ap-northeast-2");
    let res = b.open("sqs/arn:aws:sqs:ap-northeast-2:999988887777:shared-dlq").await.unwrap();
    assert_eq!(b.cl.log.lock().unwrap().url_calls, ["shared-dlq@999988887777"]);
    assert_eq!(res.rows[0][1], u);
    let res = b.open("sqs/arn:aws:sqs:us-east-1:999988887777:shared-dlq").await.unwrap();
    assert_eq!(b.cl.log.lock().unwrap().url_calls.len(), 1);
    assert_eq!(res.rows[1], ["note", "다른 리전 큐는 수만 보임"]);
}

// ─── Lambda ───

fn mapping(src: &str, state: &str, batch: Option<i32>, last: &str) -> SourceMapping {
    SourceMapping {
        source_arn: src.into(),
        state: state.into(),
        batch,
        last_result: last.into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn Lambda_함수_정보는_런타임과_KST_수정_시각이고_Enter_는_이벤트_소스_매핑이다() {
    let f = Fake {
        fns: vec![
            Function {
                name: "worker".into(),
                runtime: "nodejs20.x".into(),
                last_modified: "2026-11-01T03:04:05.000+0000".into(),
                ..Default::default()
            },
            Function {
                name: "img".into(),
                package_type: "Image".into(),
                last_modified: "bad".into(),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    f.mappings.lock().unwrap().insert(
        "worker".into(),
        vec![
            mapping("arn:aws:sqs:ap-northeast-2:111122223333:orders", "Enabled", Some(10), "OK"),
            mapping("arn:aws:kinesis:ap-northeast-2:111122223333:stream/s", "Disabled", None, ""),
        ],
    );
    let b = backend(f);
    let ns = b.children("lambda").await.unwrap();
    assert_eq!(ns[0].info, "nodejs20.x · 2026-11-01 12:04");
    assert_eq!(ns[1].info, "Image · bad");
    assert!(ns[0].path == "lambda/worker" && ns[0].openable && !ns[0].has_children);
    let res = b.open("lambda/worker").await.unwrap();
    assert_eq!(res.cols, ["source", "state", "batch", "last_result"]);
    assert_eq!(
        res.rows,
        [
            vec!["arn:aws:sqs:ap-northeast-2:111122223333:orders", "Enabled", "10", "OK"],
            vec!["arn:aws:kinesis:ap-northeast-2:111122223333:stream/s", "Disabled", "", ""]
        ]
    );
}

// ─── 대시보드 ───

#[tokio::test]
async fn 확인은_Lambda_수와_꺼진_매핑_실패한_매핑을_세고_요금_서비스는_부르지_않는다() {
    set_billed_for_test(true);
    let f = Fake {
        fns: ["a", "b", "c"]
            .map(|n| Function {
                name: n.into(),
                ..Default::default()
            })
            .to_vec(),
        ..Default::default()
    };
    f.mappings.lock().unwrap().extend([
        (
            "a".to_string(),
            vec![
                mapping("", "Enabled", None, "OK"),
                mapping("", "Enabled", None, "No records processed"),
                mapping("", "Disabled", None, "OK"),
            ],
        ),
        ("b".to_string(), vec![mapping("", "Enabled", None, "PROBLEM: Function call failed")]),
    ]);
    let b = free_backend(f);
    let h = b.probe().await;
    assert!(h.ok);
    assert_eq!(
        h.metrics,
        [
            Metric::new("Lambda", "3", false),
            Metric::new("매핑 꺼짐", "1", true),
            Metric::new("실패", "1", true)
        ]
    );
    assert_eq!(b.cl.billed_calls.load(Ordering::SeqCst), 0);
    *b.cl.mappings.lock().unwrap() = HashMap::from([("a".to_string(), vec![mapping("", "Enabled", None, "OK")])]);
    let h = b.probe().await;
    assert_eq!(
        h.metrics,
        [
            Metric::new("Lambda", "3", false),
            Metric::new("매핑 꺼짐", "0", false),
            Metric::new("실패", "0", false)
        ]
    );
}

#[tokio::test]
async fn 확인의_목록_실패는_실패다() {
    let b = free_backend(Fake {
        fn_err: Some(denied("AccessDeniedException")),
        ..Default::default()
    });
    let h = b.probe().await;
    assert!(!h.ok);
    assert_eq!(h.err, "aws lambda:ListFunctions: AccessDeniedException: not authorized");
    let b = free_backend(Fake {
        map_err: Some(denied("AccessDeniedException")),
        ..Default::default()
    });
    let h = b.probe().await;
    assert!(!h.ok && h.err.contains("lambda:ListEventSourceMappings"), "{}", h.err);
}

// ─── 요금 막기 (R72) ───

#[tokio::test]
async fn 요금이_막혀_있으면_SNS_SQS_S3_폴더가_없고_경로로_와도_보내지_않는다() {
    set_billed_for_test(false);
    let b = free_backend(Fake::default());
    assert_eq!(
        names(&b.children("").await.unwrap()),
        ["Lambda", "EventBridge", "Scheduler", "Step Functions"]
    );
    for p in ["sns", &format!("sns/{TOPIC}"), "sqs", "s3", "s3/logs", "s3/logs/app/"] {
        let err = b.children(p).await.unwrap_err();
        assert_eq!(err.to_string(), "요금이 붙는 조회라 막음 (DB_ALLOW_BILLED=1 로 허용)", "{p}");
    }
    for p in [
        &format!("sns/{TOPIC}"),
        "sqs/orders",
        "sqs/arn:aws:sqs:ap-northeast-2:111122223333:orders-dlq",
        "s3/logs/a.txt",
    ] {
        assert_eq!(b.open(p).await.unwrap_err(), Error::Billed, "{p}");
    }
    assert_eq!(b.cl.billed_calls.load(Ordering::SeqCst), 0);
    set_billed_for_test(true);
    assert_eq!(b.children("").await.unwrap().len(), 7);
}

#[test]
fn 짧은_시각은_KST_분까지이고_없으면_빈_글이다() {
    let t = Utc.with_ymd_and_hms(2026, 11, 16, 15, 30, 59).unwrap();
    assert_eq!(short_time(Some(t)), "2026-11-17 00:30");
    assert_eq!(short_time(None), "");
}

// ─── EventBridge ───

#[tokio::test]
async fn EventBridge_버스_규칙_대상을_보이고_파트너_버스의_슬래시를_견딘다() {
    let partner = "aws.partner/saas.com/123/feed";
    let rule = |n: &str, s: &str, st: &str| Rule {
        name: n.into(),
        schedule: s.into(),
        state: st.into(),
    };
    let f = Fake {
        buses: vec![vec!["default".into()], vec![partner.into()]],
        rules: HashMap::from([
            (
                "default".into(),
                vec![rule("nightly", "cron(0 18 * * ? *)", "ENABLED"), rule("on-order", "", "DISABLED")],
            ),
            (partner.into(), vec![rule("p", "", "ENABLED")]),
        ]),
        targets: HashMap::from([
            (
                "default|on-order".into(),
                vec![
                    Target {
                        id: "1".into(),
                        arn: "arn:aws:sqs:ap-northeast-2:111122223333:orders".into(),
                        input: r#"{"a":1}"#.into(),
                        dlq: Some("arn:aws:sqs:ap-northeast-2:111122223333:eb-dlq".into()),
                        ..Default::default()
                    },
                    Target {
                        id: "2".into(),
                        arn: "arn:aws:lambda:ap-northeast-2:111122223333:function:w".into(),
                        input_path: Some("$.detail".into()),
                        ..Default::default()
                    },
                    Target {
                        id: "3".into(),
                        arn: "arn:aws:sns:ap-northeast-2:111122223333:t".into(),
                        input_template: Some(r#""<id>""#.into()),
                        ..Default::default()
                    },
                ],
            ),
            (
                format!("{partner}|p"),
                vec![Target {
                    id: "x".into(),
                    arn: "arn:x".into(),
                    ..Default::default()
                }],
            ),
        ]),
        ..Default::default()
    };
    let b = backend(f);
    let buses = b.children("events").await.unwrap();
    assert_eq!(names(&buses), ["default", partner]);
    assert!(buses[0].has_children);
    let rules = b.children(&buses[0].path).await.unwrap();
    assert_eq!(rules[0].info, "cron(0 18 * * ? *) · ENABLED");
    assert_eq!(rules[1].info, "pattern · DISABLED");
    assert!(rules[1].openable && !rules[1].has_children);
    let res = b.open(&rules[1].path).await.unwrap();
    assert_eq!(res.cols, ["target_id", "arn", "input", "dlq"]);
    assert_eq!(
        res.rows,
        [
            vec![
                "1",
                "arn:aws:sqs:ap-northeast-2:111122223333:orders",
                r#"{"a":1}"#,
                "arn:aws:sqs:ap-northeast-2:111122223333:eb-dlq"
            ],
            vec!["2", "arn:aws:lambda:ap-northeast-2:111122223333:function:w", "$.detail", ""],
            vec!["3", "arn:aws:sns:ap-northeast-2:111122223333:t", r#""<id>""#, ""],
        ]
    );
    let prules = b.children(&buses[1].path).await.unwrap();
    assert_eq!(b.open(&prules[0].path).await.unwrap().rows.len(), 1);
    assert_eq!(b.cl.log.lock().unwrap().targets_asked.last().unwrap(), &format!("{partner}|p"));
}

// ─── Scheduler ───

#[tokio::test]
async fn Scheduler_그룹_일정에_식과_상태를_보이고_Enter_는_대상이다() {
    let f = Fake {
        groups: vec!["default".into()],
        schedules: HashMap::from([("default".into(), vec!["daily".into(), "secret".into()])]),
        detail: HashMap::from([(
            "default/daily".into(),
            Schedule {
                expression: "rate(1 day)".into(),
                timezone: "Asia/Seoul".into(),
                state: "ENABLED".into(),
                target: Some(ScheduleTarget {
                    arn: "arn:aws:lambda:ap-northeast-2:111122223333:function:w".into(),
                    input: "{}".into(),
                    role_arn: "arn:role".into(),
                    dlq: None,
                }),
            },
        )]),
        ..Default::default()
    };
    let b = backend(f);
    let groups = b.children("scheduler").await.unwrap();
    assert_eq!(names(&groups), ["default"]);
    let ss = b.children(&groups[0].path).await.unwrap();
    assert_eq!(ss[0].info, "rate(1 day) · ENABLED");
    assert_eq!(ss[1].info, "권한 없음: scheduler:GetSchedule");
    assert_eq!(ss[0].path, "scheduler/default/daily");
    let res = b.open(&ss[0].path).await.unwrap();
    assert_eq!(
        res.rows,
        [
            vec!["expression", "rate(1 day)"],
            vec!["timezone", "Asia/Seoul"],
            vec!["state", "ENABLED"],
            vec!["target_arn", "arn:aws:lambda:ap-northeast-2:111122223333:function:w"],
            vec!["input", "{}"],
            vec!["role_arn", "arn:role"],
        ]
    );
}

// ─── Step Functions ───

#[tokio::test]
async fn 상태_기계_Enter_는_최근_50개_실행이고_더_있으면_잘림이다() {
    let start = Utc.with_ymd_and_hms(2026, 11, 16, 1, 2, 3).unwrap();
    let f = Fake {
        machines: vec![Machine {
            name: "order-flow".into(),
            arn: "arn:aws:states:ap-northeast-2:111122223333:stateMachine:order-flow".into(),
            kind: "STANDARD".into(),
            created: Some(start),
        }],
        execs: vec![
            Execution {
                name: "run-2".into(),
                status: "RUNNING".into(),
                start: Some(start),
                stop: None,
            },
            Execution {
                name: "run-1".into(),
                status: "FAILED".into(),
                start: Some(start - chrono::TimeDelta::hours(1)),
                stop: Some(start - chrono::TimeDelta::minutes(59)),
            },
        ],
        exec_next: "more".into(),
        ..Default::default()
    };
    let b = backend(f);
    let ms = b.children("sfn").await.unwrap();
    assert_eq!(ms[0].info, "STANDARD · 2026-11-16 10:02");
    assert!(ms[0].openable);
    let res = b.open(&ms[0].path).await.unwrap();
    assert_eq!(res.cols, ["name", "status", "start", "stop"]);
    assert_eq!(
        res.rows,
        [
            vec!["run-2", "RUNNING", "2026-11-16 10:02:03", ""],
            vec!["run-1", "FAILED", "2026-11-16 09:02:03", "2026-11-16 09:03:03"]
        ]
    );
    assert!(res.truncated);
    assert_eq!(b.cl.log.lock().unwrap().exec_limit, 50);
}

// ─── S3 ───

fn s3_fixture() -> Fake {
    let modified = Some(Utc.with_ymd_and_hms(2026, 11, 1, 0, 0, 0).unwrap());
    let obj = |k: &str, size: i64| Object {
        key: k.into(),
        size,
        modified,
    };
    let head = |size: i64, ct: &str| Head {
        size,
        content_type: ct.into(),
        ..Default::default()
    };
    Fake {
        buckets: vec![("logs".into(), "us-west-2".into()), ("old".into(), String::new())],
        listing: HashMap::from([
            (
                "logs|".into(),
                Listing {
                    prefixes: vec!["2026/".into()],
                    objects: vec![obj("README.txt", 1536)],
                    truncated: false,
                },
            ),
            (
                "logs|2026/".into(),
                Listing {
                    prefixes: vec![],
                    objects: vec![obj("2026/", 0), obj("2026/a.json", 10)],
                    truncated: true,
                },
            ),
            ("old|".into(), Listing::default()),
        ]),
        location: HashMap::from([("old".into(), "EU".into())]),
        heads: HashMap::from([
            (
                "2026/a.json".into(),
                Head {
                    size: 10,
                    content_type: "application/json".into(),
                    etag: r#""e""#.into(),
                    modified,
                    metadata: HashMap::from([("owner".into(), "ops".into())]),
                    ..Default::default()
                },
            ),
            ("big.log".into(), head(S3_PEEK + 1, "text/plain")),
            ("img.png".into(), head(10, "image/png")),
            ("bin.txt".into(), head(3, "")),
            (
                "gz.json".into(),
                Head {
                    size: 3,
                    content_type: "application/json".into(),
                    content_encoding: "gzip".into(),
                    ..Default::default()
                },
            ),
        ]),
        bodies: HashMap::from([
            ("2026/a.json".into(), br#"{"ok":true}"#.to_vec()),
            ("bin.txt".into(), b"\xff\xfe\x00".to_vec()),
        ]),
        ..Default::default()
    }
}

#[tokio::test]
async fn S3_버킷_접두_객체를_한_단계씩_보이고_버킷_리전으로_묻는다() {
    set_billed_for_test(true);
    let b = backend(s3_fixture());
    let bs = b.children("s3").await.unwrap();
    assert_eq!(names(&bs), ["logs", "old"]);
    assert!(bs[0].info == "us-west-2" && bs[0].has_children);
    let top = b.children(&bs[0].path).await.unwrap();
    assert_eq!(names(&top), ["2026/", "README.txt"]);
    assert!(top[0].path == "s3/logs/2026/" && top[0].has_children);
    assert_eq!(top[1].info, "1.5KB · 2026-11-01 09:00");
    assert!(top[1].openable);
    let sub = b.children(&top[0].path).await.unwrap();
    assert_eq!(names(&sub), ["a.json", "… 앞 1000개만 보임"]);
    assert!(!sub[1].openable && !sub[1].has_children);
    assert!(b.children(&sub[0].path).await.unwrap().is_empty(), "객체는 잎이다");
    b.children(&bs[1].path).await.unwrap();
    let log = b.cl.log.lock().unwrap();
    assert_eq!(log.regions, ["us-west-2", "us-west-2", "eu-west-1"]);
    assert_eq!(log.locations, 1);
}

#[tokio::test]
async fn S3_객체_Enter_는_메타데이터이고_64KiB_이하_텍스트만_Range_로_본문을_붙인다() {
    set_billed_for_test(true);
    let b = backend(s3_fixture());
    b.regions.lock().unwrap().insert("logs".into(), "us-west-2".into());
    let res = b.open("s3/logs/2026/a.json").await.unwrap();
    let got: HashMap<&str, &str> = res.rows.iter().map(|r| (r[0].as_str(), r[1].as_str())).collect();
    assert_eq!(got["key"], "2026/a.json");
    assert_eq!(got["size"], "10");
    assert_eq!(got["content_type"], "application/json");
    assert_eq!(got["x-amz-meta-owner"], "ops");
    assert_eq!(got["body"], r#"{"ok":true}"#);
    assert_eq!(got["last_modified"], "2026-11-01 09:00:00");
    assert_eq!(b.cl.log.lock().unwrap().ranges, ["bytes=0-65535"]);
    for (key, body) in [
        ("big.log", "(64KiB 를 넘어 받지 않음)"),
        ("img.png", ""),
        ("gz.json", ""),
        ("bin.txt", "(텍스트가 아님)"),
    ] {
        let res = b.open(&format!("s3/logs/{key}")).await.unwrap();
        let last = res.rows.last().unwrap();
        let got = if last[0] == "body" { last[1].as_str() } else { "" };
        assert_eq!(got, body, "{key}");
    }
    assert_eq!(
        b.cl.log.lock().unwrap().ranges.len(),
        2,
        "큰 객체·이미지·압축 객체는 GetObject 를 부르지 않는다"
    );
    assert!(b.open("s3/logs/2026/").await.is_err(), "접두는 열 것이 없다");
}

#[test]
fn Content_Type_과_확장자로_텍스트를_가른다() {
    for (ct, key, want) in [
        ("text/plain; charset=utf-8", "a.bin", true),
        ("application/json", "a", true),
        ("application/vnd.api+json", "a", true),
        ("", "a.CSV", true),
        ("application/octet-stream", "a.log", true),
        ("application/octet-stream", "a.gz", false),
        ("image/png", "a.txt", false),
    ] {
        assert_eq!(text_like(ct, key), want, "{ct} {key}");
    }
}

// ─── 이벤트 흐름 (R69) ───

const WORKER: &str = "arn:aws:sqs:ap-northeast-2:111122223333:orders-worker";
const DLQ: &str = "arn:aws:sqs:ap-northeast-2:111122223333:orders-worker-dlq";
const OTHER: &str = "arn:aws:sqs:ap-northeast-2:999988887777:partner";

fn flow_fixture() -> Fake {
    let f = Fake::default();
    f.mappings.lock().unwrap().extend([
        (
            "worker".to_string(),
            vec![SourceMapping {
                source_arn: WORKER.into(),
                function_arn: "arn:aws:lambda:ap-northeast-2:111122223333:function:worker".into(),
                state: "Enabled".into(),
                last_result: "OK".into(),
                batch: None,
            }],
        ),
        (
            "other".to_string(),
            vec![SourceMapping {
                source_arn: "arn:aws:sqs:ap-northeast-2:111122223333:else".into(),
                function_arn: "arn:aws:lambda:x:1:function:other".into(),
                ..Default::default()
            }],
        ),
    ]);
    f
}

fn qref(name: &str, dlq: &str) -> QueueRef {
    QueueRef {
        name: name.into(),
        dlq: dlq.into(),
    }
}

#[tokio::test]
async fn 흐름_큐_이름은_계정과_리전으로_ARN_을_만들고_Lambda_매핑만_묻는다() {
    for allow in [false, true] {
        set_billed_for_test(allow);
        let b = free_backend(flow_fixture());
        let got = b
            .queue_flows(&[qref("orders-worker", "orders-worker-dlq"), qref(OTHER, "")])
            .await
            .unwrap();
        let want = [
            Queue {
                name: "orders-worker".into(),
                arn: WORKER.into(),
                dlq: "orders-worker-dlq".into(),
                dlq_arn: DLQ.into(),
                lambdas: vec![Mapping {
                    function: "worker".into(),
                    state: "Enabled".into(),
                    last_result: "OK".into(),
                }],
                err: String::new(),
            },
            Queue {
                name: "partner".into(),
                arn: OTHER.into(),
                ..Default::default()
            },
        ];
        assert_eq!(got, want);
        let mut sources = b.cl.log.lock().unwrap().sources.clone();
        sources.sort();
        assert_eq!(sources, [WORKER, OTHER]);
        assert_eq!(b.cl.billed_calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn 흐름_DLQ_이름은_큐의_계정을_따르고_ARN_이면_그대로다() {
    let b = free_backend(Fake::default());
    let got = b
        .queue_flows(&[qref(OTHER, "partner-dlq"), qref(OTHER, "arn:aws:sqs:us-east-1:555:shared")])
        .await
        .unwrap();
    assert_eq!(
        (got[0].dlq_arn.as_str(), got[0].dlq.as_str()),
        ("arn:aws:sqs:ap-northeast-2:999988887777:partner-dlq", "partner-dlq")
    );
    assert_eq!(
        (got[1].dlq_arn.as_str(), got[1].dlq.as_str()),
        ("arn:aws:sqs:us-east-1:555:shared", "shared")
    );
}

#[tokio::test]
async fn 흐름_ARN_만이면_계정을_묻지_않고_계정을_못_읽으면_오류다() {
    let b = free_backend(Fake {
        sts_err: Some(denied("ExpiredToken")),
        ..Default::default()
    });
    b.queue_flows(&[qref(OTHER, "")]).await.unwrap();
    assert_eq!(b.cl.sts_calls.load(Ordering::SeqCst), 0);
    let err = b.queue_flows(&[qref("orders-worker", "")]).await.unwrap_err();
    assert_eq!(err.to_string(), "aws sts:GetCallerIdentity: ExpiredToken: not authorized");
    let b = Backend::with_api(Fake::default(), "");
    assert!(
        b.queue_flows(&[qref("orders-worker", "")])
            .await
            .unwrap_err()
            .to_string()
            .contains("no region")
    );
}

#[tokio::test]
async fn 흐름_매핑을_못_읽으면_그_큐만_오류다() {
    let b = free_backend(Fake {
        map_err: Some(denied("AccessDeniedException")),
        ..Default::default()
    });
    let got = b.queue_flows(&[qref("orders-worker", "orders-worker-dlq")]).await.unwrap();
    assert!(got[0].err.contains("lambda:ListEventSourceMappings"), "{}", got[0].err);
    assert_eq!((got[0].name.as_str(), got[0].dlq.as_str()), ("orders-worker", "orders-worker-dlq"));
}

// ─── 읽기만 (R67.1) ───

/// sdk.rs 의 클라이언트 호출(self.<서비스>.<작업>())을 모두 찾아 작업 이름을 돌려준다.
fn sdk_calls(src: &str) -> Vec<String> {
    // rustfmt 가 self·.서비스·.작업을 줄마다 나눠 쓰므로 공백을 모두 지운 글에서 찾는다
    let flat: String = src.chars().filter(|c| !c.is_whitespace()).collect();
    let mut out = Vec::new();
    for svc in ["sts", "sns", "sqs", "lambda", "events", "scheduler", "sfn", "s3"] {
        let pat = format!("self.{svc}.");
        let mut rest = flat.as_str();
        while let Some(i) = rest.find(&pat) {
            rest = &rest[i + pat.len()..];
            out.push(rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect());
        }
    }
    out
}

#[test]
fn 트리는_AWS_에_읽기_작업만_부른다() {
    let src = include_str!("sdk.rs");
    let calls = sdk_calls(src);
    assert!(calls.len() >= 22, "호출을 다 찾지 못했다: {calls:?}");
    for c in &calls {
        assert!(
            c.starts_with("list_") || c.starts_with("get_") || c.starts_with("head_"),
            "읽기가 아닌 작업 호출: {c}"
        );
    }
    // 검사가 쓰기 호출을 실제로 잡는지 본다
    let bad = sdk_calls("self.sqs.send_message() self.sns\n .publish() self.s3.put_object()");
    assert_eq!(bad, ["publish", "send_message", "put_object"]);
}
