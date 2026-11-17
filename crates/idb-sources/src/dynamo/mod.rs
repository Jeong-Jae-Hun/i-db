//! DynamoDB 읽기 전용 저장소(R60·R63). 트리는 테이블 → 키·인덱스이고, 편집기는 PartiQL SELECT 만 ExecuteStatement 로 보낸다.
//! Go `internal/dynamo` 다. 테이블 Enter(open)는 스캔이라 늘 NeedsConfirm 이고, 확인받은 통합은 `SELECT * FROM "테이블"` 을
//! force 로 run 한다(R63.1).

mod partiql;

use std::collections::HashMap;
use std::fmt::Write as _;
use std::future::Future;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use aws_sdk_dynamodb::types::{AttributeValue, KeySchemaElement, KeyType, TableDescription};
use idb_core::gojson;
use idb_core::project::Conn;
use idb_core::result::{MAX_ROWS, NULL_TEXT, QueryResult, cap_cell};
use idb_core::textutil::korean_count;

use crate::Error;
use crate::aws::{AwsErr, sdk_err};
use crate::health::{Health, Metric, failed};
use crate::source::{self, BoxFut, Node, Query};
use partiql::{has_key_condition, is_select, key_condition, sorted_statement, target, tokenize};

/// run 이 이어 받는 상한이다. 거르기(WHERE 의 키 밖 조건)가 있으면 한 페이지(1MB 읽기)에 맞는 항목이 거의 없어도
/// NextToken 이 이어지므로, 행 상한만으로는 큰 테이블을 끝까지 읽는다. 페이지 수와 시간으로 함께 끊고 잘림으로 알린다.
const MAX_PAGES: usize = 50;
/// 호출부 마감이 없을 때만 쓰는 시간 상한
const RUN_TIMEOUT: Duration = Duration::from_secs(60);
const FULL_SCAN: &str = "전체 스캔";

/// Backend 가 부르는 DynamoDB 작업이다. 시험은 가짜로 페이지 이어 받기를 본다.
pub trait Api: Send + Sync + 'static {
    /// (테이블 이름, 다음 시작 이름)
    fn list_tables(&self, start: Option<String>) -> impl Future<Output = Result<(Vec<String>, Option<String>), AwsErr>> + Send;
    fn describe_table(&self, table: &str) -> impl Future<Output = Result<Option<TableDescription>, AwsErr>> + Send;
    /// (항목, NextToken)
    #[allow(clippy::type_complexity)]
    fn execute_statement(
        &self,
        stmt: &str,
        limit: i32,
        next: Option<String>,
    ) -> impl Future<Output = Result<(Vec<HashMap<String, AttributeValue>>, Option<String>), AwsErr>> + Send;
}

/// SDK 클라이언트다.
pub struct Sdk(aws_sdk_dynamodb::Client);

impl Api for Sdk {
    async fn list_tables(&self, start: Option<String>) -> Result<(Vec<String>, Option<String>), AwsErr> {
        let out = self
            .0
            .list_tables()
            .set_exclusive_start_table_name(start)
            .send()
            .await
            .map_err(sdk_err)?;
        Ok((out.table_names.unwrap_or_default(), out.last_evaluated_table_name))
    }

    async fn describe_table(&self, table: &str) -> Result<Option<TableDescription>, AwsErr> {
        let out = self.0.describe_table().table_name(table).send().await.map_err(sdk_err)?;
        Ok(out.table)
    }

    async fn execute_statement(
        &self,
        stmt: &str,
        limit: i32,
        next: Option<String>,
    ) -> Result<(Vec<HashMap<String, AttributeValue>>, Option<String>), AwsErr> {
        let out = self
            .0
            .execute_statement()
            .statement(stmt)
            .limit(limit)
            .set_next_token(next)
            .send()
            .await
            .map_err(sdk_err)?;
        Ok((out.items.unwrap_or_default(), out.next_token))
    }
}

/// DynamoDB 접속 하나다.
pub struct Backend<A: Api = Sdk> {
    cl: A,
    /// url 이 이 머신(DynamoDB Local)이라 요금이 붙지 않는 접속이다. 데이터 읽기 막기(R72)를 건너뛴다
    local: bool,
    /// run 이 읽은 DescribeTable 이다. 결과 정렬을 서버로 다시 읽을지(sort_key) 네트워크 없이 고른다
    tables: Mutex<HashMap<String, TableDescription>>,
}

