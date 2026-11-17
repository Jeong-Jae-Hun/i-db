//! 대시보드·`db status` 확인(R64.2)과 접속 시험. Go `internal/db/health.go`·`probe.go`·`connstore.go`(TestConn) 이다.
//! 확인은 읽기 전용 메타데이터만 묻는다. 여는 데는 PROBE_OPEN_TIMEOUT, 질의에는 PROBE_TIMEOUT 을 건다.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use idb_core::project::{Conn, Kind};
use idb_core::redact::redact;
use idb_core::textutil::{human_bytes, korean_count};

use crate::{Clients, Error};

/// 질의 한 번의 확인 제한 시간
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// 아직 열지 않은 접속을 여는 제한 시간. Redis 가 평문 3초 뒤 TLS 로 다시 붙고 PostgreSQL connect_timeout 이 8초라 길다
pub const PROBE_OPEN_TIMEOUT: Duration = Duration::from_secs(15);

/// 접속 하나를 가볍게 살펴본 결과다.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Health {
    pub ok: bool,
    pub latency: Duration,
    pub version: String,
    /// 종류별 주요 수치(활성 세션·클라이언트·메모리·문서 수 등)
    pub metrics: Vec<Metric>,
    /// 비밀값을 지운 한 줄
    pub err: String,
}

/// 대시보드 한 칸이다. name 이 비면 값만 보인다(`green`·`ENABLED` 같은 상태 낱말).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Metric {
    pub name: String,
    pub value: String,
    pub warn: bool,
}

impl Metric {
    pub fn new(name: &str, value: impl Into<String>, warn: bool) -> Metric {
        Metric {
            name: name.into(),
            value: value.into(),
            warn,
        }
    }
}

/// 확인 실패 결과다. 오류에서 secret 을 지우고 여러 줄이면 한 줄로 붙인다(R64.2).
pub fn failed(latency: Duration, err: &str, secret: &str) -> Health {
    Health {
        latency,
        err: redact(err, secret).split_whitespace().collect::<Vec<_>>().join(" "),
        ..Default::default()
    }
}

const WARN_RATIO: f64 = 0.8;

/// n 이 limit 의 80% 를 넘는지다. limit 이 0 이하면 한도가 없어 경고하지 않는다.
fn over(n: i64, limit: i64) -> bool {
    limit > 0 && n as f64 > limit as f64 * WARN_RATIO
}

async fn timed<T>(d: Duration, fut: impl std::future::Future<Output = Result<T, Error>>) -> Result<T, Error> {
    tokio::time::timeout(d, fut).await.unwrap_or(Err(Error::Deadline))
}

// ─── PostgreSQL ───

const PG_PROBE_SQL: &str = "select count(*), count(*) filter (where state = 'active' and pid <> pg_backend_pid()),
	current_setting('max_connections')::int, pg_database_size(current_database()), current_setting('server_version')
	from pg_stat_activity where backend_type = 'client backend'";
const PG_VERSION_SQL: &str = "select current_setting('server_version')";

/// server_version(`16.4 (Debian …)`)의 첫 낱말에 이름을 붙인다.
fn pg_version(v: &str) -> String {
    v.split_whitespace().next().map(|f| format!("PostgreSQL {f}")).unwrap_or_default()
}

/// PostgreSQL 접속을 READ ONLY 트랜잭션 한 번으로 살펴본다. 수치: `세션 N/M`·`활성 K`·`DB 4.2GB`. 집계가 막히면 버전만 보이고 OK 다.
pub async fn probe_pg(clients: &Clients, c: &Conn) -> Health {
    let start = Instant::now();
    let pg = match timed(PROBE_OPEN_TIMEOUT, clients.pg(c)).await {
        Ok(pg) => pg,
        Err(e) => return failed(start.elapsed(), &e.to_string(), &c.password),
    };
    let start = Instant::now();
    let deadline = start + PROBE_TIMEOUT;
    let left = || deadline.saturating_duration_since(Instant::now());
    let err = match timed(left(), pg.run_read_only_rows(PG_PROBE_SQL, 1)).await {
        Ok(res) if res.rows.first().is_some_and(|r| r.len() == 5) => {
            let r = &res.rows[0];
            let n = |i: usize| r[i].parse::<i64>().unwrap_or(0);
            let (sessions, active, max_conns, size) = (n(0), n(1), n(2), n(3));
            return Health {
                ok: true,
                latency: start.elapsed(),
                version: pg_version(&r[4]),
                metrics: vec![
                    Metric::new("세션", format!("{sessions}/{max_conns}"), over(sessions, max_conns)),
                    Metric::new("활성", active.to_string(), false),
                    Metric::new("DB", human_bytes(size), false),
                ],
                ..Default::default()
            };
        }
        Ok(_) => Error::msg("unexpected probe result"),
        Err(e) => e,
    };
    if !left().is_zero()
        && let Ok(res) = timed(left(), pg.run_read_only_rows(PG_VERSION_SQL, 1)).await
        && let Some(v) = res.rows.first().and_then(|r| r.first())
    {
        return Health {
            ok: true,
            latency: start.elapsed(),
            version: pg_version(v),
            ..Default::default()
        };
    }
    failed(start.elapsed(), &err.to_string(), &c.password)
}

