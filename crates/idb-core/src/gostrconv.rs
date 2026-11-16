//! Go `strconv`·`time.ParseDuration` 과 같은 규칙으로 숫자·기간 글을 읽는다.
//! 플래그 값·CSV 수식 보호처럼 받느냐 마느냐가 출력·종료 코드를 가르는 자리라 Rust 표준 파서 대신 쓴다.

/// Go `underscoreOK`: 밑줄은 숫자 사이나 진법 접두 바로 뒤에만 온다.
fn underscore_ok(s: &str) -> bool {
    let b = s.strip_prefix(['+', '-']).unwrap_or(s).as_bytes();
    let mut saw = b'^';
    let mut i = 0;
    let mut hex = false;
    if b.len() >= 2 && b[0] == b'0' && matches!(b[1].to_ascii_lowercase(), b'b' | b'o' | b'x') {
        i = 2;
        // 진법 접두도 숫자로 친다("0x_1" 을 받는다)
        saw = b'0';
        hex = b[1].eq_ignore_ascii_case(&b'x');
    }
    for &c in &b[i..] {
        if c.is_ascii_digit() || (hex && c.is_ascii_hexdigit()) {
            saw = b'0';
        } else if c == b'_' {
            if saw != b'0' {
                return false;
            }
            saw = b'_';
        } else {
            if saw == b'_' {
                return false;
            }
            saw = b'!';
        }
    }
    saw != b'_'
}

/// Go `strconv.ParseInt(s, 0, 64)`. 오류 글은 Go flag 가 바꿔 쓰는 꼴("parse error"·"value out of range")이다.
pub fn parse_int(s: &str) -> Result<i64, &'static str> {
    if s.contains('_') && !underscore_ok(s) {
        return Err("parse error");
    }
    let s = s.replace('_', "");
    let (neg, body) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s.as_str()),
    };
    let lower = body.to_ascii_lowercase();
    let (radix, digits) = if let Some(d) = lower.strip_prefix("0x") {
        (16, d)
    } else if let Some(d) = lower.strip_prefix("0o") {
        (8, d)
    } else if let Some(d) = lower.strip_prefix("0b") {
        (2, d)
    } else if lower.len() > 1 && lower.starts_with('0') {
        (8, &lower[1..])
    } else {
        (10, lower.as_str())
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
        return Err("parse error");
    }
    let mag = u64::from_str_radix(digits, radix).map_err(|_| "value out of range")?;
    if neg {
        if mag > i64::MAX as u64 + 1 {
            return Err("value out of range");
        }
        Ok((mag as i64).wrapping_neg())
    } else {
        i64::try_from(mag).map_err(|_| "value out of range")
    }
}

/// Go `strconv.ParseFloat(s, 64)` 가 오류 없이 받는지다. 넘치는 값(1e999)은 범위 오류라 받지 않고, 0 으로 사라지는 값은 받는다.
pub fn parses_float(s: &str) -> bool {
    let (sign, body) = match s.as_bytes().first() {
        Some(b'+' | b'-') => (true, &s[1..]),
        _ => (false, s),
    };
    match body.to_ascii_lowercase().as_str() {
        "inf" | "infinity" => return true,
        // nan 은 부호를 받지 않는다
        "nan" => return !sign,
        _ => {}
    }
    if body.contains('_') && !underscore_ok(s) {
        return false;
    }
    let body = body.replace('_', "");
    let lower = body.to_ascii_lowercase();
    if let Some(hex) = lower.strip_prefix("0x") {
        return parse_hex_float(hex).is_some_and(f64::is_finite);
    }
    // 10진: 숫자[.숫자][e[부호]숫자]. Rust 파서는 inf·nan 꼴도 받아 먼저 거른다
    let b = lower.as_bytes();
    let mantissa_end = b.iter().position(|&c| c == b'e').unwrap_or(b.len());
    let mantissa = &lower[..mantissa_end];
    let digits = mantissa.bytes().filter(u8::is_ascii_digit).count();
    if digits == 0 || mantissa.bytes().any(|c| !(c.is_ascii_digit() || c == b'.')) || mantissa.matches('.').count() > 1 {
        return false;
    }
    if mantissa_end < b.len() {
        let exp = &lower[mantissa_end + 1..];
        let exp = exp.strip_prefix(['+', '-']).unwrap_or(exp);
        if exp.is_empty() || !exp.bytes().all(|c| c.is_ascii_digit()) {
            return false;
        }
    }
    lower.parse::<f64>().is_ok_and(f64::is_finite)
}

