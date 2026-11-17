//! 결과 표(R14·R15·R31·R35·R48·R49). 칸은 글이고 NULL 은 `"NULL"` 글자다(serve 프로토콜).
//! 정렬·거르기는 받은 행 안에서 하고, 테이블 보기의 서버 정렬·WHERE 는 tableview 가 맡는다.
//! 보류 중인 바꿈은 행 자리(결과 행 번호, 추가 행은 결과 뒤 번호)마다 두고 change_set 이 제출 묶음으로 만든다.

use std::collections::{BTreeMap, BTreeSet};

use idb_core::result::{NULL_TEXT, is_numeric_type};
use idb_core::textutil::printable;
use idb_engine::{Change, ChangeOp, ChangeSet, ResultSet};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use unicode_width::UnicodeWidthStr;

use crate::input::LineInput;
use crate::theme;

const MIN_W: usize = 3;
const MAX_W: usize = 40;
const WIDTH_STEP: usize = 2;
/// 붙여넣기 한 번의 칸 상한(R48.1)
const MAX_PASTE_CELLS: usize = 10_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Text,
    Number,
    Bool,
    Json,
    Date,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Screen {
    Table,
    Filter,
    Edit,
    Select,
    Record,
    Value,
}

#[derive(Clone, Default, Debug)]
struct RowEdit {
    /// 열 → 새 값(None 은 NULL)
    vals: BTreeMap<usize, Option<String>>,
    deleted: bool,
    inserted: bool,
}

/// 키 처리 뒤 바깥이 할 일이다.
#[derive(Debug, PartialEq, Eq)]
pub enum Act {
    None,
    Ignored,
    Copy(String, String),
    Status(String),
    /// e·E 내보내기(csv·json)
    Export(&'static str),
    /// 거르기 확정(R75.5 기록)
    FilterDone(String),
    /// 칸 고치기를 시작하려는데 열이 enum 이다(R49.2)
    EnumPick(usize),
}

pub struct Grid {
    pub res: ResultSet,
    kinds: Vec<Kind>,
    /// 보이는 행 자리(결과 행 번호, 추가 행은 결과 뒤 번호)
    view: Vec<usize>,
    pub row: usize,
    pub col: usize,
    top: usize,
    left: usize,
    widths: Vec<usize>,
    /// 정렬 열과 내림차순
    pub sort: Option<(usize, bool)>,
    pub filter: String,
    pub screen: Screen,
    pub input: LineInput,
    edit_null: bool,
    edit_at: (usize, usize),
    edits: BTreeMap<usize, RowEdit>,
    inserts: usize,
    anchor: (usize, usize),
    /// 레코드·값 보기 스크롤
    pub scroll: usize,
    /// 결과가 이미 서버에서 정렬·거른 것이라 받은 행에서 다시 하지 않는다(테이블 보기)
    pub server: bool,
    json: Option<crate::jsontree::JsonTree>,
    json_raw: bool,
    /// 복호화 보기(R37): (결과 행, 열) → 푼 값. 화면과 y·Y 복사에만 쓰고 내보내기·IN·편집은 원래 값이다
    pub dec: Option<std::collections::HashMap<(usize, usize), String>>,
    h: usize,
    /// 원래 PK 열 자리(origin.pk 순서). 결과에 PK 가 모두 없으면 None
    pk_cols: Option<Vec<usize>>,
}

pub fn is_null(v: &str) -> bool {
    v == NULL_TEXT
}

fn cell(row: &[String], c: usize) -> &str {
    row.get(c).map_or("", String::as_str)
}

fn kinds_of(r: &ResultSet) -> Vec<Kind> {
    (0..r.cols.len())
        .map(|i| {
            if let Some(t) = r.types.as_ref().filter(|t| t.len() == r.cols.len()) {
                let t = t[i].as_str();
                return match t {
                    _ if is_numeric_type(t) => Kind::Number,
                    "bool" => Kind::Bool,
                    "json" | "jsonb" => Kind::Json,
                    "date" | "timestamp" | "timestamptz" => Kind::Date,
                    _ => Kind::Text,
                };
            }
            let mut seen = false;
            for row in &r.rows {
                let v = cell(row, i);
                if is_null(v) {
                    continue;
                }
                if v.parse::<f64>().is_err() {
                    return Kind::Text;
                }
                seen = true;
            }
            if seen { Kind::Number } else { Kind::Text }
        })
        .collect()
}

fn display(k: Kind, raw: &str) -> String {
    match (k, raw) {
        (Kind::Bool, "t") => "true".into(),
        (Kind::Bool, "f") => "false".into(),
        _ => printable(raw, false),
    }
}

/// CSV 칸이다. 스프레드시트 수식 실행을 막으려 `= + - @` 탭·CR 로 시작하는 값 앞에 `'` 를 붙인다(R14.2).
/// 단, `-1.5`·`+3` 처럼 숫자로 읽히는 값은 그대로다.
pub fn csv_cell(v: &str) -> String {
    let risky = v.starts_with(['=', '+', '-', '@', '\t', '\r']) && v.parse::<f64>().is_err();
    let v = if risky { format!("'{v}") } else { v.to_string() };
    if v.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", v.replace('"', "\"\""))
    } else {
        v
    }
}

fn tsv_cell(v: &str) -> String {
    if v.contains(['\t', '\n', '"', '\r']) {
        format!("\"{}\"", v.replace('"', "\"\""))
    } else {
        v.to_string()
    }
}

/// 시트에서 복사한 TSV 를 칸으로 읽는다. 따옴표 칸 안의 탭·줄바꿈은 값이다(R48.1).
pub fn parse_tsv(s: &str) -> Vec<Vec<String>> {
    let s = s.strip_suffix('\n').unwrap_or(s);
    let s = s.strip_suffix('\r').unwrap_or(s);
    let mut rows = vec![vec![String::new()]];
    let mut chars = s.chars().peekable();
    let mut quoted = false;
    let mut at_start = true;
    while let Some(c) = chars.next() {
        let cur = rows.last_mut().unwrap().last_mut().unwrap();
        if quoted {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    chars.next();
                    cur.push('"');
                }
                '"' => quoted = false,
                '\r' => {
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    cur.push('\n');
                }
                c => cur.push(c),
            }
            continue;
        }
        match c {
            '"' if at_start => {
                quoted = true;
                at_start = false;
                continue;
            }
            '\t' => {
                rows.last_mut().unwrap().push(String::new());
                at_start = true;
                continue;
            }
            '\r' if chars.peek() == Some(&'\n') => {}
            '\n' | '\r' => {
                rows.push(vec![String::new()]);
                at_start = true;
                continue;
            }
            c => cur.push(c),
        }
        at_start = false;
    }
    rows
}

impl Grid {
    pub fn new(res: ResultSet) -> Grid {
        let mut g = Grid {
            kinds: kinds_of(&res),
            res,
            view: Vec::new(),
            row: 0,
            col: 0,
            top: 0,
            left: 0,
            widths: Vec::new(),
            sort: None,
            filter: String::new(),
            screen: Screen::Table,
            input: LineInput::default(),
            edit_null: false,
            edit_at: (0, 0),
            edits: BTreeMap::new(),
            inserts: 0,
            anchor: (0, 0),
            scroll: 0,
            server: false,
            json: None,
            json_raw: false,
            dec: None,
            h: 10,
            pk_cols: None,
        };
        g.pk_cols = g.find_pk();
        g.reflow();
        g.rebuild();
        g
    }

    fn find_pk(&self) -> Option<Vec<usize>> {
        let o = self.res.origin.as_ref()?;
        o.pk.iter().map(|k| o.cols.iter().position(|c| c == k)).collect()
    }

    pub fn kind(&self, c: usize) -> Kind {
        self.kinds.get(c).copied().unwrap_or(Kind::Text)
    }

