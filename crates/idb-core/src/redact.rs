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

// ─── 비밀값 문장 가리기 ───

/// 비밀값이 든 실행 글 대신 기록·화면에 남기는 글이다.
pub const MASKED_SQL: &str = "-- (비밀값이 있어 기록하지 않음)";

/// 비밀번호를 싣는 문장이 있으면 글 전체를 MASKED_SQL 로 바꾼다(Go `db.MaskSecrets`). 실행 기록이 비밀값을 평문으로 남기지 않게 한다.
/// Go 정규식 `(?im)\bpassword\s+(e?'|\$)|^\s*(auth|acl\s+setuser|config\s+set\s+(requirepass|masterauth))\b|^\s*(hello|migrate)\b.*\bauth2?\b`
/// 을 손으로 옮겼다. \s 는 RE2 처럼 [\t\n\f\r ] 이고 \b 는 ASCII 낱말 경계다.
pub fn mask_secrets(text: &str) -> String {
    if has_secret(text.as_bytes()) {
        MASKED_SQL.to_string()
    } else {
        text.to_string()
    }
}

fn is_word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

fn is_space(c: u8) -> bool {
    matches!(c, b'\t' | b'\n' | b'\x0c' | b'\r' | b' ')
}

/// b[i..] 가 낱말 w 로 시작하고(대소문자 무시) 그 뒤가 낱말 경계면 끝 자리다.
fn word_at(b: &[u8], i: usize, w: &str) -> Option<usize> {
    let end = i + w.len();
    (end <= b.len() && b[i..end].eq_ignore_ascii_case(w.as_bytes()) && b.get(end).is_none_or(|&c| !is_word(c))).then_some(end)
}

/// 공백을 하나 이상 건너뛴 자리다.
fn spaces1(b: &[u8], i: usize) -> Option<usize> {
    let n = b[i.min(b.len())..].iter().take_while(|&&c| is_space(c)).count();
    (n > 0).then_some(i + n)
}

fn has_secret(b: &[u8]) -> bool {
    let lower = b.to_ascii_lowercase();
    // \bpassword\s+(e?'|\$)
    let pw = b"password";
    for i in 0..lower.len().saturating_sub(pw.len() - 1) {
        if &lower[i..i + pw.len()] != pw || (i > 0 && is_word(lower[i - 1])) {
            continue;
        }
        if let Some(j) = spaces1(&lower, i + pw.len()) {
            let rest = &lower[j..];
            if rest.starts_with(b"'") || rest.starts_with(b"e'") || rest.starts_with(b"$") {
                return true;
            }
        }
    }
    // 줄 머리 꼴. ^\s* 는 줄바꿈도 건너뛰므로 줄 머리마다 공백을 넘긴 자리에서 본다
    let starts = std::iter::once(0).chain(b.iter().enumerate().filter(|(_, c)| **c == b'\n').map(|(i, _)| i + 1));
    for s in starts {
        let i = s + b[s..].iter().take_while(|&&c| is_space(c)).count();
        if word_at(b, i, "auth").is_some() {
            return true;
        }
        if let Some(j) = word_at(b, i, "acl").and_then(|j| spaces1(b, j))
            && word_at(b, j, "setuser").is_some()
        {
            return true;
        }
        if let Some(j) = word_at(b, i, "config")
            .and_then(|j| spaces1(b, j))
            .and_then(|j| word_at(b, j, "set"))
            .and_then(|j| spaces1(b, j))
            && (word_at(b, j, "requirepass").is_some() || word_at(b, j, "masterauth").is_some())
        {
            return true;
        }
        if let Some(j) = word_at(b, i, "hello").or_else(|| word_at(b, i, "migrate")) {
            let line_end = b[j..].iter().position(|&c| c == b'\n').map_or(b.len(), |n| j + n);
            for k in j..line_end {
                if (k == 0 || !is_word(b[k - 1]))
                    && (word_at(&b[..line_end], k, "auth").is_some() || word_at(&b[..line_end], k, "auth2").is_some())
                {
                    return true;
                }
            }
        }
    }
    false
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

    #[test]
    fn 비밀번호를_싣는_문장은_글_전체를_가린다() {
        for s in [
            "ALTER ROLE app WITH PASSWORD 'hunter2';",
            "select 1;\ncreate user u encrypted password E'x';",
            "CREATE ROLE r LOGIN PASSWORD $$x$$",
            "AUTH default hunter2",
            "  auth hunter2",
            "ACL SETUSER bob on >hunter2",
            "CONFIG SET requirepass hunter2",
            "config set masterauth hunter2",
            "MIGRATE h 6379 k 0 5000 AUTH hunter2",
            "MIGRATE h 6379 k 0 5000 AUTH2 u hunter2",
            "HELLO 3 AUTH u hunter2",
            "GET a\nAUTH hunter2",
        ] {
            assert_eq!(mask_secrets(s), MASKED_SQL, "{s}");
        }
    }

    #[test]
    fn 비밀값이_없는_글은_그대로_둔다() {
        for s in [
            "SELECT password_hash FROM users",
            "select 'auth' as x",
            "GET auth",
            "CONFIG GET maxmemory",
            "MIGRATE h 6379 k 0 5000",
        ] {
            assert_eq!(mask_secrets(s), s);
        }
    }
}
