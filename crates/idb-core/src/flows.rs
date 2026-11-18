//! 이벤트 흐름(아웃박스 테이블 → SNS 토픽 → SQS 큐 → DLQ·Lambda, 또는 아웃박스 → 종류별 소비자) 설정과 아웃박스 수 질의(R69).
//! Go `internal/flows` 다. 토픽·큐·DLQ 는 설정에 적은 이름이고 AWS 에 묻는 것은 큐를 읽는 Lambda 매핑뿐이다(R72).
//! 설정은 프로젝트의 `.dbtui/flows.json` 이고 없으면 빈 목록이다. 질의를 보내는 쪽은 CLI 가 맡는다.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

use serde::Deserialize;

use crate::project::{Conn, Kind, go_quote};
use crate::result::{NULL_TEXT, QueryResult};
use crate::sqlsafe::{mask_sql, quote_ident, quote_literal, split_statements};

/// 프로젝트 폴더 기준 설정 파일 자리다.
pub const FILE: &str = ".dbtui/flows.json";
/// due 열이 이만큼(분) 지난 행을 `오래됨` 으로 센다. 종류별 stale 이 있으면 그것이다.
pub const STALE_MINUTES: i64 = 5;
/// limit 을 주지 않은 아웃박스의 재시도 한도다.
pub const DEFAULT_LIMIT: i64 = 5;
/// 종류 상세(재시도 중·한도 소진 행)의 최대 행 수다.
pub const RETRY_ROWS: usize = 50;
/// 종류별 줄을 받는 최대 수다. 합은 서버의 합계 행으로 내므로 넘어도 정확하고, 줄만 잘린다.
pub const MAX_TYPES: usize = 1000;
const TRUNCATED_NOTE: &str = "종류 1,000개만";

/// 흐름 하나다. outbox·topic·queues 가운데 하나는 있어야 한다. err 는 검사에서 걸린 까닭이고 있으면 풀지 않는다.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Flow {
    pub name: String,
    pub outbox: Option<Outbox>,
    pub aws: String,
    pub topic: String,
    #[serde(deserialize_with = "null_vec")]
    pub queues: Vec<Queue>,
    #[serde(skip)]
    pub err: String,
}

/// 토픽을 구독한 SQS 큐다. name 은 이름 또는 ARN, dlq 는 선택이다. 둘 다 보이기만 한다.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Queue {
    pub name: String,
    pub dlq: String,
}

/// 아웃박스 테이블이다. table 은 `스키마.테이블` 또는 `테이블`, type·retry·due 는 열 이름이다.
/// where 는 사용자 설정 파일의 SQL 조각이라 한 문장·`;` 없음만 받는다. limit 이 Option 인 까닭은 주지 않음과 잘못 준 0 을 가르기 위해서다.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Outbox {
    pub conn: String,
    pub table: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub retry: String,
    pub due: String,
    #[serde(rename = "where")]
    pub filter: String,
    pub limit: Option<i64>,
    #[serde(deserialize_with = "null_map")]
    pub types: BTreeMap<String, TypeConf>,
}

/// 이벤트 종류 하나의 설정이다. limit 이 없으면 아웃박스 한도, consumer 는 그 종류를 가져가는 잡 이름(보이기만 한다)이다.
/// stale 은 `오래됨` 기준(분)이다. 잡 주기가 기본 기준보다 긴 종류가 늘 경고로 뜨지 않게 한다.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct TypeConf {
    pub limit: Option<i64>,
    pub consumer: String,
    pub stale: Option<i64>,
}

fn null_vec<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Vec<T>, D::Error> {
    Ok(Option::<Vec<T>>::deserialize(d)?.unwrap_or_default())
}

fn null_map<'de, D: serde::Deserializer<'de>>(d: D) -> Result<BTreeMap<String, TypeConf>, D::Error> {
    Ok(Option::<BTreeMap<String, TypeConf>>::deserialize(d)?.unwrap_or_default())
}

impl Outbox {
    /// 기본 재시도 한도다.
    pub fn limit(&self) -> i64 {
        self.limit.unwrap_or(DEFAULT_LIMIT)
    }
}

