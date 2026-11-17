//! 보드(R70). Go `board.go` 의 보드 읽기·패널 검사·패널 돌리기다. `db board` 와 화면이 같이 쓴다. 격자 그리기는 화면 쪽이라 없다.
//! 패널은 같은 접속끼리 차례로, 다른 접속끼리 4개까지 동시에 돈다. prod 패널은 prod 를 켜지 않으면 묻지 않는다(R58.2·R64.3).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use idb_core::flows::{self, Flow};
use idb_core::gojson;
use idb_core::gostrconv::{parse_day_duration, parse_duration};
use idb_core::project::{Conn, Kind, Project, go_atoi, go_quote};
use idb_core::redact::redact;
use idb_core::result::QueryResult;
use idb_core::sqlsafe::{check_read_only_sql, is_read_only_redis, split_args, split_statements};
use idb_core::textutil::one_line;
use idb_sources::source::{self, Query};
use idb_sources::{Clients, Error};
use serde::Deserialize;
use tokio::sync::Semaphore;
use tokio::time::Instant;

use crate::flows::{DASH_PROD_HINT, FlowBox, STAGE_TYPE, flow_boxes, resolve_flow};

pub const BOARD_DIR: &str = ".dbtui/boards";
const BOARD_MIN_EVERY_NS: i64 = 10_000_000_000;
/// 패널 하나(여는 시간 포함)의 제한이다
const BOARD_TIMEOUT: Duration = Duration::from_secs(15);
/// 동시에 도는 패널 수다. 접속이 다른 패널만 함께 돈다
const BOARD_PARALLEL: usize = 4;
/// 숫자·표 패널이 받는 행 수이자 CLI json 상한이다. 차트는 시계열이라 저장소 기본(1,000)을 받는다
pub const BOARD_ROWS: usize = 50;
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
    pub err: String,
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
    /// refresh 간격(나노초). 0 이면 화면 기본(60초)이다
    pub every_ns: i64,
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

impl Panel {
    /// 숫자 패널 값 v 가 경고 조건에 맞는지다. 조건이 없으면 거짓이다.
    pub fn warn_hit(&self, v: &str) -> bool {
        self.warn_rule.as_ref().is_some_and(|w| w.hit(v))
    }
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
pub fn list_boards(dir: &Path) -> Vec<String> {
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
pub enum LoadErr {
    NotFound,
    Other(String),
}

/// 보드 하나를 읽는다. JSON 이 깨졌으면 패널 없이 err 를, 패널 하나가 틀리면 그 패널 err 에만 까닭을 적는다.
pub fn load_board(proj: &Project, name: &str) -> Result<Board, LoadErr> {
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
        every_ns: 0,
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
            Some(d) => b.every_ns = d,
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
pub fn panel_conns(p: &Panel) -> Vec<String> {
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
pub fn panel_prod(proj: &Project, p: &Panel) -> bool {
    panel_conns(p)
        .iter()
        .any(|n| proj.conn_by_name(n).is_some_and(|c| c.env() == "prod"))
}

// ─── 돌리기 ───

/// 패널 한 번의 결과다. 흐름 패널은 boxes 를 든다.
#[derive(Default)]
pub struct PanelResult {
    pub res: Option<QueryResult>,
    pub boxes: Vec<FlowBox>,
    pub err: String,
    pub note: String,
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
        Err(e) => r.err = one_line(&redact(&source::err_text(&e), &c.password)),
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
pub fn panel_value(res: Option<&QueryResult>) -> Option<String> {
    let res = res?;
    if let Some(v) = res.rows.first().and_then(|r| r.first()) {
        return Some(v.clone());
    }
    (!res.text.is_empty()).then(|| res.text.split('\n').next().unwrap_or("").to_string())
}

/// 보드의 패널을 한 번 돌린다(패널 순서). 설정 오류 패널은 빈 결과이고, prod 가 거짓이면 prod 패널은 묻지 않고 note 만 둔다.
/// 묶음 안은 차례로, 묶음끼리는 동시에(최대 BOARD_PARALLEL) 돌린다. 같은 접속에 질의를 겹쳐 보내지 않게 한다.
pub async fn run_panels(clients: &Arc<Clients>, proj: &Project, b: &Board, prod: bool, deadline: Instant) -> Vec<PanelResult> {
    let mut results: Vec<PanelResult> = b.panels.iter().map(|_| PanelResult::default()).collect();
    let mut run: Vec<Vec<usize>> = Vec::new();
    for g in board_groups(&b.panels) {
        let mut keep = Vec::new();
        for i in g {
            // 흐름은 상자마다 prod 를 가른다(resolve_flow)
            let p = &b.panels[i];
            if p.view != "flow" && panel_prod(proj, p) && !prod {
                results[i].note = DASH_PROD_HINT.into();
                continue;
            }
            keep.push(i);
        }
        run.push(keep);
    }
    let sem = Arc::new(Semaphore::new(BOARD_PARALLEL));
    let conns = Arc::new(proj.conns.clone());
    let panels = Arc::new(b.panels.clone());
    let jobs = run.into_iter().enumerate().map(|(gi, g)| {
        let (sem, conns, panels, clients, prod, deadline) = (sem.clone(), conns.clone(), panels.clone(), clients.clone(), prod, deadline);
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
    results
}

#[cfg(test)]
mod tests;
