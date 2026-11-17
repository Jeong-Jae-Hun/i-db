//! SQL·Redis 문장을 접속 없이 읽어 나누고 읽기 전용·위험 여부를 가른다.
//! Go `internal/db` 의 `sqltext.go`·`danger.go`·`readonly.go`·`explain.go` 에서 DB 가 필요 없는 쪽과 같은 결과를 낸다.
//! 위치는 모두 바이트 단위다. 경계는 늘 ASCII 바이트라 잘라도 UTF-8 글자가 쪼개지지 않는다.

mod danger;
mod explain;
mod readonly;

pub(crate) use crate::textutil::{go_lower, go_upper};
pub use danger::{danger_of, danger_of_redis, hidden_write_of};
pub use explain::{ExplainOpt, explain_sql, modify_table_rows};
pub use readonly::{ReadOnlyError, check_read_only_sql};

// ─── Redis 읽기 명령 ───

const REDIS_READ_CMDS: &[&str] = &[
    "GET",
    "MGET",
    "STRLEN",
    "GETRANGE",
    "EXISTS",
    "TYPE",
    "TTL",
    "PTTL",
    "KEYS",
    "SCAN",
    "HGET",
    "HGETALL",
    "HMGET",
    "HKEYS",
    "HVALS",
    "HLEN",
    "HSCAN",
    "LRANGE",
    "LLEN",
    "LINDEX",
    "SMEMBERS",
    "SCARD",
    "SISMEMBER",
    "SSCAN",
    "ZRANGE",
    "ZRANGEBYSCORE",
    "ZREVRANGE",
    "ZCARD",
    "ZSCORE",
    "ZRANK",
    "ZSCAN",
    "DBSIZE",
    "INFO",
    "PING",
    "XRANGE",
    "XLEN",
    "PFCOUNT",
    "GETBIT",
    "BITCOUNT",
    "TIME",
];

/// 관리 명령은 하위 명령까지 봐야 한다. SLOWLOG RESET·CLIENT KILL·MEMORY PURGE 는 쓰기로 본다.
fn redis_read_sub_cmds(cmd: &str) -> Option<&'static [&'static str]> {
    Some(match cmd {
        "CLIENT" => &["LIST", "INFO", "GETNAME", "ID"],
        "SLOWLOG" => &["GET", "LEN"],
        "MEMORY" => &["USAGE", "STATS", "DOCTOR"],
        "OBJECT" => &["ENCODING", "FREQ", "IDLETIME", "REFCOUNT"],
        "XINFO" => &["STREAM", "GROUPS", "CONSUMERS"],
        _ => return None,
    })
}

/// KEYS 를 막는 글이다. KEYS 는 키 전체를 한 번에 훑어 그동안 서버가 다른 요청을 받지 못한다(R58.2).
/// CLI 와 엔진이 접속하기 전에 같은 글로 거절한다. KEYS 가 아니면 None 이다.
pub fn keys_refusal(args: &[String]) -> Option<String> {
    if !args.first().is_some_and(|a| a.eq_ignore_ascii_case("KEYS")) {
        return None;
    }
    let pat = args.get(1).map_or("…", String::as_str);
    Some(format!(
        "KEYS 는 CLI 에서 하지 않음 (서버를 멈춤): SCAN 0 MATCH {pat} COUNT 1000 을 쓰세요"
    ))
}

pub fn is_read_only_redis<S: AsRef<str>>(args: &[S]) -> bool {
    let Some(first) = args.first() else {
        return false;
    };
    let cmd = go_upper(first.as_ref());
    if let Some(sub) = redis_read_sub_cmds(&cmd) {
        return args.get(1).is_some_and(|a| sub.contains(&go_upper(a.as_ref()).as_str()));
    }
    REDIS_READ_CMDS.contains(&cmd.as_str())
}

// ─── 문장 나누기 ───

const SPACE_BYTES: &[u8] = b" \t\n\r\x0c\x0b";

/// `text[start..end]` 조각이다. end 는 경계 ';' 의 위치이거나 `text.len()` 이다.
#[derive(Clone, Copy, Default)]
struct StmtSpan {
    start: usize,
    end: usize,
    /// 공백·주석 말고 내용이 있다
    code: bool,
}

/// offset(바이트)이 속한 SQL 문장이다. 경계는 따옴표·$태그$·주석 밖의 ';' 이고 앞뒤 공백과 끝 ';' 는 뗀다.
/// 틀려도 실행은 extended protocol 이 여러 문장을 거부해 막는다.
pub fn statement_at(text: &str, offset: usize) -> &str {
    let spans = scan_statements(text);
    let s = spans.iter().find(|sp| offset <= sp.end).unwrap_or(&spans[spans.len() - 1]);
    text[s.start..s.end].trim()
}

