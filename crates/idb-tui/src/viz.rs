//! 결과 차트(R28 c)와 고정 결과 비교(R31.2 =). 둘 다 받은 행 안에서 계산하고 결과 영역에 그린다.

use std::collections::HashMap;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use unicode_width::UnicodeWidthStr;

use crate::app::App;
use crate::grid::{Grid, Kind, is_null};
use crate::theme;

/// 차트 한 벌이다. line 이면 첫 열이 날짜·시간이라 선, 아니면 가로 막대다.
pub struct Chart {
    pub title: String,
    pub line: bool,
    pub points: Vec<(String, f64)>,
    pub skipped: usize,
}

impl Chart {
    /// 값은 커서 열(숫자 열이 아니면 첫 숫자 열), 이름표는 첫 글자 열이다. 거르기·정렬한 순서를 따른다.
    pub fn build(g: &Grid) -> Result<Chart, String> {
        let n = g.res.cols.len();
        let val = if g.kind(g.col) == Kind::Number {
            g.col
        } else {
            (0..n).find(|&c| g.kind(c) == Kind::Number).ok_or("차트로 그릴 숫자 열이 없음")?
        };
        let label = (0..n).find(|&c| g.kind(c) != Kind::Number);
        let line = g.kind(0) == Kind::Date;
        let mut points = Vec::new();
        let mut skipped = 0;
        for (i, row) in g.visible_values().iter().enumerate() {
            let v = row.get(val).map(String::as_str).unwrap_or("");
            match v.parse::<f64>() {
                Ok(x) if !is_null(v) && x.is_finite() => {
                    let l = label.and_then(|c| row.get(c).cloned()).unwrap_or_else(|| (i + 1).to_string());
                    points.push((l, x));
                }
                _ => skipped += 1,
            }
        }
        Ok(Chart {
            title: g.res.cols[val].clone(),
            line,
            points,
            skipped,
        })
    }

    pub fn render(&self, buf: &mut Buffer, r: Rect) {
        let w = r.width as usize;
        let mut head = format!(
            "{} · {}",
            if self.line { "선" } else { "막대" },
            idb_core::textutil::printable(&self.title, false)
        );
        if self.skipped > 0 {
            head.push_str(&format!(" · 건너뜀 {}", self.skipped));
        }
        buf.set_stringn(r.x, r.y, &head, w, theme::dim());
        let h = r.height.saturating_sub(1) as usize;
        if self.points.is_empty() || h == 0 {
            return;
        }
        let max = self.points.iter().map(|p| p.1.abs()).fold(0.0, f64::max).max(f64::MIN_POSITIVE);
        if !self.line {
            // 화면보다 행이 많으면 앞에서부터 화면만큼이다
            let lw = self.points.iter().map(|p| p.0.width()).max().unwrap_or(1).min(20);
            for (i, (l, v)) in self.points.iter().take(h).enumerate() {
                let num = fmt_num(*v);
                let room = w.saturating_sub(lw + num.width() + 3);
                let bar = "█".repeat(((v.abs() / max) * room as f64).round() as usize);
                let y = r.y + 1 + i as u16;
                let lab = idb_core::textutil::truncate(&idb_core::textutil::printable(l, false), lw, "…");
                buf.set_stringn(r.x, y, &lab, lw, Style::new().fg(theme::TEXT));
                let st = Style::new().fg(if *v < 0.0 { theme::PROD } else { theme::ACCENT });
                buf.set_stringn(r.x + lw as u16 + 1, y, &bar, room, st);
                buf.set_stringn(r.x + (lw + 2 + bar.chars().count()) as u16, y, &num, num.width(), theme::dim());
            }
            return;
        }
        // 선은 전체를 폭에 맞춰 줄인다(칸마다 그 구간의 평균)
        let plot_h = h.saturating_sub(1).max(1);
        let n = self.points.len();
        let cols = w.min(n.max(1));
        let (lo, hi) = self.points.iter().fold((f64::MAX, f64::MIN), |(a, b), p| (a.min(p.1), b.max(p.1)));
        let span = (hi - lo).max(f64::MIN_POSITIVE);
        for x in 0..cols {
            let (a, b) = (x * n / cols, ((x + 1) * n / cols).max(x * n / cols + 1));
            let avg = self.points[a..b.min(n)].iter().map(|p| p.1).sum::<f64>() / (b.min(n) - a) as f64;
            let yy = ((hi - avg) / span * (plot_h - 1) as f64).round() as usize;
            buf.set_string(r.x + x as u16, r.y + 1 + yy as u16, "•", Style::new().fg(theme::ACCENT));
        }
        let foot = format!(
            "{} … {} · 최소 {} · 최대 {}",
            self.points[0].0,
            self.points[n - 1].0,
            fmt_num(lo),
            fmt_num(hi)
        );
        buf.set_stringn(r.x, r.y + h as u16, idb_core::textutil::printable(&foot, false), w, theme::dim());
    }
}

