//! 동작: 트리 펼치기·테이블 보기·실행·트랜잭션·제출·되돌리기와 Engine 답 처리.

use std::collections::BTreeMap;
use std::time::Instant;

use idb_core::sqlsafe::{is_read_only_redis, is_read_only_sql, split_args, split_statements, statement_at};
use idb_engine::{
    AppliedResult, CODE_REFUSED, Catalog, EngineError, Fk, LinkedResult, LockState, Object, REASON_CONFIRM, REASON_CONFIRM_DANGER,
    REASON_CONFIRM_PROD, REASON_PROD_LOCKED, RawValue, RedisEdit, RedisEditResult, Request, ResultSet, RunRequest, ScanResult,
    SessionState, TablePage, TablePageRequest, TableRef, TxState, User360Item,
};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::app::{App, Overlay, Pend, Pin, RedisView, Tab, UNDO_KEEP, UndoEntry};
use crate::grid::{Act, Grid, Screen};
use crate::help::Focus;
use crate::tableview::TableView;
use crate::tree::NodeKind;

const MAX_PINS: usize = 5;
const REDIS_SLOWLOG_N: i64 = 50;
const REDIS_BIGKEY_SAMPLE: usize = 1000;
const REDIS_BIGKEY_TOP: usize = 50;
const COMMIT_ROLLED_BACK: &str = "COMMIT 이 ROLLBACK 으로 끝남(트랜잭션 오류 상태)";

/// 편집기 실행 하나(여러 문장이면 차례로, R7.2). 확인 표시는 처음 받은 것을 끝까지 쓴다(prod 확인 한 번).
pub struct Script {
    pub tab: u64,
    pub conn: usize,
    pub stmts: Vec<String>,
    pub idx: usize,
    pub confirm_prod: bool,
    pub confirm_danger: bool,
    /// 읽기 전용이 아닌 EXPLAIN ANALYZE 확인(R9.2, serve reason confirm_analyze)
    pub confirm_analyze: bool,
    pub params: BTreeMap<String, String>,
    pub redis: bool,
    pub results: Vec<ResultSet>,
    /// EXPLAIN(ANALYZE 면 true)
    pub explain: Option<bool>,
    pub started: Instant,
    /// 정렬·거르기 다시 읽기·더 읽기다(R14.5·R14.7)
    pub again: Option<Again>,
}

pub enum Again {
    Keep {
        sort: Option<(usize, bool)>,
        filter: String,
        col: usize,
    },
    More,
}

/// 더 읽기로 모으는 행 상한이다(R14.7). 칸을 모두 메모리에 들고 있어서다
const MORE_LIMIT: usize = 10_000;

fn decode<T: DeserializeOwned>(v: Value) -> Result<T, EngineError> {
    serde_json::from_value(v).map_err(|e| EngineError::run(format!("bad result: {e}")))
}

/// Go time.Duration 의 String 꼴(12ms·1.234s·1m2.5s)이다.
pub fn fmt_dur(ms: u64) -> String {
    if ms < 1000 {
        return format!("{ms}ms");
    }
    let trim = |s: String| s.trim_end_matches('0').trim_end_matches('.').to_string();
    if ms < 60_000 {
        return format!("{}s", trim(format!("{:.3}", ms as f64 / 1000.0)));
    }
    let m = ms / 60_000;
    format!("{m}m{}s", trim(format!("{:.3}", (ms % 60_000) as f64 / 1000.0)))
}

fn one_line(s: &str) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() > 200 {
        format!("{}…", s.chars().take(200).collect::<String>())
    } else {
        s
    }
}

impl App {
    // ─── 답 ───

    pub(crate) fn handle(&mut self, id: u64, p: Pend, res: Result<Value, EngineError>) {
        match p {
            Pend::Schemas { conn, node } => match res.and_then(decode::<Vec<String>>) {
                Ok(list) => {
                    let kids = list
                        .into_iter()
                        .filter(|s| !s.starts_with("pg_") && s != "information_schema")
                        .map(|s| (NodeKind::Schema { conn, name: s.clone() }, s))
                        .collect();
                    let q = self.queries_for(conn);
                    self.tree.set_conn_children(node, q, kids);
                }
                Err(e) => self.status = format!("스키마 읽기 실패: {}", one_line(&e.error)),
            },
            Pend::Tables { conn, schema, node } => match res.and_then(decode::<Vec<String>>) {
                Ok(list) => {
                    let kids = list
                        .into_iter()
                        .map(|t| {
                            (
                                NodeKind::Table {
                                    conn,
                                    schema: schema.clone(),
                                    name: t.clone(),
                                },
                                t,
                            )
                        })
                        .collect();
                    self.tree.set_children(node, kids);
                }
                Err(e) => self.status = format!("테이블 읽기 실패: {}", one_line(&e.error)),
            },
            Pend::Scan { conn, node } => match res.and_then(decode::<ScanResult>) {
                Ok(s) => {
                    let q = self.queries_for(conn);
                    self.tree.set_conn_children(node, q, Vec::new());
                    // 쿼리 묶음을 앞에 둔 채 키 폴더를 뒤에 단다
                    let keep = self.tree.nodes[node].children.clone();
                    self.tree.set_keys(node, conn, "", &s.keys, s.more);
                    let mut kids = keep;
                    kids.extend(self.tree.nodes[node].children.clone());
                    self.tree.nodes[node].children = kids;
                    self.tree.rebuild();
                }
                Err(e) => self.status = format!("SCAN 실패: {}", one_line(&e.error)),
            },
            Pend::Catalog { conn } => {
                self.catalog_loading.remove(&conn);
                match res.and_then(decode::<Catalog>) {
                    Ok(c) => {
                        self.catalogs.insert(conn, c);
                    }
                    Err(e) => self.status = format!("자동완성 목록 실패: {}", one_line(&e.error)),
                }
            }
            Pend::Run(s) => self.script_done(id, *s, res),
            Pend::Page { tab } => self.page_done(id, tab, res),
            Pend::Count { tab } => {
                let r = res.and_then(decode::<TablePage>);
                let Some(t) = self.tab_by_id(tab) else { return };
                match r {
                    Ok(p) => {
                        if let (Some(n), Some(v)) = (p.count, t.view.as_mut()) {
                            v.set_total(n, true);
                            self.status = format!("전체 {}행", idb_core::textutil::group_digits(n));
                        }
                    }
                    Err(e) => self.status = format!("실패: {}", one_line(&e.error)),
                }
            }
            Pend::Describe { tab } => self.result_done(tab, res.and_then(decode::<ResultSet>), "구조"),
            Pend::Ops { tab, title } => self.result_done(tab, res.and_then(decode::<ResultSet>), &title),
            Pend::Preview { tab, title } => self.result_done(tab, res.and_then(decode::<ResultSet>), &title),
            Pend::Ddl { conn } => match res.and_then(decode::<String>) {
                Ok(ddl) => {
                    let i = self.new_tab();
                    self.set_conn(i, Some(conn));
                    self.tabs[i].editor.set_value(&ddl);
                    self.tabs[i].editor.typed = false;
                    self.set_focus(Focus::Editor);
                }
                Err(e) => self.status = format!("DDL 실패: {}", one_line(&e.error)),
            },
            Pend::Tx { conn, op, then } => match res.and_then(decode::<TxState>) {
                Ok(st) => {
                    if op != "begin" {
                        self.record(conn, &op.to_uppercase(), 0, None, "");
                    }
                    self.note_tx(conn, st.in_tx);
                    if op == "commit" && st.text != "COMMIT" {
                        self.status = COMMIT_ROLLED_BACK.into();
                        return;
                    }
                    self.status = st.text.clone();
                    if let Some(f) = then {
                        f(self);
                    }
                }
                Err(e) => self.refused(e, move |a, cp| {
                    let name = a.conns[conn].name.clone();
                    a.call(
                        Request::TxCommit {
                            conn: name,
                            confirm_prod: cp,
                        },
                        Pend::Tx { conn, op, then: None },
                    );
                }),
            },
            Pend::Lock { conn, toggle } => match res.and_then(decode::<LockState>) {
                Ok(st) => {
                    if toggle {
                        self.status = if st.locked {
                            format!("prod({}) 쓰기 잠금", st.conn)
                        } else {
                            format!("prod({}) 쓰기 열림 {}분", st.conn, st.unlock_min)
                        };
                    }
                    self.locks.insert(conn, st);
                }
                Err(e) => self.status = one_line(&e.error),
            },
            Pend::Apply { tab, cs } => self.apply_done(tab, cs, res),
            Pend::Undo { tab } => match res.and_then(decode::<AppliedResult>) {
                Ok(r) => {
                    if let Some(t) = self.tab_by_id(tab) {
                        t.undo.pop();
                    }
                    self.status = format!("되돌림: {}", one_line(&r.result.text));
                    self.refresh_tab(tab);
                }
                Err(e) => self.status = format!("되돌리기 실패: {} · 묶음은 남김", one_line(&e.error)),
            },
            Pend::RedisValue { tab } => self.result_done(tab, res.and_then(decode::<ResultSet>), "값"),
            Pend::RedisRaw { tab } => {
                let r = res.and_then(decode::<RawValue>);
                let Some(t) = self.tab_by_id(tab) else { return };
                t.running = None;
                match r {
                    Ok(v) => {
                        let note = if v.truncated { " · 일부만 읽음" } else { "" };
                        t.summary = format!("{} · {} · TTL {}{note}", v.key, v.typ, v.ttl_secs);
                        if let Some(rv) = &mut t.redis {
                            rv.sel = rv.sel.min(redis_len(&v).saturating_sub(1));
                            rv.raw = Some(v);
                        }
                        t.err.clear();
                    }
                    Err(e) => {
                        t.err = e.error;
                        t.redis = None;
                    }
                }
            }
            Pend::RedisEdit { tab } => match res.and_then(decode::<RedisEditResult>) {
                Err(e) if e.code == CODE_REFUSED && e.reason.as_deref() != Some(REASON_PROD_LOCKED) => {
                    let Some((tab, conn, edit, cp, cd)) = self.redis_retry.remove(&id) else {
                        return;
                    };
                    let reason = e.reason.clone().unwrap_or_default();
                    let q = e.error.trim_end_matches(": y/N").to_string();
                    self.confirm(q, move |a| {
                        let (cp, cd) = (cp || reason != REASON_CONFIRM_DANGER, cd || reason == REASON_CONFIRM_DANGER);
                        a.send_redis_edit(tab, conn, edit, cp, cd);
                    });
                }
                Ok(r) => {
                    self.status = r.status.clone();
                    if let Some(t) = self.tab_by_id(tab) {
                        if r.key.is_empty() {
                            t.redis = None;
                            t.summary = "키 지움".into();
                        } else if let Some(rv) = &mut t.redis {
                            rv.key = r.key.clone();
                        }
                    }
                    if !r.key.is_empty() {
                        self.load_redis_raw(tab);
                    }
                }
                Err(e) => self.status = one_line(&e.error),
            },
            Pend::Enums { conn } => {
                if let Ok(m) = res.and_then(decode::<std::collections::HashMap<String, Vec<String>>>) {
                    for (k, v) in m {
                        self.enums.insert((conn, k), v);
                    }
                }
            }
            Pend::Resolve { tab, conn } => match res.and_then(decode::<Object>) {
                Ok(o) if o.kind == "function" => {
                    let name = self.conns[conn].name.clone();
                    self.call(
                        Request::FunctionDef {
                            conn: name,
                            schema: o.schema,
                            name: o.name,
                        },
                        Pend::FuncDef { tab },
                    );
                }
                Ok(o) => {
                    if let Some(n) = self.find_table_node(conn, &o.schema, &o.name) {
                        self.tree.select(n);
                    }
                    let name = self.conns[conn].name.clone();
                    if let Some(i) = self.tab_index(tab) {
                        self.tabs[i].running = Some(self.call(
                            Request::Describe {
                                conn: name,
                                schema: o.schema,
                                table: o.name,
                            },
                            Pend::Describe { tab },
                        ));
                    }
                }
                Err(e) => self.status = one_line(&e.error).replace("definition not found:", "정의를 찾지 못함:"),
            },
            Pend::FuncDef { tab } => match res.and_then(decode::<String>) {
                Ok(def) => {
                    if let Some(t) = self.tab_by_id(tab) {
                        t.grid = None;
                        t.view = None;
                        t.err.clear();
                        t.text = def;
                        t.summary = "함수 정의".into();
                    }
                }
                Err(e) => self.status = one_line(&e.error),
            },
            Pend::Schemas2 { conn } => match res.and_then(decode::<Vec<String>>) {
                Ok(list) => {
                    let list: Vec<String> = list
                        .into_iter()
                        .filter(|s| !s.starts_with("pg_") && s != "information_schema")
                        .collect();
                    self.picker("스키마 (search_path)", list, move |a, s| {
                        let name = a.conns[conn].name.clone();
                        a.call(
                            Request::SetSearchPath {
                                conn: name,
                                schema: s.clone(),
                            },
                            Pend::SearchPath { conn, schema: s },
                        );
                    });
                }
                Err(e) => self.status = one_line(&e.error),
            },
            Pend::SearchPath { conn, schema } => match res.and_then(decode::<SessionState>) {
                Ok(st) => {
                    self.schema_of.insert(conn, schema);
                    self.note_tx(conn, st.in_tx);
                    self.status = format!("search_path: {}", st.search_path);
                }
                Err(e) => self.status = one_line(&e.error),
            },
            Pend::Databases { conn } => match res.and_then(decode::<Vec<String>>) {
                Ok(list) => self.picker("데이터베이스", list, move |a, db| a.switch_db(conn, db)),
                Err(e) => self.status = one_line(&e.error),
            },
            Pend::SwitchDb { conn } => match res.and_then(decode::<SessionState>) {
                Ok(st) => {
                    if st.db.is_empty() || st.db == self.conns[conn].db {
                        self.db_over.remove(&conn);
                    } else {
                        self.db_over.insert(conn, st.db.clone());
                    }
                    self.catalogs.remove(&conn);
                    self.enums.retain(|(c, _), _| *c != conn);
                    self.schema_of.remove(&conn);
                    for t in self.tabs.iter_mut().filter(|t| t.result_conn == Some(conn)) {
                        t.result_conn = None;
                    }
                    if let Some(n) = self.tree.conn_node(conn) {
                        self.tree.nodes[n].loaded = false;
                        self.tree.nodes[n].children.clear();
                        self.tree.nodes[n].expanded = false;
                        self.tree.rebuild();
                    }
                    self.status = format!("데이터베이스: {}", st.db);
                    self.ensure_catalog(conn);
                }
                Err(e) => self.status = one_line(&e.error),
            },
            Pend::Fk { tab, conn, incoming, row } => match res.and_then(decode::<Vec<Fk>>) {
                Ok(fks) => self.fk_pick(tab, conn, incoming, row, fks),
                Err(e) => self.status = one_line(&e.error),
            },
            Pend::Signal { tab } => match res {
                Err(e) if e.code == CODE_REFUSED && e.reason.as_deref() != Some(REASON_PROD_LOCKED) => {
                    let Some((tab, conn, pid, terminate)) = self.signal_retry.remove(&id) else {
                        return;
                    };
                    let q = e.error.trim_end_matches(": y/N").to_string();
                    self.confirm(q, move |a| a.send_signal(tab, conn, pid, terminate, true, true));
                }
                Ok(_) => {
                    self.status = "보냄".into();
                    self.refresh_tab(tab);
                }
                Err(e) => self.status = one_line(&e.error),
            },
            Pend::Linked { tab, conn: _ } => match res.and_then(decode::<LinkedResult>) {
                Ok(l) => {
                    let summary = l.result.text.clone();
                    self.result_done(tab, Ok(l.result), "연결된 곳");
                    self.status = summary;
                }
                Err(e) => self.result_done(tab, Err(e), "연결된 곳"),
            },
            Pend::PatternKeys { conn, pattern } => self.pattern_counted(conn, pattern, res),
            Pend::Unlink => self.unlinked(res),
            Pend::Diff { tab } => self.diff_done(tab, res),
            Pend::SourceKids { conn, node } => self.source_kids_done(conn, node, res),
            Pend::Source(job) => self.source_done(id, *job, res),
            Pend::Health { tab, round, conn } => self.health_done(id, tab, round, conn, res),
            Pend::Outbox { tab, round } => self.outbox_done(tab, round, res),
            Pend::Flows { tab, round } => self.flows_done(tab, round, res),
            Pend::BoardList => self.board_list_done(res),
            Pend::Board { tab, round } => self.board_done(tab, round, res),
            Pend::User360 { tab } => match res.and_then(decode::<Vec<User360Item>>) {
                Ok(items) => {
                    let Some(t) = self.tab_by_id(tab) else { return };
                    t.running = None;
                    t.subs.clear();
                    let n = items.len();
                    for it in items {
                        let rs = it.result.unwrap_or(ResultSet {
                            text: it.error.unwrap_or_default(),
                            ..ResultSet::default()
                        });
                        t.subs.push(Pin {
                            title: format!("{} · {}", it.title, it.conn),
                            grid: Grid::new(rs),
                            pinned: false,
                        });
                    }
                    t.sub = if n > 0 { Some(0) } else { None };
                    t.grid = t.subs.first().map(|p| Grid::new(p.grid.res.clone()));
                    t.summary = format!("조회 {n}곳");
                }
                Err(e) => self.result_done(tab, Err(e), "사용자"),
            },
        }
    }

