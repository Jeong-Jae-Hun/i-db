//! Go `flag` 패키지(ContinueOnError) 와 같은 규칙으로 읽고 같은 글로 오류·도움말을 찍는다.
//! `db <명령> -h`·사용법 오류 글이 Go 판과 바이트로 같아야 해(골든 비교) 직접 만든다.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use idb_core::project::go_quote;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FlagType {
    Str,
    Int,
    Bool,
    Duration,
    /// flag.Var 로 정의한 값. 도움말에 " value" 로 보인다
    Value,
}

pub struct FlagDef {
    pub name: &'static str,
    pub usage: String,
    pub ty: FlagType,
    /// 0 값이 아닌 기본값의 글(도움말 "(default …)"). None 이면 0 값이다
    pub default: Option<String>,
}

/// 읽은 값. Value 형은 set 이 받은 글을 차례로 모은다(호출부가 뜻을 푼다).
#[derive(Default)]
pub struct Parsed {
    pub strs: HashMap<&'static str, String>,
    pub ints: HashMap<&'static str, i64>,
    pub bools: HashMap<&'static str, bool>,
    /// 나노초(부호 있음)
    pub durs: HashMap<&'static str, i128>,
    pub values: HashMap<&'static str, Vec<String>>,
    /// 실제로 준 플래그 이름(Go fs.Visit)
    pub set: HashSet<&'static str>,
}

pub enum ParseError {
    /// -h·-help. 도움말을 찍었다
    Help,
    /// 오류 글과 도움말을 찍었다
    Failed,
}

/// Value 형 플래그의 값 검사. Err 글은 Go 의 `invalid value %q for flag -%s: <글>` 끝에 붙는다.
pub type ValueCheck = fn(&str, &str) -> Result<(), String>;

pub struct FlagSet {
    pub defs: Vec<FlagDef>,
    pub check: ValueCheck,
}

impl FlagSet {
    fn find(&self, name: &str) -> Option<&FlagDef> {
        self.defs.iter().find(|d| d.name == name)
    }

    /// Go `fs.PrintDefaults` 와 같은 글이다. 이름 순으로 찍는다.
    pub fn defaults(&self) -> String {
        let sorted: BTreeMap<&str, &FlagDef> = self.defs.iter().map(|d| (d.name, d)).collect();
        let mut out = String::new();
        for d in sorted.values() {
            let mut b = format!("  -{}", d.name);
            let ty = match d.ty {
                FlagType::Str => "string",
                FlagType::Int => "int",
                FlagType::Bool => "",
                FlagType::Duration => "duration",
                FlagType::Value => "value",
            };
            if !ty.is_empty() {
                b.push(' ');
                b.push_str(ty);
            }
            // 한 글자 bool 은 같은 줄에, 나머지는 다음 줄 들여쓰기에 설명을 둔다
            if b.chars().count() <= 4 {
                b.push('\t');
            } else {
                b.push_str("\n    \t");
            }
            b.push_str(&d.usage.replace('\n', "\n    \t"));
            if let Some(def) = &d.default {
                if d.ty == FlagType::Str {
                    b.push_str(&format!(" (default {})", go_quote(def)));
                } else {
                    b.push_str(&format!(" (default {def})"));
                }
            }
            out.push_str(&b);
            out.push('\n');
        }
        out
    }