fn fmt_num(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        idb_core::textutil::group_digits(v as i64)
    } else {
        format!("{v:.2}")
    }
}

#[derive(Debug)]
/// 비교 결과 한 줄이다. kind 는 '+'(추가)·'-'(삭제)·'~'(바뀜)이다.
pub struct DiffView {
    pub lines: Vec<(char, String)>,
    pub summary: String,
    pub top: usize,
}

/// 기준 a(고정 결과)와 지금 b 를 짝지어 추가·삭제·바뀜만 남긴다. 두 결과의 PK 가 같으면 PK 로, 아니면 행 전체로 짝짓는다.
pub fn diff(a: &Grid, b: &Grid) -> Result<DiffView, String> {
    if a.res.cols != b.res.cols {
        let only_a: Vec<&String> = a.res.cols.iter().filter(|c| !b.res.cols.contains(c)).collect();
        let only_b: Vec<&String> = b.res.cols.iter().filter(|c| !a.res.cols.contains(c)).collect();
        return Err(format!("열이 달라 비교할 수 없음: 고정에만 {only_a:?} · 지금에만 {only_b:?}"));
    }
    let pk: Option<Vec<usize>> = match (&a.res.origin, &b.res.origin) {
        (Some(x), Some(y)) if x.pk == y.pk && x.table == y.table => x.pk.iter().map(|k| a.res.cols.iter().position(|c| c == k)).collect(),
        _ => None,
    };
    let key = |r: &Vec<String>| match &pk {
        Some(p) => p.iter().map(|&i| r[i].clone()).collect::<Vec<_>>().join("\u{1f}"),
        None => r.join("\u{1f}"),
    };
    let tsv = |r: &Vec<String>| r.join("\t");
    let mut left: HashMap<String, Vec<&Vec<String>>> = HashMap::new();
    for r in &a.res.rows {
        left.entry(key(r)).or_default().push(r);
    }
    let mut lines = Vec::new();
    let (mut add, mut del, mut chg) = (0, 0, 0);
    for r in &b.res.rows {
        match left
            .get_mut(&key(r))
            .and_then(|v| if v.is_empty() { None } else { Some(v.remove(0)) })
        {
            None => {
                add += 1;
                lines.push(('+', tsv(r)));
            }
            Some(old) if old != r => {
                chg += 1;
                let cells: Vec<String> = old
                    .iter()
                    .zip(r)
                    .enumerate()
                    .filter(|(_, (x, y))| x != y)
                    .map(|(i, (x, y))| format!("{}: {x} → {y}", a.res.cols[i]))
                    .collect();
                lines.push(('~', format!("{} · {}", tsv(r), cells.join(" · "))));
            }
            Some(_) => {}
        }
    }
    for r in &a.res.rows {
        if let Some(v) = left.get_mut(&key(r))
            && let Some(pos) = v.iter().position(|x| *x == r)
        {
            v.remove(pos);
            del += 1;
            lines.push(('-', tsv(r)));
        }
    }
    Ok(DiffView {
        lines,
        summary: format!("+{add} −{del} ~{chg}"),
        top: 0,
    })
}

impl App {
    /// c: 표를 차트로 바꾼다. 다시 c·esc 면 표다.
    pub(crate) fn toggle_chart(&mut self) {
        let t = self.cur_mut();
        if t.chart.take().is_some() {
            return;
        }
        let Some(g) = t.shown() else { return };
        match Chart::build(g) {
            Ok(c) => t.chart = Some(c),
            Err(e) => self.status = e,
        }
    }

