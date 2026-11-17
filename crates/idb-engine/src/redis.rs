//! Redis 읽기·모니터·일괄 삭제·값 편집 명령. Go `internal/db` 의 backend.go(GetKeys·GetValue)·redisvalue.go·
//! redismon.go·rediskeys.go 와 `internal/redisview/editor.go` 의 명령 만들기와 같다.

use std::time::Instant;

use idb_core::result::{MAX_ROWS, QueryResult, cap_cell};
use idb_core::textutil::printable;
use idb_sources::Error;
use idb_sources::redis::{RedisConn, Value as RValue};
use serde::{Deserialize, Serialize};

use crate::protocol::{ResultSet, ScanResult};

/// Redis string 값은 앞 1MiB 만 보여 준다(E63)
const MAX_STRING_BYTES: i64 = 1 << 20;

// ─── 읽기 (Go GetKeys·GetValue) ───

pub(crate) fn text(v: &RValue) -> String {
    match v {
        RValue::Str(b) => String::from_utf8_lossy(b).into_owned(),
        RValue::Int(n) => n.to_string(),
        other => idb_sources::redis::format_cell(other),
    }
}

fn cell(v: &RValue) -> String {
    match v {
        RValue::Str(b) => cap_cell(b),
        other => idb_sources::redis::format_cell(other),
    }
}

fn int(v: &RValue) -> i64 {
    match v {
        RValue::Int(n) => *n,
        other => text(other).parse().unwrap_or(0),
    }
}

/// SCAN 답(커서, 원소들)을 읽는다.
fn scan_reply(v: RValue) -> Result<(String, Vec<RValue>), Error> {
    match v {
        RValue::Array(mut a) if a.len() == 2 => {
            let items = match a.pop() {
                Some(RValue::Array(items)) => items,
                Some(RValue::Map(pairs)) => pairs.into_iter().flat_map(|(k, v)| [k, v]).collect(),
                _ => Vec::new(),
            };
            Ok((text(&a[0]), items))
        }
        _ => Err(Error::msg("unexpected SCAN reply")),
    }
}

pub(crate) fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

pub(crate) async fn scan_keys(r: &mut RedisConn, pattern: String) -> Result<ScanResult, Error> {
    let (mut keys, mut more, mut cursor) = (Vec::new(), false, "0".to_string());
    loop {
        let (next, batch) = scan_reply(r.value(&args(&["SCAN", &cursor, "MATCH", &pattern, "COUNT", "500"])).await?)?;
        keys.extend(batch.iter().map(text));
        cursor = next;
        if cursor == "0" {
            break;
        }
        if keys.len() >= MAX_ROWS {
            more = true;
            break;
        }
    }
    if keys.len() > MAX_ROWS {
        keys.truncate(MAX_ROWS);
        more = true;
    }
    // SCAN 은 리해시 중 같은 키를 두 번 줄 수 있다
    keys.sort();
    keys.dedup();
    Ok(ScanResult { keys, more })
}

/// Go HumanBytes.
fn human_bytes(n: i64) -> String {
    if n < 1024 {
        return format!("{n}B");
    }
    let mut f = n as f64;
    for u in ["KB", "MB", "GB", "TB"] {
        f /= 1024.0;
        if f < 1024.0 || u == "TB" {
            return format!("{f:.1}{u}");
        }
    }
    String::new()
}

