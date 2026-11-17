//! 접속 트리. 폴더(옮긴 폴더가 없으면 그룹, 그것도 없으면 기타 — 엔진이 채운다) 아래 접속이다.
//! 폴더는 처음 나온 순서를 지키고, 접든 펼치든 그리는 쪽은 `rows()` 의 평평한 줄만 본다.

use std::collections::BTreeSet;

use idb_engine::ConnSummary;

use super::safety::{Tone, tone};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowKind {
    Folder {
        expanded: bool,
        count: usize,
    },
    /// conns 의 자리
    Conn(usize),
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

    /// 이름·호스트·DB·그룹에 글이 든 접속만 보인다(대소문자 무시). 거르는 동안은 접힌 폴더도 편다.
    pub fn set_filter(&mut self, f: &str) {
        self.filter = f.trim().to_lowercase();
    }

    fn matches(&self, c: &ConnSummary) -> bool {
        self.filter.is_empty()
            || [&c.name, &c.host, &c.db, &c.group, &c.kind]
                .iter()
                .any(|s| s.to_lowercase().contains(&self.filter))
    }

    pub fn rows(&self) -> Vec<Row> {
        let mut out = Vec::new();
        for f in &self.folders {
            let members: Vec<usize> = (0..self.conns.len())
                .filter(|&i| self.conns[i].folder == *f && self.matches(&self.conns[i]))
                .collect();
            if members.is_empty() {
                continue;
            }
            let expanded = !self.filter.is_empty() || !self.collapsed.contains(f);
            // 폴더 안에 prod 가 있으면 폴더도 빨갛게 둬 접혀 있어도 보이게 한다
            let folder_tone = if members.iter().any(|&i| tone(&self.conns[i].env) == Tone::Prod) {
                Tone::Prod
            } else {
                Tone::Local
            };
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
            if !expanded {
                continue;
            }
            for i in members {
                let c = &self.conns[i];
                out.push(Row {
                    kind: RowKind::Conn(i),
                    depth: 1,
                    label: c.name.clone(),
                    detail: conn_detail(c),
                    tone: tone(&c.env),
                });
            }
        }
        out
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
            uuid: String::new(),
        }
    }

    fn sample() -> ConnTree {
        ConnTree::new(vec![
            conn("app-local", "app", "local", "postgresql"),
            conn("cache", "기타", "local", "redis"),
            conn("app-prod", "app", "prod", "postgresql"),
        ])
    }

    #[test]
    fn 폴더는_처음_나온_순서이고_접속은_그_아래다() {
        let rows = sample().rows();
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, vec!["app", "app-local", "app-prod", "기타", "cache"]);
        assert_eq!(rows[0].kind, RowKind::Folder { expanded: true, count: 2 });
        assert_eq!(rows[2].kind, RowKind::Conn(2));
        assert_eq!(rows[1].detail, "pg · h/d");
        assert_eq!(rows[4].detail, "redis · h/d");
    }

    #[test]
    fn prod_접속과_그_폴더는_빨갛게_표시한다() {
        let rows = sample().rows();
        assert_eq!(rows[0].tone, Tone::Prod);
        assert_eq!(rows[1].tone, Tone::Local);
        assert_eq!(rows[2].tone, Tone::Prod);
        assert_eq!(rows[3].tone, Tone::Local);
    }

    #[test]
    fn 접은_폴더는_접속을_숨기고_다시_펴면_보인다() {
        let mut t = sample();
        t.toggle("app");
        let rows = t.rows();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].kind, RowKind::Folder { expanded: false, count: 2 });
        t.toggle("app");
        assert_eq!(t.rows().len(), 5);
    }

    #[test]
    fn 거르면_맞는_접속만_남고_접힌_폴더도_편다() {
        let mut t = sample();
        t.toggle("app");
        t.set_filter("PROD");
        let labels: Vec<String> = t.rows().into_iter().map(|r| r.label).collect();
        assert_eq!(labels, vec!["app", "app-prod"]);
        t.set_filter("redis");
        let labels: Vec<String> = t.rows().into_iter().map(|r| r.label).collect();
        assert_eq!(labels, vec!["기타", "cache"]);
    }
}
