//! AWS 메시징·인프라를 트리로 보는 읽기 전용 저장소(R67). 편집기 질의 언어는 없고 트리 펼치기(children)와 Enter(open)만 있다.
//! 부르는 API 는 List*·Get*·Head* 뿐이다(sdk.rs 를 읽는 시험이 지킨다). Go `internal/awsres` 다.
//! SNS·SQS·S3 는 요청마다 요금이 붙어 source::billed 를 지나야 부른다(R72). 대시보드·흐름은 늘 요금 없는 API 만 쓴다.

mod sdk;

use std::collections::HashMap;
use std::future::Future;
use std::sync::Mutex;
use std::time::Instant;

use chrono::{DateTime, FixedOffset, Utc};
use futures_util::stream::{self, StreamExt};
use idb_core::project::Conn;
use idb_core::result::QueryResult;
use idb_core::textutil::human_bytes;

use crate::Error;
use crate::aws::AwsErr;
use crate::health::{Health, Metric, failed};
use crate::source::{self, BoxFut, Node, Query};
pub use sdk::Sdk;

/// 목록 한 단계에서 동시에 부르는 Get* 수다. 큐·구독이 수백 개여도 API 한도를 넘지 않게 작게 둔다.
const PARALLEL: usize = 8;
/// 상태 기계 Enter 가 보이는 최근 실행 수다. ListExecutions 한 페이지(최신순)만 받는다.
const RECENT_EXECUTIONS: i32 = 50;
/// 한 단계(버킷 또는 접두)에서 보이는 접두·객체 수다. ListObjectsV2 한 페이지만 받는다
const S3_LEVEL: i32 = 1000;
/// 객체 Enter 가 본문을 보이는 상한이다. 이보다 큰 객체는 받지 않고, 받을 때도 Range 로 이만큼만 요청한다
const S3_PEEK: i64 = 64 << 10;