/// url 의 호스트가 이 머신인지다.
fn is_local_url(url: &str) -> bool {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.trim_matches(['[', ']']).to_string()))
        .is_some_and(|h| h == "localhost" || h == "127.0.0.1" || h == "::1")
}

impl Backend<Sdk> {
    /// 접속을 만든다. url 이 있으면 그 주소로 보낸다(DynamoDB Local 등).
    pub async fn new(c: &Conn) -> Result<Backend<Sdk>, Error> {
        let cfg = crate::aws::sdk_config(c).await;
        let mut b = aws_sdk_dynamodb::config::Builder::from(&cfg);
        if !c.url.is_empty() {
            b = b.endpoint_url(&c.url);
        }
        Ok(Backend::with_api(
            Sdk(aws_sdk_dynamodb::Client::from_conf(b.build())),
            is_local_url(&c.url),
        ))
    }
}

fn wrap(op: &str, e: AwsErr) -> Error {
    Error::msg(format!("dynamodb {op}: {}", e.text()))
}

impl<A: Api> Backend<A> {
    pub fn with_api(cl: A, local: bool) -> Backend<A> {
        Backend {
            cl,
            local,
            tables: Mutex::new(HashMap::new()),
        }
    }

    /// 데이터 읽기(ExecuteStatement, 읽기 요청 단위로 요금) 전에 부른다(R72). 목록·DescribeTable 은 요금이 없어 묻지 않는다.
    fn billed(&self) -> Result<(), Error> {
        if self.local { Ok(()) } else { source::billed() }
    }

    async fn all_tables(&self) -> Result<Vec<String>, AwsErr> {
        let mut out = Vec::new();
        let mut start = None;
        loop {
            let (names, next) = self.cl.list_tables(start).await?;
            out.extend(names);
            match next {
                Some(n) => start = Some(n),
                None => return Ok(out),
            }
        }
    }

    /// "" 이면 테이블 이름, "<테이블>" 이면 항목 수·키·인덱스다.
    pub async fn children(&self, path: &str) -> Result<Vec<Node>, Error> {
        if path.is_empty() {
            let names = self.all_tables().await.map_err(|e| wrap("ListTables", e))?;
            return Ok(names
                .into_iter()
                .map(|t| Node {
                    name: t.clone(),
                    path: t,
                    has_children: true,
                    openable: true,
                    ..Default::default()
                })
                .collect());
        }
        // 키·인덱스 줄은 잎이다. 테이블 이름에는 / 가 올 수 없다
        if path.contains('/') {
            return Ok(Vec::new());
        }
        let d = self.describe(path).await?;
        Ok(table_nodes(&d))
    }

    /// 테이블 미리 보기다. 스캔이라 확인 없이는 보내지 않는다(R63.1). 요금 막음(R72)이 확인보다 먼저다.
    pub fn open(&self) -> Result<QueryResult, Error> {
        self.billed()?;
        Err(Error::NeedsConfirm(FULL_SCAN.into()))
    }

    /// PartiQL SELECT 만 보낸다. 다른 문장은 Write, 파티션 키 조건이 없으면 전체 스캔이라 force 없이는 NeedsConfirm 이다(R63.2).
    /// NextToken 을 이어 q.limit(0 이면 1,000)행까지 받는다.
    pub async fn run(&self, q: &Query) -> Result<QueryResult, Error> {
        self.billed()?;
        let toks = tokenize(&q.text);
        if !is_select(&toks) {
            return Err(Error::Write(String::new()));
        }
        let limit = if q.limit == 0 { MAX_ROWS } else { q.limit };
        let start = Instant::now();
        let mut keys = Vec::new();
        let mut ok = false;
        if let Some((table, index)) = target(&toks) {
            let d = self.describe(&table).await?;
            let (hash, rng) = key_names(d.key_schema());
            keys = vec![hash.clone(), rng];
            let mut pk = hash;
            if !index.is_empty() {
                let Some((ih, ir)) = index_keys(&d, &index) else {
                    return Err(Error::msg(format!(
                        "dynamodb: index {} not found on table {}",
                        idb_core::project::go_quote(&index),
                        idb_core::project::go_quote(&table)
                    )));
                };
                pk = ih.clone();
                keys.extend([ih, ir]);
            }
            ok = has_key_condition(&toks, &pk);
        }
        if !ok && !q.force {
            return Err(Error::NeedsConfirm(FULL_SCAN.into()));
        }
        let deadline = q.deadline.unwrap_or_else(|| tokio::time::Instant::now() + RUN_TIMEOUT);
        let (items, stop) = self.pages(&q.text, limit, deadline).await?;
        let mut res = to_result(items, limit, &keys);
        if let Some(stop) = stop {
            res.truncated = true;
            res.text = stop;
        }
        res.elapsed = start.elapsed();
        Ok(res)
    }

