//! 확인을 받아야 할 문장(R25)과 첫 단어 판정이 놓치는 쓰기(R58)를 찾는다. Go `internal/db/danger.go` 와 같다.

use super::readonly::blocked_call;
use super::{go_lower, go_upper, is_ident_byte, mask_bytes};

/// 환경과 상관없이 확인을 받아야 할 문장이면 이유를, 아니면 빈 글을 돌려준다(명세 R25).
/// 문장이 여럿이면 문장마다 보고 서로 다른 이유를 ", " 로 잇는다.
/// WHERE 는 괄호 밖 최상위의 것만 센다. 하위 쿼리 안의 where 는 지울 행을 좁히지 않아서다.
/// 단, with … delete(CTE 안 쓰기)·함수 안 쓰기·explain analyze delete 는 첫 단어로 보므로 잡지 못한다(R6.1 과 같은 한계).
pub fn danger_of(stmt: &str) -> String {
    let mut reasons: Vec<String> = Vec::new();
    for words in top_level_words(stmt) {
        let top = &words.top;
        let r = match words.first.as_str() {
            "delete" | "update" if !top.iter().any(|w| w == "where") => format!("WHERE 없는 {}", go_upper(&words.first)),
            "truncate" => "TRUNCATE".into(),
            "drop" => "DROP".into(),
            // ALTER [COLUMN] c DROP NOT NULL·DEFAULT·IDENTITY·EXPRESSION 은 제약·기본값만 푼다. 열·제약을 지우는 DROP 만 잡는다.
            // 열 하위 절(ALTER [COLUMN] c)이나 ALTER DOMAIN d 바로 뒤일 때만 봐준다. ALTER TABLE t DROP identity 는 COLUMN 을 뺀 열 지우기라서다
            "alter" => {
                let drops = top.iter().enumerate().any(|(i, w)| {
                    let in_column = i >= 2 && top[i - 2] == "alter"
                        || i >= 3 && top[i - 3] == "alter" && top[i - 2] == "column"
                        || i == 3 && top[1] == "domain";
                    w == "drop" && (i + 1 == top.len() || !in_column || !ALTER_DROP_KEEPS.contains(&top[i + 1].as_str()))
                });
                if drops { "DROP".into() } else { String::new() }
            }
            _ => String::new(),
        };
        if !r.is_empty() && !reasons.contains(&r) {
            reasons.push(r);
        }
    }
    reasons.join(", ")
}

/// 첫 단어 판정(`danger_of`·`is_read_only_sql`)이 놓치는 쓰기·부수효과의 이유를 ", " 로 이어 돌려준다.
/// CLI query --write(R58)가 --force 를 요구하는 데 쓴다. CTE 는 괄호 깊이와 상관없이 쓰기 단어를 찾는다.
/// 한계: 함수 안의 쓰기와 이름을 모르는 부수효과 함수는 잡지 못한다.
pub fn hidden_write_of(stmt: &str) -> String {
    let mut reasons: Vec<String> = Vec::new();
    for w in top_level_words(stmt) {
        if w.first == "with" && has_data_write(&w.all) {
            reasons.push("CTE 안 쓰기".into());
        }
        if (w.first == "select" || w.first == "with") && main_verb(&w.top) == "select" && w.top.iter().any(|t| t == "into") {
            reasons.push("SELECT INTO".into());
        }
    }
    match blocked_call(stmt) {
        Err(_) => reasons.push("U&\"…\" 식별자".into()),
        Ok(Some(name)) => reasons.push(format!("부수효과 함수 {name}")),
        Ok(None) => {}
    }
    // 문장이 여럿이면 같은 이유가 겹친다
    reasons.sort();
    reasons.dedup();
    reasons.join(", ")
}

/// 단어들에 insert·update·delete·merge 가 있는지 본다. select … for [no key] update 의 update 는 잠금이라 뺀다.
fn has_data_write(words: &[String]) -> bool {
    words.iter().enumerate().any(|(i, w)| match w.as_str() {
        "insert" | "delete" | "merge" => true,
        "update" => i == 0 || words[i - 1] != "for" && words[i - 1] != "key",
        _ => false,
    })
}

/// with 절 뒤 본 문장의 동사다(괄호 밖 첫 select·insert·update·delete·merge).
fn main_verb(top: &[String]) -> &str {
    top.iter()
        .find(|w| matches!(w.as_str(), "select" | "insert" | "update" | "delete" | "merge"))
        .map_or("", String::as_str)
}

/// ALTER 의 DROP 뒤에 오면 데이터를 지우지 않는 단어다.
const ALTER_DROP_KEEPS: &[&str] = &["not", "default", "identity", "expression"];

/// FLUSHDB·FLUSHALL 이면 그 이름을 돌려준다.
pub fn danger_of_redis<S: AsRef<str>>(args: &[S]) -> String {
    let Some(first) = args.first() else {
        return String::new();
    };
    let cmd = go_upper(first.as_ref());
    if cmd == "FLUSHDB" || cmd == "FLUSHALL" {
        cmd
    } else {
        String::new()
    }
}

/// 문장 하나의 첫 단어(괄호 깊이와 상관없이)와 괄호 밖 단어들, 모든 단어다. 모두 소문자다.
#[derive(Default)]
struct StmtWords {
    first: String,
    top: Vec<String>,
    all: Vec<String>,
}