    pub fn col_type(&self, c: usize) -> &str {
        self.res.types.as_ref().and_then(|t| t.get(c)).map_or("", String::as_str)
    }

    /// 열 폭을 머리 글과 칸(앞 1,000행)으로 다시 잰다.
    fn reflow(&mut self) {
        self.widths = (0..self.res.cols.len())
            .map(|c| {
                let mut w = printable(&self.res.cols[c], false).width();
                for row in self.res.rows.iter().take(1000) {
                    w = w.max(display(self.kind(c), cell(row, c)).width());
                }
                w.clamp(MIN_W, MAX_W)
            })
            .collect();
    }

    pub fn widths(&self) -> &[usize] {
        &self.widths
    }

    pub fn set_width(&mut self, c: usize, w: usize) {
        if let Some(x) = self.widths.get_mut(c) {
            *x = w.clamp(MIN_W, 200);
        }
    }

    /// 보류 중인 바꿈을 얹은 행 값이다. 추가 행의 정하지 않은 칸은 "" 다(DB 기본값을 받는다).
    pub fn current(&self, i: usize) -> Vec<String> {
        let base: Vec<String> = self
            .res
            .rows
            .get(i)
            .cloned()
            .unwrap_or_else(|| vec![String::new(); self.res.cols.len()]);
        match self.edits.get(&i) {
            None => base,
            Some(e) => base
                .into_iter()
                .enumerate()
                .map(|(c, v)| match e.vals.get(&c) {
                    Some(Some(s)) => s.clone(),
                    Some(None) => NULL_TEXT.into(),
                    None => v,
                })
                .collect(),
        }
    }

    fn rebuild(&mut self) {
        let n = self.res.rows.len();
        let mut view: Vec<usize> = (0..n).filter(|&i| self.server || self.matches(i)).collect();
        if let (Some((c, desc)), false) = (self.sort, self.server) {
            let num = self.kind(c) == Kind::Number;
            view.sort_by(|&a, &b| {
                let (x, y) = (cell(&self.res.rows[a], c), cell(&self.res.rows[b], c));
                // NULL 은 오름·내림 모두 맨 뒤다(R14.3)
                match (is_null(x), is_null(y)) {
                    (true, true) => return std::cmp::Ordering::Equal,
                    (true, false) => return std::cmp::Ordering::Greater,
                    (false, true) => return std::cmp::Ordering::Less,
                    _ => {}
                }
                let o = if num {
                    match (x.parse::<f64>(), y.parse::<f64>()) {
                        (Ok(p), Ok(q)) => p.partial_cmp(&q).unwrap_or(std::cmp::Ordering::Equal),
                        (Ok(_), Err(_)) => std::cmp::Ordering::Less,
                        (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
                        _ => x.cmp(y),
                    }
                } else {
                    x.cmp(y)
                };
                if desc { o.reverse() } else { o }
            });
        }
        // 추가 행은 거르기·정렬과 무관하게 끝에 둔다
        view.extend((n..n + self.inserts).filter(|i| self.edits.contains_key(i)));
        self.view = view;
        self.row = self.row.min(self.view.len().saturating_sub(1));
        self.col = self.col.min(self.res.cols.len().saturating_sub(1));
    }

    /// 거르기 글(R14.4)에 행 i 가 맞는지다.
    fn matches(&self, i: usize) -> bool {
        if self.filter.is_empty() {
            return true;
        }
        let f = crate::tableview::parse_filter(&self.filter, &self.res.cols);
        let row = &self.res.rows[i];
        use crate::tableview::Op;
        let q = f.value.to_lowercase();
        match f.op {
            Op::Contains | Op::NotContains => {
                if q.is_empty() {
                    return f.col.is_none_or(|c| !is_null(cell(row, c)));
                }
                let found = (0..row.len())
                    .filter(|&c| f.col.is_none_or(|fc| fc == c))
                    .any(|c| display(self.kind(c), cell(row, c)).to_lowercase().contains(&q));
                found == (f.op == Op::Contains)
            }
            _ => {
                let c = f.col.unwrap_or(0);
                let raw = cell(row, c);
                match f.op {
                    Op::Null => return is_null(raw),
                    _ if is_null(raw) => return false,
                    Op::NotNull => return true,
                    _ => {}
                }
                let ord = if self.kind(c) == Kind::Number && crate::tableview::is_decimal(&f.value) {
                    match (raw.parse::<f64>(), f.value.parse::<f64>()) {
                        (Ok(a), Ok(b)) => a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal),
                        _ => return false,
                    }
                } else {
                    display(self.kind(c), raw).as_str().cmp(f.value.as_str())
                };
                use std::cmp::Ordering::*;
                match f.op {
                    Op::Eq => ord == Equal,
                    Op::Ne => ord != Equal,
                    Op::Gt => ord == Greater,
                    Op::Ge => ord != Less,
                    Op::Lt => ord == Less,
                    _ => ord != Greater,
                }
            }
        }
    }

    pub fn visible_rows(&self) -> usize {
        self.view.iter().filter(|&&i| i < self.res.rows.len()).count()
    }

    pub fn view_len(&self) -> usize {
        self.view.len()
    }

    /// 커서 행의 결과 행 자리다.
    pub fn cur_index(&self) -> Option<usize> {
        self.view.get(self.row).copied()
    }

    pub fn cur_value(&self) -> Option<String> {
        let i = self.cur_index()?;
        self.current(i).get(self.col).cloned()
    }

    pub fn row_values(&self) -> Option<Vec<String>> {
        Some(self.current(self.cur_index()?))
    }

    /// 화면에 보이는 칸 값이다(복호화 보기면 푼 값).
    fn shown_cell(&self, i: usize, c: usize, raw: &str) -> String {
        self.dec
            .as_ref()
            .and_then(|d| d.get(&(i, c)).cloned())
            .unwrap_or_else(|| raw.to_string())
    }

    /// 복호화할 칸(보이는 행의 암호문 꼴)이다.
    pub fn enc_cells(&self) -> Vec<(usize, usize, String)> {
        let mut out = Vec::new();
        for &i in self.view.iter().filter(|&&i| i < self.res.rows.len()) {
            for (c, v) in self.res.rows[i].iter().enumerate() {
                if idb_core::statcrypt::looks_encrypted(v) {
                    out.push((i, c, v.clone()));
                }
            }
        }
        out
    }

    pub fn set_sort(&mut self, sort: Option<(usize, bool)>) {
        self.sort = sort;
        self.rebuild();
    }

    /// 정렬을 오름 → 내림 → 없음으로 돌린다(R14.3).
    pub fn cycle_sort(&mut self, c: usize) {
        // 받은 행 안에서 다시 정렬한다. 서버로 다시 읽으면 adopt 가 서버 결과로 갈음한다
        self.server = false;
        self.sort = match self.sort {
            Some((x, false)) if x == c => Some((c, true)),
            Some((x, true)) if x == c => None,
            _ => Some((c, false)),
        };
        self.rebuild();
    }

    pub fn set_filter(&mut self, f: &str) {
        self.filter = f.to_string();
        self.rebuild();
    }

    pub fn sort_label(&self) -> String {
        match self.sort {
            Some((c, desc)) => format!("정렬 {} {}", printable(&self.res.cols[c], false), if desc { "↓" } else { "↑" }),
            None => String::new(),
        }
    }

    // ─── 서버로 다시 읽기 (R14.5·R14.7) ───

    /// 열 이름이 겹치지 않는지다. 겹치면 서브쿼리 바깥에서 열을 가리킬 수 없다. _q 열이 있으면 모든 열 찾기가 그 열을 가리킨다.
    pub fn unique_cols(&self) -> bool {
        let mut seen = std::collections::HashSet::from(["_q".to_string()]);
        self.res.cols.iter().all(|c| seen.insert(c.clone()))
    }