pub(crate) async fn get_value(r: &mut RedisConn, key: String) -> Result<ResultSet, Error> {
    let start = Instant::now();
    let typ = text(&r.value(&args(&["TYPE", &key])).await?);
    let ttl = int(&r.value(&args(&["TTL", &key])).await?);
    let ttl_text = if ttl >= 0 {
        idb_core::gostrconv::duration_string(ttl.saturating_mul(1_000_000_000))
    } else {
        "∞".into()
    };
    let mut res = QueryResult {
        text: format!("{key} · {typ} · TTL {ttl_text}"),
        ..Default::default()
    };
    match r.value(&args(&["MEMORY", "USAGE", &key])).await {
        Ok(RValue::Nil) => {}
        Ok(v) => res.text.push_str(&format!(" · 메모리 {}", human_bytes(int(&v)))),
        // 관리형 서버가 명령을 막았거나 모른다
        Err(Error::Other(e)) if e.contains("unknown command") || e.contains("unknown subcommand") || e.starts_with("NOPERM") => {}
        Err(e) => return Err(e),
    }
    let max = MAX_ROWS.to_string();
    match typ.as_str() {
        "string" => {
            let size = int(&r.value(&args(&["STRLEN", &key])).await?);
            let v = text(
                &r.value(&args(&["GETRANGE", &key, "0", &(MAX_STRING_BYTES - 1).to_string()]))
                    .await?,
            );
            res.text.push_str(&format!("\n\n{}", idb_sources::redis::pretty_json(&v)));
            if size > MAX_STRING_BYTES {
                res.truncated = true;
                res.text
                    .push_str(&format!("\n\n… {size}바이트 중 앞 {MAX_STRING_BYTES}바이트만 보여 줌"));
            }
        }
        "hash" | "set" => {
            // HGETALL·SMEMBERS 는 큰 값에서 상한이 없어 HSCAN·SSCAN 으로 MAX_ROWS 근처까지만 읽는다
            let hash = typ == "hash";
            res.cols = if hash { args(&["field", "value"]) } else { args(&["member"]) };
            let mut cursor = "0".to_string();
            loop {
                let cmd = if hash { "HSCAN" } else { "SSCAN" };
                let (next, items) = scan_reply(r.value(&args(&[cmd, &key, &cursor, "MATCH", "*", "COUNT", "500"])).await?)?;
                if hash {
                    res.rows
                        .extend(items.as_chunks::<2>().0.iter().map(|[k, v]| vec![cell(k), cell(v)]));
                } else {
                    res.rows.extend(items.iter().map(|v| vec![cell(v)]));
                }
                cursor = next;
                if cursor == "0" || res.rows.len() >= MAX_ROWS {
                    res.truncated = cursor != "0";
                    break;
                }
            }
            res.rows.sort_by(|a, b| a[0].cmp(&b[0]));
        }
        "list" => {
            // 하나 더 받아 정확히 1,000개와 잘림을 가른다
            let RValue::Array(mut vs) = r.value(&args(&["LRANGE", &key, "0", &max])).await? else {
                return Err(Error::msg("unexpected LRANGE reply"));
            };
            if vs.len() > MAX_ROWS {
                vs.truncate(MAX_ROWS);
                res.truncated = true;
            }
            res.cols = args(&["#", "value"]);
            res.rows = vs.iter().enumerate().map(|(i, v)| vec![i.to_string(), cell(v)]).collect();
        }
        "zset" => {
            let mut pairs = zset_pairs(r.value(&args(&["ZRANGE", &key, "0", &max, "WITHSCORES"])).await?)?;
            if pairs.len() > MAX_ROWS {
                pairs.truncate(MAX_ROWS);
                res.truncated = true;
            }
            res.cols = args(&["member", "score"]);
            res.rows = pairs.iter().map(|(m, s)| vec![cell(m), score_text(s)]).collect();
        }
        "stream" => {
            let RValue::Array(xs) = r.value(&args(&["XRANGE", &key, "-", "+", "COUNT", "200"])).await? else {
                return Err(Error::msg("unexpected XRANGE reply"));
            };
            res.cols = args(&["id", "fields"]);
            for x in xs {
                let RValue::Array(parts) = x else { continue };
                let (Some(id), Some(RValue::Array(fv))) = (parts.first(), parts.get(1)) else {
                    continue;
                };
                // Go json.Marshal(map[string]any): 키 순서로 정렬한다
                let mut kv: Vec<(String, String)> = fv.as_chunks::<2>().0.iter().map(|[k, v]| (text(k), text(v))).collect();
                kv.sort();
                kv.dedup_by(|a, b| a.0 == b.0);
                let mut obj = String::from("{");
                for (i, (k, v)) in kv.iter().enumerate() {
                    if i > 0 {
                        obj.push(',');
                    }
                    idb_core::gojson::write_str(&mut obj, k);
                    obj.push(':');
                    idb_core::gojson::write_str(&mut obj, v);
                }
                obj.push('}');
                res.rows.push(vec![text(id), cap_cell(obj.as_bytes())]);
            }
        }
        "none" => res.text.push_str("\n\n(키 없음)"),
        _ => res.text.push_str("\n\n(보기 미지원 타입)"),
    }
    res.elapsed = start.elapsed();
    Ok(ResultSet::from_query(res))
}

// ─── 편집용 원값 (Go GetRawValue) ───

