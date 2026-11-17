//! 읽기 전용 저장소(OpenSearch·Loki·Prometheus·Grafana·DynamoDB·Athena·aws, R60~R68) 화면 규칙이다.
//! 트리는 `source_children` 로 펼칠 때 읽고, 실행 범위(Loki·Prometheus·Grafana)는 고정 단계로 고른다.

use std::collections::{BTreeSet, HashMap};

use idb_engine::Node;

/// 실행 범위 단계(R60): 이름, 초
pub const RANGES: [(&str, i64); 5] = [("15m", 900), ("1h", 3600), ("6h", 21_600), ("24h", 86_400), ("7d", 604_800)];
pub const DEFAULT_RANGE: usize = 1;

/// 범위를 쓰는 종류다.
pub fn uses_range(kind: &str) -> bool {
    matches!(kind, "loki" | "prometheus" | "grafana")
}

/// now_ns 에서 secs 앞부터 지금까지(나노초)다.
pub fn range_ns(now_ns: i64, secs: i64) -> (i64, i64) {
    (now_ns.saturating_sub(secs.saturating_mul(1_000_000_000)), now_ns)
}

/// 편집기 자리 글(종류마다 쓰는 질의 말)
pub fn placeholder(kind: &str) -> &'static str {
    match kind {
        "opensearch" => "GET index/_search 와 JSON 본문, 또는 SELECT …",
        "loki" => "LogQL (예: {app=\"api\"} |= \"error\")",
        "prometheus" | "grafana" => "PromQL (예: sum(rate(http_requests_total[5m])))",
        "dynamodb" => "PartiQL SELECT (요금이 붙는 조회는 DB_ALLOW_BILLED=1 일 때만)",
        "athena" => "SELECT … (요금이 붙는 조회는 DB_ALLOW_BILLED=1 일 때만)",
        _ => "",
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub depth: usize,
    pub node: Node,
    pub expanded: bool,
    /// 펼쳤는데 아직 읽는 중
    pub loading: bool,
}

/// 펼친 길과 읽은 자식들이다. 루트는 빈 길이다.
#[derive(Default)]
pub struct SourceTree {
    children: HashMap<String, Vec<Node>>,
    expanded: BTreeSet<String>,
}

impl SourceTree {
    pub fn set_children(&mut self, path: &str, nodes: Vec<Node>) {
        self.children.insert(path.to_string(), nodes);
    }

    pub fn has_children(&self, path: &str) -> bool {
        self.children.contains_key(path)
    }

    /// 펼침을 바꾼다. 펼쳤는데 자식을 아직 안 읽었으면 true(읽어야 한다).
    pub fn toggle(&mut self, path: &str) -> bool {
        if !self.expanded.remove(path) {
            self.expanded.insert(path.to_string());
            return !self.children.contains_key(path);
        }
        false
    }

    /// 다시 읽기: 그 길 아래 읽은 것을 버린다.
    pub fn forget(&mut self, path: &str) {
        self.children.retain(|p, _| !(p == path || p.starts_with(&format!("{path}/"))));
    }

    pub fn rows(&self) -> Vec<Row> {
        let mut out = Vec::new();
        self.push_rows("", 0, &mut out);
        out
    }

    fn push_rows(&self, path: &str, depth: usize, out: &mut Vec<Row>) {
        let Some(nodes) = self.children.get(path) else { return };
        for n in nodes {
            let expanded = n.has_children && self.expanded.contains(&n.path);
            out.push(Row {
                depth,
                node: n.clone(),
                expanded,
                loading: expanded && !self.children.contains_key(&n.path),
            });
            if expanded && depth < 32 {
                self.push_rows(&n.path, depth + 1, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, path: &str, kids: bool) -> Node {
        Node {
            name: name.into(),
            path: path.into(),
            info: String::new(),
            has_children: kids,
            openable: !kids,
            warn: false,
        }
    }

    #[test]
    fn 펼치면_읽어야_하는지_알려주고_읽은_뒤_자식이_보인다() {
        let mut t = SourceTree::default();
        t.set_children("", vec![node("logs", "logs", true), node("x", "x", false)]);
        assert_eq!(t.rows().len(), 2);
        assert!(t.toggle("logs"), "처음 펼치면 읽는다");
        assert!(t.rows()[0].loading);
        t.set_children("logs", vec![node("a", "logs/a", false)]);
        let rows = t.rows();
        assert_eq!(
            rows.iter().map(|r| (r.depth, r.node.name.as_str())).collect::<Vec<_>>(),
            vec![(0, "logs"), (1, "a"), (0, "x")]
        );
        assert!(!t.toggle("logs"), "접기");
        assert_eq!(t.rows().len(), 2);
        assert!(!t.toggle("logs"), "이미 읽었으면 다시 읽지 않는다");
        t.forget("logs");
        assert!(!t.has_children("logs"));
        assert!(t.rows()[0].loading);
    }

    #[test]
    fn 범위는_지금부터_거꾸로_잰다() {
        assert_eq!(range_ns(10_000_000_000, 3), (7_000_000_000, 10_000_000_000));
        assert_eq!(RANGES[DEFAULT_RANGE].0, "1h");
        assert!(uses_range("loki") && !uses_range("dynamodb"));
        assert!(!placeholder("athena").is_empty());
    }
}