    /// NextToken 을 이어 limit+1 개(잘림 가르기)까지 받는다. MAX_PAGES 나 마감에 닿으면 받은 데까지 돌려주고
    /// 까닭을 싣는다. 받은 것이 없는데 마감이 지났으면 오류다.
    #[allow(clippy::type_complexity)]
    async fn pages(
        &self,
        stmt: &str,
        limit: usize,
        deadline: tokio::time::Instant,
    ) -> Result<(Vec<HashMap<String, AttributeValue>>, Option<String>), Error> {
        let mut items: Vec<HashMap<String, AttributeValue>> = Vec::new();
        let mut next = None;
        for page in 0.. {
            if page == MAX_PAGES {
                return Ok((items, Some(format!("페이지 {MAX_PAGES}개에서 멈춤 (키 조건으로 좁히세요)"))));
            }
            let want = (limit - items.len() + 1).min(i32::MAX as usize) as i32;
            match tokio::time::timeout_at(deadline, self.cl.execute_statement(stmt, want, next.take())).await {
                Err(_) if !items.is_empty() => return Ok((items, Some("시간 초과로 멈춤 (키 조건으로 좁히세요)".into()))),
                Err(_) => return Err(Error::msg("dynamodb ExecuteStatement: context deadline exceeded")),
                Ok(Err(e)) => return Err(wrap("ExecuteStatement", e)),
                Ok(Ok((got, token))) => {
                    items.extend(got);
                    if items.len() > limit || token.is_none() {
                        return Ok((items, None));
                    }
                    next = token;
                }
            }
        }
        unreachable!()
    }

    async fn describe(&self, table: &str) -> Result<TableDescription, Error> {
        let d = self.cl.describe_table(table).await.map_err(|e| wrap("DescribeTable", e))?;
        let d = d.unwrap_or_else(|| TableDescription::builder().table_name(table).build());
        self.tables.lock().unwrap().insert(table.to_string(), d.clone());
        Ok(d)
    }

    /// 대시보드 확인이다(R64.2): ListTables 로 테이블 수만 센다. 지연은 첫 페이지 왕복이다. 서버 버전이 없어 version 은 빈다.
    pub async fn probe(&self) -> Health {
        let start = Instant::now();
        let mut latency = Duration::ZERO;
        let mut tables = 0i64;
        let mut token = None;
        loop {
            let r = self.cl.list_tables(token).await;
            if latency.is_zero() {
                latency = start.elapsed();
            }
            match r {
                Err(e) => return failed(latency, &wrap("ListTables", e).to_string(), ""),
                Ok((names, next)) => {
                    tables += names.len() as i64;
                    match next {
                        Some(n) => token = Some(n),
                        None => break,
                    }
                }
            }
        }
        Health {
            ok: true,
            latency,
            metrics: vec![Metric::new("테이블", korean_count(tables), false)],
            ..Default::default()
        }
    }

    /// PartiQL SELECT text 의 결과를 서버에서 정렬할 수 있으면 그 정렬 키다(R63.3). WHERE 가 파티션 키를 = 로 고정해야 한다.
    /// 키는 run 이 읽어 둔 DescribeTable 을 써서 네트워크를 쓰지 않는다. 정렬할 수 없으면 Err 에 까닭(한국어)이 있다.
    pub fn sort_key(&self, text: &str) -> Result<String, &'static str> {
        let toks = tokenize(text);
        let Some((table, index)) = target(&toks).filter(|_| is_select(&toks)) else {
            return Err("SELECT 가 아님");
        };
        let Some(d) = self.tables.lock().unwrap().get(&table).cloned() else {
            return Err("키 정보 없음");
        };
        let (mut pk, mut sk) = key_names(d.key_schema());
        if !index.is_empty() {
            (pk, sk) = index_keys(&d, &index).ok_or("키 정보 없음")?;
        }
        if sk.is_empty() {
            return Err("정렬 키 없는 테이블");
        }
        if !key_condition(&toks, &pk, false) {
            return Err("파티션 키 = 조건 없음");
        }
        Ok(sk)
    }
}

