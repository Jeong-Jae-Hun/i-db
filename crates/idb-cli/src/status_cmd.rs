//! `db status`(R64.3). Go `status.go`·`dash.go`(probeAny·connOrder) 를 옮긴다.
//! 고른 접속(없으면 모두)을 동시에 한 번 확인해 찍는다. prod 는 --prod 가 없으면 연결하지 않고 `--prod 로 확인` 으로 보인다(R58.2).

use std::time::Instant;

use idb_core::gojson;
use idb_core::project::{Conn, Kind, Project, folder_of, group_of};
use idb_core::result::QueryResult;
use idb_sources::health::{self, Health, failed};
use idb_sources::join_indexed;

use crate::cli::{EXIT_ERR, EXIT_OK, EXIT_USAGE};
use crate::command::Env;
use crate::output::{self, CliResult};

pub use idb_engine::flows::DASH_PROD_HINT;

// ─── 트리 순서 (R66.1) ───

fn env_rank(c: &Conn) -> u8 {
    match c.env() {
        "local" => 0,
        "stg" => 1,
        _ => 2,
    }
}

fn is_read_conn(c: &Conn) -> bool {
    c.name.contains("-read") || c.host.starts_with("read.") || c.host.contains(".cluster-ro-")
}

/// 같은 환경 안의 순서다: 쓰기 DB → 읽기 DB → Redis → 검색 → 로그·지표 → DynamoDB → Athena → aws.
fn conn_rank(c: &Conn) -> u8 {
    let read = u8::from(is_read_conn(c));
    match c.kind {
        Some(Kind::Pg) => 1 + read,
        Some(Kind::Redis) => 3 + read,
        Some(Kind::OpenSearch) => 5,
        Some(Kind::Loki | Kind::Prometheus | Kind::Grafana) => 6,
        Some(Kind::Athena) => 8,
        Some(Kind::Aws) => 9,
        _ => 7,
    }
}

/// 트리에 보이는 순서의 접속 자리다(Go connOrder·connRoots). 폴더 이름 순(그룹 없음 `기타` 는 맨 뒤), 폴더 안은 환경·종류·이름 순이다.
pub fn conn_order(conns: &[Conn]) -> Vec<usize> {
    let other = group_of(&Conn::default()).to_string();
    let mut folders: Vec<(&str, Vec<usize>)> = Vec::new();
    for (i, c) in conns.iter().enumerate().filter(|(_, c)| c.kind.is_some()) {
        let f = folder_of(c);
        match folders.iter_mut().find(|(n, _)| *n == f) {
            Some((_, v)) => v.push(i),
            None => folders.push((f, vec![i])),
        }
    }
    folders.sort_by(|a, b| (a.0 == other).cmp(&(b.0 == other)).then_with(|| a.0.cmp(b.0)));
    folders
        .into_iter()
        .flat_map(|(_, mut v)| {
            v.sort_by(|&a, &b| {
                let (ca, cb) = (&conns[a], &conns[b]);
                env_rank(ca)
                    .cmp(&env_rank(cb))
                    .then(conn_rank(ca).cmp(&conn_rank(cb)))
                    .then_with(|| ca.name.cmp(&cb.name))
            });
            v
        })
        .collect()
}

// ─── 확인 ───

/// status 의 접속 한 줄이다. ok 가 None 이면 확인하지 않은 prod 접속이다(--prod 없음).
struct Row {
    group: String,
    name: String,
    env: String,
    kind: String,
    ok: Option<bool>,
    latency_ms: u128,
    version: String,
    metrics: Vec<health::Metric>,
    error: String,
    note: String,
}

