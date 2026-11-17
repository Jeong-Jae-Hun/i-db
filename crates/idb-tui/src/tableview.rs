//! 테이블 보기(R16)와 거르기 식(R14.4). 페이지·WHERE·ORDER BY 는 serve 의 `table_page` 가 SQL 로 만들고,
//! 여기서는 상태와 입력 줄, 짧은 꼴 → SQL 조건 바꾸기만 한다(Go `internal/tableview`·`grid/filter.go`).

use idb_core::sqlsafe::{quote_ident, quote_literal, split_statements};
use idb_core::textutil::printable;
use idb_engine::TableRef;

use crate::input::LineInput;

pub const PAGE_SIZE: u64 = 200;
const ERR_WHERE: &str = "WHERE must be a single expression without a top-level ';'";
const EMPTY: &str = "(없음)";
const GAP: &str = "   ";
/// 한 낱말이지만 SQL 값·함수로 읽히는 말(대소문자 무시)이다
const SQL_VALUE_WORDS: &[&str] = &[
    "true",
    "false",
    "null",
    "unknown",
    "default",
    "current_date",
    "current_time",
    "current_timestamp",
    "localtime",
    "localtimestamp",
    "current_user",
    "current_role",
    "session_user",
    "user",
    "current_schema",
    "current_catalog",
];

// ─── 거르기 식 ───

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    Contains,
    NotContains,
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
    Null,
    NotNull,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Filter {
    /// None 이면 모든 열에서 찾는다
    pub col: Option<usize>,
    pub name: String,
    pub op: Op,
    pub value: String,
}

/// 처음 나오는 연산자의 자리와 글자다. '!' 는 뒤에 '=' 가 올 때만 연산자다.
fn filter_token(expr: &str) -> Option<(usize, &str)> {
    let b = expr.as_bytes();
    for i in 0..b.len() {
        match b[i] {
            b':' | b'=' => return Some((i, &expr[i..i + 1])),
            c @ (b'<' | b'>' | b'!') => {
                if b.get(i + 1) == Some(&b'=') {
                    return Some((i, &expr[i..i + 2]));
                }
                if c != b'!' {
                    return Some((i, &expr[i..i + 1]));
                }
            }
            _ => {}
        }
    }
    None
}

/// 거르기 글을 조건으로 읽는다. 열 이름은 대소문자까지 같은 열을 먼저, 없으면 대소문자를 무시해 찾고,
/// 모르는 열이면 글 전체를 모든 열 찾기로 본다.
pub fn parse_filter(expr: &str, cols: &[String]) -> Filter {
    if let Some((i, tok)) = filter_token(expr)
        && i > 0
    {
        let name = expr[..i].trim();
        let c = cols
            .iter()
            .position(|c| c == name)
            .or_else(|| cols.iter().position(|c| c.eq_ignore_ascii_case(name)));
        if let Some(c) = c {
            let mut op = match tok {
                ":" => Op::Contains,
                "=" => Op::Eq,
                "!=" => Op::Ne,
                ">" => Op::Gt,
                ">=" => Op::Ge,
                "<" => Op::Lt,
                _ => Op::Le,
            };
            let value = expr[i + tok.len()..].trim().to_string();
            if value.eq_ignore_ascii_case("null") {
                op = match op {
                    Op::Eq => Op::Null,
                    Op::Ne => Op::NotNull,
                    o => o,
                };
            }
            return Filter {
                col: Some(c),
                name: cols[c].clone(),
                op,
                value,
            };
        }
    }
    match expr.strip_prefix('!') {
        Some(rest) => Filter {
            col: None,
            name: String::new(),
            op: Op::NotContains,
            value: rest.into(),
        },
        None => Filter {
            col: None,
            name: String::new(),
            op: Op::Contains,
            value: expr.into(),
        },
    }
}

/// SQL 에 그대로 실어도 되는 십진수인지다. Inf·NaN·16진수는 막는다.
pub fn is_decimal(v: &str) -> bool {
    !v.is_empty() && v.bytes().all(|c| c.is_ascii_digit() || b"+-.eE".contains(&c)) && v.parse::<f64>().is_ok()
}

fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

impl Filter {
    /// 조건을 WHERE 식으로 바꾼다. 모든 열 찾기는 열을 몰라 None 이다.
    pub fn sql(&self, numeric: bool) -> Option<String> {
        self.col?;
        let col = quote_ident(&self.name);
        Some(match self.op {
            Op::Contains | Op::NotContains => format!("{col}::text ILIKE {}", quote_literal(&format!("%{}%", like_escape(&self.value)))),
            Op::Null => format!("{col} IS NULL"),
            Op::NotNull => format!("{col} IS NOT NULL"),
            op => {
                let sym = match op {
                    Op::Eq => "=",
                    Op::Ne => "<>",
                    Op::Gt => ">",
                    Op::Ge => ">=",
                    Op::Lt => "<",
                    _ => "<=",
                };
                if numeric && is_decimal(&self.value) {
                    format!("{col} {sym} {}", self.value)
                } else {
                    format!("{col}::text {sym} {}", quote_literal(&self.value))
                }
            }
        })
    }
}

/// WHERE 글이 문장 하나 안의 식인지 본다. 빈 글은 거르기 없음이라 통과다.
pub fn check_where(w: &str) -> Result<(), &'static str> {
    let t = w.trim();
    if t.is_empty() {
        return Ok(());
    }
    let s = split_statements(t);
    if s.len() != 1 || s[0] != t {
        return Err(ERR_WHERE);
    }
    Ok(())
}