// ─── Redis ───

/// INFO 글을 필드 맵과 키 공간 이름(db0·db1…, 나온 순서)으로 가른다.
pub fn parse_info(text: &str) -> (HashMap<String, String>, Vec<String>) {
    let mut info = HashMap::new();
    let mut dbs = Vec::new();
    for line in text.split('\n') {
        let Some((k, v)) = line.trim().split_once(':') else { continue };
        if k.starts_with('#') {
            continue;
        }
        info.insert(k.to_string(), v.to_string());
        if let Some(num) = k.strip_prefix("db")
            && num.parse::<i64>().is_ok_and(|n| n >= 0)
        {
            dbs.push(k.to_string());
        }
    }
    (info, dbs)
}

/// 서버 이름과 버전(`Valkey 8.0.1`·`Redis 7.2.4`)이다. Valkey 는 호환용 redis_version 도 내므로 valkey_version 을 먼저 본다.
pub fn info_version(info: &HashMap<String, String>) -> String {
    match (info.get("valkey_version"), info.get("redis_version")) {
        (Some(v), _) if !v.is_empty() => format!("Valkey {v}"),
        (_, Some(v)) if !v.is_empty() => format!("Redis {v}"),
        _ => String::new(),
    }
}

/// INFO server 글의 서버 이름과 버전이다. 버전 필드를 숨긴 서버는 이름만 보인다.
pub fn redis_version(text: &str) -> String {
    let v = info_version(&parse_info(text).0);
    if v.is_empty() { "Redis".into() } else { v }
}

/// `1.1/4.0GB` 다. 단위가 다르면 둘 다 붙인다(`512.0MB/4.0GB`).
fn bytes_pair(used: i64, total: i64) -> String {
    let (mut u, t) = (human_bytes(used), human_bytes(total));
    let unit = |s: &str| s.trim_start_matches(|c: char| c.is_ascii_digit() || c == '.').to_string();
    let tu = unit(&t);
    if unit(&u) == tu {
        u.truncate(u.len() - tu.len());
    }
    format!("{u}/{t}")
}

fn redis_metrics(info: &HashMap<String, String>, dbs: &[String]) -> Vec<Metric> {
    let mut keys = 0i64;
    for name in dbs {
        let line = info.get(name).map(String::as_str).unwrap_or("");
        let n = line
            .split(',')
            .find_map(|kv| kv.split_once('=').filter(|(k, _)| *k == "keys").map(|(_, v)| v))
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(0);
        keys += n;
    }
    let mut mem = Metric::new("메모리", "-", false);
    if let Some(used) = info.get("used_memory").and_then(|v| v.parse::<i64>().ok()) {
        match info.get("maxmemory").and_then(|v| v.parse::<i64>().ok()) {
            None => mem.value = human_bytes(used),
            Some(max) if max > 0 => {
                mem.value = bytes_pair(used, max);
                mem.warn = over(used, max);
            }
            Some(_) => mem.value = format!("{}/제한 없음", human_bytes(used)),
        }
    }
    let clients = info
        .get("connected_clients")
        .filter(|v| !v.is_empty())
        .cloned()
        .unwrap_or_else(|| "-".into());
    vec![
        Metric::new("클라이언트", clients, false),
        mem,
        Metric::new("키", korean_count(keys), false),
    ]
}

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

