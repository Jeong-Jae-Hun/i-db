//! 대시보드(R64)와 대시보드·흐름·보드 탭 화면 공통(키 나눔·타이머·한 줄 그리기).
//! 확인은 접속마다 health 요청 하나이고 모두 동시에 보낸다. 다시 확인하는 동안 마지막 결과를 그대로 보이고 머리에 `확인 중 N` 만 센다.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use idb_core::textutil::{printable, string_width, truncate};
use idb_engine::{EngineError, HealthInfo, Request};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use serde_json::Value;

use crate::app::{App, Pane, Pend};
use crate::help::Focus;
use crate::theme;
use crate::tree::NodeKind;

/// 대시보드가 활성 탭인 동안 다시 확인하는 간격(R64.2)
pub const DASH_EVERY: Duration = Duration::from_secs(30);
/// 표가 이만큼 줄을 못 얻으면 요약 카드를 한 줄로 접는다(R64.1)
const DASH_MIN_ROWS: usize = 5;
const DASH_COLS: [&str; 8] = ["그룹", "이름", "환경", "종류", "상태", "지연", "버전", "수치"];
const DASH_WIDTH: [usize; 7] = [16, 24, 5, 10, 4, 7, 18];

pub type Segs = Vec<(String, Style)>;

/// 한 줄 조각들을 폭 w 안에 쓴다. 넘치면 자른다.
pub fn put(buf: &mut Buffer, x: u16, y: u16, w: usize, segs: &[(String, Style)]) {
    let mut at = 0usize;
    for (s, st) in segs {
        if at >= w {
            break;
        }
        let s = truncate(s, w - at, "…");
        buf.set_stringn(x + at as u16, y, &s, w - at, *st);
        at += string_width(&s);
    }
}

/// 폭 w 에 맞춰 자르거나 공백으로 채운다(넓은 글자는 2칸).
pub fn pad(v: &str, w: usize) -> String {
    let v = truncate(v, w, "…");
    let n = w.saturating_sub(string_width(&v));
    format!("{v}{}", " ".repeat(n))
}

/// 지금 KST 시각 `12:04:05` 다(R70.4 보드·흐름·대시보드의 확인함 시각).
pub fn kst_now() -> String {
    let kst = chrono::FixedOffset::east_opt(9 * 3600).unwrap();
    chrono::Utc::now().with_timezone(&kst).format("%H:%M:%S").to_string()
}

/// 대시보드 탭 상태다. 결과는 접속 이름으로 들고 있다.
pub struct DashState {
    pub health: HashMap<String, HealthInfo>,
    /// 진행 중인 확인(요청 id → 접속)
    pub jobs: HashMap<u64, usize>,
    pub round: u64,
    pub cursor: usize,
    pub filter: String,
    pub due: Instant,
}

impl App {
    // ─── 탭 화면 공통 ───

    pub(crate) fn pane_key(&mut self, k: &str) {
        if k == "q" && self.focus == Focus::Result {
            return self.request_quit();
        }
        match self.cur().pane {
            Some(Pane::Dash(_)) => self.dash_key(k),
            Some(Pane::Flow(_)) => self.flow_key(k),
            Some(Pane::Board(_)) => self.board_key(k),
            None => {}
        }
    }

    /// 탭 i 의 진행 중인 회차를 멈춘다(다시 볼 때·탭을 닫을 때).
    pub(crate) fn stop_pane(&mut self, i: usize) {
        let ids: Vec<u64> = match &mut self.tabs[i].pane {
            Some(Pane::Dash(d)) => d.jobs.drain().map(|(id, _)| id).collect(),
            Some(Pane::Flow(f)) => f.job.take().into_iter().collect(),
            Some(Pane::Board(b)) => b.job.take().into_iter().collect(),
            None => Vec::new(),
        };
        for id in ids {
            self.cancel(id);
            self.pending.remove(&id);
        }
    }