// ─── 테이블 보기 ───

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Field {
    Where,
    Order,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Action {
    pub requery: bool,
    pub count: bool,
    pub handled: bool,
    pub status: String,
}

impl Action {
    fn handled() -> Action {
        Action {
            handled: true,
            ..Action::default()
        }
    }

    fn requery() -> Action {
        Action {
            handled: true,
            requery: true,
            ..Action::default()
        }
    }

    fn status(s: &str) -> Action {
        Action {
            handled: true,
            status: s.into(),
            ..Action::default()
        }
    }
}

#[derive(Clone, Debug)]
pub struct TableView {
    pub table: TableRef,
    pub page: u64,
    pub where_: String,
    /// 열과 내림차순
    pub orders: Vec<(String, bool)>,
    /// 열 이름·방향으로 읽지 못한 ORDER BY 글(식 등)
    pub order_raw: String,
    /// WHERE 입력의 짧은 꼴에서 바꾼 조건. esc 는 WHERE 가 이것일 때만 지운다
    short: String,
    pub cols: Vec<String>,
    pub numeric: Vec<bool>,
    pub total: i64,
    pub known: bool,
    pub exact: bool,
    pub typing: Option<Field>,
    pub input: LineInput,
    /// 조건을 주고 연 보기(R36·R75.4)다. 사용자가 WHERE 를 적용하면 풀린다
    pub preset: bool,
    comp: Vec<String>,
    comp_at: usize,
    comp_i: usize,
}

impl TableView {
    pub fn new(table: TableRef) -> TableView {
        TableView {
            table,
            page: 0,
            where_: String::new(),
            orders: Vec::new(),
            order_raw: String::new(),
            short: String::new(),
            cols: Vec::new(),
            numeric: Vec::new(),
            total: 0,
            known: false,
            exact: false,
            typing: None,
            input: LineInput::default(),
            preset: false,
            comp: Vec::new(),
            comp_at: 0,
            comp_i: 0,
        }
    }

    pub fn offset(&self) -> u64 {
        self.page * PAGE_SIZE
    }

    pub fn set_columns(&mut self, cols: &[String], numeric: Vec<bool>) {
        self.cols = cols.to_vec();
        self.numeric = numeric;
    }

    /// 추정치가 음수면 모르는 것으로 둔다(R16.2).
    pub fn set_total(&mut self, n: i64, exact: bool) {
        self.total = n;
        self.known = n >= 0;
        self.exact = exact;
    }

    /// ORDER BY 칸·입력에 보일 글이다. 소문자 단순 이름은 따옴표 없이 쓴다.
    pub fn order_text(&self) -> String {
        if !self.order_raw.is_empty() {
            return self.order_raw.clone();
        }
        self.orders
            .iter()
            .map(|(c, d)| {
                let mut s = crate::complete::quote_ident(c);
                if *d {
                    s.push_str(" DESC");
                }
                s
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// 서버에 보낼 ORDER BY 글이다. 열 정렬은 늘 따옴표로 감싼다.
    pub fn order_sql(&self) -> String {
        if !self.order_raw.is_empty() {
            return self.order_raw.clone();
        }
        self.orders
            .iter()
            .map(|(c, d)| format!("{} {}", quote_ident(c), if *d { "DESC" } else { "ASC" }))
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn column(&self, tok: &str) -> Option<String> {
        if tok.len() >= 2 && tok.starts_with('"') && tok.ends_with('"') {
            let name = tok[1..tok.len() - 1].replace("\"\"", "\"");
            return self.cols.contains(&name).then_some(name);
        }
        self.cols
            .iter()
            .find(|c| *c == tok)
            .or_else(|| self.cols.iter().find(|c| c.eq_ignore_ascii_case(tok)))
            .cloned()
    }

    fn parse_order(&self, text: &str) -> (Vec<(String, bool)>, String) {
        if text.is_empty() {
            return (Vec::new(), String::new());
        }
        let mut keys = Vec::new();
        for item in text.split(',') {
            let f: Vec<&str> = item.split_whitespace().collect();
            if f.is_empty() || f.len() > 2 {
                return (Vec::new(), text.into());
            }
            let Some(col) = self.column(f[0]) else {
                return (Vec::new(), text.into());
            };
            let desc = match f.get(1).map(|s| s.to_lowercase()) {
                None => false,
                Some(s) if s == "asc" => false,
                Some(s) if s == "desc" => true,
                _ => return (Vec::new(), text.into()),
            };
            keys.push((col, desc));
        }
        (keys, String::new())
    }

    /// WHERE 입력이 짧은 꼴(아는 열 + 한 낱말 값)이면 SQL 조건으로 바꾼다(R16.4). 아니면 SQL 그대로다.
    pub fn short_condition(&self, text: &str) -> (String, bool) {
        let f = parse_filter(text, &self.cols);
        let Some(c) = f.col else {
            return (text.into(), false);
        };
        if f.value.contains(['\'', '"', '(', ')']) || f.value.starts_with(['=', '<', '>', '!', ':', '~']) {
            return (text.into(), false);
        }
        if !matches!(f.op, Op::Contains | Op::Null | Op::NotNull) {
            let plain = !f.value.is_empty() && f.value.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.@-".contains(&b));
            let qualified = {
                let p: Vec<&str> = f.value.split('.').collect();
                p.len() == 2
                    && p.iter().all(|s| {
                        !s.is_empty() && !s.as_bytes()[0].is_ascii_digit() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                    })
            };
            let numeric = self.numeric.get(c).copied().unwrap_or(false);
            if !plain
                || self.column(&f.value).is_some()
                || qualified
                || SQL_VALUE_WORDS.contains(&f.value.to_lowercase().as_str())
                || (numeric && !is_decimal(&f.value))
            {
                return (text.into(), false);
            }
        }
        let numeric = self.numeric.get(c).copied().unwrap_or(false);
        (f.sql(numeric).unwrap_or_else(|| text.into()), true)
    }

    fn with_where(&mut self, w: &str) {
        self.where_ = w.into();
        self.page = 0;
        self.short.clear();
        if self.exact {
            self.known = false;
            self.exact = false;
        }
    }

    pub fn open(&mut self, f: Field) {
        self.typing = Some(f);
        self.comp.clear();
        self.input = LineInput::new(&match f {
            Field::Where => self.where_.clone(),
            Field::Order => self.order_text(),
        });
    }

    /// 보낼 SELECT·count 의 꼴(요약·검사용)이다. 실제 SQL 은 serve 가 같은 규칙으로 만든다.
    fn sql_shape(&self, where_: &str, order: &str) -> (String, String) {
        let rel = self.table.quoted();
        let wh = if where_.is_empty() {
            String::new()
        } else {
            format!(" WHERE {where_}")
        };
        let ord = if order.is_empty() {
            String::new()
        } else {
            format!(" ORDER BY {order}")
        };
        (
            format!("SELECT * FROM {rel}{wh}{ord} LIMIT {PAGE_SIZE} OFFSET 0"),
            format!("SELECT count(*) FROM {rel}{wh}"),
        )
    }

    /// 열 머리 클릭·s: 그 열만으로 오름 → 내림 → 없음(R14.3). add 면 다른 열 정렬을 둔다(shift+클릭).
    pub fn header_click(&mut self, col: &str, add: bool) -> Action {
        let mut next = if self.order_raw.is_empty() {
            self.orders.clone()
        } else {
            Vec::new()
        };
        match next.iter().position(|(c, _)| c == col) {
            None => next.push((col.into(), false)),
            Some(i) if !next[i].1 => next[i].1 = true,
            Some(i) => {
                next.remove(i);
            }
        }
        if !add {
            next.retain(|(c, _)| c == col);
        }
        self.orders = next;
        self.order_raw.clear();
        self.page = 0;
        Action::requery()
    }

    /// 칸 값 조건을 WHERE 에 더하거나 이미 있으면 뺀다(R38).
    pub fn toggle_where(&mut self, cond: &str) -> Action {
        let parts: Vec<String> = if self.where_.is_empty() {
            Vec::new()
        } else {
            self.where_.split(" AND ").map(String::from).collect()
        };
        let next = if parts.iter().any(|p| p == cond) {
            parts.into_iter().filter(|p| p != cond).collect::<Vec<_>>()
        } else {
            let mut p = parts;
            p.push(cond.into());
            p
        };
        let w = next.join(" AND ");
        self.with_where(&w);
        Action::requery()
    }

    /// 입력 줄이 열려 있으면 모든 키를 먹는다. 아니면 테이블 보기 키(w / O ] [ # esc)만 쓴다.
    pub fn key(&mut self, k: &str) -> Action {
        if let Some(field) = self.typing {
            if k != "tab" {
                self.comp.clear();
            }
            match k {
                "tab" => self.complete(),
                "esc" => {
                    self.typing = None;
                    self.input = LineInput::default();
                }
                "enter" => {
                    let text = self.input.text.trim().to_string();
                    if let Err(e) = check_where(&text) {
                        return Action::status(e);
                    }
                    let mut n = self.clone();
                    n.typing = None;
                    n.input = LineInput::default();
                    match field {
                        Field::Order => {
                            let (o, raw) = n.parse_order(&text);
                            n.orders = o;
                            n.order_raw = raw;
                            n.page = 0;
                        }
                        Field::Where => {
                            let (cond, short) = n.short_condition(&text);
                            n.with_where(&cond);
                            n.preset = false;
                            if short {
                                n.short = cond;
                            }
                        }
                    }
                    // 칸마다 보면 WHERE 의 $q$ 가 ORDER BY 까지 이어져 ';' 를 가릴 수 있다. 보낼 문장 전체가 하나인지 본다
                    let (sel, cnt) = n.sql_shape(&n.where_, &n.order_sql());
                    if split_statements(&sel).len() != 1 || split_statements(&cnt).len() != 1 {
                        return Action::status(ERR_WHERE);
                    }
                    *self = n;
                    return Action::requery();
                }
                _ => {
                    self.input.key(k);
                }
            }
            return Action::handled();
        }
        match k {
            "w" | "/" => {
                self.open(Field::Where);
                Action::handled()
            }
            "O" => {
                self.open(Field::Order);
                Action::handled()
            }
            "esc" => {
                if self.short.is_empty() || self.where_ != self.short {
                    return Action::default();
                }
                self.with_where("");
                Action::requery()
            }
            "]" => {
                if self.known && self.exact && ((self.page + 1) * PAGE_SIZE) as i64 >= self.total {
                    return Action::status("마지막 페이지");
                }
                self.page += 1;
                Action::requery()
            }
            "[" => {
                if self.page == 0 {
                    return Action::status("첫 페이지");
                }
                self.page -= 1;
                Action::requery()
            }
            "#" => Action {
                handled: true,
                count: true,
                ..Action::default()
            },
            _ => Action::default(),
        }
    }

    /// 커서(끝) 앞 낱말을 열 이름(ORDER BY 는 asc·desc 도)으로 채운다. 다시 Tab 이면 다음 후보로 돈다.
    fn complete(&mut self) {
        let text = self.input.text.clone();
        if self.comp.is_empty() {
            let (in_single, _) = quote_state(&text);
            if in_single || text.ends_with('"') {
                return;
            }
            let start = text
                .char_indices()
                .rev()
                .find(|(_, c)| !(c.is_alphanumeric() || *c == '_' || *c == '"'))
                .map_or(0, |(i, c)| i + c.len_utf8());
            let word = text[start..].trim_start_matches('"').to_lowercase();
            let mut cands: Vec<String> = self
                .cols
                .iter()
                .filter(|c| c.to_lowercase().starts_with(&word))
                .map(|c| crate::complete::quote_ident(c))
                .collect();
            if self.typing == Some(Field::Order) {
                cands.extend(["asc", "desc"].iter().filter(|k| k.starts_with(&word)).map(|s| s.to_string()));
            }
            if cands.is_empty() {
                return;
            }
            self.comp = cands;
            self.comp_at = start;
            self.comp_i = 0;
        } else {
            self.comp_i = (self.comp_i + 1) % self.comp.len();
        }
        let mut t = text[..self.comp_at.min(text.len())].to_string();
        t.push_str(&self.comp[self.comp_i]);
        self.input.set(&t);
    }

    /// 표 위 한 줄(R16.4): `WHERE … ORDER BY … · rows … · 스키마.테이블`.
    pub fn bar(&self, w: usize) -> String {
        let (ws, os) = self.segments(w);
        let from = self.offset() as i64;
        let to = from + PAGE_SIZE as i64;
        let g = idb_core::textutil::group_digits;
        let rows = if self.known && self.exact && from >= self.total {
            format!("rows 0 of {}", g(self.total))
        } else if self.known && self.exact {
            format!("rows {}–{} of {}", g(from + 1), g(to.min(self.total)), g(self.total))
        } else if self.known {
            format!("rows {}–{} of ~{} (추정)", g(from + 1), g(to), g(self.total))
        } else {
            format!("rows {}–{}", g(from + 1), g(to))
        };
        format!(
            "{ws}{GAP}{os} · {rows} · {}",
            printable(&format!("{}.{}", self.table.schema, self.table.table), false)
        )
    }

    pub fn segments(&self, w: usize) -> (String, String) {
        let wh = if self.where_.is_empty() {
            EMPTY.to_string()
        } else {
            printable(&self.where_, false)
        };
        let ot = self.order_text();
        let or = if ot.is_empty() { EMPTY.to_string() } else { printable(&ot, false) };
        (
            format!("WHERE {}", idb_core::textutil::truncate(&wh, (w / 2).max(8), "…")),
            format!("ORDER BY {}", idb_core::textutil::truncate(&or, (w / 4).max(8), "…")),
        )
    }

    /// 표 위 줄 x 칸 클릭이 WHERE·ORDER BY 칸이면 그 입력을 연다.
    pub fn bar_click(&mut self, x: usize, w: usize) -> bool {
        if self.typing.is_some() {
            return false;
        }
        let (ws, os) = self.segments(w);
        let ww = unicode_width::UnicodeWidthStr::width(ws.as_str());
        if x <= ww {
            self.open(Field::Where);
            true
        } else if x < ww + GAP.len() + unicode_width::UnicodeWidthStr::width(os.as_str()) {
            self.open(Field::Order);
            true
        } else {
            false
        }
    }
}

fn quote_state(s: &str) -> (bool, bool) {
    let (mut single, mut double) = (false, false);
    for c in s.chars() {
        match c {
            '\'' if !double => single = !single,
            '"' if !single => double = !double,
            _ => {}
        }
    }
    (single, double)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tv() -> TableView {
        let mut t = TableView::new(TableRef {
            schema: "public".into(),
            table: "users".into(),
        });
        t.set_columns(
            &["id".into(), "status".into(), "Note".into(), "email".into()],
            vec![true, false, false, false],
        );
        t
    }

    #[test]
    fn 짧은_꼴은_SQL_조건으로_바꾸고_SQL_로_읽히면_그대로_둔다() {
        let t = tv();
        assert_eq!(t.short_condition("status=ACTIVE"), ("\"status\"::text = 'ACTIVE'".into(), true));
        assert_eq!(t.short_condition("status:act"), ("\"status\"::text ILIKE '%act%'".into(), true));
        assert_eq!(t.short_condition("id>100"), ("\"id\" > 100".into(), true));
        assert_eq!(t.short_condition("email=a@b.com"), ("\"email\"::text = 'a@b.com'".into(), true));
        assert_eq!(t.short_condition("email=null"), ("\"email\" IS NULL".into(), true));
        for sql in ["status = 'ACTIVE'", "id=1+2", "id=abc", "status=true", "status=b.c", "id = other"] {
            assert_eq!(t.short_condition(sql), (sql.to_string(), false), "{sql}");
        }
    }

    #[test]
    fn 열_머리_클릭은_오름_내림_없음으로_돌고_ORDER_BY_칸에_보인다() {
        let mut t = tv();
        t.header_click("Note", false);
        assert_eq!(t.order_text(), "\"Note\"");
        t.header_click("Note", false);
        assert_eq!(t.order_text(), "\"Note\" DESC");
        t.header_click("id", true);
        assert_eq!(t.order_sql(), "\"Note\" DESC, \"id\" ASC");
        t.header_click("id", false);
        assert_eq!(t.order_text(), "id DESC");
    }

    #[test]
    fn WHERE_와_ORDER_BY_를_합쳐_문장이_둘이면_거부한다() {
        let mut t = tv();
        t.key("w");
        t.input.set("name = $q$");
        assert!(t.key("enter").requery);
        t.key("O");
        t.input.set("$q$; drop table t; --");
        assert_eq!(t.key("enter").status, ERR_WHERE);
        t.key("esc");
        t.key("w");
        t.input.set("a; b");
        assert_eq!(t.key("enter").status, ERR_WHERE);
    }

    #[test]
    fn 페이지는_정확한_개수에서_멈추고_esc_는_짧은_꼴만_지운다() {
        let mut t = tv();
        assert_eq!(t.key("[").status, "첫 페이지");
        t.set_total(150, true);
        assert_eq!(t.key("]").status, "마지막 페이지");
        t.key("w");
        t.input.set("status=A");
        t.key("enter");
        assert!(t.key("esc").requery);
        assert_eq!(t.where_, "");
        // WHERE 를 바꾸면 이전 정확한 개수는 맞지 않아 버린다
        assert_eq!(t.bar(80), "WHERE (없음)   ORDER BY (없음) · rows 1–200 · public.users");
        t.set_total(150, true);
        assert!(t.bar(80).contains("rows 1–150 of 150"));
    }

    #[test]
    fn Tab_은_열_이름을_채우고_다시_누르면_다음_후보다() {
        let mut t = tv();
        t.key("O");
        t.input.set("n");
        t.key("tab");
        assert_eq!(t.input.text, "\"Note\"");
        t.key("end");
        t.input.set("id d");
        t.key("tab");
        assert_eq!(t.input.text, "id desc");
    }
}