    /// 거절(code 3)이면 확인 창을 띄우고 y 면 again(확인 표시)을 부른다. 그 밖의 오류는 상태 줄에 보인다.
    fn refused(&mut self, e: EngineError, again: impl FnOnce(&mut App, bool) + 'static) {
        if e.code == CODE_REFUSED && e.reason.as_deref() == Some(REASON_CONFIRM_PROD) {
            self.confirm(e.error.trim_end_matches(": y/N").to_string(), move |a| again(a, true));
            return;
        }
        self.status = one_line(&e.error);
    }

    pub(crate) fn note_tx(&mut self, conn: usize, open: bool) {
        if open {
            self.in_tx.entry(conn).or_insert_with(Instant::now);
        } else {
            self.in_tx.remove(&conn);
        }
    }

    /// 표·글 결과 하나를 탭에 붙인다.
    fn result_done(&mut self, tab: u64, r: Result<ResultSet, EngineError>, what: &str) {
        let cur_id = self.cur().id;
        let Some(t) = self.tab_by_id(tab) else { return };
        t.running = None;
        t.subs.retain(|p| p.pinned);
        t.sub = None;
        match r {
            Ok(rs) => {
                t.err.clear();
                t.summary = summary_of(&rs);
                if rs.cols.is_empty() {
                    t.grid = None;
                    t.text = rs.text;
                } else {
                    t.text.clear();
                    t.grid = Some(Grid::new(rs));
                }
                let s = t.summary.clone();
                let title = t.display_name();
                self.status = if tab == cur_id { s } else { format!("{title}: {s}") };
            }
            Err(e) => {
                t.err = e.error.clone();
                t.summary = "실패".into();
                self.status = format!("{what} 실패: {}", one_line(&e.error));
            }
        }
    }

    // ─── 트리 ───

    pub(crate) fn queries_for(&self, conn: usize) -> Option<(String, Vec<String>)> {
        let f = self.files.as_ref()?;
        let files = f.for_conn(&self.conns[conn].uuid);
        if files.is_empty() {
            return None;
        }
        Some((
            format!("쿼리 ({})", files.len()),
            files.into_iter().map(|s| s.trim_start_matches("queries/").to_string()).collect(),
        ))
    }

    fn find_table_node(&self, conn: usize, schema: &str, table: &str) -> Option<usize> {
        self.tree
            .nodes
            .iter()
            .position(|n| matches!(&n.kind, NodeKind::Table { conn: c, schema: s, name } if *c == conn && s == schema && name == table))
    }

    pub(crate) fn load_keys(&mut self, conn: usize, node: usize) {
        let pattern = self.tree.patterns.get(&conn).cloned().unwrap_or_else(|| "*".into());
        let name = self.conns[conn].name.clone();
        self.call(Request::RedisScan { conn: name, pattern }, Pend::Scan { conn, node });
    }

    fn load_children(&mut self, n: usize) {
        let node = self.tree.node(n).clone();
        match &node.kind {
            NodeKind::Conn(c) => {
                let c = *c;
                let name = self.conns[c].name.clone();
                if self.is_pg(c) {
                    self.call(Request::Schemas { conn: name }, Pend::Schemas { conn: c, node: n });
                    self.ensure_catalog(c);
                } else if self.is_redis(c) {
                    self.load_keys(c, n);
                } else if self.is_source(c) {
                    self.load_source_children(n);
                } else {
                    let q = self.queries_for(c);
                    self.tree.set_conn_children(n, q, Vec::new());
                }
            }
            NodeKind::Source { .. } => self.load_source_children(n),
            NodeKind::Schema { conn, name } => {
                let cn = self.conns[*conn].name.clone();
                self.call(
                    Request::Tables {
                        conn: cn,
                        schema: name.clone(),
                    },
                    Pend::Tables {
                        conn: *conn,
                        schema: name.clone(),
                        node: n,
                    },
                );
            }
            _ => {}
        }
    }