/// 편집할 키 하나의 잘리지 않은 값이다. string 은 1MiB, 컬렉션은 1,000개까지이고 넘으면 truncated 다.
/// ttl_secs 가 음수면 만료 없음이다.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RawValue {
    pub key: String,
    #[serde(rename = "type")]
    pub typ: String,
    pub ttl_secs: i64,
    #[serde(default)]
    pub string: String,
    #[serde(default)]
    pub fields: Vec<[String; 2]>,
    #[serde(default)]
    pub items: Vec<String>,
    #[serde(default)]
    pub members: Vec<String>,
    #[serde(default)]
    pub zmembers: Vec<[String; 2]>,
    #[serde(default)]
    pub truncated: bool,
}

/// 값 편집기에 넣을 키를 읽는다. 화면용 get_value 와 달리 칸을 64KiB 에서 자르지 않는다(잘린 값을 저장하면 뒷부분이 사라진다).
pub(crate) async fn raw_value(r: &mut RedisConn, key: String) -> Result<RawValue, Error> {
    let typ = text(&r.value(&args(&["TYPE", &key])).await?);
    if typ == "none" {
        return Err(Error::msg("key does not exist"));
    }
    let ttl_secs = int(&r.value(&args(&["TTL", &key])).await?);
    let mut v = RawValue {
        key: key.clone(),
        typ: typ.clone(),
        ttl_secs,
        ..Default::default()
    };
    let max = MAX_ROWS.to_string();
    match typ.as_str() {
        "string" => {
            let size = int(&r.value(&args(&["STRLEN", &key])).await?);
            v.string = text(
                &r.value(&args(&["GETRANGE", &key, "0", &(MAX_STRING_BYTES - 1).to_string()]))
                    .await?,
            );
            v.truncated = size > MAX_STRING_BYTES;
        }
        "hash" | "set" => {
            let hash = typ == "hash";
            let mut cursor = "0".to_string();
            loop {
                let cmd = if hash { "HSCAN" } else { "SSCAN" };
                let (next, items) = scan_reply(r.value(&args(&[cmd, &key, &cursor, "MATCH", "*", "COUNT", "500"])).await?)?;
                if hash {
                    v.fields.extend(items.as_chunks::<2>().0.iter().map(|[f, x]| [text(f), text(x)]));
                } else {
                    v.members.extend(items.iter().map(text));
                }
                cursor = next;
                let n = if hash { v.fields.len() } else { v.members.len() };
                if cursor == "0" || n >= MAX_ROWS {
                    v.truncated = cursor != "0" || n > MAX_ROWS;
                    v.fields.truncate(MAX_ROWS);
                    v.members.truncate(MAX_ROWS);
                    break;
                }
            }
        }
        "list" => {
            let RValue::Array(items) = r.value(&args(&["LRANGE", &key, "0", &max])).await? else {
                return Err(Error::msg("unexpected LRANGE reply"));
            };
            v.items = items.iter().map(text).collect();
            if v.items.len() > MAX_ROWS {
                v.items.truncate(MAX_ROWS);
                v.truncated = true;
            }
        }
        "zset" => {
            let pairs = zset_pairs(r.value(&args(&["ZRANGE", &key, "0", &max, "WITHSCORES"])).await?)?;
            v.zmembers = pairs.iter().map(|(m, s)| [text(m), score_text(s)]).collect();
            if v.zmembers.len() > MAX_ROWS {
                v.zmembers.truncate(MAX_ROWS);
                v.truncated = true;
            }
        }
        _ => {}
    }
    Ok(v)
}

/// ZRANGE WITHSCORES 답을 (멤버, 점수) 쌍으로 편다. RESP2 는 [멤버, 점수, …], RESP3 는 [[멤버, 점수], …] 다.
pub(crate) fn zset_pairs(v: RValue) -> Result<Vec<(RValue, RValue)>, Error> {
    let RValue::Array(vs) = v else {
        return Err(Error::msg("unexpected ZRANGE reply"));
    };
    let mut pairs = Vec::new();
    let mut flat = Vec::new();
    for v in vs {
        match v {
            RValue::Array(mut p) if p.len() == 2 => {
                let s = p.pop().unwrap_or(RValue::Nil);
                pairs.push((p.pop().unwrap_or(RValue::Nil), s));
            }
            v => flat.push(v),
        }
    }
    let mut it = flat.into_iter();
    while let (Some(m), Some(s)) = (it.next(), it.next()) {
        pairs.push((m, s));
    }
    Ok(pairs)
}