/// `0x` 뒤 16진 가수와 반드시 있는 `p` 지수를 읽는다. 지수 부호는 하나만 받는다.
fn parse_hex_float(s: &str) -> Option<f64> {
    let (mant, exp) = s.split_once('p')?;
    let (neg_exp, exp_digits) = match exp.as_bytes().first() {
        Some(b'-') => (true, &exp[1..]),
        Some(b'+') => (false, &exp[1..]),
        _ => (false, exp),
    };
    if exp_digits.is_empty() || !exp_digits.bytes().all(|c| c.is_ascii_digit()) || mant.matches('.').count() > 1 {
        return None;
    }
    let (int, frac) = mant.split_once('.').unwrap_or((mant, ""));
    if int.is_empty() && frac.is_empty() || !int.chars().chain(frac.chars()).all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let mut v = 0f64;
    for c in int.chars().chain(frac.chars()) {
        v = v * 16.0 + c.to_digit(16)? as f64;
    }
    // 가수가 0 이면 지수와 상관없이 0 이다(0 × 2^큰수 를 계산하면 NaN 이 된다)
    if v == 0.0 {
        return Some(0.0);
    }
    let e: i64 = exp_digits.parse::<i64>().unwrap_or(i64::MAX).min(1 << 20);
    let mut e = if neg_exp { -e } else { e } - 4 * frac.len() as i64;
    // 한 번에 2^e 를 만들면 중간에 넘치거나 0 이 되므로 나눠 곱한다
    while e > 1000 {
        v *= 2f64.powi(1000);
        e -= 1000;
    }
    while e < -1000 {
        v *= 2f64.powi(-1000);
        e += 1000;
    }
    Some(v * 2f64.powi(e as i32))
}

/// Go `time.ParseDuration` 을 그대로 옮겼다. 나노초 미만은 버리고, int64 를 넘으면 None 이다.
/// 단위는 ns·us·µs·μs·ms·s·m·h, "0" 하나는 단위 없이 받는다.
pub fn parse_duration(s: &str) -> Option<i64> {
    let (neg, mut rest) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    if rest == "0" {
        return Some(0);
    }
    if rest.is_empty() {
        return None;
    }
    let mut d: u64 = 0;
    while !rest.is_empty() {
        let b = rest.as_bytes();
        if !(b[0] == b'.' || b[0].is_ascii_digit()) {
            return None;
        }
        // 정수 부분
        let int_end = b.iter().position(|c| !c.is_ascii_digit()).unwrap_or(b.len());
        let pre = int_end > 0;
        let mut v: u64 = 0;
        for &c in &b[..int_end] {
            v = v.checked_mul(10)?.checked_add((c - b'0') as u64)?;
            if v > 1 << 63 {
                return None;
            }
        }
        rest = &rest[int_end..];
        // 소수 부분. 넘칠 자리는 버린다(Go leadingFraction)
        let (mut f, mut scale, mut post) = (0u64, 1f64, false);
        if let Some(r) = rest.strip_prefix('.') {
            let fb = r.as_bytes();
            let end = fb.iter().position(|c| !c.is_ascii_digit()).unwrap_or(fb.len());
            post = end > 0;
            let mut overflow = false;
            for &c in &fb[..end] {
                if overflow {
                    continue;
                }
                if f > (1 << 63) / 10 {
                    overflow = true;
                    continue;
                }
                let y = f * 10 + (c - b'0') as u64;
                if y > 1 << 63 {
                    overflow = true;
                    continue;
                }
                f = y;
                scale *= 10.0;
            }
            rest = &r[end..];
        }
        if !pre && !post {
            return None;
        }
        let ub = rest.as_bytes();
        let unit_end = ub.iter().position(|&c| c == b'.' || c.is_ascii_digit()).unwrap_or(ub.len());
        let unit: u64 = match &rest[..unit_end] {
            "ns" => 1,
            "us" | "µs" | "μs" => 1_000,
            "ms" => 1_000_000,
            "s" => 1_000_000_000,
            "m" => 60_000_000_000,
            "h" => 3_600_000_000_000,
            _ => return None,
        };
        rest = &rest[unit_end..];
        if v > (1 << 63) / unit {
            return None;
        }
        v *= unit;
        if f > 0 {
            v += (f as f64 * (unit as f64 / scale)) as u64;
            if v > 1 << 63 {
                return None;
            }
        }
        d = d.checked_add(v)?;
        if d > 1 << 63 {
            return None;
        }
    }
    if neg {
        return Some((d as i64).wrapping_neg());
    }
    if d > (1 << 63) - 1 {
        return None;
    }
    Some(d as i64)
}

