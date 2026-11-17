use std::sync::Mutex;

use idb_core::gojson::json_error;
use idb_core::project::{Conn, go_quote};
use idb_core::result::QueryResult;
use serde::Deserialize;
use tokio::time::Instant;

use super::{Client, Loki, Now, Prometheus, system_now};
use crate::httpc::{Values, path_escape};
use crate::source::{Error, Health, Metric, Node, Query};

#[derive(Clone, Deserialize)]
struct Datasource {
    #[serde(default, deserialize_with = "crate::opensearch::nullable")]
    uid: String,
    #[serde(default, deserialize_with = "crate::opensearch::nullable")]
    name: String,
    #[serde(default, rename = "type", deserialize_with = "crate::opensearch::nullable")]
    typ: String,
}

/// Loki·Prometheus 데이터 소스를 /api/datasources/proxy/uid/<uid> 로 부른다.
/// 트리 path 는 "<uid>" 또는 "<uid>/<그 데이터 소스 안 경로>" 다.
pub struct Grafana {
    pub(super) c: Client,
    pub(super) now: Now,
    /// Loki·Prometheus 만, Grafana 가 준 순서(이름순)
    ds: Mutex<Option<Vec<Datasource>>>,
}

/// 데이터 소스 하나로 가는 Loki·Prometheus 다. 같은 HTTP 연결·토큰을 쓴다.
enum Sub {
    Loki(Loki),
    Prom(Prometheus),
}

impl Grafana {
    /// Grafana 접속이다. password 에 든 서비스 계정 토큰을 Authorization: Bearer 로 보낸다.
    pub fn new(c: &Conn) -> Result<Grafana, Error> {
        Ok(Grafana {
            c: Client::new("grafana", c, true)?,
            now: system_now,
            ds: Mutex::new(None),
        })
    }

    /// 목록을 읽어 둔다. refresh 가 아니면 읽어 둔 것을 쓴다.
    async fn datasources(&self, refresh: bool) -> Result<Vec<Datasource>, Error> {
        if !refresh && let Some(ds) = self.ds.lock().ok().and_then(|g| g.clone()) {
            return Ok(ds);
        }
        let body = self.c.get("/api/datasources", &Values::new()).await?;
        let all: Option<Vec<Datasource>> = serde_json::from_slice(&body)
            .map_err(|e| Error::msg(format!("grafana: unexpected datasources response: {}", json_error(&e))))?;
        let ds: Vec<Datasource> = all
            .unwrap_or_default()
            .into_iter()
            .filter(|d| d.typ == "loki" || d.typ == "prometheus")
            .collect();
        if let Ok(mut g) = self.ds.lock() {
            *g = Some(ds.clone());
        }
        Ok(ds)
    }

    fn backend(&self, d: &Datasource) -> Sub {
        let c = self.c.sub(
            &format!("grafana {}", d.name),
            &format!("/api/datasources/proxy/uid/{}", path_escape(&d.uid)),
        );
        if d.typ == "loki" {
            Sub::Loki(Loki { c, now: self.now })
        } else {
            Sub::Prom(Prometheus { c, now: self.now })
        }
    }

    /// uid 또는 이름이 같은 데이터 소스다. uid 를 먼저 본다.
    async fn resolve(&self, key: &str) -> Result<Datasource, Error> {
        let ds = self.datasources(false).await?;
        ds.iter()
            .find(|d| d.uid == key)
            .or_else(|| ds.iter().find(|d| d.name == key))
            .cloned()
            .ok_or_else(|| Error::msg(format!("grafana: no loki/prometheus datasource {}", go_quote(key))))
    }

    pub async fn children(&self, path: &str) -> Result<Vec<Node>, Error> {
        if path.is_empty() {
            return Ok(self
                .datasources(true)
                .await?
                .into_iter()
                .map(|d| Node {
                    name: d.name,
                    path: d.uid,
                    info: d.typ,
                    has_children: true,
                    ..Default::default()
                })
                .collect());
        }
        let (uid, rest) = path.split_once('/').unwrap_or((path, ""));
        let d = self.resolve(uid).await?;
        let mut nodes = match self.backend(&d) {
            Sub::Loki(b) => b.children(rest).await?,
            Sub::Prom(b) => b.children(rest).await?,
        };
        for n in &mut nodes {
            n.path = format!("{uid}/{}", n.path);
        }
        Ok(nodes)
    }

