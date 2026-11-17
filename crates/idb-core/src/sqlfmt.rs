//! PostgreSQL 문장을 읽기 좋게 줄 맞춤한다(R21.2). 공백과 키워드 대소문자만 바꾸고 토큰 순서는 지킨다.
//! Go `internal/sqlfmt` 와 같다. 토큰화는 params 도 같이 쓴다.

// ─── 토큰 ───

/// 토큰 종류다. 연산자·괄호·쉼표 같은 기호는 모두 한 바이트짜리 Punct 다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Word,
    Number,
    String,
    Quoted,
    Param,
    LineComment,
    BlockComment,
    Punct,
}

/// `sql[start..end]` 조각이다. 토큰 사이 빈틈은 언제나 공백뿐이다. 경계는 늘 ASCII 바이트 자리다.
#[derive(Debug)]
pub struct Token {
    pub kind: Kind,
    pub start: usize,
    pub end: usize,
}

const SPACE_BYTES: &[u8] = b" \t\n\r\x0c\x0b";
/// 문자열 바로 앞에 붙는 PostgreSQL 접두사(E·B·X·N). E 만 역슬래시 이스케이프를 쓴다
const STRING_PREFIXES: &[u8] = b"eEbBxXnN";

/// sql 을 공백 밖의 토큰으로 나눈다. 닫히지 않은 문자열·주석은 끝까지 한 토큰이다.
pub fn tokenize(sql: &str) -> Vec<Token> {
    let s = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if SPACE_BYTES.contains(&c) {
            i += 1;
            continue;
        }
        let start = i;
        let mut kind = Kind::Punct;
        if s[i..].starts_with(b"--") {
            i += 2;
            kind = Kind::LineComment;
            while i < s.len() && s[i] != b'\n' {
                i += 1;
            }
        } else if s[i..].starts_with(b"/*") {
            // PostgreSQL 블록 주석은 중첩된다
            let mut depth = 0;
            while i < s.len() {
                if s[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if s[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            kind = Kind::BlockComment;
        } else if c == b'\'' {
            i += quoted_len(&s[i..], false);
            kind = Kind::String;
        } else if c == b'"' {
            i += quoted_len(&s[i..], false);
            kind = Kind::Quoted;
        } else if STRING_PREFIXES.contains(&c) && s.get(i + 1) == Some(&b'\'') {
            i += 1 + quoted_len(&s[i + 1..], c == b'e' || c == b'E');
            kind = Kind::String;
        } else if c == b'$' && s.get(i + 1).is_some_and(u8::is_ascii_digit) {
            i += 1;
            while i < s.len() && s[i].is_ascii_digit() {
                i += 1;
            }
            kind = Kind::Param;
        } else if c == b'$' {
            // $태그$ ... $태그$ 인용. 태그 모양이 아니면 '$' 한 글자짜리 기호다
            let mut j = i + 1;
            while j < s.len() && (s[j] == b'_' || s[j].is_ascii_alphabetic() || j > i + 1 && s[j].is_ascii_digit()) {
                j += 1;
            }
            if j >= s.len() || s[j] != b'$' {
                i += 1;
            } else {
                let tag = &s[i..=j];
                kind = Kind::String;
                i = match crate::sqlsafe::find(&s[j + 1..], tag) {
                    Some(end) => j + 1 + end + tag.len(),
                    None => s.len(),
                };
            }
        } else if c.is_ascii_digit() || c == b'.' && s.get(i + 1).is_some_and(u8::is_ascii_digit) {
            i += 1;
            while i < s.len() {
                if (s[i] == b'e' || s[i] == b'E') && matches!(s.get(i + 1), Some(b'+' | b'-')) {
                    i += 2;
                    continue;
                }
                if !is_ident_byte(s[i]) && s[i] != b'.' {
                    break;
                }
                i += 1;
            }
            kind = Kind::Number;
        } else if is_ident_start(c) {
            // 0x80 이상 바이트는 모두 식별자 글자로 본다(PostgreSQL 과 같다). 덕분에 UTF-8 글자가 쪼개지지 않는다
            i += 1;
            while i < s.len() && is_ident_byte(s[i]) {
                i += 1;
            }
            kind = Kind::Word;
        } else {
            i += 1;
        }
        out.push(Token { kind, start, end: i });
    }
    out
}

/// s[0] 을 여는 따옴표로 보고 닫는 따옴표까지의 길이다. 같은 따옴표 두 개는 이스케이프다.
fn quoted_len(s: &[u8], backslash: bool) -> usize {
    let mark = s[0];
    let mut i = 1;
    while i < s.len() {
        // 이스케이프한 백슬래시와 같은 따옴표 두 개는 다음 바이트까지 건너뛴다
        if backslash && s[i] == b'\\' || s[i] == mark && s.get(i + 1) == Some(&mark) {
            i += 1;
        } else if s[i] == mark {
            return i + 1;
        }
        i += 1;
    }
    s.len()
}

fn is_ident_start(c: u8) -> bool {
    c == b'_' || c.is_ascii_alphabetic() || c >= 0x80
}

fn is_ident_byte(c: u8) -> bool {
    is_ident_start(c) || c.is_ascii_digit() || c == b'$'
}

// ─── 정렬 ───

const INDENT_UNIT: &str = "    ";

/// 대문자로 바꿀 낱말이다. 열 이름으로 흔한 낱말(name·type·key·value·first 등)은 넣지 않는다
const KEYWORDS: &[&str] = &[
    "all", "and", "any", "array", "as", "asc", "between", "by", "case", "cast", "check", "collate", "conflict", "constraint", "create",
    "cross", "current_date", "current_time", "current_timestamp", "current_user", "default", "delete", "desc", "distinct", "do", "drop",
    "else", "end", "except", "exists", "false", "fetch", "filter", "for", "foreign", "from", "full", "group", "having", "ilike", "in",
    "inner", "insert", "intersect", "interval", "into", "is", "join", "lateral", "left", "like", "limit", "natural", "not", "nothing",
    "null", "nulls", "offset", "on", "only", "or", "order", "outer", "over", "partition", "primary", "recursive", "references",
    "returning", "right", "select", "set", "similar", "some", "table", "then", "true", "union", "unique", "update", "using", "values",
    "when", "where", "window", "with",
];

/// 이 낱말 뒤의 JOIN 은 이미 줄을 바꾼 조인 구의 일부다
const JOIN_MODS: &[&str] = &["left", "right", "full", "inner", "cross", "natural", "outer"];

/// 이 절 안의 최상위 쉼표 뒤에서 줄을 바꾼다
const COMMA_CLAUSES: &[&str] = &["select", "values", "set"];

const CLAUSE_WORDS: &[&str] = &[
    "select", "from", "where", "group", "having", "order", "limit", "offset", "returning", "values", "set", "insert", "update", "delete",
    "union", "intersect", "except",
];

pub fn is_keyword(w: &str) -> bool {
    KEYWORDS.contains(&w)
}

/// 괄호 한 겹이다. stmt 는 최상위나 서브쿼리 괄호로 절마다 줄을 바꾸는 틀이다.
#[derive(Clone, Default)]
struct Frame {
    stmt: bool,
    started: bool,
    /// 이 틀의 절이 놓일 들여쓰기 단계
    indent: usize,
    /// 닫는 괄호가 놓일 들여쓰기 단계
    open: usize,
    clause: String,
    /// BETWEEN 뒤 첫 AND 는 조건 연결이 아니다
    between: bool,
    /// CASE 안에서는 AND·OR·쉼표로 줄을 바꾸지 않는다
    cases: usize,
}

fn stmt_frame() -> Frame {
    Frame {
        stmt: true,
        ..Frame::default()
    }
}

/// 절 앞에서 줄을 바꾸고 키워드를 대문자로 쓴다. 문자열·따옴표 이름·주석은 그대로 두고,
/// 줄바꿈 규칙이 없는 자리의 공백은 원래 공백이 있었으면 한 칸, 없었으면 붙여 쓴다.
/// 줄바꿈은 토큰만 보고 정하므로 결과를 다시 넣어도 같다(멱등).
pub fn format(sql: &str) -> String {
    let toks = tokenize(sql);
    let text = |t: &Token| &sql[t.start..t.end];
    let mut b = String::new();
    let mut stack = vec![stmt_frame()];
    let mut line = 0usize;
    let mut prev = String::new();
    let (mut after_semi, mut opened, mut break_comma) = (false, false, false);
    for (i, t) in toks.iter().enumerate() {
        let w = norm(sql, &toks, i);
        let next = next_sig(sql, &toks, i);
        let depth = stack.len();
        let f = stack.last_mut().expect("stack never empty");
        let mut brk = false;
        if f.stmt && f.started {
            brk = match w.as_str() {
                "select" | "where" | "having" | "limit" | "offset" | "returning" | "union" | "intersect" | "except" | "insert"
                | "inner" | "cross" | "natural" => true,
                // DELETE FROM, IS DISTINCT FROM 은 한 구다
                "from" => prev != "delete" && prev != "distinct",
                "group" | "order" => next == "by",
                // ON CONFLICT DO UPDATE, FOR UPDATE, ON DELETE CASCADE 는 절이 아니다
                "update" | "delete" => prev != "do" && prev != "for" && prev != "on",
                "values" => prev != "default",
                "set" => f.clause == "update",
                "left" | "right" | "full" => next == "join" || next == "outer",
                "join" => !JOIN_MODS.contains(&prev.as_str()),
                _ => false,
            };
        }
        let cont = f.stmt
            && f.cases == 0
            && (break_comma || ((w == "and" && !f.between) || w == "or") && (f.clause == "where" || f.clause == "having"));

        let mut sep = "";
        let mut nl: Option<usize> = None;
        if i == 0 {
        } else if after_semi {
            sep = "\n\n";
            line = 0;
        } else if w == ")" && depth > 1 && f.stmt {
            nl = Some(f.open);
        } else if opened || brk {
            nl = Some(f.indent);
        } else if cont {
            nl = Some(f.indent + 1);
        } else if toks[i - 1].kind == Kind::LineComment {
            nl = Some(line);
        } else if w == ";" {
        } else if toks[i - 1].end < t.start {
            sep = " ";
        }
        match nl {
            Some(n) => {
                b.push('\n');
                b.push_str(&INDENT_UNIT.repeat(n));
                line = n;
            }
            None => b.push_str(sep),
        }
        if t.kind == Kind::Word && is_keyword(&w) {
            b.push_str(&text(t).to_ascii_uppercase());
        } else {
            b.push_str(text(t));
        }

        after_semi = false;
        opened = false;
        break_comma = false;
        if matches!(t.kind, Kind::LineComment | Kind::BlockComment) {
            continue;
        }
        f.started = true;
        if f.stmt && CLAUSE_WORDS.contains(&w.as_str()) && (brk || w != "set") {
            f.clause = w.clone();
        }
        match w.as_str() {
            "between" => f.between = true,
            "and" => f.between = false,
            "case" => f.cases += 1,
            "end" => f.cases = f.cases.saturating_sub(1),
            "," => break_comma = f.stmt && f.cases == 0 && COMMA_CLAUSES.contains(&f.clause.as_str()),
            "(" => {
                if next == "select" || next == "with" || next == "values" {
                    stack.push(Frame {
                        stmt: true,
                        indent: line + 1,
                        open: line,
                        ..Frame::default()
                    });
                    opened = true;
                } else {
                    stack.push(Frame::default());
                }
            }
            ")" => {
                if stack.len() > 1 {
                    stack.pop();
                }
            }
            ";" => {
                // split_statements 처럼 괄호와 상관없이 ';' 에서 문장을 끊는다
                stack = vec![stmt_frame()];
                line = 0;
                after_semi = true;
            }
            _ => {}
        }
        prev = w;
    }
    b
}

/// 규칙 비교용 낱말이다. 기호는 그대로, 낱말은 소문자로, t.col 처럼 점에 붙은 이름과 그 밖의 토큰은 빈 글이다.
fn norm(sql: &str, toks: &[Token], i: usize) -> String {
    let t = &toks[i];
    let text = |t: &Token| &sql[t.start..t.end];
    match t.kind {
        Kind::Punct => text(t).to_string(),
        Kind::Word => {
            if i > 0 && text(&toks[i - 1]) == "." || toks.get(i + 1).is_some_and(|n| text(n) == ".") {
                return String::new();
            }
            text(t).to_ascii_lowercase()
        }
        _ => String::new(),
    }
}

/// i 다음의 주석이 아닌 토큰의 norm 이다.
fn next_sig(sql: &str, toks: &[Token], i: usize) -> String {
    (i + 1..toks.len())
        .find(|&j| !matches!(toks[j].kind, Kind::LineComment | Kind::BlockComment))
        .map(|j| norm(sql, toks, j))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 절마다_줄을_바꾸고_키워드를_대문자로_쓴다() {
        let cases = [
            (
                "select id, name from users u left join orders o on o.user_id = u.id where u.id = 1 and o.total > 10 or u.vip order by u.id limit 5",
                "SELECT id,\n    name\nFROM users u\nLEFT JOIN orders o ON o.user_id = u.id\nWHERE u.id = 1\n    AND o.total > 10\n    OR u.vip\nORDER BY u.id\nLIMIT 5",
            ),
            (
                "select * from t where id in (select id from u where x between 1 and 2)",
                "SELECT *\nFROM t\nWHERE id IN (\n    SELECT id\n    FROM u\n    WHERE x BETWEEN 1 AND 2\n)",
            ),
            (
                "insert into t (a, b) values (1, 2), (3, 4) returning id",
                "INSERT INTO t (a, b)\nVALUES (1, 2),\n    (3, 4)\nRETURNING id",
            ),
            ("update t set a = 1, b = 2 where id = $1", "UPDATE t\nSET a = 1,\n    b = 2\nWHERE id = $1"),
            ("delete from t where a is distinct from b", "DELETE FROM t\nWHERE a IS DISTINCT FROM b"),
            (
                "select count(*), extract(year from d) from t group by 2 having count(*) > 1 union all select 1, 2",
                "SELECT count(*),\n    extract(year FROM d)\nFROM t\nGROUP BY 2\nHAVING count(*) > 1\nUNION ALL\nSELECT 1,\n    2",
            ),
            ("with a as (select 1) select * from a", "WITH a AS (\n    SELECT 1\n)\nSELECT *\nFROM a"),
            (
                "insert into t values (1) on conflict (id) do update set a = 1",
                "INSERT INTO t\nVALUES (1) ON CONFLICT (id) DO UPDATE\nSET a = 1",
            ),
        ];
        for (input, want) in cases {
            assert_eq!(format(input), want, "{input}");
        }
    }

    #[test]
    fn 문자열_따옴표_이름_주석은_그대로_둔다() {
        let input = "select 'from x', E'a\\'b', $$ select $$, $f$ where $f$, \"Select\", t.from /* a /* b */ where */ from t -- where\nwhere x=1";
        let want = "SELECT 'from x',\n    E'a\\'b',\n    $$ select $$,\n    $f$ where $f$,\n    \"Select\",\n    t.from /* a /* b */ where */\nFROM t -- where\nWHERE x=1";
        assert_eq!(format(input), want);
    }

    #[test]
    fn 줄_주석_뒤에는_항상_줄을_바꾼다() {
        assert_eq!(
            format("select 1 -- c\n;select a, -- n\n b"),
            "SELECT 1 -- c\n;\n\nSELECT a,\n    -- n\n    b"
        );
    }

    #[test]
    fn 여러_문장은_빈_줄로_나누고_끝_세미콜론은_지킨다() {
        assert_eq!(format("select 1;select 2;  \n"), "SELECT 1;\n\nSELECT 2;");
        assert_eq!(format("  select 1 ; select 2"), "SELECT 1;\n\nSELECT 2");
    }

    const SEEDS: &[&str] = &[
        "",
        "select 1",
        "select a,b from t where a=1 and (b=2 or c=3) order by 1",
        "select case when a and b then 1 else 2 end, x from t where a between 1 and 2 and b",
        "select * from (select 1) s join (values (1),(2)) v(x) on true",
        "select 'x''y', e'\\n', b'01', $a$ $ $a$, $$, \"q\"\"q\" from t",
        "/* unterminated",
        "select 'unterminated",
        "-- only comment",
        "select 1 -- c",
        "select 1;;select 2",
        "select (((select 1)))",
        ")) select (",
        "select a::int, x[1:2], $1, :id from t",
        "select 한글, 1from t",
        "update t set a=1 from u where t.id=u.id returning *",
        "select 1 union select 2 intersect select 3 except select 4",
        "natural left join x cross join y full outer join z",
    ];

    /// Go 퍼즈 시드다. 멱등이고 토큰(키워드 대소문자 빼고)을 바꾸지 않는다.
    #[test]
    fn 결과를_다시_넣어도_같고_토큰을_바꾸지_않는다() {
        for input in SEEDS {
            let once = format(input);
            assert_eq!(format(&once), once, "not idempotent for {input:?}");
            let (a, b) = (tokenize(input), tokenize(&once));
            assert_eq!(a.len(), b.len(), "{input:?} -> {once:?}");
            for (x, y) in a.iter().zip(&b) {
                let (tx, ty) = (&input[x.start..x.end], &once[y.start..y.end]);
                assert_eq!(x.kind, y.kind);
                if x.kind == Kind::Word && is_keyword(&tx.to_ascii_lowercase()) {
                    assert!(tx.eq_ignore_ascii_case(ty));
                } else {
                    assert_eq!(tx, ty);
                }
            }
        }
    }
}
