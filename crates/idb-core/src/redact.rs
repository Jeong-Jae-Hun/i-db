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

#[cfg(test)]
mod tests {
    use super::*;

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
