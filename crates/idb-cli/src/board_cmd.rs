//! `db board`(R70). Go `board.go` 의 보드 읽기·패널 검사·패널 돌리기·runBoard·listBoardsCLI 를 옮긴다. 격자 그리기는 화면 쪽이라 없다.
//! 패널은 같은 접속끼리 차례로, 다른 접속끼리 4개까지 동시에 돈다. prod 패널은 --prod 가 없으면 묻지 않는다(R58.2·R64.3).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use idb_core::flows::{self, Flow};
use idb_core::gojson;
use idb_core::gostrconv::parse_duration;
use idb_core::project::{Conn, Kind, Project, go_atoi, go_quote};
use idb_core::redact::redact;
use idb_core::result::QueryResult;
use idb_core::sqlsafe::{check_read_only_sql, is_read_only_redis, split_args, split_statements};
use idb_core::textutil::printable;
use idb_sources::source::{self, Query};
use idb_sources::{Clients, Error};
use serde::Deserialize;
use tokio::sync::Semaphore;
use tokio::time::Instant;

use crate::cli::{EXIT_ERR, EXIT_OK, EXIT_USAGE, parse_day_duration};
use crate::command::Env;
use crate::flows_cmd::{FlowBox, STAGE_TYPE, flow_boxes, resolve_flow};
use crate::history_cmd::one_line;
use crate::output::{self, CliResult};
use crate::source_cmd::source_err_text;
use crate::status_cmd::DASH_PROD_HINT;

const BOARD_DIR: &str = ".dbtui/boards";
const BOARD_MIN_EVERY_NS: i64 = 10_000_000_000;
/// 패널 하나(여는 시간 포함)의 제한이다
const BOARD_TIMEOUT: Duration = Duration::from_secs(15);
/// 동시에 도는 패널 수다. 접속이 다른 패널만 함께 돈다
const BOARD_PARALLEL: usize = 4;
/// 숫자·표 패널이 받는 행 수이자 CLI json 상한이다. 차트는 시계열이라 저장소 기본(1,000)을 받는다
const BOARD_ROWS: usize = 50;
const SOURCE_DEFAULT_ROWS: usize = 1000;
/// 흐름 패널에 보일 아웃박스 종류 줄 최대 수다(R70.3)
const BOARD_FLOW_TYPES: usize = 10;
const MSG_BOARD_EXPENSIVE: &str = "비싼 질의라 보드에서 돌리지 않음";
const BOARD_VIEWS: [&str; 4] = ["number", "table", "chart", "flow"];

/// 패널 하나다. 소문자 칸(아래쪽)은 검사가 채운다. err 가 있으면 돌리지 않고 `설정 오류: <err>` 를 보인다.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Panel {
    pub title: String,
    pub conn: String,
    pub query: String,
    pub flow: String,
    pub view: String,
    pub since: String,
    pub warn: String,
    #[serde(skip)]
    err: String,
    #[serde(skip)]
    since_ns: i64,
    #[serde(skip)]
    warn_rule: Option<WarnRule>,
    #[serde(skip)]
    flow_def: Option<Flow>,
    /// 차례로 돌릴 묶음 이름(접속 이름, 흐름은 aws 접속)이다
    #[serde(skip)]
    key: String,
}

/// `.dbtui/boards/<name>.json` 하나다. err 는 파일 단위 오류(JSON·refresh)이고 패널과 따로 보인다.
pub struct Board {
    pub title: String,
    pub panels: Vec<Panel>,
    pub err: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct BoardFile {
    title: String,
    refresh: String,
    #[serde(deserialize_with = "null_panels")]
    panels: Vec<Panel>,
}

fn null_panels<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<Panel>, D::Error> {
    Ok(Option::<Vec<Panel>>::deserialize(d)?.unwrap_or_default())
}

// ─── 경고 조건 ───

/// 숫자 패널의 경고 조건(`>0`·`>=100`)이다.
#[derive(Clone, Debug, PartialEq)]
struct WarnRule {
    op: &'static str,
    v: f64,
}

fn is_re_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\x0c' | '\r')
}

