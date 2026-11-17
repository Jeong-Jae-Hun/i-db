//! 흐름(R69) 풀이. Go `flow.go` 의 resolveFlow·flowBoxes 다. `db flows`·보드 흐름 패널·화면이 같이 쓴다.
//! 아웃박스 수는 READ ONLY 로 세고, 큐는 aws 접속으로 Lambda 매핑만 찾는다(SNS·SQS 는 요금이 붙어 묻지 않는다, R72).

use std::sync::Arc;
use std::time::Duration;

use idb_core::flows::{self, Flow, MAX_TYPES, Stat, TypeStat};
use idb_core::project::Conn;
use idb_core::redact::redact;
use idb_core::textutil::one_line;
use idb_sources::awsres::{self, Queue, QueueRef, mapping_ok};
use idb_sources::health::{PROBE_OPEN_TIMEOUT, PROBE_TIMEOUT};
use idb_sources::{Clients, Error, join_indexed};
use serde::{Deserialize, Serialize};
use tokio::time::Instant;

/// 확인하지 않은 prod 접속(R64.3)이다. CLI 는 --prod, 화면은 prod 를 켜야 묻는다
pub const DASH_PROD_HINT: &str = "--prod 로 확인";
/// 흐름 하나의 큐를 푸는 시간이다. 계정·큐마다 매핑을 물어 질의 한 번보다 길게 준다
const FLOW_AWS_TIMEOUT: Duration = Duration::from_secs(15);
/// 아웃박스 종류 한 줄(R69.6)이다
pub const STAGE_TYPE: &str = "outbox-type";

/// fut 를 d 와 deadline 가운데 이른 쪽까지 기다린다.
async fn within<T>(d: Duration, deadline: Instant, fut: impl Future<Output = Result<T, Error>>) -> Result<T, Error> {
    let at = (Instant::now() + d).min(deadline);
    tokio::time::timeout_at(at, fut).await.unwrap_or(Err(Error::Deadline))
}

/// 대시보드 확인처럼 여는 데 PROBE_OPEN_TIMEOUT, 질의에 PROBE_TIMEOUT 을 걸고 READ ONLY 로 센다.
async fn outbox_count(clients: &Clients, c: &Conn, o: &flows::Outbox, deadline: Instant) -> Result<Stat, Error> {
    let pg = within(PROBE_OPEN_TIMEOUT, deadline, clients.pg(c)).await?;
    // 합계 행 하나를 더 받는다
    let res = within(PROBE_TIMEOUT, deadline, pg.run_read_only_rows(&flows::count_sql(o), MAX_TYPES + 1)).await?;
    flows::parse_count(&res).map_err(Error::msg)
}

async fn queue_flows(c: &Conn, refs: &[QueueRef], deadline: Instant) -> Result<Vec<Queue>, Error> {
    let b = within(PROBE_OPEN_TIMEOUT, deadline, awsres::Backend::new(c)).await?;
    within(FLOW_AWS_TIMEOUT, deadline, b.queue_flows(refs)).await
}

/// 흐름 하나를 한 번 푼 결과다. queues 는 설정의 큐와 같은 순서이고, aws_err·aws_note 는 큐 전체에 붙는다.
#[derive(Default)]
pub struct FlowSnap {
    outbox: Option<Stat>,
    outbox_err: String,
    outbox_note: String,
    queues: Vec<Queue>,
    aws_err: String,
    aws_note: String,
}

