//! SQL 자동 완성 후보. 카탈로그(`catalog` op)만 보고 서버에 다시 묻지 않는다.
//! `스키마.` 뒤는 테이블, `테이블.`·`별칭.` 뒤는 열, 그 밖은 문장에 나온 테이블의 열·테이블·함수·키워드 순이다.

use std::ops::Range;

use idb_core::sqlsafe::statement_at;
use idb_engine::Catalog;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Column,
    Table,
    View,
    Schema,
    Function,
    Keyword,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub label: String,
    pub kind: Kind,
    pub detail: String,
}

const MAX: usize = 200;

const KEYWORDS: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "AND",
    "OR",
    "NOT",
    "NULL",
    "IS",
    "IN",
    "LIKE",
    "ILIKE",
    "BETWEEN",
    "EXISTS",
    "JOIN",
    "LEFT",
    "RIGHT",
    "INNER",
    "OUTER",
    "FULL",
    "CROSS",
    "ON",
    "USING",
    "GROUP",
    "BY",
    "ORDER",
    "HAVING",
    "LIMIT",
    "OFFSET",
    "ASC",
    "DESC",
    "DISTINCT",
    "AS",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "UNION",
    "ALL",
    "INSERT",
    "INTO",
    "VALUES",
    "UPDATE",
    "SET",
    "DELETE",
    "RETURNING",
    "WITH",
    "COUNT",
    "SUM",
    "AVG",
    "MIN",
    "MAX",
    "COALESCE",
    "NOW",
    "INTERVAL",
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    "EXPLAIN",
    "ANALYZE",
    "TRUE",
    "FALSE",
    "CREATE",
    "TABLE",
    "INDEX",
    "ALTER",
    "DROP",
    "TRUNCATE",
];

/// 바꿀 범위(커서 앞 낱말)와 후보다. offset 은 바이트다.
pub fn complete(cat: &Catalog, text: &str, offset: usize) -> (Range<usize>, Vec<Candidate>) {
    let offset = floor_char(text, offset.min(text.len()));
    let start = text[..offset]
        .char_indices()
        .rev()
        .take_while(|&(_, c)| is_ident(c))
        .last()
        .map_or(offset, |(i, _)| i);
    let prefix = text[start..offset].trim_matches('"').to_lowercase();
    let qualifier = text[..start]
        .strip_suffix('.')
        .map(|before| {
            let q = before
                .char_indices()
                .rev()
                .take_while(|&(_, c)| is_ident(c))
                .last()
                .map_or(before.len(), |(i, _)| i);
            before[q..].trim_matches('"').to_string()
        })
        .filter(|q| !q.is_empty());

    let stmt = statement_at(text, offset);
    let refs = table_refs(stmt);
    let mut out = Vec::new();
    let push = |out: &mut Vec<Candidate>, label: &str, kind: Kind, detail: String| {
        if label.to_lowercase().starts_with(&prefix) && !out.iter().any(|c: &Candidate| c.label == label && c.kind == kind) {
            out.push(Candidate {
                label: label.to_string(),
                kind,
                detail,
            });
        }
    };

    if let Some(q) = qualifier {
        let ql = q.to_lowercase();
        if let Some(s) = cat.schemas.iter().find(|s| s.name.to_lowercase() == ql) {
            for t in &s.tables {
                push(&mut out, &t.name, if t.view { Kind::View } else { Kind::Table }, s.name.clone());
            }
        }
        // 별칭이면 원래 테이블로 바꾼다
        let target = refs
            .iter()
            .find(|r| r.alias.as_deref().map(str::to_lowercase) == Some(ql.clone()))
            .map_or(ql, |r| r.table.to_lowercase());
        for (schema, t) in cat.schemas.iter().flat_map(|s| s.tables.iter().map(move |t| (&s.name, t))) {
            if t.name.to_lowercase() == target {
                for c in &t.columns {
                    push(&mut out, &c.name, Kind::Column, format!("{schema}.{} · {}", t.name, c.typ));
                }
            }
        }
        out.truncate(MAX);
        return (start..offset, out);
    }

    for r in &refs {
        for s in &cat.schemas {
            if r.schema.as_ref().is_some_and(|rs| !rs.eq_ignore_ascii_case(&s.name)) {
                continue;
            }
            for t in s.tables.iter().filter(|t| t.name.eq_ignore_ascii_case(&r.table)) {
                for c in &t.columns {
                    push(&mut out, &c.name, Kind::Column, format!("{} · {}", t.name, c.typ));
                }
            }
        }
    }
    for s in &cat.schemas {
        for t in &s.tables {
            push(&mut out, &t.name, if t.view { Kind::View } else { Kind::Table }, s.name.clone());
        }
    }
    for s in &cat.schemas {
        push(&mut out, &s.name, Kind::Schema, "schema".into());
    }
    for f in &cat.functions {
        push(&mut out, f, Kind::Function, "function".into());
    }
    // 빈 앞글자에 키워드까지 쏟지 않는다
    if !prefix.is_empty() {
        for k in KEYWORDS {
            push(&mut out, k, Kind::Keyword, String::new());
        }
    }
    out.truncate(MAX);
    (start..offset, out)
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$' || c == '"'
}

