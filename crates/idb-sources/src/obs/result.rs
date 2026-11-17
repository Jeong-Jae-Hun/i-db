//! Loki·Prometheus query·query_range 응답을 결과 표로 편다.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, FixedOffset};
use idb_core::gojson::json_error;
use idb_core::project::go_quote;
use idb_core::result::{QueryResult, cap_cell};
use serde::Deserialize;
use serde_json::value::RawValue;

/// 결과 time 열 형식이다. chart(R28)의 "2006-01-02 15:04:05.999999999" 로 읽힌다
const TIME_FORMAT: &str = "%Y-%m-%d %H:%M:%S%.3f";

/// 유닉스 나노초를 KST `2026-11-15 12:00:00.250` 으로 쓴다(밀리초 아래는 버린다).
pub(super) fn kst_time(ns: i64) -> String {
    let kst = FixedOffset::east_opt(9 * 3600).expect("KST offset");
    DateTime::from_timestamp(ns.div_euclid(1_000_000_000), ns.rem_euclid(1_000_000_000) as u32)
        .map(|t| t.with_timezone(&kst).format(TIME_FORMAT).to_string())
        .unwrap_or_default()
}

/// query·query_range 의 data 다.
#[derive(Deserialize)]
pub(super) struct QueryData {
    #[serde(default, rename = "resultType", deserialize_with = "crate::opensearch::nullable")]
    pub result_type: String,
    #[serde(default)]
    result: Option<Box<RawValue>>,
}

struct LabeledRow {
    at: i64,
    labels: BTreeMap<String, String>,
    /// line 또는 value
    last: String,
}

/// resultType 에 맞춰 표로 바꾼다. 로그는 time·라벨·line(최신 먼저),
/// 지표는 time·라벨·value(시계열끼리 묶고 그 안은 시간 순)이다. limit 을 넘는 행은 자르고 truncated 를 켠다.
/// asc 면 로그 줄을 오래된 것부터 둔다(Loki forward, R62.3). 지표는 늘 시계열별 서버 순서다.
pub(super) fn to_result(d: &QueryData, limit: usize, asc: bool) -> Result<QueryResult, String> {
    let raw = d.result.as_ref().map_or("", |r| r.get());
    let (rows, last) = match d.result_type.as_str() {
        "streams" => (stream_rows(raw, asc), "line"),
        "matrix" | "vector" => (series_rows(raw), "value"),
        "scalar" | "string" => (
            sample(raw).map(|(at, last)| {
                vec![LabeledRow {
                    at,
                    labels: BTreeMap::new(),
                    last,
                }]
            }),
            "value",
        ),
        other => return Err(format!("unsupported result type {}", go_quote(other))),
    };
    let rows = rows.map_err(|e| format!("unexpected {} result: {e}", d.result_type))?;
    Ok(table(rows, last, limit))
}

fn table(mut rows: Vec<LabeledRow>, last: &str, limit: usize) -> QueryResult {
    let labels: BTreeSet<String> = rows.iter().flat_map(|r| r.labels.keys().cloned()).collect();
    let mut res = QueryResult {
        cols: std::iter::once("time".to_string())
            .chain(labels.iter().cloned())
            .chain(std::iter::once(last.to_string()))
            .collect(),
        ..Default::default()
    };
    if rows.len() > limit {
        rows.truncate(limit);
        res.truncated = true;
    }
    for r in rows {
        let mut row = Vec::with_capacity(res.cols.len());
        row.push(kst_time(r.at));
        for l in &labels {
            row.push(cap_cell(r.labels.get(l).map_or("", String::as_str).as_bytes()));
        }
        row.push(cap_cell(r.last.as_bytes()));
        res.rows.push(row);
    }
    res
}

fn parse<'a, T: Deserialize<'a>>(raw: &'a str) -> Result<T, String> {
    serde_json::from_str(raw).map_err(|e| json_error(&e))
}

