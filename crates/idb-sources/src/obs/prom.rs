use idb_core::project::{Conn, go_quote};
use idb_core::result::{MAX_ROWS, QueryResult};
use tokio::time::Instant;

use super::result::{QueryData, first_vector_value, to_result};
use super::{Client, Now, empty_query, limit_of, step, system_now, window};
use crate::httpc::{Values, values};
use crate::source::{Error, Health, Metric, Node, Query};

/// R62.1 지표 이름 앞 1,000개
const MAX_METRIC_NAMES: usize = 1000;

pub struct Prometheus {
    pub(super) c: Client,
    pub(super) now: Now,
}

/// 유닉스 나노초를 Prometheus 시각(초, 소수 셋째 자리)으로 쓴다.
fn secs(ns: i64) -> String {
    format!("{:.3}", (ns / 1_000_000) as f64 / 1000.0)
}

impl Prometheus {
    /// Prometheus 접속이다. 인증 규칙은 Loki 와 같다.
    pub fn new(c: &Conn) -> Result<Prometheus, Error> {
        Ok(Prometheus {
            c: Client::new("prometheus", c, false)?,
            now: system_now,
        })
    }

    /// "" 이면 지표 이름 앞 1,000개다. 지표 아래는 없다.
    pub async fn children(&self, path: &str) -> Result<Vec<Node>, Error> {
        if !path.is_empty() {
            return Ok(Vec::new());
        }
        // limit 은 Prometheus 2.47+ 만 따르므로 받은 뒤에도 자른다
        let names: Option<Vec<String>> = self
            .c
            .get_data("/api/v1/label/__name__/values", &values([("limit", MAX_METRIC_NAMES.to_string())]))
            .await?;
        Ok(names
            .unwrap_or_default()
            .into_iter()
            .take(MAX_METRIC_NAMES)
            .map(|n| Node {
                name: n.clone(),
                path: n,
                openable: true,
                ..Default::default()
            })
            .collect())
    }

    /// 지표의 즉시값(vector)이다.
    pub async fn open(&self, path: &str, limit: usize) -> Result<QueryResult, Error> {
        if path.is_empty() {
            return Err(Error::msg("prometheus: open needs a metric name"));
        }
        let params = values([("query", path.to_string()), ("time", secs((self.now)()))]);
        self.query("/api/v1/query", &params, limit_of(limit, MAX_ROWS, 0)).await
    }

    /// PromQL 을 늘 범위 질의(query_range)로 보낸다. since 가 0 이면 최근 1시간, step 은 범위/250(최소 15초)이다.
    /// 즉시값은 트리 Enter(open)로 본다. 범위 결과라 차트(R28)로 바로 그릴 수 있다.
    pub async fn run(&self, q: &Query) -> Result<QueryResult, Error> {
        if q.text.trim().is_empty() {
            return Err(empty_query());
        }
        let (start, end) = window(q, (self.now)())?;
        let params = values([
            ("query", q.text.clone()),
            ("start", secs(start)),
            ("end", secs(end)),
            ("step", step(start, end)),
        ]);
        self.query("/api/v1/query_range", &params, limit_of(q.limit, MAX_ROWS, 0)).await
    }

    async fn query(&self, path: &str, params: &Values, limit: usize) -> Result<QueryResult, Error> {
        let began = Instant::now();
        let d: QueryData = self.c.get_data(path, params).await?;
        let mut res = to_result(&d, limit, false).map_err(|e| Error::msg(format!("prometheus: {e}")))?;
        res.elapsed = began.elapsed();
        Ok(res)
    }

    /// 즉시 질의 `count(up)`·`count(up == 0)` 이다. down 대상이 있으면 경고다. 예: `up 372/380`.
    pub async fn probe(&self) -> Health {
        let start = Instant::now();
        let total = self.count("count(up)").await;
        let latency = start.elapsed();
        let fail = |e: Error| Health::failed(latency, &e.to_string(), &self.c.secret);
        let total = match total {
            Ok(n) => n,
            Err(e) => return fail(e),
        };
        let down = match self.count("count(up == 0)").await {
            Ok(n) => n,
            Err(e) => return fail(e),
        };
        Health {
            ok: true,
            latency,
            version: self.c.build_version("/api/v1/status/buildinfo", "Prometheus").await,
            metrics: vec![Metric::new("up", format!("{}/{total}", total - down), down > 0)],
            err: String::new(),
        }
    }

    /// count(...) 즉시 질의의 값이다. 맞는 시계열이 없으면 count 는 빈 vector 라 0 이다.
    async fn count(&self, query: &str) -> Result<i64, Error> {
        let params = values([("query", query.to_string()), ("time", secs((self.now)()))]);
        let raw: Box<serde_json::value::RawValue> = self.c.get_data("/api/v1/query", &params).await?;
        let v = first_vector_value(raw.get()).map_err(|e| Error::msg(format!("prometheus: unexpected data: {e}")))?;
        let Some(v) = v else { return Ok(0) };
        match v.parse::<f64>() {
            Ok(n) => Ok(n as i64),
            Err(_) => Err(Error::msg(format!("prometheus: {query}: unexpected value {}", go_quote(&v)))),
        }
    }
}
