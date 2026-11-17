//! 그리기와 상태 줄 키 안내(R13·R74). 화면 틀은 Go View 와 같다: 헤더 1줄, 왼쪽 트리, 오른쪽 탭 줄·편집기·결과, 상태 줄.

use idb_core::textutil::{printable, truncate};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use unicode_width::UnicodeWidthStr;

use crate::app::{App, Drag, MIN_H, MIN_W, Overlay, PLACEHOLDER};
use crate::grid::Screen;
use crate::help::{self, Focus};
use crate::theme;
use crate::tree::NodeKind;

fn width(s: &str) -> usize {
    idb_core::textutil::string_width(s)
}

/// 둥근 테두리 영역. 위 테두리에 제목(왼쪽)과 정보(오른쪽)를 넣는다. 끄는 중이면 굵은 선이다(R46.1).
fn pane(buf: &mut Buffer, r: Rect, title: &str, info: &str, focused: bool, hot: bool) {
    if r.width < 2 || r.height < 2 {
        return;
    }
    let (mut bst, mut tst) = (Style::new().fg(theme::BORDER), theme::dim());
    if focused {
        bst = Style::new().fg(theme::ACCENT);
        tst = Style::new().fg(theme::ACCENT).add_modifier(Modifier::BOLD);
    }
    let (tl, tr, bl, br, hz, vt) = if hot {
        bst = Style::new().fg(theme::STG);
        ("┏", "┓", "┗", "┛", "━", "┃")
    } else {
        ("╭", "╮", "╰", "╯", "─", "│")
    };
    let w = r.width as usize;
    let right = r.x + r.width - 1;
    let bottom = r.y + r.height - 1;
    buf.set_string(r.x, r.y, tl, bst);
    buf.set_string(right, r.y, tr, bst);
    buf.set_string(r.x, bottom, bl, bst);
    buf.set_string(right, bottom, br, bst);
    for x in r.x + 1..right {
        buf.set_string(x, r.y, hz, bst);
        buf.set_string(x, bottom, hz, bst);
    }
    for y in r.y + 1..bottom {
        buf.set_string(r.x, y, vt, bst);
        buf.set_string(right, y, vt, bst);
    }
    let title = truncate(&format!(" {title} "), w.saturating_sub(3), "…");
    buf.set_string(r.x + 2, r.y, &title, tst);
    let room = w.saturating_sub(4 + width(&title));
    if !info.is_empty() && room >= 6 {
        let info = truncate(&format!(" {info} "), room, "…");
        let x = right - width(&info) as u16;
        buf.set_string(x, r.y, &info, theme::dim());
    }
}

fn inner(r: Rect) -> Rect {
    Rect::new(r.x + 1, r.y + 1, r.width.saturating_sub(2), r.height.saturating_sub(2))
}

/// 왼쪽·오른쪽 글을 폭에 맞춰 배경으로 채운 한 줄이다. 좁으면 오른쪽을 버리고 왼쪽을 자른다.
fn bar_line(buf: &mut Buffer, y: u16, w: u16, left: &[(String, Style)], right: &[(String, Style)]) {
    buf.set_stringn(0, y, " ".repeat(w as usize), w as usize, theme::bar());
    let lw: usize = left.iter().map(|(s, _)| width(s)).sum();
    let rw: usize = right.iter().map(|(s, _)| width(s)).sum();
    let mut x = 0u16;
    for (s, st) in left {
        if x >= w {
            break;
        }
        let room = (w - x) as usize;
        let s = truncate(s, room, "…");
        buf.set_stringn(x, y, &s, room, *st);
        x += width(&s) as u16;
    }
    if lw + rw < w as usize {
        let mut x = w - rw as u16;
        for (s, st) in right {
            buf.set_string(x, y, s, *st);
            x += width(s) as u16;
        }
    }
}

