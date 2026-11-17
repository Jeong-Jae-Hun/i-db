//! 쿼리 보드 탭(R70.4). 보드 한 회차는 Engine board_run 요청 하나다(패널 묶음·동시 4개는 serve 가 지킨다).
//! 패널 질의·범위와 refresh 간격은 board_run 답에 없어 로컬 프로젝트에서만 보드 파일을 읽어 쓴다(원격은 기본 60초).

use std::time::{Duration, Instant};

use idb_core::gostrconv::{duration_string, parse_day_duration};
use idb_core::textutil::{one_line, printable, string_width, truncate};
use idb_engine::{BoardResult, BoardSummary, EngineError, PanelOut, Request, ResultSet};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use serde::Deserialize;
use serde_json::Value;

use crate::app::{App, Pane, Pend};
use crate::dash::{Segs, kst_now, pad, put};
use crate::help::Focus;
use crate::theme;

const BOARD_DIR: &str = ".dbtui/boards";
const BOARD_NO_FILE: &str = "보드 없음 · .dbtui/boards/<이름>.json 을 쓰세요";
const BOARD_DEFAULT_EVERY: Duration = Duration::from_secs(60);
const BOARD_MIN_EVERY: Duration = Duration::from_secs(10);

/// 보드 파일에서 화면이 쓰는 칸(패널 질의·범위·refresh)이다. 검사는 serve 가 한다.
#[derive(Deserialize, Default, Clone)]
#[serde(default)]
pub struct PanelConf {
    pub conn: String,
    pub query: String,
    pub since: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct BoardConf {
    refresh: String,
    panels: Option<Vec<PanelConf>>,
}

pub struct BoardState {
    pub name: String,
    pub res: Option<BoardResult>,
    pub cursor: usize,
    pub round: u64,
    pub job: Option<u64>,
    /// 마지막 회차가 끝난 KST 시각과 걸린 시간
    pub at: String,
    pub elapsed_ms: u64,
    pub started: Instant,
    pub every: Duration,
    pub due: Instant,
    /// 로컬 보드 파일의 패널(원격이면 비어 있다)
    pub conf: Vec<PanelConf>,
}

/// 보드 영역 폭에 맞는 패널 열 수다: 80칸 아래 1, 140칸 아래 2, 그 위 3.
fn board_cols(w: usize) -> usize {
    if w < 80 {
        1
    } else if w < 140 {
        2
    } else {
        3
    }
}

/// 패널 안쪽 줄 수(머리 두 줄 포함)다: 숫자·오류 4, 표·차트 10, 흐름은 상자 수 + 2.
fn panel_height(p: &PanelOut) -> usize {
    if p.error.starts_with("설정 오류") || p.view == "number" {
        return 4;
    }
    if p.view == "flow" {
        return (p.boxes.len() + 2).max(4);
    }
    10
}

impl App {
    /// Alt+B·팔레트 `보드`: 보드 목록을 받아 고르게 한다.
    pub fn pick_board(&mut self) {
        self.call(Request::BoardList, Pend::BoardList);
    }

    pub(crate) fn board_list_done(&mut self, res: Result<Value, EngineError>) {
        match res.and_then(|v| serde_json::from_value::<Vec<BoardSummary>>(v).map_err(|e| EngineError::run(format!("bad result: {e}")))) {
            Ok(list) if list.is_empty() => self.status = BOARD_NO_FILE.into(),
            Ok(list) => {
                let names = list.into_iter().map(|b| b.name).collect();
                self.picker("보드", names, |a, name| a.open_board(&name));
            }
            Err(e) => self.status = format!("보드 목록을 읽지 못함: {}", one_line(&e.error)),
        }
    }

    /// 보드 탭을 열거나 같은 보드 탭이 있으면 그 탭으로 옮겨 바로 다시 돌린다. 보드마다 탭 하나다.
    pub fn open_board(&mut self, name: &str) {
        if let Some(i) = self.find_pane_tab(|p| matches!(p, Pane::Board(b) if b.name == name)) {
            self.switch_tab(i);
            self.set_focus(Focus::Result);
            return self.board_refresh(i, true);
        }
        let now = Instant::now();
        let ti = self.new_pane_tab(
            name,
            Pane::Board(BoardState {
                name: name.to_string(),
                res: None,
                cursor: 0,
                round: 0,
                job: None,
                at: String::new(),
                elapsed_ms: 0,
                started: now,
                every: BOARD_DEFAULT_EVERY,
                due: now,
                conf: Vec::new(),
            }),
        );
        self.set_focus(Focus::Result);
        self.board_refresh(ti, true);
    }

