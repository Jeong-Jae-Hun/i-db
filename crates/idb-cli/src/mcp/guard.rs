//! redis_command 의 추가 제한. Go `internal/mcpserver/guard.go` 와 같다.

use idb_core::textutil::go_upper;

/// SCAN 계열 COUNT·MEMORY USAGE SAMPLES 의 상한이다. 한 번에 서버가 훑는 양을 묶는다.
const MAX_REDIS_COUNT: i64 = 1000;

fn denied(cmd: &str) -> Option<String> {
    let snoop = |c: &str| format!("{c} is not allowed over MCP: it exposes other clients' addresses and commands");
    let whole = |c: &str| format!("{c} returns the whole collection; use redis_get or the matching SSCAN/HSCAN with COUNT");
    Some(match cmd {
        "KEYS" => "KEYS blocks the server; use redis_scan instead".into(),
        "CLIENT" | "SLOWLOG" | "MONITOR" => snoop(cmd),
        "SMEMBERS" | "HGETALL" | "HKEYS" | "HVALS" => whole(cmd),
        _ => return None,
    })
}

/// Redis 명령 옵션 하나의 정수 범위다. name 뒤 offset 번째 값을 본다(LIMIT offset count 의 count 는 2).
struct OptRule {
    name: &'static str,
    offset: usize,
    min: i64,
    max: i64,
    required: bool,
}

/// is_read_only_redis 를 통과한 명령 중 다른 클라이언트 정보를 드러내거나 응답·서버 작업량에 상한이 없는 것을 거부한다.
/// TUI 는 사람이 보고 실행하므로 이 제한은 MCP 에만 둔다. args 는 비어 있지 않다.
pub(crate) fn check_redis_args(args: &[String], max_rows: usize) -> Result<(), String> {
    let cmd = go_upper(&args[0]);
    if let Some(msg) = denied(&cmd) {
        return Err(msg);
    }
    let max_rows = max_rows as i64;
    let from = |i: usize| &args[i.min(args.len())..];
    let scan_count = OptRule {
        name: "COUNT",
        offset: 1,
        min: 1,
        max: MAX_REDIS_COUNT,
        required: false,
    };
    let limit = |opts: &[String]| {
        check_option(
            opts,
            &OptRule {
                name: "LIMIT",
                offset: 2,
                min: 0,
                max: max_rows,
                required: true,
            },
        )
    };
    match cmd.as_str() {
        "SCAN" => check_option(from(2), &scan_count),
        "SSCAN" | "HSCAN" | "ZSCAN" => check_option(from(3), &scan_count),
        "XRANGE" | "XREVRANGE" => check_option(
            from(4),
            &OptRule {
                name: "COUNT",
                offset: 1,
                min: 1,
                max: max_rows,
                required: true,
            },
        ),
        "MEMORY" if args.len() > 1 && go_upper(&args[1]) == "USAGE" => check_option(
            from(3),
            &OptRule {
                name: "SAMPLES",
                offset: 1,
                min: 1,
                max: MAX_REDIS_COUNT,
                required: false,
            },
        ),
        "ZRANGEBYSCORE" => limit(from(4)),
        "LRANGE" | "ZRANGE" | "ZREVRANGE" => {
            if args.len() < 4 {
                return Err(format!("{cmd} needs key, start and stop"));
            }
            if cmd == "ZRANGE" && args[4..].iter().any(|a| matches!(go_upper(a).as_str(), "BYSCORE" | "BYLEX")) {
                return limit(&args[4..]);
            }
            let (start, stop) = (args[2].parse::<i64>(), args[3].parse::<i64>());
            // 부호가 다르면(0 -1 처럼) 길이에 따라 전체가 된다. 같은 부호끼리는 뺄셈이 넘치지 않는다
            match (start, stop) {
                (Ok(start), Ok(stop)) if (start < 0) == (stop < 0) && stop - start < max_rows => Ok(()),
                _ => Err(format!(
                    "{cmd} range must be integer indices of the same sign spanning at most {max_rows} elements"
                )),
            }
        }
        _ => Ok(()),
    }
}

/// opts 에 나오는 r.name 마다 값을 확인한다. Redis 는 같은 옵션이 반복되면 마지막 값을 쓰므로 전부 본다.
/// 키·패턴이 옵션 이름과 같아도 값이 정수가 아니면 거부한다(안전 쪽).
fn check_option(opts: &[String], r: &OptRule) -> Result<(), String> {
    let mut found = false;
    for (i, a) in opts.iter().enumerate() {
        if go_upper(a) != r.name {
            continue;
        }
        found = true;
        let Some(v) = opts.get(i + r.offset) else {
            return Err(format!("{} needs a value", r.name));
        };
        match v.parse::<i64>() {
            Ok(n) if (r.min..=r.max).contains(&n) => {}
            _ => return Err(format!("{} must be an integer between {} and {}", r.name, r.min, r.max)),
        }
    }
    if r.required && !found {
        return Err(format!("{} is required (at most {}) to bound the reply", r.name, r.max));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn 정보_노출과_상한_없는_읽기를_거부한다() {
        let rejected: &[&[&str]] = &[
            &["CLIENT", "LIST"],
            &["slowlog", "get"],
            &["MONITOR"],
            &["KEYS", "*"],
            &["SMEMBERS", "s"],
            &["HGETALL", "h"],
            &["hkeys", "h"],
            &["HVALS", "h"],
            &["LRANGE", "l", "0", "-1"],
            &["LRANGE", "l", "0", "200"],
            &["LRANGE", "l", "a", "b"],
            &["LRANGE", "l"],
            &["ZRANGE", "z", "0", "-1"],
            &["ZREVRANGE", "z", "-1000", "-1"],
            &["LRANGE", "l", "0", "9223372036854775807"],
            &["ZRANGE", "z", "-inf", "+inf", "BYSCORE"],
            &["ZRANGE", "z", "-inf", "+inf", "BYSCORE", "LIMIT", "0", "-1"],
            &["ZRANGEBYSCORE", "z", "-inf", "+inf"],
            &["XRANGE", "x", "-", "+"],
            &["XREVRANGE", "x", "+", "-", "COUNT", "100000"],
            &["SCAN", "0", "COUNT", "100000"],
            &["HSCAN", "h", "0", "COUNT", "10", "COUNT", "5000"],
            &["SSCAN", "s", "0", "COUNT"],
            &["MEMORY", "USAGE", "k", "SAMPLES", "0"],
        ];
        for a in rejected {
            assert!(check_redis_args(&v(a), 200).is_err(), "거부해야 한다: {a:?}");
        }
        let allowed: &[&[&str]] = &[
            &["GET", "k"],
            &["LRANGE", "l", "0", "199"],
            &["LRANGE", "l", "-10", "-1"],
            &["ZRANGE", "z", "0", "9", "WITHSCORES"],
            &["ZRANGE", "z", "-inf", "+inf", "BYSCORE", "LIMIT", "0", "50"],
            &["ZRANGEBYSCORE", "z", "0", "10", "LIMIT", "5", "10"],
            &["XRANGE", "x", "-", "+", "COUNT", "10"],
            &["SCAN", "0", "MATCH", "a:*", "COUNT", "1000"],
            &["ZSCAN", "z", "0"],
            &["MEMORY", "USAGE", "k"],
            &["MEMORY", "USAGE", "k", "SAMPLES", "5"],
            &["MEMORY", "STATS"],
        ];
        for a in allowed {
            assert_eq!(check_redis_args(&v(a), 200), Ok(()), "통과해야 한다: {a:?}");
        }
    }
}
