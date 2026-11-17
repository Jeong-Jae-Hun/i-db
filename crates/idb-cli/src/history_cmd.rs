//! `db history`(R11.5). Go `historycli.go` 를 옮긴다. 화면의 실행 기록과 결과 사본을 화면 없이 찍는다.
//! 기록은 프로젝트와 상관없는 개인 상태 폴더 하나라 -p 를 받지 않는다.

use std::io;

use chrono::{Local, SecondsFormat};
use idb_core::gojson;
use idb_core::gostrconv::parse_day_duration;
use idb_core::history::{self, Filter, GoTime, Results};
use idb_core::redact::mask_secrets;
use idb_core::result::QueryResult;
use idb_core::textutil::one_line;

use crate::cli::{EXIT_ERR, EXIT_OK, EXIT_USAGE, Opts};
use crate::goflag::{FlagDef, FlagSet, FlagType, ParseError, Parsed};
use crate::output::{self, CliResult};
use crate::usage_text::HISTORY_USAGE;

const HISTORY_FORMATS: [&str; 3] = ["table", "tsv", "json"];

fn check(name: &str, v: &str) -> Result<(), String> {
    if name == "since" {
        return parse_day_duration(v).map(|_| ());
    }
    Ok(())
}

fn flag_set() -> FlagSet {
    let def = |name, ty, usage: &str, default: Option<&str>| FlagDef {
        name,
        usage: usage.into(),
        ty,
        default: default.map(String::from),
    };
    FlagSet {
        defs: vec![
            def("conn", FlagType::Str, "이 접속의 기록만", None),
            def("c", FlagType::Str, "--conn 과 같음", None),
            def("since", FlagType::Value, "지금부터 거슬러 볼 기간(1h·24h·7d)", None),
            def("limit", FlagType::Int, "최대 줄 수(0 이면 전부)", Some("50")),
            def(
                "format",
                FlagType::Str,
                "출력 형식 table|tsv|json (기본: 터미널이면 table, 아니면 tsv)",
                None,
            ),
        ],
        check,
    }
}

/// 지역 시각 `2006-01-02 15:04:05` 다.
fn local_time(t: &GoTime) -> String {
    t.0.with_timezone(&Local).format("%Y-%m-%d %H:%M:%S").to_string()
}