/// Go 정규식 `^\s*(>=|<=|==|!=|>|<|=)\s*(-?[0-9][0-9,]*(?:\.[0-9]+)?)\s*$` 를 손으로 읽는다.
fn parse_warn(s: &str) -> Result<WarnRule, String> {
    let bad = || format!("warn must be like >0 or >=100: {}", go_quote(s));
    let t = s.trim_matches(is_re_space);
    let op = [">=", "<=", "==", "!=", ">", "<", "="]
        .into_iter()
        .find(|op| t.starts_with(op))
        .ok_or_else(bad)?;
    let num = t[op.len()..].trim_start_matches(is_re_space);
    let b = num.as_bytes();
    let mut i = usize::from(b.first() == Some(&b'-'));
    if !b.get(i).is_some_and(u8::is_ascii_digit) {
        return Err(bad());
    }
    while i < b.len() && (b[i].is_ascii_digit() || b[i] == b',') {
        i += 1;
    }
    if b.get(i) == Some(&b'.') {
        let st = i + 1;
        i = st;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == st {
            return Err(bad());
        }
    }
    if i != b.len() {
        return Err(bad());
    }
    let v = num.replace(',', "").parse::<f64>().map_err(|_| bad())?;
    Ok(WarnRule { op, v })
}

impl WarnRule {
    /// 칸이 조건에 맞는지다. 숫자가 아니면 경고하지 않는다.
    fn hit(&self, cell: &str) -> bool {
        let Ok(v) = cell.trim().replace(',', "").parse::<f64>() else {
            return false;
        };
        match self.op {
            ">" => v > self.v,
            ">=" => v >= self.v,
            "<" => v < self.v,
            "<=" => v <= self.v,
            "!=" => v != self.v,
            _ => v == self.v,
        }
    }
}

// ─── 파일 ───

/// .dbtui/boards 의 보드 이름(.json 뺀 파일 이름) 순서다. 폴더가 없으면 빈 목록이다.
fn list_boards(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(dir.join(BOARD_DIR))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| !t.is_dir()))
        .filter_map(|e| e.file_name().to_str().and_then(|n| n.strip_suffix(".json")).map(String::from))
        .filter(|n| !n.is_empty() && !n.starts_with('.'))
        .collect();
    out.sort();
    out
}

/// 보드를 못 읽은 까닭이다. NotFound 면 파일이 없다.
enum LoadErr {
    NotFound,
    Other(String),
}

/// 보드 하나를 읽는다. JSON 이 깨졌으면 패널 없이 err 를, 패널 하나가 틀리면 그 패널 err 에만 까닭을 적는다.
fn load_board(proj: &Project, name: &str) -> Result<Board, LoadErr> {
    let base_ok = !name.is_empty() && !name.contains('/') && name != ".." && !name.starts_with('.');
    if !base_ok {
        return Err(LoadErr::Other(format!(
            "board name must be a file name in {BOARD_DIR}: {}",
            go_quote(name)
        )));
    }
    let path = proj.dir.join(BOARD_DIR).join(format!("{name}.json"));
    let data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(LoadErr::NotFound),
        Err(e) => return Err(LoadErr::Other(format!("open {}: {e}", path.display()))),
    };
    let mut b = Board {
        title: name.into(),
        panels: Vec::new(),
        err: String::new(),
    };
    let f: BoardFile = match serde_json::from_slice::<Option<BoardFile>>(&data) {
        Ok(f) => f.unwrap_or_default(),
        Err(e) => {
            b.err = format!("{BOARD_DIR}/{name}.json: {}", gojson::json_error(&e));
            return Ok(b);
        }
    };
    if !f.title.trim().is_empty() {
        b.title = f.title;
    }
    if !f.refresh.is_empty() {
        match parse_duration(&f.refresh) {
            None => b.err = format!("refresh: invalid duration {}", go_quote(&f.refresh)),
            // 접속마다 질의를 보내므로 너무 잦게 돌지 않게 10초로 올린다
            Some(d) if d < BOARD_MIN_EVERY_NS => b.err = format!("refresh must be at least 10s: {}", f.refresh),
            Some(_) => {}
        }
    }
    let fs = flows::load(&proj.dir, &proj.conns);
    b.panels = f.panels;
    for (i, p) in b.panels.iter_mut().enumerate() {
        if let Err(e) = check_panel(p, proj, &fs) {
            p.err = e;
        }
        if p.title.trim().is_empty() {
            p.title = format!("#{}", i + 1);
        }
    }
    Ok(b)
}

