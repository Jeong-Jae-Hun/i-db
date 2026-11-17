//! 읽기 전용 실행 전에 하는 검사다. Go `internal/db/readonly.go` 에서 접속 없는 쪽(CheckReadOnlySQL·blockedCall)과 같다.

use std::fmt;

use super::{dollar_tag, find, go_lower, is_ident_byte, is_read_only_sql};

/// 부수효과가 있는 함수다. READ ONLY 트랜잭션에서도 돈다(백엔드 종료·알림·advisory lock·파일 읽기·외부 접속 등).
/// 한계: 이름을 아는 함수만 막는다. 이 함수들을 감싼 사용자 함수·뷰, 새 확장의 함수는 통과하므로
/// 근본 대책은 MCP 전용 읽기 전용 DB 역할이다. query_to_xml·ts_stat 은 문자열로 받은 SQL 을 실행해 이 검사를 비켜 가서 막는다.
const BLOCKED_FUNCS: &[&str] = &[
    "pg_terminate_backend",
    "pg_cancel_backend",
    "pg_signal_backend",
    "pg_notify",
    "pg_reload_conf",
    "pg_rotate_logfile",
    "pg_rotate_logfile_old",
    "pg_switch_wal",
    "pg_switch_xlog",
    "pg_create_restore_point",
    "pg_logical_emit_message",
    "pg_promote",
    "pg_read_file",
    "pg_read_file_old",
    "pg_read_binary_file",
    "pg_stat_file",
    "loread",
    "lowrite",
    "set_config",
    "ts_stat",
    "cursor_to_xml",
    "cursor_to_xmlschema",
    "pg_log_backend_memory_contexts",
    "pg_import_system_collations",
    "pg_prewarm",
    "pg_truncate_visibility_map",
    "nextval",
    "setval",
];

const BLOCKED_FUNC_PREFIXES: &[&str] = &[
    "dblink",
    "postgres_fdw_",
    "pg_advisory",
    "pg_try_advisory",
    "lo_",
    "pg_ls_",
    "pg_file_",
    "pg_stat_reset",
    "pg_stat_statements_reset",
    "pg_wal_replay_",
    "pg_backup_",
    "pg_start_backup",
    "pg_stop_backup",
    "pg_create_physical_replication_slot",
    "pg_create_logical_replication_slot",
    "pg_drop_replication_slot",
    "pg_replication_slot_advance",
    "pg_logical_slot_",
    "pg_copy_",
    "pg_replication_origin_",
    "query_to_xml",
];

/// 읽기 전용 검사에 걸린 까닭이다. 글은 Go 판 오류와 바이트로 같다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadOnlyError {
    /// Go `ErrNotReadOnly`
    NotReadOnly,
    UnicodeIdent,
    BlockedFunc(String),
}

impl fmt::Display for ReadOnlyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadOnlyError::NotReadOnly => {
                f.write_str("only read-only statements are allowed (select, with, values, table, show, explain without analyze)")
            }
            ReadOnlyError::UnicodeIdent => f.write_str(r#"unicode-escaped identifiers (U&"...") are not allowed"#),
            ReadOnlyError::BlockedFunc(name) => {
                write!(
                    f,
                    "function {name} is not allowed: it has side effects even in a read-only transaction"
                )
            }
        }
    }
}

impl std::error::Error for ReadOnlyError {}

/// 읽기 전용 실행 전에 하는 검사다. 호출부는 접속을 열기 전에 같은 검사로 미리 거를 수 있다.
pub fn check_read_only_sql(sql: &str) -> Result<(), ReadOnlyError> {
    // copy ... to program·commit·set transaction read write·call·do 는 READ ONLY 트랜잭션 안에서도 돈다
    if !is_read_only_sql(sql) {
        return Err(ReadOnlyError::NotReadOnly);
    }
    match blocked_call(sql)? {
        Some(name) => Err(ReadOnlyError::BlockedFunc(name)),
        None => Ok(()),
    }
}

