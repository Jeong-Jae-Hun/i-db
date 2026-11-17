//! Go `encoding/json`(SetEscapeHTML(false)) 과 바이트가 같은 JSON 글을 쓴다.
//! CLI JSON 은 db-cli 스킬·Claude 가 읽는 계약이라 serde_json 의 이스케이프 차이(U+2028 등)도 맞춘다.

use std::fmt::Write;

/// JSON 문자열 하나를 따옴표까지 붙여 쓴다.
pub fn write_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            // JSONP 로 실릴 때 줄 끝으로 읽히지 않게 Go 가 늘 이스케이프한다
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c => out.push(c),
        }
    }
    out.push('"');
}

pub fn str_array<'a>(out: &mut String, items: impl IntoIterator<Item = &'a str>) {
    out.push('[');
    for (i, s) in items.into_iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_str(out, s);
    }
    out.push(']');
}

/// serde_json 오류를 값 없는 글로 바꾼다. serde 는 `invalid type: string "원문"` 처럼 잘못 든 값을 오류에 싣는데,
/// 설정·상태 파일에 비밀값을 잘못 붙여 넣으면 그 값이 stderr·상태 줄로 나간다. 종류와 위치만 남긴다.
pub fn json_error(e: &serde_json::Error) -> String {
    let kind = match e.classify() {
        serde_json::error::Category::Io => "io error",
        serde_json::error::Category::Syntax => "invalid JSON syntax",
        serde_json::error::Category::Data => "unexpected JSON value type",
        serde_json::error::Category::Eof => "unexpected end of JSON input",
    };
    format!("{kind} at line {} column {}", e.line(), e.column())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn JSON_오류에는_잘못_든_값이_나오지_않는다() {
        let e = serde_json::from_str::<Vec<i64>>(r#"["secret-pw-9"]"#).unwrap_err();
        let msg = json_error(&e);
        assert!(!msg.contains("secret"), "{msg}");
        assert_eq!(msg, "unexpected JSON value type at line 1 column 14");
    }

    #[test]
    fn go_와_같은_이스케이프() {
        let mut s = String::new();
        write_str(&mut s, "a\"\\\n\t\x01<&>\u{2028}한");
        assert_eq!(s, concat!(r#""a\"\\\n\t\u0001<&>"#, "\\u2028", "한\""));
    }
}