    /// 1초마다 부른다. 때가 된 화면은 활성 탭일 때만 다시 보고, 다른 탭이면 다음 때만 미룬다(Go dashTicked 와 같다).
    pub(crate) fn pane_tick(&mut self) {
        let cur = self.tab;
        let now = Instant::now();
        for i in 0..self.tabs.len() {
            let (due, every) = match &self.tabs[i].pane {
                Some(Pane::Dash(d)) => (d.due, DASH_EVERY),
                Some(Pane::Flow(f)) => (f.due, crate::flow::FLOW_EVERY),
                Some(Pane::Board(b)) => (b.due, b.every),
                None => continue,
            };
            if now < due {
                continue;
            }
            if i != cur {
                match &mut self.tabs[i].pane {
                    Some(Pane::Dash(d)) => d.due = now + every,
                    Some(Pane::Flow(f)) => f.due = now + every,
                    Some(Pane::Board(b)) => b.due = now + every,
                    None => {}
                }
                continue;
            }
            match self.tabs[i].pane {
                Some(Pane::Dash(_)) => self.dash_refresh(i),
                Some(Pane::Flow(_)) => self.flow_refresh(i),
                Some(Pane::Board(_)) => self.board_refresh(i, false),
                None => {}
            }
        }
    }

    pub(crate) fn find_pane_tab(&self, f: impl Fn(&Pane) -> bool) -> Option<usize> {
        self.tabs.iter().position(|t| t.pane.as_ref().is_some_and(&f))
    }

    /// 화면 탭을 새로 연다. 접속 없는 탭이다.
    pub(crate) fn new_pane_tab(&mut self, title: &str, pane: Pane) -> usize {
        let ti = self.new_tab();
        self.tabs[ti].conn = None;
        self.tabs[ti].editor.redis = false;
        self.tabs[ti].title = title.to_string();
        self.tabs[ti].pane = Some(pane);
        ti
    }

    // ─── 대시보드 ───

    /// 트리와 같은 순서(폴더 이름 순, 폴더 안은 접속 순)의 접속이다.
    pub(crate) fn conn_order(&self) -> Vec<usize> {
        let mut out = Vec::new();
        for &r in &self.tree.roots {
            for &c in &self.tree.nodes[r].children {
                if let NodeKind::Conn(i) = self.tree.nodes[c].kind {
                    out.push(i);
                }
            }
        }
        out
    }

    /// Ctrl+G: 대시보드 탭을 열거나 이미 있으면 그 탭으로 옮겨 바로 다시 확인한다. 대시보드 탭은 하나뿐이다.
    pub fn open_dash(&mut self) {
        let ti = match self.find_pane_tab(|p| matches!(p, Pane::Dash(_))) {
            Some(i) => {
                self.switch_tab(i);
                i
            }
            None => self.new_pane_tab(
                "대시보드",
                Pane::Dash(DashState {
                    health: HashMap::new(),
                    jobs: HashMap::new(),
                    round: 0,
                    cursor: 0,
                    filter: String::new(),
                    due: Instant::now(),
                }),
            ),
        };
        self.set_focus(Focus::Result);
        self.dash_refresh(ti);
    }

    /// 모든 접속을 동시에 확인한다. 진행 중인 이전 회차는 취소하고 늦게 온 결과는 round 이 낡아 버린다.
    pub(crate) fn dash_refresh(&mut self, ti: usize) {
        self.stop_pane(ti);
        let order = self.conn_order();
        let tab = self.tabs[ti].id;
        let Some(Pane::Dash(d)) = &mut self.tabs[ti].pane else { return };
        d.round += 1;
        d.due = Instant::now() + DASH_EVERY;
        let round = d.round;
        let mut jobs = HashMap::new();
        for c in order {
            let name = self.conns[c].name.clone();
            let id = self.call(Request::Health { conn: name }, Pend::Health { tab, round, conn: c });
            jobs.insert(id, c);
        }
        if let Some(Pane::Dash(d)) = &mut self.tabs[ti].pane {
            d.jobs = jobs;
        }
        self.status = "대시보드 확인 중…".into();
    }

    pub(crate) fn health_done(&mut self, id: u64, tab: u64, round: u64, conn: usize, res: Result<Value, EngineError>) {
        let Some(ti) = self.tab_index(tab) else { return };
        let Some(Pane::Dash(d)) = &mut self.tabs[ti].pane else { return };
        if d.round != round {
            return;
        }
        let h = res
            .and_then(|v| serde_json::from_value::<HealthInfo>(v).map_err(|e| EngineError::run(format!("bad result: {e}"))))
            .unwrap_or_else(|e| HealthInfo {
                err: e.error,
                ..Default::default()
            });
        d.health.insert(self.conns[conn].name.clone(), h);
        d.jobs.remove(&id);
        if d.jobs.is_empty() && self.status.starts_with("대시보드 확인 중") {
            self.status = format!("대시보드 확인함 {}", kst_now());
        }
    }

