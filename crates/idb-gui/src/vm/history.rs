//! 입력 기록(테이블 보기 WHERE·ORDER BY, R75.5). 테이블마다 최근 것이 앞이고 같은 글은 한 번만 둔다.
//! prod 접속의 조건은 남기지도 보이지도 않는다.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const MAX: usize = 20;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableHistory {
    #[serde(default, rename = "where")]
    pub where_: Vec<String>,
    #[serde(default)]
    pub order: Vec<String>,
}

/// 접속 열쇠(uuid, 옛 serve 면 이름)와 스키마·테이블로 기록 자리를 정한다.
pub fn table_key(conn_key: &str, schema: &str, table: &str) -> String {
    format!("{conn_key}\t{schema}.{table}")
}

/// 적용한 WHERE·ORDER BY 를 남긴다. prod 면 남기지 않는다.
pub fn record(all: &mut BTreeMap<String, TableHistory>, key: &str, prod: bool, w: &str, o: &str) {
    if prod {
        return;
    }
    let h = all.entry(key.to_string()).or_default();
    push(&mut h.where_, w);
    push(&mut h.order, o);
    if h.where_.is_empty() && h.order.is_empty() {
        all.remove(key);
    }
}

/// 보일 기록이다. prod 면 비어 있다.
pub fn shown(all: &BTreeMap<String, TableHistory>, key: &str, prod: bool) -> TableHistory {
    if prod {
        TableHistory::default()
    } else {
        all.get(key).cloned().unwrap_or_default()
    }
}

/// 남아 있는 prod 접속 기록을 지운다(접속 목록을 받을 때 부른다).
pub fn purge_prod(all: &mut BTreeMap<String, TableHistory>, prod_conn_keys: &[String]) -> bool {
    let before = all.len();
    all.retain(|k, _| !prod_conn_keys.iter().any(|c| k.starts_with(&format!("{c}\t"))));
    all.len() != before
}

/// 앞에 넣는다. 빈 글은 넣지 않는다.
pub fn push(items: &mut Vec<String>, s: &str) {
    let s = s.trim();
    if s.is_empty() {
        return;
    }
    items.retain(|x| x != s);
    items.insert(0, s.to_string());
    items.truncate(MAX);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prod_기록은_남기지도_보이지도_않고_남은_것은_지운다() {
        let mut all = BTreeMap::new();
        let k = table_key("u-prod", "public", "users");
        record(&mut all, &k, true, "email = 'a@b.com'", "");
        assert!(all.is_empty());
        let s = table_key("u-stg", "public", "users");
        record(&mut all, &s, false, "id > 1", "id desc");
        assert_eq!(shown(&all, &s, false).where_, vec!["id > 1"]);
        assert!(shown(&all, &s, true).where_.is_empty());
        // 옛 판이 남긴 prod 기록
        all.insert(
            k.clone(),
            TableHistory {
                where_: vec!["x".into()],
                order: vec![],
            },
        );
        assert!(purge_prod(&mut all, &["u-prod".to_string()]));
        assert!(!all.contains_key(&k) && all.contains_key(&s));
    }

    #[test]
    fn 최근_것이_앞이고_같은_글은_한_번이다() {
        let mut h = Vec::new();
        push(&mut h, "id > 1");
        push(&mut h, " name = 'a' ");
        push(&mut h, "id > 1");
        push(&mut h, "   ");
        assert_eq!(h, vec!["id > 1", "name = 'a'"]);
        for i in 0..30 {
            push(&mut h, &format!("x = {i}"));
        }
        assert_eq!(h.len(), MAX);
        assert_eq!(h[0], "x = 29");
    }
}
