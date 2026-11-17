//! 결과 행을 SQL 로 복사한다(INSERT 문, IN 목록). 값은 모두 리터럴로 감싸 PostgreSQL 이 열 타입으로 바꾸게 한다.

use idb_core::sqlsafe::{quote_ident, quote_literal};

use super::grid::NULL;

fn literal(v: &str) -> String {
    if v == NULL { "NULL".into() } else { quote_literal(v) }
}

/// 행마다 INSERT 한 줄이다. table 은 `스키마.테이블`(없으면 자리표시 `table_name`).
pub fn insert_sql(table: Option<(&str, &str)>, cols: &[String], rows: &[Vec<String>]) -> String {
    let target = match table {
        Some((s, t)) => format!("{}.{}", quote_ident(s), quote_ident(t)),
        None => "table_name".into(),
    };
    let cols_sql: Vec<String> = cols.iter().map(|c| quote_ident(c)).collect();
    rows.iter()
        .map(|r| {
            let vals: Vec<String> = r.iter().map(|v| literal(v)).collect();
            format!("INSERT INTO {target} ({}) VALUES ({});", cols_sql.join(", "), vals.join(", "))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `('a', 'b', NULL)` 꼴이다. 같은 값은 한 번만 둔다.
pub fn in_list(values: &[&str]) -> String {
    let mut seen: Vec<&str> = Vec::new();
    for v in values {
        if !seen.contains(v) {
            seen.push(v);
        }
    }
    format!("({})", seen.iter().map(|v| literal(v)).collect::<Vec<_>>().join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn INSERT_는_이름과_값을_감싸고_NULL_은_그대로다() {
        let cols = vec!["id".to_string(), "Name".to_string()];
        let rows = vec![
            vec!["1".to_string(), "o'neil".to_string()],
            vec!["2".to_string(), "NULL".to_string()],
        ];
        assert_eq!(
            insert_sql(Some(("public", "users")), &cols, &rows),
            "INSERT INTO \"public\".\"users\" (\"id\", \"Name\") VALUES ('1', 'o''neil');\nINSERT INTO \"public\".\"users\" (\"id\", \"Name\") VALUES ('2', NULL);"
        );
        assert!(insert_sql(None, &cols, &rows[..1]).starts_with("INSERT INTO table_name "));
    }

    #[test]
    fn IN_목록은_중복을_빼고_감싼다() {
        assert_eq!(in_list(&["1", "2", "1", "NULL"]), "('1', '2', NULL)");
    }
}