/// 막힌 함수 호출(식별자 뒤 '(')의 이름이다. 쓰기 경로(CLI --write, R58)도 같은 목록으로 위험을 본다.
/// 문자열·$태그$·주석은 건너뛰고, 스키마 한정(pg_catalog.x)과 "따옴표 이름"도 마지막 식별자로 대소문자 없이 본다.
/// 문자열은 standard_conforming_strings=on 규칙으로 읽는다(읽기 전용 실행이 트랜잭션에 그 값을 고정한다).
pub(crate) fn blocked_call(sql: &str) -> Result<Option<String>, ReadOnlyError> {
    let s = sql.as_bytes();
    let mut prev = String::new(); // 바로 앞 토큰이 식별자면 소문자 이름
    let mut prev_end: Option<usize> = None; // 그 식별자가 끝난 위치. E'...' 처럼 접두어가 붙었는지 본다
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if s[i..].starts_with(b"--") {
            match s[i..].iter().position(|&b| b == b'\n') {
                Some(j) => i += j + 1,
                None => return Ok(None),
            }
        } else if s[i..].starts_with(b"/*") {
            // 블록 주석은 중첩된다
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
        } else if b" \t\n\r\x0c\x0b".contains(&c) {
            i += 1;
        } else if c == b'\'' || c == b'"' {
            let escapes = c == b'\'' && prev == "e" && prev_end == Some(i);
            let mut j = i + 1;
            while j < s.len() {
                if escapes && s[j] == b'\\' {
                    j += 2;
                    continue;
                }
                if s[j] != c {
                    j += 1;
                    continue;
                }
                if j + 1 < s.len() && s[j + 1] == c {
                    // '' 이스케이프
                    j += 2;
                    continue;
                }
                break;
            }
            prev.clear();
            prev_end = None;
            if c == b'"' {
                let name = String::from_utf8_lossy(&s[i + 1..j.min(s.len())]).replace("\"\"", "\"");
                prev = go_lower(&name);
            }
            i = j + 1;
        } else if c == b'$' {
            i += 1; // $1 같은 위치 인자나 연산자의 $
            if let Some(tag) = dollar_tag(&s[i - 1..]) {
                let tl = tag.len();
                match find(&s[i - 1 + tl..], tag) {
                    Some(k) => i += 2 * tl + k - 1,
                    None => i = s.len(),
                }
            }
            prev.clear();
            prev_end = None;
        } else if is_ident_byte(c) && !c.is_ascii_digit() {
            let j = i + s[i..].iter().take_while(|&&b| is_ident_byte(b)).count();
            prev = go_lower(&String::from_utf8_lossy(&s[i..j]));
            prev_end = Some(j);
            // U&"..." 은 \XXXX 로 글자를 숨긴 식별자라 이름을 대조할 수 없다
            if prev == "u" && s[j..].starts_with(b"&\"") {
                return Err(ReadOnlyError::UnicodeIdent);
            }
            i = j;
        } else if c == b'(' {
            let blocked =
                BLOCKED_FUNCS.contains(&prev.as_str()) || !prev.is_empty() && BLOCKED_FUNC_PREFIXES.iter().any(|p| prev.starts_with(p));
            if blocked {
                return Ok(Some(prev));
            }
            prev.clear();
            prev_end = None;
            i += 1;
        } else {
            prev.clear();
            prev_end = None;
            i += 1;
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Go `TestRunReadOnly_읽기_전용이_아닌_문장은_접속_전에_거부한다` 에서 접속 없이 보는 부분이다.
    #[test]
    fn 읽기_전용이_아닌_문장은_접속_전에_거부한다() {
        for sql in [
            "copy (select 'x') to program 'touch /tmp/dbtui_pwn'",
            "commit",
            "set transaction read write",
            "call some_proc()",
            "do $$ begin perform 1; end $$",
        ] {
            assert_eq!(check_read_only_sql(sql), Err(ReadOnlyError::NotReadOnly), "{sql}");
        }
    }

    #[test]
    fn 부수효과_함수_호출을_막는다() {
        let blocked = [
            "select pg_terminate_backend(1)",
            "SELECT PG_CATALOG.PG_TERMINATE_BACKEND (1)",
            r#"select "pg_notify"('c', 'x')"#,
            "select pg_catalog./**/pg_cancel_backend/* x */(1)",
            "select dblink_exec('host=x', 'drop table t')",
            "select pg_try_advisory_lock(1)",
            "select lo_import('/etc/passwd')",
            "select * from pg_ls_dir('.')",
            "select pg_read_file('postgresql.conf')",
            "select set_config('statement_timeout', '0', true)",
            "select query_to_xml('select pg_notify(''a'', ''b'')', true, true, '')",
            r#"select U&"pg_notify"('a', 'b')"#,
            r"select e'\'' , pg_notify('a', 'b')",
            "select 1 where $$ $$ = '' or pg_notify('a', 'b') is null",
            "select postgres_fdw_disconnect_all()",
        ];
        for sql in blocked {
            assert!(check_read_only_sql(sql).is_err(), "막아야 한다: {sql}");
        }
        let allowed = [
            "select 'pg_terminate_backend(1)'",
            "select 1 -- pg_notify(1)",
            "/* pg_notify( */ select 1",
            "select $$pg_notify('a')$$",
            "select $t$ $$ pg_notify( $t$",
            r#"select "pg_notify" from t"#,
            "select lower(name), count(*) from lo_table group by 1",
            "select 'it''s pg_notify('",
            r"select e'it\'s pg_notify('",
            "select a$pg_notify(1)",
        ];
        for sql in allowed {
            assert_eq!(check_read_only_sql(sql), Ok(()), "통과해야 한다: {sql}");
        }
    }

    #[test]
    fn 오류_글은_Go_와_같다() {
        assert_eq!(
            check_read_only_sql("select pg_notify('a')").unwrap_err().to_string(),
            "function pg_notify is not allowed: it has side effects even in a read-only transaction"
        );
        assert_eq!(
            check_read_only_sql(r#"select U&"x"()"#).unwrap_err().to_string(),
            r#"unicode-escaped identifiers (U&"...") are not allowed"#
        );
        assert_eq!(
            check_read_only_sql("delete from t").unwrap_err().to_string(),
            "only read-only statements are allowed (select, with, values, table, show, explain without analyze)"
        );
    }
}