    /// =: 고정한 결과 하나를 골라 지금 결과와 비교한다.
    pub(crate) fn compare_pinned(&mut self) {
        let t = self.cur();
        let pins: Vec<String> = t.subs.iter().filter(|p| p.pinned).map(|p| p.title.clone()).collect();
        if pins.is_empty() {
            self.status = "비교할 고정 결과가 없음 (p 로 고정)".into();
            return;
        }
        if t.grid.is_none() {
            return;
        }
        self.picker("비교할 고정 결과", pins, |a, title| {
            let t = a.cur();
            let Some(p) = t.subs.iter().find(|p| p.title == title) else {
                return;
            };
            match diff(&p.grid, t.grid.as_ref().unwrap()) {
                Ok(d) => {
                    a.status = format!("{title} 기준 비교 {} (esc 닫기)", d.summary);
                    a.cur_mut().diff = Some(d);
                }
                Err(e) => a.status = e,
            }
        });
    }

    /// Alt+C: 커서 문장을 다른 PostgreSQL 접속에서도 읽기 전용으로 돌려 비교한다(R53.1). 지금 탭 접속이 기준이다.
    pub(crate) fn compare_conns(&mut self) {
        let t = self.cur();
        let Some(base) = t.conn.filter(|&c| self.is_pg(c)) else {
            self.status = "PostgreSQL 탭에서만 비교".into();
            return;
        };
        let text = t.editor.value().to_string();
        let src = t
            .editor
            .selected_text()
            .map(str::to_string)
            .unwrap_or_else(|| idb_core::sqlsafe::statement_at(&text, t.editor.cursor()).to_string());
        let stmts = idb_core::sqlsafe::split_statements(&src);
        let why = if stmts.len() != 1 {
            "문장 하나만 비교"
        } else if !idb_core::sqlsafe::is_read_only_sql(stmts[0]) {
            "읽기 문장만 비교"
        } else if !idb_core::params::bind(stmts[0]).1.is_empty() {
            "파라미터 문장은 비교하지 않음"
        } else {
            ""
        };
        if !why.is_empty() {
            self.status = why.into();
            return;
        }
        let sql = stmts[0].trim().to_string();
        let name = self.conns[base].name.clone();
        let want = if name.contains("stg") {
            name.replace("stg", "prod")
        } else {
            name.replace("prod", "stg")
        };
        let others: Vec<String> = self
            .conns
            .iter()
            .enumerate()
            .filter(|(i, c)| *i != base && c.kind == "postgresql")
            .map(|(_, c)| c.name.clone())
            .collect();
        if others.is_empty() {
            self.status = "비교할 다른 PostgreSQL 접속이 없음".into();
            return;
        }
        let sel = others.iter().position(|n| *n == want).unwrap_or(0);
        self.picker(format!("{name} 와 비교할 접속"), others, move |a, other| {
            let Some(oc) = a.conns.iter().position(|c| c.name == other) else {
                return;
            };
            let i = a.new_tab();
            a.set_conn(i, Some(base));
            a.tabs[i].title = format!("비교 {name}↔{other}");
            a.tabs[i].editor.set_value(&sql);
            a.tabs[i].editor.typed = false;
            let tab = a.tabs[i].id;
            a.tabs[i].compare = vec![None, None];
            for (k, c) in [base, oc].into_iter().enumerate() {
                let req = idb_engine::Request::ReadQuery {
                    conn: a.conns[c].name.clone(),
                    sql: sql.clone(),
                    limit: 0,
                };
                a.call(req, crate::app::Pend::Compare { tab, side: k, conn: c });
            }
            a.tabs[i].summary = "비교 중…".into();
            a.set_focus(crate::help::Focus::Result);
        });
        if let crate::app::Overlay::Picker { sel: s, .. } = &mut self.overlay {
            *s = sel;
        }
    }

