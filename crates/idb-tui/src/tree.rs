//! 접속 트리(R2·R43·R66). 노드는 한 벡터(arena)에 두고 부모·자식은 자리로 잇는다.
//! 다시 읽으면 옛 자식 자리는 버린다(작아서 모으지 않는다).

use std::cmp::Ordering;

use idb_engine::ConnSummary;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeKind {
    Group,
    Conn(usize),
    /// 접속 아래 `쿼리 (N)` 묶음, None 은 `쿼리 (접속 없음)`
    Queries(Option<usize>),
    /// queries/ 아래 상대 경로
    File(String),
    Schema {
        conn: usize,
        name: String,
    },
    Table {
        conn: usize,
        schema: String,
        name: String,
    },
    /// Redis 이름공간(끝에 `:` 를 붙인 접두어)
    Folder {
        conn: usize,
        prefix: String,
    },
    Key {
        conn: usize,
        key: String,
    },
    Info,
}

#[derive(Clone, Debug)]
pub struct Node {
    pub kind: NodeKind,
    pub label: String,
    pub expanded: bool,
    pub loaded: bool,
    pub children: Vec<usize>,
    pub parent: Option<usize>,
    /// 폴더 옆 키 수
    pub count: usize,
}

impl Node {
    pub fn expandable(&self) -> bool {
        matches!(
            self.kind,
            NodeKind::Group | NodeKind::Conn(_) | NodeKind::Queries(_) | NodeKind::Schema { .. } | NodeKind::Folder { .. }
        )
    }

    pub fn conn(&self) -> Option<usize> {
        match &self.kind {
            NodeKind::Conn(c) | NodeKind::Schema { conn: c, .. } | NodeKind::Table { conn: c, .. } => Some(*c),
            NodeKind::Folder { conn, .. } | NodeKind::Key { conn, .. } => Some(*conn),
            NodeKind::Queries(c) => *c,
            _ => None,
        }
    }
}

#[derive(Default)]
pub struct Tree {
    pub nodes: Vec<Node>,
    pub roots: Vec<usize>,
    /// 보이는 줄(노드, 깊이)
    pub rows: Vec<(usize, usize)>,
    pub cursor: usize,
    pub top: usize,
    pub filter: String,
    /// 접속마다 Redis SCAN 패턴(R2.5)
    pub patterns: std::collections::HashMap<usize, String>,
}

fn env_rank(env: &str) -> u8 {
    match env {
        "local" => 0,
        "stg" => 1,
        _ => 2,
    }
}

fn is_read(c: &ConnSummary) -> bool {
    c.name.contains("-read") || c.host.starts_with("read.") || c.host.contains(".cluster-ro-")
}

/// 같은 환경 안의 순서다: 쓰기 DB → 읽기 DB → Redis → 검색 → 로그·지표 → DynamoDB → Athena → aws(R66.1).
fn conn_rank(c: &ConnSummary) -> u8 {
    let r = u8::from(is_read(c));
    match c.kind.as_str() {
        "postgresql" => 1 + r,
        "redis" => 3 + r,
        "opensearch" => 5,
        "loki" | "prometheus" | "grafana" => 6,
        "athena" => 8,
        "aws" => 9,
        _ => 7,
    }
}

/// 숫자 마디는 숫자로 견주는 자연 정렬이다(`k2` < `k10`).
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut x, mut y) = (a.as_bytes(), b.as_bytes());
    loop {
        match (x.first(), y.first()) {
            (None, None) => return Ordering::Equal,
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
            (Some(p), Some(q)) if p.is_ascii_digit() && q.is_ascii_digit() => {
                let i = x.iter().take_while(|c| c.is_ascii_digit()).count();
                let j = y.iter().take_while(|c| c.is_ascii_digit()).count();
                let (n, m) = (std::str::from_utf8(&x[..i]).unwrap(), std::str::from_utf8(&y[..j]).unwrap());
                let (nt, mt) = (n.trim_start_matches('0'), m.trim_start_matches('0'));
                let o = nt.len().cmp(&mt.len()).then(nt.cmp(mt));
                if o != Ordering::Equal {
                    return o;
                }
                x = &x[i..];
                y = &y[j..];
            }
            (Some(p), Some(q)) => {
                if p != q {
                    return p.cmp(q);
                }
                x = &x[1..];
                y = &y[1..];
            }
        }
    }
}

impl Tree {
    fn add(&mut self, kind: NodeKind, label: impl Into<String>, parent: Option<usize>) -> usize {
        self.nodes.push(Node {
            kind,
            label: label.into(),
            expanded: false,
            loaded: false,
            children: Vec::new(),
            parent,
            count: 0,
        });
        self.nodes.len() - 1
    }