    /// 로컬 보드 파일의 refresh 간격과 패널 질의다. 원격·없음·깨짐이면 None 이다.
    fn board_conf(&self, name: &str) -> Option<(Duration, Vec<PanelConf>)> {
        if self.remote {
            return None;
        }
        let data = std::fs::read(std::path::Path::new(&self.dir).join(BOARD_DIR).join(format!("{name}.json"))).ok()?;
        let c: BoardConf = serde_json::from_slice::<Option<BoardConf>>(&data).ok()?.unwrap_or_default();
        let every = match parse_day_duration(&c.refresh) {
            Ok(ns) if ns > 0 => Duration::from_nanos(ns as u64).max(BOARD_MIN_EVERY),
            _ => BOARD_DEFAULT_EVERY,
        };
        Some((every, c.panels.unwrap_or_default()))
    }

    /// 진행 중인 회차를 취소하고 다시 돌린다. reload 면(r·열기) 파일도 다시 읽는다. 화면은 prod 패널도 돌린다(읽기 전용).
    pub(crate) fn board_refresh(&mut self, ti: usize, reload: bool) {
        self.stop_pane(ti);
        let tab = self.tabs[ti].id;
        let name = match &self.tabs[ti].pane {
            Some(Pane::Board(b)) => b.name.clone(),
            _ => return,
        };
        let conf = if reload { self.board_conf(&name) } else { None };
        let Some(Pane::Board(b)) = &mut self.tabs[ti].pane else { return };
        if let Some((every, panels)) = conf {
            b.every = every;
            b.conf = panels;
        }
        b.round += 1;
        b.started = Instant::now();
        b.due = b.started + b.every;
        let round = b.round;
        let id = self.call(Request::BoardRun { name, prod: true }, Pend::Board { tab, round });
        if let Some(Pane::Board(b)) = &mut self.tabs[ti].pane {
            b.job = Some(id);
        }
        self.status = "보드 확인 중…".into();
    }

    pub(crate) fn board_done(&mut self, tab: u64, round: u64, res: Result<Value, EngineError>) {
        let Some(ti) = self.tab_index(tab) else { return };
        let Some(Pane::Board(b)) = &mut self.tabs[ti].pane else { return };
        if b.round != round {
            return;
        }
        b.job = None;
        match res.and_then(|v| serde_json::from_value::<BoardResult>(v).map_err(|e| EngineError::run(format!("bad result: {e}")))) {
            Ok(r) => {
                b.at = kst_now();
                b.elapsed_ms = b.started.elapsed().as_millis() as u64;
                b.cursor = b.cursor.min(r.panels.len().saturating_sub(1));
                let title = r.title.clone();
                // serve 가 refresh 간격을 보내면(원격 포함) 그것을 쓴다. 옛 serve 는 0 이라 로컬 파일 값을 둔다
                if r.every_secs > 0 {
                    b.every = Duration::from_secs(r.every_secs).max(BOARD_MIN_EVERY);
                    b.due = b.started + b.every;
                }
                b.res = Some(r);
                self.tabs[ti].title = title;
                if self.status.starts_with("보드 확인 중") {
                    self.status = format!("보드 확인함 {}", kst_now());
                }
            }
            Err(e) => {
                let first = b.res.is_none();
                self.status = if first {
                    format!("보드를 열지 못함: {}", one_line(&e.error))
                } else {
                    format!("보드를 다시 돌리지 못해 이전 결과를 보임: {}", one_line(&e.error))
                };
            }
        }
    }

    pub(crate) fn board_key(&mut self, k: &str) {
        let ti = self.tab;
        let cols = board_cols(self.layout().right_w as usize);
        let Some(Pane::Board(b)) = &mut self.tabs[ti].pane else { return };
        let n = b.res.as_ref().map_or(0, |r| r.panels.len());
        let mut cur = b.cursor as isize;
        match k {
            "r" => return self.board_refresh(ti, true),
            "left" | "h" => cur -= 1,
            "right" | "l" => cur += 1,
            "up" | "k" => cur -= cols as isize,
            "down" | "j" if b.cursor + cols < n => cur += cols as isize,
            "home" | "g" => cur = 0,
            "end" | "G" => cur = n as isize - 1,
            "enter" if n > 0 => {
                let i = b.cursor.min(n - 1);
                let p = b.res.as_ref().unwrap().panels[i].clone();
                // serve 가 패널 질의를 보내면 그것을, 아니면(옛 serve) 로컬 보드 파일 값을 쓴다
                let conf = if p.query.is_empty() {
                    (b.conf.len() == n).then(|| b.conf[i].clone())
                } else {
                    Some(PanelConf {
                        conn: p.conn.clone(),
                        query: p.query.clone(),
                        since: p.since.clone(),
                    })
                };
                return self.open_panel(p, conf);
            }
            _ => return,
        }
        b.cursor = cur.clamp(0, n.saturating_sub(1) as isize) as usize;
    }