/// Go strconv.FormatFloat(f, 'f', -1, 64): 지수 없이 가장 짧은 자릿수다.
pub(crate) fn score_text(s: &RValue) -> String {
    let f = match s {
        RValue::Float(f) => *f,
        other => text(other).parse().unwrap_or(0.0),
    };
    format!("{f}")
}

// ─── 모니터 (R34) ───

/// Redis 오류가 명령을 모르거나 ACL 이 막은 것인지다(관리형 서버).
pub(crate) fn command_unavailable(e: &Error) -> bool {
    matches!(e, Error::Other(m) if m.contains("unknown command") || m.contains("unknown subcommand") || m.starts_with("NOPERM"))
}

use idb_core::textutil::group_digits;

fn digits(s: &str) -> String {
    match s.parse::<i64>() {
        Ok(n) if n >= 0 => group_digits(n),
        _ => "-".into(),
    }
}

/// 큰 단위 둘만 보인다(예: 3일 4시간).
fn format_uptime(secs: i64) -> String {
    let units = [("일", 86400), ("시간", 3600), ("분", 60), ("초", 1)];
    for (i, (name, size)) in units.iter().enumerate() {
        if secs < *size && *name != "초" {
            continue;
        }
        let mut out = format!("{}{name}", secs / size);
        if let Some((next, nsize)) = units.get(i + 1) {
            let rest = secs % size / nsize;
            if rest > 0 {
                out.push_str(&format!(" {rest}{next}"));
            }
        }
        return out;
    }
    "0초".into()
}

/// INFO 글을 항목·값 표로 줄인다. 없는 값은 "-" 다(ElastiCache·Valkey 는 일부 필드를 숨긴다).
pub(crate) fn summarize_info(text: &str) -> QueryResult {
    let mut info = std::collections::HashMap::new();
    let mut dbs: Vec<String> = Vec::new();
    for line in text.split('\n') {
        let Some((k, v)) = line.trim().split_once(':') else { continue };
        if k.starts_with('#') {
            continue;
        }
        info.insert(k.to_string(), v.to_string());
        if let Some(n) = k.strip_prefix("db")
            && digits(n) != "-"
        {
            dbs.push(k.to_string());
        }
    }
    let get = |k: &str| info.get(k).cloned().unwrap_or_default();
    let or_dash = |s: String| if s.is_empty() { "-".to_string() } else { s };
    let version = if !get("valkey_version").is_empty() {
        format!("Valkey {}", get("valkey_version"))
    } else if !get("redis_version").is_empty() {
        format!("Redis {}", get("redis_version"))
    } else {
        "-".into()
    };
    let uptime = get("uptime_in_seconds").parse().map_or("-".to_string(), format_uptime);
    let memory = match get("used_memory").parse::<i64>() {
        Ok(used) => {
            let max = match get("maxmemory").parse::<i64>() {
                Err(_) => "-".to_string(),
                Ok(0) => "제한 없음".to_string(),
                Ok(m) => human_bytes(m),
            };
            format!("{} / {max}", human_bytes(used))
        }
        Err(_) => "-".into(),
    };
    let hit_rate = match (get("keyspace_hits").parse::<i64>(), get("keyspace_misses").parse::<i64>()) {
        (Ok(h), Ok(m)) if h + m > 0 => format!("{:.1}%", h as f64 * 100.0 / (h + m) as f64),
        _ => "-".into(),
    };
    let mut rows = vec![
        vec!["버전".to_string(), version],
        vec!["가동 시간".into(), uptime],
        vec!["메모리 사용/최대".into(), memory],
        vec!["클라이언트 수".into(), or_dash(get("connected_clients"))],
        vec!["초당 명령".into(), or_dash(get("instantaneous_ops_per_sec"))],
        vec!["적중률".into(), hit_rate],
    ];
    // db10 이 db2 뒤에 오도록 번호로 정렬한다
    dbs.sort_by_key(|d| d[2..].parse::<i64>().unwrap_or(0));
    for name in dbs {
        let fields: std::collections::HashMap<&str, &str> =
            info[&name].split(',').map(|kv| kv.split_once('=').unwrap_or((kv, ""))).collect();
        let f = |k: &str| digits(fields.get(k).copied().unwrap_or(""));
        rows.push(vec![name.clone(), format!("키 {} · 만료 있음 {}", f("keys"), f("expires"))]);
    }
    QueryResult {
        cols: args(&["항목", "값"]),
        rows,
        ..Default::default()
    }
}