/// project_dir 의 설정을 읽는다. 파일이 없으면 빈 목록이다. JSON 이 깨졌으면 오류이고, 항목 하나가 틀리면 그 항목 err 에만
/// 까닭을 적어 나머지는 그대로 쓴다. conns 로 접속 이름·종류를 검사한다.
pub fn load(project_dir: &Path, conns: &[Conn]) -> Result<Vec<Flow>, String> {
    let data = match fs::read(project_dir.join(FILE)) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.to_string()),
    };
    let mut fs: Vec<Flow> = serde_json::from_slice::<Option<Vec<Flow>>>(&data)
        .map_err(|e| format!("{FILE}: {}", crate::gojson::json_error(&e)))?
        .unwrap_or_default();
    let mut seen = std::collections::HashSet::new();
    for (i, f) in fs.iter_mut().enumerate() {
        if let Err(e) = validate(f, conns) {
            f.err = e;
        } else if seen.contains(&f.name) {
            f.err = format!("duplicate name {}", f.name);
        }
        seen.insert(f.name.clone());
        if f.name.is_empty() {
            f.name = format!("#{}", i + 1);
        }
    }
    Ok(fs)
}

fn validate(f: &Flow, conns: &[Conn]) -> Result<(), String> {
    if f.name.trim().is_empty() {
        return Err("name is empty".into());
    }
    if f.outbox.is_none() && f.topic.trim().is_empty() && f.queues.is_empty() {
        return Err("outbox, topic or queues is required".into());
    }
    if f.queues.is_empty() && !f.aws.is_empty() {
        return Err("aws requires queues".into());
    }
    if !f.queues.is_empty() {
        check_conn(conns, &f.aws, Kind::Aws, "aws")?;
    }
    if let Some(i) = f.queues.iter().position(|q| q.name.trim().is_empty()) {
        return Err(format!("queues[{i}].name is empty"));
    }
    let Some(o) = &f.outbox else { return Ok(()) };
    check_conn(conns, &o.conn, Kind::Pg, "outbox.conn")?;
    for (field, v) in [("outbox.table", &o.table), ("outbox.type", &o.kind)] {
        if v.trim().is_empty() {
            return Err(format!("{field} is empty"));
        }
    }
    if o.table.split('.').count() > 2 || o.table.starts_with('.') || o.table.ends_with('.') {
        return Err(format!("outbox.table must be table or schema.table: {}", go_quote(&o.table)));
    }
    // 한도는 1 이상, 종류 이름은 비지 않아야 하고, 한도·종류 설정은 retry 열이 있어야 뜻이 있다
    if o.retry.is_empty() && o.limit.is_some() {
        return Err("outbox.limit requires outbox.retry".into());
    }
    if o.retry.is_empty() && !o.types.is_empty() {
        return Err("outbox.types requires outbox.retry".into());
    }
    if o.limit.is_some_and(|l| l < 1) {
        return Err("outbox.limit must be >= 1".into());
    }
    for (k, t) in &o.types {
        if k.trim().is_empty() {
            return Err("outbox.types has an empty type".into());
        }
        if t.limit.is_some_and(|l| l < 1) {
            return Err(format!("outbox.types.{k}.limit must be >= 1"));
        }
        if t.stale.is_some_and(|m| m < 1) {
            return Err(format!("outbox.types.{k}.stale must be >= 1"));
        }
        if t.stale.is_some() && o.due.is_empty() {
            return Err(format!("outbox.types.{k}.stale requires outbox.due"));
        }
    }
    check_where(&o.filter)
}

/// name 이 kind 접속인지다. 없는 이름과 다른 종류를 가른다.
fn check_conn(conns: &[Conn], name: &str, kind: Kind, field: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err(format!("{field} is empty"));
    }
    match conns.iter().find(|c| c.name == name && c.kind.is_some()) {
        Some(c) if c.kind != Some(kind) => Err(format!("{field} {name}: want kind {kind}, got {}", c.kind_str())),
        Some(_) => Ok(()),
        None => Err(format!("{field} {name}: no such connection")),
    }
}

/// where 조각이 문장 하나이고 문자열·주석 밖에 `;` 가 없는지다(다른 문장을 이어 붙이지 못하게).
fn check_where(filter: &str) -> Result<(), String> {
    if filter.trim().is_empty() {
        return Ok(());
    }
    if split_statements(filter).len() != 1 || mask_sql(filter).contains(';') {
        return Err("outbox.where must be a single condition without ';'".into());
    }
    Ok(())
}

// ─── 아웃박스 질의 ───

/// `스키마.테이블` 을 마디마다 따옴표로 감싼다.
fn quote_table(t: &str) -> String {
    t.split('.').map(quote_ident).collect::<Vec<_>>().join(".")
}