/// Redis 접속에 네 구역만 INFO 로 묻는다. 여러 구역 INFO 는 Redis 7 부터라 옛 서버가 오류로 답하면 인자 없는 INFO 로 다시 묻는다.
pub async fn probe_redis(clients: &Clients, c: &Conn) -> Health {
    let start = Instant::now();
    let r = match timed(PROBE_OPEN_TIMEOUT, clients.redis(c)).await {
        Ok(r) => r,
        Err(e) => return failed(start.elapsed(), &e.to_string(), &c.password),
    };
    let start = Instant::now();
    let res = timed(PROBE_TIMEOUT, async {
        let mut rc = r.lock().await;
        match rc.run(&args(&["INFO", "server", "clients", "memory", "keyspace"])).await {
            // 서버 오류 답(ERR …)만 옛 서버로 보고 다시 묻는다
            Err(Error::Other(e)) if e.starts_with("ERR") => rc.run(&args(&["INFO"])).await,
            r => r,
        }
    })
    .await;
    match res {
        Ok(res) => {
            let (info, dbs) = parse_info(&res.text);
            Health {
                ok: true,
                latency: start.elapsed(),
                version: info_version(&info),
                metrics: redis_metrics(&info, &dbs),
                ..Default::default()
            }
        }
        Err(e) => failed(start.elapsed(), &e.to_string(), &c.password),
    }
}

// ─── 시험 ───

/// c(비밀번호 c.password)로 새로 붙어 서버 버전을 읽고 닫는다(Go db.TestConn). 오류에서 비밀번호를 지운다.
pub async fn test_db_conn(c: &Conn) -> Result<String, String> {
    let mut c = c.clone();
    c.uuid = "test".into();
    if c.name.is_empty() {
        c.name = c.host.clone();
    }
    let cs = Clients::new();
    let r = async {
        match c.kind {
            Some(Kind::Pg) => {
                let pg = cs.pg(&c).await?;
                let res = pg.run_read_only_rows("show server_version", 1).await?;
                let v = res.rows.first().and_then(|r| r.first()).cloned().unwrap_or_default();
                // 16.4 (Debian 16.4-1.pgdg120+1) 처럼 뒤에 붙는 배포판 글은 뺀다
                Ok(format!("PostgreSQL {}", v.split(' ').next().unwrap_or("")))
            }
            Some(Kind::Redis) => {
                let r = cs.redis(&c).await?;
                let res = r.lock().await.run(&args(&["INFO", "server"])).await?;
                Ok(redis_version(&res.text))
            }
            _ => Err(Error::msg(format!(
                "kind must be postgresql or redis: {}",
                idb_core::project::go_quote(c.kind_str())
            ))),
        }
    }
    .await;
    cs.close().await;
    r.map_err(|e: Error| redact(&e.to_string(), &c.password))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redis_버전은_Valkey_를_먼저_보고_없으면_이름만() {
        assert_eq!(
            redis_version("# Server\r\nredis_version:7.2.4\r\nvalkey_version:8.0.1\r\n"),
            "Valkey 8.0.1"
        );
        assert_eq!(redis_version("redis_version:7.2.4\r\n"), "Redis 7.2.4");
        assert_eq!(redis_version("# Server\r\nconnected_clients:1\r\n"), "Redis");
    }

    #[test]
    fn redis_수치는_키_공간을_더하고_메모리_한도를_본다() {
        let (info, dbs) = parse_info(
            "connected_clients:3\r\nused_memory:3758096384\r\nmaxmemory:4294967296\r\ndb0:keys=10000,expires=0\r\ndb1:keys=2000\r\n",
        );
        let m = redis_metrics(&info, &dbs);
        assert_eq!(m[0], Metric::new("클라이언트", "3", false));
        assert_eq!(m[1], Metric::new("메모리", "3.5/4.0GB", true));
        assert_eq!(m[2], Metric::new("키", "1.2만", false));
        let (info, dbs) = parse_info("used_memory:1024\r\nmaxmemory:0\r\n");
        assert_eq!(redis_metrics(&info, &dbs)[1].value, "1.0KB/제한 없음");
        assert_eq!(redis_metrics(&HashMap::new(), &[])[0].value, "-");
    }

    #[test]
    fn 실패_결과는_비밀값을_지우고_한_줄로_붙인다() {
        let h = failed(Duration::ZERO, "dial\n  pw-9 refused", "pw-9");
        assert_eq!(h.err, "dial *** refused");
        assert!(!h.ok);
    }
}