    /// 트리 Enter·l: 펼치기·테이블 보기·값 편집·파일 열기(R2·R3.1·R4.1·R43). enter 면 접속 고르기로 최근 파일을 연다(R44).
    pub(crate) fn activate(&mut self, n: usize, enter: bool) {
        let node = self.tree.node(n).clone();
        match &node.kind {
            NodeKind::Table { conn, schema, name } => {
                let r = TableRef {
                    schema: schema.clone(),
                    table: name.clone(),
                };
                self.open_table(*conn, r, String::new());
            }
            NodeKind::Key { conn, key } => self.open_redis_key(*conn, key.clone()),
            NodeKind::File(rel) => self.open_query_file(&format!("queries/{rel}")),
            NodeKind::Info => {}
            // Enter 는 미리 보기, l 은 펼치기다(R60.3)
            NodeKind::Source { openable: true, .. } if enter => self.open_source_node(n),
            NodeKind::Source { conn, kids: false, .. } => {
                let (ti, c) = (self.tab, *conn);
                if self.tabs[ti].conn != Some(c) {
                    self.set_conn(ti, Some(c));
                }
            }
            _ => {
                let opening = !node.expanded;
                self.tree.nodes[n].expanded = opening;
                if opening && !node.loaded {
                    self.load_children(n);
                }
                self.tree.rebuild();
                if let NodeKind::Source { conn, .. } = node.kind
                    && self.tabs[self.tab].conn != Some(conn)
                {
                    let ti = self.tab;
                    self.set_conn(ti, Some(conn));
                }
                if let NodeKind::Conn(c) = node.kind {
                    let ti = self.tab;
                    if self.tabs[ti].conn != Some(c) {
                        self.set_conn(ti, Some(c));
                    }
                    if opening && enter {
                        self.auto_open(c);
                    }
                }
            }
        }
    }

    pub(crate) fn refresh_node(&mut self, n: usize) {
        if let Some(f) = &mut self.files {
            f.reload();
        }
        let node = self.tree.node(n).clone();
        // 접속 줄 r 은 자동완성 목록도 다시 읽는다(R8.1)
        let target = match &node.kind {
            NodeKind::Conn(c) => {
                self.catalogs.remove(c);
                self.ensure_catalog(*c);
                Some(n)
            }
            NodeKind::Schema { .. } => Some(n),
            _ => node.parent.or(Some(n)),
        };
        if let Some(t) = target {
            let tn = self.tree.node(t).clone();
            if matches!(tn.kind, NodeKind::Folder { .. } | NodeKind::Key { .. }) || self.redis_parent(t) {
                if let Some(c) = tn.conn()
                    && let Some(cn) = self.tree.conn_node(c)
                {
                    self.load_keys(c, cn);
                }
                return;
            }
            self.tree.nodes[t].expanded = true;
            self.load_children(t);
        }
        self.status = "새로고침".into();
    }

    fn redis_parent(&self, n: usize) -> bool {
        self.tree.node(n).conn().is_some_and(|c| self.is_redis(c))
    }

    /// 탭을 테이블 보기로 바꿀지, 새 탭에 열지 정한다(R3.1). 글·파일·다른 접속의 보기가 있으면 새 탭이다.
    pub(crate) fn target_tab(&mut self, conn: usize) -> usize {
        let t = self.cur();
        let other = t.conn.is_some_and(|c| c != conn) && (t.view.is_some() || t.redis.is_some() || t.pending() > 0);
        if t.file.is_some() || !t.editor.value().trim().is_empty() || other || t.running.is_some() {
            self.new_tab()
        } else {
            self.tab
        }
    }

    pub fn open_table(&mut self, conn: usize, r: TableRef, preset_where: String) {
        if !self.is_pg(conn) {
            return;
        }
        let ti = self.target_tab(conn);
        if self.tabs[ti].pending() > 0 {
            let n = self.tabs[ti].pending();
            self.confirm(format!("변경 {n}건을 버릴까요"), move |a| {
                if let Some(g) = &mut a.tabs[ti].grid {
                    g.clear_changes();
                }
                a.open_table(conn, r, preset_where);
            });
            return;
        }
        self.set_conn(ti, Some(conn));
        let mut v = TableView::new(r.clone());
        let mut note = String::new();
        let uuid = self.conns[conn].uuid.clone();
        let prod = self.conns[conn].env == "prod";
        if preset_where.is_empty() {
            if let Some((order, wh)) = self.session.table_view(&uuid, &r.schema, &r.table, prod) {
                v.where_ = wh.clone();
                v.order_raw = order.clone();
                if !wh.is_empty() {
                    note = "이전 정렬·조건 적용 (w·O 칸을 비우고 Enter 로 해제)".into();
                } else if !order.is_empty() {
                    note = "이전 정렬 적용 (w·O 칸을 비우고 Enter 로 해제)".into();
                }
            }
        } else {
            v.where_ = preset_where;
            v.preset = true;
        }
        let t = &mut self.tabs[ti];
        t.view = Some(v);
        t.redis = None;
        t.title = r.table.clone();
        t.subs.retain(|p| p.pinned);
        t.sub = None;
        self.session.touch_table(&uuid, &r.schema, &r.table);
        self.tab = ti;
        self.set_focus(Focus::Result);
        self.request_page(ti, true);
        if !note.is_empty() {
            self.status = note;
        }
    }

    /// 테이블 보기 한 페이지를 읽는다(R16). first 면 추정 행 수도 읽는다.
    pub(crate) fn request_page(&mut self, ti: usize, first: bool) {
        let t = &self.tabs[ti];
        let (Some(v), Some(conn)) = (&t.view, t.conn) else { return };
        let req = TablePageRequest {
            conn: self.conns[conn].name.clone(),
            schema: v.table.schema.clone(),
            table: v.table.table.clone(),
            filter: v.where_.clone(),
            order_by: v.order_sql(),
            offset: v.offset(),
            limit: crate::tableview::PAGE_SIZE,
            count: false,
            estimate: first,
        };
        let id = t.id;
        let job = self.call(Request::TablePage(req), Pend::Page { tab: id });
        let t = &mut self.tabs[ti];
        t.running = Some(job);
        t.cancelled = false;
        t.running_read_only = true;
        t.result_conn = Some(conn);
    }

    fn page_done(&mut self, id: u64, tab: u64, res: Result<Value, EngineError>) {
        let r = res.and_then(decode::<TablePage>);
        let Some(ti) = self.tab_index(tab) else { return };
        if self.tabs[ti].running != Some(id) {
            return;
        }
        let conn = self.tabs[ti].conn;
        let t = &mut self.tabs[ti];
        t.running = None;
        match r {
            Ok(p) => {
                let keep_col = t.grid.as_ref().map_or(0, |g| g.col);
                let widths = t.grid.as_ref().map(|g| (g.res.cols.clone(), g.widths().to_vec()));
                let mut g = Grid::new(p.result.clone());
                g.server = true;
                g.col = keep_col.min(p.result.cols.len().saturating_sub(1));
                // 같은 열이면 바꾼 폭을 지킨다
                if let Some((cols, ws)) = widths
                    && cols == p.result.cols
                {
                    for (c, w) in ws.into_iter().enumerate() {
                        g.set_width(c, w);
                    }
                }
                let v = t.view.as_mut().unwrap();
                let numeric = (0..p.result.cols.len()).map(|c| g.kind(c) == crate::grid::Kind::Number).collect();
                v.set_columns(&p.result.cols, numeric);
                if let Some(e) = p.estimate
                    && !v.exact
                {
                    v.set_total(e, false);
                }
                let n = p.result.rows.len() as u64;
                if n < crate::tableview::PAGE_SIZE {
                    v.set_total((v.offset() + n) as i64, true);
                }
                // 정렬 열 표시(첫 정렬 열)
                if let Some((c, d)) = v.orders.first()
                    && let Some(i) = p.result.cols.iter().position(|x| x == c)
                {
                    g.sort = Some((i, *d));
                }
                t.summary = summary_of(&p.result);
                t.err.clear();
                t.text.clear();
                t.snapshot = false;
                t.last_sql = p.sql.clone();
                if let Some(c) = conn {
                    self.record(c, &p.sql, p.result.elapsed_ms, Some(&p.result), "");
                }
                let t = &mut self.tabs[ti];
                t.grid = Some(g);
                let v = t.view.as_ref().unwrap();
                let (w, o, tbl, prod) = (v.where_.clone(), v.order_text(), v.table.clone(), false);
                let s = t.summary.clone();
                self.status = s;
                if let Some(c) = conn {
                    let uuid = self.conns[c].uuid.clone();
                    let prod = prod || self.conns[c].env == "prod";
                    if !self.tabs[ti].view.as_ref().unwrap().preset {
                        self.session.set_table_view(&uuid, &tbl.schema, &tbl.table, &o, &w, prod);
                    }
                }
            }
            Err(e) => {
                t.err = e.error.clone();
                t.summary = "실패".into();
                self.status = format!("실패: {}", one_line(&e.error));
            }
        }
    }

    pub(crate) fn describe_node(&mut self, n: usize) {
        let NodeKind::Table { conn, schema, name } = self.tree.node(n).kind.clone() else {
            return;
        };
        let ti = self.target_tab(conn);
        self.set_conn(ti, Some(conn));
        let id = self.tabs[ti].id;
        let cn = self.conns[conn].name.clone();
        let job = self.call(
            Request::Describe {
                conn: cn,
                schema,
                table: name.clone(),
            },
            Pend::Describe { tab: id },
        );
        let t = &mut self.tabs[ti];
        t.view = None;
        t.redis = None;
        t.running = Some(job);
        t.title = format!("{name} 구조");
        self.tab = ti;
    }

    pub(crate) fn ddl_node(&mut self, n: usize) {
        let NodeKind::Table { conn, schema, name } = self.tree.node(n).kind.clone() else {
            return;
        };
        let cn = self.conns[conn].name.clone();
        self.call(
            Request::Ddl {
                conn: cn,
                schema,
                table: name,
            },
            Pend::Ddl { conn },
        );
    }

    /// 접속 줄 A·L·S: 활동·잠금·크기, Redis 는 INFO·SLOWLOG·큰 키(R22.1·R34.1).
    pub(crate) fn ops_node(&mut self, n: usize, k: &str) {
        let NodeKind::Conn(conn) = self.tree.node(n).kind else { return };
        let cn = self.conns[conn].name.clone();
        let (req, title) = match (self.is_redis(conn), k) {
            (false, "A") if self.is_pg(conn) => (Request::Activity { conn: cn }, "활동"),
            (false, "L") if self.is_pg(conn) => (Request::Locks { conn: cn }, "잠금"),
            (false, "S") if self.is_pg(conn) => (Request::TableSizes { conn: cn }, "테이블 크기"),
            (true, "A") => (Request::RedisInfo { conn: cn }, "INFO"),
            (true, "L") => (
                Request::RedisSlowlog {
                    conn: cn,
                    n: REDIS_SLOWLOG_N,
                },
                "SLOWLOG",
            ),
            (true, "S") => (
                Request::RedisBigKeys {
                    conn: cn,
                    sample: REDIS_BIGKEY_SAMPLE,
                    top: REDIS_BIGKEY_TOP,
                },
                "큰 키",
            ),
            _ => return,
        };
        let ti = self.target_tab(conn);
        self.set_conn(ti, Some(conn));
        let id = self.tabs[ti].id;
        let job = self.call(
            req,
            Pend::Ops {
                tab: id,
                title: title.into(),
            },
        );
        let t = &mut self.tabs[ti];
        t.view = None;
        t.redis = None;
        t.running = Some(job);
        t.result_conn = Some(conn);
        t.title = format!("{} {title}", self.conns[conn].name);
        self.tab = ti;
        self.set_focus(Focus::Result);
    }