    /// 패널 질의를 새 쿼리 탭에 채우고 그 접속에서 보통 실행한다(R6 확인 그대로). 흐름 패널은 흐름 탭에서 그 흐름을 고른다.
    fn open_panel(&mut self, p: PanelOut, conf: Option<PanelConf>) {
        if p.error.starts_with("설정 오류") {
            self.status = one_line(&p.error);
            return;
        }
        if p.view == "flow" {
            self.open_flows(None);
            self.flow_want(&p.flow);
            return;
        }
        let Some(conf) = conf.filter(|c| c.conn == p.conn && !c.query.is_empty()) else {
            self.status = if self.remote {
                "원격 접속에서는 패널 질의를 열지 않음".into()
            } else {
                "패널 질의를 읽지 못함 (r 로 보드 파일을 다시 읽기)".into()
            };
            return;
        };
        let Some(ci) = self.conns.iter().position(|c| c.name == p.conn) else {
            self.status = format!("접속 없음: {}", p.conn);
            return;
        };
        let ti = self.new_tab();
        self.tabs[ti].title = p.title.clone();
        self.tabs[ti].editor.set_value(&conf.query);
        self.set_conn(ti, Some(ci));
        if crate::sources::uses_range(&self.conns[ci].kind) {
            self.tabs[ti].since = parse_day_duration(&conf.since).unwrap_or(0);
        }
        self.set_focus(Focus::Editor);
        self.tabs[ti].editor.key("ctrl+a");
        self.run_current(None);
    }

    // ─── 그리기 ───

    /// 패널 격자를 편집기·결과 자리에 그린다. 고른 패널이 보이게 위를 맞춘다.
    pub(crate) fn draw_board(&self, buf: &mut Buffer, r: Rect, b: &BoardState) {
        let (w, h) = (r.width as usize, r.height as usize);
        let Some(res) = &b.res else {
            put(buf, r.x, r.y, w, &[("보드 확인 중…".into(), theme::dim())]);
            return;
        };
        let mut y = r.y;
        if !res.error.is_empty() {
            put(
                buf,
                r.x,
                y,
                w,
                &[(printable(&format!("설정 오류: {}", res.error), false), theme::err())],
            );
            y += 1;
        }
        if res.panels.is_empty() {
            if res.error.is_empty() {
                put(buf, r.x, y, w, &[("패널 없음 · panels 에 패널을 더하세요".into(), theme::dim())]);
            }
            return;
        }
        let cols = board_cols(w);
        let pw = ((w.saturating_sub(cols - 1)) / cols).max(12);
        let cur = b.cursor.min(res.panels.len() - 1);
        let rows: Vec<(usize, usize, usize)> = (0..res.panels.len())
            .step_by(cols)
            .map(|start| {
                let end = (start + cols).min(res.panels.len());
                let rh = res.panels[start..end].iter().map(panel_height).max().unwrap_or(4);
                (start, end, rh + 2)
            })
            .collect();
        let cur_row = cur / cols;
        // 고른 줄이 보이도록 그 줄부터 위로 들어가는 만큼 올라간다
        let room = (r.y as usize + h).saturating_sub(y as usize);
        let mut top = cur_row;
        let mut used = rows[cur_row].2;
        while top > 0 && used + rows[top - 1].2 <= room {
            used += rows[top - 1].2;
            top -= 1;
        }
        let bottom = r.y + r.height;
        for &(start, end, rh) in &rows[top..] {
            if y >= bottom {
                break;
            }
            for i in start..end {
                let x = r.x + ((i - start) * (pw + 1)) as u16;
                let rect = Rect::new(x, y, pw as u16, (rh as u16).min(bottom - y));
                self.draw_panel(buf, rect, b, &res.panels[i], i == cur && self.focus != Focus::Tree);
            }
            y += rh as u16;
        }
    }