pub(crate) async fn info_summary(r: &mut RedisConn) -> Result<ResultSet, Error> {
    let start = Instant::now();
    let mut res = summarize_info(&text(&r.value(&args(&["INFO"])).await?));
    res.elapsed = start.elapsed();
    Ok(ResultSet::from_query(res))
}

/// SLOWLOG 인자에는 값이 그대로 들어 있을 수 있어 앞 80자만 싣는다(R34.1)
const SLOWLOG_CMD_RUNES: usize = 80;

/// SLOWLOG GET n 이다. 최근 것이 먼저 온다.
pub(crate) async fn slow_log(r: &mut RedisConn, n: i64) -> Result<ResultSet, Error> {
    let start = Instant::now();
    let RValue::Array(entries) = r.value(&args(&["SLOWLOG", "GET", &n.to_string()])).await? else {
        return Err(Error::msg("unexpected SLOWLOG reply"));
    };
    let mut res = QueryResult {
        cols: args(&["시각", "걸린 시간", "명령", "클라이언트"]),
        ..Default::default()
    };
    for e in entries {
        let RValue::Array(f) = e else { continue };
        let at = f.get(1).map_or(0, int);
        let us = f.get(2).map_or(0, int);
        let took = if us >= 1000 {
            format!("{:.1}ms", us as f64 / 1000.0)
        } else {
            format!("{us}µs")
        };
        let cmd: Vec<String> = match f.get(3) {
            Some(RValue::Array(a)) => a.iter().map(text).collect(),
            _ => Vec::new(),
        };
        // 제어 문자를 먼저 보이는 글자로 바꾼 뒤 자른다
        let mut cmd_text: String = printable(&cmd.join(" "), false);
        if cmd_text.chars().count() > SLOWLOG_CMD_RUNES {
            cmd_text = cmd_text.chars().take(SLOWLOG_CMD_RUNES).chain(['…']).collect();
        }
        let mut client = f.get(4).map(text).unwrap_or_default();
        let name = f.get(5).map(text).unwrap_or_default();
        if !name.is_empty() {
            client.push_str(&format!(" ({name})"));
        }
        let when = chrono::DateTime::from_timestamp(at, 0)
            .map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_default();
        res.rows.push(vec![when, took, cmd_text, printable(&client, false)]);
    }
    res.elapsed = start.elapsed();
    Ok(ResultSet::from_query(res))
}

/// MEMORY USAGE 를 못 쓰는 서버의 오류 글(Go ErrNoMemoryUsage)
pub const ERR_NO_MEMORY_USAGE: &str = "MEMORY USAGE is not available on this server";

/// SCAN 이 준 순서의 앞 sample 개 키에서 MEMORY USAGE 가 큰 top 개다. 표본이라 text 에 표본 수를 적는다(R34.1).
pub(crate) async fn big_keys(r: &mut RedisConn, sample: usize, top: usize) -> Result<ResultSet, Error> {
    if sample == 0 || top == 0 {
        return Err(Error::msg("sample and top must be positive"));
    }
    let start = Instant::now();
    let mut keys: Vec<String> = Vec::new();
    let mut cursor = "0".to_string();
    let count = sample.min(500).to_string();
    while keys.len() < sample {
        let (next, batch) = scan_reply(r.value(&args(&["SCAN", &cursor, "MATCH", "*", "COUNT", &count])).await?)?;
        for k in batch.iter().map(text) {
            // SCAN 은 리해시 중 같은 키를 두 번 줄 수 있다
            if keys.len() < sample && !keys.contains(&k) {
                keys.push(k);
            }
        }
        cursor = next;
        if cursor == "0" {
            break;
        }
    }
    let mut found: Vec<(String, String, i64, i64)> = Vec::new();
    for k in &keys {
        let n = match r.value(&args(&["MEMORY", "USAGE", k])).await {
            Ok(RValue::Nil) => continue, // SCAN 뒤 지워진 키
            Ok(v) => int(&v),
            Err(e) if command_unavailable(&e) => return Err(Error::msg(ERR_NO_MEMORY_USAGE)),
            Err(e) => return Err(e),
        };
        let typ = text(&r.value(&args(&["TYPE", k])).await?);
        let ttl = int(&r.value(&args(&["TTL", k])).await?);
        found.push((k.clone(), typ, n, ttl));
    }
    found.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    found.truncate(top);
    let res = QueryResult {
        cols: args(&["키", "타입", "크기", "바이트", "TTL"]),
        rows: found
            .into_iter()
            .map(|(k, t, n, ttl)| {
                let ttl = if ttl >= 0 {
                    idb_core::gostrconv::duration_string(ttl.saturating_mul(1_000_000_000))
                } else {
                    "∞".into()
                };
                vec![cap_cell(k.as_bytes()), t, human_bytes(n), n.to_string(), ttl]
            })
            .collect(),
        text: format!("표본 {}개", group_digits(keys.len() as i64)),
        elapsed: start.elapsed(),
        ..Default::default()
    };
    Ok(ResultSet::from_query(res))
}

