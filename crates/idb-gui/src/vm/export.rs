//! 전체 내보내기(R33.1)와 복호화 보기(R37)의 화면 쪽 규칙이다.

use std::collections::HashMap;

use idb_core::sqlsafe::quote_ident;
use idb_core::statcrypt::looks_encrypted;

/// 테이블 보기의 전체 내보내기 문장이다(WHERE·정렬 그대로, 행 제한 없음).
pub fn table_sql(schema: &str, table: &str, filter: &str, order: &str) -> String {
    let mut s = format!("SELECT * FROM {}.{}", quote_ident(schema), quote_ident(table));
    if !filter.trim().is_empty() {
        s.push_str(&format!(" WHERE {}", filter.trim()));
    }
    if !order.trim().is_empty() {
        s.push_str(&format!(" ORDER BY {}", order.trim()));
    }
    s
}

pub fn default_name(conn: &str, now: chrono::DateTime<chrono::Local>) -> String {
    let c = idb_core::mapping::path_safe(conn);
    format!("{}-{}.csv", if c.is_empty() { "export" } else { &c }, now.format("%Y%m%d-%H%M%S"))
}

/// 진행 글이다(받은 바이트, 끝나면 행 수).
pub fn progress(bytes: u64, rows: Option<i64>) -> String {
    let size = if bytes >= 1 << 30 {
        format!("{:.1}GiB", bytes as f64 / (1u64 << 30) as f64)
    } else if bytes >= 1 << 20 {
        format!("{:.1}MiB", bytes as f64 / (1u64 << 20) as f64)
    } else if bytes >= 1 << 10 {
        format!("{:.1}KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes}B")
    };
    match rows {
        Some(n) => format!("{n}행 · {size}"),
        None => format!("{size} 받는 중"),
    }
}

/// 복호화할 칸이다. 암호문 꼴만, 같은 글은 한 번, 상한까지.
pub fn cipher_cells<'a>(cells: impl Iterator<Item = &'a str>, max: usize) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for c in cells {
        if out.len() >= max {
            break;
        }
        if looks_encrypted(c) && seen.insert(c) {
            out.push(c.to_string());
        }
    }
    out
}

/// 보낸 칸과 받은 값으로 암호문 → 평문 표를 만든다. 풀지 못한 칸(None)은 넣지 않는다.
pub fn overlay(cells: &[String], values: &[Option<String>]) -> HashMap<String, String> {
    cells
        .iter()
        .zip(values)
        .filter_map(|(c, v)| v.as_ref().map(|v| (c.clone(), v.clone())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENC: &str = "000102030405060708090a0b0c0d0e0f^|^00112233445566778899aabbccddeeff";

    #[test]
    fn 테이블_내보내기_문장은_WHERE_정렬을_그대로_싣는다() {
        assert_eq!(table_sql("public", "users", "", ""), "SELECT * FROM \"public\".\"users\"");
        assert_eq!(
            table_sql("public", "users", " id > 1 ", "id desc"),
            "SELECT * FROM \"public\".\"users\" WHERE id > 1 ORDER BY id desc"
        );
    }

    #[test]
    fn 진행_글은_크기_단위와_행_수를_보인다() {
        assert_eq!(progress(512, None), "512B 받는 중");
        assert_eq!(progress(2048, None), "2.0KiB 받는 중");
        assert_eq!(progress(3 << 20, Some(10)), "10행 · 3.0MiB");
    }

    #[test]
    fn 파일_이름은_접속과_시각이다() {
        let now = chrono::Local::now();
        let n = default_name("app/prod", now);
        assert!(n.starts_with("app-prod-") && n.ends_with(".csv"), "{n}");
    }

    #[test]
    fn 암호문_꼴_칸만_한_번씩_고른다() {
        let cells = ["plain", ENC, ENC, "NULL"];
        assert_eq!(cipher_cells(cells.into_iter(), 10), vec![ENC.to_string()]);
        assert!(cipher_cells([ENC].into_iter(), 0).is_empty());
    }

    #[test]
    fn 풀지_못한_칸은_덮지_않는다() {
        let cells = vec!["a".to_string(), "b".to_string()];
        let m = overlay(&cells, &[Some("A".into()), None]);
        assert_eq!(m.get("a").map(String::as_str), Some("A"));
        assert!(!m.contains_key("b"));
    }
}