    /// 거르기(폴더·이름·환경 부분 글자)를 지난 접속이다.
    fn dash_rows(&self, filter: &str) -> Vec<usize> {
        let f = filter.to_lowercase();
        self.conn_order()
            .into_iter()
            .filter(|&i| {
                let c = &self.conns[i];
                f.is_empty() || format!("{}\x00{}\x00{}", folder_of(c), c.name, c.env).to_lowercase().contains(&f)
            })
            .collect()
    }

    fn dash_key(&mut self, k: &str) {
        let ti = self.tab;
        let Some(Pane::Dash(d)) = &self.tabs[ti].pane else { return };
        let rows = self.dash_rows(&d.filter);
        let mut cur = d.cursor.min(rows.len().saturating_sub(1)) as isize;
        match k {
            "r" => return self.dash_refresh(ti),
            "up" | "k" => cur -= 1,
            "down" | "j" => cur += 1,
            "pgup" => cur -= 10,
            "pgdown" => cur += 10,
            "home" | "g" => cur = 0,
            "end" | "G" => cur = rows.len() as isize - 1,
            "/" => {
                let f = d.filter.clone();
                return self.prompt("대시보드 거르기 (그룹·이름·환경) ", &f, move |a, v| {
                    if let Some(Pane::Dash(d)) = &mut a.tabs[ti].pane {
                        d.filter = v.trim().to_string();
                        d.cursor = 0;
                    }
                });
            }
            "esc" => {
                if let Some(Pane::Dash(d)) = &mut self.tabs[ti].pane {
                    d.filter.clear();
                }
                return;
            }
            "enter" => {
                if let Some(&c) = rows.get(cur.max(0) as usize) {
                    self.dash_select(c);
                }
                return;
            }
            _ => return,
        }
        if let Some(Pane::Dash(d)) = &mut self.tabs[ti].pane {
            d.cursor = cur.clamp(0, rows.len().saturating_sub(1) as isize) as usize;
        }
    }

    /// 트리에서 접속 노드를 골라 보이게 하고 트리로 옮긴다. 탭 접속은 바꾸지 않는다(트리 Enter 로 바꾼다).
    fn dash_select(&mut self, c: usize) {
        let Some(n) = self.tree.conn_node(c) else {
            self.status = format!("트리에서 찾지 못함: {}", self.conns[c].name);
            return;
        };
        self.tree.filter.clear();
        self.tree.select(n);
        self.set_focus(Focus::Tree);
    }

    // ─── 그리기 ───

    /// 대시보드를 편집기·결과 자리(테두리 안쪽 r)에 그린다.
    pub(crate) fn draw_dash(&self, buf: &mut Buffer, r: Rect, d: &DashState) {
        let w = r.width as usize;
        let h = r.height as usize;
        let s = self.dash_sum(d);
        let cards: Vec<(String, Segs)> = vec![
            (
                "접속".into(),
                vec![(
                    format!("{} (PG {} · Redis {} · 저장소 {})", s.total, s.pg, s.redis, s.source),
                    Style::new().fg(theme::TEXT),
                )],
            ),
            (
                "환경".into(),
                vec![
                    (format!("prod {}", s.prod), theme::env_style("prod")),
                    (" · ".into(), Style::new().fg(theme::TEXT)),
                    (format!("stg {}", s.stg), theme::env_style("stg")),
                    (format!(" · local {}", s.local), Style::new().fg(theme::TEXT)),
                ],
            ),
            (
                "연결".into(),
                vec![
                    (format!("● {}", s.ok), theme::ok()),
                    (" / ".into(), Style::new().fg(theme::TEXT)),
                    (format!("✕ {}", s.fail), theme::err()),
                ],
            ),
            ("PG 활성 세션".into(), vec![(s.pg_active.to_string(), Style::new().fg(theme::TEXT))]),
            (
                "Redis 클라이언트".into(),
                vec![(s.redis_clients.to_string(), Style::new().fg(theme::TEXT))],
            ),
            ("열린 트랜잭션".into(), vec![(s.open_tx.to_string(), Style::new().fg(theme::TEXT))]),
            (
                "prod 잠금 해제".into(),
                vec![(s.unlocked.to_string(), Style::new().fg(theme::TEXT))],
            ),
        ];
        let mut lines = card_lines(&cards, w);
        let mut extra: Vec<Segs> = Vec::new();
        if !d.filter.is_empty() {
            extra.push(vec![(format!("거르기 {} · esc 해제", printable(&d.filter, false)), theme::dim())]);
        }
        // 머리 줄 하나를 뺀 표 줄이 DASH_MIN_ROWS 보다 적으면 카드를 한 줄 요약으로 접는다
        if h.saturating_sub(lines.len() + extra.len() + 1) < DASH_MIN_ROWS {
            lines = vec![vec![(
                format!(
                    "접속 {} · prod {} · stg {} · local {} · ● {} ✕ {} · PG 활성 {} · Redis 클라이언트 {} · 트랜잭션 {} · 잠금 해제 {}",
                    s.total, s.prod, s.stg, s.local, s.ok, s.fail, s.pg_active, s.redis_clients, s.open_tx, s.unlocked
                ),
                Style::new().fg(theme::TEXT),
            )]];
        }
        lines.extend(extra);
        let rows = self.dash_rows(&d.filter);
        let room = h.saturating_sub(lines.len()).max(1);
        lines.extend(self.dash_table(d, &rows, room, self.focus != Focus::Tree));
        for (i, l) in lines.iter().take(h).enumerate() {
            put(buf, r.x, r.y + i as u16, w, l);
        }
    }