    /// 그룹 → 접속 트리를 만든다. 폴더 이름 순(`기타`는 맨 뒤), 폴더 안은 환경·종류·이름 순이다(R66.1).
    /// closed 에 든 폴더는 접고, 접속 없는 쿼리 파일이 있으면 맨 아래 `쿼리 (접속 없음)` 을 둔다(R43).
    pub fn build(conns: &[ConnSummary], closed: &[String], loose_files: usize) -> Tree {
        let mut t = Tree::default();
        let mut groups: Vec<String> = Vec::new();
        for c in conns {
            let g = if c.folder.is_empty() {
                "기타".to_string()
            } else {
                c.folder.clone()
            };
            if !groups.contains(&g) {
                groups.push(g);
            }
        }
        groups.sort_by(|a, b| (a == "기타").cmp(&(b == "기타")).then(a.cmp(b)));
        for g in groups {
            let gi = t.add(NodeKind::Group, g.clone(), None);
            t.nodes[gi].expanded = !closed.contains(&g);
            t.nodes[gi].loaded = true;
            t.roots.push(gi);
            let mut members: Vec<usize> = (0..conns.len())
                .filter(|&i| {
                    let f = if conns[i].folder.is_empty() {
                        "기타"
                    } else {
                        conns[i].folder.as_str()
                    };
                    f == g
                })
                .collect();
            members.sort_by(|&a, &b| {
                let (x, y) = (&conns[a], &conns[b]);
                env_rank(&x.env)
                    .cmp(&env_rank(&y.env))
                    .then(conn_rank(x).cmp(&conn_rank(y)))
                    .then(x.name.cmp(&y.name))
            });
            for i in members {
                let ci = t.add(NodeKind::Conn(i), conns[i].name.clone(), Some(gi));
                t.nodes[gi].children.push(ci);
            }
        }
        if loose_files > 0 {
            let q = t.add(NodeKind::Queries(None), format!("쿼리 (접속 없음) ({loose_files})"), None);
            t.nodes[q].expanded = true;
            t.roots.push(q);
        }
        t.rebuild();
        t
    }

    pub fn rebuild(&mut self) {
        let mut rows = Vec::new();
        let f = self.filter.to_lowercase();
        for &r in &self.roots.clone() {
            self.walk(r, 0, &f, &mut rows);
        }
        self.rows = rows;
        self.cursor = self.cursor.min(self.rows.len().saturating_sub(1));
    }

    /// 거르기가 걸리면 이름이 맞는 노드와 그 조상만 보인다. 펼친 노드만 내려간다.
    fn walk(&self, n: usize, depth: usize, f: &str, rows: &mut Vec<(usize, usize)>) -> bool {
        let node = &self.nodes[n];
        let start = rows.len();
        rows.push((n, depth));
        let mut any = f.is_empty() || node.label.to_lowercase().contains(f);
        if node.expanded {
            for &c in &node.children {
                any |= self.walk(c, depth + 1, f, rows);
            }
        }
        if !any {
            rows.truncate(start);
        }
        any
    }

    pub fn current(&self) -> Option<usize> {
        self.rows.get(self.cursor).map(|r| r.0)
    }

    pub fn node(&self, n: usize) -> &Node {
        &self.nodes[n]
    }

    pub fn select(&mut self, n: usize) {
        // 조상을 펼쳐 보이게 한다
        let mut p = self.nodes[n].parent;
        while let Some(i) = p {
            self.nodes[i].expanded = true;
            p = self.nodes[i].parent;
        }
        self.rebuild();
        if let Some(i) = self.rows.iter().position(|r| r.0 == n) {
            self.cursor = i;
        }
    }

    pub fn conn_node(&self, conn: usize) -> Option<usize> {
        self.nodes.iter().position(|n| n.kind == NodeKind::Conn(conn))
    }

    /// 노드 n 의 자식을 갈음한다.
    pub fn set_children(&mut self, n: usize, kids: Vec<(NodeKind, String)>) {
        let mut ids = Vec::with_capacity(kids.len());
        for (k, l) in kids {
            ids.push(self.add(k, l, Some(n)));
        }
        self.nodes[n].children = ids;
        self.nodes[n].loaded = true;
        self.rebuild();
    }

    /// 접속 줄 아래 앞에 둘 노드(쿼리 묶음)를 지키며 뒤쪽 자식을 갈음한다.
    pub fn set_conn_children(&mut self, n: usize, queries: Option<(String, Vec<String>)>, kids: Vec<(NodeKind, String)>) {
        let conn = self.nodes[n].conn();
        let mut all = Vec::new();
        if let Some((label, files)) = queries {
            let q = self.add(NodeKind::Queries(conn), label, Some(n));
            for f in files {
                let fi = self.add(NodeKind::File(f.clone()), f, Some(q));
                self.nodes[q].children.push(fi);
            }
            self.nodes[q].loaded = true;
            all.push(q);
        }
        for (k, l) in kids {
            all.push(self.add(k, l, Some(n)));
        }
        self.nodes[n].children = all;
        self.nodes[n].loaded = true;
        self.rebuild();
    }

