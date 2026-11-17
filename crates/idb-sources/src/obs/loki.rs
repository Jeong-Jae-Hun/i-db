use idb_core::project::{Conn, Kind, go_quote};
use idb_core::result::{MAX_ROWS, QueryResult};
use idb_core::textutil::korean_count;
use tokio::time::Instant;

use super::result::{QueryData, to_result};
use super::{Client, Now, empty_query, limit_of, step, system_now, window};
use crate::Error;
use crate::httpc::{Values, path_escape, values};
use crate::source::{DEFAULT_RANGE_NS, Health, LOKI_OPEN_LIMIT, Metric, Node, Query};

/// R62.2 Loki 서버 상한
const LOKI_MAX_LIMIT: usize = 5000;

pub struct Loki {
    pub(super) c: Client,
    pub(super) now: Now,
}

impl Loki {
    /// Loki 접속이다. auth "basic" 이면 user·password, "token" 이면 password 를 Bearer 로 보낸다.
    pub fn new(c: &Conn) -> Result<Loki, Error> {
        Ok(Loki {
            c: Client::new("loki", c, false)?,
            now: system_now,
        })
    }

    fn label_range(&self) -> Values {
        let end = (self.now)();
        values([("start", (end - DEFAULT_RANGE_NS).to_string()), ("end", end.to_string())])
    }

    /// "" 이면 라벨 이름, "라벨" 이면 그 값(최근 1시간)이다. 값 노드 path 는 "라벨/값" 이다
    /// (라벨 이름에는 '/' 가 올 수 없어 첫 '/' 로 가른다).
    pub async fn children(&self, path: &str) -> Result<Vec<Node>, Error> {
        let params = self.label_range();
        if path.is_empty() {
            let names: Option<Vec<String>> = self.c.get_data("/loki/api/v1/labels", &params).await?;
            return Ok(names
                .unwrap_or_default()
                .into_iter()
                .map(|n| Node {
                    name: n.clone(),
                    path: n,
                    has_children: true,
                    ..Default::default()
                })
                .collect());
        }
        if path.contains('/') {
            return Ok(Vec::new());
        }
        let vals: Option<Vec<String>> = self
            .c
            .get_data(&format!("/loki/api/v1/label/{}/values", path_escape(path)), &params)
            .await?;
        Ok(vals
            .unwrap_or_default()
            .into_iter()
            .map(|v| Node {
                path: format!("{path}/{v}"),
                name: v,
                openable: true,
                ..Default::default()
            })
            .collect())
    }

    /// "라벨/값" 을 {라벨="값"} 최근 1시간 로그로 연다. limit 0 이면 100줄이다.
    pub async fn open(&self, path: &str, limit: usize) -> Result<QueryResult, Error> {
        match loki_open_query(path, limit) {
            Some(q) => self.run(&q).await,
            None => Err(Error::msg(format!("loki: open needs label/value, got {}", go_quote(path)))),
        }
    }

    /// LogQL 을 query_range 로 보낸다(기본 backward 최신 먼저, direction "forward" 면 오래된 것부터).
    /// 로그 질의는 서버 limit 으로, 지표 질의는 받은 뒤 행을 자른다.
    pub async fn run(&self, q: &Query) -> Result<QueryResult, Error> {
        if q.text.trim().is_empty() {
            return Err(empty_query());
        }
        let direction = match q.direction.as_str() {
            "" => "backward",
            d @ ("forward" | "backward") => d,
            d => return Err(Error::msg(format!("loki: unknown direction {} (forward or backward)", go_quote(d)))),
        };
        let (start, end) = window(q, (self.now)())?;
        let limit = limit_of(q.limit, MAX_ROWS, LOKI_MAX_LIMIT);
        let began = Instant::now();
        let d: QueryData = self
            .c
            .get_data(
                "/loki/api/v1/query_range",
                &values([
                    ("query", q.text.clone()),
                    ("start", start.to_string()),
                    ("end", end.to_string()),
                    ("limit", limit.to_string()),
                    ("direction", direction.to_string()),
                    ("step", step(start, end)),
                ]),
            )
            .await?;
        let mut res = to_result(&d, limit, direction == "forward").map_err(|e| Error::msg(format!("loki: {e}")))?;
        // 서버가 limit 에서 멈췄으면 더 있을 수 있다
        if d.result_type == "streams" && res.rows.len() == limit {
            res.truncated = true;
        }
        res.elapsed = began.elapsed();
        Ok(res)
    }

    /// `/ready` 와 최근 1시간 라벨 수다. 준비되지 않았으면 Loki 가 503 을 줘 실패다. 예: `ready · 라벨 13`.
    pub async fn probe(&self) -> Health {
        let start = Instant::now();
        let ready = self.c.get("/ready", &Values::new()).await;
        let latency = start.elapsed();
        if let Err(e) = ready {
            return Health::failed(latency, &e.to_string(), &self.c.secret);
        }
        let names: Option<Vec<String>> = match self.c.get_data("/loki/api/v1/labels", &self.label_range()).await {
            Ok(n) => n,
            Err(e) => return Health::failed(latency, &e.to_string(), &self.c.secret),
        };
        Health {
            ok: true,
            latency,
            version: self.c.build_version("/loki/api/v1/status/buildinfo", "Loki").await,
            metrics: vec![
                Metric::new("", "ready", false),
                Metric::new("라벨", korean_count(names.map_or(0, |n| n.len()) as i64), false),
            ],
            err: String::new(),
        }
    }
}

fn loki_open_query(path: &str, limit: usize) -> Option<Query> {
    let (label, value) = path.split_once('/').filter(|(l, _)| !l.is_empty())?;
    Some(Query {
        text: format!("{{{label}={}}}", go_quote(value)),
        limit: limit_of(limit, LOKI_OPEN_LIMIT, 0),
        ..Default::default()
    })
}

/// 트리 Enter 미리 보기(open)와 같은 질의를 run 꼴로 쓴 것이다. 미리 보기 결과도 time 정렬을
/// 서버로 다시 읽을 수 있게 결과의 원래 질의로 남긴다(R62.3). Grafana 는 path 첫 마디(uid)를 지시줄로 싣는다.
/// Loki 라벨 값 경로가 아니면(Prometheus 지표 등) None 이다.
///   - 예: Loki "app/api" → `{app="api"}`, Grafana "lk1/app/api" → "# datasource: lk1\n{app=\"api\"}"
pub fn open_query(kind: Kind, path: &str, limit: usize) -> Option<Query> {
    if kind != Kind::Grafana {
        return loki_open_query(path, limit);
    }
    let (uid, rest) = path.split_once('/').unwrap_or((path, ""));
    let mut q = loki_open_query(rest, limit)?;
    if uid.is_empty() {
        return None;
    }
    q.text = format!("# datasource: {uid}\n{}", q.text);
    Some(q)
}