/// 문자열·주석·"따옴표 이름"을 가린 뒤 괄호 밖 ';' 마다 문장을 나눠 단어를 모은다.
fn top_level_words(text: &str) -> Vec<StmtWords> {
    let m = mask_bytes(text.as_bytes()).0;
    let mut out = Vec::new();
    let mut cur = StmtWords::default();
    let mut depth = 0usize;
    let mut i = 0;
    while i < m.len() {
        let c = m[i];
        match c {
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            b';' if depth == 0 => out.push(std::mem::take(&mut cur)),
            _ if is_ident_byte(c) && c != b'$' && !c.is_ascii_digit() => {
                let j = i + m[i..].iter().take_while(|&&b| is_ident_byte(b)).count();
                let w = go_lower(&String::from_utf8_lossy(&m[i..j]));
                if cur.first.is_empty() {
                    cur.first = w.clone();
                }
                if depth == 0 {
                    cur.top.push(w.clone());
                }
                cur.all.push(w);
                i = j - 1;
            }
            // 숫자로 시작하는 토큰(1e5 등)은 단어가 아니다
            _ if is_ident_byte(c) => {
                while i + 1 < m.len() && is_ident_byte(m[i + 1]) {
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    out.push(cur);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 위험_문장과_이유() {
        let cases = [
            ("delete from customers;", "WHERE 없는 DELETE"),
            ("update t set a=1;", "WHERE 없는 UPDATE"),
            ("truncate t;", "TRUNCATE"),
            ("drop table t;", "DROP"),
            ("alter table t drop column c;", "DROP"),
            ("DELETE FROM t", "WHERE 없는 DELETE"),
            ("-- where 지우기\n/* where */ delete from t", "WHERE 없는 DELETE"),
            ("delete from t -- where id = 1", "WHERE 없는 DELETE"),
            ("update t set note = 'where id = 1'", "WHERE 없는 UPDATE"),
            (r#"update t set "where" = 1"#, "WHERE 없는 UPDATE"),
            ("update t set a = $$ where $$", "WHERE 없는 UPDATE"),
            ("update t set a = (select b from u where u.id = 1)", "WHERE 없는 UPDATE"),
            ("delete from t using (select id from u where x) s", "WHERE 없는 DELETE"),
            ("select 1; delete from t; truncate u", "WHERE 없는 DELETE, TRUNCATE"),
            ("drop table a; drop table b", "DROP"),
            ("alter table t drop constraint t_pk;", "DROP"),
            ("alter table t drop c", "DROP"),
            ("alter table t alter column a drop not null, drop column b", "DROP"),
            // COLUMN 을 뺀 열 지우기: 열 이름이 identity·expression 이어도 지운다
            ("alter table t drop identity", "DROP"),
            ("alter table t drop expression;", "DROP"),
            ("alter domain d drop constraint c", "DROP"),
        ];
        for (stmt, want) in cases {
            assert_eq!(danger_of(stmt), want, "{stmt:?}");
        }
    }

    #[test]
    fn 안전한_문장은_빈_글() {
        for stmt in [
            "delete from t where id = 1;",
            "update t set a = 1 from u where u.id = t.id",
            "delete from t using u where u.id = t.id",
            "update t set a = 'x' WHERE id in (select id from u)",
            "select * from t",
            "alter table t add column c int",
            "insert into t values ('drop', 'truncate')",
            "select 'delete from t'",
            "-- delete from t\nselect 1",
            "create table drop_log (id int)",
            "alter table t alter column c drop not null",
            "alter table t alter c drop default",
            "ALTER TABLE t ALTER COLUMN c DROP IDENTITY IF EXISTS",
            "alter table t alter column c drop expression",
            "alter domain d drop not null",
            "",
            // CTE 안 쓰기는 R6.1 과 같이 잡지 못한다(명세 R25.1 단서)
            "with d as (select 1) delete from t",
        ] {
            assert_eq!(danger_of(stmt), "", "{stmt:?}");
        }
    }

    #[test]
    fn FLUSH_명령만_잡는다() {
        let cases: [(&[&str], &str); 4] = [
            (&["FLUSHDB"], "FLUSHDB"),
            (&["flushall", "async"], "FLUSHALL"),
            (&["DEL", "flushdb"], ""),
            (&[], ""),
        ];
        for (args, want) in cases {
            assert_eq!(danger_of_redis(args), want, "{args:?}");
        }
    }

    #[test]
    fn CTE_쓰기_SELECT_INTO_부수효과_함수() {
        let cases = [
            ("with x as (delete from t returning id) select * from x", "CTE 안 쓰기"),
            ("WITH x AS (select 1) UPDATE t SET a = 1 FROM x WHERE t.id = 1", "CTE 안 쓰기"),
            ("select * into t2 from t", "SELECT INTO"),
            (
                "update t set a = pg_terminate_backend(1) where id = 1",
                "부수효과 함수 pg_terminate_backend",
            ),
            ("insert into t select dblink_exec('x')", "부수효과 함수 dblink_exec"),
            (
                "with x as (insert into t values (1) returning 1) select * into t3 from x",
                "CTE 안 쓰기, SELECT INTO",
            ),
        ];
        for (sql, want) in cases {
            assert_eq!(hidden_write_of(sql), want, "{sql:?}");
        }
        for sql in [
            "insert into t values (1)",
            "update t set a = 'with x as (delete' where id = 1",
            "with x as (select 1) select * from x",
            "insert into t (\"update\") values (1)",
            "select 'into' from t",
        ] {
            assert_eq!(hidden_write_of(sql), "", "{sql:?}");
        }
    }

    #[test]
    fn U앤드_식별자는_이유로_알린다() {
        assert_eq!(hidden_write_of(r#"select U&"pg_notify"('a')"#), "U&\"…\" 식별자");
    }
}
