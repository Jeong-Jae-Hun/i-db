//! 서버 오류 글에 실린 비밀번호를 지운다. Go `internal/db/redact.go` 의 `Redact` 와 같다.

/// 서버 오류가 비밀번호를 되돌려줘도 화면·응답·기록에 나오지 않게 지운다.
/// 원문과 URL 인코딩 세 가지(query·path·접속 URL 의 userinfo)를 지우고, %HH 의 16진수는 대소문자를 가리지 않는다.
pub fn redact(s: &str, secret: &str) -> String {
    if secret.is_empty() {
        return s.to_string();
    }
    let mut out = s.to_string();
    for v in [
        secret.to_string(),
        escape(secret, Mode::Query),
        escape(secret, Mode::PathSegment),
        escape(secret, Mode::UserPassword),
    ] {
        out = replace_pattern(&out, v.as_bytes());
    }
    out
}

/// 비밀값이 든 실행 글 대신 기록·화면에 남기는 글이다.
pub const MASKED_SQL: &str = "-- (비밀값이 있어 기록하지 않음)";

/// 비밀번호를 싣는 문장이 있으면 글 전체를 MASKED_SQL 로 바꾼다(Go `MaskSecrets`). 정규식
/// `(?im)\bpassword\s+(e?'|\$)|^\s*(auth|acl\s+setuser|config\s+set\s+(requirepass|masterauth))\b|^\s*(hello|migrate)\b.*\bauth2?\b`
/// 과 같은 판정을 바이트로 한다. 리터럴만 골라 지우면 따옴표·이스케이프 변형에서 새므로 글 전체를 가린다.
pub fn mask_secrets(text: &str) -> String {
    if has_secret(text) { MASKED_SQL.into() } else { text.into() }
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b'\x0c')
}

/// s 가 word 로 시작하고 그 뒤가 낱말 글자가 아닌지(\b)다.
fn starts_word(s: &[u8], word: &str) -> bool {
    s.len() >= word.len() && s[..word.len()].eq_ignore_ascii_case(word.as_bytes()) && s.get(word.len()).is_none_or(|&b| !is_word(b))
}

fn has_secret(text: &str) -> bool {
    let b = text.as_bytes();
    // password\s+(e?'|$)
    for i in 0..b.len() {
        if (i == 0 || !is_word(b[i - 1])) && b.len() - i >= 8 && b[i..i + 8].eq_ignore_ascii_case(b"password") {
            let mut j = i + 8;
            let ws = j;
            while j < b.len() && is_space(b[j]) {
                j += 1;
            }
            if j > ws {
                let rest = &b[j..];
                if rest.first().is_some_and(|&c| c == b'\'' || c == b'$')
                    || rest.len() >= 2 && rest[0].eq_ignore_ascii_case(&b'e') && rest[1] == b'\''
                {
                    return true;
                }
            }
        }
    }
    text.split('\n').any(|line| {
        let t: Vec<&[u8]> = line.as_bytes().split(|&c| is_space(c)).filter(|w| !w.is_empty()).collect();
        let Some(first) = t.first() else { return false };
        let word = |i: usize, w: &str| t.get(i).is_some_and(|x| x.eq_ignore_ascii_case(w.as_bytes()));
        if starts_word(first, "auth")
            || word(0, "acl") && t.get(1).is_some_and(|x| starts_word(x, "setuser"))
            || word(0, "config")
                && word(1, "set")
                && t.get(2)
                    .is_some_and(|x| starts_word(x, "requirepass") || starts_word(x, "masterauth"))
        {
            return true;
        }
        if !(starts_word(first, "hello") || starts_word(first, "migrate")) {
            return false;
        }
        // 첫 낱말 뒤 어디든 낱말 auth·auth2
        let lb = line.as_bytes();
        let start = lb.iter().position(|&c| !is_space(c)).unwrap_or(0) + 5;
        (start..lb.len()).any(|i| (i == 0 || !is_word(lb[i - 1])) && (starts_word(&lb[i..], "auth") || starts_word(&lb[i..], "auth2")))
    })
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Query,
    PathSegment,
    UserPassword,
}