    pub(crate) fn pick_database(&mut self, n: usize) {
        let NodeKind::Conn(conn) = self.tree.node(n).kind else { return };
        if !self.is_pg(conn) {
            return;
        }
        if self.tabs.iter().any(|t| t.conn == Some(conn) && t.running.is_some()) {
            self.status = "실행 중인 쿼리를 먼저 끝내세요".into();
            return;
        }
        if let Some(t) = self.tabs.iter().find(|t| t.result_conn == Some(conn) && t.pending() > 0) {
            self.status = format!(
                "{} 탭의 변경 {}건을 먼저 제출(Ctrl+S)하거나 되돌리세요(u)",
                t.display_name(),
                t.pending()
            );
            return;
        }
        let name = self.conns[conn].name.clone();
        self.call(Request::Databases { conn: name }, Pend::Databases { conn });
    }

    fn switch_db(&mut self, conn: usize, db: String) {
        if self.in_tx.contains_key(&conn) {
            self.overlay = Overlay::Tx {
                conns: vec![conn],
                then: Box::new(move |a| a.switch_db(conn, db)),
            };
            return;
        }
        let name = self.conns[conn].name.clone();
        self.call(Request::SwitchDb { conn: name, db }, Pend::SwitchDb { conn });
    }

    // ─── Redis 값 (R4.1·R20) ───

    fn open_redis_key(&mut self, conn: usize, key: String) {
        let ti = self.target_tab(conn);
        self.set_conn(ti, Some(conn));
        let t = &mut self.tabs[ti];
        t.view = None;
        t.grid = None;
        t.text.clear();
        t.title = key.clone();
        t.redis = Some(RedisView {
            key,
            raw: None,
            sel: 0,
            input: None,
            ask: None,
        });
        t.result_conn = Some(conn);
        let id = t.id;
        self.tab = ti;
        self.set_focus(Focus::Result);
        self.load_redis_raw(id);
    }

    pub(crate) fn load_redis_raw(&mut self, tab: u64) {
        let Some(ti) = self.tab_index(tab) else { return };
        let (Some(conn), Some(rv)) = (self.tabs[ti].conn, &self.tabs[ti].redis) else {
            return;
        };
        let req = Request::RedisRawValue {
            conn: self.conns[conn].name.clone(),
            key: rv.key.clone(),
        };
        let job = self.call(req, Pend::RedisRaw { tab });
        self.tabs[ti].running = Some(job);
    }

    fn redis_key(&mut self, k: &str) {
        let ti = self.tab;
        let id = self.tabs[ti].id;
        let Some(conn) = self.tabs[ti].conn else { return };
        let rv = self.tabs[ti].redis.as_mut().unwrap();
        if let Some(op) = rv.ask.clone() {
            rv.ask = None;
            if k == "y" {
                self.redis_submit(id, conn, &op, String::new());
            }
            return;
        }
        if let Some((op, input)) = &mut rv.input {
            match k {
                "enter" => {
                    let (op, text) = (op.clone(), input.text.clone());
                    rv.input = None;
                    self.redis_submit(id, conn, &op, text);
                }
                "esc" => rv.input = None,
                _ => {
                    input.key(k);
                }
            }
            return;
        }
        let Some(raw) = rv.raw.clone() else {
            if k == "esc" {
                self.tabs[ti].redis = None;
            }
            return;
        };
        let n = redis_len(&raw);
        let collection = matches!(raw.typ.as_str(), "hash" | "list" | "set" | "zset");
        match k {
            "down" | "j" => rv.sel = (rv.sel + 1).min(n.saturating_sub(1)),
            "up" | "k" => rv.sel = rv.sel.saturating_sub(1),
            "esc" | "q" => {
                self.tabs[ti].redis = None;
                self.tabs[ti].summary.clear();
            }
            "e" | "enter" if raw.typ == "string" || collection => {
                let cur = redis_item(&raw, rv.sel).map(|(_, v)| v).unwrap_or_default();
                rv.input = Some(("edit".into(), crate::input::LineInput::new(&cur)));
            }
            "a" if collection => rv.input = Some(("add".into(), crate::input::LineInput::default())),
            "d" if collection => rv.ask = Some("delete".into()),
            "t" => rv.input = Some(("ttl".into(), crate::input::LineInput::new(&raw.ttl_secs.to_string()))),
            "r" => rv.input = Some(("rename".into(), crate::input::LineInput::new(&raw.key))),
            "D" => rv.ask = Some("delete_key".into()),
            "?" => self.open_keys(),
            _ => {}
        }
    }

    fn redis_submit(&mut self, tab: u64, conn: usize, op: &str, text: String) {
        let Some(ti) = self.tab_index(tab) else { return };
        let Some(rv) = &self.tabs[ti].redis else { return };
        let Some(raw) = &rv.raw else { return };
        let (target, old) = redis_item(raw, rv.sel).unwrap_or_default();
        let edit = RedisEdit {
            key: raw.key.clone(),
            typ: raw.typ.clone(),
            op: op.into(),
            index: rv.sel as i64,
            target,
            old,
            text,
            keep_ttl: true,
        };
        self.send_redis_edit(tab, conn, edit, false, false);
    }

    fn send_redis_edit(&mut self, tab: u64, conn: usize, edit: RedisEdit, cp: bool, cd: bool) {
        let name = self.conns[conn].name.clone();
        let req = Request::RedisEdit {
            conn: name,
            edit: edit.clone(),
            confirm_prod: cp,
            confirm_danger: cd,
        };
        let job = self.call(req, Pend::RedisEdit { tab });
        // 거절이면 확인 창을 띄워 다시 보낸다: 답 처리 전에 같은 묶음을 기억해 둔다
        self.redis_retry.insert(job, (tab, conn, edit, cp, cd));
    }

    // ─── 결과 영역 키 ───

    pub fn result_key(&mut self, k: &str) {
        let ti = self.tab;
        if self.tabs[ti].redis.is_some() {
            return self.redis_key(k);
        }
        // 테이블 보기 키가 먼저다(R16.1)
        let grid_typing = self.tabs[ti]
            .grid
            .as_ref()
            .is_some_and(|g| g.screen != Screen::Table && g.screen != Screen::Select);
        let grid_col = self.tabs[ti].grid.as_ref().map(|g| g.col);
        if let Some(v) = &mut self.tabs[ti].view
            && !grid_typing
        {
            if k == "s" && v.typing.is_none() {
                let Some(c) = grid_col else {
                    self.status = "정렬할 열이 없음".into();
                    return;
                };
                return self.header_click(c, false);
            }
            let a = v.key(k);
            if a.handled {
                if !a.status.is_empty() {
                    self.status = a.status;
                }
                if a.requery {
                    self.requery_view(ti);
                }
                if a.count {
                    self.request_count(ti);
                }
                return;
            }
        }
        // 기록 사본 탭의 r 은 그 SQL 을 기록의 접속에서 다시 실행한다(R11.4)
        if self.tabs[ti].snapshot && k == "r" && self.tabs[ti].shown().is_none_or(|g| g.screen == Screen::Table) {
            if self.tabs[ti].conn.is_none() {
                self.status = "기록의 접속이 없음".into();
                return;
            }
            self.tabs[ti].editor.key("ctrl+a");
            return self.run_current(None);
        }
        match k {
            "q" if self.tabs[ti].shown().is_none_or(|g| g.screen == Screen::Table) => return self.request_quit(),
            "?" if self.tabs[ti].shown().is_none_or(|g| g.screen == Screen::Table) => return self.open_keys(),
            "ctrl+z" => return self.undo_submit(),
            "p" if self.tabs[ti].shown().is_some_and(|g| g.screen == Screen::Table) => return self.pin(),
            "P" if self.tabs[ti].shown().is_some_and(|g| g.screen == Screen::Table) => return self.unpin(),
            "{" | "}" => return self.cycle_sub(if k == "}" { 1 } else { -1 }),
            "]" if self.tabs[ti].view.is_none() && self.tabs[ti].shown().is_some_and(|g| g.screen == Screen::Table) => {
                return self.read_more(ti);
            }
            "F" | "R" if self.tabs[ti].shown().is_some_and(|g| g.screen == Screen::Table) => return self.follow_fk(k == "R"),
            "K" | "X" if self.tabs[ti].shown().is_some_and(|g| g.screen == Screen::Table) => return self.signal(k == "X"),
            "+" | "-" if self.tabs[ti].shown().is_some_and(|g| g.screen == Screen::Table) => return self.cell_where(k == "-"),
            "ctrl+b" => return self.find_linked(),
            "ctrl+k" => return self.copy_menu(),
            "c" | "D" | "t" | "=" | "r" if self.tabs[ti].shown().is_some_and(|g| g.screen == Screen::Table) => {
                self.status = format!("{k} 는 Rust 화면에 아직 옮기지 않음");
                return;
            }
            _ => {}
        }
        if self.tabs[ti].grid.is_none() && self.tabs[ti].sub.is_none() {
            // 글 결과 스크롤
            let t = &mut self.tabs[ti];
            match k {
                "down" | "j" => t.text_top += 1,
                "up" | "k" => t.text_top = t.text_top.saturating_sub(1),
                "pgdown" => t.text_top += 10,
                "pgup" => t.text_top = t.text_top.saturating_sub(10),
                "y" if !t.text.is_empty() => {
                    let s = t.text.clone();
                    self.copy(s, "복사함".into());
                }
                _ => {}
            }
            return;
        }
        let sub = self.tabs[ti].sub.is_some();
        let Some(g) = self.tabs[ti].shown_mut() else { return };
        let before = (g.sort, g.filter.clone());
        let act = if sub && matches!(k, "enter" | "f2" | "a" | "d" | "u" | "ctrl+d") && g.screen == Screen::Table {
            Act::Status("고정·문장별 결과는 고칠 수 없음".into())
        } else {
            g.key(k)
        };
        let after = (g.sort, g.filter.clone());
        let typing = g.screen == Screen::Filter;
        let sort_changed = before.0 != after.0;
        let filter_done = matches!(act, Act::FilterDone(_)) || (!typing && before.1 != after.1);
        self.grid_act(act);
        if sort_changed || filter_done {
            self.server_requery(ti, sort_changed);
        }
    }