/// 아웃박스 수와 큐의 Lambda 매핑을 동시에 푼다. 토픽은 이름만 보여 묻지 않는다.
/// prod 가 거짓이면 prod 접속은 묻지 않는다(R58.2). 오류는 비밀번호를 지운 한 줄이다.
pub async fn resolve_flow(clients: &Clients, conns: &[Conn], f: &Flow, prod: bool, deadline: Instant) -> FlowSnap {
    let by_name = |n: &str| conns.iter().find(|c| c.name == n);
    let mut s = FlowSnap::default();
    if !f.err.is_empty() {
        return s;
    }
    let outbox = async {
        let Some(o) = &f.outbox else {
            return (None, String::new(), String::new());
        };
        match by_name(&o.conn) {
            None => (None, format!("no such connection: {}", o.conn), String::new()),
            Some(c) if c.env() == "prod" && !prod => (None, String::new(), DASH_PROD_HINT.to_string()),
            Some(c) => match outbox_count(clients, c, o, deadline).await {
                Ok(st) => (Some(st), String::new(), String::new()),
                Err(e) => (None, one_line(&redact(&e.to_string(), &c.password)), String::new()),
            },
        }
    };
    let queues = async {
        if f.queues.is_empty() {
            return (Vec::new(), String::new(), String::new());
        }
        match by_name(&f.aws) {
            None => (Vec::new(), format!("no such connection: {}", f.aws), String::new()),
            Some(c) if c.env() == "prod" && !prod => (Vec::new(), String::new(), DASH_PROD_HINT.to_string()),
            Some(c) => {
                let refs: Vec<QueueRef> = f
                    .queues
                    .iter()
                    .map(|q| QueueRef {
                        name: q.name.clone(),
                        dlq: q.dlq.clone(),
                    })
                    .collect();
                match queue_flows(c, &refs, deadline).await {
                    Ok(qs) => (qs, String::new(), String::new()),
                    Err(e) => (Vec::new(), one_line(&e.to_string()), String::new()),
                }
            }
        }
    };
    let ((ob, oe, on), (qs, ae, an)) = tokio::join!(outbox, queues);
    (s.outbox, s.outbox_err, s.outbox_note) = (ob, oe, on);
    (s.queues, s.aws_err, s.aws_note) = (qs, ae, an);
    s
}

// ─── 상자 ───

/// `db flows` 의 한 줄(상자 하나)이다. idle 은 설정에만 있고 행이 없는 종류다.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowBox {
    pub stage: String,
    pub name: String,
    pub metrics: String,
    pub warn: bool,
    pub idle: bool,
    pub err: String,
    pub note: String,
    /// 상자의 접속 이름(오류 글에서 비밀번호를 지울 때 쓴다)
    pub conn: String,
}

impl FlowBox {
    pub fn failed(&self) -> bool {
        !self.err.is_empty()
    }
}

/// ARN·이름의 마지막 : 뒤 마디다.
fn last_part(s: &str) -> &str {
    s.rsplit(':').next().unwrap_or(s)
}

fn first_non_empty(a: &str, b: &str) -> String {
    if a.is_empty() { b.to_string() } else { a.to_string() }
}