/// text 를 `statement_at` 과 같은 경계로 나눈다. 공백·주석뿐인 조각은 뺀다.
pub fn split_statements(text: &str) -> Vec<&str> {
    scan_statements(text)
        .into_iter()
        .filter(|s| s.code)
        .map(|s| text[s.start..s.end].trim())
        .filter(|s| !s.is_empty())
        .collect()
}

/// 따옴표·$태그$·주석 밖의 ';' 로 text 를 조각낸다. 조각은 하나 이상이다.
fn scan_statements(text: &str) -> Vec<StmtSpan> {
    let m = mask_bytes(text.as_bytes()).0;
    let mut spans = Vec::new();
    let mut cur = StmtSpan::default();
    for (i, &c) in m.iter().enumerate() {
        if c == b';' {
            cur.end = i;
            spans.push(cur);
            cur = StmtSpan {
                start: i + 1,
                ..Default::default()
            };
        } else if !SPACE_BYTES.contains(&c) {
            cur.code = true;
        }
    }
    cur.end = m.len();
    spans.push(cur);
    spans
}

// ─── 가리기 ───

/// text 와 길이가 같은 사본에서 주석과 따옴표·$태그$ 안 글자를 공백으로 바꾼다.
/// 따옴표와 태그는 남겨 내용이 있었다는 표시로 쓴다. 남은 글자에서 ';'·괄호·키워드를 찾으면
/// 문자열·주석·"따옴표 이름" 안의 것을 잘못 세지 않는다.
pub fn mask_sql(text: &str) -> String {
    let m = mask_bytes(text.as_bytes()).0;
    // 공백으로 채우는 구간은 늘 ASCII 바이트 사이라 UTF-8 이 깨지지 않는다. 혹시 깨져도 패닉 대신 바꿔 적는다
    String::from_utf8(m).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// text 에 따옴표·$태그$ 밖의 주석(--·/* */)이 있는지다. `mask_sql` 과 같은 어휘 규칙으로 읽는다.
pub fn has_comment(text: &str) -> bool {
    mask_bytes(text.as_bytes()).1
}

/// `mask_sql` 의 바이트 사본과 주석이 있었는지다. 갈래마다 i 를 마지막으로 읽은 자리에 두고 끝에서 한 칸 넘긴다.
pub(crate) fn mask_bytes(t: &[u8]) -> (Vec<u8>, bool) {
    let mut comment = false;
    let mut b = t.to_vec();
    let mut i = 0;
    while i < t.len() {
        let start = i;
        if t[i] == b'\'' || t[i] == b'"' {
            let q = t[i];
            // E'...' 안에서는 백슬래시가 다음 글자를 이스케이프한다
            let escapes = q == b'\'' && i > 0 && (t[i - 1] == b'E' || t[i - 1] == b'e') && (i < 2 || !is_ident_byte(t[i - 2]));
            i += 1;
            while i < t.len() {
                if escapes && t[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if t[i] != q {
                    i += 1;
                    continue;
                }
                if i + 1 < t.len() && t[i + 1] == q {
                    // '' 이스케이프
                    i += 2;
                    continue;
                }
                break;
            }
            fill_spaces(&mut b, start + 1, i);
        } else if t[i..].starts_with(b"--") {
            comment = true;
            while i < t.len() && t[i] != b'\n' {
                i += 1;
            }
            fill_spaces(&mut b, start, i);
        } else if t[i..].starts_with(b"/*") {
            // 블록 주석은 중첩된다
            comment = true;
            let mut depth = 0;
            while i < t.len() {
                if t[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 1;
                } else if t[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 1;
                    if depth == 0 {
                        break;
                    }
                }
                i += 1;
            }
            fill_spaces(&mut b, start, i + 1);
        } else if t[i] == b'$'
            && (i == 0 || !is_ident_byte(t[i - 1]))
            && let Some(tag) = dollar_tag(&t[i..])
        {
            let tl = tag.len();
            if let Some(j) = find(&t[i + tl..], tag) {
                i += 2 * tl + j - 1;
                fill_spaces(&mut b, start + tl, i + 1 - tl);
            } else {
                i = t.len();
                fill_spaces(&mut b, start + tl, i);
            }
        }
        i += 1;
    }
    (b, comment)
}

/// `b[from..to]` 를 공백으로 채운다. 범위가 b 를 넘으면 끝에서 멈춘다.
fn fill_spaces(b: &mut [u8], from: usize, to: usize) {
    let to = to.min(b.len());
    if from < to {
        b[from..to].fill(b' ');
    }
}

/// s 앞의 `$태그$`·`$$` 다. Go 정규식 `^\$[A-Za-z_][A-Za-z0-9_]*\$|^\$\$` 와 같다.
pub(crate) fn dollar_tag(s: &[u8]) -> Option<&[u8]> {
    if s.first() != Some(&b'$') {
        return None;
    }
    match s.get(1) {
        Some(b'$') => return Some(&s[..2]),
        Some(c) if c.is_ascii_alphabetic() || *c == b'_' => {}
        _ => return None,
    }
    let j = 2 + s[2..].iter().take_while(|c| c.is_ascii_alphanumeric() || **c == b'_').count();
    (s.get(j) == Some(&b'$')).then(|| &s[..=j])
}

/// haystack 에서 needle 이 처음 나오는 자리다. needle 은 비어 있지 않다.
pub(crate) fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// 앞뒤 공백과 끝의 ';'(여러 개, 사이 공백 포함)를 뗀다.
pub fn trim_statement(s: &str) -> &str {
    s.trim().trim_end_matches([';', ' ', '\t', '\r', '\n', '\x0c', '\x0b'])
}

/// b 가 따옴표 없는 SQL 식별자에 들 수 있는 바이트인지다($ 와 UTF-8 바이트 포함).
pub fn is_ident_byte(b: u8) -> bool {
    b == b'_' || b == b'$' || b.is_ascii_alphanumeric() || b >= 0x80
}

/// 식별자를 늘 큰따옴표로 감싼다. 예약어·대문자 판정 없이도 항상 맞다.
pub fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// PostgreSQL quote_literal 과 같다. 백슬래시가 있으면 standard_conforming_strings 설정과
/// 무관하게 같은 값이 되도록 E'' 로 쓴다.
pub fn quote_literal(s: &str) -> String {
    let q = format!("'{}'", s.replace('\'', "''"));
    if s.contains('\\') {
        return format!("E{}", q.replace('\\', "\\\\"));
    }
    q
}

// ─── 줄·인자 ───

/// offset 이 있는 줄이다.
pub fn line_at(text: &str, offset: usize) -> &str {
    let t = text.as_bytes();
    let offset = offset.min(t.len());
    let start = t[..offset].iter().rposition(|&c| c == b'\n').map_or(0, |p| p + 1);
    match t[offset..].iter().position(|&c| c == b'\n') {
        Some(end) => text[start..offset + end].trim(),
        None => text[start..].trim(),
    }
}

/// Redis 명령 줄을 인자로 나눈다. 따옴표 안 공백은 유지하고 따옴표 안 \" 는 따옴표로 푼다.
pub fn split_args(line: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut in_arg = false;
    let mut quote: Option<char> = None;
    let rs: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < rs.len() {
        let c = rs[i];
        if let Some(q) = quote {
            if c == '\\' && i + 1 < rs.len() {
                i += 1;
                cur.push(rs[i]);
            } else if c == q {
                quote = None;
            } else {
                cur.push(c);
            }
        } else if c == '"' || c == '\'' {
            quote = Some(c);
            in_arg = true;
        } else if c.is_whitespace() {
            if in_arg {
                args.push(std::mem::take(&mut cur));
                in_arg = false;
            }
        } else {
            cur.push(c);
            in_arg = true;
        }
        i += 1;
    }
    if in_arg {
        args.push(cur);
    }
    args
}

// ─── 첫 단어 판정 ───

/// 앞쪽 공백·주석·여는 괄호를 건너뛴 첫 단어([A-Za-z0-9_] 연속)를 소문자로 돌려준다. 없으면 빈 글이다.
/// 단어는 공백이 아니라 식별자 글자에서 끊는다. select*from 처럼 붙여 써도 select 로 봐야 판정과 커서 상한을 피하지 못한다.
pub fn first_sql_word(stmt: &str) -> String {
    let mut s = stmt.as_bytes();
    loop {
        let skip = s.iter().take_while(|c| b" \t\r\n(".contains(c)).count();
        s = &s[skip..];
        if s.starts_with(b"--") {
            match s.iter().position(|&c| c == b'\n') {
                Some(i) => {
                    s = &s[i + 1..];
                    continue;
                }
                None => return String::new(),
            }
        }
        if !s.starts_with(b"/*") {
            break;
        }
        let (mut depth, mut i) = (0, 0);
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
        if depth != 0 {
            return String::new(); // 닫히지 않은 주석
        }
        s = &s[i..];
    }
    let end = s.iter().take_while(|c| **c == b'_' || c.is_ascii_alphanumeric()).count();
    String::from_utf8_lossy(&s[..end]).to_ascii_lowercase()
}

/// 첫 단어로 판정한다. with 안의 쓰기(CTE), select ... into, 부수효과가 있는 함수 호출은 잡지 못한다(명세 R6.1).
pub fn is_read_only_sql(stmt: &str) -> bool {
    match first_sql_word(stmt).as_str() {
        "" | "select" | "with" | "show" | "table" | "values" => true,
        // EXPLAIN ANALYZE 는 문장을 실제로 실행한다. EXPLAIN (ANALYZE, ...) 처럼 옵션 목록에 있어도 같다.
        "explain" => !has_analyze(stmt),
        _ => false,
    }
}

/// Go 정규식 `(?i)\banaly[sz]e\b` 와 같다. PostgreSQL 은 ANALYSE 철자도 받는다.
/// \b 는 ASCII 단어 경계이고, (?i) 는 유니코드 단순 접기라 s 자리에 ſ(U+017F)도 맞는다.
pub(crate) fn has_analyze(s: &str) -> bool {
    let b = s.as_bytes();
    let word = |c: u8| c == b'_' || c.is_ascii_alphanumeric();
    (0..b.len()).any(|i| {
        if i > 0 && word(b[i - 1]) {
            return false;
        }
        let r = &b[i..];
        if r.len() < 5 || !r[..5].eq_ignore_ascii_case(b"analy") {
            return false;
        }
        let n = match &r[5..] {
            [b's' | b'S' | b'z' | b'Z', ..] => 6,
            [0xc5, 0xbf, ..] => 7,
            _ => return false,
        };
        matches!(r.get(n), Some(b'e' | b'E')) && r.get(n + 1).is_none_or(|&c| !word(c))
    })
}

// ─── Go 와 같은 대소문자 ───

#[cfg(test)]
mod tests {
    use super::*;

    fn idx(text: &str, probe: &str) -> usize {
        text.find(probe).unwrap()
    }

    #[test]
    fn 커서가_있는_문장만_고른다() {
        let text = "select 1; select 2;";
        assert_eq!(statement_at(text, idx(text, "2")), "select 2");
        assert_eq!(statement_at(text, 0), "select 1");
    }

    #[test]
    fn 따옴표_달러태그_주석_안의_세미콜론은_경계가_아니다() {
        let text = "select ';', \"a;b\" -- x;y\n from t;\n\
                    create function f() returns int as $$ begin; return 1; end $$ language plpgsql;\n\
                    select $fmt$ a;b $fmt$ /* c;d */;";
        let stmts: Vec<&str> = ["from t", "return 1", "$fmt$ a"]
            .iter()
            .map(|p| statement_at(text, idx(text, p)))
            .collect();
        assert!(
            stmts[0].ends_with("from t") && stmts[0].starts_with("select ';'"),
            "첫 문장: {:?}",
            stmts[0]
        );
        assert!(
            stmts[1].starts_with("create function") && stmts[1].ends_with("language plpgsql"),
            "함수 문장: {:?}",
            stmts[1]
        );
        assert_eq!(stmts[2], "select $fmt$ a;b $fmt$ /* c;d */");
    }

    #[test]
    fn 마지막_세미콜론_뒤_커서는_뒤_조각() {
        let text = "select 1;\nselect 2";
        assert_eq!(statement_at(text, text.len()), "select 2");
    }

    #[test]
    fn 커서가_있는_줄을_고른다() {
        let text = "GET a\nHGETALL b\n";
        assert_eq!(line_at(text, idx(text, "HGET") + 2), "HGETALL b");
    }

    #[test]
    fn 따옴표_안_공백을_유지한다() {
        assert_eq!(split_args(r#"SET "a b" 'c d' "x\"y""#), ["SET", "a b", "c d", r#"x"y"#]);
        assert_eq!(split_args("GET 세션:키 값"), ["GET", "세션:키", "값"]);
        assert_eq!(split_args(r#"GET """#), ["GET", ""]);
    }

    #[test]
    fn 첫_단어로_읽기_전용을_가른다() {
        let read = [
            "select 1",
            "  WITH x as (select 1) select * from x",
            "-- 주석\nselect 1",
            "/* a */ explain select 1",
            "(select 1)",
        ];
        let write = [
            "update t set a=1",
            "delete from t",
            "insert into t values (1)",
            "create table t()",
            "drop table t",
            "explain analyze delete from t",
            "EXPLAIN (ANALYZE, BUFFERS) update t set a=1",
        ];
        for s in read {
            assert!(is_read_only_sql(s), "읽기여야 한다: {s:?}");
        }
        for s in write {
            assert!(!is_read_only_sql(s), "쓰기여야 한다: {s:?}");
        }
    }

    #[test]
    fn 읽기_명령만_통과한다() {
        assert!(is_read_only_redis(&["hgetall", "k"]));
        assert!(!is_read_only_redis(&["DEL", "k"]));
        assert!(!is_read_only_redis::<&str>(&[]));
    }

    /// Go 퍼즈 시드다. go test 가 시드만 돌리듯 같은 성질을 시드로 본다.
    #[test]
    fn 패닉없이_원문의_일부를_돌려준다() {
        for s in [
            "select 1; select 2;",
            "$$ a;b $$;",
            "'it''s';--x\n;",
            "/* ; */ select $t$ ; $t$;",
            "\"a;\"",
            "$1",
        ] {
            let offset = 3;
            if offset > s.len() {
                continue;
            }
            let got = statement_at(s, offset);
            assert!(s.contains(got), "원문에 없는 문장: {s:?} / {got:?}");
        }
    }

    #[test]
    fn 패닉없이_따옴표_없는_입력은_공백으로만_나눈다() {
        for line in [r#"GET "a b""#, r#"SET 'x' "y\"z""#, r#""unterminated"#, "\\", "", "GET 세션 값\r"] {
            let args = split_args(line);
            if line.contains(['"', '\'', '\\']) {
                continue;
            }
            let want: Vec<&str> = line.split_whitespace().collect();
            assert_eq!(args, want, "{line:?}");
        }
    }

    #[test]
    fn E문자열_중첩주석_식별자_달러() {
        let cases = [
            (r"SELECT E'it\'s; x'; DELETE FROM t;", "DELETE", "DELETE FROM t"),
            (r"SELECT E'it\'s; x'; DELETE FROM t;", "SELECT", r"SELECT E'it\'s; x'"),
            ("select /* a /* b; */ c; */ 1; select 2;", "select 2", "select 2"),
            ("select a$b; select 2;", "select 2", "select 2"),
            ("select 'e\\'; select 2;", "select 2", "select 2"), // E 접두가 없으면 백슬래시는 이스케이프가 아니다
        ];
        for (text, probe, want) in cases {
            assert_eq!(statement_at(text, idx(text, probe)), want, "{text:?} @{probe:?}");
        }
    }

    #[test]
    fn EXPLAIN_옵션_ANALYZE와_Redis_관리_하위명령() {
        for s in [
            "EXPLAIN (ANALYZE) DELETE FROM t",
            "explain (format json, analyze true) update t set a=1",
            "EXPLAIN ANALYSE DELETE FROM t",
            "explain (analyse) delete from t",
        ] {
            assert!(!is_read_only_sql(s), "쓰기여야 한다: {s:?}");
        }
        let write: [&[&str]; 5] = [
            &["SLOWLOG", "RESET"],
            &["client", "kill", "x"],
            &["CLIENT", "PAUSE", "1"],
            &["MEMORY", "PURGE"],
            &["CLIENT"],
        ];
        for args in write {
            assert!(!is_read_only_redis(args), "쓰기여야 한다: {args:?}");
        }
        // Redis 모니터(R34)가 보내는 명령도 읽기다
        let read: [&[&str]; 8] = [
            &["SLOWLOG", "GET"],
            &["client", "list"],
            &["MEMORY", "USAGE", "k"],
            &["SLOWLOG", "GET", "50"],
            &["info"],
            &["SCAN", "0", "COUNT", "500"],
            &["TYPE", "k"],
            &["TTL", "k"],
        ];
        for args in read {
            assert!(is_read_only_redis(args), "읽기여야 한다: {args:?}");
        }
    }

    #[test]
    fn 중첩_주석과_여러_줄_주석을_건너뛴다() {
        assert!(
            !is_read_only_sql("/* /* */ with */ delete from t"),
            "중첩 주석 뒤 delete 는 쓰기여야 한다"
        );
        assert_eq!(first_sql_word("/*\n설명\n*/\n-- 한 줄\nselect * from big"), "select");
        assert_eq!(first_sql_word("/* 안 닫힘 select"), "");
    }

    #[test]
    fn StatementAt과_같은_경계로_나누고_빈_조각은_뺀다() {
        let text = "select ';';\n-- 주석만\n;\n\
                    create function f() returns int as $$ begin; return 1; end $$ language plpgsql;\n\
                    select E'a\\';b', /* x; /* y; */ z; */ 1 ;;  \n select 3 -- 끝";
        assert_eq!(
            split_statements(text),
            [
                "select ';'",
                "create function f() returns int as $$ begin; return 1; end $$ language plpgsql",
                "select E'a\\';b', /* x; /* y; */ z; */ 1",
                "select 3 -- 끝",
            ]
        );
        assert!(split_statements(" \n-- x\n/* y */ ; ").is_empty(), "주석·공백뿐이면 비어야 한다");
    }

    #[test]
    fn 문장마다_원문의_일부이고_그_안의_커서는_그_문장을_고른다() {
        for text in [
            "select 1; select 2;",
            "$$ a;b $$;",
            "'it''s';--x\n;",
            "/* ; */ select $t$ ; $t$;",
            "E'\\';' ; x",
            "a$b$; c",
            "; ;\u{85};",
        ] {
            let got = split_statements(text);
            let mut i = 0;
            for sp in scan_statements(text) {
                let seg = &text[sp.start..sp.end];
                let stmt = seg.trim();
                if !sp.code || stmt.is_empty() {
                    continue;
                }
                assert_eq!(got.get(i), Some(&stmt), "{text:?}: {i}번째 문장이 다르다: {got:?}");
                let from = sp.start + seg.len() - seg.trim_start().len();
                for off in from..=from + stmt.len() {
                    assert_eq!(statement_at(text, off), stmt, "{text:?} @{off}");
                }
                i += 1;
            }
            assert_eq!(i, got.len(), "{text:?}: 문장 수가 다르다: {got:?}");
        }
    }

    #[test]
    fn 식별자_글자가_아닌_곳에서_끊는다() {
        let cases = [
            ("select*from t", "select"),
            ("select'x'", "select"),
            (r#"select"a"from t"#, "select"),
            ("(select 1)", "select"),
            ("( /* c */ (select 1))", "select"),
            ("with/*c*/x as(select 1) select 1", "with"),
            ("copy(select 1) to stdout", "copy"),
        ];
        for (input, want) in cases {
            assert_eq!(first_sql_word(input), want, "{input:?}");
            if want != "copy" {
                assert!(is_read_only_sql(input), "읽기여야 한다: {input:?}");
            }
        }
    }

    // ─── danger_test.go 의 가리기 시험 ───

    #[test]
    fn 문자열_주석_따옴표_이름_안을_지운다() {
        let input = String::from(r#"select 'a;b', "c;d", $t$ e;f $t$, E'g\'h;' -- i;j"#) + "\n/* k /* l; */ m; */ n";
        let want = String::from(r#"select '   ', "   ", $t$     $t$, E'     '"#) + &" ".repeat(7) + "\n" + &" ".repeat(20) + "n";
        assert_eq!(mask_sql(&input), want);
    }

    #[test]
    fn 따옴표와_달러_인용_밖의_주석만_찾는다() {
        let cases = [
            ("a = 1 -- 메모", true),
            ("a = 1 /* x */", true),
            ("a = '--' and b = 2", false),
            ("a = $$/* x */$$", false),
            (r"a = E'\' -- '", false),
            ("a = 1", false),
        ];
        for (input, want) in cases {
            assert_eq!(has_comment(input), want, "{input:?}");
        }
    }

    // ─── Go 와 다를 수 있는 자리 ───

    #[test]
    fn ANALYZE_는_ASCII_단어_경계와_유니코드_접기로_찾는다() {
        assert!(has_analyze("explain (ANALYZE) x"));
        assert!(has_analyze("explain analyſe x"));
        assert!(has_analyze("é analyze"));
        assert!(!has_analyze("explain analyzed x"));
        assert!(!has_analyze("explain _analyze x"));
        assert!(!has_analyze("explain analyz"));
    }

    #[test]
    fn 대소문자는_Go_처럼_글자_하나씩_바꾼다() {
        assert_eq!(go_lower("İNTO ΣΑΣ"), "into σασ");
        assert_eq!(go_upper("ﬆrlen fluſhdb ß"), "ﬆRLEN FLUSHDB ß");
    }
}
