//! 접속 트리(DataGrip·TUI 처럼). 폴더(옮긴 폴더가 없으면 그룹, 그것도 없으면 기타 — 엔진이 채운다) 아래 접속,
//! 접속을 펼치면 그 자리에 자식이 붙는다: PostgreSQL 은 스키마 → 테이블·뷰(카탈로그 한 번), Redis 는 키 이름공간
//! (SCAN 한 번, `:` 마디), 저장소는 `source_children`. 자식은 펼칠 때 읽고, 읽는 동안은 "읽는 중…" 줄, 실패하면
//! 오류 줄(다시 시도)을 둔다 — 끝없이 읽는 중으로 남지 않는다. 그리는 쪽은 `rows()` 의 평평한 줄만 본다.

use std::collections::{BTreeSet, HashMap};

use idb_engine::{Catalog, ConnSummary, Node};

use super::safety::{Tone, tone};

/// 한 단계에 보일 Redis 이름공간·키 수(넘치면 "… N개 더" 줄)
pub const REDIS_LEVEL_CAP: usize = 200;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChildKind {
    Schema,
    Table,
    View,
    /// Redis 이름공간(`user:`), 그 아래 키 수
    RedisNs {
        count: usize,
    },
    RedisKey,
    Source {
        openable: bool,
        has_children: bool,
        warn: bool,
    },
    /// 상한을 넘어 보이지 않은 수
    More(usize),
    /// SCAN 이 상한(1,000개)에서 끊긴 부분 목록이라는 표시. 거르기도 받은 키 안에서만 찾는다
    Partial,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Child {
    /// 트리 안에서 하나뿐인 자리(펼침 기억에 쓴다)
    pub id: String,
    pub label: String,
    /// 오른쪽 흐린 글(저장소 info)
    pub info: String,
    pub kind: ChildKind,
    /// 열 때 쓸 값: 스키마 이름, 테이블의 스키마, Redis 이름공간·키 전체, 저장소 길
    pub path: String,
}

impl Child {
    pub fn expandable(&self) -> bool {
        matches!(
            self.kind,
            ChildKind::Schema | ChildKind::RedisNs { .. } | ChildKind::Source { has_children: true, .. }
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Load {
    Loading,
    Loaded(Vec<Child>),
    Failed(String),
}

/// 화면이 엔진에 보낼 읽기다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadReq {
    Catalog { conn: String },
    RedisScan { conn: String },
    SourceChildren { conn: String, node: String, path: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowKind {
    Folder {
        expanded: bool,
        count: usize,
    },
    /// conns 의 자리
    Conn {
        index: usize,
        expanded: bool,
    },
    Child {
        conn: usize,
        child: Child,
        expanded: bool,
    },
    /// node 의 자식을 읽는 중
    Loading {
        node: String,
    },
    /// node 의 자식 읽기가 실패했다(다시 시도)
    Failed {
        node: String,
        msg: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub kind: RowKind,
    pub depth: usize,
    pub label: String,
    /// 접속 줄의 흐린 설명(종류·호스트/DB)
    pub detail: String,
    pub tone: Tone,
}

#[derive(Default)]
pub struct ConnTree {
    pub conns: Vec<ConnSummary>,
    folders: Vec<String>,
    collapsed: BTreeSet<String>,
    filter: String,
    expanded: BTreeSet<String>,
    loads: HashMap<String, Load>,
    /// Redis 접속마다 SCAN 으로 받은 키(정렬, 이름공간은 여기서 셈한다)
    redis_keys: HashMap<String, Vec<String>>,
    /// SCAN 이 끊겨 부분 목록인 Redis 접속
    redis_partial: BTreeSet<String>,
    /// 지난 실행에서 펼쳐 둔 자리(R75.6). 그 자리의 부모가 읽히면 다시 편다
    remembered: BTreeSet<String>,
}

pub fn child_id(parent: &str, tag: &str, path: &str) -> String {
    format!("{parent}\u{1f}{tag}:{path}")
}

/// node id 의 접속 이름(맨 앞 마디)
pub fn conn_of(node: &str) -> &str {
    node.split('\u{1f}').next().unwrap_or(node)
}

impl ConnTree {
    pub fn new(conns: Vec<ConnSummary>) -> ConnTree {
        let mut folders: Vec<String> = Vec::new();
        for c in &conns {
            if !folders.contains(&c.folder) {
                folders.push(c.folder.clone());
            }
        }
        ConnTree {
            conns,
            folders,
            ..Default::default()
        }
    }

    pub fn toggle(&mut self, folder: &str) {
        if !self.collapsed.remove(folder) {
            self.collapsed.insert(folder.to_string());
        }
    }

    pub fn set_filter(&mut self, f: &str) {
        self.filter = f.trim().to_lowercase();
    }

    fn conn_index(&self, name: &str) -> Option<usize> {
        self.conns.iter().position(|c| c.name == name)
    }

    /// 마디 읽기가 끝났다(읽었거나 실패). 읽기를 보낸 적 없으면 false 다.
    pub fn is_settled(&self, node: &str) -> bool {
        matches!(self.loads.get(node), Some(Load::Loaded(_) | Load::Failed(_)))
    }

    pub fn is_expanded(&self, node: &str) -> bool {
        self.expanded.contains(node)
    }

    // ─── 펼침·읽기 ───

    fn request_for(&self, node: &str) -> Option<LoadReq> {
        let conn = conn_of(node).to_string();
        if node == conn {
            let c = &self.conns[self.conn_index(&conn)?];
            return match c.kind.as_str() {
                "postgresql" => Some(LoadReq::Catalog { conn }),
                "redis" => Some(LoadReq::RedisScan { conn }),
                _ => Some(LoadReq::SourceChildren {
                    conn,
                    node: node.to_string(),
                    path: String::new(),
                }),
            };
        }
        // 저장소 자식만 따로 읽는다(스키마·이름공간은 부모를 읽을 때 이미 안다)
        let (_, rest) = node.rsplit_once('\u{1f}')?;
        let path = rest.strip_prefix("o:")?;
        Some(LoadReq::SourceChildren {
            conn,
            node: node.to_string(),
            path: path.to_string(),
        })
    }

    /// 펼치거나 접는다. 펼쳤는데 자식을 아직 모르면 읽기 요청을 돌려준다(그동안 "읽는 중…").
    pub fn toggle_node(&mut self, node: &str) -> Option<LoadReq> {
        if self.expanded.remove(node) {
            self.remembered.remove(node);
            return None;
        }
        self.expanded.insert(node.to_string());
        self.remembered.insert(node.to_string());
        self.ensure_loaded(node)
    }

    fn ensure_loaded(&mut self, node: &str) -> Option<LoadReq> {
        if self.loads.contains_key(node) {
            return None;
        }
        // Redis 이름공간은 받은 키에서 바로 셈한다
        if let Some(prefix) = node.rsplit_once('\u{1f}').and_then(|(_, r)| r.strip_prefix("r:")) {
            let kids = self.redis_level(conn_of(node), node, prefix);
            self.loads.insert(node.to_string(), Load::Loaded(kids));
            return None;
        }
        let req = self.request_for(node)?;
        self.loads.insert(node.to_string(), Load::Loading);
        Some(req)
    }

    /// 실패한 읽기를 다시 한다.
    pub fn retry(&mut self, node: &str) -> Option<LoadReq> {
        self.loads.remove(node);
        self.expanded.insert(node.to_string());
        self.ensure_loaded(node)
    }

    /// 접속의 읽은 것을 버린다(데이터베이스를 바꿨거나 새로 고침).
    pub fn forget_conn(&mut self, conn: &str) {
        let pre = format!("{conn}\u{1f}");
        self.loads.retain(|k, _| k != conn && !k.starts_with(&pre));
        self.redis_keys.remove(conn);
        self.redis_partial.remove(conn);
    }

    pub fn set_failed(&mut self, node: &str, msg: String) {
        self.loads.insert(node.to_string(), Load::Failed(msg));
    }

    /// 펼쳐 둔 자식이 있으면 이어서 읽을 요청을 모은다(R75.6).
    fn reopen_remembered(&mut self, kids: &[Child]) -> Vec<LoadReq> {
        let mut out = Vec::new();
        for k in kids {
            if k.expandable() && self.remembered.contains(&k.id) {
                self.expanded.insert(k.id.clone());
                if let Some(r) = self.ensure_loaded(&k.id) {
                    out.push(r);
                }
            }
        }
        out
    }

    pub fn set_catalog(&mut self, conn: &str, cat: &Catalog) -> Vec<LoadReq> {
        let mut schemas = Vec::new();
        for s in &cat.schemas {
            let sid = child_id(conn, "s", &s.name);
            let tables = s
                .tables
                .iter()
                .map(|t| Child {
                    id: child_id(&sid, "t", &t.name),
                    label: t.name.clone(),
                    info: String::new(),
                    kind: if t.view { ChildKind::View } else { ChildKind::Table },
                    path: s.name.clone(),
                })
                .collect();
            self.loads.insert(sid.clone(), Load::Loaded(tables));
            schemas.push(Child {
                id: sid,
                label: s.name.clone(),
                info: s.tables.len().to_string(),
                kind: ChildKind::Schema,
                path: s.name.clone(),
            });
        }
        let out = self.reopen_remembered(&schemas);
        self.loads.insert(conn.to_string(), Load::Loaded(schemas));
        out
    }

    /// more 는 SCAN 이 상한에서 끊겼다는 뜻이다(R2). 그때 맨 위에 부분 목록 줄을 둔다.
    pub fn set_redis_keys(&mut self, conn: &str, mut keys: Vec<String>, more: bool) -> Vec<LoadReq> {
        keys.sort();
        keys.dedup();
        let n = keys.len();
        self.redis_keys.insert(conn.to_string(), keys);
        let mut kids = self.redis_level(conn, conn, "");
        if more {
            self.redis_partial.insert(conn.to_string());
            kids.insert(
                0,
                Child {
                    id: child_id(conn, "p", ""),
                    label: format!("부분 목록 ({n}개까지) — 패턴으로 좁힌다"),
                    info: String::new(),
                    kind: ChildKind::Partial,
                    path: String::new(),
                },
            );
        } else {
            self.redis_partial.remove(conn);
        }
        let out = self.reopen_remembered(&kids);
        self.loads.insert(conn.to_string(), Load::Loaded(kids));
        out
    }

    /// prefix 아래 한 단계: `a:b:c` 는 prefix "" 에서 이름공간 `a:`, prefix `a:` 에서 `a:b:` 다.
    fn redis_level(&self, conn: &str, parent: &str, prefix: &str) -> Vec<Child> {
        let Some(keys) = self.redis_keys.get(conn) else { return Vec::new() };
        let mut out: Vec<Child> = Vec::new();
        let mut total = 0usize;
        for k in keys.iter().filter(|k| k.starts_with(prefix) && k.len() > prefix.len()) {
            let rest = &k[prefix.len()..];
            match rest.find(':') {
                Some(i) => {
                    let ns = format!("{prefix}{}", &rest[..=i]);
                    match out.last_mut() {
                        Some(Child {
                            kind: ChildKind::RedisNs { count },
                            path,
                            ..
                        }) if *path == ns => *count += 1,
                        _ => {
                            total += 1;
                            if total <= REDIS_LEVEL_CAP {
                                out.push(Child {
                                    id: child_id(parent, "r", &ns),
                                    label: rest[..=i].to_string(),
                                    info: String::new(),
                                    kind: ChildKind::RedisNs { count: 1 },
                                    path: ns,
                                });
                            }
                        }
                    }
                }
                None => {
                    total += 1;
                    if total <= REDIS_LEVEL_CAP {
                        out.push(Child {
                            id: child_id(parent, "k", k),
                            label: rest.to_string(),
                            info: String::new(),
                            kind: ChildKind::RedisKey,
                            path: k.clone(),
                        });
                    }
                }
            }
        }
        for c in &mut out {
            if let ChildKind::RedisNs { count } = c.kind {
                c.info = count.to_string();
            }
        }
        if total > REDIS_LEVEL_CAP {
            out.push(Child {
                id: child_id(parent, "m", prefix),
                label: format!("… {}개 더 (패턴으로 좁힌다)", total - REDIS_LEVEL_CAP),
                info: String::new(),
                kind: ChildKind::More(total - REDIS_LEVEL_CAP),
                path: String::new(),
            });
        }
        out
    }

    pub fn set_source_children(&mut self, node: &str, nodes: Vec<Node>) -> Vec<LoadReq> {
        let kids: Vec<Child> = nodes
            .into_iter()
            .map(|n| Child {
                id: child_id(node, "o", &n.path),
                label: n.name,
                info: n.info,
                kind: ChildKind::Source {
                    openable: n.openable,
                    has_children: n.has_children,
                    warn: n.warn,
                },
                path: n.path,
            })
            .collect();
        let out = self.reopen_remembered(&kids);
        self.loads.insert(node.to_string(), Load::Loaded(kids));
        out
    }

    // ─── 펼침 기억(R75.6) ───

    /// 기억할 자리: 접은 폴더와 펼친 자리
    pub fn memory(&self) -> (Vec<String>, Vec<String>) {
        (self.collapsed.iter().cloned().collect(), self.remembered.iter().cloned().collect())
    }

    /// 켤 때 되살린다. 폴더는 바로 접고, 접속 아래는 그 접속을 사용자가 처음 펼쳐 읽힐 때 다시 편다(켤 때 접속하지 않는다).
    pub fn restore(&mut self, collapsed: &[String], expanded: &[String]) {
        self.collapsed.extend(collapsed.iter().cloned());
        self.remembered.extend(expanded.iter().filter(|e| e.contains('\u{1f}')).cloned());
        // 접속 줄 자체의 펼침도 기억만 하고 읽지는 않는다
        self.remembered.extend(expanded.iter().filter(|e| !e.contains('\u{1f}')).cloned());
    }

    /// gui.json 에 남겨도 되는 펼침이다. prod 접속의 Redis 이름공간 길은 키 이름(이메일 등 개인정보)이 들 수 있어
    /// 남기지 않는다(R75 의 prod 제외와 같은 규칙). prod 가 아닌 접속과 prod 의 스키마·저장소 길은 남긴다.
    pub fn persistable(expanded: &[String], prod_conns: &[String]) -> Vec<String> {
        expanded
            .iter()
            .filter(|id| !(prod_conns.iter().any(|c| c == conn_of(id)) && id.contains("\u{1f}r:")))
            .cloned()
            .collect()
    }

    // ─── 거르기 ───

    fn conn_matches(&self, c: &ConnSummary) -> bool {
        self.filter.is_empty()
            || [&c.name, &c.host, &c.db, &c.group, &c.kind]
                .iter()
                .any(|s| s.to_lowercase().contains(&self.filter))
    }

    /// 읽은 자식 가운데 거르기 글이 든 것이 있나(접속 아래 전체)
    fn subtree_matches(&self, node: &str) -> bool {
        match self.loads.get(node) {
            Some(Load::Loaded(kids)) => kids.iter().any(|k| {
                k.kind != ChildKind::Partial
                    && (k.label.to_lowercase().contains(&self.filter) || (k.expandable() && self.subtree_matches(&k.id)))
            }),
            _ => false,
        }
    }

    pub fn rows(&self) -> Vec<Row> {
        let filtering = !self.filter.is_empty();
        let mut out = Vec::new();
        for f in &self.folders {
            let members: Vec<usize> = (0..self.conns.len())
                .filter(|&i| self.conns[i].folder == *f)
                .filter(|&i| !filtering || self.conn_matches(&self.conns[i]) || self.subtree_matches(&self.conns[i].name))
                .collect();
            if members.is_empty() {
                continue;
            }
            // 폴더 없는 접속은 머리 줄 없이 그대로 둔다(이름 없는 접힘 줄이 보이지 않게)
            let expanded = f.is_empty() || filtering || !self.collapsed.contains(f);
            // 폴더 안에 prod 가 있으면 폴더도 빨갛게 둬 접혀 있어도 보이게 한다
            let folder_tone = if members.iter().any(|&i| tone(&self.conns[i].env) == Tone::Prod) {
                Tone::Prod
            } else {
                Tone::Local
            };
            if !f.is_empty() {
                out.push(Row {
                    kind: RowKind::Folder {
                        expanded,
                        count: members.len(),
                    },
                    depth: 0,
                    label: f.clone(),
                    detail: String::new(),
                    tone: folder_tone,
                });
            }
            if !expanded {
                continue;
            }
            for i in members {
                let c = &self.conns[i];
                // 거르는 동안 자식만 맞으면 그 길을 펴서 보인다
                let only_kids = filtering && !self.conn_matches(c);
                let open = self.expanded.contains(&c.name) || only_kids;
                out.push(Row {
                    kind: RowKind::Conn { index: i, expanded: open },
                    depth: 1,
                    label: c.name.clone(),
                    detail: conn_detail(c),
                    tone: tone(&c.env),
                });
                if open {
                    self.push_children(i, &c.name, 2, only_kids, &mut out);
                }
            }
        }
        out
    }

    fn push_children(&self, conn: usize, node: &str, depth: usize, only_matches: bool, out: &mut Vec<Row>) {
        let t = tone(&self.conns[conn].env);
        match self.loads.get(node) {
            None => {}
            Some(Load::Loading) => out.push(Row {
                kind: RowKind::Loading { node: node.to_string() },
                depth,
                label: "읽는 중…".into(),
                detail: String::new(),
                tone: t,
            }),
            Some(Load::Failed(msg)) => out.push(Row {
                kind: RowKind::Failed {
                    node: node.to_string(),
                    msg: msg.clone(),
                },
                depth,
                label: msg.clone(),
                detail: String::new(),
                tone: t,
            }),
            Some(Load::Loaded(kids)) => {
                for k in kids {
                    // 부분 목록 표시는 거를 때도 보여 "받은 키 안에서만 찾았다"를 알린다
                    let label_hit = k.label.to_lowercase().contains(&self.filter) || k.kind == ChildKind::Partial;
                    let sub_hit = k.expandable() && self.subtree_matches(&k.id);
                    if only_matches && !label_hit && !sub_hit {
                        continue;
                    }
                    let open = k.expandable() && (self.expanded.contains(&k.id) || (only_matches && sub_hit));
                    out.push(Row {
                        kind: RowKind::Child {
                            conn,
                            child: k.clone(),
                            expanded: open,
                        },
                        depth,
                        label: k.label.clone(),
                        detail: k.info.clone(),
                        tone: t,
                    });
                    if open && depth < 32 {
                        // 이름이 맞은 묶음은 안을 다 보이고, 안쪽만 맞은 묶음은 맞는 것만 보인다
                        self.push_children(conn, &k.id, depth + 1, only_matches && !label_hit, out);
                    }
                }
            }
        }
    }
}

fn conn_detail(c: &ConnSummary) -> String {
    let kind = match c.kind.as_str() {
        "postgresql" => "pg",
        k => k,
    };
    match (c.host.is_empty(), c.db.is_empty()) {
        (false, false) => format!("{kind} · {}/{}", c.host, c.db),
        (false, true) => format!("{kind} · {}", c.host),
        _ => kind.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(name: &str, folder: &str, env: &str, kind: &str) -> ConnSummary {
        ConnSummary {
            name: name.into(),
            group: folder.into(),
            folder: folder.into(),
            kind: kind.into(),
            env: env.into(),
            host: "h".into(),
            db: "d".into(),
            source: "db".into(),
            ..ConnSummary::default()
        }
    }

    fn sample() -> ConnTree {
        ConnTree::new(vec![
            conn("app-local", "app", "local", "postgresql"),
            conn("cache", "기타", "local", "redis"),
            conn("app-prod", "app", "prod", "postgresql"),
            conn("logs", "기타", "stg", "loki"),
        ])
    }

    fn catalog() -> Catalog {
        serde_json::from_value(serde_json::json!({
            "schemas": [{"name": "public", "tables": [
                {"name": "users", "view": false, "columns": []},
                {"name": "active_users", "view": true, "columns": []}
            ]}, {"name": "audit", "tables": [{"name": "log", "view": false, "columns": []}]}],
            "functions": []
        }))
        .unwrap()
    }

    fn labels(t: &ConnTree) -> Vec<(usize, String)> {
        t.rows().into_iter().map(|r| (r.depth, r.label)).collect()
    }

    #[test]
    fn 폴더는_처음_나온_순서이고_접속은_그_아래다() {
        let rows = sample().rows();
        let l: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(l, vec!["app", "app-local", "app-prod", "기타", "cache", "logs"]);
        assert_eq!(rows[0].kind, RowKind::Folder { expanded: true, count: 2 });
        // 폴더 이름이 없으면 머리 줄이 없다
        let mut bare = ConnTree::new(vec![ConnSummary {
            name: "solo".into(),
            ..Default::default()
        }]);
        let r = bare.rows();
        assert!(matches!(r[0].kind, RowKind::Conn { .. }), "{:?}", r[0].kind);
        bare.toggle_node("solo");
        assert!(bare.rows().iter().all(|r| !matches!(r.kind, RowKind::Folder { .. })));
        assert_eq!(rows[2].kind, RowKind::Conn { index: 2, expanded: false });
        assert_eq!(rows[1].detail, "pg · h/d");
        assert_eq!(rows[2].tone, Tone::Prod);
        assert_eq!(rows[0].tone, Tone::Prod, "prod 가 든 폴더도 빨갛다");
    }

    #[test]
    fn 접은_폴더는_접속을_숨긴다() {
        let mut t = sample();
        t.toggle("app");
        assert_eq!(t.rows().len(), 4);
        t.toggle("app");
        assert_eq!(t.rows().len(), 6);
    }

    #[test]
    fn PG_접속을_펼치면_카탈로그를_읽고_스키마_테이블이_그_자리에_붙는다() {
        let mut t = sample();
        assert_eq!(t.toggle_node("app-local"), Some(LoadReq::Catalog { conn: "app-local".into() }));
        let rows = t.rows();
        assert!(
            matches!(&rows[2].kind, RowKind::Loading { node } if node == "app-local"),
            "{rows:?}"
        );
        assert_eq!(rows[2].depth, 2);
        assert!(t.set_catalog("app-local", &catalog()).is_empty());
        let sid = child_id("app-local", "s", "public");
        assert_eq!(t.toggle_node(&sid), None, "스키마는 이미 안다");
        let l = labels(&t);
        assert_eq!(
            &l[1..6],
            &[
                (1, "app-local".into()),
                (2, "public".into()),
                (3, "users".into()),
                (3, "active_users".into()),
                (2, "audit".into())
            ]
        );
        let view = t.rows().into_iter().find(|r| r.label == "active_users").unwrap();
        assert!(matches!(view.kind, RowKind::Child { child: Child { kind: ChildKind::View, ref path, .. }, .. } if path == "public"));
        // 접으면 자식이 사라지고, 다시 펴도 다시 읽지 않는다
        assert_eq!(t.toggle_node("app-local"), None);
        assert_eq!(t.rows().len(), 6);
        assert_eq!(t.toggle_node("app-local"), None);
    }

    #[test]
    fn 읽기가_실패하면_오류_줄이고_다시_시도하면_다시_읽는다() {
        let mut t = sample();
        t.toggle_node("app-local");
        t.set_failed("app-local", "connection refused".into());
        let rows = t.rows();
        assert!(
            matches!(&rows[2].kind, RowKind::Failed { node, msg } if node == "app-local" && msg.contains("refused")),
            "끝없이 읽는 중이 아니다"
        );
        assert_eq!(t.retry("app-local"), Some(LoadReq::Catalog { conn: "app-local".into() }));
        assert!(matches!(t.rows()[2].kind, RowKind::Loading { .. }));
    }

    #[test]
    fn Redis_는_키를_이름공간으로_묶고_펼치면_다음_마디다() {
        let mut t = sample();
        assert_eq!(t.toggle_node("cache"), Some(LoadReq::RedisScan { conn: "cache".into() }));
        t.set_redis_keys(
            "cache",
            vec![
                "user:1:name".into(),
                "user:2:name".into(),
                "user:9".into(),
                "plain".into(),
                "user:1:age".into(),
            ],
            false,
        );
        let kids: Vec<(String, String)> = t.rows().into_iter().filter(|r| r.depth == 2).map(|r| (r.label, r.detail)).collect();
        assert_eq!(kids, vec![("plain".into(), String::new()), ("user:".into(), "4".into())]);
        let ns = child_id("cache", "r", "user:");
        assert_eq!(t.toggle_node(&ns), None, "이름공간은 받은 키에서 셈한다");
        let l: Vec<String> = t.rows().into_iter().filter(|r| r.depth == 3).map(|r| r.label).collect();
        assert_eq!(l, vec!["1:", "2:", "9"]);
    }

    #[test]
    fn Redis_한_단계가_넘치면_더_있음_줄로_자른다() {
        let mut t = sample();
        t.toggle_node("cache");
        t.set_redis_keys("cache", (0..REDIS_LEVEL_CAP + 5).map(|i| format!("k{i:04}")).collect(), false);
        let kids: Vec<Row> = t.rows().into_iter().filter(|r| r.depth == 2).collect();
        assert_eq!(kids.len(), REDIS_LEVEL_CAP + 1);
        assert!(matches!(
            kids.last().unwrap().kind,
            RowKind::Child {
                child: Child {
                    kind: ChildKind::More(5),
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    fn SCAN_이_끊기면_부분_목록_줄을_두고_거를_때도_보인다() {
        let mut t = sample();
        t.toggle_node("cache");
        t.set_redis_keys("cache", vec!["user:1".into(), "plain".into()], true);
        let kids: Vec<Row> = t.rows().into_iter().filter(|r| r.depth == 2).collect();
        assert!(matches!(
            kids[0].kind,
            RowKind::Child {
                child: Child {
                    kind: ChildKind::Partial,
                    ..
                },
                ..
            }
        ));
        assert!(kids[0].label.contains("부분 목록 (2개까지)"), "{}", kids[0].label);
        t.set_filter("plain");
        let l: Vec<String> = t.rows().into_iter().filter(|r| r.depth == 2).map(|r| r.label).collect();
        assert_eq!(l.len(), 2, "{l:?}");
        assert!(l[0].starts_with("부분 목록") && l[1] == "plain");
        // 부분 목록 줄만으로는 접속이 맞은 것으로 치지 않는다
        t.set_filter("zzz");
        assert!(!t.rows().iter().any(|r| r.label == "cache"));
        // 다시 읽어 끊기지 않았으면 줄이 사라진다
        t.set_filter("");
        t.set_redis_keys("cache", vec!["a".into()], false);
        assert!(!t.rows().iter().any(|r| r.label.starts_with("부분 목록")));
    }

    #[test]
    fn prod_Redis_이름공간_펼침은_남기지_않는다() {
        let ex = vec![
            "cache-prod".to_string(),
            "cache-prod\u{1f}r:user:a@b.com:".to_string(),
            "cache\u{1f}r:user:".to_string(),
            "db-prod\u{1f}s:public".to_string(),
        ];
        let kept = ConnTree::persistable(&ex, &["cache-prod".to_string(), "db-prod".to_string()]);
        assert_eq!(
            kept,
            vec![
                "cache-prod".to_string(),
                "cache\u{1f}r:user:".to_string(),
                "db-prod\u{1f}s:public".to_string()
            ]
        );
    }

    #[test]
    fn 저장소는_source_children_으로_한_단계씩_읽는다() {
        let mut t = sample();
        let req = t.toggle_node("logs").unwrap();
        assert_eq!(
            req,
            LoadReq::SourceChildren {
                conn: "logs".into(),
                node: "logs".into(),
                path: String::new()
            }
        );
        let node = |name: &str, path: &str, kids: bool| Node {
            name: name.into(),
            path: path.into(),
            info: String::new(),
            has_children: kids,
            openable: !kids,
            warn: false,
        };
        t.set_source_children("logs", vec![node("app", "app", true)]);
        let id = child_id("logs", "o", "app");
        assert_eq!(
            t.toggle_node(&id),
            Some(LoadReq::SourceChildren {
                conn: "logs".into(),
                node: id.clone(),
                path: "app".into()
            })
        );
        t.set_source_children(&id, vec![node("api", "app/api", false)]);
        assert_eq!(labels(&t).last().unwrap(), &(3, "api".to_string()));
    }

    #[test]
    fn 거르기는_접속과_읽은_테이블을_찾고_맞는_길을_편다() {
        let mut t = sample();
        t.toggle_node("app-local");
        t.set_catalog("app-local", &catalog());
        t.toggle_node("app-local"); // 접어 둔다
        t.toggle("app"); // 폴더도 접어 둔다
        t.set_filter("LOG");
        let l = labels(&t);
        // logs 접속 이름과 audit.log 테이블이 맞는다. audit 스키마만 펴고 public 은 감춘다
        assert!(
            l.contains(&(1, "app-local".into())) && l.contains(&(2, "audit".into())) && l.contains(&(3, "log".into())),
            "{l:?}"
        );
        assert!(!l.iter().any(|(_, x)| x == "public" || x == "users"), "{l:?}");
        assert!(l.contains(&(1, "logs".into())));
        assert!(!l.iter().any(|(_, x)| x == "app-prod"), "{l:?}");
        t.set_filter("");
        assert!(
            !labels(&t).iter().any(|(_, x)| x == "audit"),
            "거르기를 풀면 원래 펼침으로 돌아간다"
        );
    }

    #[test]
    fn 펼침은_기억했다가_부모가_읽히면_다시_편다() {
        let mut t = sample();
        t.toggle_node("logs");
        let id = child_id("logs", "o", "app");
        t.set_source_children(
            "logs",
            vec![Node {
                name: "app".into(),
                path: "app".into(),
                info: String::new(),
                has_children: true,
                openable: false,
                warn: false,
            }],
        );
        t.toggle_node(&id);
        t.toggle("app");
        let (collapsed, expanded) = t.memory();
        // 다시 켠다: 접속은 읽지 않고, 사용자가 펼쳐 읽히면 기억한 자식을 다시 편다
        let mut t2 = sample();
        t2.restore(&collapsed, &expanded);
        assert_eq!(t2.rows().len(), 4, "폴더는 바로 접는다");
        assert!(!t2.is_expanded("logs"), "켤 때 접속하지 않는다");
        t2.toggle_node("logs");
        let again = t2.set_source_children(
            "logs",
            vec![Node {
                name: "app".into(),
                path: "app".into(),
                info: String::new(),
                has_children: true,
                openable: false,
                warn: false,
            }],
        );
        assert_eq!(
            again,
            vec![LoadReq::SourceChildren {
                conn: "logs".into(),
                node: id.clone(),
                path: "app".into()
            }]
        );
        assert!(t2.is_expanded(&id));
    }

    #[test]
    fn 접속을_잊으면_다시_읽는다() {
        let mut t = sample();
        t.toggle_node("app-local");
        t.set_catalog("app-local", &catalog());
        t.forget_conn("app-local");
        t.toggle_node("app-local");
        assert_eq!(t.toggle_node("app-local"), Some(LoadReq::Catalog { conn: "app-local".into() }));
    }
}