// ─── 패턴 일괄 삭제 (R42) ───

/// 한 번에 지울 키 상한·UNLINK 배치(Go redisDeleteLimit·redisDeleteBatch)
pub const DELETE_LIMIT: usize = 100_000;
const DELETE_BATCH: usize = 500;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatternKeys {
    pub keys: Vec<String>,
    /// DELETE_LIMIT 개 뒤에 다른 키가 실제로 더 있다
    pub truncated: bool,
}

/// SCAN MATCH 로 처음 나온 순서대로 limit 개까지 모은다. truncated 는 limit 개를 채운 뒤 새 키를 하나 더 찾았다는 뜻이다.
pub(crate) async fn pattern_keys(r: &mut RedisConn, pattern: String, limit: usize) -> Result<PatternKeys, Error> {
    let mut seen = std::collections::HashSet::new();
    let mut out = PatternKeys::default();
    let mut cursor = "0".to_string();
    loop {
        let (next, batch) = scan_reply(r.value(&args(&["SCAN", &cursor, "MATCH", &pattern, "COUNT", "1000"])).await?)?;
        for k in batch.iter().map(text) {
            if seen.contains(&k) {
                continue;
            }
            if out.keys.len() == limit {
                out.truncated = true;
                return Ok(out);
            }
            seen.insert(k.clone());
            out.keys.push(k);
        }
        cursor = next;
        if cursor == "0" {
            return Ok(out);
        }
    }
}

/// keys 를 배치로 UNLINK 한다. 서버가 UNLINK 를 모르거나 ACL 이 막으면 DEL 로 같은 배치부터 다시 보낸다.
/// 지운 수는 응답의 합이라 그사이 사라진 키는 세지 않는다.
pub(crate) async fn unlink_keys(r: &mut RedisConn, keys: Vec<String>) -> Result<i64, Error> {
    let (mut done, mut use_del, mut start) = (0, false, 0);
    while start < keys.len() {
        let chunk = &keys[start..(start + DELETE_BATCH).min(keys.len())];
        let mut cmd = vec![if use_del { "DEL" } else { "UNLINK" }.to_string()];
        cmd.extend(chunk.iter().cloned());
        match r.value(&cmd).await {
            Err(e) if !use_del && command_unavailable(&e) => {
                use_del = true;
                continue;
            }
            Err(e) => return Err(Error::msg(format!("{done}개 지운 뒤 실패: {e}"))),
            Ok(v) => done += int(&v),
        }
        start += chunk.len();
    }
    Ok(done)
}

// ─── 값 편집 (Go redisview.build) ───

/// 편집기로 고칠 수 있는 값의 최대 바이트(Go MaxStringEdit)
const MAX_STRING_EDIT: usize = 1 << 20;
const SENTINEL_PREFIX: &str = "__db_deleted_";
// list 편집·삭제는 그 칸이 아직 읽은 값일 때만 바꾸도록 확인과 쓰기를 스크립트 하나로 묶는다. ARGV: 인덱스, 읽은 값, 새 값(삭제는 표식)
const LIST_SET_SCRIPT: &str = "if redis.call('LINDEX',KEYS[1],ARGV[1])==ARGV[2] then return redis.call('LSET',KEYS[1],ARGV[1],ARGV[3]) else return redis.error_reply('list changed since it was read') end";
const LIST_DELETE_SCRIPT: &str = "if redis.call('LINDEX',KEYS[1],ARGV[1])==ARGV[2] then redis.call('LSET',KEYS[1],ARGV[1],ARGV[3]) return redis.call('LREM',KEYS[1],1,ARGV[3]) else return redis.error_reply('list changed since it was read') end";