/// Go `time.Duration.String()`.
pub fn duration_string(ns: i64) -> String {
    if ns == 0 {
        return "0s".into();
    }
    let neg = ns < 0;
    let u = ns.unsigned_abs() as u128;
    let body = if u < 1_000_000_000 {
        let (unit, div) = if u < 1_000 {
            ("ns", 1u128)
        } else if u < 1_000_000 {
            ("µs", 1_000)
        } else {
            ("ms", 1_000_000)
        };
        format!("{}{unit}", frac(u, div))
    } else {
        let total_s = u / 1_000_000_000;
        let (h, m) = (total_s / 3600, (total_s / 60) % 60);
        let mut out = String::new();
        if h > 0 {
            out.push_str(&format!("{h}h"));
        }
        if h > 0 || m > 0 {
            out.push_str(&format!("{m}m"));
        }
        out.push_str(&format!("{}s", frac(u % 60_000_000_000, 1_000_000_000)));
        out
    };
    if neg { format!("-{body}") } else { body }
}

/// v/div 를 끝 0 없는 소수로 쓴다.
fn frac(v: u128, div: u128) -> String {
    let (whole, rem) = (v / div, v % div);
    if rem == 0 {
        return whole.to_string();
    }
    let width = div.to_string().len() - 1;
    format!("{whole}.{}", format!("{rem:0width$}").trim_end_matches('0'))
}

#[cfg(test)]
mod tests {
    use super::*;

    // 기대값은 Go 1.23 strconv·time 에 같은 입력을 넣어 얻었다
    #[test]
    fn 정수는_Go_ParseInt_진법0_과_같다() {
        let cases: [(&str, Result<i64, &str>); 12] = [
            ("200", Ok(200)),
            ("1_000", Ok(1000)),
            ("0x_10", Ok(16)),
            ("0_7", Ok(7)),
            ("0b1_0", Ok(2)),
            ("07", Ok(7)),
            ("0o17", Ok(15)),
            ("-0x10", Ok(-16)),
            ("_1", Err("parse error")),
            ("1__0", Err("parse error")),
            ("08", Err("parse error")),
            ("99999999999999999999", Err("value out of range")),
        ];
        for (s, want) in cases {
            assert_eq!(parse_int(s), want, "{s}");
        }
    }

    #[test]
    fn 기간은_Go_ParseDuration_과_같다() {
        let cases: [(&str, Option<i64>); 13] = [
            ("0.5ns", Some(0)),
            ("2562048h", None),
            ("2562047h", Some(9223369200000000000)),
            ("1.0000000005s", Some(1000000000)),
            ("1h1.5m", Some(3690000000000)),
            (".5s", Some(500000000)),
            ("5.s", Some(5000000000)),
            ("-0", Some(0)),
            ("1e3s", None),
            ("9223372036854775807ns", Some(i64::MAX)),
            ("9223372036854775808ns", None),
            ("-1s", Some(-1_000_000_000)),
            ("10", None),
        ];
        for (s, want) in cases {
            assert_eq!(parse_duration(s), want, "{s}");
        }
        assert_eq!(duration_string(30_000_000_000), "30s");
        assert_eq!(duration_string(3_600_000_000_000), "1h0m0s");
        assert_eq!(duration_string(1_500_000_000), "1.5s");
        assert_eq!(duration_string(250_000_000), "250ms");
    }

    #[test]
    fn 실수는_Go_ParseFloat_가_받는_것만_받는다() {
        let cases = [
            ("-0x1p2", true),
            ("+1_000", true),
            ("+1e999", false),
            ("-inf", true),
            ("+Inf", true),
            ("-nan", false),
            ("+infinity", true),
            ("-1e-999", true),
            ("+0x1_0p0", true),
            ("-.5", true),
            ("+5.", true),
            ("-1e", false),
            ("+0x10", false),
            ("-1_0", true),
            ("+0x1p+-2", false),
            ("+0x0p1024", true),
            ("0x1p1023", true),
            ("0x1p1024", false),
            ("0x.8p1025", false),
            ("0x1p-2000", true),
            ("-0x1.fffffffffffffp1023", true),
            ("0xp1", false),
            ("0x1.p1", true),
            ("0x_1p1", true),
            ("=SUM(A1)", false),
            ("-", false),
        ];
        for (s, want) in cases {
            assert_eq!(parses_float(s), want, "{s}");
        }
    }
}