    /// 정렬·거르기 확정을 서버에서 다시 읽는다(R14.5). 못 하면 받은 행만 한 까닭을 알린다(R14.6).
    fn server_requery(&mut self, ti: usize, sort: bool) {
        let what = if sort { "정렬" } else { "거르기" };
        let t = &self.tabs[ti];
        let why = if t.sub.is_some() {
            "고정·문장별 결과"
        } else if t.snapshot || t.base.is_none() {
            t.base_why
        } else if t.running.is_some() {
            "다른 실행 중"
        } else if t.result_conn != t.conn {
            "접속이 바뀜"
        } else if !t.grid.as_ref().is_some_and(Grid::unique_cols) {
            "열 이름 겹침"
        } else {
            ""
        };
        if !why.is_empty() {
            self.status = format!("받은 행만 {what}: {why}");
            return;
        }
        if t.pending() > 0 {
            self.status = format!("받은 행만 {what}: 고친 칸이 있음 (제출하거나 되돌린 뒤 다시)");
            return;
        }
        let g = t.grid.as_ref().unwrap();
        let sql = g.server_sql(t.base.as_ref().unwrap(), None);
        let again = Again::Keep {
            sort: g.sort,
            filter: g.filter.clone(),
            col: g.col,
        };
        let (tab, conn) = (t.id, t.result_conn.unwrap());
        self.start_script(Script {
            tab,
            conn,
            stmts: vec![sql],
            idx: 0,
            confirm_prod: false,
            confirm_danger: false,
            confirm_analyze: false,
            params: BTreeMap::new(),
            redis: false,
            results: Vec::new(),
            explain: None,
            started: Instant::now(),
            again: Some(again),
        });
    }

    /// 일반 결과의 `]`: 잘린 PostgreSQL 결과의 다음 1,000행을 같은 정렬·거르기로 덧붙인다(R14.7).
    fn read_more(&mut self, ti: usize) {
        let t = &self.tabs[ti];
        let Some(g) = &t.grid else { return };
        let why = if t.result_conn.is_none_or(|c| !self.is_pg(c)) {
            "PostgreSQL 결과만 더 읽음"
        } else if t.pending() > 0 {
            "고친 칸을 먼저 제출하거나 되돌리세요"
        } else if !g.res.truncated {
            "더 읽을 행이 없음"
        } else if g.res.rows.len() >= MORE_LIMIT {
            "10,000행까지만 더 읽음 (F7 전체 내보내기)"
        } else if t.base.is_none() || t.sub.is_some() || t.snapshot {
            "정렬·거르기가 받은 행에만 걸림"
        } else if t.running.is_some() {
            "다른 실행 중"
        } else {
            ""
        };
        if !why.is_empty() {
            self.status = format!("더 읽지 않음: {why}");
            return;
        }
        let sql = g.server_sql(t.base.as_ref().unwrap(), Some(g.res.rows.len()));
        let (tab, conn) = (t.id, t.result_conn.unwrap());
        self.start_script(Script {
            tab,
            conn,
            stmts: vec![sql],
            idx: 0,
            confirm_prod: false,
            confirm_danger: false,
            confirm_analyze: false,
            params: BTreeMap::new(),
            redis: false,
            results: Vec::new(),
            explain: None,
            started: Instant::now(),
            again: Some(Again::More),
        });
    }

    /// 복사 꼴 고르기 창(R71.1). 항목은 `대상 · 꼴`이고 커서는 기본 대상의 마지막 꼴이다.
    fn copy_menu(&mut self) {
        let Some(g) = self.cur().shown() else { return };
        if g.view_len() == 0 || !matches!(g.screen, Screen::Table | Screen::Select) {
            return;
        }
        let select = g.screen == Screen::Select;
        let origin = g.res.origin.is_some();
        let mut targets = Vec::new();
        if select {
            targets.push(("select", "선택"));
        }
        targets.extend([("row", "현재 행"), ("visible", "보이는 행 전체"), ("column", "현재 열")]);
        let mut items = Vec::new();
        let mut keys = Vec::new();
        for (t, tl) in &targets {
            for (f, fl) in [
                ("tsv", "TSV (헤더)"),
                ("csv", "CSV (헤더)"),
                ("json", "JSON"),
                ("md", "Markdown 표"),
                ("insert", "SQL INSERT"),
                ("in", "IN 목록"),
            ] {
                if (f == "insert" && (!origin || *t == "column")) || (f == "in" && *t != "column") {
                    continue;
                }
                items.push(format!("{tl} · {fl}"));
                keys.push((*t, f));
            }
        }
        let default_t = if select { "select" } else { "row" };
        let last = self.last_copy_fmt;
        let sel = keys
            .iter()
            .position(|&(t, f)| t == default_t && f == last)
            .or_else(|| keys.iter().position(|&(_, f)| f == last))
            .unwrap_or(0);
        let pairs = keys.clone();
        let names = items.clone();
        self.picker("복사 꼴", items, move |a, label| {
            let Some(i) = names.iter().position(|n| *n == label) else { return };
            let (t, f) = pairs[i];
            a.last_copy_fmt = f;
            let Some(g) = a.cur_mut().shown_mut() else { return };
            match g.copy_text(t, f) {
                Ok((text, n)) => {
                    let mut st = format!("복사함 ({n}줄) · {label}");
                    if f == "in" {
                        st.push_str(&format!(" · IN 목록 {}개", g.in_list().1));
                    }
                    if g.screen == Screen::Select {
                        g.screen = Screen::Table;
                    }
                    a.copy(text, st);
                }
                Err(e) => a.status = e,
            }
        });
        if let Overlay::Picker { sel: s, .. } = &mut self.overlay {
            *s = sel;
        }
    }

    pub(crate) fn grid_act(&mut self, act: Act) {
        match act {
            Act::None | Act::Ignored => {}
            Act::Copy(s, what) => self.copy(s, format!("{what} 복사함")),
            Act::Status(s) => self.status = s,
            Act::Export(kind) => self.export(kind),
            Act::FilterDone(f) => {
                let ti = self.tab;
                let prod = self.tabs[ti].result_conn.is_some_and(|c| self.conns[c].env == "prod");
                if !f.is_empty() && !prod && self.tabs[ti].sub.is_none() {
                    self.session.add_filter(&f);
                }
            }
            Act::EnumPick(c) => self.enum_pick(c),
        }
    }

    fn enum_pick(&mut self, c: usize) {
        let ti = self.tab;
        let Some(conn) = self.tabs[ti].result_conn else { return };
        let g = self.tabs[ti].grid.as_mut().unwrap();
        let typ = g.col_type(c).to_string();
        match self.enums.get(&(conn, typ.clone())) {
            None => {
                self.status = "enum 값을 아직 읽지 못함".into();
                let name = self.conns[conn].name.clone();
                self.call(
                    Request::EnumLabels {
                        conn: name,
                        types: vec![typ],
                    },
                    Pend::Enums { conn },
                );
            }
            Some(v) if v.is_empty() => {
                let act = g.start_edit();
                self.grid_act(act);
            }
            Some(v) => {
                let mut items = v.clone();
                items.push("NULL".into());
                self.picker("enum 값", items, move |a, val| {
                    if let Some(g) = &mut a.tabs[ti].grid {
                        g.set_cell(if val == "NULL" { None } else { Some(val) });
                    }
                });
            }
        }
    }

    /// 열 머리 클릭·s(R14.3): 테이블 보기는 서버 ORDER BY, 그 밖은 받은 행 정렬이다.
    pub(crate) fn header_click(&mut self, c: usize, add: bool) {
        let ti = self.tab;
        if self.tabs[ti].view.is_some()
            && let Some(col) = self.tabs[ti].grid.as_ref().and_then(|g| g.res.cols.get(c).cloned())
        {
            if self.tabs[ti].pending() > 0 {
                self.status = format!("변경 {}건을 먼저 제출(Ctrl+S)하거나 되돌리세요(u)", self.tabs[ti].pending());
                return;
            }
            self.tabs[ti].view.as_mut().unwrap().header_click(&col, add);
            if let Some(g) = &mut self.tabs[ti].grid {
                g.col = c;
            }
            return self.requery_view(ti);
        }
        if let Some(g) = self.tabs[ti].shown_mut() {
            g.col = c;
            g.cycle_sort(c);
        }
    }

    fn requery_view(&mut self, ti: usize) {
        if self.stale(ti) {
            return;
        }
        let n = self.tabs[ti].pending();
        if n > 0 {
            self.confirm(format!("변경 {n}건을 버릴까요"), move |a| {
                if let Some(g) = &mut a.tabs[ti].grid {
                    g.clear_changes();
                }
                a.request_page(ti, false);
            });
            return;
        }
        self.request_page(ti, false);
    }

    fn request_count(&mut self, ti: usize) {
        let t = &self.tabs[ti];
        let (Some(v), Some(conn)) = (&t.view, t.conn) else { return };
        let req = TablePageRequest {
            conn: self.conns[conn].name.clone(),
            schema: v.table.schema.clone(),
            table: v.table.table.clone(),
            filter: v.where_.clone(),
            order_by: String::new(),
            offset: 0,
            limit: 1,
            count: true,
            estimate: false,
        };
        let id = t.id;
        self.call(Request::TablePage(req), Pend::Count { tab: id });
    }

    /// 탭 접속이 결과를 만든 접속과 다르면 막는다(R3.1).
    fn stale(&mut self, ti: usize) -> bool {
        let t = &self.tabs[ti];
        if t.result_conn.is_some() && t.result_conn != t.conn {
            self.status = "보이는 결과는 다른 접속(또는 바꾸기 전 데이터베이스)의 것이라 막음 (다시 실행하거나 새로 여세요)".into();
            return true;
        }
        false
    }

    /// 탭의 마지막 읽기를 다시 한다(제출·신호 뒤).
    pub(crate) fn refresh_tab(&mut self, tab: u64) {
        let Some(ti) = self.tab_index(tab) else { return };
        if self.tabs[ti].view.is_some() {
            self.request_page(ti, false);
            return;
        }
        let t = &self.tabs[ti];
        if let (Some(conn), false) = (t.result_conn, t.last_sql.is_empty()) {
            let sql = t.last_sql.clone();
            self.start_script(Script {
                tab,
                conn,
                stmts: vec![sql],
                idx: 0,
                confirm_prod: false,
                confirm_danger: false,
                confirm_analyze: false,
                params: BTreeMap::new(),
                redis: false,
                results: Vec::new(),
                explain: None,
                started: Instant::now(),
                again: None,
            });
        }
    }

    fn pin(&mut self) {
        let t = self.cur_mut();
        if t.pending() > 0 {
            self.status = "변경을 먼저 제출하거나 되돌리세요".into();
            return;
        }
        if t.subs.iter().filter(|p| p.pinned).count() >= MAX_PINS {
            self.status = format!("고정은 탭마다 {MAX_PINS}개까지");
            return;
        }
        let Some(g) = t.shown() else { return };
        let res = g.res.clone();
        if let Some(i) = t.sub {
            t.subs[i].pinned = true;
        } else {
            let n = t.subs.len() + 1;
            t.subs.push(Pin {
                title: format!("고정 {n}"),
                grid: Grid::new(res),
                pinned: true,
            });
        }
        self.status = "결과 고정".into();
    }

    fn unpin(&mut self) {
        let t = self.cur_mut();
        if let Some(i) = t.sub {
            t.subs.remove(i);
            t.sub = None;
            self.status = "고정 풂".into();
        }
    }