    /// 플래그를 앞에서부터 읽고 남은 인자를 돌려준다(Go `fs.Parse` 한 번). 오류 글은 err_out 에 쓰고 usage 를 부른다.
    fn parse_once(
        &self,
        p: &mut Parsed,
        mut args: &[String],
        err_out: &mut String,
        usage: &dyn Fn(&mut String),
    ) -> Result<Vec<String>, ParseError> {
        loop {
            let Some(s) = args.first() else { return Ok(Vec::new()) };
            let b = s.as_bytes();
            if b.len() < 2 || b[0] != b'-' {
                return Ok(args.to_vec());
            }
            let mut minuses = 1;
            if b[1] == b'-' {
                minuses = 2;
                if b.len() == 2 {
                    return Ok(args[1..].to_vec());
                }
            }
            let name_part = &s[minuses..];
            if name_part.is_empty() || name_part.starts_with('-') || name_part.starts_with('=') {
                return Err(self.fail(err_out, usage, format!("bad flag syntax: {s}")));
            }
            args = &args[1..];
            let (name, value) = match name_part.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (name_part, None),
            };
            let Some(def) = self.find(name) else {
                if name == "help" || name == "h" {
                    usage(err_out);
                    return Err(ParseError::Help);
                }
                return Err(self.fail(err_out, usage, format!("flag provided but not defined: -{name}")));
            };
            if def.ty == FlagType::Bool {
                let v = value.clone().unwrap_or_else(|| "true".into());
                match go_parse_bool(&v) {
                    Some(x) => {
                        p.bools.insert(def.name, x);
                    }
                    None => {
                        return Err(self.fail(
                            err_out,
                            usage,
                            format!("invalid boolean value {} for -{name}: parse error", go_quote(&v)),
                        ));
                    }
                }
                p.set.insert(def.name);
                continue;
            }
            let value = match value {
                Some(v) => v,
                None => match args.first() {
                    Some(v) => {
                        args = &args[1..];
                        v.clone()
                    }
                    None => return Err(self.fail(err_out, usage, format!("flag needs an argument: -{name}"))),
                },
            };
            let bad = |e: &str| format!("invalid value {} for flag -{name}: {e}", go_quote(&value));
            match def.ty {
                FlagType::Str => {
                    p.strs.insert(def.name, value.clone());
                }
                FlagType::Int => match go_parse_int(&value) {
                    Ok(n) => {
                        p.ints.insert(def.name, n);
                    }
                    Err(e) => return Err(self.fail(err_out, usage, bad(e))),
                },
                FlagType::Duration => match parse_go_duration(&value) {
                    Some(ns) => {
                        p.durs.insert(def.name, ns);
                    }
                    None => return Err(self.fail(err_out, usage, bad("parse error"))),
                },
                FlagType::Value => {
                    if let Err(e) = (self.check)(def.name, &value) {
                        return Err(self.fail(err_out, usage, bad(&e)));
                    }
                    p.values.entry(def.name).or_default().push(value.clone());
                }
                FlagType::Bool => unreachable!(),
            }
            p.set.insert(def.name);
        }
    }

    fn fail(&self, err_out: &mut String, usage: &dyn Fn(&mut String), msg: String) -> ParseError {
        err_out.push_str(&msg);
        err_out.push('\n');
        usage(err_out);
        ParseError::Failed
    }

    /// 플래그와 위치 인자를 섞어 읽는다(`db query -c x "select 1" --format json`). `--` 뒤는 모두 위치 인자다.
    /// Go `parseInterleaved` 와 같다: 플래그가 아닌 인자에서 멈추면 그 하나를 위치 인자로 떼고 다시 읽는다.
    pub fn parse_interleaved(
        &self,
        p: &mut Parsed,
        args: &[String],
        err_out: &mut String,
        usage: &dyn Fn(&mut String),
    ) -> Result<Vec<String>, ParseError> {
        let mut pos = Vec::new();
        let mut args = args.to_vec();
        loop {
            let rest = self.parse_once(p, &args, err_out, usage)?;
            if rest.is_empty() {
                return Ok(pos);
            }
            if args.len() > rest.len() && args[args.len() - rest.len() - 1] == "--" {
                pos.extend(rest);
                return Ok(pos);
            }
            pos.push(rest[0].clone());
            args = rest[1..].to_vec();
        }
    }
}

/// Go `strconv.ParseBool`.
fn go_parse_bool(s: &str) -> Option<bool> {
    match s {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
        _ => None,
    }
}

/// Go `strconv.ParseInt(s, 0, 64)`: 부호, 0x·0o·0b·0 접두, 접두 뒤 밑줄을 받는다. 오류 글은 flag 가 바꿔 쓰는 꼴이다.
fn go_parse_int(s: &str) -> Result<i64, &'static str> {
    let (neg, body) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let lower = body.to_ascii_lowercase();
    let (radix, digits, prefixed) = if let Some(d) = lower.strip_prefix("0x") {
        (16, d.to_string(), true)
    } else if let Some(d) = lower.strip_prefix("0o") {
        (8, d.to_string(), true)
    } else if let Some(d) = lower.strip_prefix("0b") {
        (2, d.to_string(), true)
    } else if lower.len() > 1 && lower.starts_with('0') {
        (8, lower[1..].to_string(), true)
    } else {
        (10, lower.clone(), false)
    };
    // 밑줄은 접두가 있을 때만, 숫자 사이에만 받는다
    if digits.contains('_') && (!prefixed || digits.starts_with('_') || digits.ends_with('_') || digits.contains("__")) {
        return Err("parse error");
    }
    let digits = digits.replace('_', "");
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
        return Err("parse error");
    }
    let mag = u64::from_str_radix(&digits, radix).map_err(|_| "value out of range")?;
    if neg {
        if mag > i64::MAX as u64 + 1 {
            return Err("value out of range");
        }
        Ok((mag as i64).wrapping_neg())
    } else {
        i64::try_from(mag).map_err(|_| "value out of range")
    }
}