    /// 지금 정렬·거르기의 서버 WHERE·ORDER BY 다. NULL 은 받은 행 정렬처럼 늘 뒤다.
    pub fn server_clauses(&self) -> (String, String) {
        let f = crate::tableview::parse_filter(&self.filter, &self.res.cols);
        let like = |v: &str| idb_core::sqlsafe::quote_literal(&format!("%{}%", crate::tableview::like_escape(v)));
        let where_ = match f.col {
            Some(c) => f.sql(self.kind(c) == Kind::Number).unwrap_or_default(),
            None if f.value.is_empty() => String::new(),
            None if f.op == crate::tableview::Op::NotContains => format!("NOT (_q::text ILIKE {})", like(&f.value)),
            None => format!("_q::text ILIKE {}", like(&f.value)),
        };
        let order = match self.sort {
            Some((c, d)) => format!(
                "{} {} NULLS LAST",
                idb_core::sqlsafe::quote_ident(&self.res.cols[c]),
                if d { "DESC" } else { "ASC" }
            ),
            None => String::new(),
        };
        (where_, order)
    }

    /// 읽기 문장 base 를 감싸 지금 정렬·거르기를 서버에서 하는 SELECT 다. offset 이 있으면 그 뒤부터 읽는다(더 읽기).
    /// base 끝의 줄 주석이 닫는 괄호를 삼키지 않게 괄호 앞에 줄을 바꾼다. 둘 다 없고 offset 도 없으면 base 그대로다.
    pub fn server_sql(&self, base: &str, offset: Option<usize>) -> String {
        let (w, o) = self.server_clauses();
        if w.is_empty() && o.is_empty() && offset.is_none() {
            return base.to_string();
        }
        let mut s = format!("SELECT * FROM (\n{base}\n) AS _q");
        if !w.is_empty() {
            s.push_str(&format!(" WHERE {w}"));
        }
        if !o.is_empty() {
            s.push_str(&format!(" ORDER BY {o}"));
        }
        if let Some(n) = offset {
            s.push_str(&format!(" OFFSET {n}"));
        }
        s
    }

    /// 서버에서 그 정렬·거르기로 읽은 결과로 바꾼다. 받은 행에서 다시 거르거나 정렬하지 않는다.
    pub fn adopt(&mut self, sort: Option<(usize, bool)>, filter: &str, col: usize) {
        self.sort = sort.filter(|(c, _)| *c < self.res.cols.len());
        self.filter = filter.to_string();
        self.server = true;
        self.col = col.min(self.res.cols.len().saturating_sub(1));
        self.rebuild();
    }

    /// 더 읽은 행을 덧붙인다(R14.7). 커서·정렬·거르기·열 폭은 그대로다.
    pub fn extend(&mut self, rows: Vec<Vec<String>>, truncated: bool) {
        self.res.rows.extend(rows);
        self.res.truncated = truncated;
        self.edits.clear();
        self.inserts = 0;
        self.rebuild();
    }

    // ─── 편집 (R15) ───

    pub fn pending(&self) -> usize {
        self.edits.len()
    }

    pub fn clear_changes(&mut self) {
        self.edits.clear();
        self.inserts = 0;
        self.rebuild();
    }