    fn cycle_sub(&mut self, d: isize) {
        let t = self.cur_mut();
        if t.subs.is_empty() || t.pending() > 0 {
            return;
        }
        // None(지금 결과) → 0 → 1 … 을 돈다
        let n = t.subs.len() as isize + 1;
        let cur = t.sub.map_or(0, |i| i as isize + 1);
        let next = (cur + d).rem_euclid(n);
        t.sub = if next == 0 { None } else { Some(next as usize - 1) };
    }

    /// 칸 값으로 WHERE 거르기(R38): `+` 는 같음, `-` 는 다름을 더하거나 뺀다.
    fn cell_where(&mut self, ne: bool) {
        let ti = self.tab;
        let Some(g) = &self.tabs[ti].grid else { return };
        if self.tabs[ti].view.is_none() {
            self.status = "테이블 보기에서만 칸으로 거르기".into();
            return;
        }
        let col = g.res.cols[g.col].clone();
        let v = g.cur_value().unwrap_or_default();
        let q = idb_core::sqlsafe::quote_ident(&col);
        let cond = match (crate::grid::is_null(&v), ne) {
            (true, false) => format!("{q} IS NULL"),
            (true, true) => format!("{q} IS NOT NULL"),
            (false, false) => format!("{q}::text = {}", idb_core::sqlsafe::quote_literal(&v)),
            (false, true) => format!("{q}::text <> {}", idb_core::sqlsafe::quote_literal(&v)),
        };
        let a = self.tabs[ti].view.as_mut().unwrap().toggle_where(&cond);
        if a.requery {
            self.requery_view(ti);
        }
    }

    /// 외래 키 따라가기(R36): F 는 이 행이 가리키는 곳, R 은 이 행을 가리키는 곳이다.
    fn follow_fk(&mut self, incoming: bool) {
        let ti = self.tab;
        let t = &self.tabs[ti];
        let (Some(conn), Some(g)) = (t.result_conn, &t.grid) else { return };
        let Some(o) = &g.res.origin else {
            self.status = "외래 키: 한 테이블의 결과가 아님".into();
            return;
        };
        let row: Vec<(String, String)> = match g.row_values() {
            Some(r) => g.res.cols.iter().cloned().zip(r).collect(),
            None => return,
        };
        let req = Request::ForeignKeys {
            conn: self.conns[conn].name.clone(),
            schema: o.table.schema.clone(),
            table: o.table.table.clone(),
            incoming,
        };
        let id = t.id;
        self.call(
            req,
            Pend::Fk {
                tab: id,
                conn,
                incoming,
                row,
            },
        );
    }

    fn fk_pick(&mut self, _tab: u64, conn: usize, incoming: bool, row: Vec<(String, String)>, fks: Vec<Fk>) {
        if fks.is_empty() {
            self.status = if incoming {
                "이 테이블을 참조하는 외래 키 없음"
            } else {
                "외래 키 없음"
            }
            .into();
            return;
        }
        let items: Vec<String> = fks
            .iter()
            .map(|f| {
                let target = if incoming { &f.table } else { &f.ref_table };
                format!("{}.{} ({})", target.schema, target.table, f.name)
            })
            .collect();
        let open = move |a: &mut App, pick: String| {
            let Some(i) = items_index(&fks, &pick, incoming) else { return };
            let f = &fks[i];
            let (target, from, to) = if incoming {
                (f.table.clone(), f.ref_cols.clone(), f.cols.clone())
            } else {
                (f.ref_table.clone(), f.cols.clone(), f.ref_cols.clone())
            };
            let map: std::collections::HashMap<String, String> = row.iter().cloned().collect();
            let Some(vals) = from.iter().map(|c| map.get(c).cloned()).collect::<Option<Vec<String>>>() else {
                a.status = format!("외래 키 {} 의 열이 결과에 없음", f.name);
                return;
            };
            if vals.iter().any(|v| crate::grid::is_null(v)) {
                a.status = format!("외래 키 {} 의 값이 NULL 이라 따라갈 수 없음", f.name);
                return;
            }
            match idb_engine::fk_where(&to, &vals) {
                Ok(wh) => a.open_table(conn, target, wh),
                Err(e) => a.status = e,
            }
        };
        if items.len() == 1 {
            let only = items[0].clone();
            open(self, only);
        } else {
            self.picker(if incoming { "참조하는 테이블" } else { "외래 키" }, items, open);
        }
    }

    /// pid 행의 쿼리 취소·연결 종료(R22.1). 확인은 serve 의 reason 으로 묻는다.
    fn signal(&mut self, terminate: bool) {
        let ti = self.tab;
        let t = &self.tabs[ti];
        let (Some(conn), Some(g)) = (t.result_conn, &t.grid) else { return };
        let Some(pc) = g.res.cols.iter().position(|c| c == "pid") else {
            self.status = "pid 열이 있는 결과에서만".into();
            return;
        };
        let Some(pid) = g.row_values().and_then(|r| r[pc].parse::<i64>().ok()) else {
            return;
        };
        let tab = t.id;
        self.send_signal(tab, conn, pid, terminate, false, false);
    }

    fn send_signal(&mut self, tab: u64, conn: usize, pid: i64, terminate: bool, confirm: bool, cp: bool) {
        let req = Request::SignalBackend {
            conn: self.conns[conn].name.clone(),
            pid,
            terminate,
            confirm,
            confirm_prod: cp,
        };
        let job = self.call(req, Pend::Signal { tab });
        self.signal_retry.insert(job, (tab, conn, pid, terminate));
    }

    /// 연결된 곳 찾기(R73): 칸 값이 든 다른 테이블을 serve 가 센다.
    fn find_linked(&mut self) {
        let ti = self.tab;
        let t = &self.tabs[ti];
        let (Some(conn), Some(g)) = (t.result_conn, t.shown()) else {
            return;
        };
        if !self.is_pg(conn) {
            return;
        }
        let col = g.res.cols.get(g.col).cloned().unwrap_or_default();
        let Some(v) = g.cur_value() else { return };
        let table = g.res.origin.as_ref().map(|o| o.table.table.clone()).unwrap_or_default();
        let req = Request::Linked {
            conn: self.conns[conn].name.clone(),
            column: col,
            value: v,
            table,
        };
        let i = self.new_tab();
        self.set_conn(i, Some(conn));
        let id = self.tabs[i].id;
        self.tabs[i].title = "연결된 곳".into();
        let job = self.call(req, Pend::Linked { tab: id, conn });
        self.tabs[i].running = Some(job);
        self.set_focus(Focus::Result);
    }

    fn export(&mut self, kind: &'static str) {
        let t = self.cur();
        let Some(g) = t.shown() else { return };
        let body = if kind == "csv" { g.csv() } else { g.json() };
        let title = t.display_name();
        match crate::files::export_file(&title, kind, body.as_bytes()) {
            Ok(p) => self.status = format!("내보냄: {p}"),
            Err(e) => self.status = format!("내보내기 실패: {e}"),
        }
    }

    // ─── 실행 (R3·R4·R6·R7·R9·R21·R25) ───

    /// 실행 키. explain 이 Some 이면 EXPLAIN(true 면 ANALYZE, BUFFERS)이다.
    pub fn run_current(&mut self, explain: Option<bool>) {
        let ti = self.tab;
        let Some(conn) = self.tabs[ti].conn else {
            self.status = "트리에서 접속을 먼저 고르세요".into();
            return;
        };
        if self.is_source(conn) {
            return self.run_source(ti, conn, explain.is_some());
        }
        if self.tabs[ti].running.is_some() {
            self.status = "실행 중 (Ctrl+C 로 취소)".into();
            return;
        }
        let redis = self.is_redis(conn);
        let ed = &self.tabs[ti].editor;
        let text = ed.value().to_string();
        let stmts: Vec<String> = if redis {
            let src = ed
                .selected_text()
                .map(str::to_string)
                .unwrap_or_else(|| idb_core::sqlsafe::line_at(&text, ed.cursor()).to_string());
            src.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect()
        } else if let Some(sel) = ed.selected_text() {
            split_statements(sel).into_iter().map(String::from).collect()
        } else {
            let s = statement_at(&text, ed.cursor()).trim();
            if s.is_empty() { Vec::new() } else { vec![s.to_string()] }
        };
        if stmts.is_empty() {
            self.status = "실행할 문장이 없음".into();
            return;
        }
        if explain.is_some() && (redis || stmts.len() != 1) {
            self.status = if redis {
                "Redis 는 실행 계획이 없음"
            } else {
                "실행 계획은 문장 하나만"
            }
            .into();
            return;
        }
        if self.tabs[ti].pending() > 0 {
            let n = self.tabs[ti].pending();
            self.confirm(format!("변경 {n}건을 버릴까요"), move |a| {
                if let Some(g) = &mut a.tabs[a.tab].grid {
                    g.clear_changes();
                }
                a.run_current(explain);
            });
            return;
        }
        // 파일 없는 탭의 첫 실행은 queries/ 에 저장한다(R45.1)
        if !redis {
            self.save_new_query(ti);
        }
        let tab = self.tabs[ti].id;
        let script = Script {
            tab,
            conn,
            stmts,
            idx: 0,
            confirm_prod: false,
            confirm_danger: false,
            confirm_analyze: false,
            params: BTreeMap::new(),
            redis,
            results: Vec::new(),
            explain,
            started: Instant::now(),
            again: None,
        };
        // EXPLAIN ANALYZE 는 문장을 실제로 실행하므로 쓰기면 늘 묻는다(R9.2)
        if explain == Some(true) && !is_read_only_sql(&script.stmts[0]) {
            self.confirm("EXPLAIN ANALYZE 는 문장을 실제로 실행합니다", move |a| {
                a.ask_params(script)
            });
            return;
        }
        self.ask_params(script);
    }

    /// `:name`·`$n` 자리가 있으면 값을 차례로 묻는다(R21.3). 기억한 값을 채워 둔다(R55).
    fn ask_params(&mut self, mut s: Script) {
        if s.redis {
            return self.begin_script(s);
        }
        let mut names = Vec::new();
        for st in &s.stmts {
            for n in idb_core::params::bind(st).1 {
                if !names.contains(&n) {
                    names.push(n);
                }
            }
        }
        let Some(next) = names.into_iter().find(|n| !s.params.contains_key(n)) else {
            return self.begin_script(s);
        };
        let remembered = self.session.param(&next);
        let label = format!(":{next} = ");
        self.prompt(label, &remembered, move |a, v| {
            a.session.set_param(&next, &v);
            s.params.insert(next, v);
            a.ask_params(s);
        });
    }

    /// Manual 이면 트랜잭션 밖일 때 BEGIN 을 먼저 보낸다(R17.1).
    fn begin_script(&mut self, s: Script) {
        if !s.redis && self.manual.contains(&s.conn) && !self.in_tx.contains_key(&s.conn) && s.explain.is_none() {
            let name = self.conns[s.conn].name.clone();
            let conn = s.conn;
            self.call(
                Request::TxBegin { conn: name },
                Pend::Tx {
                    conn,
                    op: "begin",
                    then: Some(Box::new(move |a| a.start_script(s))),
                },
            );
            return;
        }
        self.start_script(s);
    }