    fn dash_table(&self, d: &DashState, rows: &[usize], h: usize, focused: bool) -> Vec<Segs> {
        let mut widths: Vec<usize> = DASH_COLS[..DASH_WIDTH.len()].iter().map(|c| string_width(c)).collect();
        let cells: Vec<Vec<String>> = rows
            .iter()
            .map(|&ci| {
                let c = &self.conns[ci];
                let (mut st, mut lat, mut ver) = ("…".to_string(), String::new(), String::new());
                if let Some(hh) = d.health.get(&c.name) {
                    st = if hh.ok { "●" } else { "✕" }.into();
                    ver = hh.version.clone();
                    if hh.latency_ms > 0 {
                        lat = format!("{}ms", hh.latency_ms);
                    }
                }
                let mut name = c.name.clone();
                if c.source == "db" {
                    name.push_str(" (db)");
                }
                let row: Vec<String> = [
                    folder_of(c).to_string(),
                    name,
                    c.env.clone(),
                    theme::kind_label(&c.kind).into(),
                    st,
                    lat,
                    ver,
                ]
                .into_iter()
                .map(|v| printable(&v, false))
                .collect();
                for (i, v) in row.iter().enumerate() {
                    widths[i] = DASH_WIDTH[i].min(widths[i].max(string_width(v)));
                }
                row
            })
            .collect();
        let mut head = "  ".to_string();
        for (i, c) in DASH_COLS[..widths.len()].iter().enumerate() {
            head.push_str(&pad(c, widths[i]));
            head.push_str("  ");
        }
        head.push_str(DASH_COLS[widths.len()]);
        let mut out = vec![vec![(head, theme::dim())]];
        if rows.is_empty() {
            out.push(vec![("접속 없음".into(), theme::dim())]);
            return out;
        }
        let cur = d.cursor.min(rows.len() - 1);
        let room = h.saturating_sub(1).max(1);
        let top = (cur + 1).saturating_sub(room);
        for r in top..rows.len().min(top + room) {
            let c = &self.conns[rows[r]];
            let mut line: Segs = Vec::new();
            if r == cur {
                let st = if focused {
                    Style::new().fg(theme::ACCENT).add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(theme::TEXT)
                };
                line.push(("▸ ".into(), st));
            } else {
                line.push(("  ".into(), Style::new()));
            }
            for (i, v) in cells[r].iter().enumerate() {
                let st = match (i, v.as_str()) {
                    (2, _) => theme::env_style(&c.env),
                    (4, "●") => theme::ok(),
                    (4, "✕") => theme::err(),
                    (4, _) => theme::dim(),
                    _ => Style::new().fg(theme::TEXT),
                };
                line.push((pad(v, widths[i]), st));
                line.push(("  ".into(), Style::new()));
            }
            line.extend(dash_metrics(d, &c.name));
            out.push(line);
        }
        out
    }