fn uses_range(k: Option<Kind>) -> bool {
    matches!(k, Some(Kind::Loki | Kind::Prometheus | Kind::Grafana))
}

/// 패널 설정을 검사하고 돌릴 값(범위·경고·흐름·묶음)을 채운다. 쓰기로 보이는 질의는 여기서 막는다.
fn check_panel(p: &mut Panel, proj: &Project, fs: &Result<Vec<Flow>, String>) -> Result<(), String> {
    if !BOARD_VIEWS.contains(&p.view.as_str()) {
        return Err(format!("view must be one of {}: {}", BOARD_VIEWS.join("|"), go_quote(&p.view)));
    }
    if !p.warn.is_empty() {
        if p.view != "number" {
            return Err("warn is only for view number".into());
        }
        p.warn_rule = Some(parse_warn(&p.warn)?);
    }
    if p.view == "flow" {
        if p.flow.trim().is_empty() {
            return Err("flow is empty".into());
        }
        let fs = fs.as_ref().map_err(Clone::clone)?;
        let Some(f) = fs.iter().find(|f| f.name == p.flow) else {
            return Err(format!("flow {}: no such flow (.dbtui/flows.json)", p.flow));
        };
        if !f.err.is_empty() {
            return Err(format!("flow {}: {}", p.flow, f.err));
        }
        // 토픽 없는 흐름은 aws 접속이 없어 아웃박스 접속으로 묶는다
        p.key = f.aws.clone();
        if p.key.is_empty()
            && let Some(o) = &f.outbox
        {
            p.key = o.conn.clone();
        }
        p.flow_def = Some(f.clone());
        return Ok(());
    }
    if p.conn.trim().is_empty() {
        return Err("conn is empty".into());
    }
    if p.query.trim().is_empty() {
        return Err("query is empty".into());
    }
    let Some(c) = proj.conn_by_name(&p.conn).filter(|c| c.kind.is_some()) else {
        return Err(format!("conn {}: no such connection", p.conn));
    };
    p.key = c.name.clone();
    match c.kind {
        Some(Kind::Aws) => return Err(format!("conn {}: aws connections are not query panels (use view flow)", c.name)),
        Some(Kind::Pg) => {
            if split_statements(&p.query).len() != 1 {
                return Err("query must be a single statement".into());
            }
            check_read_only_sql(&p.query).map_err(|e| format!("query is not read-only: {e}"))?;
        }
        Some(Kind::Redis) => {
            let q = p.query.trim();
            let args = split_args(q);
            if q.contains('\n') {
                return Err("redis panel takes one command".into());
            }
            // CLI 와 같다(R58.2): KEYS 는 키 전체를 훑는 동안 서버를 멈춘다
            if args.first().is_some_and(|a| a.eq_ignore_ascii_case("KEYS")) {
                return Err("KEYS is not run on boards (it blocks the server): use SCAN".into());
            }
            if !is_read_only_redis(&args) {
                return Err("redis command is not a read command".into());
            }
            if !board_redis_bounded(&args) {
                return Err(format!("보드에서는 범위를 정한 읽기만: {}", args[0].to_uppercase()));
            }
        }
        _ => {}
    }
    if !p.since.is_empty() {
        if !uses_range(c.kind) {
            return Err("since is only for loki·prometheus·grafana".into());
        }
        p.since_ns = parse_day_duration(&p.since).map_err(|e| format!("since: {e}"))?;
    }
    Ok(())
}

/// 보드 Redis 패널이 받는 명령이다. 값 하나·수 하나를 돌려주는 것만 둔다.
const BOARD_REDIS_CMDS: [&str; 16] = [
    "GET", "MGET", "HGET", "HMGET", "LLEN", "SCARD", "ZCARD", "HLEN", "STRLEN", "TTL", "PTTL", "TYPE", "EXISTS", "XLEN", "INFO", "DBSIZE",
];