    pub(crate) fn start_script(&mut self, s: Script) {
        let Some(ti) = self.tab_index(s.tab) else { return };
        let name = self.conns[s.conn].name.clone();
        let stmt = s.stmts[s.idx].clone();
        let ro = if s.redis {
            is_read_only_redis(&split_args(&stmt))
        } else {
            is_read_only_sql(&stmt) && s.explain != Some(true)
        };
        let req = if s.redis {
            Request::RedisRun {
                conn: name,
                args: split_args(&stmt),
                confirm_prod: s.confirm_prod,
                confirm_danger: s.confirm_danger,
            }
        } else {
            let run = RunRequest {
                conn: name,
                sql: stmt,
                params: s.params.clone(),
                confirm_prod: s.confirm_prod,
                confirm_danger: s.confirm_danger,
                confirm_analyze: s.confirm_analyze,
            };
            match s.explain {
                Some(analyze) => Request::Explain {
                    run,
                    analyze,
                    buffers: analyze,
                    format: String::new(),
                },
                None => Request::Run(run),
            }
        };
        let conn = s.conn;
        let job = self.call(req, Pend::Run(Box::new(s)));
        let t = &mut self.tabs[ti];
        t.running = Some(job);
        t.running_read_only = ro;
        t.cancelled = false;
        t.result_conn = Some(conn);
        t.view = None;
        t.redis = None;
    }

    fn script_done(&mut self, id: u64, mut s: Script, res: Result<Value, EngineError>) {
        let Some(ti) = self.tab_index(s.tab) else { return };
        if self.tabs[ti].running != Some(id) {
            return;
        }
        self.tabs[ti].running = None;
        match res.and_then(decode::<ResultSet>) {
            Ok(rs) => {
                if let Some(open) = rs.in_tx {
                    self.note_tx(s.conn, open);
                }
                let sql = self.history_sql(&s);
                self.record(s.conn, &sql, rs.elapsed_ms, Some(&rs), "");
                s.results.push(rs);
                s.idx += 1;
                if s.idx < s.stmts.len() {
                    return self.start_script(s);
                }
                self.show_script(ti, s);
            }
            Err(e)
                if e.code == CODE_REFUSED
                    && matches!(
                        e.reason.as_deref(),
                        Some(REASON_CONFIRM_PROD | REASON_CONFIRM_DANGER | REASON_CONFIRM | idb_engine::REASON_CONFIRM_ANALYZE)
                    ) =>
            {
                // 확인을 받아 같은 문장부터 다시 보낸다. prod 쓰기와 위험 문장이 겹치면 한 창에 이유를 같이 보인다(R25.1)
                let reason = e.reason.clone().unwrap_or_default();
                let stmt = s.stmts[s.idx].clone();
                let danger = if s.redis {
                    idb_core::sqlsafe::danger_of_redis(&split_args(&stmt))
                } else {
                    idb_core::sqlsafe::danger_of(&stmt)
                };
                let name = self.conns[s.conn].name.clone();
                let q = if reason == REASON_CONFIRM_PROD && !danger.is_empty() && !s.confirm_danger {
                    format!("prod({name})에 쓰기 실행 · 위험: {danger} — 실행할까요?")
                } else if reason == REASON_CONFIRM_DANGER {
                    format!("{} — 실행할까요?", e.error)
                } else {
                    e.error.trim_end_matches(": y/N").to_string()
                };
                let both = reason == REASON_CONFIRM_PROD && !danger.is_empty();
                self.confirm(q, move |a| {
                    match reason.as_str() {
                        REASON_CONFIRM_PROD => {
                            s.confirm_prod = true;
                            if both {
                                s.confirm_danger = true;
                            }
                        }
                        REASON_CONFIRM_DANGER => s.confirm_danger = true,
                        REASON_CONFIRM => s.confirm_prod = true,
                        idb_engine::REASON_CONFIRM_ANALYZE => s.confirm_analyze = true,
                        _ => {}
                    }
                    a.start_script(s);
                });
            }
            Err(e) => {
                if e.reason.as_deref() == Some("in_tx") {
                    self.note_tx(s.conn, true);
                }
                let sql = self.history_sql(&s);
                self.record(s.conn, &sql, s.started.elapsed().as_millis() as u64, None, &e.error);
                // 다시 읽기·더 읽기가 실패하면 보던 결과·정렬·거르기를 둔다(R14.5)
                if s.again.is_some() {
                    self.status = format!("다시 읽지 못함: {}", one_line(&e.error));
                    return;
                }
                let t = &mut self.tabs[ti];
                let mut msg = e.error.clone();
                if s.stmts.len() > 1 {
                    msg = format!("statement {} of {}: {msg}", s.idx + 1, s.stmts.len());
                }
                t.err = msg.clone();
                t.grid = None;
                t.text.clear();
                t.summary = if t.cancelled { "취소됨".into() } else { "실패".into() };
                self.status = format!("실패: {}", one_line(&msg));
            }
        }
    }

    /// 기록에 남길 문장이다. 파라미터는 값 없이 이름만 붙인다(R21.3). EXPLAIN 은 감싼 꼴로 남긴다.
    fn history_sql(&self, s: &Script) -> String {
        let mut sql = s.stmts[s.idx].clone();
        if let Some(analyze) = s.explain {
            sql = format!("EXPLAIN {}{sql}", if analyze { "(ANALYZE, BUFFERS) " } else { "" });
        }
        if !s.params.is_empty() {
            let names: Vec<&str> = s.params.keys().map(String::as_str).collect();
            sql = format!("{sql}\n-- 파라미터: {}", names.join(", "));
        }
        sql
    }