    fn dash_sum(&self, d: &DashState) -> DashSummary {
        let mut s = DashSummary::default();
        for i in self.conn_order() {
            let c = &self.conns[i];
            s.total += 1;
            match c.kind.as_str() {
                "postgresql" => s.pg += 1,
                "redis" => s.redis += 1,
                _ => s.source += 1,
            }
            match c.env.as_str() {
                "prod" => {
                    s.prod += 1;
                    if self.locks.get(&i).is_some_and(|l| l.prod && !l.locked) {
                        s.unlocked += 1;
                    }
                }
                "stg" => s.stg += 1,
                _ => s.local += 1,
            }
            if self.in_tx.contains_key(&i) {
                s.open_tx += 1;
            }
            let Some(h) = d.health.get(&c.name) else { continue };
            if !h.ok {
                s.fail += 1;
                continue;
            }
            s.ok += 1;
            match c.kind.as_str() {
                "postgresql" => s.pg_active += metric_int(h, "활성"),
                "redis" => s.redis_clients += metric_int(h, "클라이언트"),
                _ => {}
            }
        }
        s
    }
}

/// 요약 카드 숫자다(R64.1). ok·fail 은 결과가 한 번이라도 온 접속을 마지막 결과로 센다.
#[derive(Default)]
struct DashSummary {
    total: usize,
    pg: usize,
    redis: usize,
    source: usize,
    prod: usize,
    stg: usize,
    local: usize,
    ok: usize,
    fail: usize,
    pg_active: i64,
    redis_clients: i64,
    open_tx: usize,
    unlocked: usize,
}

fn folder_of(c: &idb_engine::ConnSummary) -> &str {
    if c.folder.is_empty() { "기타" } else { &c.folder }
}

/// 이름이 name 인 수치 값의 앞자리 정수다(`1,234`·`12 / 100` 도 받는다). 없거나 숫자가 아니면 0 이다.
fn metric_int(h: &HealthInfo, name: &str) -> i64 {
    let Some(m) = h.metrics.iter().find(|m| m.name == name) else {
        return 0;
    };
    let v = m.value.trim().replace(',', "");
    let digits: String = v.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().unwrap_or(0)
}

/// 수치 칸이다. warn 인 수치는 경고 색으로, 실패면 오류 글을 빨강으로 보인다.
fn dash_metrics(d: &DashState, name: &str) -> Segs {
    let Some(h) = d.health.get(name) else { return Vec::new() };
    if !h.ok {
        return vec![(printable(&idb_core::textutil::one_line(&h.err), false), theme::err())];
    }
    let mut out = Vec::new();
    for (i, m) in h.metrics.iter().enumerate() {
        if i > 0 {
            out.push((" · ".into(), Style::new().fg(theme::TEXT)));
        }
        let text = printable(format!("{} {}", m.name, m.value).trim(), false);
        out.push((text, if m.warn { theme::warn() } else { Style::new().fg(theme::TEXT) }));
    }
    out
}

/// 테두리 카드(제목 흐리게·값)를 폭 w 에 맞게 왼쪽부터 채우고 넘치면 다음 줄로 넘긴다. 카드 줄 하나는 네 줄이다.
fn card_lines(cards: &[(String, Segs)], w: usize) -> Vec<Segs> {
    let mut out: Vec<Segs> = Vec::new();
    let mut row: Vec<[Segs; 4]> = Vec::new();
    let mut width = 0;
    let border = Style::new().fg(theme::BORDER);
    let flush = |row: &mut Vec<[Segs; 4]>, out: &mut Vec<Segs>| {
        for k in 0..4 {
            let mut line = Vec::new();
            for (i, c) in row.iter().enumerate() {
                if i > 0 {
                    line.push((" ".to_string(), Style::new()));
                }
                line.extend(c[k].clone());
            }
            out.push(line);
        }
        row.clear();
    };
    for (title, value) in cards {
        let vw: usize = value.iter().map(|(s, _)| string_width(s)).sum();
        let inner = string_width(title).max(vw);
        let cw = inner + 4;
        if !row.is_empty() && width + 1 + cw > w {
            flush(&mut row, &mut out);
            width = 0;
        }
        if !row.is_empty() {
            width += 1;
        }
        width += cw;
        let mut mid = vec![("│ ".to_string(), border)];
        mid.extend(value.clone());
        mid.push((format!("{} │", " ".repeat(inner - vw)), border));
        row.push([
            vec![(format!("╭{}╮", "─".repeat(inner + 2)), border)],
            vec![("│ ".into(), border), (pad(title, inner), theme::dim()), (" │".into(), border)],
            mid,
            vec![(format!("╰{}╯", "─".repeat(inner + 2)), border)],
        ]);
    }
    if !row.is_empty() {
        flush(&mut row, &mut out);
    }
    out
}