    pub async fn open(&self, path: &str, limit: usize) -> Result<QueryResult, Error> {
        let (uid, rest) = path.split_once('/').unwrap_or((path, ""));
        let d = self.resolve(uid).await?;
        match self.backend(&d) {
            Sub::Loki(b) => b.open(rest, limit).await,
            Sub::Prom(b) => b.open(rest, limit).await,
        }
    }

    /// 첫 비어 있지 않은 줄이 "# datasource: <이름|uid>" 면 그 데이터 소스로, 아니면
    /// 목록의 첫 Loki(없으면 첫 Prometheus) 데이터 소스로 보낸다. 지시줄은 지우고 보낸다.
    pub async fn run(&self, q: &Query) -> Result<QueryResult, Error> {
        let (key, text) = split_directive(&q.text);
        let d = match key {
            Some(k) => self.resolve(&k).await?,
            None => self.default_datasource().await?,
        };
        let q = Query { text, ..q.clone() };
        match self.backend(&d) {
            Sub::Loki(b) => b.run(&q).await,
            Sub::Prom(b) => b.run(&q).await,
        }
    }

    async fn default_datasource(&self) -> Result<Datasource, Error> {
        let ds = self.datasources(false).await?;
        ["loki", "prometheus"]
            .iter()
            .find_map(|typ| ds.iter().find(|d| d.typ == *typ))
            .cloned()
            .ok_or_else(|| Error::msg("grafana: no loki/prometheus datasource"))
    }

    /// `/api/health` 의 database 상태와 버전이다. database 가 `ok` 가 아니면 경고다. 예: `ok`(버전은 version 칸에만).
    pub async fn probe(&self) -> Health {
        #[derive(Deserialize)]
        struct H {
            #[serde(default, deserialize_with = "crate::opensearch::nullable")]
            database: String,
            #[serde(default, deserialize_with = "crate::opensearch::nullable")]
            version: String,
        }
        let start = Instant::now();
        let body = self.c.get("/api/health", &Values::new()).await;
        let latency = start.elapsed();
        let body = match body {
            Ok(b) => b,
            Err(e) => return Health::failed(latency, &e.to_string(), &self.c.secret),
        };
        let h = match serde_json::from_slice::<H>(&body) {
            Ok(h) if !h.database.is_empty() => h,
            _ => return Health::failed(latency, "grafana: unexpected /api/health response", &self.c.secret),
        };
        Health {
            ok: true,
            latency,
            version: if h.version.is_empty() {
                String::new()
            } else {
                format!("Grafana {}", h.version)
            },
            metrics: vec![Metric::new("", h.database.clone(), h.database != "ok")],
            err: String::new(),
        }
    }
}

/// 첫 비어 있지 않은 줄의 지시(`# datasource: <이름|uid>`, 대소문자 무시)를 떼어 낸다. 지시가 없으면 key 는 None 이고 글은 그대로다.
/// LogQL·PromQL 모두 '#' 를 주석으로 받아 지시줄을 지우지 않고 보내도 질의가 깨지지 않는다(그래도 보내기 전에 지운다).
pub(super) fn split_directive(text: &str) -> (Option<String>, String) {
    let lines: Vec<&str> = text.split('\n').collect();
    for (i, ln) in lines.iter().enumerate() {
        if ln.trim().is_empty() {
            continue;
        }
        return match directive(ln) {
            Some(key) => (Some(key), lines[i + 1..].join("\n")),
            None => (None, text.to_string()),
        };
    }
    (None, text.to_string())
}

/// Go 정규식 `(?i)^\s*#\s*datasource\s*:\s*(.+?)\s*$` 의 손 구현이다(\s 는 ASCII 공백).
fn directive(line: &str) -> Option<String> {
    let ws = |c: char| matches!(c, ' ' | '\t' | '\n' | '\x0c' | '\r');
    let s = line.trim_start_matches(ws).strip_prefix('#')?.trim_start_matches(ws);
    let word = s.get(..10)?;
    if !word.eq_ignore_ascii_case("datasource") {
        return None;
    }
    let rest = s[10..].trim_start_matches(ws).strip_prefix(':')?;
    let key = rest.trim_matches(ws);
    if !key.is_empty() {
        return Some(key.to_string());
    }
    // 공백뿐이면 (.+?) 가 마지막 공백 한 글자를 잡는다
    rest.chars().last().map(String::from)
}