/// Go `time.ParseDuration`: `[-+]` 뒤에 (숫자[.숫자] 단위) 묶음. 단위는 ns·us·µs·μs·ms·s·m·h. "0" 하나는 단위 없이 받는다.
/// 나노초(부호 있음)로 돌려준다. 음수를 어떻게 다룰지는 플래그마다 다르다(--timeout 은 사용법 오류, --since 는 값 오류).
pub fn parse_go_duration(s: &str) -> Option<i128> {
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
    let mut total: f64 = 0.0;
    while !rest.is_empty() {
        let num_end = rest.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(rest.len());
        let num = &rest[..num_end];
        if num.is_empty() || num == "." {
            return None;
        }
        let v: f64 = num.parse().ok()?;
        rest = &rest[num_end..];
        let unit_end = rest.find(|c: char| c.is_ascii_digit() || c == '.').unwrap_or(rest.len());
        let mult = match &rest[..unit_end] {
            "ns" => 1e-9,
            "us" | "µs" | "μs" => 1e-6,
            "ms" => 1e-3,
            "s" => 1.0,
            "m" => 60.0,
            "h" => 3600.0,
            _ => return None,
        };
        total += v * mult;
        rest = &rest[unit_end..];
    }
    let ns = (total * 1e9).round() as i128;
    Some(if neg { -ns } else { ns })
}

/// Go `time.Duration.String()`. 도움말 기본값(30s)·시간 초과 글에 쓴다.
pub fn go_duration_string(d: Duration) -> String {
    let ns = d.as_nanos();
    if ns == 0 {
        return "0s".into();
    }
    if ns < 1_000_000_000 {
        let (unit, div) = if ns < 1_000 {
            ("ns", 1u128)
        } else if ns < 1_000_000 {
            ("µs", 1_000)
        } else {
            ("ms", 1_000_000)
        };
        return format!("{}{unit}", frac(ns, div));
    }
    let total_s = ns / 1_000_000_000;
    let (h, m) = (total_s / 3600, (total_s / 60) % 60);
    let s = frac(ns % 60_000_000_000, 1_000_000_000);
    let mut out = String::new();
    if h > 0 {
        out.push_str(&format!("{h}h"));
    }
    if h > 0 || m > 0 {
        out.push_str(&format!("{m}m"));
    }
    out.push_str(&format!("{s}s"));
    out
}

/// v/div 를 끝 0 없는 소수로 쓴다.
fn frac(v: u128, div: u128) -> String {
    let whole = v / div;
    let rem = v % div;
    if rem == 0 {
        return whole.to_string();
    }
    let width = div.to_string().len() - 1;
    let f = format!("{rem:0width$}");
    format!("{whole}.{}", f.trim_end_matches('0'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_기간_글() {
        assert_eq!(go_duration_string(Duration::from_secs(30)), "30s");
        assert_eq!(go_duration_string(Duration::from_secs(3600)), "1h0m0s");
        assert_eq!(go_duration_string(Duration::from_millis(1500)), "1.5s");
        assert_eq!(go_duration_string(Duration::from_millis(250)), "250ms");
        assert_eq!(parse_go_duration("1h30m"), Some(5_400_000_000_000));
        assert_eq!(parse_go_duration("1.5s"), Some(1_500_000_000));
        assert_eq!(parse_go_duration("10"), None);
        assert_eq!(parse_go_duration("-1s"), Some(-1_000_000_000));
    }

    #[test]
    fn go_정수_읽기() {
        assert_eq!(go_parse_int("200"), Ok(200));
        assert_eq!(go_parse_int("0x10"), Ok(16));
        assert_eq!(go_parse_int("1_000"), Err("parse error"));
        assert_eq!(go_parse_int("0x1_0"), Ok(16));
        assert_eq!(go_parse_int("abc"), Err("parse error"));
        assert_eq!(go_parse_int("99999999999999999999"), Err("value out of range"));
    }
}