fn floor_char(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[derive(Debug, PartialEq, Eq)]
struct TableRef {
    schema: Option<String>,
    table: String,
    alias: Option<String>,
}

const NOT_ALIAS: &[&str] = &[
    "where",
    "join",
    "left",
    "right",
    "inner",
    "outer",
    "full",
    "cross",
    "on",
    "using",
    "group",
    "order",
    "limit",
    "offset",
    "set",
    "values",
    "union",
    "having",
    "returning",
    "natural",
    "lateral",
    "window",
];

/// FROM·JOIN·UPDATE·INTO 뒤의 `[스키마.]테이블 [AS] 별칭` 을 찾는다. 따옴표·괄호 안은 대충 넘긴다(후보용이라 틀려도 된다).
fn table_refs(stmt: &str) -> Vec<TableRef> {
    let words: Vec<&str> = stmt
        .split(|c: char| c.is_whitespace() || c == ',' || c == '(' || c == ')' || c == ';')
        .filter(|w| !w.is_empty())
        .collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < words.len() {
        let w = words[i].to_lowercase();
        if matches!(w.as_str(), "from" | "join" | "update" | "into") && i + 1 < words.len() {
            let name = words[i + 1].replace('"', "");
            let (schema, table) = match name.split_once('.') {
                Some((s, t)) => (Some(s.to_string()), t.to_string()),
                None => (None, name),
            };
            let mut j = i + 2;
            if words.get(j).is_some_and(|w| w.eq_ignore_ascii_case("as")) {
                j += 1;
            }
            let alias = words
                .get(j)
                .filter(|a| !NOT_ALIAS.contains(&a.to_lowercase().as_str()) && a.chars().all(|c| c.is_alphanumeric() || c == '_'))
                .map(|a| a.to_string());
            if !table.is_empty() && table.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_') {
                out.push(TableRef { schema, table, alias });
            }
            i += 2;
            continue;
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cat() -> Catalog {
        serde_json::from_value(serde_json::json!({
            "schemas": [
                {"name": "public", "tables": [
                    {"name": "users", "view": false, "columns": [{"name": "id", "type": "bigint"}, {"name": "email", "type": "text"}]},
                    {"name": "orders", "view": false, "columns": [{"name": "id", "type": "bigint"}, {"name": "user_id", "type": "bigint"}]},
                ]},
                {"name": "audit", "tables": [{"name": "log_view", "view": true, "columns": []}]},
            ],
            "functions": ["uuid_generate_v4"],
        }))
        .unwrap()
    }

    fn labels(v: &[Candidate]) -> Vec<&str> {
        v.iter().map(|c| c.label.as_str()).collect()
    }

    #[test]
    fn 스키마_점_뒤는_그_스키마의_테이블이다() {
        let text = "select * from audit.lo";
        let (r, c) = complete(&cat(), text, text.len());
        assert_eq!(&text[r], "lo");
        assert_eq!(labels(&c), vec!["log_view"]);
        assert_eq!(c[0].kind, Kind::View);
    }

    #[test]
    fn 별칭_점_뒤는_원래_테이블의_열이다() {
        let text = "select o. from orders o join users u on u.id = o.user_id";
        let (r, c) = complete(&cat(), text, 9);
        assert_eq!(r, 9..9);
        assert_eq!(labels(&c), vec!["id", "user_id"]);
        let text = "select u.e from orders o join users as u on true";
        let (_, c) = complete(&cat(), text, 10);
        assert_eq!(labels(&c), vec!["email"]);
    }

    #[test]
    fn 앞글자만_있으면_문장의_테이블_열이_먼저다() {
        let text = "select us from orders";
        let (r, c) = complete(&cat(), text, 9);
        assert_eq!(r, 7..9);
        assert_eq!(c[0].label, "user_id");
        assert_eq!(c[0].kind, Kind::Column);
        assert!(labels(&c).contains(&"users"));
        assert!(!c.iter().any(|x| x.kind == Kind::Keyword && x.label == "SELECT"));
        let (_, c) = complete(&cat(), "sel", 3);
        assert_eq!(labels(&c), vec!["SELECT"]);
    }

    #[test]
    fn 다른_문장의_테이블은_보지_않는다() {
        let text = "select * from users;\nselect i from orders";
        let off = text.rfind(" i ").unwrap() + 2;
        let (_, c) = complete(&cat(), text, off);
        let cols: Vec<&str> = c.iter().filter(|x| x.kind == Kind::Column).map(|x| x.detail.as_str()).collect();
        assert_eq!(cols, vec!["orders · bigint"]);
    }

    #[test]
    fn 한글_경계에서도_넘어지지_않는다() {
        let text = "select 이름";
        let (r, _) = complete(&cat(), text, text.len() - 1);
        assert!(r.end <= text.len() && text.is_char_boundary(r.start) && text.is_char_boundary(r.end));
    }

    #[test]
    fn 테이블_참조는_스키마와_별칭을_가른다() {
        let refs = table_refs("select * from public.users u left join orders on true where x");
        assert_eq!(
            refs,
            vec![
                TableRef {
                    schema: Some("public".into()),
                    table: "users".into(),
                    alias: Some("u".into())
                },
                TableRef {
                    schema: None,
                    table: "orders".into(),
                    alias: None
                },
            ]
        );
    }
}
