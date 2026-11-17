use std::collections::BTreeMap;

use idb_core::flows::{Outbox, TypeConf};
use idb_sources::awsres::Mapping;

use super::*;

fn hub_flow() -> Flow {
    Flow {
        name: "hub".into(),
        aws: "aws-stg".into(),
        topic: "stg-hub".into(),
        queues: vec![
            flows::Queue {
                name: "q1".into(),
                dlq: "q1-dlq".into(),
            },
            flows::Queue {
                name: "q2".into(),
                dlq: String::new(),
            },
        ],
        outbox: Some(Outbox {
            conn: "shop-stg".into(),
            table: "public.t_outbox".into(),
            kind: "event_type".into(),
            retry: "retry_count".into(),
            due: "check_needed_at".into(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn sample_queues() -> Vec<Queue> {
    vec![
        Queue {
            name: "q1".into(),
            arn: "arn:aws:sqs:ap-northeast-2:111:q1".into(),
            dlq: "q1-dlq".into(),
            dlq_arn: "arn:aws:sqs:ap-northeast-2:999:q1-dlq".into(),
            lambdas: vec![Mapping {
                function: "worker".into(),
                state: "Enabled".into(),
                last_result: "OK".into(),
            }],
            err: String::new(),
        },
        Queue {
            name: "q2".into(),
            arn: "arn:aws:sqs:ap-northeast-2:111:q2".into(),
            ..Default::default()
        },
    ]
}

fn lines(boxes: &[FlowBox]) -> Vec<String> {
    boxes
        .iter()
        .map(|b| {
            let mut w = if b.warn { "!".to_string() } else { String::new() };
            if b.idle {
                w.push_str("(idle)");
            }
            format!("{}:{}:{}{w}", b.stage, b.name, b.metrics)
        })
        .collect()
}

#[test]
fn 상자는_아웃박스_SNS_큐_DLQ_Lambda_순서이고_SNS_큐_DLQ_는_수치가_없다() {
    let snap = FlowSnap {
        outbox: Some(Stat {
            pending: 120,
            retry: 3,
            stale: 1,
            ..Default::default()
        }),
        queues: sample_queues(),
        ..Default::default()
    };
    assert_eq!(
        lines(&flow_boxes(&hub_flow(), &snap)),
        [
            "outbox:shop-stg public.t_outbox:대기 120 · 재시도 3 · 한도 소진 0 · 오래됨 1!",
            "sns:stg-hub:",
            "sqs:q1:",
            "dlq:q1-dlq:",
            "lambda:worker:Enabled · OK",
            "sqs:q2:",
        ]
    );
    let mut f = hub_flow();
    f.topic = "arn:aws:sns:ap-northeast-2:111:stg-hub".into();
    let b = &flow_boxes(&f, &FlowSnap::default())[1];
    assert_eq!((b.name.as_str(), b.conn.as_str()), ("stg-hub", "aws-stg"));
}

#[test]
fn 큐_상자는_결과_전에도_설정_순서대로_이름을_보이고_계정_오류는_큐마다_붙는다() {
    assert_eq!(
        lines(&flow_boxes(&hub_flow(), &FlowSnap::default())),
        [
            "outbox:shop-stg public.t_outbox:",
            "sns:stg-hub:",
            "sqs:q1:",
            "dlq:q1-dlq:",
            "sqs:q2:"
        ]
    );
    let bad = flow_boxes(
        &hub_flow(),
        &FlowSnap {
            aws_err: "aws sts:GetCallerIdentity: ExpiredToken".into(),
            ..Default::default()
        },
    );
    assert!(bad[2].failed() && bad[4].failed() && !bad[1].failed());
    assert_eq!(bad[2].err, "aws sts:GetCallerIdentity: ExpiredToken");
}

#[test]
fn Lambda_는_꺼졌거나_문제면_경고이고_처리할_것_없음은_아니다() {
    for (state, last, warn) in [
        ("Enabled", "OK", false),
        ("Enabled", "No records processed", false),
        ("Disabled", "OK", true),
        ("Enabled", "PROBLEM: Function call failed", true),
    ] {
        let snap = FlowSnap {
            queues: vec![Queue {
                name: "q".into(),
                lambdas: vec![Mapping {
                    function: "f".into(),
                    state: state.into(),
                    last_result: last.into(),
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let f = Flow {
            name: "x".into(),
            aws: "a".into(),
            queues: vec![flows::Queue {
                name: "q".into(),
                dlq: String::new(),
            }],
            ..Default::default()
        };
        let boxes = flow_boxes(&f, &snap);
        let b = boxes.last().unwrap();
        assert_eq!((b.stage.as_str(), b.warn), ("lambda", warn), "{state} {last}");
    }
}

#[test]
fn 설정_오류와_prod_미확인() {
    let bad = flow_boxes(
        &Flow {
            name: "bad".into(),
            err: "topic is empty".into(),
            ..Default::default()
        },
        &FlowSnap::default(),
    );
    assert_eq!(bad.len(), 1);
    assert_eq!(bad[0].err, "설정 오류: topic is empty");
    let prod = flow_boxes(
        &hub_flow(),
        &FlowSnap {
            outbox_note: DASH_PROD_HINT.into(),
            aws_note: DASH_PROD_HINT.into(),
            ..Default::default()
        },
    );
    assert!(prod[0].note == DASH_PROD_HINT && !prod[0].failed());
    assert_eq!(prod[2].note, DASH_PROD_HINT);
    assert!(prod[1].note.is_empty(), "SNS 는 묻지 않아 prod 안내가 없다");
}

fn type_flow() -> Flow {
    let tc = |limit: Option<i64>, consumer: &str| TypeConf {
        limit,
        consumer: consumer.into(),
    };
    Flow {
        name: "shop-outbox-stg".into(),
        outbox: Some(Outbox {
            conn: "shop-stg".into(),
            table: "public.t_outbox".into(),
            kind: "event_type".into(),
            retry: "retry_count".into(),
            due: "check_needed_at".into(),
            types: BTreeMap::from([
                ("EARN".to_string(), tc(Some(20), "consume-outbox-earn")),
                ("CLIP".to_string(), tc(None, "consume-outbox-clip")),
                ("IDLE".to_string(), tc(None, "consume-outbox-idle")),
            ]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn type_stat() -> Stat {
    let t = |name: &str, rows, waiting, retrying, exhausted, stale| TypeStat {
        name: name.into(),
        rows,
        waiting,
        retrying,
        exhausted,
        stale,
        ..Default::default()
    };
    Stat {
        pending: 9,
        retry: 2,
        stale: 1,
        exhausted: 3,
        types: vec![t("CLIP", 4, 4, 1, 0, 1), t("LONG_UNKNOWN", 2, 2, 0, 0, 0), t("EARN", 6, 3, 1, 3, 0)],
        truncated: false,
    }
}

#[test]
fn 아웃박스_종류_줄은_한도_소진_대기_이름_순이고_행_없는_설정_종류는_맨_뒤다() {
    let snap = FlowSnap {
        outbox: Some(type_stat()),
        ..Default::default()
    };
    assert_eq!(
        lines(&flow_boxes(&type_flow(), &snap)),
        [
            "outbox:shop-stg public.t_outbox:대기 9 · 재시도 2 · 한도 소진 3 · 오래됨 1!",
            "outbox-type:EARN:대기 3 · 재시도 1 · 한도 소진 3 · → consume-outbox-earn!",
            "outbox-type:CLIP:대기 4 · 재시도 1 · 한도 소진 0 · 오래됨 1 · → consume-outbox-clip!",
            "outbox-type:LONG_UNKNOWN:대기 2 · 재시도 0 · 한도 소진 0 · → ?",
            "outbox-type:IDLE:대기 0 · 재시도 0 · 한도 소진 0 · → consume-outbox-idle(idle)",
        ]
    );
}

#[test]
fn 종류가_잘리면_빠진_설정_종류를_0건으로_그리지_않는다() {
    let mut st = type_stat();
    st.truncated = true;
    let snap = FlowSnap {
        outbox: Some(st),
        ..Default::default()
    };
    assert!(flow_boxes(&type_flow(), &snap).iter().all(|b| b.name != "IDLE"));
}

#[test]
fn NULL_종류는_괄호_NULL_로_보이고_설정_소비자는_글자_NULL_종류에만_붙는다() {
    let st = Stat {
        pending: 3,
        retry: 2,
        stale: -1,
        exhausted: 0,
        types: vec![
            TypeStat {
                name: "NULL".into(),
                null: true,
                rows: 2,
                waiting: 2,
                retrying: 1,
                ..Default::default()
            },
            TypeStat {
                name: "NULL".into(),
                rows: 1,
                waiting: 1,
                retrying: 1,
                ..Default::default()
            },
        ],
        truncated: false,
    };
    let mut f = type_flow();
    f.outbox.as_mut().unwrap().types = BTreeMap::from([(
        "NULL".to_string(),
        TypeConf {
            limit: None,
            consumer: "consume-null".into(),
        },
    )]);
    let boxes = flow_boxes(
        &f,
        &FlowSnap {
            outbox: Some(st),
            ..Default::default()
        },
    );
    assert_eq!(boxes[1].name, "(NULL)");
    assert!(boxes[1].metrics.ends_with("→ ?"));
    assert_eq!(boxes[2].name, "NULL");
    assert!(boxes[2].metrics.ends_with("→ consume-null"));
}

#[test]
fn 상자_상세_길은_큐_차례와_아웃박스_문장으로_만든다() {
    let mut f = hub_flow();
    f.topic = "arn:aws:sns:ap-northeast-2:111:stg-hub".into();
    f.queues[0].dlq = "arn:aws:sqs:ap-northeast-2:111:q1-dlq".into();
    let b = |stage: &str, name: &str| FlowBox {
        stage: stage.into(),
        name: name.into(),
        ..Default::default()
    };
    let boxes = [
        b("outbox", "t_outbox"),
        b("sns", "stg-hub"),
        b("sqs", "q1"),
        b("dlq", "q1-dlq"),
        b("lambda", "fn1"),
        b("sqs", "q2"),
        b("outbox-type", "(NULL)"),
    ];
    let o = box_opens(&f, &boxes);
    assert_eq!(o.len(), boxes.len());
    assert_eq!(o[0].sql, flows::count_sql(f.outbox.as_ref().unwrap()));
    assert_eq!(o[1].path, "sns/arn:aws:sns:ap-northeast-2:111:stg-hub");
    assert_eq!(
        (o[2].path.as_str(), o[3].path.as_str()),
        ("sqs/q1", "sqs/arn:aws:sqs:ap-northeast-2:111:q1-dlq")
    );
    assert_eq!((o[4].path.as_str(), o[5].path.as_str()), ("lambda/fn1", "sqs/q2"));
    assert!(o[6].sql.contains("is null"), "{}", o[6].sql);
    // 이름만 준 토픽·이름 DLQ 는 열지 않는다
    let h = hub_flow();
    let o = box_opens(&h, &boxes[..4]);
    assert!(o[1].path.is_empty() && o[3].path.is_empty());
}