pub fn run_history(args: &[String], stdout: &mut String, stderr: &mut String, tty: bool) -> i32 {
    let fs = flag_set();
    let mut p = Parsed::default();
    let usage = |out: &mut String| out.push_str(HISTORY_USAGE);
    let pos = match fs.parse_interleaved(&mut p, args, stderr, &usage) {
        Ok(pos) => pos,
        Err(ParseError::Help) => return EXIT_OK,
        Err(ParseError::Failed) => return EXIT_USAGE,
    };
    let mut format = p.strs.get("format").cloned().unwrap_or_default();
    if format.is_empty() {
        format = if tty { "table" } else { "tsv" }.into();
    } else if !HISTORY_FORMATS.contains(&format.as_str()) {
        stderr.push_str(&format!("db history: --format must be one of {}\n", HISTORY_FORMATS.join("|")));
        return EXIT_USAGE;
    }
    let limit = p.ints.get("limit").copied().unwrap_or(50);
    if limit < 0 {
        stderr.push_str("db history: --limit must be 0 or more\n");
        return EXIT_USAGE;
    }
    let show = pos.first().is_some_and(|a| a == "show");
    if (show && pos.len() != 2) || (!show && !pos.is_empty()) {
        stderr.push_str(HISTORY_USAGE);
        return EXIT_USAGE;
    }
    let hp = match history::default_path() {
        Ok(hp) => hp,
        Err(e) => {
            stderr.push_str(&format!("{e}\n"));
            return EXIT_ERR;
        }
    };
    let o = Opts {
        cmd: "history".into(),
        format,
        ..Default::default()
    };
    if show {
        return history_show(&hp, &pos[1], &o, stdout, stderr);
    }
    let st = match history::Store::open(&hp) {
        Ok(st) => st,
        Err(e) => {
            stderr.push_str(&format!("{e}\n"));
            return EXIT_ERR;
        }
    };
    // Go 는 -c 와 --conn 이 같은 변수다. 둘 다 주면 -c 를 쓴다(순서는 보지 않는다)
    let conn = p.strs.get("c").or(p.strs.get("conn")).cloned().unwrap_or_default();
    let mut f = Filter {
        conn,
        limit: limit as usize,
        ..Default::default()
    };
    if let Some(since) = p
        .values
        .get("since")
        .and_then(|v| v.last())
        .and_then(|v| parse_day_duration(v).ok())
        && since > 0
    {
        f.from = Some((chrono::Local::now() - chrono::TimeDelta::nanoseconds(since)).into());
    }
    // 읽기만 하므로 사본 폴더를 만들지 않는다. 없으면(DB_HISTORY_RESULTS=0 로만 썼거나 아직 없음) ◆ 없이 찍는다
    let results = Results::read_dir(&history::results_dir(&hp)).ok();
    let has = |e: &history::Entry| e.result && results.as_ref().is_some_and(|r| r.has(&e.id));
    let entries = st.list(&f);
    if o.format == "json" {
        stdout.push('[');
        for (i, e) in entries.iter().enumerate() {
            if i > 0 {
                stdout.push(',');
            }
            stdout.push_str("{\"id\":");
            gojson::write_str(stdout, &e.id);
            stdout.push_str(",\"time\":");
            gojson::write_str(stdout, &e.time.0.to_rfc3339_opts(SecondsFormat::Secs, true));
            stdout.push_str(",\"conn\":");
            gojson::write_str(stdout, &e.conn);
            stdout.push_str(",\"sql\":");
            gojson::write_str(stdout, &mask_secrets(&e.sql));
            stdout.push_str(&format!(",\"rows\":{},\"elapsed_ms\":{}", e.rows, e.elapsed / 1_000_000));
            if !e.err.is_empty() {
                stdout.push_str(",\"err\":");
                gojson::write_str(stdout, &e.err);
            }
            stdout.push_str(&format!(",\"result\":{}}}", has(e)));
        }
        stdout.push_str("]\n");
        return EXIT_OK;
    }
    let mut res = QueryResult {
        cols: ["id", "time", "conn", "rows", "elapsed", "result", "sql", "err"]
            .map(String::from)
            .to_vec(),
        ..Default::default()
    };
    for e in &entries {
        res.rows.push(vec![
            e.id.clone(),
            local_time(&e.time),
            e.conn.clone(),
            e.rows.to_string(),
            format!("{:.2}s", e.elapsed as f64 / 1e9),
            if has(e) { "◆" } else { "" }.into(),
            one_line(&mask_secrets(&e.sql)),
            one_line(&e.err),
        ]);
    }
    output::write_results(
        stdout,
        stderr,
        &o,
        vec![CliResult {
            res: Some(res),
            ..Default::default()
        }],
    )
}

/// 사본 하나를 db query 와 같은 꼴로 찍는다. table 은 위에 시각·접속·SQL 을 붙인다.
fn history_show(hp: &std::path::Path, id: &str, o: &Opts, stdout: &mut String, stderr: &mut String) -> i32 {
    let s = match Results::read_dir(&history::results_dir(hp)).and_then(|r| r.read(id)) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            stderr.push_str(&format!("결과 사본이 없음: {id}\n"));
            return EXIT_ERR;
        }
        Err(e) => {
            stderr.push_str(&format!("{e}\n"));
            return EXIT_ERR;
        }
    };
    if o.format == "table" {
        stdout.push_str(&format!("-- {} · {}\n{}\n", local_time(&s.time), s.conn, mask_secrets(&s.sql)));
    }
    if s.truncated {
        stderr.push_str(&format!("사본은 앞 {}행만 (받은 행 {})\n", s.rows.len(), s.row_count));
    }
    let res = QueryResult {
        cols: s.cols,
        types: (!s.types.is_empty()).then_some(s.types),
        rows: s.rows,
        text: s.text,
        truncated: s.result_truncated,
        elapsed: std::time::Duration::from_nanos(s.elapsed.max(0) as u64),
    };
    output::write_results(
        stdout,
        stderr,
        o,
        vec![CliResult {
            conn: s.conn,
            res: Some(res),
            ..Default::default()
        }],
    )
}