/// 보드에서 돌려도 되는 Redis 읽기인지다. LRANGE·ZRANGE·ZREVRANGE 는 0 이상 start ≤ stop, stop-start < BOARD_ROWS 인 색인 범위
/// (ZRANGE 는 WITHSCORES 만 덧붙임)이고, SCAN 은 COUNT 가 BOARD_ROWS 이하다.
fn board_redis_bounded(args: &[String]) -> bool {
    let cmd = args[0].to_uppercase();
    match cmd.as_str() {
        "LRANGE" | "ZRANGE" | "ZREVRANGE" => {
            if args.len() < 4 || args.len() > 5 || (args.len() == 5 && (cmd == "LRANGE" || !args[4].eq_ignore_ascii_case("WITHSCORES"))) {
                return false;
            }
            match (go_atoi(&args[2]), go_atoi(&args[3])) {
                (Some(start), Some(stop)) => start >= 0 && stop >= start && stop - start < BOARD_ROWS as i64,
                _ => false,
            }
        }
        "SCAN" => {
            let mut i = 2;
            while i + 1 < args.len() {
                if args[i].eq_ignore_ascii_case("COUNT") && go_atoi(&args[i + 1]).is_none_or(|n| n <= 0 || n > BOARD_ROWS as i64) {
                    return false;
                }
                i += 2;
            }
            args.len() >= 2 && args.len().is_multiple_of(2)
        }
        c => BOARD_REDIS_CMDS.contains(&c),
    }
}

/// 패널이 묻는 접속 이름이다(흐름은 아웃박스와, 큐가 있으면 aws 접속).
fn panel_conns(p: &Panel) -> Vec<String> {
    if p.view != "flow" {
        return vec![p.conn.clone()];
    }
    let Some(f) = &p.flow_def else { return Vec::new() };
    let mut out = Vec::new();
    if let Some(o) = &f.outbox {
        out.push(o.conn.clone());
    }
    if !f.queues.is_empty() {
        out.push(f.aws.clone());
    }
    out
}

/// 패널이 prod 접속(R6.2)을 묻는지다.
fn panel_prod(proj: &Project, p: &Panel) -> bool {
    panel_conns(p)
        .iter()
        .any(|n| proj.conn_by_name(n).is_some_and(|c| c.env() == "prod"))
}

// ─── 돌리기 ───

/// 패널 한 번의 결과다. 흐름 패널은 boxes 를 든다.
#[derive(Default)]
struct PanelResult {
    res: Option<QueryResult>,
    boxes: Vec<FlowBox>,
    err: String,
    note: String,
}

/// 패널 하나를 15초 안에 돌린다. PostgreSQL 은 READ ONLY, Redis 는 읽기 명령, 저장소는 Backend.run 이고
/// 비싼 질의(NeedsConfirm)는 force 로 다시 보내지 않는다. prod 가 거짓이면 흐름의 prod 접속은 묻지 않는다.
async fn run_panel(clients: &Clients, conns: &[Conn], p: &Panel, prod: bool, deadline: Instant) -> PanelResult {
    let mut r = PanelResult::default();
    let deadline = (Instant::now() + BOARD_TIMEOUT).min(deadline);
    if p.view == "flow" {
        let f = p.flow_def.clone().unwrap_or_default();
        let snap = resolve_flow(clients, conns, &f, prod, deadline).await;
        // 종류 줄(R69.6)은 행이 있는 것만 BOARD_FLOW_TYPES 개까지다. 행 없는 설정 종류는 맨 뒤라 앞에서 센 수에 끼지 않는다
        let mut n = 0;
        r.boxes = flow_boxes(&f, &snap)
            .into_iter()
            .filter(|b| {
                if b.stage != STAGE_TYPE {
                    return true;
                }
                n += 1;
                !b.idle && n <= BOARD_FLOW_TYPES
            })
            .collect();
        return r;
    }
    let Some(c) = conns.iter().find(|c| c.name == p.conn) else {
        r.err = format!("no such connection: {}", p.conn);
        return r;
    };
    let limit = if p.view == "chart" { SOURCE_DEFAULT_ROWS } else { BOARD_ROWS };
    let run = async {
        match c.kind {
            Some(Kind::Pg) => clients.pg(c).await?.run_read_only_rows(&p.query, limit).await,
            Some(Kind::Redis) => {
                let rc = clients.redis(c).await?;
                let mut res = rc.lock().await.run(&split_args(p.query.trim())).await?;
                // 검사가 범위를 정한 읽기만 받지만 MGET·HMGET 처럼 인자 수만큼 오는 명령도 패널 행 수에서 자른다
                if res.rows.len() > limit {
                    res.rows.truncate(limit);
                    res.truncated = true;
                }
                Ok(res)
            }
            _ => {
                let b = source::open(c).await?;
                let mut q = Query {
                    text: p.query.trim().to_string(),
                    limit,
                    deadline: Some(deadline),
                    ..Default::default()
                };
                if uses_range(c.kind) {
                    q.since_ns = if p.since_ns <= 0 { 3_600_000_000_000 } else { p.since_ns };
                }
                b.run(&q).await
            }
        }
    };
    match tokio::time::timeout_at(deadline, run).await.unwrap_or(Err(Error::Deadline)) {
        Ok(res) => r.res = Some(res),
        Err(Error::NeedsConfirm(reason)) => {
            r.err = MSG_BOARD_EXPENSIVE.into();
            if !reason.is_empty() {
                r.err.push_str(&format!(": {reason}"));
            }
        }
        Err(e) => r.err = one_line(&redact(&source_err_text(&e), &c.password)),
    }
    r
}