    pub(crate) fn compare_done(&mut self, tab: u64, side: usize, conn: usize, res: Result<serde_json::Value, idb_engine::EngineError>) {
        let name = self.conns[conn].name.clone();
        let r =
            res.and_then(|v| serde_json::from_value::<idb_engine::ResultSet>(v).map_err(|e| idb_engine::EngineError::run(e.to_string())));
        let Some(t) = self.tab_by_id(tab) else { return };
        if t.compare.len() != 2 {
            return;
        }
        t.compare[side] = Some(r.map(|rs| (name.clone(), rs)).map_err(|e| format!("비교 실패 {name}: {}", e.error)));
        if t.compare.iter().any(Option::is_none) {
            return;
        }
        let parts: Vec<_> = std::mem::take(&mut t.compare).into_iter().flatten().collect();
        if let Some(Err(e)) = parts.iter().find(|p| p.is_err()) {
            t.err = e.clone();
            t.summary = "비교 실패".into();
            let e = e.clone();
            self.status = e;
            return;
        }
        let ok: Vec<(String, idb_engine::ResultSet)> = parts.into_iter().map(|p| p.unwrap()).collect();
        let (a, b) = (Grid::new(ok[0].1.clone()), Grid::new(ok[1].1.clone()));
        // 기준(지금 탭 접속)에서 다른 접속이 어떻게 다른지다
        let d = diff(&a, &b);
        t.subs = ok
            .iter()
            .map(|(n, rs)| crate::app::Pin {
                title: n.clone(),
                grid: Grid::new(rs.clone()),
                pinned: false,
            })
            .collect();
        t.grid = Some(a);
        match d {
            Ok(d) => {
                t.summary = format!("{} 기준 {} 비교 {}", ok[0].0, ok[1].0, d.summary);
                t.diff = Some(d);
            }
            Err(e) => t.summary = e,
        }
        let s = t.summary.clone();
        self.status = s;
    }

    pub(crate) fn draw_diff(d: &DiffView, buf: &mut Buffer, r: Rect) {
        buf.set_stringn(r.x, r.y, &d.summary, r.width as usize, Style::new().fg(theme::TEXT));
        for (i, (k, l)) in d.lines.iter().skip(d.top).take(r.height.saturating_sub(1) as usize).enumerate() {
            let st = Style::new().fg(match k {
                '+' => theme::GREEN,
                '-' => theme::PROD,
                _ => theme::STG,
            });
            let text = idb_core::textutil::printable(&format!("{k} {l}"), false);
            buf.set_stringn(r.x, r.y + 1 + i as u16, text, r.width as usize, st);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(cols: &[&str], types: &[&str], rows: &[&[&str]], pk: bool) -> Grid {
        let mut v = serde_json::json!({"cols": cols, "types": types, "rows": rows.iter().map(|r| r.to_vec()).collect::<Vec<_>>(), "text": "", "truncated": false, "elapsed_ms": 1});
        if pk {
            v["origin"] = serde_json::json!({"table":{"schema":"public","table":"t"},"pk":["id"],"cols":cols});
        }
        Grid::new(serde_json::from_value(v).unwrap())
    }

    #[test]
    fn 차트는_숫자_열과_첫_글자_열로_만들고_못_읽는_칸은_건너뛴다() {
        let c = Chart::build(&g(
            &["ch", "n"],
            &["text", "int8"],
            &[&["a", "3"], &["b", "NULL"], &["c", "5"]],
            false,
        ))
        .unwrap();
        assert_eq!((c.line, c.points.len(), c.skipped), (false, 2, 1));
        assert_eq!(c.points[1], ("c".into(), 5.0));
        assert!(Chart::build(&g(&["a"], &["text"], &[&["x"]], false)).is_err());
        assert!(
            Chart::build(&g(&["d", "n"], &["date", "int4"], &[&["2026-01-01", "1"]], false))
                .unwrap()
                .line
        );
    }

    #[test]
    fn 비교는_PK_로_짝짓고_추가_삭제_바뀜만_남긴다() {
        let a = g(&["id", "v"], &["int4", "text"], &[&["1", "a"], &["2", "b"]], true);
        let b = g(&["id", "v"], &["int4", "text"], &[&["1", "a"], &["2", "x"], &["3", "c"]], true);
        let d = diff(&a, &b).unwrap();
        assert_eq!(d.summary, "+1 −0 ~1");
        assert!(d.lines.iter().any(|(k, l)| *k == '~' && l.ends_with("v: b → x")));
        let c = g(&["id"], &["int4"], &[&["1"]], false);
        assert!(diff(&a, &c).unwrap_err().starts_with("열이 달라 비교할 수 없음"));
        // PK 가 없으면 행 전체로 짝짓는다
        let x = g(&["v"], &["text"], &[&["a"], &["b"]], false);
        let y = g(&["v"], &["text"], &[&["b"], &["c"]], false);
        assert_eq!(diff(&x, &y).unwrap().summary, "+1 −1 ~0");
    }
}