/// Go `url.QueryEscape`·`url.PathEscape`·`url.UserPassword(…).String()` 의 비밀번호 쪽 인코딩이다.
fn escape(s: &str, mode: Mode) -> String {
    let mut out = String::new();
    for &c in s.as_bytes() {
        let keep = match c {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => true,
            b'$' | b'&' | b'+' | b',' | b'/' | b':' | b';' | b'=' | b'?' | b'@' => match mode {
                Mode::Query => false,
                Mode::PathSegment => !matches!(c, b'/' | b';' | b',' | b'?'),
                Mode::UserPassword => !matches!(c, b'@' | b'/' | b'?' | b':'),
            },
            _ => false,
        };
        if keep {
            out.push(c as char);
        } else if c == b' ' && mode == Mode::Query {
            out.push('+');
        } else {
            out.push_str(&format!("%{c:02X}"));
        }
    }
    out
}

/// pat 과 같은 조각을 모두 *** 로 바꾼다. pat 의 %HH 는 16진수 대소문자를 가리지 않고 맞춘다(Go 판의 정규식과 같다).
fn replace_pattern(s: &str, pat: &[u8]) -> String {
    let b = s.as_bytes();
    let matches_at = |i: usize| {
        if i + pat.len() > b.len() {
            return false;
        }
        let mut j = 0;
        while j < pat.len() {
            let hex = pat[j] == b'%' && j + 2 < pat.len() && pat[j + 1].is_ascii_hexdigit() && pat[j + 2].is_ascii_hexdigit();
            if hex {
                if b[i + j] != b'%' || !b[i + j + 1].eq_ignore_ascii_case(&pat[j + 1]) || !b[i + j + 2].eq_ignore_ascii_case(&pat[j + 2]) {
                    return false;
                }
                j += 3;
                continue;
            }
            if b[i + j] != pat[j] {
                return false;
            }
            j += 1;
        }
        true
    };
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if matches_at(i) {
            out.extend_from_slice(b"***");
            i += pat.len();
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    // 바꾼 자리는 ASCII 이고 pat 은 온전한 UTF-8 이라 경계가 글자 중간에 걸리지 않는다
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 비밀번호_문장은_글_전체를_가린다() {
        for s in [
            "alter role a password 'x'",
            "CREATE USER u WITH PASSWORD E'x'",
            "alter user u password $$x$$",
            "select 1;\n  AUTH secret",
            "acl  setuser bob on >pw",
            "CONFIG SET requirepass x",
            "hello 3 AUTH u p",
            "migrate h 6379 k 0 5000 AUTH2 u p",
        ] {
            assert_eq!(mask_secrets(s), MASKED_SQL, "{s}");
        }
        for s in [
            "select password from users",
            "select 'password'",
            "authors",
            "hello world",
            "config get requirepass",
            "select mypassword 'x'",
        ] {
            assert_eq!(mask_secrets(s), s, "{s}");
        }
    }

    #[test]
    fn 원문과_URL_인코딩과_소문자_퍼센트를_지운다() {
        let secret = "p@+ss/:?";
        for leaked in [
            secret.to_string(),
            escape(secret, Mode::Query),
            escape(secret, Mode::PathSegment),
            escape(secret, Mode::UserPassword),
            escape(secret, Mode::Query).to_lowercase(),
        ] {
            assert_eq!(redact(&format!("x {leaked} y"), secret), "x *** y", "{leaked}");
        }
        assert_eq!(redact("x y", ""), "x y");
    }

    #[test]
    fn 퍼센트_16진수_대소문자가_섞여도_지운다() {
        for leaked in ["p%3a%2F%3f", "p%3A%2f%3F", "p:/?"] {
            assert_eq!(redact(&format!("x {leaked} y"), "p:/?"), "x *** y", "{leaked}");
        }
    }

    #[test]
    fn 인코딩은_Go_url_패키지와_같다() {
        let s = "a b!$&+,/:;=?@~";
        assert_eq!(escape(s, Mode::Query), "a+b%21%24%26%2B%2C%2F%3A%3B%3D%3F%40~");
        assert_eq!(escape(s, Mode::PathSegment), "a%20b%21$&+%2C%2F:%3B=%3F@~");
        assert_eq!(escape(s, Mode::UserPassword), "a%20b%21$&+,%2F%3A;=%3F%40~");
    }
}
