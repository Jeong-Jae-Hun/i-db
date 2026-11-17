//! 명령 팔레트(R40·R75.2)와 객체 찾기(R22.3·R56·R75.3). 둘 다 부분 글자 고르기 창(Overlay::Picker)이다.

use std::collections::HashMap;

use idb_engine::TableRef;

use crate::app::App;
use crate::help::Focus;

enum Pick {
    Action(crate::help::PaletteAction),
    Table(usize, String, String),
    Conn(usize),
    Key(usize, String),
}

impl App {
    fn table_label(&self, conn: usize, schema: &str, table: &str) -> String {
        format!("[{}] {} · {schema}.{table}", self.conns[conn].env, self.conns[conn].name)
    }

    fn conn_by_uuid(&self, uuid: &str) -> Option<usize> {
        self.conns.iter().position(|c| !uuid.is_empty() && c.uuid == uuid)
    }

    /// 목록을 읽은 접속의 테이블·뷰다. 환경 local → stg → prod 순이다.
    fn catalog_tables(&self) -> Vec<(usize, String, String)> {
        let mut out = Vec::new();
        let mut conns: Vec<&usize> = self.catalogs.keys().collect();
        conns.sort_by_key(|&&c| (env_rank(&self.conns[c].env), self.conns[c].name.clone()));
        for &c in conns {
            for s in &self.catalogs[&c].schemas {
                for t in &s.tables {
                    out.push((c, s.name.clone(), t.name.clone()));
                }
            }
        }
        out
    }

    pub fn open_palette(&mut self) {
        let mut items = Vec::new();
        let mut map: HashMap<String, Pick> = HashMap::new();
        let push = |label: String, p: Pick, items: &mut Vec<String>, map: &mut HashMap<String, Pick>| {
            let mut l = label;
            while map.contains_key(&l) {
                l.push_str(" #");
            }
            items.push(l.clone());
            map.insert(l, p);
        };
        let actions = crate::help::palette_actions(self.mac);
        // 자주 쓴 동작 5개를 맨 위에 둔다(R75.2)
        let top = self.session.top_actions(5);
        let mut used = Vec::new();
        for memo in &top {
            if let Some(a) = actions.iter().find(|a| &a.memo() == memo) {
                used.push(a.memo());
                push(a.label.clone(), Pick::Action(a.clone()), &mut items, &mut map);
            }
        }
        // 최근 연 테이블(R40.2) 10개
        let mut seen = Vec::new();
        for rt in self.session.recent_tables().into_iter().take(10) {
            let Some(c) = self.conn_by_uuid(&rt.conn) else { continue };
            seen.push((c, rt.schema.clone(), rt.table.clone()));
            push(
                self.table_label(c, &rt.schema, &rt.table),
                Pick::Table(c, rt.schema, rt.table),
                &mut items,
                &mut map,
            );
        }
        for (c, s, t) in self.catalog_tables() {
            if seen.contains(&(c, s.clone(), t.clone())) {
                continue;
            }
            push(self.table_label(c, &s, &t), Pick::Table(c, s, t), &mut items, &mut map);
        }
        for (i, c) in self.conns.iter().enumerate() {
            push(format!("접속 {}", c.name), Pick::Conn(i), &mut items, &mut map);
        }
        for a in actions {
            if used.contains(&a.memo()) {
                continue;
            }
            push(a.label.clone(), Pick::Action(a), &mut items, &mut map);
        }
        self.picker("명령 팔레트", items, move |a, label| {
            let Some(p) = map.remove(&label) else { return };
            a.palette_pick(p);
        });
    }

    fn palette_pick(&mut self, p: Pick) {
        match p {
            Pick::Action(act) => {
                self.session.use_action(&act.memo());
                if act.at != Focus::Keep {
                    self.set_focus(act.at);
                }
                self.on_key(act.press);
            }
            Pick::Table(c, s, t) => self.open_table_from_tree(c, s, t),
            Pick::Conn(c) => {
                if let Some(n) = self.tree.conn_node(c) {
                    self.tree.select(n);
                    self.set_focus(Focus::Tree);
                    self.activate(n, true);
                }
            }
            Pick::Key(c, k) => {
                if let Some(n) = self
                    .tree
                    .nodes
                    .iter()
                    .position(|x| matches!(&x.kind, crate::tree::NodeKind::Key { conn, key } if *conn == c && *key == k))
                {
                    self.tree.select(n);
                    self.activate(n, true);
                }
            }
        }
    }

    /// 트리에서 그 테이블을 골라(읽혀 있으면) 테이블 보기로 연다.
    fn open_table_from_tree(&mut self, c: usize, schema: String, table: String) {
        if let Some(n) = self.tree.nodes.iter().position(
            |x| matches!(&x.kind, crate::tree::NodeKind::Table { conn, schema: s, name } if *conn == c && *s == schema && *name == table),
        ) {
            self.tree.select(n);
        }
        self.open_table(c, TableRef { schema, table }, String::new());
    }

    /// 객체 찾기(R22.3): 목록을 읽은 접속의 테이블·뷰·열·함수와 트리에 읽힌 Redis 키. 빈 입력이면 최근 연 테이블이 먼저다.
    pub fn open_finder(&mut self) {
        let mut items = Vec::new();
        let mut map: HashMap<String, Pick> = HashMap::new();
        let mut seen = Vec::new();
        for rt in self.session.recent_tables().into_iter().take(20) {
            let Some(c) = self.conn_by_uuid(&rt.conn) else { continue };
            let l = self.table_label(c, &rt.schema, &rt.table);
            seen.push(l.clone());
            items.push(l.clone());
            map.insert(l, Pick::Table(c, rt.schema, rt.table));
        }
        for (c, s, t) in self.catalog_tables() {
            let l = self.table_label(c, &s, &t);
            if seen.contains(&l) || map.contains_key(&l) {
                continue;
            }
            items.push(l.clone());
            map.insert(l, Pick::Table(c, s.clone(), t.clone()));
            // 열(R56)은 테이블로 연다
            if let Some(cat) = self.catalogs.get(&c) {
                for sc in cat.schemas.iter().filter(|x| x.name == s) {
                    for tb in sc.tables.iter().filter(|x| x.name == t) {
                        for col in &tb.columns {
                            let l = format!("{} · {s}.{t}.{} ({})", self.conns[c].name, col.name, col.typ);
                            items.push(l.clone());
                            map.insert(l, Pick::Table(c, s.clone(), t.clone()));
                        }
                    }
                }
            }
        }
        for n in &self.tree.nodes {
            if let crate::tree::NodeKind::Key { conn, key } = &n.kind {
                let l = format!("[{}] {} · {key}", self.conns[*conn].env, self.conns[*conn].name);
                items.push(l.clone());
                map.insert(l, Pick::Key(*conn, key.clone()));
            }
        }
        if items.is_empty() {
            self.status = "찾을 목록이 없음 (접속을 펼쳐 목록을 읽으세요)".into();
            return;
        }
        self.picker("객체 찾기", items, move |a, label| {
            if let Some(p) = map.remove(&label) {
                a.palette_pick(p);
            }
        });
    }
}

fn env_rank(e: &str) -> u8 {
    match e {
        "local" => 0,
        "stg" => 1,
        _ => 2,
    }
}