/// 값 편집 하나다. op 은 edit·add·delete·ttl·rename·delete_key, target 은 고를 대상(hash 필드, set·zset 멤버), old 는 바꾸기 전 값이다.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedisEdit {
    pub key: String,
    #[serde(rename = "type", default)]
    pub typ: String,
    pub op: String,
    #[serde(default)]
    pub index: i64,
    #[serde(default)]
    pub target: String,
    #[serde(default)]
    pub old: String,
    #[serde(default)]
    pub text: String,
    /// string 을 SET 할 때 TTL 을 지키라(KEEPTTL)
    #[serde(default)]
    pub keep_ttl: bool,
}

/// 편집 하나를 Redis 명령 인자로 만든다. 값은 언제나 인자 하나로 넣는다. 바뀐 것이 없으면 빈 목록이다.
pub fn edit_commands(e: &RedisEdit) -> Result<Vec<Vec<String>>, String> {
    if e.text.len() > MAX_STRING_EDIT {
        return Err(format!("value is larger than {MAX_STRING_EDIT} bytes"));
    }
    let k = e.key.clone();
    let one = |a: &[&str]| Ok(vec![args(a)]);
    match (e.op.as_str(), e.typ.as_str()) {
        ("ttl", _) => {
            let s = e.text.trim();
            if s.is_empty() || s == "-1" {
                return one(&["PERSIST", &k]);
            }
            match s.parse::<i64>() {
                Ok(n) if n > 0 => one(&["EXPIRE", &k, &n.to_string()]),
                _ => Err("TTL must be positive seconds, or empty/-1 to persist".into()),
            }
        }
        ("rename", _) if e.text.is_empty() => Err("new key name is empty".into()),
        ("rename", _) if e.text == e.key => Ok(Vec::new()),
        // RENAME 은 대상 키가 있으면 말없이 덮어써 남의 키를 지울 수 있다. 결과 0 이면 바꾸지 않은 것이다
        ("rename", _) => one(&["RENAMENX", &k, &e.text]),
        ("delete_key", _) => one(&["DEL", &k]),
        ("edit", _) if e.text == e.old => Ok(Vec::new()),
        // SET 은 TTL 을 지운다. KEEPTTL 은 Redis 6.0 부터라 TTL 이 있을 때만 붙인다
        ("edit", "string") if e.keep_ttl => one(&["SET", &k, &e.text, "KEEPTTL"]),
        ("edit", "string") => one(&["SET", &k, &e.text]),
        ("edit", "hash") => one(&["HSET", &k, &e.target, &e.text]),
        ("edit", "list") => one(&["EVAL", LIST_SET_SCRIPT, "1", &k, &e.index.to_string(), &e.old, &e.text]),
        // 바꾼 값이 이미 있으면 SADD 가 아무것도 하지 않고 SREM 만 되어 합쳐진다. 값을 잃지는 않는다
        ("edit", "set") => Ok(vec![args(&["SADD", &k, &e.text]), args(&["SREM", &k, &e.target])]),
        ("edit", "zset") => one(&["ZADD", &k, "XX", &parse_score(&e.text)?, &e.target]),
        // 추가는 기존 필드를 덮어쓰지 않는다
        ("add", "hash") => one(&["HSETNX", &k, &e.target, &e.text]),
        ("add", "list") => one(&["RPUSH", &k, &e.text]),
        ("add", "set") => one(&["SADD", &k, &e.text]),
        ("add", "zset") => one(&["ZADD", &k, "NX", &parse_score(&e.text)?, &e.target]),
        ("delete", "hash") => one(&["HDEL", &k, &e.target]),
        // 같은 값이 여럿일 때 LREM 은 첫 번째를 지운다. 고른 칸을 겹치지 않는 표식으로 바꾼 뒤 그 표식을 지운다
        ("delete", "list") => one(&["EVAL", LIST_DELETE_SCRIPT, "1", &k, &e.index.to_string(), &e.old, &sentinel()]),
        ("delete", "set") => one(&["SREM", &k, &e.target]),
        ("delete", "zset") => one(&["ZREM", &k, &e.target]),
        (_, t) => Err(format!("{t}: edit not supported")),
    }
}

/// list 삭제 표식이다. 다른 값과 겹치지 않으면 되어 해시 키(프로세스마다 무작위)로 만든다.
fn sentinel() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::from(SENTINEL_PREFIX);
    for i in 0..2u64 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(i);
        h.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
        );
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}