/// 흐름을 아웃박스 → SNS → 큐(→ DLQ·Lambda) → 아웃박스 종류 순서의 상자로 편다. SNS·큐·DLQ 는 설정의 이름만이고 수치가 없다(R72).
pub fn flow_boxes(f: &Flow, s: &FlowSnap) -> Vec<FlowBox> {
    if !f.err.is_empty() {
        return vec![FlowBox {
            stage: "config".into(),
            name: f.name.clone(),
            err: format!("설정 오류: {}", f.err),
            ..Default::default()
        }];
    }
    let mut out = Vec::new();
    if let Some(o) = &f.outbox {
        let mut b = FlowBox {
            stage: "outbox".into(),
            name: format!("{} {}", o.conn, o.table),
            err: s.outbox_err.clone(),
            note: s.outbox_note.clone(),
            conn: o.conn.clone(),
            ..Default::default()
        };
        if let Some(st) = &s.outbox {
            (b.metrics, b.warn) = st.metrics();
        }
        out.push(b);
    }
    if !f.topic.is_empty() {
        let name = if f.topic.starts_with("arn:") {
            last_part(&f.topic)
        } else {
            &f.topic
        };
        out.push(FlowBox {
            stage: "sns".into(),
            name: name.into(),
            conn: f.aws.clone(),
            ..Default::default()
        });
    }
    for (i, cq) in f.queues.iter().enumerate() {
        let q = s.queues.get(i).cloned().unwrap_or_else(|| Queue {
            name: last_part(&cq.name).into(),
            dlq: last_part(&cq.dlq).into(),
            ..Default::default()
        });
        out.push(FlowBox {
            stage: "sqs".into(),
            name: q.name.clone(),
            err: first_non_empty(&s.aws_err, &q.err),
            note: s.aws_note.clone(),
            conn: f.aws.clone(),
            ..Default::default()
        });
        if !q.dlq.is_empty() {
            out.push(FlowBox {
                stage: "dlq".into(),
                name: q.dlq.clone(),
                conn: f.aws.clone(),
                ..Default::default()
            });
        }
        for m in &q.lambdas {
            out.push(FlowBox {
                stage: "lambda".into(),
                name: m.function.clone(),
                metrics: format!("{} · {}", m.state, m.last_result),
                warn: m.state != "Enabled" || !mapping_ok(&m.last_result),
                conn: f.aws.clone(),
                ..Default::default()
            });
        }
    }
    let (Some(o), Some(st)) = (&f.outbox, &s.outbox) else { return out };
    // 종류 줄(R69.6)은 설정의 종류 ∪ 테이블에 있는 종류다. 한도 소진 많은 순 → 대기 많은 순 → 이름 순이고 행 없는 설정 종류는 맨 뒤다.
    // 종류가 잘렸으면 빠진 설정 종류에도 행이 있을 수 있어 0건으로 그리지 않는다
    let mut types = st.types.clone();
    if !st.truncated {
        for k in o.types.keys() {
            if !types.iter().any(|t| !t.null && &t.name == k) {
                types.push(TypeStat {
                    name: k.clone(),
                    retrying: st.retry.min(0),
                    exhausted: st.exhausted.min(0),
                    ..Default::default()
                });
            }
        }
    }
    types.sort_by(|a, b| {
        b.rows
            .min(1)
            .cmp(&a.rows.min(1))
            .then(b.exhausted.cmp(&a.exhausted))
            .then(b.waiting.cmp(&a.waiting))
            .then_with(|| a.label().cmp(&b.label()))
    });
    for t in types {
        let (m, warn) = t.metrics();
        let consumer = if t.null {
            "?".to_string()
        } else {
            o.types
                .get(&t.name)
                .map(|c| c.consumer.clone())
                .filter(|c| !c.is_empty())
                .unwrap_or_else(|| "?".into())
        };
        out.push(FlowBox {
            stage: STAGE_TYPE.into(),
            name: t.label(),
            metrics: format!("{m} · → {consumer}"),
            warn,
            idle: t.rows == 0,
            conn: o.conn.clone(),
            ..Default::default()
        });
    }
    out
}

/// 흐름들을 동시에 한 번 풀어 흐름마다 상자를 돌려준다(list 순서).
pub async fn resolve_flows(clients: &Arc<Clients>, conns: &[Conn], list: &[Flow], prod: bool, deadline: Instant) -> Vec<Vec<FlowBox>> {
    // 작업은 'static 이어야 해 접속 목록을 한 번 복사해 나눠 쓴다
    let conns = Arc::new(conns.to_vec());
    let jobs = list.iter().enumerate().map(|(i, f)| {
        let (clients, conns, f) = (clients.clone(), conns.clone(), f.clone());
        (i, async move { resolve_flow(&clients, &conns, &f, prod, deadline).await })
    });
    let mut snaps: Vec<FlowSnap> = list.iter().map(|_| FlowSnap::default()).collect();
    for (i, s) in join_indexed(jobs).await {
        snaps[i] = s.unwrap_or_else(|e| FlowSnap {
            outbox_err: e.to_string(),
            ..Default::default()
        });
    }
    list.iter().zip(&snaps).map(|(f, s)| flow_boxes(f, s)).collect()
}

#[cfg(test)]
mod tests;