/// SELECT text 의 맨 바깥 ORDER BY 를 `ORDER BY "<sk>" ASC|DESC` 로 바꾸거나 붙인 글이다(R63.3).
pub fn sorted(text: &str, sk: &str, desc: bool) -> Option<String> {
    sorted_statement(&tokenize(text), text, sk, desc)
}

impl<A: Api> source::Backend for Backend<A> {
    fn children<'a>(&'a self, path: &'a str) -> BoxFut<'a, Result<Vec<Node>, Error>> {
        Box::pin(Backend::children(self, path))
    }

    fn run<'a>(&'a self, q: &'a Query) -> BoxFut<'a, Result<QueryResult, Error>> {
        Box::pin(Backend::run(self, q))
    }

    fn open<'a>(&'a self, _path: &'a str, _limit: usize) -> BoxFut<'a, Result<QueryResult, Error>> {
        Box::pin(async move { Backend::open(self) })
    }

    fn probe(&self) -> Option<BoxFut<'_, Health>> {
        Some(Box::pin(Backend::probe(self)))
    }
}

// ─── 트리·키 ───

fn table_nodes(d: &TableDescription) -> Vec<Node> {
    let table = d.table_name().unwrap_or_default();
    let attr_type: HashMap<&str, &str> = d
        .attribute_definitions()
        .iter()
        .map(|a| (a.attribute_name(), a.attribute_type().as_str()))
        .collect();
    let mut out = vec![Node {
        name: "항목 수".into(),
        path: format!("{table}/#items"),
        info: format!("≈{}", d.item_count().unwrap_or(0)),
        ..Default::default()
    }];
    for k in d.key_schema() {
        let role = if *k.key_type() == KeyType::Range { "SK" } else { "PK" };
        let name = k.attribute_name();
        out.push(Node {
            name: format!("{role} {name}"),
            path: format!("{table}/{role}"),
            info: attr_type.get(name).map(|s| s.to_string()).unwrap_or_default(),
            ..Default::default()
        });
    }
    for g in d.global_secondary_indexes() {
        out.push(index_node(table, "gsi", g.index_name().unwrap_or_default(), g.key_schema()));
    }
    for l in d.local_secondary_indexes() {
        out.push(index_node(table, "lsi", l.index_name().unwrap_or_default(), l.key_schema()));
    }
    out
}

fn index_node(table: &str, kind: &str, name: &str, ks: &[KeySchemaElement]) -> Node {
    let (hash, rng) = key_names(ks);
    let info = if rng.is_empty() { hash } else { format!("{hash}/{rng}") };
    Node {
        name: format!("{kind} {name}"),
        path: format!("{table}/{kind}/{name}"),
        info,
        ..Default::default()
    }
}

fn key_names(ks: &[KeySchemaElement]) -> (String, String) {
    let (mut hash, mut rng) = (String::new(), String::new());
    for k in ks {
        if *k.key_type() == KeyType::Hash {
            hash = k.attribute_name().to_string();
        } else {
            rng = k.attribute_name().to_string();
        }
    }
    (hash, rng)
}

fn index_keys(d: &TableDescription, index: &str) -> Option<(String, String)> {
    d.global_secondary_indexes()
        .iter()
        .find(|g| g.index_name() == Some(index))
        .map(|g| key_names(g.key_schema()))
        .or_else(|| {
            d.local_secondary_indexes()
                .iter()
                .find(|l| l.index_name() == Some(index))
                .map(|l| key_names(l.key_schema()))
        })
}

// ─── 결과 ───

/// 항목을 표로 편다. 열은 keys(테이블·인덱스 키, 항목에 있는 것만) 먼저, 나머지 속성 이름순이다.
/// 없는 속성은 NULL 칸이다. types 는 열마다 처음 본 DynamoDB 타입 글자다(S·N·B·BOOL·NULL·M·L·SS·NS·BS).
fn to_result(mut items: Vec<HashMap<String, AttributeValue>>, limit: usize, keys: &[String]) -> QueryResult {
    let mut res = QueryResult::default();
    if items.len() > limit {
        items.truncate(limit);
        res.truncated = true;
    }
    let mut seen: Vec<&String> = items.iter().flat_map(|it| it.keys()).collect();
    seen.sort();
    seen.dedup();
    for k in keys {
        if !k.is_empty() && seen.contains(&k) && !res.cols.contains(k) {
            res.cols.push(k.clone());
        }
    }
    let rest: Vec<String> = seen.into_iter().filter(|k| !res.cols.contains(k)).cloned().collect();
    res.cols.extend(rest);
    let mut types = vec![String::new(); res.cols.len()];
    for it in &items {
        let row = res
            .cols
            .iter()
            .enumerate()
            .map(|(i, c)| match it.get(c) {
                None => NULL_TEXT.to_string(),
                Some(v) => {
                    let (typ, text) = cell(v);
                    if types[i].is_empty() {
                        types[i] = typ.into();
                    }
                    cap_cell(text.as_bytes())
                }
            })
            .collect();
        res.rows.push(row);
    }
    res.types = Some(types);
    res
}

