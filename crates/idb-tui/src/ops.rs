//! 트리의 운영 동작: Redis 패턴 일괄 삭제(R42), 데이터 검색(R33.2), 다른 접속과 스키마 비교(R53.2).

use idb_engine::{EngineError, PatternKeys, Request, ResultSet};
use serde_json::Value;

use crate::app::{App, Overlay, Pend};
use crate::help::Focus;
use crate::tree::NodeKind;

/// 한 번에 UNLINK 할 키 수(R42.1)
const UNLINK_BATCH: usize = 500;
/// 확인 창에 미리 보일 키 수
const PREVIEW_KEYS: usize = 20;

/// 지우는 중인 일괄 삭제다. esc 면 보낸 배치의 답까지 받고 멈춘다.
pub struct Deleting {
    pub conn: usize,
    pub left: Vec<String>,
    pub done: i64,
    pub total: usize,
    pub stop: bool,
}

/// glob 글자(`\ * ? [ ]`)를 감싼다. 이름공간을 패턴으로 채울 때 쓴다.
pub fn glob_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if matches!(c, '\\' | '*' | '?' | '[' | ']') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `*`·`?` 로만 되어 있고 `*` 가 든 패턴은 모든 키라 FLUSHDB 와 같다.
pub fn flush_like(p: &str) -> bool {
    p.contains('*') && p.chars().all(|c| c == '*' || c == '?')
}

impl App {
    /// 트리 X: 패턴을 받아 맞는 키를 세고 미리 보인 뒤 묻는다(R42).
    pub(crate) fn pattern_delete(&mut self, n: usize) {
        let node = self.tree.node(n).clone();
        let (conn, fill) = match &node.kind {
            NodeKind::Conn(c) if self.is_redis(*c) => (*c, self.tree.patterns.get(c).cloned().unwrap_or_else(|| "*".into())),
            NodeKind::Folder { conn, prefix } => (*conn, format!("{}*", glob_escape(prefix))),
            _ => return,
        };
        if self.deleting.is_some() {
            self.status = "키를 지우는 중".into();
            return;
        }
        self.prompt("지울 키 패턴: ", &fill, move |a, pat| {
            let pat = pat.trim().to_string();
            if pat.is_empty() {
                return;
            }
            let name = a.conns[conn].name.clone();
            a.call(
                Request::RedisPatternKeys {
                    conn: name,
                    pattern: pat.clone(),
                },
                Pend::PatternKeys { conn, pattern: pat },
            );
            a.status = "키를 세는 중…".into();
        });
    }

    pub(crate) fn pattern_counted(&mut self, conn: usize, pattern: String, res: Result<Value, EngineError>) {
        let pk: PatternKeys = match res.and_then(|v| serde_json::from_value(v).map_err(|e| EngineError::run(e.to_string()))) {
            Ok(p) => p,
            Err(e) => {
                self.status = e.error;
                return;
            }
        };
        if pk.truncated {
            self.status = "키가 100,000개를 넘어 지우지 않음 (패턴을 좁히세요)".into();
            return;
        }
        if pk.keys.is_empty() {
            self.status = format!("맞는 키 없음: {pattern}");
            return;
        }
        let c = &self.conns[conn];
        let n = pk.keys.len();
        let mut q = if c.env == "prod" {
            format!("prod({})에서 키 {n}개 지움", c.name)
        } else {
            format!("{}에서 키 {n}개 지움", c.name)
        };
        if flush_like(&pattern) {
            q.push_str(" · 위험: FLUSHDB 와 같음");
        }
        let mut preview: Vec<String> = pk.keys.iter().take(PREVIEW_KEYS).cloned().collect();
        if n > PREVIEW_KEYS {
            preview.push(format!("… 외 {}개", n - PREVIEW_KEYS));
        }
        let keys = pk.keys;
        self.overlay = Overlay::Confirm {
            q,
            then: Box::new(move |a| {
                a.deleting = Some(Deleting {
                    conn,
                    total: keys.len(),
                    left: keys,
                    done: 0,
                    stop: false,
                });
                a.unlink_next();
            }),
            preview,
            top: 0,
        };
    }

    /// 다음 배치를 보낸다. 멈췄거나 다 지웠으면 끝을 알리고 트리를 다시 읽는다.
    pub(crate) fn unlink_next(&mut self) {
        let Some(d) = &mut self.deleting else { return };
        if d.left.is_empty() || d.stop {
            let (conn, done, stop) = (d.conn, d.done, d.stop);
            self.deleting = None;
            self.status = if stop {
                format!("멈춤: {done}개 지움")
            } else {
                format!("{done}개 지움")
            };
            if let Some(n) = self.tree.conn_node(conn) {
                self.load_keys(conn, n);
            }
            return;
        }
        let batch: Vec<String> = d.left.drain(..d.left.len().min(UNLINK_BATCH)).collect();
        let conn = d.conn;
        self.status = format!("지우는 중 {}/{} (esc 멈춤)", d.done, d.total);
        let prod = self.conns[conn].env == "prod";
        let name = self.conns[conn].name.clone();
        self.call(
            Request::RedisUnlink {
                conn: name,
                keys: batch,
                confirm: true,
                confirm_prod: prod,
            },
            Pend::Unlink,
        );
    }

