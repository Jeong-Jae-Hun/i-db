//! 여러 출력이 같이 쓰는 글자 처리. Go `internal/textutil`·charmbracelet `ansi` 의 폭 계산과 같은 결과를 낸다.

use std::fmt::Write;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// DB 값·키·오류의 제어 문자를 보이는 글자로 바꾼다. 터미널 제어(ESC·OSC)로 해석되지 않게 한다.
/// 탭은 칸 폭을 지키려고 공백 두 칸으로 바꾼다.
pub fn printable(s: &str, keep_newline: bool) -> String {
    let mut b = String::with_capacity(s.len());
    for r in s.chars() {
        let n = r as u32;
        match r {
            '\n' if keep_newline => b.push(r),
            '\t' => b.push_str("  "),
            _ if n < 0x20 || n == 0x7f || (0x80..0xa0).contains(&n) => {
                let _ = write!(b, "\\x{n:02x}");
            }
            _ => b.push(r),
        }
    }
    b
}

/// 글자 묶음(grapheme) 하나의 터미널 폭이다. uniseg 처럼 넓은 글자·이모지는 2, 결합 문자만이면 0 이다.
fn grapheme_width(g: &str) -> usize {
    let first = g.chars().next().unwrap_or(' ');
    let n = first as u32;
    if (n as usize) < 0x20 || n == 0x7f {
        return 0;
    }
    // 이모지 표시 선택자(FE0F)가 붙으면 2칸으로 보인다
    if g.contains('\u{FE0F}') {
        return 2;
    }
    UnicodeWidthStr::width(first.encode_utf8(&mut [0; 4]) as &str).min(2)
}

/// 터미널에 찍힐 폭이다. 입력은 printable 을 거친 글이라 ANSI 제어열이 없다.
pub fn string_width(s: &str) -> usize {
    s.graphemes(true).map(grapheme_width).sum()
}

/// 폭 length 를 넘으면 tail 을 붙여 그 폭 안으로 자른다(charmbracelet `ansi.Truncate`).
pub fn truncate(s: &str, length: usize, tail: &str) -> String {
    if string_width(s) <= length {
        return s.to_string();
    }
    let tw = string_width(tail);
    let Some(length) = length.checked_sub(tw) else {
        return String::new();
    };
    let mut out = String::new();
    let mut cur = 0;
    for g in s.graphemes(true) {
        let w = grapheme_width(g);
        if cur + w > length {
            out.push_str(tail);
            return out;
        }
        cur += w;
        out.push_str(g);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 제어문자는_보이는_글자로_줄바꿈은_고를_수_있다() {
        assert_eq!(printable("a\x1b[31mb", false), r"a\x1b[31mb");
        assert_eq!(printable("a\nb", false), r"a\x0ab");
        assert_eq!(printable("a\nb", true), "a\nb");
        assert_eq!(printable("탭\t끝\x7f\u{85}", false), r"탭  끝\x7f\x85");
    }

    #[test]
    fn 넓은_글자는_2칸이고_자르면_꼬리까지_폭_안이다() {
        assert_eq!(string_width("넓은글자"), 8);
        assert_eq!(string_width("ab"), 2);
        assert_eq!(truncate("가나다라", 5, "…"), "가나…");
        assert_eq!(truncate("abc", 3, "…"), "abc");
    }
}