fn b64(b: &[u8]) -> String {
    aws_smithy_types::base64::encode(b)
}

/// 값 하나의 타입 글자와 보일 글자다. 숫자는 원문, M·L·집합은 DynamoDB JSON 을 푼 JSON 이다(R63.2).
fn cell(v: &AttributeValue) -> (&'static str, String) {
    match v {
        AttributeValue::S(s) => ("S", s.clone()),
        AttributeValue::N(n) => ("N", n.clone()),
        AttributeValue::Bool(b) => ("BOOL", b.to_string()),
        AttributeValue::Null(_) => ("NULL", NULL_TEXT.into()),
        AttributeValue::B(b) => ("B", b64(b.as_ref())),
        AttributeValue::M(_) => ("M", to_json(v)),
        AttributeValue::L(_) => ("L", to_json(v)),
        AttributeValue::Ss(_) => ("SS", to_json(v)),
        AttributeValue::Ns(_) => ("NS", to_json(v)),
        AttributeValue::Bs(_) => ("BS", to_json(v)),
        _ => ("?", "unknown".into()),
    }
}

/// Go encoding/json 이 json.Number 로 받는 꼴인지다(-?(0|[1-9]\d*)(\.\d+)?([eE][+-]?\d+)?).
fn valid_number(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = usize::from(b.first() == Some(&b'-'));
    let digits = |i: &mut usize| {
        let st = *i;
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
        *i - st
    };
    match b.get(i) {
        Some(b'0') => i += 1,
        Some(c) if c.is_ascii_digit() => {
            digits(&mut i);
        }
        _ => return false,
    }
    if b.get(i) == Some(&b'.') {
        i += 1;
        if digits(&mut i) == 0 {
            return false;
        }
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        if digits(&mut i) == 0 {
            return false;
        }
    }
    i == b.len()
}

/// DynamoDB JSON 을 보통 JSON 으로 쓴다. 숫자는 원문 그대로(정밀도 보존), 맵 키는 이름순, < > & 는 그대로다.
fn write_plain(out: &mut String, v: &AttributeValue) -> Result<(), String> {
    let num = |out: &mut String, n: &str| {
        if !valid_number(n) {
            return Err(format!("json: invalid number literal {}", idb_core::project::go_quote(n)));
        }
        out.push_str(n);
        Ok(())
    };
    match v {
        AttributeValue::S(s) => gojson::write_str(out, s),
        AttributeValue::N(n) => num(out, n)?,
        AttributeValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        AttributeValue::B(b) => gojson::write_str(out, &b64(b.as_ref())),
        AttributeValue::M(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                gojson::write_str(out, k);
                out.push(':');
                write_plain(out, &m[*k])?;
            }
            out.push('}');
        }
        AttributeValue::L(l) => {
            out.push('[');
            for (i, e) in l.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_plain(out, e)?;
            }
            out.push(']');
        }
        AttributeValue::Ss(ss) => gojson::str_array(out, ss.iter().map(String::as_str)),
        AttributeValue::Ns(ns) => {
            out.push('[');
            for (i, n) in ns.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                num(out, n)?;
            }
            out.push(']');
        }
        AttributeValue::Bs(bs) => {
            let enc: Vec<String> = bs.iter().map(|b| b64(b.as_ref())).collect();
            gojson::str_array(out, enc.iter().map(String::as_str));
        }
        _ => out.push_str("null"),
    }
    Ok(())
}

/// 한 줄 JSON 이다.
fn to_json(v: &AttributeValue) -> String {
    let mut out = String::new();
    match write_plain(&mut out, v) {
        Ok(()) => out,
        Err(e) => {
            let mut s = String::new();
            let _ = write!(s, "<{e}>");
            s
        }
    }
}

#[cfg(test)]
mod tests;
