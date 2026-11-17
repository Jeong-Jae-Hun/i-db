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

/// Go `strings.ToLower` 와 같다. 글자마다 단순 소문자 하나로 바꾼다(İ 는 i, 끝 Σ 도 σ).
pub fn go_lower(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\u{130}' => 'i',
            c => c.to_lowercase().next().unwrap_or(c),
        })
        .collect()
}

/// Go `strings.ToUpper` 에 가깝다. 대문자가 여러 글자로 늘어나는 글자(ß·합자 등)는 Go 처럼 그대로 둔다.
/// 결과는 ASCII 명령·형식 이름과만 대조해서 늘어나지 않는 글자만 맞으면 된다.
pub fn go_upper(s: &str) -> String {
    s.chars()
        .map(|c| {
            let mut up = c.to_uppercase();
            match (up.next(), up.next()) {
                (Some(u), None) => u,
                _ => c,
            }
        })
        .collect()
}

// ─── 수 글 ───

/// Go `textutil.GroupDigits`: 세 자리마다 쉼표를 넣는다. 음수의 부호 뒤에도 넣는 Go 꼴(-,123)까지 같다.
pub fn group_digits(n: i64) -> String {
    let mut s = n.to_string();
    let mut i = s.len() as i64 - 3;
    while i > 0 {
        s.insert(i as usize, ',');
        i -= 3;
    }
    s
}

/// Go `textutil.KoreanCount`: 만·억·조 단위로 줄인다. 1만 미만은 쉼표 꼴이고, 단위로 10 미만이면 소수 한 자리(.0 은 뗌),
/// 10 이상이면 정수다. 올리지 않고 버린다.
///   - 예: 37 → "37", 12000 → "1.2만", 1200000 → "120만", 150000000 → "1.5억"
pub fn korean_count(n: i64) -> String {
    let sign = if n < 0 { "-" } else { "" };
    let u = n.unsigned_abs();
    for (name, size) in [("조", 1_000_000_000_000u64), ("억", 100_000_000), ("만", 10_000)] {
        if u < size {
            continue;
        }
        let whole = u / size;
        if whole >= 10 {
            return format!("{sign}{}{name}", group_digits(whole as i64));
        }
        let tenths = u * 10 / size;
        if tenths.is_multiple_of(10) {
            return format!("{sign}{}{name}", tenths / 10);
        }
        return format!("{sign}{}.{}{name}", tenths / 10, tenths % 10);
    }
    format!("{sign}{}", group_digits(u as i64))
}

/// Go `db.HumanBytes`: 1024 단위로 줄인다(예: 1.5KB).
pub fn human_bytes(n: i64) -> String {
    if n < 1024 {
        return format!("{n}B");
    }
    let mut f = n as f64;
    for u in ["KB", "MB", "GB", "TB"] {
        f /= 1024.0;
        if f < 1024.0 || u == "TB" {
            return format!("{f:.1}{u}");
        }
    }
    String::new()
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

    #[test]
    fn 수를_만_억_단위와_바이트로_줄인다() {
        assert_eq!(korean_count(37), "37");
        assert_eq!(korean_count(9999), "9,999");
        assert_eq!(korean_count(12000), "1.2만");
        assert_eq!(korean_count(1_200_000), "120만");
        assert_eq!(korean_count(150_000_000), "1.5억");
        assert_eq!(korean_count(-20000), "-2만");
        assert_eq!(korean_count(i64::MIN), "-9,223,372조");
        assert_eq!(group_digits(-123), "-,123");
        assert_eq!(human_bytes(1023), "1023B");
        assert_eq!(human_bytes(1536), "1.5KB");
    }
}