pub async fn run_status(env: &Env<'_>, stdout: &mut String, stderr: &mut String) -> i32 {
    let (o, proj): (_, &Project) = (env.o, env.proj);
    if o.format == "csv" {
        stderr.push_str("db status: --format must be one of table|tsv|json\n");
        return EXIT_USAGE;
    }
    let mut order = conn_order(&proj.conns);
    let names: Vec<&String> = o.conns.iter().chain(&o.args).collect();
    if !names.is_empty() {
        order.clear();
        for n in names {
            match proj.conns.iter().position(|c| &c.name == n && c.kind.is_some()) {
                Some(i) => order.push(i),
                None => {
                    stderr.push_str(&format!("db status: 접속 없음: {n} (db conns)\n"));
                    return EXIT_USAGE;
                }
            }
        }
    }
    let mut rows: Vec<Row> = order
        .iter()
        .map(|&i| {
            let c = &proj.conns[i];
            Row {
                group: folder_of(c).into(),
                name: c.name.clone(),
                env: c.env().into(),
                kind: c.kind_str().into(),
                ok: None,
                latency_ms: 0,
                version: String::new(),
                metrics: Vec::new(),
                error: String::new(),
                note: String::new(),
            }
        })
        .collect();
    let mut jobs = Vec::new();
    for (r, &i) in order.iter().enumerate() {
        let c = proj.conns[i].clone();
        if c.env() == "prod" && !o.prod {
            rows[r].note = DASH_PROD_HINT.into();
            continue;
        }
        let (clients, deadline) = (env.clients.clone(), env.deadline);
        // 접속마다 전체 시간(--timeout)을 따로 건다. 끝난 접속의 결과는 다른 접속이 늦어도 남는다
        jobs.push((r, async move {
            let start = Instant::now();
            tokio::time::timeout_at(deadline, health::probe_any(&clients, &c))
                .await
                .unwrap_or_else(|_| failed(start.elapsed(), "context deadline exceeded", &c.password))
        }));
    }
    for (r, h) in join_indexed(jobs).await {
        let h = h.unwrap_or_else(|e| Health {
            err: e.to_string(),
            ..Default::default()
        });
        apply(env, &mut rows[r], h);
    }
    let failed = rows.iter().any(|r| r.ok == Some(false));
    if o.format == "json" {
        write_json(stdout, &rows);
    } else {
        let mut res = QueryResult {
            cols: ["group", "name", "env", "kind", "ok", "latency_ms", "version", "metrics"]
                .map(String::from)
                .to_vec(),
            types: Some(["", "", "", "", "", "int8", "", ""].map(String::from).to_vec()),
            ..Default::default()
        };
        for r in &rows {
            let (mut ok, mut lat, mut metrics) = ("-".to_string(), String::new(), r.note.clone());
            if let Some(v) = r.ok {
                ok = v.to_string();
                lat = r.latency_ms.to_string();
                metrics = r
                    .metrics
                    .iter()
                    .map(|x| {
                        let mut s = format!("{} {}", x.name, x.value).trim().to_string();
                        if x.warn {
                            s.push_str(" !");
                        }
                        s
                    })
                    .collect::<Vec<_>>()
                    .join(" · ");
                if !r.error.is_empty() {
                    metrics = r.error.clone();
                }
            }
            res.rows.push(vec![
                r.group.clone(),
                r.name.clone(),
                r.env.clone(),
                r.kind.clone(),
                ok,
                lat,
                r.version.clone(),
                metrics,
            ]);
        }
        output::write_results(
            stdout,
            stderr,
            o,
            vec![CliResult {
                res: Some(res),
                ..Default::default()
            }],
        );
    }
    if failed { EXIT_ERR } else { EXIT_OK }
}

fn apply(env: &Env<'_>, row: &mut Row, h: Health) {
    row.ok = Some(h.ok);
    row.latency_ms = h.latency.as_millis();
    row.version = h.version;
    row.metrics = h.metrics;
    if !h.ok {
        row.error = env.err_msg(&h.err, false, &[&row.name]);
    }
}

/// Go json.Encoder(SetEscapeHTML(false)) 의 statusRow 배열과 같은 글이다.
fn write_json(out: &mut String, rows: &[Row]) {
    out.push('[');
    for (i, r) in rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let s = |out: &mut String, k: &str, v: &str| {
            out.push_str(&format!("\"{k}\":"));
            gojson::write_str(out, v);
        };
        out.push('{');
        s(out, "group", &r.group);
        out.push(',');
        s(out, "name", &r.name);
        out.push(',');
        s(out, "env", &r.env);
        out.push(',');
        s(out, "kind", &r.kind);
        out.push_str(&format!(
            ",\"ok\":{},\"latency_ms\":{},",
            r.ok.map_or("null".to_string(), |v| v.to_string()),
            r.latency_ms
        ));
        s(out, "version", &r.version);
        out.push_str(",\"metrics\":[");
        for (j, m) in r.metrics.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            out.push('{');
            s(out, "name", &m.name);
            out.push(',');
            s(out, "value", &m.value);
            out.push_str(&format!(",\"warn\":{}}}", m.warn));
        }
        out.push(']');
        if !r.error.is_empty() {
            out.push(',');
            s(out, "error", &r.error);
        }
        if !r.note.is_empty() {
            out.push(',');
            s(out, "note", &r.note);
        }
        out.push('}');
    }
    out.push_str("]\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 트리_순서는_폴더_이름순이고_기타는_맨_뒤_폴더_안은_환경_종류_이름순이다() {
        let c = |name: &str, group: &str, kind: Kind| Conn {
            name: name.into(),
            group: group.into(),
            kind: Some(kind),
            ..Default::default()
        };
        let conns = vec![
            c("z-prod", "b", Kind::Pg),
            c("free", "", Kind::Pg),
            c("aws-stg", "a", Kind::Aws),
            c("ddb-stg", "a", Kind::DynamoDb),
            c("app-stg-read", "a", Kind::Pg),
            c("app-stg", "a", Kind::Pg),
            c("cache-stg", "a", Kind::Redis),
            c("local-db", "a", Kind::Pg),
            Conn::default(),
        ];
        let names: Vec<&str> = conn_order(&conns).iter().map(|&i| conns[i].name.as_str()).collect();
        assert_eq!(
            names,
            [
                "local-db",
                "app-stg",
                "app-stg-read",
                "cache-stg",
                "ddb-stg",
                "aws-stg",
                "z-prod",
                "free"
            ]
        );
    }
}