    /// 결과 전체를 고칠 수 없는 까닭이다. 고칠 수 있으면 None 이다.
    pub fn edit_block(&self) -> Option<&'static str> {
        if self.res.cols.is_empty() {
            Some("편집할 수 없음: 표 결과가 아님")
        } else if self.res.origin.is_none() {
            Some("편집할 수 없음: PK 를 모르는 결과 (한 테이블의 PK 를 모두 조회해야 함)")
        } else if self.pk_cols.is_none() {
            Some("편집할 수 없음: 결과에 PK 열이 모두 있지 않음")
        } else {
            None
        }
    }

    /// 커서 칸을 고칠 수 없는 까닭이다.
    pub fn cell_block(&self) -> Option<&'static str> {
        if let Some(w) = self.edit_block() {
            return Some(w);
        }
        let Some(i) = self.cur_index() else {
            return Some("편집할 행 없음 (a 로 행 추가)");
        };
        let o = self.res.origin.as_ref()?;
        // 푼 값을 보이며 고치면 무엇을 고치는지 헷갈리고 푼 값을 그대로 저장할 위험이 있다
        if self.dec.is_some() {
            return Some("편집할 수 없음: 복호화 보기 중 (D 로 원래 값)");
        }
        if o.cols.get(self.col).is_none_or(String::is_empty) {
            return Some("편집할 수 없음: 식·계산 열");
        }
        if self.edits.get(&i).is_some_and(|e| e.deleted) {
            return Some("편집할 수 없음: 삭제 표시한 행 (u 로 되돌리기)");
        }
        None
    }

    /// 칸 값 보기를 연다. JSON 이면 나무로 연다(R32.1).
    pub fn open_value(&mut self) {
        self.screen = Screen::Value;
        self.scroll = 0;
        self.json_raw = false;
        let v = self.cur_value().unwrap_or_default();
        self.json = crate::jsontree::JsonTree::parse(&v, self.kind(self.col) == Kind::Json);
    }

    /// 값 보기가 JSON 나무인지·JSON 원문인지다(키 안내).
    pub fn value_mode(&self) -> (bool, bool) {
        (self.json.is_some() && !self.json_raw, self.json.is_some())
    }

    pub fn editable(&self) -> bool {
        self.edit_block().is_none()
    }

    fn put(&mut self, i: usize, c: usize, v: Option<String>) {
        let base = self.res.rows.get(i).map(|r| cell(r, c).to_string());
        let e = self.edits.entry(i).or_default();
        e.vals.insert(c, v.clone());
        if e.inserted {
            return;
        }
        let base = base.unwrap_or_default();
        let unchanged = (v.is_none() && is_null(&base)) || (v.as_deref() == Some(base.as_str()) && !is_null(&base));
        if unchanged {
            e.vals.remove(&c);
        }
        if e.vals.is_empty() && !e.deleted {
            self.edits.remove(&i);
        }
    }

    /// 칸 하나에 값을 둔다(enum 고르기 등).
    pub fn set_cell(&mut self, v: Option<String>) {
        if let Some(i) = self.cur_index() {
            let c = self.col;
            self.put(i, c, v);
        }
    }

    pub fn start_edit(&mut self) -> Act {
        if let Some(w) = self.cell_block() {
            return Act::Status(w.into());
        }
        let v = self.cur_value().unwrap_or_default();
        self.edit_null = is_null(&v)
            && self
                .cur_index()
                .is_some_and(|i| i < self.res.rows.len() || self.cell_set(i, self.col));
        self.input = LineInput::new(if self.edit_null { "" } else { &v });
        self.input.multiline = true;
        self.edit_at = (self.cur_index().unwrap(), self.col);
        self.screen = Screen::Edit;
        Act::None
    }

    fn cell_set(&self, i: usize, c: usize) -> bool {
        self.edits.get(&i).is_some_and(|e| e.vals.contains_key(&c))
    }

    fn commit_edit(&mut self) {
        let (i, c) = self.edit_at;
        let text = self.input.text.clone();
        let v = match self.kind(c) {
            Kind::Date if text.eq_ignore_ascii_case("now") || text.eq_ignore_ascii_case("today") => {
                Some(time_literal(self.col_type(c), text.eq_ignore_ascii_case("today")))
            }
            _ => Some(text),
        };
        let v = if self.edit_null && v.as_deref() == Some("") { None } else { v };
        self.put(i, c, v);
        self.screen = Screen::Table;
    }

    fn edit_key(&mut self, k: &str) -> Act {
        match k {
            "enter" => self.commit_edit(),
            "esc" => self.screen = Screen::Table,
            "ctrl+n" => {
                let (i, c) = self.edit_at;
                self.put(i, c, None);
                self.screen = Screen::Table;
            }
            "space" if self.kind(self.edit_at.1) == Kind::Bool => {
                // 참 → 거짓 → NULL 로 돈다(R49.1)
                let next = match (self.edit_null, self.input.text.as_str()) {
                    (true, _) => "true",
                    (false, "true" | "t") => "false",
                    (false, "false" | "f") => "",
                    _ => "true",
                };
                self.edit_null = next.is_empty();
                self.input.set(next);
            }
            _ => {
                if self.input.key(k) {
                    self.edit_null = false;
                }
            }
        }
        Act::None
    }

    pub fn add_row(&mut self, vals: BTreeMap<usize, Option<String>>) -> Act {
        if let Some(w) = self.edit_block() {
            return Act::Status(w.into());
        }
        let i = self.res.rows.len() + self.inserts;
        self.inserts += 1;
        self.edits.insert(
            i,
            RowEdit {
                vals,
                inserted: true,
                deleted: false,
            },
        );
        self.rebuild();
        self.row = self.view.iter().position(|&x| x == i).unwrap_or(0);
        Act::None
    }

    fn duplicate(&mut self) -> Act {
        let Some(row) = self.row_values() else {
            return Act::Status("복제할 행 없음".into());
        };
        let o = self.res.origin.clone();
        let Some(o) = o else {
            return Act::Status(self.edit_block().unwrap_or("").into());
        };
        // PK·식 열은 비워 DB 가 정하게 한다
        let vals = row
            .into_iter()
            .enumerate()
            .filter(|(c, _)| o.cols.get(*c).is_some_and(|n| !n.is_empty() && !o.pk.contains(n)))
            .map(|(c, v)| (c, if is_null(&v) { None } else { Some(v) }))
            .collect();
        self.add_row(vals)
    }

    fn toggle_delete(&mut self) -> Act {
        if let Some(w) = self.edit_block() {
            return Act::Status(w.into());
        }
        let Some(i) = self.cur_index() else { return Act::None };
        if self.edits.get(&i).is_some_and(|e| e.inserted) {
            self.edits.remove(&i);
            self.rebuild();
            return Act::None;
        }
        let e = self.edits.entry(i).or_default();
        e.deleted = !e.deleted;
        if !e.deleted && e.vals.is_empty() {
            self.edits.remove(&i);
        }
        Act::None
    }

    fn revert_row(&mut self) -> Act {
        if let Some(i) = self.cur_index()
            && self.edits.remove(&i).is_some()
        {
            self.rebuild();
        }
        Act::None
    }

    /// 보류 중인 바꿈을 제출 묶음으로 만든다. 지움 → 고침 → 추가 순, 같은 종류는 행 순서다.
    pub fn change_set(&self) -> Option<ChangeSet> {
        let o = self.res.origin.as_ref()?;
        let pk = self.pk_cols.as_ref()?;
        let mut cs = ChangeSet {
            table: o.table.clone(),
            pk: o.pk.clone(),
            changes: Vec::new(),
            override_system: false,
        };
        for op in [ChangeOp::Delete, ChangeOp::Update, ChangeOp::Insert] {
            for (&i, e) in &self.edits {
                let this = if e.inserted {
                    ChangeOp::Insert
                } else if e.deleted {
                    ChangeOp::Delete
                } else {
                    ChangeOp::Update
                };
                if this != op {
                    continue;
                }
                let mut ch = Change {
                    op,
                    key: BTreeMap::new(),
                    values: BTreeMap::new(),
                    old: BTreeMap::new(),
                };
                if !e.inserted {
                    let row = &self.res.rows[i];
                    for (k, name) in o.pk.iter().enumerate() {
                        ch.key.insert(name.clone(), cell(row, pk[k]).to_string());
                    }
                    // 옛값: 고침은 바꾸는 열, 지움은 PK 가 아닌 모든 테이블 열. 그새 바뀐 행을 고치지 않게 serve 가 WHERE 로 확인한다
                    for (c, name) in o.cols.iter().enumerate() {
                        let edited = e.vals.contains_key(&c);
                        if name.is_empty() || (!e.deleted && !edited) || (e.deleted && o.pk.contains(name)) {
                            continue;
                        }
                        let v = cell(row, c);
                        // 잘린 칸은 원래 값과 같을 수 없어 확인에서 뺀다
                        if idb_core::result::is_capped_cell(v) {
                            continue;
                        }
                        ch.old.insert(name.clone(), if is_null(v) { None } else { Some(v.to_string()) });
                    }
                }
                if !e.deleted {
                    for (c, v) in &e.vals {
                        if let Some(name) = o.cols.get(*c).filter(|n| !n.is_empty()) {
                            ch.values.insert(name.clone(), v.clone());
                        }
                    }
                }
                cs.changes.push(ch);
            }
        }
        Some(cs)
    }

    /// 시트 칸(TSV)을 커서부터 보이는 순서로 덮어쓰고 넘친 줄은 추가 행으로 만든다(R48.1).
    pub fn paste(&mut self, text: &str) -> Act {
        if let Some(w) = self.edit_block() {
            return Act::Status(w.into());
        }
        if self.dec.is_some() {
            return Act::Status("복호화 보기 중에는 붙여넣지 않음 (D 로 원래 값)".into());
        }
        let rows = parse_tsv(text);
        let total: usize = rows.iter().map(Vec::len).sum();
        if total > MAX_PASTE_CELLS {
            return Act::Status(format!("붙여넣기는 한 번에 {MAX_PASTE_CELLS}칸까지"));
        }
        let o = self.res.origin.clone().unwrap();
        let (mut cells, mut skipped, mut added) = (0, 0, 0);
        let start_col = self.col;
        let start_row = self.row;
        for (r, vals) in rows.iter().enumerate() {
            let vi = start_row + r;
            let i = match self.view.get(vi) {
                Some(&i) => i,
                None => {
                    self.add_row(BTreeMap::new());
                    added += 1;
                    *self.view.last().unwrap()
                }
            };
            for (k, v) in vals.iter().enumerate() {
                let c = start_col + k;
                let deleted = self.edits.get(&i).is_some_and(|e| e.deleted);
                if c >= self.res.cols.len() || o.cols.get(c).is_none_or(String::is_empty) || deleted {
                    skipped += 1;
                    continue;
                }
                let v = if v == NULL_TEXT { None } else { Some(v.clone()) };
                self.put(i, c, v);
                cells += 1;
            }
        }
        self.row = start_row;
        let mut msg = format!("붙여넣기 {cells}칸 · ctrl+s 로 미리 보기");
        if added > 0 {
            msg.push_str(&format!(" · 새 행 {added}개"));
        }
        if skipped > 0 {
            msg.push_str(&format!(" · 건너뜀 {skipped}칸"));
        }
        Act::Status(msg)
    }

    // ─── 복사 ───

    fn visible_values(&self) -> Vec<Vec<String>> {
        self.view.iter().map(|&i| self.current(i)).collect()
    }

    pub fn csv(&self) -> String {
        let mut out = self.res.cols.iter().map(|c| csv_cell(c)).collect::<Vec<_>>().join(",");
        out.push('\n');
        for row in self.visible_values() {
            out.push_str(&row.iter().map(|v| csv_cell(v)).collect::<Vec<_>>().join(","));
            out.push('\n');
        }
        out
    }

    pub fn json(&self) -> String {
        let rows: Vec<Vec<String>> = self.visible_values();
        self.json_of(&rows, &(0..self.res.cols.len()).collect::<Vec<_>>())
    }

    /// 숫자·bool·json 타입은 JSON 값, NULL 은 null, 나머지는 문자열이다(R71.1).
    fn json_value(&self, c: usize, v: &str) -> serde_json::Value {
        use serde_json::Value as J;
        if is_null(v) {
            return J::Null;
        }
        match self.kind(c) {
            Kind::Number => v
                .parse::<serde_json::Number>()
                .map(J::Number)
                .unwrap_or_else(|_| J::String(v.into())),
            Kind::Bool => match v {
                "t" | "true" => J::Bool(true),
                "f" | "false" => J::Bool(false),
                _ => J::String(v.into()),
            },
            Kind::Json => serde_json::from_str(v).unwrap_or_else(|_| J::String(v.into())),
            _ => J::String(v.into()),
        }
    }

    fn json_of(&self, rows: &[Vec<String>], cols: &[usize]) -> String {
        let out: Vec<serde_json::Value> = rows
            .iter()
            .map(|row| {
                let mut m = serde_json::Map::new();
                for &c in cols {
                    m.insert(self.res.cols[c].clone(), self.json_value(c, cell(row, c)));
                }
                serde_json::Value::Object(m)
            })
            .collect();
        serde_json::to_string_pretty(&out).unwrap_or_default() + "\n"
    }

    /// 복사 꼴 고르기(R71.1)의 글과 줄 수다. 대상은 row·select·visible·column, 꼴은 tsv·csv·json·md·insert·in 이다.
    pub fn copy_text(&self, target: &str, fmt: &str) -> Result<(String, usize), String> {
        let (r0, r1, c0, c1) = match target {
            "row" => (self.row, self.row, 0, self.res.cols.len().saturating_sub(1)),
            "select" => self.sel_rect(),
            _ => (0, self.view.len().saturating_sub(1), 0, self.res.cols.len().saturating_sub(1)),
        };
        let cols: Vec<usize> = if target == "column" { vec![self.col] } else { (c0..=c1).collect() };
        let idx: Vec<usize> = (r0..=r1).filter_map(|r| self.view.get(r).copied()).collect();
        if idx.is_empty() {
            return Err("복사할 행 없음".into());
        }
        let size: usize = idx
            .iter()
            .map(|&i| cols.iter().map(|&c| cell(&self.current(i), c).len()).sum::<usize>())
            .sum();
        if size > 10 << 20 {
            return Err("너무 커서 복사하지 않음".into());
        }
        let shown: Vec<Vec<String>> = idx.iter().map(|&i| self.current(i)).collect();
        let n = idx.len();
        let head = |f: &dyn Fn(&str) -> String| cols.iter().map(|&c| f(&self.res.cols[c])).collect::<Vec<_>>();
        let text = match fmt {
            "tsv" if target == "column" => shown.iter().map(|r| tsv_cell(cell(r, self.col))).collect::<Vec<_>>().join("\n"),
            "tsv" | "csv" => {
                let esc: fn(&str) -> String = if fmt == "tsv" { tsv_cell } else { csv_cell };
                let sep = if fmt == "tsv" { "\t" } else { "," };
                let mut lines = vec![head(&|c| esc(c)).join(sep)];
                lines.extend(
                    shown
                        .iter()
                        .map(|r| cols.iter().map(|&c| esc(cell(r, c))).collect::<Vec<_>>().join(sep)),
                );
                lines.join("\n")
            }
            "json" => self.json_of(&shown, &cols),
            "md" => {
                let md = |v: &str| v.replace('|', "\\|").replace('\n', "<br>");
                let mut lines = vec![format!("| {} |", head(&|c| md(c)).join(" | "))];
                let align: Vec<&str> = cols
                    .iter()
                    .map(|&c| if self.kind(c) == Kind::Number { "---:" } else { "---" })
                    .collect();
                lines.push(format!("| {} |", align.join(" | ")));
                for r in &shown {
                    lines.push(format!(
                        "| {} |",
                        cols.iter().map(|&c| md(cell(r, c))).collect::<Vec<_>>().join(" | ")
                    ));
                }
                lines.join("\n")
            }
            "insert" => {
                let o = self.res.origin.as_ref().ok_or("SQL INSERT: 원본 테이블을 모르는 결과")?;
                let q = idb_core::sqlsafe::quote_ident;
                let lit = |v: &str| {
                    if is_null(v) {
                        "NULL".to_string()
                    } else {
                        idb_core::sqlsafe::quote_literal(v)
                    }
                };
                let all: Vec<usize> = (0..self.res.cols.len()).collect();
                // DB 값이다. 추가 행은 뺀다
                let vals: Vec<String> = idx
                    .iter()
                    .filter(|&&i| i < self.res.rows.len())
                    .map(|&i| {
                        format!(
                            "({})",
                            all.iter().map(|&c| lit(cell(&self.res.rows[i], c))).collect::<Vec<_>>().join(", ")
                        )
                    })
                    .collect();
                if vals.is_empty() {
                    return Err("복사할 행 없음".into());
                }
                format!(
                    "INSERT INTO {} ({}) VALUES {};",
                    o.table.quoted(),
                    self.res.cols.iter().map(|c| q(c)).collect::<Vec<_>>().join(", "),
                    vals.join(", ")
                )
            }
            "in" => self.in_list().0,
            _ => return Err(format!("모르는 꼴 {fmt}")),
        };
        let lines = text.lines().count().max(1);
        let _ = n;
        Ok((text, lines))
    }

    fn sel_rect(&self) -> (usize, usize, usize, usize) {
        let (r0, c0) = self.anchor;
        (r0.min(self.row), r0.max(self.row), c0.min(self.col), c0.max(self.col))
    }

    pub fn selection_tsv(&self) -> String {
        let (r0, r1, c0, c1) = self.sel_rect();
        (r0..=r1)
            .filter_map(|r| self.view.get(r))
            .map(|&i| {
                let row = self.current(i);
                (c0..=c1).map(|c| tsv_cell(cell(&row, c))).collect::<Vec<_>>().join("\t")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 선택 칸의 `칸 N · 합 · 평균 · 최소 · 최대`(R31.1)다. 숫자 칸만 센다.
    pub fn selection_stats(&self) -> String {
        let (r0, r1, c0, c1) = self.sel_rect();
        let mut n = 0;
        let mut nums = Vec::new();
        for r in r0..=r1 {
            let Some(&i) = self.view.get(r) else { continue };
            let row = self.current(i);
            for c in c0..=c1 {
                n += 1;
                if let Ok(v) = cell(&row, c).parse::<f64>()
                    && v.is_finite()
                {
                    nums.push(v);
                }
            }
        }
        if nums.is_empty() {
            return format!("칸 {n}");
        }
        let sum: f64 = nums.iter().sum();
        let min = nums.iter().copied().fold(f64::INFINITY, f64::min);
        let max = nums.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        format!(
            "칸 {n} · 합 {} · 평균 {} · 최소 {} · 최대 {}",
            num(sum),
            num(sum / nums.len() as f64),
            num(min),
            num(max)
        )
    }

    /// 열(선택이면 선택 칸) 값을 `IN (…)` 목록으로 만든다(R35). NULL 은 빼고 같은 값은 하나다.
    pub fn in_list(&self) -> (String, usize) {
        let (r0, r1, c0, c1) = if self.screen == Screen::Select {
            self.sel_rect()
        } else {
            (0, self.view.len().saturating_sub(1), self.col, self.col)
        };
        let numeric = (c0..=c1).all(|c| self.kind(c) == Kind::Number);
        let mut seen = BTreeSet::new();
        let mut vals = Vec::new();
        for r in r0..=r1 {
            let Some(&i) = self.view.get(r) else { continue };
            let row = self.current(i);
            for c in c0..=c1 {
                let v = cell(&row, c).to_string();
                if is_null(&v) || !seen.insert(v.clone()) {
                    continue;
                }
                vals.push(if numeric { v } else { idb_core::sqlsafe::quote_literal(&v) });
            }
        }
        let n = vals.len();
        (format!("IN ({})", vals.join(", ")), n)
    }

    /// 고른 행을 INSERT·UPDATE 문으로 만든다(R15.4).
    pub fn row_sql(&self, update: bool) -> Result<String, &'static str> {
        let row = self.row_values().ok_or("행 없음")?;
        let (table, pk) = match &self.res.origin {
            Some(o) => (o.table.quoted(), o.pk.clone()),
            None => ("\"table\"".to_string(), Vec::new()),
        };
        let lit = |v: &str| {
            if is_null(v) {
                "NULL".to_string()
            } else {
                idb_core::sqlsafe::quote_literal(v)
            }
        };
        let q = idb_core::sqlsafe::quote_ident;
        if !update {
            let cols: Vec<String> = self.res.cols.iter().map(|c| q(c)).collect();
            let vals: Vec<String> = row.iter().map(|v| lit(v)).collect();
            return Ok(format!("INSERT INTO {table} ({}) VALUES ({});", cols.join(", "), vals.join(", ")));
        }
        if pk.is_empty() {
            return Err("UPDATE 복사: PK 를 모르는 결과");
        }
        let sets: Vec<String> = self
            .res
            .cols
            .iter()
            .enumerate()
            .filter(|(_, c)| !pk.contains(c))
            .map(|(i, c)| format!("{} = {}", q(c), lit(&row[i])))
            .collect();
        let wh: Vec<String> = pk
            .iter()
            .filter_map(|k| {
                self.res
                    .cols
                    .iter()
                    .position(|c| c == k)
                    .map(|i| format!("{} = {}", q(k), lit(&row[i])))
            })
            .collect();
        Ok(format!("UPDATE {table} SET {} WHERE {};", sets.join(", "), wh.join(" AND ")))
    }

    // ─── 키 ───

    fn move_by(&mut self, dr: isize, dc: isize) {
        let rows = self.view.len() as isize;
        let cols = self.res.cols.len() as isize;
        self.row = (self.row as isize + dr).clamp(0, (rows - 1).max(0)) as usize;
        self.col = (self.col as isize + dc).clamp(0, (cols - 1).max(0)) as usize;
    }

    pub fn key(&mut self, k: &str) -> Act {
        match self.screen {
            Screen::Edit => return self.edit_key(k),
            Screen::Filter => {
                match k {
                    "enter" => {
                        self.screen = Screen::Table;
                        return Act::FilterDone(self.filter.clone());
                    }
                    "esc" => {
                        self.screen = Screen::Table;
                        self.set_filter("");
                    }
                    _ => {
                        if self.input.key(k) {
                            let f = self.input.text.clone();
                            self.set_filter(&f);
                        }
                    }
                }
                return Act::None;
            }
            Screen::Record => {
                match k {
                    "esc" | "v" | "q" => self.screen = Screen::Table,
                    "down" | "j" => self.scroll = (self.scroll + 1).min(self.res.cols.len().saturating_sub(1)),
                    "up" | "k" => self.scroll = self.scroll.saturating_sub(1),
                    "y" => {
                        let row = self.row_values().unwrap_or_default();
                        let i = self.cur_index().unwrap_or(0);
                        return Act::Copy(self.shown_cell(i, self.scroll, cell(&row, self.scroll)), "값".into());
                    }
                    "Y" => return self.copy_row(),
                    _ => {}
                }
                return Act::None;
            }
            Screen::Value => {
                // JSON 나무(R32): Enter·space 접기, y 값, Y 경로, t 원문
                if let (Some(t), false) = (&mut self.json, self.json_raw) {
                    match k {
                        "esc" | "o" | "q" => self.screen = Screen::Table,
                        "down" | "j" => t.move_by(1),
                        "up" | "k" => t.move_by(-1),
                        "enter" | "space" => t.toggle(),
                        "t" => self.json_raw = true,
                        "y" => {
                            return Act::Copy(
                                t.current().map(|n| crate::jsontree::copy_value(&n.value)).unwrap_or_default(),
                                "값".into(),
                            );
                        }
                        "Y" => return Act::Copy(t.current().map(|n| n.path.clone()).unwrap_or_default(), "경로".into()),
                        _ => {}
                    }
                    return Act::None;
                }
                match k {
                    "esc" | "o" | "q" | "enter" => {
                        self.screen = Screen::Table;
                        self.scroll = 0;
                    }
                    "t" if self.json.is_some() => self.json_raw = false,
                    "down" | "j" => self.scroll += 1,
                    "up" | "k" => self.scroll = self.scroll.saturating_sub(1),
                    "y" => return Act::Copy(self.cur_value().unwrap_or_default(), "칸".into()),
                    _ => {}
                }
                return Act::None;
            }
            Screen::Select => match k {
                "esc" | "V" => {
                    self.screen = Screen::Table;
                    return Act::None;
                }
                "y" => {
                    let t = self.selection_tsv();
                    self.screen = Screen::Table;
                    return Act::Copy(t, "선택".into());
                }
                _ => {}
            },
            Screen::Table => {}
        }
        let page = self.h.saturating_sub(1).max(1) as isize;
        match k {
            "down" | "j" => self.move_by(1, 0),
            "up" | "k" => self.move_by(-1, 0),
            "left" | "h" => self.move_by(0, -1),
            "right" | "l" => self.move_by(0, 1),
            "pgdown" | "ctrl+f" => self.move_by(page, 0),
            "pgup" | "ctrl+b" if self.screen == Screen::Select || k == "pgup" => self.move_by(-page, 0),
            "g" | "home" => self.row = 0,
            "G" | "end" => self.row = self.view.len().saturating_sub(1),
            "0" => self.col = 0,
            "$" => self.col = self.res.cols.len().saturating_sub(1),
            _ if self.screen == Screen::Select => return Act::Ignored,
            "s" => {
                if self.res.cols.is_empty() {
                    return Act::Status("정렬할 열이 없음".into());
                }
                let c = self.col;
                self.cycle_sort(c);
            }
            "/" => {
                self.screen = Screen::Filter;
                self.server = false;
                self.input = LineInput::new(&self.filter);
            }
            "<" => {
                let w = self.widths.get(self.col).copied().unwrap_or(MIN_W);
                self.set_width(self.col, w.saturating_sub(WIDTH_STEP));
            }
            ">" => {
                let w = self.widths.get(self.col).copied().unwrap_or(MIN_W);
                self.set_width(self.col, w + WIDTH_STEP);
            }
            "v" if !self.res.cols.is_empty() => {
                self.screen = Screen::Record;
                self.scroll = self.col;
            }
            "o" if self.cur_index().is_some() => {
                self.open_value();
                self.scroll = 0;
            }
            "enter" | "f2" => {
                if self.cur_index().is_none() && !self.editable() {
                    return Act::None;
                }
                if (k == "enter" && self.cell_block().is_some()) || (k == "f2" && self.cell_block().is_some()) {
                    if k == "f2" || self.edit_block().is_some() {
                        if self.edit_block().is_some() && k == "enter" && self.cur_index().is_some() {
                            self.open_value();
                            return Act::None;
                        }
                        return Act::Status(self.cell_block().unwrap_or("").into());
                    }
                    self.open_value();
                    return Act::None;
                }
                if self.col_type(self.col).bytes().all(|b| b.is_ascii_digit()) && !self.col_type(self.col).is_empty() {
                    return Act::EnumPick(self.col);
                }
                return self.start_edit();
            }
            "a" => return self.add_row(BTreeMap::new()),
            "ctrl+d" => return self.duplicate(),
            "d" => return self.toggle_delete(),
            "u" => return self.revert_row(),
            "y" => {
                return match self.cur_value() {
                    Some(v) => Act::Copy(self.shown_cell(self.cur_index().unwrap_or(0), self.col, &v), "칸".into()),
                    None => Act::None,
                };
            }
            "Y" => return self.copy_row(),
            "V" if self.cur_index().is_some() => {
                self.screen = Screen::Select;
                self.anchor = (self.row, self.col);
            }
            "C" => {
                let (s, n) = self.in_list();
                return Act::Copy(s, format!("IN 목록 {n}개"));
            }
            "i" | "U" => {
                return match self.row_sql(k == "U") {
                    Ok(s) => Act::Copy(s, if k == "U" { "UPDATE 문" } else { "INSERT 문" }.into()),
                    Err(e) => Act::Status(e.into()),
                };
            }
            "e" => return Act::Export("csv"),
            "E" => return Act::Export("json"),
            _ => return Act::Ignored,
        }
        Act::None
    }

    fn copy_row(&self) -> Act {
        let i = self.cur_index().unwrap_or(0);
        match self.row_values() {
            Some(r) => Act::Copy(
                r.iter()
                    .enumerate()
                    .map(|(c, v)| tsv_cell(&self.shown_cell(i, c, v)))
                    .collect::<Vec<_>>()
                    .join("\t"),
                "행".into(),
            ),
            None => Act::None,
        }
    }

    // ─── 그리기 ───

    /// 열 자리(x 시작, 폭)다. left 열부터 화면 폭 w 에 드는 만큼이다.
    fn layout(&self, w: usize) -> Vec<(usize, usize, usize)> {
        let mut out = Vec::new();
        let mut x = 0;
        for c in self.left..self.res.cols.len() {
            let cw = self.widths[c];
            if x >= w {
                break;
            }
            out.push((c, x, cw.min(w - x)));
            x += cw + 1;
        }
        out
    }

    fn follow(&mut self, w: usize) {
        if self.row < self.top {
            self.top = self.row;
        }
        let body = self.h.saturating_sub(1).max(1);
        if self.row >= self.top + body {
            self.top = self.row + 1 - body;
        }
        if self.col < self.left {
            self.left = self.col;
        }
        while self.left < self.col {
            let used: usize = (self.left..=self.col).map(|c| self.widths[c] + 1).sum();
            if used <= w + 1 {
                break;
            }
            self.left += 1;
        }
    }

    /// 머리 줄 x 칸이 가리키는 열이다(정렬 클릭).
    pub fn col_at(&self, x: usize, w: usize) -> Option<usize> {
        self.layout(w)
            .into_iter()
            .find(|&(_, cx, cw)| x >= cx && x < cx + cw + 1)
            .map(|(c, _, _)| c)
    }

    /// 화면 (x, y) 클릭으로 칸을 고른다. y 0 은 머리 줄이라 열만 돌려준다.
    pub fn click(&mut self, x: usize, y: usize, w: usize) -> Option<usize> {
        let c = self.col_at(x, w)?;
        if y == 0 {
            return Some(c);
        }
        let r = self.top + y - 1;
        if r < self.view.len() {
            self.row = r;
            self.col = c;
        }
        None
    }

    pub fn scroll_by(&mut self, d: isize) {
        self.move_by(d, 0);
    }

    pub fn render(&mut self, area: Rect, buf: &mut Buffer, focused: bool) {
        self.h = area.height as usize;
        let w = area.width as usize;
        match self.screen {
            Screen::Record => return self.render_record(area, buf),
            Screen::Value => return self.render_value(area, buf),
            _ => {}
        }
        if self.res.cols.is_empty() {
            return;
        }
        self.follow(w);
        let cols = self.layout(w);
        let head = Style::new().fg(theme::TEXT).add_modifier(Modifier::BOLD);
        for &(c, x, cw) in &cols {
            let mut name = printable(&self.res.cols[c], false);
            if let Some((sc, desc)) = self.sort
                && sc == c
            {
                name.push_str(if desc { " ↓" } else { " ↑" });
            }
            buf.set_stringn(area.x + x as u16, area.y, &name, cw, head);
            if x + cw < w {
                buf.set_string(area.x + (x + cw) as u16, area.y, "│", Style::new().fg(theme::BORDER));
            }
        }
        let (r0, r1, c0, c1) = self.sel_rect();
        for y in 1..area.height as usize {
            let r = self.top + y - 1;
            let Some(&i) = self.view.get(r) else { break };
            let e = self.edits.get(&i);
            let vals = self.current(i);
            for &(c, x, cw) in &cols {
                let raw = cell(&vals, c);
                let mut st = Style::new().fg(theme::TEXT);
                let mut text = display(self.kind(c), raw);
                if is_null(raw) {
                    st = theme::dim();
                }
                if let Some(v) = self.dec.as_ref().and_then(|d| d.get(&(i, c))) {
                    text = printable(v, false);
                    st = st.fg(Color::Indexed(141));
                }
                if let Some(e) = e {
                    if e.inserted {
                        st = st.fg(theme::GREEN);
                        if !e.vals.contains_key(&c) {
                            text = "DEFAULT".into();
                            st = theme::dim();
                        }
                    }
                    if e.vals.contains_key(&c) && !e.inserted {
                        st = st.bg(Color::Indexed(58));
                    }
                    if e.deleted {
                        st = Style::new().fg(theme::PROD).add_modifier(Modifier::CROSSED_OUT);
                    }
                }
                let selected = self.screen == Screen::Select && r >= r0 && r <= r1 && c >= c0 && c <= c1;
                if selected {
                    st = st.bg(Color::Indexed(24));
                }
                if r == self.row && c == self.col {
                    st = if focused {
                        st.add_modifier(Modifier::REVERSED)
                    } else {
                        st.bg(theme::BORDER)
                    };
                }
                let tw = text.width();
                let px = if self.kind(c) == Kind::Number && tw < cw { x + cw - tw } else { x };
                let yy = area.y + y as u16;
                if self.screen == Screen::Edit && self.edit_at == (i, c) {
                    self.input.render(
                        buf,
                        area.x + x as u16,
                        yy,
                        cw.max(8).min(w - x),
                        Style::new().bg(Color::Indexed(237)),
                        true,
                    );
                    continue;
                }
                if r == self.row && c == self.col {
                    buf.set_stringn(area.x + x as u16, yy, " ".repeat(cw), cw, st);
                }
                buf.set_stringn(area.x + px as u16, yy, &text, cw - (px - x), st);
                if x + cw < w {
                    buf.set_string(area.x + (x + cw) as u16, yy, "│", Style::new().fg(theme::BORDER));
                }
            }
        }
    }

    fn render_record(&self, area: Rect, buf: &mut Buffer) {
        let row = self.row_values().unwrap_or_default();
        let kw = self.res.cols.iter().map(|c| printable(c, false).width()).max().unwrap_or(4).min(30);
        let top = self.scroll.saturating_sub(area.height.saturating_sub(1) as usize);
        for (y, c) in (top..self.res.cols.len()).take(area.height as usize).enumerate() {
            let st = if c == self.scroll {
                Style::new().add_modifier(Modifier::REVERSED)
            } else {
                Style::new()
            };
            let yy = area.y + y as u16;
            buf.set_stringn(area.x, yy, printable(&self.res.cols[c], false), kw, theme::help_key().patch(st));
            let v = cell(&row, c);
            let vs = if is_null(v) { theme::dim() } else { Style::new().fg(theme::TEXT) };
            buf.set_stringn(
                area.x + kw as u16 + 2,
                yy,
                display(self.kind(c), v),
                (area.width as usize).saturating_sub(kw + 2),
                vs.patch(st),
            );
        }
    }

    fn render_value(&mut self, area: Rect, buf: &mut Buffer) {
        if let (Some(t), false) = (&mut self.json, self.json_raw) {
            let h = area.height as usize;
            if t.sel < t.top {
                t.top = t.sel;
            }
            if t.sel >= t.top + h {
                t.top = t.sel + 1 - h;
            }
            for (y, l) in t.lines().iter().enumerate().skip(t.top).take(h) {
                let st = if y == t.sel {
                    Style::new().add_modifier(Modifier::REVERSED)
                } else {
                    Style::new().fg(theme::TEXT)
                };
                buf.set_stringn(area.x, area.y + (y - t.top) as u16, printable(l, false), area.width as usize, st);
            }
            return;
        }
        let v = self.cur_value().unwrap_or_default();
        let pretty = if self.kind(self.col) == Kind::Json || v.starts_with(['{', '[']) {
            serde_json::from_str::<serde_json::Value>(&v)
                .ok()
                .and_then(|j| serde_json::to_string_pretty(&j).ok())
                .unwrap_or(v)
        } else {
            v
        };
        for (y, line) in wrap(&pretty, area.width as usize)
            .iter()
            .skip(self.scroll)
            .take(area.height as usize)
            .enumerate()
        {
            buf.set_stringn(area.x, area.y + y as u16, line, area.width as usize, Style::new().fg(theme::TEXT));
        }
    }
}

/// 글을 폭 w 로 접는다(제어 문자는 보이는 글자로).
pub fn wrap(s: &str, w: usize) -> Vec<String> {
    let w = w.max(1);
    let mut out = Vec::new();
    for line in s.split('\n') {
        let line = printable(line, false);
        let mut cur = String::new();
        let mut cw = 0;
        for ch in line.chars() {
            let x = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
            if cw + x > w {
                out.push(std::mem::take(&mut cur));
                cw = 0;
            }
            cur.push(ch);
            cw += x;
        }
        out.push(cur);
    }
    out
}

fn num(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        let s = format!("{v:.4}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// `now`·`today` 를 이 컴퓨터 시각의 리터럴로 바꾼다(R49.1).
fn time_literal(typ: &str, today: bool) -> String {
    let now = chrono::Local::now();
    match (typ, today) {
        ("date", _) => now.format("%Y-%m-%d").to_string(),
        ("timestamptz", true) => now.format("%Y-%m-%d 00:00:00%:z").to_string(),
        ("timestamptz", false) => now.format("%Y-%m-%d %H:%M:%S%:z").to_string(),
        (_, true) => now.format("%Y-%m-%d 00:00:00").to_string(),
        _ => now.format("%Y-%m-%d %H:%M:%S").to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn rs(cols: &[&str], types: &[&str], rows: &[&[&str]], origin: bool) -> ResultSet {
        let mut v = serde_json::json!({
            "cols": cols, "types": types,
            "rows": rows.iter().map(|r| r.to_vec()).collect::<Vec<_>>(),
            "text": "", "truncated": false, "elapsed_ms": 1
        });
        if origin {
            v["origin"] = serde_json::json!({"table":{"schema":"public","table":"t"},"pk":["id"],"cols":cols});
        }
        serde_json::from_value(v).unwrap()
    }

    fn sample() -> Grid {
        Grid::new(rs(
            &["id", "name", "city"],
            &["int4", "text", "text"],
            &[&["1", "Alice", "Seoul"], &["2", "bob", "Busan"], &["3", "carol", "NULL"]],
            true,
        ))
    }

    #[test]
    fn 정렬은_오름_내림_없음으로_돌고_NULL_은_늘_뒤다() {
        let mut g = sample();
        g.col = 2;
        g.key("s");
        assert_eq!(g.view, [1, 0, 2]);
        g.key("s");
        assert_eq!(g.view, [0, 1, 2]);
        g.key("s");
        assert_eq!(g.sort, None);
        g.col = 0;
        g.key("s");
        g.key("s");
        assert_eq!(g.view, [2, 1, 0]);
    }

    #[test]
    fn 거르기는_열_찾기_같음_크기_NULL_을_읽는다() {
        let mut g = sample();
        g.set_filter("ali");
        assert_eq!(g.view, [0]);
        g.set_filter("!ali");
        assert_eq!(g.view, [1, 2]);
        g.set_filter("id>1");
        assert_eq!(g.view, [1, 2]);
        g.set_filter("city=null");
        assert_eq!(g.view, [2]);
        g.set_filter("name=Alice");
        assert_eq!(g.view, [0]);
        g.set_filter("city:");
        assert_eq!(g.view, [0, 1]);
    }

    #[test]
    fn 고친_칸과_지운_행과_추가_행을_제출_묶음으로_만든다() {
        let mut g = sample();
        g.col = 1;
        g.key("enter");
        assert_eq!(g.screen, Screen::Edit);
        g.input.set("Zed");
        g.key("enter");
        g.row = 1;
        g.key("d");
        g.row = 2;
        g.col = 2;
        g.key("enter");
        g.input.set("x");
        g.key("enter");
        g.key("a");
        let cs = g.change_set().unwrap();
        let ops: Vec<ChangeOp> = cs.changes.iter().map(|c| c.op).collect();
        assert_eq!(ops, [ChangeOp::Delete, ChangeOp::Update, ChangeOp::Update, ChangeOp::Insert]);
        assert_eq!(cs.changes[0].key["id"], "2");
        assert_eq!(cs.changes[0].old.get("city"), Some(&Some("Busan".to_string())));
        assert!(!cs.changes[0].old.contains_key("id"));
        assert_eq!(cs.changes[1].values["name"], Some("Zed".into()));
        assert_eq!(cs.changes[1].old["name"], Some("Alice".into()));
        assert_eq!(cs.changes[2].old["city"], None);
        assert_eq!(g.pending(), 4);
    }

    #[test]
    fn 원래_값으로_되돌린_칸은_바꿈이_아니고_NULL_은_Ctrl_N_이다() {
        let mut g = sample();
        g.col = 1;
        g.key("enter");
        g.input.set("Alice");
        g.key("enter");
        assert_eq!(g.pending(), 0);
        g.key("enter");
        g.key("ctrl+n");
        assert_eq!(g.change_set().unwrap().changes[0].values["name"], None);
    }

    #[test]
    fn PK_가_없거나_식_열이면_고치지_않고_까닭을_알린다() {
        let mut g = Grid::new(rs(&["a"], &["text"], &[&["x"]], false));
        assert_eq!(
            g.key("a"),
            Act::Status("편집할 수 없음: PK 를 모르는 결과 (한 테이블의 PK 를 모두 조회해야 함)".into())
        );
        assert_eq!(g.key("enter"), Act::None);
        assert_eq!(g.screen, Screen::Value);
    }

    #[test]
    fn 붙여넣기는_커서부터_덮고_넘친_줄은_새_행이다() {
        let mut g = sample();
        g.row = 2;
        g.col = 1;
        let act = g.paste("p\tq\nr\tNULL\n");
        assert_eq!(act, Act::Status("붙여넣기 4칸 · ctrl+s 로 미리 보기 · 새 행 1개".into()));
        let cs = g.change_set().unwrap();
        assert_eq!(cs.changes.len(), 2);
        assert_eq!(cs.changes[1].values["city"], None);
        assert_eq!(
            parse_tsv("\"a\tb\"\tc\r\n\"x\r\ny\""),
            [vec!["a\tb".to_string(), "c".into()], vec!["x\ny".into()]]
        );
    }

    #[test]
    fn 복사_꼴은_CSV_수식_막기와_IN_목록과_선택_합계다() {
        assert_eq!(csv_cell("=1+1"), "'=1+1");
        assert_eq!(csv_cell("-1.5"), "-1.5");
        assert_eq!(csv_cell("a,b"), "\"a,b\"");
        let mut g = sample();
        assert_eq!(g.in_list(), ("IN (1, 2, 3)".into(), 3));
        g.col = 2;
        assert_eq!(g.in_list(), ("IN ('Seoul', 'Busan')".into(), 2));
        g.col = 0;
        g.key("V");
        g.key("j");
        g.key("j");
        assert_eq!(g.selection_stats(), "칸 3 · 합 6 · 평균 2 · 최소 1 · 최대 3");
        assert_eq!(
            g.row_sql(true).unwrap(),
            "UPDATE \"public\".\"t\" SET \"name\" = 'carol', \"city\" = NULL WHERE \"id\" = '3';"
        );
    }
}