/// 조건을 AND 로 잇고 where 조각을 붙인다. 조각 끝의 `--` 주석이 뒤 글을 먹지 않게 줄을 바꿔 괄호로 감싼다.
fn filter(o: &Outbox, mut conds: Vec<String>) -> String {
    if !o.filter.trim().is_empty() {
        conds.push(format!("(\n{}\n)", o.filter));
    }
    if conds.is_empty() {
        return String::new();
    }
    format!(" where {}", conds.join(" and "))
}

/// 종류 열을 글로 바꾼다. enum 열이면 설정에만 있는 종류 이름 리터럴이 형 오류를 내지 않게 한다.
fn type_text(o: &Outbox) -> String {
    format!("{}::text", quote_ident(&o.kind))
}

/// NULL 재시도를 0 으로 본다(아직 한 번도 안 돈 행).
fn retry_expr(o: &Outbox) -> String {
    format!("coalesce({}, 0)", quote_ident(&o.retry))
}

/// 행마다의 재시도 한도다. 종류별 한도가 있으면 `case <type> when 'X' then 20 … else <limit> end` 이고 종류 이름 순이다.
fn limit_expr(o: &Outbox) -> String {
    let def = o.limit().to_string();
    let whens: String = o
        .types
        .iter()
        .filter_map(|(k, t)| t.limit.map(|l| format!(" when {} then {l}", quote_literal(k))))
        .collect();
    if whens.is_empty() {
        def
    } else {
        format!("case {}{whens} else {def} end", type_text(o))
    }
}

/// 행마다의 `오래됨` 기준이다. 종류별 stale 이 있으면 `case <type> when 'X' then interval '70 minutes' … else interval '5 minutes' end` 다.
fn stale_expr(o: &Outbox) -> String {
    let def = format!("interval '{STALE_MINUTES} minutes'");
    let whens: String = o
        .types
        .iter()
        .filter_map(|(k, t)| t.stale.map(|m| format!(" when {} then interval '{m} minutes'", quote_literal(k))))
        .collect();
    if whens.is_empty() {
        def
    } else {
        format!("case {}{whens} else {def} end", type_text(o))
    }
}

/// 종류별 수와 합계를 한 문장으로 묻는다(type·rows·waiting·retrying·exhausted·stale·oldest_due·type_null·total).
/// `grouping sets ((type), ())` 의 합계 행(total = 1)이 맨 앞이라 종류가 결과 행 상한을 넘어도 합은 정확하다.
pub fn count_sql(o: &Outbox) -> String {
    let (mut waiting, mut retrying, mut exhausted, mut stale, mut oldest) = (
        "count(*)".to_string(),
        "null::bigint".to_string(),
        "null::bigint".to_string(),
        "null::bigint".to_string(),
        "null::timestamptz".to_string(),
    );
    let mut live = String::new();
    if !o.retry.is_empty() {
        let (r, l) = (retry_expr(o), limit_expr(o));
        live = format!("{r} < {l}");
        waiting = format!("count(*) filter (where {live})");
        retrying = format!("count(*) filter (where {r} > 0 and {live})");
        exhausted = format!("count(*) filter (where {r} >= {l})");
    }
    if !o.due.is_empty() {
        let due = quote_ident(&o.due);
        let mut cond = format!("{due} < now() - {}", stale_expr(o));
        oldest = format!("min({due})");
        if !live.is_empty() {
            cond.push_str(&format!(" and {live}"));
            oldest.push_str(&format!(" filter (where {live})"));
        }
        stale = format!("count(*) filter (where {cond})");
    }
    let typ = type_text(o);
    format!(
        "select {typ} as type, count(*) as rows, {waiting} as waiting, {retrying} as retrying, {exhausted} as exhausted, {stale} as stale, {oldest} as oldest_due, {typ} is null as type_null, grouping({typ}) as total from {}{} group by grouping sets (({typ}), ()) order by 9 desc, 5 desc nulls last, 3 desc, 1",
        quote_table(&o.table),
        filter(o, Vec::new())
    )
}

/// 종류 하나의 재시도 중·한도 소진 행(retry > 0) 최근 RETRY_ROWS 개다. retry 가 없으면 그 종류의 행을 due 순으로 보인다.
/// NULL 종류는 `is null` 로 묻는다.
pub fn retry_sql(o: &Outbox, t: &TypeStat) -> String {
    let mut conds = vec![if t.null {
        format!("{} is null", type_text(o))
    } else {
        format!("{} = {}", type_text(o), quote_literal(&t.name))
    }];
    let mut order = Vec::new();
    if !o.retry.is_empty() {
        conds.push(format!("{} > 0", retry_expr(o)));
        order.push(format!("{} desc", retry_expr(o)));
    }
    if !o.due.is_empty() {
        order.push(quote_ident(&o.due));
    }
    let mut q = format!("select * from {}{}", quote_table(&o.table), filter(o, conds));
    if !order.is_empty() {
        q.push_str(&format!(" order by {}", order.join(", ")));
    }
    format!("{q} limit {RETRY_ROWS}")
}