    fn draw_panel(&self, buf: &mut Buffer, r: Rect, b: &BoardState, p: &PanelOut, focused: bool) {
        let border = Style::new().fg(if focused { theme::ACCENT } else { theme::BORDER });
        let (w, h) = (r.width as usize, r.height as usize);
        if w < 4 || h < 2 {
            return;
        }
        let bottom_y = r.y + r.height - 1;
        put(buf, r.x, r.y, w, &[(format!("╭{}╮", "─".repeat(w - 2)), border)]);
        for y in r.y + 1..bottom_y {
            put(buf, r.x, y, 1, &[("│".into(), border)]);
            put(buf, r.x + r.width - 1, y, 1, &[("│".into(), border)]);
        }
        put(buf, r.x, bottom_y, w, &[(format!("╰{}╯", "─".repeat(w - 2)), border)]);
        let iw = w.saturating_sub(4).max(1);
        let mut title: Segs = vec![(
            printable(&p.title, false),
            Style::new().fg(theme::TEXT).add_modifier(Modifier::BOLD),
        )];
        let src = if p.view == "flow" {
            format!("흐름 {}", p.flow)
        } else {
            p.conn.clone()
        };
        title.push((format!(" · {}", printable(&src, false)), theme::dim()));
        if p.prod {
            title.push((" prod".into(), Style::new().fg(theme::PROD).add_modifier(Modifier::BOLD)));
        }
        let mut meta = if p.error.starts_with("설정 오류") {
            "돌리지 않음".to_string()
        } else {
            format!("{} · {}ms", b.at, b.elapsed_ms)
        };
        if b.job.is_some() {
            meta.push_str(" · 확인 중");
        }
        let mut lines: Vec<Segs> = vec![title, vec![(meta, theme::dim())]];
        let body = h.saturating_sub(4);
        if !p.error.is_empty() {
            lines.extend(wrap_lines(theme::err(), &p.error, iw, body));
        } else if !p.note.is_empty() {
            lines.extend(wrap_lines(theme::dim(), &p.note, iw, body));
        } else {
            lines.extend(panel_body(p, iw, body));
        }
        for (i, l) in lines.iter().take(h.saturating_sub(2)).enumerate() {
            put(buf, r.x + 2, r.y + 1 + i as u16, iw, l);
        }
    }

    /// 보드 탭 머리의 `확인 중 N` 수다.
    pub(crate) fn board_pending(b: &BoardState) -> usize {
        if b.job.is_none() {
            0
        } else {
            b.res.as_ref().map_or(1, |r| r.panels.len().max(1))
        }
    }

    /// 보드 탭 머리 글 `패널 N · 1m0s마다` 다.
    pub(crate) fn board_head(b: &BoardState) -> String {
        format!(
            "패널 {} · {}마다",
            b.res.as_ref().map_or(b.conf.len(), |r| r.panels.len()),
            duration_string(b.every.as_nanos() as i64)
        )
    }
}

/// 글을 폭 w 로 접어 최대 n 줄을 st 로 칠한다.
fn wrap_lines(st: Style, s: &str, w: usize, n: usize) -> Vec<Segs> {
    crate::grid::wrap(&printable(&one_line(s), false), w)
        .into_iter()
        .take(n.max(1))
        .map(|l| vec![(l, st)])
        .collect()
}

/// 결과 본문 n 줄 이하다. 차트는 아직 그리지 않아 표로 보인다.
fn panel_body(p: &PanelOut, w: usize, n: usize) -> Vec<Segs> {
    match p.view.as_str() {
        "number" => match p.value.as_deref().filter(|v| !v.is_empty()) {
            None => vec![vec![("행 없음".into(), theme::dim())]],
            Some(v) => {
                let st = if p.warn {
                    theme::warn()
                } else {
                    Style::new().fg(theme::TEXT).add_modifier(Modifier::BOLD)
                };
                vec![vec![(truncate(&printable(v, false), w, "…"), st)]]
            }
        },
        "flow" => crate::flow::panel_lines(&p.boxes),
        _ => compact_table(p.result.as_ref(), w, n),
    }
}

/// 머리 줄과 들어가는 만큼의 행이다. 열 폭은 내용에 맞추되 20칸을 넘지 않는다.
fn compact_table(res: Option<&ResultSet>, w: usize, n: usize) -> Vec<Segs> {
    let Some(res) = res else { return Vec::new() };
    if res.cols.is_empty() {
        let t = if res.text.is_empty() { "(빈 결과)" } else { &res.text };
        return wrap_lines(Style::new().fg(theme::TEXT), t, w, n);
    }
    let rows = &res.rows[..res.rows.len().min(n.saturating_sub(1))];
    let mut widths: Vec<usize> = res.cols.iter().map(|c| string_width(&printable(c, false)).min(20)).collect();
    for row in rows {
        for (i, wd) in widths.iter_mut().enumerate() {
            if let Some(v) = row.get(i) {
                *wd = (*wd).max(string_width(&printable(v, false))).min(20);
            }
        }
    }
    let line = |cells: &[String]| {
        let s: String = widths
            .iter()
            .enumerate()
            .map(|(i, &wd)| format!("{}  ", pad(&printable(cells.get(i).map_or("", String::as_str), false), wd)))
            .collect();
        truncate(s.trim_end(), w, "…")
    };
    let mut out = vec![vec![(line(&res.cols), theme::dim())]];
    for row in rows {
        out.push(vec![(line(row), Style::new().fg(theme::TEXT))]);
    }
    if res.rows.is_empty() {
        out.push(vec![("행 없음".into(), theme::dim())]);
    }
    out
}