fn stream_rows(raw: &str, asc: bool) -> Result<Vec<LabeledRow>, String> {
    #[derive(Deserialize)]
    struct Stream {
        #[serde(default, deserialize_with = "crate::opensearch::null_default")]
        stream: BTreeMap<String, String>,
        /// [ns 문자열, 줄, (구조화 메타데이터)]
        #[serde(default, deserialize_with = "crate::opensearch::null_default")]
        values: Vec<Vec<Box<RawValue>>>,
    }
    let streams: Option<Vec<Stream>> = parse(raw)?;
    let mut rows = Vec::new();
    for s in streams.unwrap_or_default() {
        for v in &s.values {
            if v.len() < 2 {
                return Err(format!("log entry has {} fields", v.len()));
            }
            let ns: String = parse(v[0].get())?;
            let line: String = parse(v[1].get())?;
            let at = ns.parse::<i64>().map_err(|_| format!("bad timestamp {}", go_quote(&ns)))?;
            rows.push(LabeledRow {
                at,
                labels: s.stream.clone(),
                last: line,
            });
        }
    }
    // 스트림마다 따로 오므로 합친 뒤 최신 먼저(asc 면 오래된 것 먼저)로 다시 줄 세운다
    if asc {
        rows.sort_by_key(|r| r.at);
    } else {
        rows.sort_by_key(|r| std::cmp::Reverse(r.at));
    }
    Ok(rows)
}

/// [초(소수), "값"] 쌍을 (유닉스 나노초, 값)으로 읽는다. 시각은 밀리초로 반올림한다.
fn sample(raw: &str) -> Result<(i64, String), String> {
    let pair: Vec<Box<RawValue>> = parse(raw)?;
    if pair.len() != 2 {
        return Err(format!("sample has {} fields", pair.len()));
    }
    let sec: f64 = parse(pair[0].get())?;
    let val: String = parse(pair[1].get())?;
    Ok(((sec * 1000.0).round() as i64 * 1_000_000, val))
}

/// 시계열을 라벨 순으로 묶고, 각 시계열 안은 서버가 준 시간 순 그대로 둔다.
fn series_rows(raw: &str) -> Result<Vec<LabeledRow>, String> {
    #[derive(Deserialize)]
    struct Series {
        #[serde(default, deserialize_with = "crate::opensearch::null_default")]
        metric: BTreeMap<String, String>,
        #[serde(default, deserialize_with = "crate::opensearch::null_default")]
        values: Vec<Box<RawValue>>,
        #[serde(default)]
        value: Option<Box<RawValue>>,
    }
    let ss: Option<Vec<Series>> = parse(raw)?;
    let mut ss = ss.unwrap_or_default();
    ss.sort_by_cached_key(|s| label_key(&s.metric));
    let mut rows = Vec::new();
    for s in ss {
        for v in s.values.iter().chain(&s.value) {
            let (at, last) = sample(v.get())?;
            rows.push(LabeledRow {
                at,
                labels: s.metric.clone(),
                last,
            });
        }
    }
    Ok(rows)
}

/// 라벨 묶음을 이름순 "k=v," 로 이어 시계열 정렬 키로 쓴다.
fn label_key(m: &BTreeMap<String, String>) -> String {
    m.iter().map(|(k, v)| format!("{k}={v},")).collect()
}

/// count(...) 즉시 질의 결과의 첫 시계열 값이다. 맞는 시계열이 없으면 None 이다(Prometheus 확인).
pub(super) fn first_vector_value(raw: &str) -> Result<Option<String>, String> {
    #[derive(Deserialize)]
    struct Series {
        #[serde(default)]
        value: Option<Box<RawValue>>,
    }
    #[derive(Deserialize)]
    struct Data {
        #[serde(default, deserialize_with = "crate::opensearch::null_default")]
        result: Vec<Series>,
    }
    let d: Data = parse(raw)?;
    match d.result.first().and_then(|s| s.value.as_ref()) {
        Some(v) => sample(v.get()).map(|(_, val)| Some(val)),
        None => Ok(None),
    }
}