/// 종류 하나의 수다. retrying·exhausted·stale 이 -1 이면 그 열을 주지 않았다. null 은 종류 열이 NULL 인 행이다.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TypeStat {
    pub name: String,
    pub null: bool,
    pub rows: i64,
    pub waiting: i64,
    pub retrying: i64,
    pub exhausted: i64,
    pub stale: i64,
    pub oldest_due: String,
}

impl TypeStat {
    /// 보일 종류 이름이다. NULL 종류는 `(NULL)` 이다.
    pub fn label(&self) -> String {
        if self.null { "(NULL)".into() } else { self.name.clone() }
    }

    /// 종류 줄의 `대기 3 · 재시도 1 · 한도 소진 2` 다. 오래됨은 있을 때만 붙인다. warn 은 한도 소진·오래됨이 0보다 클 때다.
    pub fn metrics(&self) -> (String, bool) {
        let mut parts = vec![format!("대기 {}", self.waiting)];
        if self.retrying >= 0 {
            parts.push(format!("재시도 {}", self.retrying));
        }
        if self.exhausted >= 0 {
            parts.push(format!("한도 소진 {}", self.exhausted));
        }
        if self.stale > 0 {
            parts.push(format!("오래됨 {}", self.stale));
        }
        (parts.join(" · "), self.exhausted > 0 || self.stale > 0)
    }
}

/// 아웃박스 수다(서버 합계). retry·exhausted·stale 이 -1 이면 그 열을 주지 않았다.
/// truncated 는 종류가 MAX_TYPES 를 넘어 types 가 잘렸다는 뜻이다(합은 그대로 정확하다).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stat {
    pub pending: i64,
    pub retry: i64,
    pub stale: i64,
    pub exhausted: i64,
    pub types: Vec<TypeStat>,
    pub truncated: bool,
}

impl Stat {
    /// `대기 120 · 재시도 3 · 한도 소진 2 · 오래됨 1` 이다. 주지 않은 열은 뺀다. warn 은 재시도·한도 소진·오래됨이 0보다 클 때다.
    pub fn metrics(&self) -> (String, bool) {
        let mut parts = vec![format!("대기 {}", self.pending)];
        if self.retry >= 0 {
            parts.push(format!("재시도 {}", self.retry));
        }
        if self.exhausted >= 0 {
            parts.push(format!("한도 소진 {}", self.exhausted));
        }
        if self.stale >= 0 {
            parts.push(format!("오래됨 {}", self.stale));
        }
        if self.truncated {
            parts.push(TRUNCATED_NOTE.into());
        }
        (parts.join(" · "), self.retry > 0 || self.stale > 0 || self.exhausted > 0)
    }
}

/// count_sql 결과(MAX_TYPES+1 행까지 받은 것)를 합계와 종류별 수로 읽는다.
pub fn parse_count(res: &QueryResult) -> Result<Stat, String> {
    if res.rows.first().is_none_or(|r| r.len() != 9 || r[8] != "1") {
        return Err("unexpected outbox count result".into());
    }
    let mut s = Stat::default();
    for (k, row) in res.rows.iter().enumerate() {
        let mut t = TypeStat {
            name: row[0].clone(),
            null: row[7] == "t",
            oldest_due: row[6].clone(),
            ..Default::default()
        };
        let mut nums = [0i64; 5];
        for (i, n) in nums.iter_mut().enumerate() {
            let v = &row[i + 1];
            *n = if v.is_empty() || v == NULL_TEXT {
                -1
            } else {
                v.parse::<i64>()
                    .map_err(|_| format!("outbox count: strconv.ParseInt: parsing {}: invalid syntax", go_quote(v)))?
            };
        }
        [t.rows, t.waiting, t.retrying, t.exhausted, t.stale] = nums;
        if k == 0 {
            (s.pending, s.retry, s.exhausted, s.stale) = (t.waiting, t.retrying, t.exhausted, t.stale);
            continue;
        }
        s.types.push(t);
    }
    s.truncated = res.truncated;
    Ok(s)
}

#[cfg(test)]
mod tests;