/// 돌릴 패널을 묶음(같은 접속)마다 차례로 늘어놓는다. 설정 오류 패널은 뺀다.
fn board_groups(panels: &[Panel]) -> Vec<Vec<usize>> {
    let mut out: Vec<Vec<usize>> = Vec::new();
    let mut at: HashMap<&str, usize> = HashMap::new();
    for (i, p) in panels.iter().enumerate().filter(|(_, p)| p.err.is_empty()) {
        let g = *at.entry(&p.key).or_insert_with(|| {
            out.push(Vec::new());
            out.len() - 1
        });
        out[g].push(i);
    }
    out
}

/// 숫자 패널 값(첫 행 첫 칸, 행이 없고 글이면 첫 줄)이다.
fn panel_value(res: Option<&QueryResult>) -> Option<String> {
    let res = res?;
    if let Some(v) = res.rows.first().and_then(|r| r.first()) {
        return Some(v.clone());
    }
    (!res.text.is_empty()).then(|| res.text.split('\n').next().unwrap_or("").to_string())
}

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
    let mut results: Vec<PanelResult> = b.panels.iter().map(|_| PanelResult::default()).collect();
    let mut run: Vec<Vec<usize>> = Vec::new();
    for g in board_groups(&b.panels) {
        let mut keep = Vec::new();
        for i in g {
            // 흐름은 상자마다 prod 를 가른다(resolve_flow)
            let p = &b.panels[i];
            if p.view != "flow" && panel_prod(proj, p) && !o.prod {
                results[i].note = DASH_PROD_HINT.into();
                continue;
            }
            keep.push(i);
        }
        run.push(keep);
    }
    // 묶음 안은 차례로, 묶음끼리는 동시에(최대 BOARD_PARALLEL) 돌린다. 같은 접속에 질의를 겹쳐 보내지 않게 한다
    let sem = Arc::new(Semaphore::new(BOARD_PARALLEL));
    let conns = Arc::new(proj.conns.clone());
    let panels = Arc::new(b.panels.clone());
    let jobs = run.into_iter().enumerate().map(|(gi, g)| {
        let (sem, conns, panels, clients, prod, deadline) = (
            sem.clone(),
            conns.clone(),
            panels.clone(),
            env.clients.clone(),
            o.prod,
            env.deadline,
        );
        (gi, async move {
            let mut out = Vec::new();
            for i in g {
                let _permit = sem.acquire().await;
                out.push((i, run_panel(&clients, &conns, &panels[i], prod, deadline).await));
            }
            out
        })
    });
    for (_, r) in idb_sources::join_indexed(jobs).await {
        for (i, pr) in r.unwrap_or_default() {
            results[i] = pr;
        }
    }

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
            row.warn = v.as_deref().is_some_and(|v| p.warn_rule.as_ref().is_some_and(|w| w.hit(v)));
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

#[cfg(test)]
mod tests;