    /// Redis 키를 `:` 이름공간 폴더 트리로 단다(R2.4). 폴더 먼저, 자연 정렬, 폴더 옆 키 수.
    pub fn set_keys(&mut self, parent: usize, conn: usize, prefix: &str, keys: &[String], more: bool) {
        let mut folders: Vec<(String, usize)> = Vec::new();
        let mut leaves: Vec<String> = Vec::new();
        for k in keys {
            let Some(rest) = k.strip_prefix(prefix) else { continue };
            match rest.find(':') {
                Some(i) => {
                    let name = &rest[..i];
                    match folders.iter_mut().find(|(f, _)| f == name) {
                        Some(f) => f.1 += 1,
                        None => folders.push((name.to_string(), 1)),
                    }
                }
                None => leaves.push(k.clone()),
            }
        }
        folders.sort_by(|a, b| natural_cmp(&a.0, &b.0));
        leaves.sort_by(|a, b| natural_cmp(a, b));
        let mut ids = Vec::new();
        for (f, n) in folders {
            let p = format!("{prefix}{f}:");
            let fi = self.add(NodeKind::Folder { conn, prefix: p.clone() }, format!("{f} ({n})"), Some(parent));
            self.nodes[fi].count = n;
            let sub: Vec<String> = keys.iter().filter(|k| k.starts_with(&p)).cloned().collect();
            self.set_keys(fi, conn, &p, &sub, false);
            self.nodes[fi].expanded = false;
            ids.push(fi);
        }
        for k in leaves {
            let label = k[prefix.len()..].to_string();
            ids.push(self.add(NodeKind::Key { conn, key: k }, label, Some(parent)));
        }
        if more {
            ids.push(self.add(NodeKind::Info, "… 더 있음", Some(parent)));
        }
        self.nodes[parent].children = ids;
        self.nodes[parent].loaded = true;
    }

    /// conn 아래 읽힌 Redis 키다(자동완성 R8.2·찾기 R22.3).
    pub fn loaded_keys(&self, conn: usize) -> Vec<String> {
        self.nodes
            .iter()
            .filter_map(|n| match &n.kind {
                NodeKind::Key { conn: c, key } if *c == conn => Some(key.clone()),
                _ => None,
            })
            .collect()
    }

    pub fn move_by(&mut self, d: isize) {
        let n = self.rows.len() as isize;
        if n == 0 {
            return;
        }
        self.cursor = (self.cursor as isize + d).clamp(0, n - 1) as usize;
    }

    /// 접은 폴더 이름들(R75.6 다음 실행에 접어 둔다)이다.
    pub fn closed_groups(&self) -> Vec<String> {
        self.roots
            .iter()
            .filter(|&&r| self.nodes[r].kind == NodeKind::Group && !self.nodes[r].expanded)
            .map(|&r| self.nodes[r].label.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(name: &str, folder: &str, kind: &str, env: &str) -> ConnSummary {
        ConnSummary {
            name: name.into(),
            folder: folder.into(),
            kind: kind.into(),
            env: env.into(),
            ..ConnSummary::default()
        }
    }

    #[test]
    fn 폴더는_이름순_기타는_맨_뒤_안은_환경_종류_이름순이다() {
        let conns = vec![
            conn("shop-prod", "shop", "postgresql", "prod"),
            conn("shop-stg-redis", "shop", "redis", "stg"),
            conn("shop-stg-read", "shop", "postgresql", "stg"),
            conn("shop-stg", "shop", "postgresql", "stg"),
            conn("solo", "", "postgresql", "local"),
            conn("shop-local", "shop", "postgresql", "local"),
            conn("app", "app", "postgresql", "local"),
        ];
        let t = Tree::build(&conns, &["app".into()], 0);
        let labels: Vec<&str> = t.rows.iter().map(|&(n, _)| t.nodes[n].label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "app",
                "shop",
                "shop-local",
                "shop-stg",
                "shop-stg-read",
                "shop-stg-redis",
                "shop-prod",
                "기타",
                "solo"
            ]
        );
    }

    #[test]
    fn 레디스_키는_이름공간_폴더_먼저_자연_정렬이다() {
        let mut t = Tree::build(&[conn("r", "", "redis", "local")], &[], 0);
        let c = t.conn_node(0).unwrap();
        let keys: Vec<String> = ["user:10", "user:2", "solo", "a:b:c"].iter().map(|s| s.to_string()).collect();
        t.set_keys(c, 0, "", &keys, true);
        t.nodes[c].expanded = true;
        t.rebuild();
        let labels: Vec<&str> = t.rows.iter().map(|&(n, _)| t.nodes[n].label.as_str()).collect();
        assert_eq!(labels, ["기타", "r", "a (1)", "user (2)", "solo", "… 더 있음"]);
        let user = t.rows[3].0;
        t.nodes[user].expanded = true;
        t.rebuild();
        let labels: Vec<&str> = t.rows.iter().map(|&(n, _)| t.nodes[n].label.as_str()).collect();
        assert_eq!(&labels[4..6], ["2", "10"]);
    }

    #[test]
    fn 거르기는_맞는_노드와_조상만_보인다() {
        let mut t = Tree::build(
            &[conn("app-stg", "a", "postgresql", "stg"), conn("x", "b", "postgresql", "local")],
            &[],
            0,
        );
        t.filter = "stg".into();
        t.rebuild();
        let labels: Vec<&str> = t.rows.iter().map(|&(n, _)| t.nodes[n].label.as_str()).collect();
        assert_eq!(labels, ["a", "app-stg"]);
    }
}