    fn show_script(&mut self, ti: usize, s: Script) {
        let multi = s.results.len() > 1;
        let elapsed = s.started.elapsed().as_millis() as u64;
        let conn = s.conn;
        let last = s.results.last().cloned().unwrap_or_default();
        let cur_id = self.cur().id;
        let t = &mut self.tabs[ti];
        t.err.clear();
        if t.snapshot {
            t.snapshot = false;
            t.title = format!("Query {}", t.id);
        }
        t.subs.retain(|p| p.pinned);
        t.sub = None;
        if multi {
            for (i, r) in s.results.iter().enumerate() {
                if !r.cols.is_empty() {
                    t.subs.push(Pin {
                        title: format!("문장 {}", i + 1),
                        grid: Grid::new(r.clone()),
                        pinned: false,
                    });
                }
            }
        }
        let mut summary = summary_of(&last);
        match (&s.again, t.grid.as_mut()) {
            (Some(Again::More), Some(g)) => {
                g.extend(last.rows.clone(), last.truncated);
                let n = g.res.rows.len() as i64;
                summary = format!(
                    "{}행{}",
                    idb_core::textutil::group_digits(n),
                    if last.truncated { " · 더 있음" } else { "" }
                );
            }
            (Some(Again::Keep { sort, filter, col }), _) => {
                let mut g = Grid::new(last.clone());
                g.adopt(*sort, filter, *col);
                t.grid = Some(g);
            }
            _ if last.cols.is_empty() => {
                t.grid = None;
                t.text = last.text.clone();
                t.base = None;
                t.base_why = "표 결과가 아님";
            }
            _ => {
                t.text.clear();
                t.grid = Some(Grid::new(last.clone()));
                t.base = None;
                t.base_why = if s.redis {
                    "이 저장소는 서버 정렬 없음"
                } else if s.stmts.len() > 1 {
                    "문장이 둘 이상"
                } else if s.explain.is_some() || !is_read_only_sql(&s.stmts[0]) {
                    "쓰기 문장"
                } else if !s.params.is_empty() || !idb_core::params::bind(&s.stmts[0]).1.is_empty() {
                    "파라미터 문장"
                } else {
                    t.base = Some(s.stmts[0].clone());
                    ""
                };
                if !s.redis && s.explain.is_none() && s.stmts.len() == 1 {
                    t.last_sql = s.stmts[0].clone();
                }
            }
        }
        if multi {
            summary = format!("{} statements · {summary}", s.results.len());
        }
        if last.in_tx == Some(true) {
            summary.push_str(" · 트랜잭션 열림");
        }
        if !last.note.is_empty() {
            summary.push_str(&format!(" · {}", one_line(&last.note)));
        }
        t.summary = summary.clone();
        let title = t.display_name();
        self.status = if self.tabs[ti].id == cur_id {
            summary
        } else {
            format!("{title}: {summary}")
        };
        if elapsed >= 10_000 {
            crate::files::notify(&self.conns[conn].name, elapsed, last.rows.len(), false);
        }
        // enum 열은 고르기 창으로 고친다(R49.2). 모르는 사용자 타입만 읽는다
        if let Some(types) = &last.types {
            let unknown: Vec<String> = types
                .iter()
                .filter(|t| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()) && !self.enums.contains_key(&(conn, (*t).clone())))
                .cloned()
                .collect();
            if !unknown.is_empty() && last.origin.is_some() {
                let name = self.conns[conn].name.clone();
                self.call(
                    Request::EnumLabels {
                        conn: name,
                        types: unknown,
                    },
                    Pend::Enums { conn },
                );
            }
        }
        self.session
            .touch_file(self.tabs[ti].conn.map(|c| self.conns[c].uuid.clone()), self.tabs[ti].file.clone());
    }

    // ─── 자동완성 (R8) ───

    pub(crate) fn complete(&mut self) {
        let ti = self.tab;
        let Some(conn) = self.tabs[ti].conn else { return };
        let ed = &self.tabs[ti].editor;
        let text = ed.value().to_string();
        let off = ed.cursor();
        let items = if self.is_redis(conn) {
            let line_start = text[..off].rfind('\n').map_or(0, |i| i + 1);
            crate::complete::redis(&text[line_start..off], &self.tree.loaded_keys(conn))
        } else {
            self.ensure_catalog(conn);
            // 커서가 든 문장만 본다
            let start = text[..off].rfind(';').map_or(0, |i| i + 1);
            let end = text[off..].find(';').map_or(text.len(), |i| off + i);
            crate::complete::sql(self.catalogs.get(&conn), &text[start..end], off - start)
        };
        self.tabs[ti].editor.open_popup(items);
    }

    pub(crate) fn format_sql(&mut self) {
        let ti = self.tab;
        if self.tabs[ti].editor.redis {
            return;
        }
        let ed = &mut self.tabs[ti].editor;
        match ed.selection() {
            Some((s, e)) => {
                let f = idb_core::sqlfmt::format(&ed.value()[s..e]);
                ed.replace_range(s, e, &f);
            }
            None => {
                let f = idb_core::sqlfmt::format(ed.value());
                let len = ed.value().len();
                ed.replace_range(0, len, &f);
            }
        }
        self.edited();
        self.status = "SQL 정렬".into();
    }

    // ─── 트랜잭션 (R17) ───

    pub(crate) fn toggle_manual(&mut self) {
        let Some(c) = self.cur().conn.filter(|&c| self.is_pg(c)) else {
            self.status = "PostgreSQL 탭에서만".into();
            return;
        };
        if !self.manual.remove(&c) {
            self.manual.insert(c);
        }
        self.status = if self.manual.contains(&c) { "Manual" } else { "Auto" }.into();
    }

    pub(crate) fn tx_end(&mut self, commit: bool) {
        let Some(c) = self.cur().conn.filter(|&c| self.is_pg(c)) else {
            return;
        };
        if !self.in_tx.contains_key(&c) {
            self.status = "열린 트랜잭션 없음".into();
            return;
        }
        let name = self.conns[c].name.clone();
        let (req, op) = if commit {
            (
                Request::TxCommit {
                    conn: name,
                    confirm_prod: false,
                },
                "commit",
            )
        } else {
            (Request::TxRollback { conn: name }, "rollback")
        };
        self.call(req, Pend::Tx { conn: c, op, then: None });
    }

    /// 열린 트랜잭션을 모두 끝내고 then 을 한다(R17.3). c 는 prod COMMIT 확인을 겸한다.
    pub(crate) fn finish_txs(&mut self, mut conns: Vec<usize>, commit: bool, then: Box<dyn FnOnce(&mut App)>) {
        let Some(c) = conns.pop() else {
            return then(self);
        };
        let name = self.conns[c].name.clone();
        let (req, op) = if commit {
            (
                Request::TxCommit {
                    conn: name,
                    confirm_prod: true,
                },
                "commit",
            )
        } else {
            (Request::TxRollback { conn: name }, "rollback")
        };
        self.call(
            req,
            Pend::Tx {
                conn: c,
                op,
                then: Some(Box::new(move |a| a.finish_txs(conns, commit, then))),
            },
        );
    }

    pub(crate) fn toggle_lock(&mut self) {
        let Some(c) = self.cur().conn else { return };
        if self.conns[c].env != "prod" {
            self.status = "prod 접속이 아니라 잠그지 않음".into();
            return;
        }
        let locked = self.locks.get(&c).is_none_or(|l| l.locked);
        let name = self.conns[c].name.clone();
        self.call(
            Request::ProdLock {
                conn: name,
                unlock: Some(locked),
            },
            Pend::Lock { conn: c, toggle: true },
        );
    }

    // ─── 제출·되돌리기 (R15.2·R50) ───

    pub(crate) fn ctrl_s(&mut self) {
        let ti = self.tab;
        if self.tabs[ti].pending() > 0 {
            return self.submit(ti);
        }
        self.save_file(ti);
    }

    fn submit(&mut self, ti: usize) {
        if self.stale(ti) {
            return;
        }
        let Some(cs) = self.tabs[ti].grid.as_ref().and_then(Grid::change_set) else {
            return;
        };
        let Some(conn) = self.tabs[ti].result_conn else { return };
        let n = cs.changes.len();
        let name = self.conns[conn].name.clone();
        let prod = self.conns[conn].env == "prod";
        let preview = idb_engine_preview(&cs);
        let q = if prod {
            format!("prod({name})에 {n}건 제출")
        } else {
            format!("{n}건 제출")
        };
        let tab = self.tabs[ti].id;
        self.overlay = Overlay::Confirm {
            q,
            then: Box::new(move |a| a.send_apply(tab, conn, cs, prod)),
            preview,
            top: 0,
        };
    }

    fn send_apply(&mut self, tab: u64, conn: usize, cs: idb_engine::ChangeSet, confirm_prod: bool) {
        let name = self.conns[conn].name.clone();
        let req = Request::ApplyChanges {
            conn: name,
            changes: cs.clone(),
            confirm_prod,
        };
        self.call(req, Pend::Apply { tab, cs });
    }

    fn apply_done(&mut self, tab: u64, cs: idb_engine::ChangeSet, res: Result<Value, EngineError>) {
        match res.and_then(decode::<AppliedResult>) {
            Ok(r) => {
                let Some(ti) = self.tab_index(tab) else { return };
                let conn = self.tabs[ti].result_conn.unwrap_or(0);
                let mut status = format!("제출함: {}", one_line(&r.result.text));
                let count = |op| cs.changes.iter().filter(|c| c.op == op).count();
                let note = format!(
                    "-- 제출 {}건 {}.{} (UPDATE {} · INSERT {} · DELETE {})",
                    cs.changes.len(),
                    cs.table.schema,
                    cs.table.table,
                    count(idb_engine::ChangeOp::Update),
                    count(idb_engine::ChangeOp::Insert),
                    count(idb_engine::ChangeOp::Delete)
                );
                self.record(conn, &note, r.result.elapsed_ms, None, "");
                if r.applied.no_returning {
                    status.push_str(" · 되돌리기 불가: RETURNING 권한이 없어 바꾼 행을 받지 못함");
                } else {
                    let t = &mut self.tabs[ti];
                    t.undo.push(UndoEntry {
                        conn,
                        changes: cs.clone(),
                        applied: r.applied.clone(),
                    });
                    if t.undo.len() > UNDO_KEEP {
                        t.undo.remove(0);
                    }
                    let uuid = self.conns[conn].uuid.clone();
                    crate::state::log_undo(&uuid, &cs, &r.applied);
                }
                if let Some(g) = &mut self.tabs[ti].grid {
                    g.clear_changes();
                }
                self.status = status;
                self.refresh_tab(tab);
            }
            Err(e) if e.code == CODE_REFUSED && e.reason.as_deref() == Some(REASON_CONFIRM_PROD) => {
                let Some(ti) = self.tab_index(tab) else { return };
                let conn = self.tabs[ti].result_conn.unwrap_or(0);
                self.send_apply(tab, conn, cs, true);
            }
            // 실패하면 묶음 전체가 되돌려졌으므로 보류 중인 편집을 남겨 고쳐 다시 내게 한다
            Err(e) => self.status = format!("제출 실패: {}", one_line(&e.error)),
        }
    }

    fn undo_submit(&mut self) {
        let ti = self.tab;
        let tab = self.tabs[ti].id;
        let Some(u) = self.tabs[ti].undo.last() else {
            self.status = "되돌릴 제출이 없음".into();
            return;
        };
        let (conn, cs, applied) = (u.conn, u.changes.clone(), u.applied.clone());
        let undo = match undo_preview(&cs, &applied) {
            Ok(p) => p,
            Err(e) => {
                self.status = format!("되돌리기 불가: {e}");
                return;
            }
        };
        let name = self.conns[conn].name.clone();
        let prod = self.conns[conn].env == "prod";
        let n = cs.changes.len();
        let q = if prod {
            format!("prod({name})에 제출 되돌리기 {n}건")
        } else {
            format!("제출 되돌리기 {n}건")
        };
        self.overlay = Overlay::Confirm {
            q,
            then: Box::new(move |a| {
                a.call(
                    Request::UndoChanges {
                        conn: name,
                        changes: cs,
                        applied,
                        confirm_prod: prod,
                    },
                    Pend::Undo { tab },
                );
            }),
            preview: undo,
            top: 0,
        };
    }

    // ─── 그 밖 ───

    pub(crate) fn pick_schema(&mut self) {
        let Some(c) = self.cur().conn.filter(|&c| self.is_pg(c)) else {
            self.status = "PostgreSQL 탭에서만".into();
            return;
        };
        let name = self.conns[c].name.clone();
        self.call(Request::Schemas { conn: name }, Pend::Schemas2 { conn: c });
    }

    pub(crate) fn goto_definition(&mut self) {
        let ti = self.tab;
        let Some(c) = self.tabs[ti].conn.filter(|&c| self.is_pg(c)) else {
            return;
        };
        let name = self.tabs[ti].editor.ident_at();
        if name.is_empty() {
            return;
        }
        let tab = self.tabs[ti].id;
        let cn = self.conns[c].name.clone();
        self.call(Request::Resolve { conn: cn, name }, Pend::Resolve { tab, conn: c });
    }

    pub(crate) fn rename_tab(&mut self) {
        let cur = self.cur().display_name();
        self.prompt("탭 이름: ", &cur, |a, v| {
            let t = a.cur_mut();
            t.name = v.trim().to_string();
        });
    }

    pub(crate) fn open_user360(&mut self) {
        self.prompt("사용자 (이메일·uid·id·닉네임): ", "", |a, q| {
            let q = q.trim().to_string();
            if q.is_empty() {
                return;
            }
            let i = a.new_tab();
            a.tabs[i].title = format!("사용자 {q}");
            let id = a.tabs[i].id;
            let job = a.call(Request::User360 { query: q }, Pend::User360 { tab: id });
            a.tabs[i].running = Some(job);
            a.set_focus(Focus::Result);
        });
    }
}

fn items_index(fks: &[Fk], pick: &str, incoming: bool) -> Option<usize> {
    fks.iter().position(|f| {
        let target = if incoming { &f.table } else { &f.ref_table };
        format!("{}.{} ({})", target.schema, target.table, f.name) == pick
    })
}

pub fn redis_len(v: &RawValue) -> usize {
    match v.typ.as_str() {
        "hash" => v.fields.len(),
        "list" => v.items.len(),
        "set" => v.members.len(),
        "zset" => v.zmembers.len(),
        "string" => 1,
        _ => 0,
    }
}

/// 고른 항목의 (대상, 지금 값)이다. hash 는 (필드, 값), zset 은 (멤버, 점수), list 는 (자리, 값)이다.
pub fn redis_item(v: &RawValue, i: usize) -> Option<(String, String)> {
    match v.typ.as_str() {
        "hash" => v.fields.get(i).map(|f| (f[0].clone(), f[1].clone())),
        "list" => v.items.get(i).map(|x| (i.to_string(), x.clone())),
        "set" => v.members.get(i).map(|m| (m.clone(), m.clone())),
        "zset" => v.zmembers.get(i).map(|z| (z[0].clone(), z[1].clone())),
        "string" => Some((String::new(), v.string.clone())),
        _ => None,
    }
}

pub fn summary_of(rs: &ResultSet) -> String {
    if rs.cols.is_empty() {
        return format!("{} · {}", one_line(&rs.text), fmt_dur(rs.elapsed_ms));
    }
    let mut s = format!("{} rows · {}", rs.rows.len(), fmt_dur(rs.elapsed_ms));
    if rs.truncated {
        s.push_str(&format!(" · {}에서 자름", idb_core::result::MAX_ROWS));
    }
    s
}

/// 제출 미리 보기 문장이다(R15.2). serve 의 changes_sql 과 같은 글을 만들려 idb-engine 이 내보낸 꼴을 쓴다.
fn idb_engine_preview(cs: &idb_engine::ChangeSet) -> Vec<String> {
    idb_engine::changes_preview(cs, None).unwrap_or_else(|e| vec![format!("-- {e}")])
}

fn undo_preview(cs: &idb_engine::ChangeSet, applied: &idb_engine::Applied) -> Result<Vec<String>, String> {
    idb_engine::changes_preview(cs, Some(applied))
}

impl Tab {
    pub fn is_redis_view(&self) -> bool {
        self.redis.is_some()
    }
}