    pub(crate) fn unlinked(&mut self, res: Result<Value, EngineError>) {
        match res {
            Ok(v) => {
                let n = v.get("rows").and_then(Value::as_i64).unwrap_or(0);
                if let Some(d) = &mut self.deleting {
                    d.done += n;
                }
                self.unlink_next();
            }
            Err(e) => {
                let done = self.deleting.take().map_or(0, |d| d.done);
                self.status = format!("지우다 멈춤({done}개 지움): {}", e.error);
            }
        }
    }

    /// 트리 F: 스키마의 글자 열에서 찾는다(R33.2). prod 면 먼저 묻는다.
    pub(crate) fn data_search(&mut self, n: usize) {
        let node = self.tree.node(n).clone();
        let (conn, schema) = match &node.kind {
            NodeKind::Conn(c) if self.is_pg(*c) => (*c, self.schema_of.get(c).cloned().unwrap_or_else(|| "public".into())),
            NodeKind::Schema { conn, name } => (*conn, name.clone()),
            _ => return,
        };
        self.prompt("찾을 글: ", "", move |a, text| {
            if text.is_empty() {
                return;
            }
            let tables = a
                .catalogs
                .get(&conn)
                .and_then(|cat| cat.schemas.iter().find(|s| s.name == schema))
                .map(|s| s.tables.len());
            let go = move |a: &mut App| {
                let i = a.new_tab();
                a.set_conn(i, Some(conn));
                a.tabs[i].title = format!("검색 {text}");
                let id = a.tabs[i].id;
                let name = a.conns[conn].name.clone();
                let job = a.call(
                    Request::Search {
                        conn: name,
                        schema: schema.clone(),
                        text,
                    },
                    Pend::Preview {
                        tab: id,
                        title: "검색".into(),
                    },
                );
                a.tabs[i].running = Some(job);
                a.tabs[i].result_conn = Some(conn);
                a.set_focus(Focus::Result);
            };
            let c = &a.conns[conn];
            if c.env == "prod" {
                let q = match tables {
                    Some(t) => format!("prod({}) 테이블 {t}개 전체 검색", c.name),
                    None => format!("prod({}) 테이블 전체 검색", c.name),
                };
                a.confirm(q, go);
            } else {
                go(a);
            }
        });
    }

    /// 트리 =: 다른 PostgreSQL 접속과 스키마를 비교한다(R53.2).
    pub(crate) fn schema_compare(&mut self, n: usize) {
        let node = self.tree.node(n).clone();
        let (conn, schema) = match &node.kind {
            NodeKind::Conn(c) if self.is_pg(*c) => (*c, "public".to_string()),
            NodeKind::Schema { conn, name } => (*conn, name.clone()),
            _ => return,
        };
        let others: Vec<String> = self
            .conns
            .iter()
            .enumerate()
            .filter(|(i, c)| *i != conn && c.kind == "postgresql")
            .map(|(_, c)| c.name.clone())
            .collect();
        if others.is_empty() {
            self.status = "비교할 다른 PostgreSQL 접속이 없음".into();
            return;
        }
        self.picker(format!("{} 와 비교할 접속", self.conns[conn].name), others, move |a, to| {
            let i = a.new_tab();
            a.set_conn(i, Some(conn));
            a.tabs[i].title = format!("비교 {to}");
            let id = a.tabs[i].id;
            let name = a.conns[conn].name.clone();
            let job = a.call(
                Request::SchemaDiff {
                    conn: name,
                    to,
                    schema: schema.clone(),
                    to_schema: schema,
                },
                Pend::Diff { tab: id },
            );
            a.tabs[i].running = Some(job);
            a.set_focus(Focus::Result);
        });
    }

    pub(crate) fn diff_done(&mut self, tab: u64, res: Result<Value, EngineError>) {
        let r = res.and_then(|v| serde_json::from_value::<ResultSet>(v).map_err(|e| EngineError::run(e.to_string())));
        let Some(t) = self.tab_by_id(tab) else { return };
        t.running = None;
        match r {
            Ok(rs) => {
                t.summary = if rs.rows.is_empty() {
                    "스키마가 같음".into()
                } else {
                    format!("다른 곳 {}", rs.rows.len())
                };
                t.grid = Some(crate::grid::Grid::new(rs));
                t.err.clear();
                self.status = self
                    .tabs
                    .iter()
                    .find(|t| t.id == tab)
                    .map(|t| t.summary.clone())
                    .unwrap_or_default();
            }
            Err(e) => {
                t.err = e.error;
                t.summary = "스키마 비교 실패".into();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 이름공간_패턴은_glob_글자를_감싸고_모든_키_패턴은_경고한다() {
        assert_eq!(format!("{}*", glob_escape("a*b:")), "a\\*b:*");
        assert!(flush_like("*") && flush_like("?*") && flush_like("**"));
        assert!(!flush_like("?") && !flush_like("vg:*"));
    }
}
