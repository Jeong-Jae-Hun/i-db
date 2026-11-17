//! `db flows`(R69). Go `flow.go` 의 runFlows 다. 풀이·상자는 엔진(idb_engine::flows)이 하고 그림 그리기는 화면 쪽이라 없다.

use idb_core::flows;
use idb_core::gojson;
use idb_core::result::QueryResult;
use idb_core::textutil::printable;
use idb_engine::flows::resolve_flows;

use crate::cli::{EXIT_ERR, EXIT_OK, EXIT_USAGE};
use crate::command::Env;
use crate::output::{self, CliResult};

// ─── db flows ───

/// 흐름(-f 로 고른 것, 없으면 모두)을 동시에 한 번 풀어 상자마다 한 줄을 찍는다. 설정 오류·실패가 하나라도 있으면 1 이다.
pub async fn run_flows(env: &Env<'_>, stdout: &mut String, stderr: &mut String) -> i32 {
    let (o, proj) = (env.o, env.proj);
    if o.format == "csv" {
        stderr.push_str("db flows: --format must be one of table|tsv|json\n");
        return EXIT_USAGE;
    }
    let all = match flows::load(&proj.dir, &proj.conns) {
        Ok(a) => a,
        Err(e) => {
            stderr.push_str(&format!("{}\n", printable(&e, true)));
            return EXIT_ERR;
        }
    };
    let names: Vec<&String> = o.conns.iter().chain(&o.args).collect();
    let mut list = all.clone();
    if !names.is_empty() {
        list.clear();
        for n in names {
            match all.iter().find(|f| &f.name == n) {
                Some(f) => list.push(f.clone()),
                None => {
                    stderr.push_str(&format!("db flows: 흐름 없음: {n} (.dbtui/flows.json)\n"));
                    return EXIT_USAGE;
                }
            }
        }
    }
    if all.is_empty() {
        stderr.push_str("db flows: 흐름 없음 (.dbtui/flows.json)\n");
    }
    let boxes = resolve_flows(&env.clients, &proj.conns, &list, o.prod, env.deadline).await;
    let mut failed = false;
    let mut rows = Vec::new();
    for (f, bs) in list.iter().zip(boxes) {
        for mut b in bs {
            if b.failed() {
                failed = true;
                b.err = env.err_msg(&b.err, false, &[&b.conn]);
            }
            rows.push((f.name.clone(), b));
        }
    }
    if o.format == "json" {
        stdout.push('[');
        for (i, (flow, b)) in rows.iter().enumerate() {
            if i > 0 {
                stdout.push(',');
            }
            stdout.push_str("{\"flow\":");
            gojson::write_str(stdout, flow);
            stdout.push_str(",\"stage\":");
            gojson::write_str(stdout, &b.stage);
            stdout.push_str(",\"name\":");
            gojson::write_str(stdout, &b.name);
            stdout.push_str(",\"metrics\":");
            gojson::write_str(stdout, &b.metrics);
            stdout.push_str(&format!(",\"warn\":{}", b.warn));
            if !b.err.is_empty() {
                stdout.push_str(",\"error\":");
                gojson::write_str(stdout, &b.err);
            }
            if !b.note.is_empty() {
                stdout.push_str(",\"note\":");
                gojson::write_str(stdout, &b.note);
            }
            stdout.push('}');
        }
        stdout.push_str("]\n");
    } else {
        let mut res = QueryResult {
            cols: ["flow", "stage", "name", "metrics", "warn"].map(String::from).to_vec(),
            ..Default::default()
        };
        for (flow, b) in &rows {
            let m = if !b.err.is_empty() {
                b.err.clone()
            } else if !b.note.is_empty() {
                b.note.clone()
            } else {
                b.metrics.clone()
            };
            res.rows
                .push(vec![flow.clone(), b.stage.clone(), b.name.clone(), m, b.warn.to_string()]);
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