/// zset 점수를 Redis 가 받는 표기로 고친다(Go strconv.FormatFloat 'g' -1). NaN 은 Redis 가 거부하므로 미리 막는다.
fn parse_score(s: &str) -> Result<String, String> {
    let f: f64 = s.trim().parse().map_err(|e| format!("score must be a number: {e}"))?;
    if f.is_nan() {
        return Err("score must not be NaN".into());
    }
    if f.is_infinite() {
        return Ok(if f > 0.0 { "+inf" } else { "-inf" }.into());
    }
    // Go 'g' 는 지수가 -4 미만이거나 21 이상이면 e 표기다. 그 사이는 가장 짧은 자릿수다
    let exp = if f == 0.0 { 0 } else { f.abs().log10().floor() as i32 };
    if (-4..21).contains(&exp) {
        Ok(format!("{f}"))
    } else {
        let sci = format!("{f:e}");
        let (m, e) = sci.split_once('e').unwrap_or((&sci, "0"));
        let e: i32 = e.parse().unwrap_or(0);
        Ok(format!("{m}e{}{:02}", if e < 0 { '-' } else { '+' }, e.abs()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 바이트_크기는_Go_HumanBytes_와_같다() {
        assert_eq!(human_bytes(1023), "1023B");
        assert_eq!(human_bytes(1536), "1.5KB");
        assert_eq!(human_bytes(5 << 20), "5.0MB");
    }

    #[test]
    fn INFO_요약은_없는_값을_대시로_두고_DB_를_번호순으로_둔다() {
        let r = summarize_info(
            "# Server\r\nredis_version:7.2.4\r\nuptime_in_seconds:273600\r\nused_memory:1536\r\nmaxmemory:0\r\nkeyspace_hits:3\r\nkeyspace_misses:1\r\n# Keyspace\r\ndb10:keys=1,expires=0\r\ndb2:keys=12345,expires=1\r\n",
        );
        assert_eq!(r.rows[0], ["버전", "Redis 7.2.4"]);
        assert_eq!(r.rows[1], ["가동 시간", "3일 4시간"]);
        assert_eq!(r.rows[2], ["메모리 사용/최대", "1.5KB / 제한 없음"]);
        assert_eq!(r.rows[3], ["클라이언트 수", "-"]);
        assert_eq!(r.rows[5], ["적중률", "75.0%"]);
        assert_eq!(r.rows[6], ["db2", "키 12,345 · 만료 있음 1"]);
        assert_eq!(r.rows[7][0], "db10");
    }

    #[test]
    fn 값_편집은_Go_와_같은_명령을_만든다() {
        let e = |op: &str, typ: &str, text: &str| RedisEdit {
            key: "k".into(),
            typ: typ.into(),
            op: op.into(),
            target: "f".into(),
            old: "o".into(),
            text: text.into(),
            ..Default::default()
        };
        assert_eq!(edit_commands(&e("ttl", "string", "")).unwrap(), [args(&["PERSIST", "k"])]);
        assert!(edit_commands(&e("ttl", "string", "0")).is_err());
        assert_eq!(edit_commands(&e("rename", "string", "n")).unwrap(), [args(&["RENAMENX", "k", "n"])]);
        assert!(edit_commands(&e("edit", "string", "o")).unwrap().is_empty());
        assert_eq!(
            edit_commands(&e("edit", "set", "n")).unwrap(),
            [args(&["SADD", "k", "n"]), args(&["SREM", "k", "f"])]
        );
        assert_eq!(
            edit_commands(&e("add", "zset", "1.50")).unwrap(),
            [args(&["ZADD", "k", "NX", "1.5", "f"])]
        );
        assert_eq!(parse_score("1e21").unwrap(), "1e+21");
        assert_eq!(parse_score("-inf").unwrap(), "-inf");
        assert!(parse_score("NaN").is_err());
        let d = edit_commands(&e("delete", "list", "")).unwrap();
        assert!(d[0][6].starts_with(SENTINEL_PREFIX) && d[0][6].len() == SENTINEL_PREFIX.len() + 32);
        assert!(edit_commands(&e("edit", "stream", "x")).is_err());
    }

    #[test]
    fn 숫자는_세_자리마다_쉼표를_넣는다() {
        assert_eq!(group_digits(1234567), "1,234,567");
        assert_eq!(group_digits(999), "999");
        assert_eq!(format_uptime(59), "59초");
        assert_eq!(format_uptime(3600), "1시간");
    }
}
