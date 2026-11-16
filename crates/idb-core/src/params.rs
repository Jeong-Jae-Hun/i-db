//! SQL 의 :name·$n 자리표시를 찾아 PostgreSQL 바인딩 형식($n)으로 맞춘다. Go `internal/params` 와 같다.
//! 토큰은 Go `internal/sqlfmt` 의 `Tokenize` 를 옮긴 것으로 읽는다. sqlfmt 를 옮기면 그쪽으로 뺀다.

use crate::project::go_atoi;

/// PostgreSQL 이 한 문장에 받는 바인딩 인자의 상한이다. 넘거나 정수로 못 읽는 $n 은 이름 목록을 만들지 않는다
const MAX_PARAMS: i64 = 65535;

/// :name 을 $k 로 바꾼 SQL 과, $k 마다 물어볼 이름(names[k-1])을 돌려준다.
/// $1·$2 처럼 이미 번호인 자리는 그대로 두고 이름을 "1"·"2" 로 쓴다. 같은 :name 은 같은 번호를 받고,
/// 번호와 섞이면 :name 은 가장 큰 $n 다음 번호부터 받는다. 문자열·따옴표 이름·주석·$$ 안과
/// ::type 캐스트, a[1:2]·a[lo:hi]·a[:n] 처럼 ':' 앞뒤가 붙은 배열 조각은 건드리지 않는다.
pub fn bind(sql: &str) -> (String, Vec<String>) {
    let toks = tokenize(sql);
    let text = |t: &Token| &sql[t.start..t.end];
    let highest = toks
        .iter()
        .filter(|t| t.kind == Kind::Param)
        .filter_map(|t| go_atoi(&text(t)[1..]).filter(|&n| n <= MAX_PARAMS))
        .max()
        .unwrap_or(0);
    let mut names: Vec<String> = (1..=highest).map(|n| n.to_string()).collect();

    let mut b = String::new();
    let mut last = 0;
    let mut nums: Vec<(&str, usize)> = Vec::new();
    let mut i = 0;
    while i + 1 < toks.len() {
        let (colon, word) = (&toks[i], &toks[i + 1]);
        if colon.kind != Kind::Punct || text(colon) != ":" || colon.end != word.start {
            i += 1;
            continue;
        }
        if text(word) == ":" {
            i += 2; // :: 캐스트 뒤 타입 이름은 자리표시가 아니다
            continue;
        }
        // ':' 바로 앞에 이름·숫자가 붙으면 a[lo:hi] 같은 배열 조각이다. 바꾸면 x$1·$$1 처럼 앞 토큰과 붙어
        // 다른 낱말이나 $$ 인용이 되기도 한다. a[:n] 처럼 '[' 바로 뒤도 아래쪽을 생략한 배열 조각이다
        let glued = i > 0 && toks[i - 1].end == colon.start && {
            let p = &toks[i - 1];
            matches!(p.kind, Kind::Word | Kind::Number | Kind::Quoted | Kind::Param) || matches!(text(p), "$" | "[")
        };
        if word.kind != Kind::Word || glued {
            i += 1;
            continue;
        }
        let name = text(word);
        let k = match nums.iter().find(|(n, _)| *n == name) {
            Some(&(_, k)) => k,
            None => {
                names.push(name.to_string());
                nums.push((name, names.len()));
                names.len()
            }
        };
        b.push_str(&sql[last..colon.start]);
        b.push('$');
        b.push_str(&k.to_string());
        last = word.end;
        i += 1;
    }
    if last == 0 {
        return (sql.to_string(), names);
    }
    b.push_str(&sql[last..]);
    (b, names)
}

// ─── 토큰 ───

/// 토큰 종류다. 연산자·괄호·쉼표 같은 기호는 모두 한 바이트짜리 Punct 다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
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
struct Token {
    kind: Kind,
    start: usize,
    end: usize,
}

const SPACE_BYTES: &[u8] = b" \t\n\r\x0c\x0b";
/// 문자열 바로 앞에 붙는 PostgreSQL 접두사(E·B·X·N). E 만 역슬래시 이스케이프를 쓴다
const STRING_PREFIXES: &[u8] = b"eEbBxXnN";

/// sql 을 공백 밖의 토큰으로 나눈다. 닫히지 않은 문자열·주석은 끝까지 한 토큰이다.
fn tokenize(sql: &str) -> Vec<Token> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 이름_자리를_번호로_바꾸고_나머지는_건드리지_않는다() {
        let (sql, names) = bind("select ':x', \"a:b\", x::int, :id, :id, arr[1:2], a[lo:hi], $$ :no $$, :name -- :c\n/* :d */");
        assert_eq!(
            sql,
            "select ':x', \"a:b\", x::int, $1, $1, arr[1:2], a[lo:hi], $$ :no $$, $2 -- :c\n/* :d */"
        );
        assert_eq!(names, ["id", "name"]);
    }

    #[test]
    fn 아래쪽을_생략한_배열_조각은_자리로_보지_않는다() {
        assert_eq!(
            bind("select a[:n], b[ :m], :id"),
            ("select a[:n], b[ $1], $2".into(), vec!["m".into(), "id".into()])
        );
    }

    #[test]
    fn 번호_자리는_그대로_두고_이름은_가장_큰_번호_다음을_받는다() {
        let (sql, names) = bind("select $2, :a, $1, :b, :a");
        assert_eq!(sql, "select $2, $3, $1, $4, $3");
        assert_eq!(names, ["1", "2", "a", "b"]);
        let (sql, names) = bind("select $1 where x = $1");
        assert_eq!(sql, "select $1 where x = $1");
        assert_eq!(names, ["1"]);
    }

    #[test]
    fn 자리가_없으면_그대로_돌려준다() {
        let input = "select 1 where a = ':b' and $9999999999999999999 is null";
        assert_eq!(bind(input), (input.into(), vec![]));
    }

    #[test]
    fn 붙여_쓴_비교와_한글_이름도_찾는다() {
        let (sql, names) = bind("select * from t where id=:id and 이름 = :이름");
        assert_eq!(sql, "select * from t where id=$1 and 이름 = $2");
        assert_eq!(names, ["id", "이름"]);
    }

    /// Go 퍼즈 시드다. go test 가 시드만 돌리듯 같은 성질을 시드로 본다.
    #[test]
    fn 패닉없이_번호는_이름_수를_넘지_않는다() {
        for input in ["select :a, $2, x::int, ':b'", "::a:b:::c$1:"] {
            let (sql, names) = bind(input);
            assert!(!names.is_empty() || sql == input, "changed without names: {input:?} -> {sql:?}");
            // 바꾼 결과에는 이름 자리가 남지 않아 다시 묶으면 그대로이고 이름은 "1".."N" 이다
            let (again, numbered) = bind(&sql);
            assert!(
                again == sql && numbered.len() == names.len(),
                "rebind {sql:?} -> {again:?}, names {names:?} -> {numbered:?}"
            );
        }
    }

    #[test]
    fn 닫히지_않은_E_문자열은_끝까지_한_토큰이다() {
        let toks = tokenize("e'\\");
        assert_eq!((toks.len(), toks[0].kind, toks[0].end), (1, Kind::String, 3));
    }
}