impl App {
    pub fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        self.w = area.width;
        self.h = area.height;
        let buf = f.buffer_mut();
        if self.w < MIN_W || self.h < MIN_H {
            let msg = format!("터미널이 너무 작습니다 ({}x{}). 최소 {MIN_W}x{MIN_H}.", self.w, self.h);
            buf.set_stringn(0, 0, msg, self.w as usize, Style::new());
            return;
        }
        if let Some((ed, json, _)) = &mut self.big_edit {
            let title = if *json { "칸 값 편집 (JSON)" } else { "칸 값 편집" };
            let r = Rect::new(0, 0, self.w, self.h.saturating_sub(1));
            pane(buf, r, title, "", true, false);
            ed.render(inner(r), buf, true, "");
            let hint = if *json {
                "Ctrl+S 저장 · F8 JSON 정렬 · esc 버림"
            } else {
                "Ctrl+S 저장 · esc 버림"
            };
            let st = format!(" {} ", printable(&self.status, false));
            bar_line(
                buf,
                self.h - 1,
                self.w,
                &[(st, theme::bar())],
                &[(format!("{} ", self.label(hint)), theme::hint())],
            );
            return;
        }
        if self.history_view.is_some() && self.overlay.is_none() {
            return self.draw_history(buf);
        }
        match &self.overlay {
            Overlay::Help => return self.draw_help(buf, None),
            Overlay::Keys(secs) => {
                let secs = secs.clone();
                return self.draw_help(buf, Some(secs));
            }
            Overlay::Picker { .. } => return self.draw_picker(buf),
            _ => {}
        }
        self.draw_header(buf);
        let l = self.layout();
        // ─── 트리 ───
        let tree_rect = Rect::new(0, 1, l.tree_w + 2, l.body_h);
        let info = if self.tree.filter.is_empty() {
            String::new()
        } else {
            format!("거르기 {}", printable(&self.tree.filter, false))
        };
        pane(
            buf,
            tree_rect,
            "Explorer",
            &info,
            self.focus == Focus::Tree,
            self.drag == Some(Drag::Tree),
        );
        self.draw_tree(buf, inner(tree_rect));
        // ─── 탭 줄 ───
        let rx = l.tree_w + 2;
        let mut x = rx;
        for (i, lb) in self.tab_labels().iter().enumerate() {
            let st = if i == self.tab {
                theme::tab_active()
            } else {
                theme::tab_inactive()
            };
            if x >= self.w {
                break;
            }
            let room = (self.w - x) as usize;
            buf.set_stringn(x, 1, lb, room, st);
            x += width(lb) as u16 + 1;
        }
        if x + 3 <= self.w {
            buf.set_string(x, 1, " + ", theme::tab_inactive());
        }
        // 대시보드·흐름·보드(R64·R69·R70)는 편집기·결과 두 영역(테두리 포함)을 한 영역으로 쓴다
        if self.tabs[self.tab].pane.is_some() {
            let rect = Rect::new(rx, 2, l.right_w + 2, l.ed_h + l.res_h + 4);
            self.draw_pane(buf, rect);
            self.draw_status(buf);
            return;
        }
        // ─── 편집기 ───
        let ed_rect = Rect::new(rx, 2, l.right_w + 2, l.ed_h + 2);
        let t = &self.tabs[self.tab];
        let mut ed_info = t.file.clone().map(|f| printable(&f, false)).unwrap_or_default();
        if t.autosave_stopped {
            ed_info = format!("{ed_info} · 자동 저장 멈춤");
        }
        // Loki·Prometheus·Grafana 탭은 편집기 위 줄(테두리)에 범위를 보인다(R62.2)
        if let Some(c) = t.conn
            && crate::sources::uses_range(&self.conns[c].kind)
        {
            let head = format!("최근 {} · {}", crate::sources::range_label(t.since), self.label("Ctrl+]"));
            ed_info = if ed_info.is_empty() { head } else { format!("{head} · {ed_info}") };
        }
        let title = printable(&t.display_name(), false);
        pane(
            buf,
            ed_rect,
            &title,
            &ed_info,
            self.focus == Focus::Editor,
            self.drag == Some(Drag::Split),
        );
        let ph = self.label(PLACEHOLDER);
        let focused = self.focus == Focus::Editor && self.overlay.is_none();
        self.tabs[self.tab].editor.render(inner(ed_rect), buf, focused, &ph);
        // ─── 결과 ───
        let res_rect = Rect::new(rx, 2 + l.ed_h + 2, l.right_w + 2, l.res_h + 2);
        let t = &self.tabs[self.tab];
        let res_info = if t.running.is_some() {
            "실행 중…".to_string()
        } else {
            printable(&t.summary, false)
        };
        pane(buf, res_rect, "Result", &res_info, self.focus == Focus::Result, false);
        self.draw_result(buf, inner(res_rect));
        // ─── 상태 줄 ───
        self.pick_tip();
        self.draw_status(buf);
        if self.conn_form.is_some() {
            self.draw_conn_form(buf, area);
        }
    }

    fn draw_header(&self, buf: &mut Buffer) {
        let t = self.cur();
        let mut left: Vec<(String, Style)> = vec![(" db ".into(), theme::app_badge()), (" ".into(), theme::bar())];
        match t.conn {
            Some(c) => {
                let cs = &self.conns[c];
                let mut name = cs.name.clone();
                if let Some(d) = self.db_over.get(&c) {
                    name = format!("{name}/{d}");
                }
                if let Some(s) = self.schema_of.get(&c) {
                    name = format!("{name} · {s}");
                }
                left.push((
                    theme::kind_icon(&cs.kind).into(),
                    Style::new().bg(theme::BAR).fg(theme::kind_color(&cs.kind)),
                ));
                left.push((printable(&name, false), theme::bar().add_modifier(Modifier::BOLD)));
                left.push((" ".into(), theme::bar()));
                left.push((format!(" {} ", cs.env.to_uppercase()), theme::env_badge(&cs.env)));
                left.push((format!(" {}", theme::kind_label(&cs.kind)), theme::hint()));
                if self.is_pg(c) {
                    let mode = if self.manual.contains(&c) { "Manual" } else { "Auto" };
                    match self.in_tx.get(&c) {
                        Some(since) => {
                            let s = since.elapsed().as_secs();
                            left.push((format!("  {mode} · tx {:02}:{:02}", s / 60, s % 60), theme::tx_badge()));
                        }
                        None => left.push((format!("  {mode}"), theme::hint())),
                    }
                }
                if let Some(l) = self.locks.get(&c)
                    && l.prod
                {
                    // 열린 남은 시간은 받은 뒤 흐른 만큼 줄여 센다. 다 흐르면 serve 도 다시 잠근다(R54)
                    let rest = l
                        .remaining_secs
                        .saturating_sub(self.lock_at.get(&c).map_or(0, |t| t.elapsed().as_secs()));
                    let b = if l.locked || rest == 0 {
                        "🔒 잠김".to_string()
                    } else {
                        format!("🔓 열림 {}:{:02}", rest / 60, rest % 60)
                    };
                    left.push((format!("  {b}"), theme::tx_badge()));
                }
            }
            None => {
                let pane = match &t.pane {
                    Some(crate::app::Pane::Dash(_)) => Some(("대시보드".to_string(), format!(" · 접속 {}", self.conns.len()))),
                    Some(crate::app::Pane::Flow(f)) => Some(("흐름".to_string(), format!(" · {}", f.items.len() + f.adhoc.len()))),
                    Some(crate::app::Pane::Board(b)) => Some(("보드".to_string(), format!(" · {}", App::board_head(b)))),
                    None => None,
                };
                match pane {
                    Some((name, rest)) => {
                        left.push((name, theme::bar().add_modifier(Modifier::BOLD)));
                        left.push((rest, theme::hint()));
                    }
                    None => left.push(("접속 없음 · 트리에서 고르세요".into(), theme::hint())),
                }
            }
        }
        let base = std::path::Path::new(&self.dir)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        bar_line(buf, 0, self.w, &left, &[(format!("{} ", printable(&base, false)), theme::hint())]);
    }

    fn draw_tree(&mut self, buf: &mut Buffer, r: Rect) {
        let h = r.height as usize;
        let tree = &mut self.tree;
        if tree.cursor < tree.top {
            tree.top = tree.cursor;
        }
        if tree.cursor >= tree.top + h {
            tree.top = tree.cursor + 1 - h;
        }
        let cur_conn = self.tabs[self.tab].conn;
        for (y, &(n, depth)) in self.tree.rows.iter().skip(self.tree.top).take(h).enumerate() {
            let node = &self.tree.nodes[n];
            let toggle = if node.expandable() {
                if node.expanded { "▾ " } else { "▸ " }
            } else {
                "  "
            };
            let mut style = Style::new().fg(theme::TEXT);
            let (mut icon, mut ist) = (
                match &node.kind {
                    NodeKind::Schema { .. } => "▣ ",
                    NodeKind::Table { .. } => "▦ ",
                    NodeKind::Key { .. } => "◇ ",
                    NodeKind::Folder { .. } => "▤ ",
                    NodeKind::Queries(_) => "▥ ",
                    NodeKind::File(_) => "▢ ",
                    _ => "",
                },
                theme::dim(),
            );
            let mut label = printable(&node.label, false);
            match &node.kind {
                NodeKind::Group => style = style.add_modifier(Modifier::BOLD),
                NodeKind::Conn(c) => {
                    let cs = &self.conns[*c];
                    icon = theme::kind_icon(&cs.kind);
                    ist = Style::new().fg(theme::kind_color(&cs.kind));
                    style = theme::env_style(&cs.env);
                    if let Some(d) = self.db_over.get(c) {
                        label = format!("{label}/{}", printable(d, false));
                    }
                    if cs.source == "db" {
                        label.push_str(" (db)");
                    }
                    if cur_conn == Some(*c) {
                        label = format!("● {label}");
                    }
                }
                NodeKind::Info => style = theme::dim(),
                _ => {}
            }
            // 저장소 노드 오른쪽 흐린 글(R60, warn 이면 경고 색 R67.2)
            let info = (matches!(node.kind, NodeKind::Source { .. }) && !node.info.is_empty()).then(|| {
                (
                    format!(" {}", printable(&node.info, false)),
                    if node.warn { theme::warn() } else { theme::dim() },
                )
            });
            let row = self.tree.top + y;
            if row == self.tree.cursor {
                let sel = if self.focus == Focus::Tree {
                    Style::new().add_modifier(Modifier::REVERSED)
                } else {
                    Style::new().bg(theme::BORDER)
                };
                style = style.patch(sel);
                ist = ist.patch(sel);
            }
            let yy = r.y + y as u16;
            let indent = "  ".repeat(depth);
            let head = format!("{indent}{toggle}");
            let w = r.width as usize;
            buf.set_stringn(r.x, yy, &head, w, style);
            let mut x = r.x + width(&head) as u16;
            if !icon.is_empty() && (x - r.x) < r.width {
                buf.set_stringn(x, yy, icon, w.saturating_sub((x - r.x) as usize), ist);
                x += 2;
            }
            if (x - r.x) < r.width {
                let room = w - (x - r.x) as usize;
                let shown = truncate(&label, room, "…");
                buf.set_stringn(x, yy, &shown, room, style);
                let used = width(&shown);
                if let Some((info, ist)) = info
                    && used < room
                {
                    let ist = if row == self.tree.cursor { ist.patch(style) } else { ist };
                    buf.set_stringn(x + used as u16, yy, truncate(&info, room - used, "…"), room - used, ist);
                }
            }
        }
    }

    /// 대시보드·흐름·보드 탭 한 영역이다. 머리 오른쪽에 그 화면의 키와 `확인 중 N` 을 보인다.
    fn draw_pane(&self, buf: &mut Buffer, rect: Rect) {
        use crate::app::Pane;
        let t = &self.tabs[self.tab];
        let Some(p) = &t.pane else { return };
        let (title, keys, n) = match p {
            Pane::Dash(d) => ("대시보드".to_string(), "r 새로고침 · Enter 트리 · / 거르기", d.jobs.len()),
            Pane::Flow(f) => ("흐름".to_string(), "r 새로고침 · ←→ 상자 · Enter 상세", App::flow_pending(f)),
            Pane::Board(b) => (printable(&t.title, false), "r 새로고침 · Enter 쿼리 탭", App::board_pending(b)),
        };
        let mut info = self.label(keys);
        if n > 0 {
            info = format!("확인 중 {n} · {info}");
        }
        pane(buf, rect, &title, &info, self.focus != Focus::Tree, false);
        let r = inner(rect);
        match p {
            Pane::Dash(d) => self.draw_dash(buf, r, d),
            Pane::Flow(f) => self.draw_flow(buf, r, f),
            Pane::Board(b) => self.draw_board(buf, r, b),
        }
    }

    /// 결과 표 위 줄(테이블 보기 줄·결과 탭 줄·설명·거르기 줄)이다.
    pub fn result_lines(&self, w: usize) -> Vec<(String, Style)> {
        let t = self.cur();
        let mut out = Vec::new();
        if let Some(v) = &t.view {
            match v.typing {
                Some(f) => {
                    let head = if f == crate::tableview::Field::Where {
                        "WHERE "
                    } else {
                        "ORDER BY "
                    };
                    out.push((format!("{head}\u{0}"), Style::new().fg(theme::TEXT)));
                }
                None => out.push((v.bar(w), theme::dim())),
            }
        }
        if !t.subs.is_empty() {
            let mut s = String::new();
            let cur = if t.sub.is_none() { "[지금]" } else { " 지금 " };
            s.push_str(cur);
            for (i, p) in t.subs.iter().enumerate() {
                let mark = if p.pinned { "📌" } else { "" };
                if t.sub == Some(i) {
                    s.push_str(&format!(" [{mark}{}]", p.title));
                } else {
                    s.push_str(&format!("  {mark}{} ", p.title));
                }
            }
            out.push((s, theme::dim()));
        }
        if t.view.is_none()
            && let Some(g) = t.shown()
        {
            if !g.res.text.is_empty() {
                out.push((printable(&g.res.text.replace('\n', " · "), false), theme::dim()));
            }
            if g.screen == Screen::Filter || !g.filter.is_empty() {
                let mut s = format!(
                    "거르기 {}",
                    if g.screen == Screen::Filter {
                        "\u{0}".into()
                    } else {
                        printable(&g.filter, false)
                    }
                );
                s.push_str(&format!(" · {}/{}행", g.visible_rows(), g.res.rows.len()));
                let sl = g.sort_label();
                if !sl.is_empty() {
                    s.push_str(&format!(" · {sl}"));
                }
                out.push((s, Style::new().fg(theme::TEXT)));
            }
        }
        out
    }

    fn draw_result(&mut self, buf: &mut Buffer, r: Rect) {
        let w = r.width as usize;
        if let Overlay::Confirm { preview, top, .. } = &self.overlay
            && !preview.is_empty()
        {
            let head = format!("미리 보기 {}줄 · ↑↓ 넘기기 · y 실행 · 그 밖의 키 취소", preview.len());
            buf.set_stringn(r.x, r.y, head, w, theme::dim());
            for (i, l) in preview.iter().skip(*top).take(r.height.saturating_sub(1) as usize).enumerate() {
                buf.set_stringn(r.x, r.y + 1 + i as u16, printable(l, false), w, Style::new().fg(theme::TEXT));
            }
            return;
        }
        if self.cur().redis.is_some() {
            return self.draw_redis(buf, r);
        }
        let lines = self.result_lines(w);
        let mut y = r.y;
        for (s, st) in &lines {
            if let Some((pre, post)) = s.split_once('\u{0}') {
                buf.set_stringn(r.x, y, pre, w, *st);
                let px = r.x + width(pre) as u16;
                let t = self.cur();
                let input = t
                    .view
                    .as_ref()
                    .filter(|v| v.typing.is_some())
                    .map(|v| &v.input)
                    .or_else(|| t.shown().map(|g| &g.input));
                if let Some(inp) = input {
                    let room = w.saturating_sub(width(pre) + width(post));
                    inp.render(buf, px, y, room.max(1), Style::new().fg(theme::TEXT), true);
                    let ix = px + room as u16;
                    buf.set_stringn(ix, y, post, w.saturating_sub(width(pre) + room), theme::dim());
                }
            } else {
                buf.set_stringn(r.x, y, s, w, *st);
            }
            y += 1;
        }
        let body = Rect::new(r.x, y, r.width, r.height.saturating_sub(lines.len() as u16));
        let focused = self.focus == Focus::Result;
        let t = &mut self.tabs[self.tab];
        if let Some(c) = &t.chart {
            return c.render(buf, body);
        }
        if let Some(d) = &t.diff {
            return App::draw_diff(d, buf, body);
        }
        if let Some(p) = &mut t.plan {
            let h = body.height as usize;
            if p.show_raw {
                for (i, l) in crate::grid::wrap(&p.raw, w).iter().take(h).enumerate() {
                    buf.set_stringn(body.x, body.y + i as u16, l, w, Style::new().fg(theme::TEXT));
                }
                return;
            }
            if p.sel < p.top {
                p.top = p.sel;
            }
            if p.sel >= p.top + h {
                p.top = p.sel + 1 - h;
            }
            for (i, (l, hot)) in p.lines().iter().enumerate().skip(p.top).take(h) {
                let mut st = match hot {
                    2 => theme::err(),
                    1 => Style::new().fg(theme::STG),
                    _ => Style::new().fg(theme::TEXT),
                };
                if i == p.sel && focused {
                    st = st.add_modifier(Modifier::REVERSED);
                }
                buf.set_stringn(body.x, body.y + (i - p.top) as u16, printable(l, false), w, st);
            }
            return;
        }
        if !t.err.is_empty() {
            for (i, l) in crate::grid::wrap(&t.err, w).iter().take(body.height as usize).enumerate() {
                buf.set_stringn(body.x, body.y + i as u16, l, w, theme::err());
            }
            return;
        }
        if let Some(g) = t.shown_mut() {
            g.render(body, buf, focused);
            return;
        }
        if !t.text.is_empty() {
            let wrapped = crate::grid::wrap(&t.text, w);
            t.text_top = t.text_top.min(wrapped.len().saturating_sub(1));
            for (i, l) in wrapped.iter().skip(t.text_top).take(body.height as usize).enumerate() {
                buf.set_stringn(body.x, body.y + i as u16, l, w, Style::new().fg(theme::TEXT));
            }
            return;
        }
        buf.set_stringn(body.x, body.y, "결과가 여기에 나옵니다", w, theme::dim());
    }

    fn draw_redis(&self, buf: &mut Buffer, r: Rect) {
        let rv = self.cur().redis.as_ref().unwrap();
        let w = r.width as usize;
        let Some(raw) = &rv.raw else {
            buf.set_stringn(r.x, r.y, format!("{} 읽는 중…", printable(&rv.key, false)), w, theme::dim());
            return;
        };
        let head = format!("{} · {} · TTL {}", printable(&raw.key, false), raw.typ, raw.ttl_secs);
        buf.set_stringn(r.x, r.y, head, w, Style::new().fg(theme::REDIS).add_modifier(Modifier::BOLD));
        let mut y = r.y + 1;
        if let Some((op, input)) = &rv.input {
            let label = format!("{op}: ");
            buf.set_stringn(r.x, y, &label, w, theme::help_key());
            input.render(
                buf,
                r.x + width(&label) as u16,
                y,
                w.saturating_sub(width(&label)),
                Style::new().fg(theme::TEXT),
                true,
            );
            y += 1;
        } else if let Some(op) = &rv.ask {
            let q = if op == "delete_key" {
                "키를 지움: y/N"
            } else {
                "항목을 지움: y/N"
            };
            buf.set_stringn(r.x, y, q, w, theme::confirm_bar());
            y += 1;
        }
        let rows: Vec<String> = match raw.typ.as_str() {
            "string" => crate::grid::wrap(&raw.string, w),
            "hash" => raw
                .fields
                .iter()
                .map(|f| format!("{}  {}", printable(&f[0], false), printable(&f[1], false)))
                .collect(),
            "list" => raw
                .items
                .iter()
                .enumerate()
                .map(|(i, v)| format!("{i}  {}", printable(v, false)))
                .collect(),
            "set" => raw.members.iter().map(|m| printable(m, false)).collect(),
            "zset" => raw
                .zmembers
                .iter()
                .map(|z| format!("{}  {}", printable(&z[1], false), printable(&z[0], false)))
                .collect(),
            _ => vec![format!("{} 값은 보기만", raw.typ)],
        };
        let h = (r.y + r.height).saturating_sub(y) as usize;
        let top = rv.sel.saturating_sub(h.saturating_sub(1));
        for (i, l) in rows.iter().enumerate().skip(top).take(h) {
            let st = if i == rv.sel && raw.typ != "string" {
                Style::new().add_modifier(Modifier::REVERSED)
            } else {
                Style::new().fg(theme::TEXT)
            };
            buf.set_stringn(r.x, y, l, w, st);
            y += 1;
        }
    }

    fn draw_status(&self, buf: &mut Buffer) {
        let y = self.h - 1;
        match &self.overlay {
            Overlay::Confirm { q, .. } => {
                let s = format!(" {}: y/N ", printable(q, false));
                buf.set_stringn(0, y, " ".repeat(self.w as usize), self.w as usize, theme::bar());
                buf.set_stringn(0, y, truncate(&s, self.w as usize, "…"), self.w as usize, theme::confirm_bar());
                return;
            }
            Overlay::Tx { conns, .. } => {
                let names: Vec<String> = conns.iter().map(|&c| self.conns[c].name.clone()).collect();
                let prod = conns.iter().any(|&c| self.conns[c].env == "prod");
                let label = if prod {
                    "열린 트랜잭션(prod 포함)"
                } else {
                    "열린 트랜잭션"
                };
                let s = format!(" {label} {}: c=Commit r=Rollback esc=취소 ", names.join(", "));
                buf.set_stringn(0, y, " ".repeat(self.w as usize), self.w as usize, theme::bar());
                buf.set_stringn(
                    0,
                    y,
                    truncate(&printable(&s, false), self.w as usize, "…"),
                    self.w as usize,
                    theme::confirm_bar(),
                );
                return;
            }
            Overlay::Prompt { label, input, .. } => {
                buf.set_stringn(0, y, " ".repeat(self.w as usize), self.w as usize, Style::new());
                buf.set_stringn(0, y, label, self.w as usize, theme::help_key());
                let lw = width(label) as u16;
                input.render(buf, lw, y, (self.w - lw) as usize, Style::new().fg(theme::TEXT), true);
                return;
            }
            _ => {}
        }
        if let Some(input) = &self.tree_filtering {
            buf.set_stringn(0, y, " ".repeat(self.w as usize), self.w as usize, Style::new());
            let label = if self.tree.current().is_some_and(|n| {
                let node = self.tree.node(n);
                node.conn().is_some_and(|c| self.is_redis(c)) && !matches!(node.kind, NodeKind::Conn(_))
            }) {
                "SCAN 패턴: "
            } else {
                "거르기: "
            };
            buf.set_stringn(0, y, label, self.w as usize, theme::help_key());
            let lw = width(label) as u16;
            input.render(buf, lw, y, (self.w - lw) as usize, Style::new().fg(theme::TEXT), true);
            return;
        }
        let status = format!(" {}", printable(&self.status, false));
        let room = (self.w as usize).saturating_sub(width(&status) + 2);
        let mut items: Vec<String> = self.hints().into_iter().map(|h| self.label(&h)).collect();
        if let Some(t) = &self.tip {
            items.insert(0, t.clone());
        }
        let hint = help::fit(&items, room);
        let right = if hint.is_empty() {
            Vec::new()
        } else {
            vec![(format!("{hint} "), theme::hint())]
        };
        bar_line(buf, y, self.w, &[(status, theme::bar())], &right);
    }

    /// 처음 들어간 화면 종류면 팁을 한 번 건다(R74.3). 창이 떠 있거나 상태 줄 남은 폭에 다 들어가지 않으면 기다린다.
    fn pick_tip(&mut self) {
        if !self.session.tips_on || self.tip.is_some() || !self.overlay.is_none() {
            return;
        }
        let t = self.cur();
        let kind = match (&t.pane, self.focus) {
            (Some(crate::app::Pane::Flow(_)), _) => "flow",
            (Some(crate::app::Pane::Board(_)), _) => "board",
            (None, Focus::Result) if t.view.is_some() && t.grid.is_some() => "table",
            (None, Focus::Result) => match t.shown() {
                Some(g) if g.screen == Screen::Table && g.editable() && t.sub.is_none() => "edit",
                Some(g) if g.screen == Screen::Table => "readonly",
                _ => return,
            },
            _ => return,
        };
        if self.session.tips_shown.iter().any(|k| k == kind) {
            return;
        }
        let text = match kind {
            "edit" => "팁: a 로 행 추가, Enter 로 고치고 Ctrl+S 로 제출 (? 로 이 화면의 키)",
            "readonly" => "팁: ? 로 이 화면의 키를 볼 수 있음",
            "table" => "팁: w 로 WHERE, [ ] 로 페이지 (? 로 이 화면의 키)",
            "flow" => "팁: ←→ 로 상자를 고르고 Enter 로 상세 (? 로 이 화면의 키)",
            _ => "팁: 방향키로 패널을 고르고 Enter 로 쿼리 탭에 열기 (? 로 이 화면의 키)",
        };
        let text = self.label(text);
        let room = (self.w as usize).saturating_sub(width(&format!(" {}", printable(&self.status, false))) + 2);
        if width(&text) > room {
            return;
        }
        self.session.tip_shown(kind);
        self.tip = Some(text);
    }

    /// 지금 포커스·상태에서 실제로 듣는 키를 중요한 순서로 늘어놓는다(R74.1).
    pub fn hints(&self) -> Vec<String> {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        if self.focus != Focus::Tree {
            let pane: Option<&[&str]> = match &self.cur().pane {
                Some(crate::app::Pane::Dash(_)) => Some(help::DASH_HINTS),
                Some(crate::app::Pane::Flow(_)) => Some(help::FLOW_HINTS),
                Some(crate::app::Pane::Board(_)) => Some(help::BOARD_HINTS),
                None => None,
            };
            if let Some(p) = pane {
                return s(p);
            }
        }
        match self.focus {
            Focus::Editor => s(help::EDITOR_HINTS),
            Focus::Tree => self.tree_hints(),
            Focus::Result => self.result_hints(),
            Focus::Keep => Vec::new(),
        }
    }

    fn tree_hints(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        if !self.tree.filter.is_empty() {
            out.push("esc 거르기 해제".into());
        }
        let Some(n) = self.tree.current() else { return out };
        let node = self.tree.node(n);
        let list: Vec<&str> = match &node.kind {
            NodeKind::Group => vec!["Enter 펼치기", "N 새 접속", "/ 거르기", "? 키"],
            NodeKind::Conn(c) => {
                let db = self.conns[*c].source == "db";
                let mut v = vec!["Enter 펼치기"];
                if db {
                    v.push("E 고치기");
                }
                v.push("N 새 접속");
                if db && !self.is_redis(*c) {
                    v.push("X 지우기");
                }
                if self.is_pg(*c) {
                    v.extend(["A/L/S 운영", "B DB", "F 검색", "= 비교"]);
                } else if self.is_redis(*c) {
                    v.extend(["X 키 삭제", "A/L/S 운영", "/ SCAN 패턴"]);
                }
                v.extend(["M 폴더", "r 새로고침"]);
                if !self.is_redis(*c) {
                    v.push("/ 거르기");
                }
                v.push("? 키");
                v
            }
            NodeKind::Source { openable, kids, .. } => {
                let mut v = Vec::new();
                if self.sns_topic(n).is_some() {
                    v.push("F 흐름");
                }
                if *openable {
                    v.push("Enter 미리 보기");
                }
                if *kids {
                    v.push("l 펼치기");
                }
                v.extend(["r 새로고침", "? 키"]);
                v
            }
            NodeKind::Schema { .. } => vec!["Enter 펼치기", "F 검색", "= 비교", "r 새로고침", "/ 거르기", "? 키"],
            NodeKind::Table { .. } => vec!["Enter 열기", "d 구조", "D DDL", "I CSV 가져오기", "/ 거르기", "? 키"],
            NodeKind::Key { .. } => vec!["Enter 값 편집", "/ SCAN 패턴", "r 새로고침", "? 키"],
            NodeKind::Folder { .. } => vec!["Enter 펼치기", "X 키 삭제", "/ SCAN 패턴", "r 새로고침", "? 키"],
            NodeKind::Queries(_) => vec!["Enter 펼치기", "r 새로고침", "? 키"],
            NodeKind::File(_) => vec!["Enter 파일 열기", "r 새로고침", "? 키"],
            NodeKind::Info => vec!["↑↓ 이동", "? 키"],
        };
        out.extend(list.into_iter().map(String::from));
        out
    }

    fn result_hints(&self) -> Vec<String> {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let t = self.cur();
        if let Some(rv) = &t.redis {
            if rv.ask.is_some() {
                return s(help::REDIS_ASK_HINTS);
            }
            if rv.input.is_some() {
                return s(help::REDIS_EDIT_HINTS);
            }
            let typ = rv.raw.as_ref().map_or("", |r| r.typ.as_str());
            let mut out = s(help::redis_type_hints(typ));
            out.extend(s(help::REDIS_HINTS));
            return out;
        }
        let has_hist = self.input_history().is_some_and(|h| !h.is_empty());
        if let Some(v) = &t.view
            && v.typing.is_some()
        {
            let mut out = s(help::WHERE_HINTS);
            if has_hist && v.typing == Some(crate::tableview::Field::Where) {
                out.push("↑↓ 이전 조건".into());
            }
            return out;
        }
        if t.plan.is_some() {
            return s(&["Enter 접기·펴기", "t JSON", "esc 닫기", "? 키"]);
        }
        if t.chart.is_some() {
            return s(&["c 표로", "esc 표로"]);
        }
        if t.diff.is_some() {
            return s(&["↑↓ 줄", "esc 닫기"]);
        }
        let Some(g) = t.shown() else {
            if !t.text.is_empty() {
                let mut out = if t.snapshot {
                    vec!["r 다시 실행".to_string()]
                } else {
                    Vec::new()
                };
                out.extend(s(help::TEXT_HINTS));
                return out;
            }
            return s(help::NONE_HINTS);
        };
        match g.screen {
            Screen::Filter => {
                let mut out = s(help::FILTER_HINTS);
                if has_hist {
                    out.push("↑↓ 이전 거르기".into());
                }
                return out;
            }
            Screen::Edit => return s(help::CELL_EDIT_HINTS),
            Screen::Select => return s(help::SELECT_HINTS),
            Screen::Record => return s(help::RECORD_HINTS),
            Screen::Value => {
                return match g.value_mode() {
                    (true, _) => s(&["Enter 접기·펴기", "y 값 복사", "Y 경로 복사", "t 원문", "esc 닫기"]),
                    (false, true) => s(help::VALUE_JSON_HINTS),
                    _ => s(help::VALUE_HINTS),
                };
            }
            Screen::Table => {}
        }
        let mut out = Vec::new();
        if t.snapshot {
            out.push("r 다시 실행".into());
        }
        if t.view.is_some() {
            out.extend(s(&["[ ] 페이지", "w WHERE", "O 정렬식"]));
        }
        let editable = g.editable() && t.sub.is_none();
        let pg = t.result_conn.is_some_and(|c| self.is_pg(c));
        if g.pending() > 0 {
            out.push(format!("변경 {}건", g.pending()));
            out.extend(s(&["Ctrl+S 제출", "u 되돌리기"]));
        }
        if editable {
            let enter = if g.cell_block().is_some() { "Enter 값" } else { "Enter 고치기" };
            if g.pending() > 0 {
                out.extend([enter.to_string(), "a 행 추가".into()]);
            } else {
                out.extend(["a 행 추가".to_string(), enter.to_string()]);
            }
            for h in help::EDIT_TAIL {
                if t.view.is_some() && *h == "/ 거르기" {
                    continue;
                }
                out.push(h.to_string());
            }
        } else {
            out.push("s 정렬".into());
            if t.view.is_none() {
                out.push("/ 거르기".into());
            }
            out.push("Ctrl+K 복사".into());
            if pg {
                out.push("Ctrl+B 연결된 곳".into());
            }
            out.extend(s(&["v 레코드", "o 값"]));
            if pg {
                out.push("편집: PK 있는 단일 테이블만".into());
            }
        }
        out.push("? 키".into());
        out
    }

    fn draw_help(&self, buf: &mut Buffer, secs: Option<Vec<usize>>) {
        let keys = secs.is_some();
        let list: Vec<&help::Section> = match &secs {
            Some(v) => v.iter().map(|&i| &help::SECTIONS[i]).collect(),
            None => help::SECTIONS.iter().collect(),
        };
        let mut lines: Vec<(String, String, bool)> = Vec::new();
        for s in list {
            lines.push((s.title.to_string(), String::new(), true));
            for (k, d, _) in s.keys {
                if k.is_empty() {
                    lines.push((format!("─ {d}"), String::new(), false));
                } else {
                    lines.push((self.label(k), self.label(d), false));
                }
            }
            lines.push((String::new(), String::new(), false));
        }
        let foot = if keys {
            self.label("esc·? 닫기 · 닫은 뒤 F1 전체 도움말")
        } else {
            "↑↓ 넘기기 · 아무 키나 눌러 닫기".to_string()
        };
        let w = self.w.saturating_sub(4) as usize;
        let h = self.h.saturating_sub(3) as usize;
        let top = if keys {
            0
        } else {
            self.help_top.min(lines.len().saturating_sub(h))
        };
        pane(
            buf,
            Rect::new(0, 0, self.w, self.h),
            if keys { "이 화면의 키" } else { "도움말" },
            "",
            true,
            false,
        );
        for (i, (k, d, title)) in lines.iter().skip(top).take(h).enumerate() {
            let y = 1 + i as u16;
            if *title {
                buf.set_stringn(2, y, k, w, theme::help_title());
            } else if d.is_empty() {
                buf.set_stringn(2, y, k, w, theme::dim());
            } else {
                buf.set_stringn(2, y, k, 22.min(w), theme::help_key());
                buf.set_stringn(26, y, d, w.saturating_sub(24), Style::new().fg(theme::TEXT));
            }
        }
        buf.set_stringn(2, self.h - 2, &foot, w, theme::dim());
    }

    /// 기록 창(R11.2). 둘째 줄이 키 안내다(R74.1).
    fn draw_history(&self, buf: &mut Buffer) {
        let Some(v) = &self.history_view else { return };
        let w = self.w as usize;
        let label = "실행 기록: ";
        buf.set_stringn(0, 0, label, w, theme::help_key());
        v.filter.render(
            buf,
            width(label) as u16,
            0,
            w.saturating_sub(width(label)),
            Style::new().fg(theme::TEXT),
            true,
        );
        let hint =
            "↑↓ 고르기 · Enter 열기(◆ 사본은 새 탭) · Ctrl+E 편집기에 넣기 · Ctrl+D 사본 지우기 · 글 SQL·접속·오늘·어제 거르기 · esc 닫기";
        buf.set_stringn(0, 1, self.label(hint), w, theme::dim());
        let h = self.h.saturating_sub(3) as usize;
        let top = v.sel.saturating_sub(h.saturating_sub(1));
        let hist = self.history.as_ref();
        for (i, e) in v.list.iter().enumerate().skip(top).take(h) {
            let snap = hist.is_some_and(|x| x.has_snapshot(&e.id));
            let text = crate::history::line(e, snap);
            let mut st = if e.err.is_empty() {
                Style::new().fg(theme::TEXT)
            } else {
                theme::err()
            };
            if i == v.sel {
                st = Style::new().add_modifier(Modifier::REVERSED);
            }
            buf.set_stringn(0, 2 + (i - top) as u16, truncate(&text, w, "…"), w, st);
        }
        if v.list.is_empty() {
            buf.set_stringn(0, 2, "기록 없음", w, theme::dim());
        }
    }

    fn draw_picker(&self, buf: &mut Buffer) {
        let Overlay::Picker {
            title,
            items,
            filter,
            sel,
            del,
            ..
        } = &self.overlay
        else {
            return;
        };
        let w = self.w as usize;
        let label = format!("{title}: ");
        buf.set_stringn(0, 0, &label, w, theme::help_key());
        filter.render(
            buf,
            width(&label) as u16,
            0,
            w.saturating_sub(width(&label)),
            Style::new().fg(theme::TEXT),
            true,
        );
        let hint = if del.is_some() {
            "↑↓ 고르기 · Enter 열기 · d(거르기 글이 없을 때)·Ctrl+D 지우기 · esc 닫기"
        } else {
            "↑↓ 고르기 · Enter 고르기 · esc 닫기"
        };
        buf.set_stringn(0, 1, self.label(hint), w, theme::dim());
        let m = App::picker_matches(items, &filter.text);
        let h = self.h.saturating_sub(3) as usize;
        let top = sel.saturating_sub(h.saturating_sub(1));
        for (i, &idx) in m.iter().enumerate().skip(top).take(h) {
            let y = 2 + (i - top) as u16;
            let text = printable(&items[idx], false);
            let mut st = Style::new().fg(theme::TEXT);
            // 환경 표시는 R6.2 색으로 칠한다
            for (env, style) in [("[prod]", theme::env_style("prod")), ("[stg]", theme::env_style("stg"))] {
                if text.starts_with(env) {
                    st = st.patch(style);
                }
            }
            if i == *sel {
                st = Style::new().add_modifier(Modifier::REVERSED);
            }
            buf.set_stringn(0, y, truncate(&text, w, "…"), w, st);
        }
        if m.is_empty() {
            buf.set_stringn(0, 2, "맞는 항목 없음", w, theme::dim());
        }
        let _ = UnicodeWidthStr::width("");
    }
}
