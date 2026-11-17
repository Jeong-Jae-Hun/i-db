//! SQL·Redis 편집기(R7·R8·R21). Go `internal/editor` 처럼 글 하나와 바이트 커서로 다룬다.
//! 문장 나누기(sqlsafe::statement_at)가 바이트 자리를 받으므로 줄·열은 그릴 때만 계산한다.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use unicode_width::UnicodeWidthChar;

use crate::theme;

const COMMENT_MARK: &str = "-- ";
const POPUP_ROWS: usize = 8;
const UNDO_MAX: usize = 200;
const TAB_CELLS: usize = 4;

/// 자동완성 후보다. insert 가 비면 label 을 넣는다. prefix_len 은 바꿀 커서 앞 바이트 수(0 이면 단어 규칙)다.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Item {
    pub label: String,
    pub insert: String,
    pub kind: &'static str,
    pub detail: String,
    pub prefix_len: usize,
}

impl Item {
    pub fn new(label: impl Into<String>, kind: &'static str) -> Item {
        Item {
            label: label.into(),
            kind,
            ..Item::default()
        }
    }

    fn text(&self) -> &str {
        if self.insert.is_empty() { &self.label } else { &self.insert }
    }
}

/// JetBrains 식 SQL 라이브 템플릿(R21.1)이다.
pub const TEMPLATES: &[(&str, &str)] = &[
    ("cnt", "SELECT count(*) FROM $TABLE$;"),
    ("del", "DELETE FROM $TABLE$ WHERE $END$;"),
    ("ins", "INSERT INTO $TABLE$ ($COLS$) VALUES ($END$);"),
    ("lim", "LIMIT $END$"),
    ("sel", "SELECT * FROM $TABLE$ $END$;"),
    ("selc", "SELECT count(*) FROM $TABLE$ $END$;"),
    ("selw", "SELECT * FROM $TABLE$ WHERE $END$;"),
    ("upd", "UPDATE $TABLE$ SET $END$ WHERE ;"),
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Op {
    Other,
    Insert,
    Delete,
}

#[derive(Clone)]
struct Snap {
    text: String,
    cur: usize,
    anchor: Option<usize>,
}

/// 편집기 키 처리 뒤 바깥이 할 일이다.
#[derive(Debug, PartialEq, Eq)]
pub enum Effect {
    None,
    /// 이 키를 쓰지 않았다(Tab 은 포커스 이동 등)
    Ignored,
    /// 복사할 글
    Copy(String),
    /// 자동완성 목록을 열어 달라(완성기는 바깥 상태를 본다)
    Complete,
    /// 글이 바뀌었다(자동 저장 타이머)
    Changed,
}

#[derive(Default)]
struct Popup {
    open: bool,
    items: Vec<Item>,
    sel: usize,
    top: usize,
}

pub struct Editor {
    text: String,
    cur: usize,
    anchor: Option<usize>,
    /// 위아래 이동 때 지킬 화면 열
    goal_x: Option<usize>,
    top: usize,
    left: usize,
    w: usize,
    h: usize,
    pub redis: bool,
    undo: Vec<Snap>,
    redo: Vec<Snap>,
    last_op: Op,
    pop: Popup,
    /// 펼친 템플릿의 남은 자리. 첫 원소가 지금 자리이고 마지막은 $END$ 다
    stops: Vec<(usize, usize)>,
    clip: String,
    /// 사용자가 친 글이 있다(R45.1 끝낼 때 새 쿼리 저장 대상)
    pub typed: bool,
    /// 글이 바뀔 때마다 오른다
    rev: u64,
}

impl Default for Editor {
    fn default() -> Self {
        Editor::new()
    }
}

impl Editor {
    pub fn new() -> Editor {
        Editor {
            text: String::new(),
            cur: 0,
            anchor: None,
            goal_x: None,
            top: 0,
            left: 0,
            w: 40,
            h: 5,
            redis: false,
            undo: Vec::new(),
            redo: Vec::new(),
            last_op: Op::Other,
            pop: Popup::default(),
            stops: Vec::new(),
            clip: String::new(),
            typed: false,
            rev: 0,
        }
    }

    pub fn value(&self) -> &str {
        &self.text
    }

    /// 글을 통째로 바꾼다. 되돌리기 기록은 비운다(R21.2 단).
    pub fn set_value(&mut self, s: &str) {
        self.text = s.to_string();
        self.cur = self.cur.min(self.text.len());
        while !self.text.is_char_boundary(self.cur) {
            self.cur -= 1;
        }
        self.anchor = None;
        self.undo.clear();
        self.redo.clear();
        self.pop = Popup::default();
        self.stops.clear();
        self.goal_x = None;
    }

    pub fn cursor(&self) -> usize {
        self.cur
    }

    pub fn set_cursor(&mut self, off: usize) {
        let mut off = off.min(self.text.len());
        while !self.text.is_char_boundary(off) {
            off -= 1;
        }
        self.cur = off;
        self.anchor = None;
        self.goal_x = None;
    }

    pub fn selection(&self) -> Option<(usize, usize)> {
        let a = self.anchor?;
        if a == self.cur {
            return None;
        }
        Some((a.min(self.cur), a.max(self.cur)))
    }

    pub fn selected_text(&self) -> Option<&str> {
        self.selection().map(|(s, e)| &self.text[s..e])
    }

    pub fn clear_selection(&mut self) -> bool {
        let had = self.selection().is_some();
        self.anchor = None;
        had
    }

    pub fn popup_open(&self) -> bool {
        self.pop.open
    }

    pub fn close_popup(&mut self) {
        self.pop = Popup::default();
    }

    /// Tab 을 편집기가 쓰는지다(R13.4): 자동완성 창·템플릿 이름·템플릿 자리.
    pub fn wants_tab(&self) -> bool {
        self.pop.open || !self.stops.is_empty() || self.template_at().is_some()
    }

    pub fn set_size(&mut self, w: usize, h: usize) {
        self.w = w.max(1);
        self.h = h.max(1);
    }

    /// 커서 앞 단어의 시작이다.
    pub fn prefix_start(&self) -> usize {
        prefix_start(&self.text, self.cur)
    }

    /// 자동완성 후보를 연다. 후보가 없으면 닫는다. SQL 탭이면 커서 앞 단어로 시작하는 템플릿을 더한다.
    pub fn open_popup(&mut self, mut items: Vec<Item>) {
        let start = self.prefix_start();
        if !self.redis && self.stops.is_empty() && start < self.cur && (start == 0 || self.text.as_bytes()[start - 1] != b'.') {
            let word = &self.text[start..self.cur];
            let mut more = Vec::new();
            for (k, body) in TEMPLATES {
                let it = Item {
                    label: k.to_string(),
                    kind: "template",
                    detail: body.to_string(),
                    ..Item::default()
                };
                if *k == word {
                    items.insert(0, it);
                } else if k.starts_with(word) {
                    more.push(it);
                }
            }
            items.extend(more);
        }
        self.pop = if items.is_empty() {
            Popup::default()
        } else {
            Popup {
                open: true,
                items,
                sel: 0,
                top: 0,
            }
        };
    }

    // ─── 편집 ───

    fn snapshot(&self) -> Snap {
        Snap {
            text: self.text.clone(),
            cur: self.cur,
            anchor: self.anchor,
        }
    }

    /// text[start..end] 를 바꾼다. 연속한 글자 넣기·지우기는 되돌리기 한 번으로 묶는다.
    fn replace(&mut self, start: usize, end: usize, with: &str, op: Op) {
        let merge = op != Op::Other && op == self.last_op && !with.contains('\n');
        if !merge {
            self.undo.push(self.snapshot());
            if self.undo.len() > UNDO_MAX {
                self.undo.remove(0);
            }
        }
        self.redo.clear();
        self.last_op = op;
        // 템플릿 자리는 편집 뒤로 민다
        let delta = with.len() as isize - (end - start) as isize;
        for s in self.stops.iter_mut() {
            if s.0 >= end {
                s.0 = (s.0 as isize + delta) as usize;
                s.1 = (s.1 as isize + delta) as usize;
            } else if s.1 >= start {
                s.1 = (s.1 as isize + delta).max(s.0 as isize) as usize;
            }
        }
        self.text.replace_range(start..end, with);
        self.rev += 1;
        self.cur = start + with.len();
        self.anchor = None;
        self.goal_x = None;
        self.typed = true;
    }

    pub fn insert_str(&mut self, s: &str) {
        let (start, end) = self.selection().unwrap_or((self.cur, self.cur));
        let op = if s.chars().count() == 1 && !s.contains(char::is_whitespace) {
            Op::Insert
        } else {
            Op::Other
        };
        self.replace(start, end, s, op);
    }

    /// 범위를 바꾸고 커서를 끝에 둔다(완성·정렬).
    pub fn replace_range(&mut self, start: usize, end: usize, with: &str) {
        self.replace(start, end, with, Op::Other);
    }

    fn undo_step(&mut self, redo: bool) {
        let (from, to) = if redo {
            (&mut self.redo, &mut self.undo)
        } else {
            (&mut self.undo, &mut self.redo)
        };
        let Some(s) = from.pop() else { return };
        to.push(Snap {
            text: std::mem::take(&mut self.text),
            cur: self.cur,
            anchor: self.anchor,
        });
        self.text = s.text;
        self.rev += 1;
        self.cur = s.cur.min(self.text.len());
        self.anchor = s.anchor;
        self.last_op = Op::Other;
        self.stops.clear();
    }

    fn line_start(&self, off: usize) -> usize {
        self.text[..off].rfind('\n').map_or(0, |i| i + 1)
    }

    fn line_end(&self, off: usize) -> usize {
        self.text[off..].find('\n').map_or(self.text.len(), |i| off + i)
    }

    fn row_of(&self, off: usize) -> usize {
        self.text.as_bytes()[..off].iter().filter(|&&b| b == b'\n').count()
    }

    fn row_start(&self, row: usize) -> usize {
        if row == 0 {
            return 0;
        }
        self.text.match_indices('\n').nth(row - 1).map_or(self.text.len(), |(i, _)| i + 1)
    }

    fn row_count(&self) -> usize {
        self.text.matches('\n').count() + 1
    }

    /// 줄 안 off 까지의 화면 칸 수다.
    fn col_of(&self, off: usize) -> usize {
        cells(&self.text[self.line_start(off)..off])
    }

    /// row 줄에서 화면 칸 x 에 가장 가까운 바이트 자리다.
    fn offset_at(&self, row: usize, x: usize) -> usize {
        let start = self.row_start(row);
        let end = self.line_end(start);
        let mut w = 0;
        for (i, ch) in self.text[start..end].char_indices() {
            let cw = char_cells(ch);
            if w + cw > x {
                return start + i;
            }
            w += cw;
        }
        end
    }

    fn selected_rows(&self) -> (usize, usize) {
        match self.selection() {
            Some((s, e)) => {
                let mut last = self.row_of(e);
                // 줄 첫 칸에서 끝나는 선택은 그 줄을 걸치지 않는다
                if e > s && self.line_start(e) == e && last > self.row_of(s) {
                    last -= 1;
                }
                (self.row_of(s), last)
            }
            None => {
                let r = self.row_of(self.cur);
                (r, r)
            }
        }
    }

    fn toggle_comment(&mut self) {
        let (first, last) = self.selected_rows();
        let start = self.row_start(first);
        let end = self.line_end(self.row_start(last));
        let lines: Vec<&str> = self.text[start..end].split('\n').collect();
        let nonblank: Vec<&&str> = lines.iter().filter(|l| !l.trim().is_empty()).collect();
        let uncomment = !nonblank.is_empty() && nonblank.iter().all(|l| l.trim_start_matches([' ', '\t']).starts_with("--"));
        let had_sel = self.selection().is_some();
        let single = lines.len() == 1;
        let out: Vec<String> = lines
            .iter()
            .map(|l| {
                let ind = l.len() - l.trim_start_matches([' ', '\t']).len();
                let t = &l[ind..];
                if uncomment {
                    if let Some(r) = t.strip_prefix(COMMENT_MARK) {
                        format!("{}{r}", &l[..ind])
                    } else if let Some(r) = t.strip_prefix("--") {
                        format!("{}{r}", &l[..ind])
                    } else {
                        l.to_string()
                    }
                } else if single || !t.trim().is_empty() {
                    format!("{COMMENT_MARK}{l}")
                } else {
                    l.to_string()
                }
            })
            .collect();
        let block = out.join("\n");
        let old = self.cur;
        let len = block.len();
        self.replace(start, end, &block, Op::Other);
        if had_sel {
            self.anchor = Some(start);
            self.cur = start + len;
        } else if !uncomment {
            self.cur = (old + COMMENT_MARK.len()).min(self.text.len());
        } else {
            self.cur = old.min(start + len).max(start);
            while !self.text.is_char_boundary(self.cur) {
                self.cur -= 1;
            }
        }
    }

    fn duplicate_lines(&mut self) {
        let (first, last) = self.selected_rows();
        let start = self.row_start(first);
        let end = self.line_end(self.row_start(last));
        let add = format!("\n{}", &self.text[start..end]);
        let (old, anchor) = (self.cur, self.anchor);
        self.replace(end, end, &add, Op::Other);
        self.cur = old + add.len();
        self.anchor = anchor.map(|a| a + add.len());
    }

    fn template_at(&self) -> Option<(usize, &'static str)> {
        if self.selection().is_some() || self.redis {
            return None;
        }
        if self.text[self.cur..].chars().next().is_some_and(is_word) {
            return None;
        }
        let start = self.prefix_start();
        if start == self.cur || start > 0 && self.text.as_bytes()[start - 1] == b'.' {
            return None;
        }
        let word = &self.text[start..self.cur];
        TEMPLATES.iter().find(|(k, _)| *k == word).map(|(_, b)| (start, *b))
    }

    fn expand_at(&mut self, start: usize, body: &str) {
        let (text, stops) = expand(body);
        self.replace(start, self.cur, &text, Op::Other);
        self.stops = stops.into_iter().map(|(s, e)| (s + start, e + start)).collect();
        self.select_stop();
    }

    fn select_stop(&mut self) {
        let (s, e) = self.stops[0];
        self.cur = e;
        self.anchor = if s == e { None } else { Some(s) };
        if self.stops.len() == 1 {
            self.stops.clear();
        }
    }

    fn accept(&mut self) {
        let it = self.pop.items[self.pop.sel].clone();
        self.pop = Popup::default();
        if it.kind == "template" {
            let start = self.prefix_start();
            if let Some((_, body)) = TEMPLATES.iter().find(|(k, _)| *k == it.label) {
                self.expand_at(start, body);
            }
            return;
        }
        let mut start = if it.prefix_len > 0 {
            let mut s = self.cur.saturating_sub(it.prefix_len);
            while !self.text.is_char_boundary(s) {
                s += 1;
            }
            s
        } else {
            self.prefix_start()
        };
        // "Us 처럼 따옴표를 연 채 고르면 따옴표째 바꿔 "" 가 겹치지 않게 한다
        if it.text().starts_with('"') && start > 0 && self.text.as_bytes()[start - 1] == b'"' {
            start -= 1;
        }
        let text = it.text().to_string();
        self.replace(start, self.cur, &text, Op::Other);
    }

    fn move_to(&mut self, off: usize, select: bool) {
        if select {
            if self.anchor.is_none() {
                self.anchor = Some(self.cur);
            }
        } else {
            self.anchor = None;
        }
        self.cur = off;
        self.last_op = Op::Other;
    }

    fn vertical(&mut self, d: isize, select: bool) {
        let row = self.row_of(self.cur) as isize;
        let x = self.goal_x.unwrap_or_else(|| self.col_of(self.cur));
        let target = row + d;
        let off = if target < 0 {
            0
        } else if target as usize >= self.row_count() {
            self.text.len()
        } else {
            self.offset_at(target as usize, x)
        };
        self.move_to(off, select);
        self.goal_x = Some(x);
    }

    /// 키 하나를 처리한다. 이름은 keys::name 꼴이다.
    pub fn key(&mut self, k: &str) -> Effect {
        if self.pop.open {
            let n = self.pop.items.len();
            match k {
                "up" | "ctrl+p" => {
                    self.pop.sel = (self.pop.sel + n - 1) % n;
                    self.follow_popup();
                    return Effect::None;
                }
                "down" | "ctrl+n" => {
                    self.pop.sel = (self.pop.sel + 1) % n;
                    self.follow_popup();
                    return Effect::None;
                }
                "enter" | "tab" => {
                    self.accept();
                    return Effect::Changed;
                }
                "esc" => {
                    self.pop = Popup::default();
                    return Effect::None;
                }
                _ => self.pop = Popup::default(),
            }
        }
        let before = self.rev;
        let eff = self.key_inner(k);
        if eff == Effect::None && self.rev != before {
            return Effect::Changed;
        }
        eff
    }

    fn follow_popup(&mut self) {
        if self.pop.sel < self.pop.top {
            self.pop.top = self.pop.sel;
        }
        if self.pop.sel >= self.pop.top + POPUP_ROWS {
            self.pop.top = self.pop.sel + 1 - POPUP_ROWS;
        }
    }

    fn key_inner(&mut self, k: &str) -> Effect {
        let shift = k.starts_with("shift+") || k.starts_with("ctrl+shift+");
        let base = k.trim_start_matches("ctrl+shift+").trim_start_matches("shift+");
        let len = self.text.len();
        match k {
            "tab" => {
                if !self.stops.is_empty() {
                    self.stops.remove(0);
                    if !self.stops.is_empty() {
                        self.select_stop();
                    }
                    return Effect::None;
                }
                if let Some((start, body)) = self.template_at() {
                    self.expand_at(start, body);
                    return Effect::Changed;
                }
                return Effect::Ignored;
            }
            "esc" => {
                if !self.stops.is_empty() {
                    self.stops.clear();
                    return Effect::None;
                }
                if self.clear_selection() {
                    return Effect::None;
                }
                return Effect::Ignored;
            }
            "enter" => {
                let ls = self.line_start(self.cur);
                let line = &self.text[ls..self.cur];
                let ind = &line[..line.len() - line.trim_start_matches([' ', '\t']).len()];
                let s = format!("\n{ind}");
                self.insert_str(&s);
                return Effect::Changed;
            }
            "space" => {
                self.insert_str(" ");
                return Effect::Changed;
            }
            "backspace" => {
                if let Some((s, e)) = self.selection() {
                    self.replace(s, e, "", Op::Other);
                } else if self.cur > 0 {
                    let p = prev_boundary(&self.text, self.cur);
                    self.replace(p, self.cur, "", Op::Delete);
                }
                return Effect::Changed;
            }
            "delete" => {
                if let Some((s, e)) = self.selection() {
                    self.replace(s, e, "", Op::Other);
                } else if self.cur < len {
                    let n = next_boundary(&self.text, self.cur);
                    let c = self.cur;
                    self.replace(c, n, "", Op::Delete);
                    self.cur = c;
                }
                return Effect::Changed;
            }
            "ctrl+a" => {
                self.anchor = Some(0);
                self.cur = len;
                return Effect::None;
            }
            "ctrl+k" | "ctrl+c" => {
                return match self.selected_text().map(str::to_string) {
                    Some(t) => {
                        self.clip = t.clone();
                        Effect::Copy(t)
                    }
                    None => Effect::None,
                };
            }
            "ctrl+x" => {
                let Some((s, e)) = self.selection() else {
                    return Effect::None;
                };
                let t = self.text[s..e].to_string();
                self.replace(s, e, "", Op::Other);
                self.clip = t.clone();
                return Effect::Copy(t);
            }
            "ctrl+v" => {
                let c = self.clip.clone();
                if !c.is_empty() {
                    self.insert_str(&c);
                }
                return Effect::Changed;
            }
            "ctrl+z" => {
                self.undo_step(false);
                return Effect::Changed;
            }
            "ctrl+y" => {
                self.undo_step(true);
                return Effect::Changed;
            }
            "ctrl+d" => {
                self.duplicate_lines();
                return Effect::Changed;
            }
            "ctrl+/" => {
                self.toggle_comment();
                return Effect::Changed;
            }
            "ctrl+@" => return Effect::Complete,
            "ctrl+up" => {
                self.top = self.top.saturating_sub(1);
                return Effect::None;
            }
            "ctrl+down" => {
                self.top = (self.top + 1).min(self.row_count().saturating_sub(1));
                return Effect::None;
            }
            _ => {}
        }
        match base {
            "left" => {
                let off = if !shift && let Some((s, _)) = self.selection() {
                    s
                } else {
                    prev_boundary(&self.text, self.cur)
                };
                self.move_to(off, shift);
                self.goal_x = None;
            }
            "right" => {
                let off = if !shift && let Some((_, e)) = self.selection() {
                    e
                } else {
                    next_boundary(&self.text, self.cur)
                };
                self.move_to(off, shift);
                self.goal_x = None;
            }
            "up" => self.vertical(-1, shift),
            "down" => self.vertical(1, shift),
            "pgup" => self.vertical(-(self.h as isize), shift),
            "pgdown" => self.vertical(self.h as isize, shift),
            "home" => {
                let ls = self.line_start(self.cur);
                // 들여쓰기 뒤 첫 글자, 이미 거기면 줄 처음
                let ind =
                    ls + self.text[ls..self.line_end(ls)].len() - self.text[ls..self.line_end(ls)].trim_start_matches([' ', '\t']).len();
                let off = if self.cur == ind { ls } else { ind };
                self.move_to(off, shift);
                self.goal_x = None;
            }
            "end" => {
                let off = self.line_end(self.cur);
                self.move_to(off, shift);
                self.goal_x = None;
            }
            "ctrl+left" => {
                let off = word_left(&self.text, self.cur);
                self.move_to(off, shift);
                self.goal_x = None;
            }
            "ctrl+right" => {
                let off = word_right(&self.text, self.cur);
                self.move_to(off, shift);
                self.goal_x = None;
            }
            "ctrl+home" => self.move_to(0, shift),
            "ctrl+end" => self.move_to(len, shift),
            _ => {
                // 글자 하나(한글 포함)만 넣는다. 수정자 붙은 이름은 무시한다
                let mut chars = k.chars();
                if let (Some(c), None) = (chars.next(), chars.next())
                    && !c.is_control()
                {
                    self.insert_str(k);
                    return Effect::Changed;
                }
                return Effect::Ignored;
            }
        }
        Effect::None
    }

    /// 붙여넣기 본문을 넣는다. CRLF 는 LF 로 맞춘다.
    pub fn paste(&mut self, s: &str) {
        let s = s.replace("\r\n", "\n").replace('\r', "\n");
        self.insert_str(&s);
    }

    /// 화면 안 (x, y) 클릭으로 커서를 옮긴다. 거터를 뺀 자리다.
    pub fn click(&mut self, x: usize, y: usize, select: bool) {
        let row = (self.top + y).min(self.row_count().saturating_sub(1));
        let gw = self.gutter_width();
        let off = self.offset_at(row, (x.saturating_sub(gw)) + self.left);
        self.move_to(off, select);
        self.goal_x = None;
        self.pop = Popup::default();
    }

    /// 커서 자리 낱말을 고른다(더블클릭, R13.2).
    pub fn select_word(&mut self) {
        let s = prefix_start(&self.text, self.cur);
        let mut e = self.cur;
        while let Some(c) = self.text[e..].chars().next() {
            if !is_word(c) {
                break;
            }
            e += c.len_utf8();
        }
        if s < e {
            self.anchor = Some(s);
            self.cur = e;
        }
    }

    pub fn scroll(&mut self, d: isize) {
        let max = self.row_count().saturating_sub(1) as isize;
        self.top = (self.top as isize + d).clamp(0, max) as usize;
    }

    /// 커서 아래 이름(`schema.name`, `"따옴표 이름"` 포함, R26)이다.
    pub fn ident_at(&self) -> String {
        let b = self.text.as_bytes();
        let ok = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'.' || c == b'"' || c >= 0x80 || c == b'$';
        let mut s = self.cur;
        while s > 0 && ok(b[s - 1]) {
            s -= 1;
        }
        let mut e = self.cur;
        while e < b.len() && ok(b[e]) {
            e += 1;
        }
        while s < e && !self.text.is_char_boundary(s) {
            s += 1;
        }
        while e > s && !self.text.is_char_boundary(e) {
            e -= 1;
        }
        self.text[s..e].trim_matches('.').to_string()
    }

    fn gutter_width(&self) -> usize {
        self.row_count().to_string().len().max(3) + 1
    }

    fn ensure_visible(&mut self) {
        let row = self.row_of(self.cur);
        if row < self.top {
            self.top = row;
        }
        if row >= self.top + self.h {
            self.top = row + 1 - self.h;
        }
        let cw = self.w.saturating_sub(self.gutter_width()).max(1);
        let x = self.col_of(self.cur);
        if x < self.left {
            self.left = x;
        }
        if x >= self.left + cw {
            self.left = x + 1 - cw;
        }
    }

    // ─── 그리기 ───

    /// area 에 그린다. 빈 편집기면 placeholder 를 흐리게 보인다(R7.1).
    pub fn render(&mut self, area: Rect, buf: &mut Buffer, focused: bool, placeholder: &str) {
        self.set_size(area.width as usize, area.height as usize);
        self.ensure_visible();
        let gw = self.gutter_width();
        let styles = if self.redis {
            redis_styles(&self.text)
        } else {
            sql_styles(&self.text)
        };
        let sel = self.selection();
        let cur_row = self.row_of(self.cur);
        let mut off = self.row_start(self.top);
        for y in 0..area.height as usize {
            let row = self.top + y;
            if row >= self.row_count() {
                break;
            }
            let yy = area.y + y as u16;
            let num = format!("{:>w$} ", row + 1, w = gw - 1);
            let nst = if row == cur_row {
                Style::new().fg(theme::TEXT)
            } else {
                theme::dim()
            };
            buf.set_stringn(area.x, yy, &num, gw, nst);
            let end = self.line_end(off);
            let mut x = 0usize;
            let maxx = area.width as usize - gw.min(area.width as usize);
            let mut cursor_drawn = false;
            for (i, ch) in self.text[off..end].char_indices() {
                let at = off + i;
                let cw = char_cells(ch);
                if x + cw > self.left && x >= self.left && x - self.left + cw <= maxx {
                    let mut st = styles.get(at).copied().unwrap_or_default();
                    if sel.is_some_and(|(s, e)| at >= s && at < e) {
                        st = st.bg(Color::Indexed(24));
                    }
                    if focused && at == self.cur {
                        st = st.add_modifier(Modifier::REVERSED);
                        cursor_drawn = true;
                    }
                    let px = area.x + (gw + x - self.left) as u16;
                    let glyph = if ch == '\t' {
                        " ".repeat(TAB_CELLS)
                    } else if ch.is_control() {
                        "?".into()
                    } else {
                        ch.to_string()
                    };
                    buf.set_stringn(px, yy, &glyph, cw, st);
                }
                x += cw;
            }
            if focused && !cursor_drawn && row == cur_row && x >= self.left && x - self.left < maxx {
                let px = area.x + (gw + x - self.left) as u16;
                buf.set_string(px, yy, " ", Style::new().add_modifier(Modifier::REVERSED));
            }
            if self.text.is_empty() && y == 0 && !placeholder.is_empty() {
                let px = area.x + gw as u16 + 1;
                if px < area.x + area.width {
                    buf.set_stringn(px, yy, format!(" {placeholder}"), (area.x + area.width - px) as usize, theme::dim());
                }
            }
            off = (end + 1).min(self.text.len());
        }
        if self.pop.open {
            self.render_popup(area, buf, gw, cur_row);
        }
    }

    fn render_popup(&self, area: Rect, buf: &mut Buffer, gw: usize, cur_row: usize) {
        let rows = self.pop.items.len().min(POPUP_ROWS);
        let w = self
            .pop
            .items
            .iter()
            .map(|i| cells(&i.label) + i.kind.len() + 3)
            .max()
            .unwrap_or(10)
            .clamp(12, 48)
            .min(area.width as usize);
        let px = (area.x as usize + gw + self.col_of(self.cur).saturating_sub(self.left)).min((area.x + area.width) as usize - w);
        let below = area.y as usize + cur_row.saturating_sub(self.top) + 1;
        let py = if below + rows <= (area.y + area.height) as usize {
            below
        } else {
            below.saturating_sub(rows + 1).max(area.y as usize)
        };
        for (i, it) in self.pop.items.iter().skip(self.pop.top).take(rows).enumerate() {
            let st = if self.pop.top + i == self.pop.sel {
                Style::new().bg(theme::ACCENT).fg(theme::BLACK)
            } else {
                Style::new().bg(Color::Indexed(237)).fg(theme::TEXT)
            };
            let line = format!(" {:<lw$} {}", it.label, it.kind, lw = w.saturating_sub(it.kind.len() + 3));
            let y = (py + i) as u16;
            if y < buf.area.y + buf.area.height {
                buf.set_stringn(px as u16, y, &line, w, st);
            }
        }
    }
}

// ─── 글자 ───

pub fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

fn char_cells(c: char) -> usize {
    if c == '\t' { TAB_CELLS } else { c.width().unwrap_or(1).max(1) }
}

fn cells(s: &str) -> usize {
    s.chars().map(char_cells).sum()
}

fn prev_boundary(s: &str, off: usize) -> usize {
    s[..off].char_indices().next_back().map_or(0, |(i, _)| i)
}

fn next_boundary(s: &str, off: usize) -> usize {
    s[off..].chars().next().map_or(off, |c| off + c.len_utf8())
}

fn prefix_start(s: &str, mut off: usize) -> usize {
    while let Some(c) = s[..off].chars().next_back() {
        if !is_word(c) {
            break;
        }
        off -= c.len_utf8();
    }
    off
}

/// 단어 이동의 경계다. 0 공백, 1 단어, 2 줄바꿈, 3 그 밖의 기호.
fn class(c: char) -> u8 {
    match c {
        ' ' | '\t' => 0,
        '\n' | '\r' => 2,
        c if is_word(c) => 1,
        _ => 3,
    }
}

fn word_right(s: &str, off: usize) -> usize {
    let mut it = s[off..].char_indices().peekable();
    let Some(&(_, c)) = it.peek() else { return s.len() };
    if class(c) == 2 {
        return off + c.len_utf8();
    }
    let mut pos = off;
    // 공백을 건너뛰고 같은 종류 글자 묶음 끝까지
    while let Some(&(i, c)) = it.peek() {
        if class(c) != 0 {
            break;
        }
        pos = off + i + c.len_utf8();
        it.next();
    }
    let Some(&(_, first)) = it.peek() else { return pos };
    let k = class(first);
    if k == 2 {
        return pos;
    }
    while let Some(&(i, c)) = it.peek() {
        if class(c) != k {
            break;
        }
        pos = off + i + c.len_utf8();
        it.next();
    }
    pos
}

fn word_left(s: &str, off: usize) -> usize {
    let mut pos = off;
    let mut it = s[..off].char_indices().rev().peekable();
    let Some(&(i, c)) = it.peek() else { return 0 };
    if class(c) == 2 {
        return i;
    }
    while let Some(&(i, c)) = it.peek() {
        if class(c) != 0 {
            break;
        }
        pos = i;
        it.next();
    }
    let Some(&(_, first)) = it.peek() else { return pos };
    let k = class(first);
    if k == 2 {
        return pos;
    }
    while let Some(&(i, c)) = it.peek() {
        if class(c) != k {
            break;
        }
        pos = i;
        it.next();
    }
    pos
}

/// 템플릿 본문의 $NAME$ 를 소문자 이름으로 바꾼 글과 자리 목록이다. $END$ 는 빈 자리로 맨 뒤에 두고,
/// 없으면 글 끝을 $END$ 로 본다.
fn expand(body: &str) -> (String, Vec<(usize, usize)>) {
    let mut out = String::new();
    let mut stops = Vec::new();
    let mut end: Option<(usize, usize)> = None;
    let mut rest = body;
    while let Some(i) = rest.find('$') {
        let after = &rest[i + 1..];
        let Some(j) = after.find('$') else { break };
        let name = &after[..j];
        if name.is_empty() || !name.bytes().all(|c| c.is_ascii_uppercase() || c == b'_') {
            out.push_str(&rest[..=i]);
            rest = after;
            continue;
        }
        out.push_str(&rest[..i]);
        let at = out.len();
        if name == "END" {
            end.get_or_insert((at, at));
        } else {
            out.push_str(&name.to_ascii_lowercase());
            stops.push((at, out.len()));
        }
        rest = &after[j + 1..];
    }
    out.push_str(rest);
    stops.push(end.unwrap_or((out.len(), out.len())));
    (out, stops)
}

// ─── 색 (R7.1) ───

/// 글 바이트마다의 색이다. SQL 토큰(sqlfmt::tokenize)으로 칠한다.
fn sql_styles(text: &str) -> Vec<Style> {
    use idb_core::sqlfmt::{Kind, tokenize};
    let mut out = vec![Style::new().fg(theme::TEXT); text.len()];
    for t in tokenize(text) {
        let word = &text[t.start..t.end];
        let st = match t.kind {
            Kind::Word if crate::complete::is_sql_keyword(&word.to_ascii_lowercase()) => {
                Style::new().fg(theme::ACCENT).add_modifier(Modifier::BOLD)
            }
            Kind::String => Style::new().fg(Color::Indexed(150)),
            Kind::Number => Style::new().fg(Color::Indexed(180)),
            Kind::LineComment | Kind::BlockComment => theme::dim(),
            Kind::Param => Style::new().fg(Color::Indexed(213)),
            Kind::Quoted => Style::new().fg(Color::Indexed(223)),
            _ => continue,
        };
        out[t.start..t.end].fill(st);
    }
    // :name 자리(R21.3)
    let b = text.as_bytes();
    let mut i = 0;
    while i + 1 < b.len() {
        if b[i] == b':' && (i == 0 || b[i - 1] != b':') && b[i + 1] != b':' && (b[i + 1].is_ascii_alphabetic() || b[i + 1] == b'_') {
            let mut j = i + 1;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                j += 1;
            }
            if out[i].fg == Some(theme::TEXT) {
                out[i..j].fill(Style::new().fg(Color::Indexed(213)));
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

/// Redis 는 줄 첫 낱말을 명령으로, 따옴표 글을 문자열로 칠한다.
fn redis_styles(text: &str) -> Vec<Style> {
    let mut out = vec![Style::new().fg(theme::TEXT); text.len()];
    let mut start = 0;
    for line in text.split('\n') {
        let trimmed = line.trim_start();
        let lead = line.len() - trimmed.len();
        let w = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
        out[start + lead..start + lead + w].fill(Style::new().fg(theme::REDIS).add_modifier(Modifier::BOLD));
        let mut q: Option<(usize, u8)> = None;
        for (i, c) in line.bytes().enumerate() {
            match q {
                None if c == b'"' || c == b'\'' => q = Some((i, c)),
                Some((s, m)) if c == m => {
                    out[start + s..=start + i].fill(Style::new().fg(Color::Indexed(150)));
                    q = None;
                }
                _ => {}
            }
        }
        start += line.len() + 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(s: &str) -> Editor {
        let mut e = Editor::new();
        for c in s.chars() {
            match c {
                '\n' => e.key("enter"),
                ' ' => e.key("space"),
                c => e.key(&c.to_string()),
            };
        }
        e
    }

    #[test]
    fn 글자를_넣고_되돌리기는_낱말_묶음으로_한다() {
        let mut e = typed("select 1");
        assert_eq!(e.value(), "select 1");
        e.key("ctrl+z");
        assert_eq!(e.value(), "select ");
        e.key("ctrl+z");
        assert_eq!(e.value(), "select");
        e.key("ctrl+y");
        assert_eq!(e.value(), "select ");
    }

    #[test]
    fn 엔터는_들여쓰기를_잇고_한글도_한_글자씩_지운다() {
        let mut e = typed("  a\nb");
        assert_eq!(e.value(), "  a\n  b");
        e.key("backspace");
        e.key("가");
        e.key("나");
        e.key("backspace");
        assert_eq!(e.value(), "  a\n  가");
    }

    #[test]
    fn 템플릿_이름_뒤_Tab_은_펼치고_자리를_돈다() {
        let mut e = typed("sel");
        assert!(e.wants_tab());
        e.key("tab");
        assert_eq!(e.value(), "SELECT * FROM table ;");
        assert_eq!(e.selected_text(), Some("table"));
        e.key("o");
        assert_eq!(e.value(), "SELECT * FROM o ;");
        e.key("tab");
        assert_eq!(e.cursor(), "SELECT * FROM o ".len());
        assert!(!e.wants_tab());
        let mut r = typed("sel");
        r.redis = true;
        assert!(!r.wants_tab());
    }

    #[test]
    fn 주석_토글과_줄_복제() {
        let mut e = typed("a\nb");
        e.key("ctrl+a");
        e.key("ctrl+/");
        assert_eq!(e.value(), "-- a\n-- b");
        e.key("ctrl+/");
        assert_eq!(e.value(), "a\nb");
        let mut d = typed("x");
        d.key("ctrl+d");
        assert_eq!(d.value(), "x\nx");
    }

    #[test]
    fn 위아래는_화면_열을_지키고_단어_이동은_기호를_묶는다() {
        let mut e = typed("abcdef\nab\nabcdef");
        e.set_cursor(5);
        e.key("down");
        assert_eq!(e.cursor(), 9);
        e.key("down");
        assert_eq!(e.cursor(), 15);
        assert_eq!(word_right("foo.bar baz", 0), 3);
        assert_eq!(word_right("foo.bar baz", 7), 11);
        assert_eq!(word_left("foo.bar baz", 11), 8);
    }

    #[test]
    fn 자동완성은_고른_후보로_단어를_바꾸고_따옴표를_겹치지_않는다() {
        let mut e = typed("select \"Us");
        e.open_popup(vec![Item {
            label: "Users".into(),
            insert: "\"Users\"".into(),
            kind: "table",
            ..Item::default()
        }]);
        e.key("enter");
        assert_eq!(e.value(), "select \"Users\"");
    }

    #[test]
    fn 커서_아래_이름은_스키마와_따옴표를_담는다() {
        let mut e = Editor::new();
        e.set_value("select * from public.\"Orders\" o");
        e.set_cursor(23);
        assert_eq!(e.ident_at(), "public.\"Orders\"");
    }
}
