//! `db board`(R70). Go `board.go` 의 runBoard·listBoardsCLI 다. 보드 읽기·검사·돌리기는 엔진(idb_engine::board)이 한다.

use idb_core::gojson;
use idb_core::result::QueryResult;
use idb_core::textutil::printable;
use idb_engine::board::{BOARD_DIR, BOARD_ROWS, LoadErr, list_boards, load_board, panel_conns, panel_prod, panel_value, run_panels};

use crate::cli::{EXIT_ERR, EXIT_OK, EXIT_USAGE};
use crate::command::Env;
use crate::output::{self, CliResult};

// ─── db board ───

/// `db board` 의 패널 한 줄이다. 숫자 패널은 value, 표·차트·흐름은 columns·rows(50행까지)다.
#[derive(Default)]
struct Row {
    panel: String,
    conn: String,
    view: String,
    prod: bool,
    value: Option<String>,
    warn: bool,
    columns: Option<Vec<String>>,
    rows: Vec<Vec<String>>,
    truncated: bool,
    error: String,
    note: String,
}

/// 보드 하나의 패널을 한 번 돌려 패널마다 한 줄을 찍는다. 이름이 없으면 보드 목록이다. 실패한 패널이 있으면 1 이다.
pub async fn run_board(env: &Env<'_>, stdout: &mut String, stderr: &mut String) -> i32 {
    let (o, proj) = (env.o, env.proj);
    if o.format == "csv" {
        stderr.push_str("db board: --format must be one of table|tsv|json\n");
        return EXIT_USAGE;
    }
    if o.args.len() > 1 {
        stderr.push_str("db board: 보드 이름은 하나만\n");
        return EXIT_USAGE;
    }
    let Some(name) = o.args.first() else {
        return list_boards_cli(env, stdout, stderr);
    };
    let b = match load_board(proj, name) {
        Ok(b) => b,
        Err(LoadErr::NotFound) => {
            stderr.push_str(&format!("db board: 보드 없음: {name} ({BOARD_DIR})\n"));
            return EXIT_USAGE;
        }
        Err(LoadErr::Other(e)) => {
            stderr.push_str(&format!("db board: {}\n", printable(&e, true)));
            return EXIT_USAGE;
        }
    };
    let mut failed = !b.err.is_empty();
    if !b.err.is_empty() {
        stderr.push_str(&format!("설정 오류: {}\n", printable(&b.err, true)));
    }
    let results = run_panels(&env.clients, proj, &b, o.prod, env.deadline).await;

    let mut rows = Vec::new();
    for (p, r) in b.panels.iter().zip(&results) {
        let conns = panel_conns(p);
        let conn_refs: Vec<&str> = conns.iter().map(String::as_str).collect();
        let mut row = Row {
            panel: p.title.clone(),
            conn: if p.view == "flow" { p.flow.clone() } else { p.conn.clone() },
            view: p.view.clone(),
            prod: panel_prod(proj, p),
            note: r.note.clone(),
            ..Default::default()
        };
        if !p.err.is_empty() {
            row.error = format!("설정 오류: {}", p.err);
        } else if !r.err.is_empty() {
            row.error = env.err_msg(&r.err, false, &conn_refs);
        } else if !r.note.is_empty() {
        } else if p.view == "flow" {
            row.columns = Some(["stage", "name", "metrics", "warn"].map(String::from).to_vec());
            for bx in &r.boxes {
                let mut m = bx.metrics.clone();
                if !bx.err.is_empty() {
                    m = env.err_msg(&bx.err, false, &conn_refs);
                    if row.error.is_empty() {
                        row.error = format!("{} {}: {m}", bx.stage, bx.name);
                    }
                } else if !bx.note.is_empty() {
                    m = bx.note.clone();
                }
                row.rows.push(vec![bx.stage.clone(), bx.name.clone(), m, bx.warn.to_string()]);
            }
        } else if p.view == "number" {
            let v = panel_value(r.res.as_ref());
            row.warn = v.as_deref().is_some_and(|v| p.warn_hit(v));
            row.value = Some(v.unwrap_or_default());
        } else if let Some(res) = &r.res {
            if !res.cols.is_empty() {
                row.columns = Some(res.cols.clone());
            }
            row.rows = res.rows.clone();
            if row.rows.len() > BOARD_ROWS {
                row.rows.truncate(BOARD_ROWS);
                row.truncated = true;
            }
            row.truncated = row.truncated || res.truncated;
            if row.columns.is_none() && !res.text.is_empty() {
                row.value = Some(res.text.clone());
            }
        }
        failed = failed || !row.error.is_empty();
        rows.push(row);
    }
    if o.format == "json" {
        write_rows_json(stdout, &rows);
    } else {
        let mut res = QueryResult {
            cols: ["panel", "conn", "view", "value", "error"].map(String::from).to_vec(),
            ..Default::default()
        };
        for r in &rows {
            let mut v = r.note.clone();
            if let Some(val) = &r.value {
                v = val.clone();
                if r.warn {
                    v.push_str(" !");
                }
            } else if r.columns.is_some() && r.view == "flow" {
                v = r
                    .rows
                    .iter()
                    .map(|x| {
                        let mut s = format!("{} {} {}", x[0], x[1], x[2]).trim().to_string();
                        if x[3] == "true" {
                            s.push_str(" !");
                        }
                        s
                    })
                    .collect::<Vec<_>>()
                    .join(" · ");
            } else if r.columns.is_some() {
                v = format!("{}행", r.rows.len());
                if r.truncated {
                    v.push('+');
                }
            }
            res.rows
                .push(vec![r.panel.clone(), r.conn.clone(), r.view.clone(), v, r.error.clone()]);
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

fn write_rows_json(out: &mut String, rows: &[Row]) {
    let field = |out: &mut String, k: &str, v: &str| {
        out.push_str(&format!(",\"{k}\":"));
        gojson::write_str(out, v);
    };
    out.push('[');
    for (i, r) in rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"panel\":");
        gojson::write_str(out, &r.panel);
        field(out, "conn", &r.conn);
        field(out, "view", &r.view);
        if r.prod {
            out.push_str(",\"prod\":true");
        }
        if let Some(v) = &r.value {
            field(out, "value", v);
        }
        if r.warn {
            out.push_str(",\"warn\":true");
        }
        if let Some(cols) = r.columns.as_ref().filter(|c| !c.is_empty()) {
            out.push_str(",\"columns\":");
            gojson::str_array(out, cols.iter().map(String::as_str));
        }
        if !r.rows.is_empty() {
            out.push_str(",\"rows\":[");
            for (j, row) in r.rows.iter().enumerate() {
                if j > 0 {
                    out.push(',');
                }
                gojson::str_array(out, row.iter().map(String::as_str));
            }
            out.push(']');
        }
        if r.truncated {
            out.push_str(",\"truncated\":true");
        }
        if !r.error.is_empty() {
            field(out, "error", &r.error);
        }
        if !r.note.is_empty() {
            field(out, "note", &r.note);
        }
        out.push('}');
    }
    out.push_str("]\n");
}

/// `db board`(이름 없음)의 보드 목록이다: name·title·panels·error.
fn list_boards_cli(env: &Env<'_>, stdout: &mut String, stderr: &mut String) -> i32 {
    let (o, proj) = (env.o, env.proj);
    let names = list_boards(&proj.dir);
    if names.is_empty() {
        stderr.push_str(&format!("db board: 보드 없음 ({BOARD_DIR}/<이름>.json)\n"));
    }
    let items: Vec<(String, String, usize, String)> = names
        .into_iter()
        .map(|n| match load_board(proj, &n) {
            Ok(b) => (n, b.title, b.panels.len(), b.err),
            Err(LoadErr::NotFound) => (
                n.clone(),
                String::new(),
                0,
                format!("open {BOARD_DIR}/{n}.json: no such file or directory"),
            ),
            Err(LoadErr::Other(e)) => (n, String::new(), 0, e),
        })
        .collect();
    if o.format == "json" {
        stdout.push('[');
        for (i, (name, title, panels, err)) in items.iter().enumerate() {
            if i > 0 {
                stdout.push(',');
            }
            stdout.push_str("{\"name\":");
            gojson::write_str(stdout, name);
            stdout.push_str(",\"title\":");
            gojson::write_str(stdout, title);
            stdout.push_str(&format!(",\"panels\":{panels}"));
            if !err.is_empty() {
                stdout.push_str(",\"error\":");
                gojson::write_str(stdout, err);
            }
            stdout.push('}');
        }
        stdout.push_str("]\n");
        return EXIT_OK;
    }
    let res = QueryResult {
        cols: ["name", "title", "panels", "error"].map(String::from).to_vec(),
        rows: items.into_iter().map(|(n, t, p, e)| vec![n, t, p.to_string(), e]).collect(),
        ..Default::default()
    };
    output::write_results(
        stdout,
        stderr,
        o,
        vec![CliResult {
            res: Some(res),
            ..Default::default()
        }],
    );
    EXIT_OK
}