// ─── 서비스 값 ───

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Subscription {
    pub protocol: String,
    pub endpoint: String,
    pub arn: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Function {
    pub name: String,
    pub runtime: String,
    pub package_type: String,
    /// ISO 8601(예: 2026-11-01T03:04:05.000+0000)
    pub last_modified: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceMapping {
    pub source_arn: String,
    pub function_arn: String,
    pub state: String,
    pub last_result: String,
    pub batch: Option<i32>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rule {
    pub name: String,
    pub schedule: String,
    pub state: String,
}

/// EventBridge 대상이다. input_template 은 InputTransformer 가 있을 때만 Some 이다.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Target {
    pub id: String,
    pub arn: String,
    pub input: String,
    pub input_path: Option<String>,
    pub input_template: Option<String>,
    pub dlq: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Schedule {
    pub expression: String,
    pub timezone: String,
    pub state: String,
    pub target: Option<ScheduleTarget>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScheduleTarget {
    pub arn: String,
    pub input: String,
    pub role_arn: String,
    pub dlq: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Machine {
    pub name: String,
    pub arn: String,
    pub kind: String,
    pub created: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Execution {
    pub name: String,
    pub status: String,
    pub start: Option<DateTime<Utc>>,
    pub stop: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Object {
    pub key: String,
    pub size: i64,
    pub modified: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Listing {
    pub prefixes: Vec<String>,
    pub objects: Vec<Object>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Head {
    pub size: i64,
    pub modified: Option<DateTime<Utc>>,
    pub content_type: String,
    pub content_encoding: String,
    pub etag: String,
    pub storage_class: String,
    pub sse: String,
    pub version_id: String,
    pub metadata: HashMap<String, String>,
}

type Page<T> = Result<(Vec<T>, Option<String>), AwsErr>;

/// Backend 가 부르는 AWS 작업이다(서비스마다 읽기만). 페이지는 (항목, 다음 토큰)이다. 시험은 가짜로 바꿔 끼운다.
pub trait Api: Send + Sync + 'static {
    /// (계정, 호출자 ARN)
    fn sts_caller(&self) -> impl Future<Output = Result<(String, String), AwsErr>> + Send;
    fn sns_topics(&self, next: Option<String>) -> impl Future<Output = Page<String>> + Send;
    fn sns_subscriptions(&self, topic: &str, next: Option<String>) -> impl Future<Output = Page<Subscription>> + Send;
    fn sns_subscription_attrs(&self, arn: &str) -> impl Future<Output = Result<HashMap<String, String>, AwsErr>> + Send;
    fn sqs_queues(&self, next: Option<String>) -> impl Future<Output = Page<String>> + Send;
    /// names 는 속성 이름(All 등)이다
    fn sqs_queue_attrs(&self, url: &str, names: &[&str]) -> impl Future<Output = Result<HashMap<String, String>, AwsErr>> + Send;
    fn sqs_queue_url(&self, name: &str, owner: Option<&str>) -> impl Future<Output = Result<String, AwsErr>> + Send;
    fn lambda_functions(&self, next: Option<String>) -> impl Future<Output = Page<Function>> + Send;
    /// function·source 로 고른다. 둘 다 None 이면 모든 매핑이다
    fn lambda_mappings(
        &self,
        function: Option<&str>,
        source: Option<&str>,
        next: Option<String>,
    ) -> impl Future<Output = Page<SourceMapping>> + Send;
    fn events_buses(&self, next: Option<String>) -> impl Future<Output = Page<String>> + Send;
    fn events_rules(&self, bus: &str, next: Option<String>) -> impl Future<Output = Page<Rule>> + Send;
    fn events_targets(&self, bus: &str, rule: &str, next: Option<String>) -> impl Future<Output = Page<Target>> + Send;
    fn scheduler_groups(&self, next: Option<String>) -> impl Future<Output = Page<String>> + Send;
    fn scheduler_schedules(&self, group: &str, next: Option<String>) -> impl Future<Output = Page<String>> + Send;
    fn scheduler_get(&self, group: &str, name: &str) -> impl Future<Output = Result<Schedule, AwsErr>> + Send;
    fn sfn_machines(&self, next: Option<String>) -> impl Future<Output = Page<Machine>> + Send;
    /// 최근 실행 max 개 한 페이지와 다음 토큰
    fn sfn_executions(&self, arn: &str, max: i32) -> impl Future<Output = Page<Execution>> + Send;
    /// (이름, 리전)
    fn s3_buckets(&self, next: Option<String>) -> impl Future<Output = Page<(String, String)>> + Send;
    fn s3_list(&self, bucket: &str, prefix: &str, region: Option<&str>) -> impl Future<Output = Result<Listing, AwsErr>> + Send;
    /// LocationConstraint 원문
    fn s3_location(&self, bucket: &str) -> impl Future<Output = Result<String, AwsErr>> + Send;
    fn s3_head(&self, bucket: &str, key: &str, region: Option<&str>) -> impl Future<Output = Result<Head, AwsErr>> + Send;
    fn s3_get_range(
        &self,
        bucket: &str,
        key: &str,
        range: &str,
        region: Option<&str>,
    ) -> impl Future<Output = Result<Vec<u8>, AwsErr>> + Send;
}

/// 페이지를 끝까지 잇는다.
async fn all_pages<T, F, Fut>(mut call: F) -> Result<Vec<T>, AwsErr>
where
    F: FnMut(Option<String>) -> Fut,
    Fut: Future<Output = Page<T>>,
{
    let mut out = Vec::new();
    let mut next = None;
    loop {
        let (items, token) = call(next).await?;
        out.extend(items);
        match token.filter(|t| !t.is_empty()) {
            Some(t) => next = Some(t),
            None => return Ok(out),
        }
    }
}

/// 0..n 에 f 를 PARALLEL 개씩 동시에 부르고 순서대로 모은다.
async fn each<T, Fut: Future<Output = T>>(n: usize, f: impl FnMut(usize) -> Fut) -> Vec<T> {
    stream::iter(0..n).map(f).buffered(PARALLEL).collect().await
}

// ─── 오류 ───

/// 실패한 AWS 작업(IAM 이름, 예: sqs:ListQueues)과 원래 오류다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpErr {
    pub action: String,
    pub err: AwsErr,
}

impl std::fmt::Display for OpErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "aws {}: {}", self.action, self.err.text())
    }
}

fn op(action: &str) -> impl FnOnce(AwsErr) -> OpErr + '_ {
    move |err| OpErr {
        action: action.into(),
        err,
    }
}

impl From<OpErr> for Error {
    fn from(e: OpErr) -> Error {
        Error::msg(e.to_string())
    }
}

/// e 가 권한 거절이면 그 작업 이름이다. 서비스마다 코드가 달라(SNS AuthorizationError, SQS·S3 AccessDenied,
/// Lambda·Scheduler AccessDeniedException) 이름에 AccessDenied 가 든 것과 둘을 더 본다.
fn denied_action(e: &OpErr) -> Option<&str> {
    let code = &e.err.code;
    (code.contains("AccessDenied") || code == "AuthorizationError" || code == "UnauthorizedOperation").then_some(e.action.as_str())
}

/// 트리 한 단계의 결과다. 서비스 오류(OpErr)는 권한 거절이면 한 줄로 바꾸고, 요금 막음 같은 다른 오류는 그대로 낸다.
enum TreeErr {
    Op(OpErr),
    Other(Error),
}

impl From<OpErr> for TreeErr {
    fn from(e: OpErr) -> TreeErr {
        TreeErr::Op(e)
    }
}

impl From<Error> for TreeErr {
    fn from(e: Error) -> TreeErr {
        TreeErr::Other(e)
    }
}

// ─── 공통 ───

fn kst() -> FixedOffset {
    FixedOffset::east_opt(9 * 3600).expect("KST")
}

/// KST `2006-01-02 15:04` 다. 없으면 빈 글이다.
fn short_time(t: Option<DateTime<Utc>>) -> String {
    t.map(|t| t.with_timezone(&kst()).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

/// KST 초까지다. 없으면 빈 글이다.
fn long_time(t: Option<DateTime<Utc>>) -> String {
    t.map(|t| t.with_timezone(&kst()).format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_default()
}

/// ARN 의 i 번째 마디다(arn:partition:service:region:account:resource). 없으면 빈 글이다. resource(5) 는 뒤에 ':' 가 더 있어도 통째다.
fn arn_part(arn: &str, i: usize) -> &str {
    if !arn.starts_with("arn:") {
        return "";
    }
    arn.splitn(6, ':').nth(i).unwrap_or("")
}

fn table(cols: &[&str]) -> QueryResult {
    QueryResult {
        cols: cols.iter().map(|c| c.to_string()).collect(),
        ..Default::default()
    }
}

/// 맨 위 폴더다. key 가 경로의 첫 마디다(R67.1).
const FOLDERS: [(&str, &str); 7] = [
    ("sns", "SNS 토픽"),
    ("sqs", "SQS 큐"),
    ("lambda", "Lambda"),
    ("events", "EventBridge"),
    ("scheduler", "Scheduler"),
    ("sfn", "Step Functions"),
    ("s3", "S3"),
];

/// 요청마다 요금이 붙는 폴더다(R72). 막혀 있으면 맨 위에 보이지 않고, 경로로 와도 보내기 전에 Billed 다.
fn billed_service(svc: &str) -> bool {
    matches!(svc, "sns" | "sqs" | "s3")
}

/// 이벤트 흐름 설정의 큐 하나다. name·dlq 는 이름 또는 ARN 이다.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QueueRef {
    pub name: String,
    pub dlq: String,
}

/// 풀어 낸 큐다. arn 은 매핑을 찾은 ARN 이고 dlq_arn 은 상세(open `sqs/<ARN>`)로 열 DLQ 다. 매핑을 못 읽었으면 err 다.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Queue {
    pub name: String,
    pub arn: String,
    pub dlq: String,
    pub dlq_arn: String,
    pub lambdas: Vec<Mapping>,
    pub err: String,
}

/// 큐를 읽는 Lambda 이벤트 소스 매핑이다.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mapping {
    pub function: String,
    pub state: String,
    pub last_result: String,
}

/// 매핑의 마지막 처리 결과가 문제 없음인지다. `No records processed` 는 메시지가 없던 매핑의 보통 값이라 경고하지 않는다.
pub fn mapping_ok(r: &str) -> bool {
    r == "OK" || r.is_empty() || r == "No records processed"
}

/// aws 접속 하나다.
pub struct Backend<A: Api = Sdk> {
    cl: A,
    /// 접속 리전. 다른 리전 큐 ARN 을 가르고, 흐름 큐 이름을 ARN 으로 만든다
    region: String,
    /// S3 버킷 이름 → 리전
    regions: Mutex<HashMap<String, String>>,
}

impl Backend<Sdk> {
    pub async fn new(c: &Conn) -> Result<Backend<Sdk>, Error> {
        let cfg = crate::aws::sdk_config(c).await;
        let region = cfg.region().map(|r| r.to_string()).unwrap_or_default();
        Ok(Backend::with_api(Sdk::new(&cfg), &region))
    }
}

impl<A: Api> Backend<A> {
    pub fn with_api(cl: A, region: &str) -> Backend<A> {
        Backend {
            cl,
            region: region.into(),
            regions: Mutex::new(HashMap::new()),
        }
    }

    /// "" 이면 폴더, 그 밖은 서비스별 목록이다. 맨 위는 GetCallerIdentity 로 자격 증명부터 확인한다(그러지 않으면 접속 시험이
    /// 자격 증명 없이도 성공한다). 권한이 없는 목록은 오류 대신 `권한 없음: <작업>` 한 줄이라 다른 폴더는 그대로 쓴다.
    pub async fn children(&self, path: &str) -> Result<Vec<Node>, Error> {
        if path.is_empty() {
            self.cl.sts_caller().await.map_err(op("sts:GetCallerIdentity"))?;
            let allow = source::billed().is_ok();
            return Ok(FOLDERS
                .iter()
                .filter(|(k, _)| allow || !billed_service(k))
                .map(|(k, n)| Node {
                    name: n.to_string(),
                    path: k.to_string(),
                    has_children: true,
                    ..Default::default()
                })
                .collect());
        }
        let (svc, rest) = path.split_once('/').unwrap_or((path, ""));
        if billed_service(svc) {
            source::billed()?;
        }
        let r: Result<Vec<Node>, TreeErr> = match svc {
            "sns" => self.sns_children(rest).await.map_err(Into::into),
            "sqs" if rest.is_empty() => self.queues().await.map_err(Into::into),
            "lambda" if rest.is_empty() => self.functions().await.map_err(Into::into),
            "events" => self.events_children(rest).await.map_err(Into::into),
            "scheduler" => self.scheduler_children(rest).await.map_err(Into::into),
            "sfn" if rest.is_empty() => self.machines().await.map_err(Into::into),
            "s3" => self.s3_children(rest).await,
            _ => Ok(Vec::new()),
        };
        match r {
            Ok(n) => Ok(n),
            Err(TreeErr::Op(e)) => match denied_action(&e) {
                Some(action) => Ok(vec![Node {
                    name: format!("권한 없음: {action}"),
                    path: format!("#denied:{path}"),
                    ..Default::default()
                }]),
                None => Err(e.into()),
            },
            Err(TreeErr::Other(e)) => Err(e),
        }
    }

    /// 노드 Enter 의 표다. 경로 첫 마디로 서비스를 고른다. `권한 없음`·`… 앞 N개만` 줄(#로 시작)은 열 것이 없다.
    pub async fn open(&self, path: &str) -> Result<QueryResult, Error> {
        let (svc, rest) = path.split_once('/').unwrap_or((path, ""));
        if billed_service(svc) {
            source::billed()?;
        }
        let r = match svc {
            _ if rest.is_empty() => None,
            "sns" => Some(self.subscription_table(rest).await),
            "sqs" => Some(self.queue_table(rest).await),
            "lambda" => Some(self.mapping_table(rest).await),
            "events" => match self.target_table(rest).await {
                Ok(Some(t)) => Some(Ok(t)),
                Ok(None) => None,
                Err(e) => Some(Err(e)),
            },
            "scheduler" => Some(self.schedule_table(rest).await),
            "sfn" => Some(self.execution_table(rest).await),
            "s3" if rest.contains('/') && !rest.ends_with('/') => Some(self.object_table(rest).await),
            _ => None,
        };
        match r {
            Some(r) => r.map_err(Into::into),
            None => Err(Error::msg(format!("nothing to open: {path}"))),
        }
    }

    /// 대시보드 확인이다(R64.2·R67.1): `Lambda 86 · 매핑 꺼짐 2 · 실패 1`. 요금이 붙지 않는 lambda:ListFunctions·ListEventSourceMappings 만
    /// 부르고 요금 허용(R72)과 상관없이 SQS 는 묻지 않는다. 지연은 함수 목록 시간이다.
    pub async fn probe(&self) -> Health {
        let start = Instant::now();
        let fns = match all_pages(|n| self.cl.lambda_functions(n)).await {
            Ok(f) => f.len(),
            Err(e) => return failed(start.elapsed(), &op("lambda:ListFunctions")(e).to_string(), ""),
        };
        let latency = start.elapsed();
        let maps = match all_pages(|n| self.cl.lambda_mappings(None, None, n)).await {
            Ok(m) => m,
            Err(e) => return failed(latency, &op("lambda:ListEventSourceMappings")(e).to_string(), ""),
        };
        let off = maps.iter().filter(|m| m.state != "Enabled").count();
        let bad = maps.iter().filter(|m| !mapping_ok(&m.last_result)).count();
        Health {
            ok: true,
            latency,
            metrics: vec![
                Metric::new("Lambda", fns.to_string(), false),
                Metric::new("매핑 꺼짐", off.to_string(), off > 0),
                Metric::new("실패", bad.to_string(), bad > 0),
            ],
            ..Default::default()
        }
    }

    // ─── 이벤트 흐름 (R69) ───

    /// refs 마다 Lambda 매핑을 찾는다. 이름은 GetCallerIdentity 의 계정(이름이 하나라도 있을 때 한 번)과 접속 리전으로 ARN 을 만들고,
    /// DLQ 이름은 그 큐와 같은 계정·리전으로 본다. 큐 수·DLQ 수는 SQS 요청마다 요금이 붙어 묻지 않는다(R72).
    /// 계정을 못 읽으면 오류이고, 큐 하나의 실패는 그 큐의 err 다.
    pub async fn queue_flows(&self, refs: &[QueueRef]) -> Result<Vec<Queue>, Error> {
        let (mut partition, mut account) = ("aws".to_string(), String::new());
        if refs.iter().any(|r| !r.name.starts_with("arn:")) {
            if self.region.is_empty() {
                return Err(Error::msg("aws connection has no region"));
            }
            let (acct, arn) = self.cl.sts_caller().await.map_err(op("sts:GetCallerIdentity"))?;
            account = acct;
            let p = arn_part(&arn, 1);
            if !p.is_empty() {
                partition = p.to_string();
            }
        }
        let out = each(refs.len(), |i| {
            let r = &refs[i];
            let arn = if r.name.starts_with("arn:") {
                r.name.clone()
            } else {
                format!("arn:{partition}:sqs:{}:{account}:{}", self.region, r.name)
            };
            async move {
                let mut q = Queue {
                    name: arn_part(&arn, 5).to_string(),
                    arn: arn.clone(),
                    dlq_arn: r.dlq.clone(),
                    ..Default::default()
                };
                if !r.dlq.is_empty() && !r.dlq.starts_with("arn:") {
                    q.dlq_arn = format!(
                        "arn:{}:sqs:{}:{}:{}",
                        arn_part(&arn, 1),
                        arn_part(&arn, 3),
                        arn_part(&arn, 4),
                        r.dlq
                    );
                }
                q.dlq = arn_part(&q.dlq_arn, 5).to_string();
                match self.queue_mappings(&arn).await {
                    Ok(m) => q.lambdas = m,
                    Err((m, e)) => {
                        q.lambdas = m;
                        q.err = e.to_string();
                    }
                }
                q
            }
        })
        .await;
        Ok(out)
    }

    /// EventSourceArn 이 큐인 Lambda 매핑이다. 함수 이름은 FunctionArn 끝 마디다. 실패해도 받은 데까지 함께 돌려준다.
    async fn queue_mappings(&self, queue_arn: &str) -> Result<Vec<Mapping>, (Vec<Mapping>, OpErr)> {
        let mut out = Vec::new();
        let mut next = None;
        loop {
            let (page, token) = match self.cl.lambda_mappings(None, Some(queue_arn), next).await {
                Ok(p) => p,
                Err(e) => return Err((out, op("lambda:ListEventSourceMappings")(e))),
            };
            for m in page {
                let f = m.function_arn.rsplit_once(":function:").map_or(m.function_arn.as_str(), |(_, f)| f);
                out.push(Mapping {
                    function: f.to_string(),
                    state: m.state,
                    last_result: m.last_result,
                });
            }
            match token.filter(|t| !t.is_empty()) {
                Some(t) => next = Some(t),
                None => return Ok(out),
            }
        }
    }

    // ─── SNS (R67.2) ───

    /// rest "" 면 토픽, "<토픽 ARN>" 이면 구독이다. 토픽 경로는 sns/<ARN> 이고 ARN 에는 / 가 없다.
    async fn sns_children(&self, rest: &str) -> Result<Vec<Node>, OpErr> {
        if rest.is_empty() {
            let topics = all_pages(|n| self.cl.sns_topics(n)).await.map_err(op("sns:ListTopics"))?;
            return Ok(topics
                .into_iter()
                .map(|arn| Node {
                    name: arn_part(&arn, 5).to_string(),
                    path: format!("sns/{arn}"),
                    has_children: true,
                    openable: true,
                    ..Default::default()
                })
                .collect());
        }
        // 구독 줄은 잎이다
        if rest.contains('/') {
            return Ok(Vec::new());
        }
        let subs = self.subscriptions(rest).await?;
        Ok(subs
            .iter()
            .map(|s| Node {
                name: sub_endpoint(rest, s),
                info: s.protocol.clone(),
                path: format!("sns/{rest}/{}", s.arn),
                ..Default::default()
            })
            .collect())
    }

    async fn subscriptions(&self, topic: &str) -> Result<Vec<Subscription>, OpErr> {
        all_pages(|n| self.cl.sns_subscriptions(topic, n))
            .await
            .map_err(op("sns:ListSubscriptionsByTopic"))
    }

    /// 토픽 Enter 의 구독 표다. raw·filter 는 확인된 구독마다 GetSubscriptionAttributes 로 읽는다. 확인 전 구독은 빈 칸이다.
    async fn subscription_table(&self, topic: &str) -> Result<QueryResult, OpErr> {
        let subs = self.subscriptions(topic).await?;
        let attrs = each(subs.len(), |i| {
            let arn = subs[i].arn.clone();
            async move {
                if !arn.starts_with("arn:") {
                    return Ok(HashMap::new());
                }
                self.cl
                    .sns_subscription_attrs(&arn)
                    .await
                    .map_err(op("sns:GetSubscriptionAttributes"))
            }
        })
        .await;
        let mut res = table(&["protocol", "endpoint", "subscription_arn", "raw", "filter"]);
        for (s, a) in subs.iter().zip(attrs) {
            let (raw, filter) = match a {
                Ok(a) => (
                    a.get("RawMessageDelivery").cloned().unwrap_or_default(),
                    a.get("FilterPolicy").cloned().unwrap_or_default(),
                ),
                Err(e) => (e.to_string(), String::new()),
            };
            res.rows
                .push(vec![s.protocol.clone(), sub_endpoint(topic, s), s.arn.clone(), raw, filter]);
        }
        Ok(res)
    }

    // ─── SQS (R67.2) ───

    async fn queue_attrs(&self, url: &str, names: &[&str]) -> Result<HashMap<String, String>, OpErr> {
        self.cl.sqs_queue_attrs(url, names).await.map_err(op("sqs:GetQueueAttributes"))
    }

    /// 큐 목록이다. info 는 `대기 · 처리 중 · 지연` 이고 DLQ 가 있으면 그 큐의 대기 수를 붙인다(R67.2).
    /// DLQ 는 보통 같은 목록에 있어 그 속성을 쓰고, 없으면(다른 계정 등) GetQueueUrl 로 따로 읽는다. DLQ 에 메시지가 있으면 경고다.
    async fn queues(&self) -> Result<Vec<Node>, OpErr> {
        let urls = all_pages(|n| self.cl.sqs_queues(n)).await.map_err(op("sqs:ListQueues"))?;
        let attrs = each(urls.len(), |i| self.queue_attrs(&urls[i], &["All"])).await;
        let by_arn: HashMap<String, &HashMap<String, String>> = attrs
            .iter()
            .filter_map(|a| a.as_ref().ok())
            .map(|a| (a.get(ATTR_ARN).cloned().unwrap_or_default(), a))
            .collect();
        let nodes = each(urls.len(), |i| {
            let (url, a, by_arn) = (&urls[i], &attrs[i], &by_arn);
            async move {
                let name = url.rsplit('/').next().unwrap_or(url).to_string();
                let mut n = Node {
                    path: format!("sqs/{name}"),
                    name,
                    openable: true,
                    ..Default::default()
                };
                let a = match a {
                    Ok(a) => a,
                    Err(e) => {
                        n.info = match denied_action(e) {
                            Some(action) => format!("권한 없음: {action}"),
                            None => "속성 읽기 실패".into(),
                        };
                        return n;
                    }
                };
                let get = |k: &str| a.get(k).cloned().unwrap_or_default();
                n.info = format!(
                    "대기 {} · 처리 중 {} · 지연 {}",
                    get(ATTR_VISIBLE),
                    get(ATTR_IN_FLIGHT),
                    get(ATTR_DELAYED)
                );
                let dlq = dlq_arn(a);
                if !dlq.is_empty() {
                    let cnt = match by_arn.get(&dlq) {
                        Some(d) => d.get(ATTR_VISIBLE).cloned(),
                        // 못 읽으면 수는 ? 로 둔다
                        None => self.dlq_attrs(&dlq).await.ok().and_then(|d| d.get(ATTR_VISIBLE).cloned()),
                    }
                    .unwrap_or_else(|| "?".into());
                    n.info.push_str(&format!(" · DLQ {} {cnt}", arn_part(&dlq, 5)));
                    n.warn = cnt.parse::<i64>().unwrap_or(0) > 0;
                }
                n
            }
        })
        .await;
        Ok(nodes)
    }

    /// 목록에 없는 DLQ 의 대기 수를 ARN 으로 읽는다.
    async fn dlq_attrs(&self, arn: &str) -> Result<HashMap<String, String>, OpErr> {
        let url = self
            .cl
            .sqs_queue_url(arn_part(arn, 5), Some(arn_part(arn, 4)))
            .await
            .map_err(op("sqs:GetQueueUrl"))?;
        self.queue_attrs(&url, &[ATTR_VISIBLE]).await
    }

    /// 큐 Enter 의 `attribute, value` 표다. 첫 줄은 QueueUrl 이고 나머지는 속성 이름순이다. name 이 ARN 이면 그 계정의 큐를 연다.
    /// 접속과 다른 리전 큐는 이 리전 클라이언트로 열 수 없어 안내 한 줄만 둔다.
    async fn queue_table(&self, name: &str) -> Result<QueryResult, OpErr> {
        let (qname, owner) = if name.starts_with("arn:") {
            let r = arn_part(name, 3);
            if !self.region.is_empty() && r != self.region {
                let mut res = table(&["attribute", "value"]);
                res.rows.push(vec!["QueueArn".into(), name.into()]);
                res.rows.push(vec!["note".into(), "다른 리전 큐는 수만 보임".into()]);
                return Ok(res);
            }
            (arn_part(name, 5), Some(arn_part(name, 4)))
        } else {
            (name, None)
        };
        let url = self.cl.sqs_queue_url(qname, owner).await.map_err(op("sqs:GetQueueUrl"))?;
        let attrs = self.queue_attrs(&url, &["All"]).await?;
        let mut res = table(&["attribute", "value"]);
        res.rows.push(vec!["QueueUrl".into(), url]);
        let mut keys: Vec<&String> = attrs.keys().collect();
        keys.sort();
        for k in keys {
            res.rows.push(vec![k.clone(), attrs[k].clone()]);
        }
        Ok(res)
    }

    // ─── Lambda (R67.3) ───

    /// 함수 목록이다. info 는 런타임과 마지막 수정 시각(KST)이다. 컨테이너 이미지 함수는 런타임이 없다.
    async fn functions(&self) -> Result<Vec<Node>, OpErr> {
        let fns = all_pages(|n| self.cl.lambda_functions(n))
            .await
            .map_err(op("lambda:ListFunctions"))?;
        Ok(fns
            .into_iter()
            .map(|f| {
                let modified = DateTime::parse_from_str(&f.last_modified, "%Y-%m-%dT%H:%M:%S%.3f%z")
                    .map(|t| short_time(Some(t.with_timezone(&Utc))))
                    .unwrap_or(f.last_modified);
                let info = if f.runtime.is_empty() { f.package_type } else { f.runtime };
                Node {
                    path: format!("lambda/{}", f.name),
                    name: f.name,
                    info: format!("{info} · {modified}"),
                    openable: true,
                    ..Default::default()
                }
            })
            .collect())
    }

    /// 함수 Enter 의 이벤트 소스 매핑 표다(SQS·Kinesis·DynamoDB 스트림 등).
    async fn mapping_table(&self, f: &str) -> Result<QueryResult, OpErr> {
        let maps = all_pages(|n| self.cl.lambda_mappings(Some(f), None, n))
            .await
            .map_err(op("lambda:ListEventSourceMappings"))?;
        let mut res = table(&["source", "state", "batch", "last_result"]);
        for m in maps {
            res.rows.push(vec![
                m.source_arn,
                m.state,
                m.batch.map(|b| b.to_string()).unwrap_or_default(),
                m.last_result,
            ]);
        }
        Ok(res)
    }

    // ─── EventBridge (R67.4) ───

    /// rest "" 면 버스, "<버스>" 면 규칙이다. 규칙은 잎이고 경로는 events/<버스>/<규칙>이다.
    /// 파트너 버스 이름(aws.partner/…)에는 / 가 있지만 규칙 이름에는 없어 open 은 마지막 / 로 가른다.
    async fn events_children(&self, rest: &str) -> Result<Vec<Node>, OpErr> {
        if rest.is_empty() {
            let buses = all_pages(|n| self.cl.events_buses(n)).await.map_err(op("events:ListEventBuses"))?;
            return Ok(buses
                .into_iter()
                .map(|b| Node {
                    path: format!("events/{b}"),
                    name: b,
                    has_children: true,
                    ..Default::default()
                })
                .collect());
        }
        let rules = all_pages(|n| self.cl.events_rules(rest, n)).await.map_err(op("events:ListRules"))?;
        Ok(rules
            .into_iter()
            .map(|r| {
                let kind = if r.schedule.is_empty() { "pattern".to_string() } else { r.schedule };
                Node {
                    path: format!("events/{rest}/{}", r.name),
                    name: r.name,
                    info: format!("{kind} · {}", r.state),
                    openable: true,
                    ..Default::default()
                }
            })
            .collect())
    }

    /// 규칙 Enter 의 대상 표다. input 은 Input, 없으면 InputPath, 없으면 InputTransformer 의 템플릿이다. 버스·규칙이 아니면 None 이다.
    async fn target_table(&self, rest: &str) -> Result<Option<QueryResult>, OpErr> {
        let Some((bus, rule)) = rest.rsplit_once('/') else {
            return Ok(None);
        };
        let targets = all_pages(|n| self.cl.events_targets(bus, rule, n))
            .await
            .map_err(op("events:ListTargetsByRule"))?;
        let mut res = table(&["target_id", "arn", "input", "dlq"]);
        for t in targets {
            let input = if !t.input.is_empty() {
                t.input
            } else if let Some(p) = t.input_path {
                p
            } else {
                t.input_template.unwrap_or_default()
            };
            res.rows.push(vec![t.id, t.arn, input, t.dlq.unwrap_or_default()]);
        }
        Ok(Some(res))
    }

    // ─── EventBridge Scheduler (R67.4) ───

    /// rest "" 면 그룹, "<그룹>" 이면 일정이다. 목록에는 식이 없어 일정마다 GetSchedule 을 PARALLEL 개씩 부른다.
    async fn scheduler_children(&self, rest: &str) -> Result<Vec<Node>, OpErr> {
        if rest.is_empty() {
            let groups = all_pages(|n| self.cl.scheduler_groups(n))
                .await
                .map_err(op("scheduler:ListScheduleGroups"))?;
            return Ok(groups
                .into_iter()
                .map(|g| Node {
                    path: format!("scheduler/{g}"),
                    name: g,
                    has_children: true,
                    ..Default::default()
                })
                .collect());
        }
        if rest.contains('/') {
            return Ok(Vec::new());
        }
        let names = all_pages(|n| self.cl.scheduler_schedules(rest, n))
            .await
            .map_err(op("scheduler:ListSchedules"))?;
        Ok(each(names.len(), |i| {
            let name = names[i].clone();
            async move {
                let info = match self.schedule(rest, &name).await {
                    Ok(s) => format!("{} · {}", s.expression, s.state),
                    Err(e) => match denied_action(&e) {
                        Some(action) => format!("권한 없음: {action}"),
                        None => "읽기 실패".into(),
                    },
                };
                Node {
                    path: format!("scheduler/{rest}/{name}"),
                    name,
                    info,
                    openable: true,
                    ..Default::default()
                }
            }
        })
        .await)
    }

    async fn schedule(&self, group: &str, name: &str) -> Result<Schedule, OpErr> {
        self.cl.scheduler_get(group, name).await.map_err(op("scheduler:GetSchedule"))
    }

    /// 일정 Enter 의 `attribute, value` 표다(식·시간대·상태·대상).
    async fn schedule_table(&self, rest: &str) -> Result<QueryResult, OpErr> {
        let (group, name) = rest.split_once('/').unwrap_or((rest, ""));
        let s = self.schedule(group, name).await?;
        let mut res = table(&["attribute", "value"]);
        let mut push = |k: &str, v: String| res.rows.push(vec![k.into(), v]);
        push("expression", s.expression);
        push("timezone", s.timezone);
        push("state", s.state);
        if let Some(t) = s.target {
            push("target_arn", t.arn);
            push("input", t.input);
            push("role_arn", t.role_arn);
            if let Some(d) = t.dlq {
                push("dlq", d);
            }
        }
        Ok(res)
    }

    // ─── Step Functions (R67.5) ───

    /// 상태 기계 목록이다(info 는 STANDARD·EXPRESS 와 만든 시각). 경로는 sfn/<ARN> 이다.
    async fn machines(&self) -> Result<Vec<Node>, OpErr> {
        let ms = all_pages(|n| self.cl.sfn_machines(n))
            .await
            .map_err(op("states:ListStateMachines"))?;
        Ok(ms
            .into_iter()
            .map(|m| Node {
                name: m.name,
                path: format!("sfn/{}", m.arn),
                info: format!("{} · {}", m.kind, short_time(m.created)),
                openable: true,
                ..Default::default()
            })
            .collect())
    }

    /// 최근 실행 50개 표다. 시각은 KST 초까지이고 실행 중이면 stop 이 빈 칸이다.
    async fn execution_table(&self, arn: &str) -> Result<QueryResult, OpErr> {
        let (execs, next) = self
            .cl
            .sfn_executions(arn, RECENT_EXECUTIONS)
            .await
            .map_err(op("states:ListExecutions"))?;
        let mut res = table(&["name", "status", "start", "stop"]);
        for e in execs {
            res.rows.push(vec![e.name, e.status, long_time(e.start), long_time(e.stop)]);
        }
        res.truncated = next.is_some_and(|n| !n.is_empty());
        Ok(res)
    }

    // ─── S3 (R67.6) ───

    /// rest "" 면 버킷, "<버킷>[/<접두>]" 면 그 접두 바로 아래 접두(/ 구분)와 객체다.
    async fn s3_children(&self, rest: &str) -> Result<Vec<Node>, TreeErr> {
        if rest.is_empty() {
            return Ok(self.buckets().await?);
        }
        let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
        // 객체는 잎이다
        if !prefix.is_empty() && !prefix.ends_with('/') {
            return Ok(Vec::new());
        }
        let region = self.bucket_region(bucket).await;
        let l = self
            .cl
            .s3_list(bucket, prefix, region.as_deref())
            .await
            .map_err(op("s3:ListBucket"))?;
        let mut nodes: Vec<Node> = l
            .prefixes
            .iter()
            .map(|p| Node {
                name: p.strip_prefix(prefix).unwrap_or(p).to_string(),
                path: format!("s3/{bucket}/{p}"),
                has_children: true,
                ..Default::default()
            })
            .collect();
        // 콘솔이 만든 빈 "폴더" 객체는 뺀다
        for o in l.objects.iter().filter(|o| o.key != prefix) {
            nodes.push(Node {
                name: o.key.strip_prefix(prefix).unwrap_or(&o.key).to_string(),
                path: format!("s3/{bucket}/{}", o.key),
                info: format!("{} · {}", human_bytes(o.size), short_time(o.modified)),
                openable: true,
                ..Default::default()
            });
        }
        if l.truncated {
            nodes.push(Node {
                name: format!("… 앞 {S3_LEVEL}개만 보임"),
                path: format!("#more:{rest}"),
                ..Default::default()
            });
        }
        Ok(nodes)
    }

    /// 버킷 목록이다(info 는 리전). 리전은 버킷마다 기억해 두고 그 리전 엔드포인트로 묻는다:
    /// 리다이렉트를 따라가지 않아(R60.1) 다른 리전 버킷을 접속 리전으로 물으면 301 오류다.
    async fn buckets(&self) -> Result<Vec<Node>, OpErr> {
        let bs = all_pages(|n| self.cl.s3_buckets(n)).await.map_err(op("s3:ListAllMyBuckets"))?;
        let mut regions = self.regions.lock().unwrap();
        Ok(bs
            .into_iter()
            .map(|(name, region)| {
                if !region.is_empty() {
                    regions.insert(name.clone(), region.clone());
                }
                Node {
                    path: format!("s3/{name}"),
                    name,
                    info: region,
                    has_children: true,
                    ..Default::default()
                }
            })
            .collect())
    }

    /// 버킷 리전이다. 목록에서 못 받았으면 GetBucketLocation 으로 묻고(빈 값은 us-east-1), 그것도 안 되면 None(접속 리전)이다.
    async fn bucket_region(&self, bucket: &str) -> Option<String> {
        if let Some(r) = self.regions.lock().unwrap().get(bucket) {
            return Some(r.clone());
        }
        let loc = self.cl.s3_location(bucket).await.ok()?;
        let r = match loc.as_str() {
            "" => "us-east-1".to_string(),
            "EU" => "eu-west-1".to_string(),
            r => r.to_string(),
        };
        self.regions.lock().unwrap().insert(bucket.into(), r.clone());
        Some(r)
    }

    /// 객체 Enter 의 `attribute, value` 표다. HeadObject 로 메타데이터를 보이고, 텍스트로 보이는 64KiB 이하 객체만 본문을 붙인다.
    /// 본문은 Range bytes=0-65535 로 요청해 그 사이 객체가 커져도 64KiB 넘게 받지 않는다.
    async fn object_table(&self, rest: &str) -> Result<QueryResult, OpErr> {
        let (bucket, key) = rest.split_once('/').unwrap_or((rest, ""));
        let region = self.bucket_region(bucket).await;
        // HeadObject 의 IAM 작업은 s3:GetObject 다
        let h = self.cl.s3_head(bucket, key, region.as_deref()).await.map_err(op("s3:GetObject"))?;
        let mut res = table(&["attribute", "value"]);
        for (k, v) in [
            ("key", key.to_string()),
            ("size", h.size.to_string()),
            ("last_modified", long_time(h.modified)),
            ("content_type", h.content_type.clone()),
            ("content_encoding", h.content_encoding.clone()),
            ("etag", h.etag.clone()),
            ("storage_class", h.storage_class.clone()),
            ("server_side_encryption", h.sse.clone()),
            ("version_id", h.version_id.clone()),
        ] {
            res.rows.push(vec![k.into(), v]);
        }
        let mut meta: Vec<(&String, &String)> = h.metadata.iter().collect();
        meta.sort();
        for (k, v) in meta {
            res.rows.push(vec![format!("x-amz-meta-{k}"), v.clone()]);
        }
        let encoded = !h.content_encoding.is_empty() && h.content_encoding != "identity";
        if !text_like(&h.content_type, key) || encoded {
        } else if h.size > S3_PEEK {
            res.rows.push(vec!["body".into(), "(64KiB 를 넘어 받지 않음)".into()]);
        } else {
            let data = self
                .cl
                .s3_get_range(bucket, key, &format!("bytes=0-{}", S3_PEEK - 1), region.as_deref())
                .await
                .map_err(op("s3:GetObject"))?;
            let data = &data[..data.len().min(S3_PEEK as usize)];
            let body = match std::str::from_utf8(data) {
                Ok(s) => s.to_string(),
                Err(_) => "(텍스트가 아님)".into(),
            };
            res.rows.push(vec!["body".into(), body]);
        }
        Ok(res)
    }
}

impl<A: Api> source::Backend for Backend<A> {
    fn children<'a>(&'a self, path: &'a str) -> BoxFut<'a, Result<Vec<Node>, Error>> {
        Box::pin(Backend::children(self, path))
    }

    /// 늘 거절한다. aws 접속은 트리로 본다(R67.1). 통합은 보내기 전에 종류로 막고 이것은 안전망이다.
    fn run<'a>(&'a self, _q: &'a Query) -> BoxFut<'a, Result<QueryResult, Error>> {
        Box::pin(async { Err(Error::Write("aws connection has no query language".into())) })
    }

    fn open<'a>(&'a self, path: &'a str, _limit: usize) -> BoxFut<'a, Result<QueryResult, Error>> {
        Box::pin(Backend::open(self, path))
    }

    fn probe(&self) -> Option<BoxFut<'_, Health>> {
        Some(Box::pin(Backend::probe(self)))
    }
}

const ATTR_ARN: &str = "QueueArn";
const ATTR_REDRIVE: &str = "RedrivePolicy";
const ATTR_VISIBLE: &str = "ApproximateNumberOfMessages";
const ATTR_IN_FLIGHT: &str = "ApproximateNumberOfMessagesNotVisible";
const ATTR_DELAYED: &str = "ApproximateNumberOfMessagesDelayed";

/// RedrivePolicy 의 deadLetterTargetArn 이다. 정책이 없거나 읽을 수 없으면 빈 글이다.
fn dlq_arn(attrs: &HashMap<String, String>) -> String {
    attrs
        .get(ATTR_REDRIVE)
        .and_then(|p| serde_json::from_str::<serde_json::Value>(p).ok())
        .and_then(|v| v.get("deadLetterTargetArn").and_then(|t| t.as_str()).map(String::from))
        .unwrap_or_default()
}

/// 구독 대상의 보일 글이다. 토픽과 같은 계정의 SQS 큐는 큐 이름으로 줄인다(R67.2).
fn sub_endpoint(topic: &str, s: &Subscription) -> String {
    let acct = arn_part(&s.endpoint, 4);
    if s.protocol == "sqs" && !acct.is_empty() && acct == arn_part(topic, 4) {
        return arn_part(&s.endpoint, 5).to_string();
    }
    s.endpoint.clone()
}

/// Content-Type 이 비었거나 application/octet-stream 일 때 텍스트로 볼 확장자다.
const TEXT_EXTS: [&str; 16] = [
    ".txt", ".log", ".csv", ".tsv", ".json", ".jsonl", ".ndjson", ".md", ".yaml", ".yml", ".xml", ".html", ".htm", ".sql", ".ini", ".toml",
];

/// 본문을 글로 보일 만한 객체인지다. text/*, JSON·XML·YAML·JavaScript·CSV 꼴 Content-Type 이거나
/// 형식을 모르는 객체의 확장자가 TEXT_EXTS 에 있으면 그렇다.
fn text_like(content_type: &str, key: &str) -> bool {
    // Go mime.ParseMediaType 처럼 ; 앞을 소문자로 다듬는다. 잘못된 값은 빈 형식으로 본다
    let mt = content_type.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    if mt.starts_with("text/") {
        return true;
    }
    if matches!(
        mt.as_str(),
        "application/json"
            | "application/xml"
            | "application/yaml"
            | "application/x-yaml"
            | "application/javascript"
            | "application/x-ndjson"
            | "application/csv"
    ) || mt.ends_with("+json")
        || mt.ends_with("+xml")
    {
        return true;
    }
    if mt.is_empty() || mt == "application/octet-stream" || mt == "binary/octet-stream" {
        let base = key.rsplit('/').next().unwrap_or(key);
        let ext = base.rfind('.').map(|i| base[i..].to_ascii_lowercase()).unwrap_or_default();
        return TEXT_EXTS.contains(&ext.as_str());
    }
    false
}

#[cfg(test)]
mod tests;
