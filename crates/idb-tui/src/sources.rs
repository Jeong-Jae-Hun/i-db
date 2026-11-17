//! 읽기 전용 저장소(R60~R68) 화면: 트리 펼치기·미리 보기, 편집기 실행, 범위 돌리기(Ctrl+], R62.2), 거절 처리.
//! OpenSearch·Loki·Prometheus·Grafana·DynamoDB·Athena·aws 모두 Engine 의 source_children·source_open·source_run 으로 읽는다.

use std::time::Instant;

use idb_core::project::go_quote;
use idb_core::sqlsafe::{quote_ident, statement_at};
use idb_engine::{CODE_REFUSED, EngineError, REASON_CONFIRM_EXPENSIVE, Request, ResultSet, SourceRunRequest};
use serde_json::Value;

use crate::app::{App, Pend};
use crate::grid::Grid;
use crate::help::Focus;
use crate::tree::NodeKind;

const MINUTE_NS: i64 = 60_000_000_000;
const HOUR_NS: i64 = 60 * MINUTE_NS;
const DAY_NS: i64 = 24 * HOUR_NS;
/// Ctrl+] 가 도는 Loki·Prometheus 범위다(R62.2)
pub const RANGES_NS: [i64; 5] = [15 * MINUTE_NS, HOUR_NS, 6 * HOUR_NS, DAY_NS, 7 * DAY_NS];
/// 범위를 고르지 않은 탭의 범위다(engine 기본과 같은 1시간)
const DEFAULT_RANGE_NS: i64 = HOUR_NS;
/// 트리 Enter 미리 보기 행 수(engine OPEN_LIMIT 과 같다)
const OPEN_LIMIT: usize = 50;

pub const MSG_TREE_ONLY: &str = "aws 접속은 트리로 본다";
const MSG_EXPENSIVE: &str = "비싼 질의";

/// 읽기 전용 저장소 종류다(PostgreSQL·Redis 가 아닌 것).
pub fn is_source_kind(kind: &str) -> bool {
    matches!(
        kind,
        "opensearch" | "loki" | "prometheus" | "grafana" | "dynamodb" | "athena" | "aws"
    )
}

/// 편집기 실행에 범위를 싣는 종류다. Grafana 는 Loki·Prometheus 데이터 소스를 거쳐 보내 같이 쓴다.
pub fn uses_range(kind: &str) -> bool {
    matches!(kind, "loki" | "prometheus" | "grafana")
}

fn range_or_default(ns: i64) -> i64 {
    if ns <= 0 { DEFAULT_RANGE_NS } else { ns }
}

/// 범위의 짧은 글이다(15m·1h·24h·7d). Go rangeLabel 과 같다.
pub fn range_label(ns: i64) -> String {
    let d = range_or_default(ns);
    if d % DAY_NS == 0 && d >= 2 * DAY_NS {
        format!("{}d", d / DAY_NS)
    } else if d % HOUR_NS == 0 {
        format!("{}h", d / HOUR_NS)
    } else if d % MINUTE_NS == 0 {
        format!("{}m", d / MINUTE_NS)
    } else {
        format!("{}s", d / 1_000_000_000)
    }
}

/// RANGES_NS 에서 다음 범위다. 목록에 없으면 처음부터 돈다.
pub fn next_range(ns: i64) -> i64 {
    let d = range_or_default(ns);
    match RANGES_NS.iter().position(|&r| r == d) {
        Some(i) => RANGES_NS[(i + 1) % RANGES_NS.len()],
        None => RANGES_NS[0],
    }
}

/// 미리 보기가 보내는 요청을 편집기 질의 꼴로 쓴 글이다. 기록(R11.1)에 남기고, DynamoDB·Athena 는 비싼 질의 확인 뒤
/// force 로 다시 보낼 질의이기도 하다. aws 노드(R67)는 질의가 아니라 빈 글이다.
/// idb-tui 는 idb-sources 에 기대지 않아 opensearch::open_text·obs::open_query·athena::open_sql 의 꼴을 여기 옮겼다.
fn open_text(kind: &str, path: &str) -> String {
    match kind {
        "opensearch" => {
            let index = path.split('/').next().unwrap_or("");
            format!("POST {index}/_search\n{{\"query\":{{\"match_all\":{{}}}},\"size\":{OPEN_LIMIT}}}")
        }
        "loki" => loki_open(path).unwrap_or_default(),
        "grafana" => {
            let (uid, rest) = path.split_once('/').unwrap_or((path, ""));
            let q = loki_open(rest).unwrap_or_else(|| rest.to_string());
            format!("# datasource: {uid}\n{q}")
        }
        "prometheus" => path.to_string(),
        "dynamodb" => {
            let table = path.split('/').next().unwrap_or(path);
            format!("SELECT * FROM {}", quote_ident(table))
        }
        "athena" => {
            let (db, table) = path.split_once('/').unwrap_or((path, ""));
            format!("SELECT * FROM {}.{} LIMIT {OPEN_LIMIT}", quote_ident(db), quote_ident(table))
        }
        _ => String::new(),
    }
}

fn loki_open(path: &str) -> Option<String> {
    let (label, value) = path.split_once('/').filter(|(l, _)| !l.is_empty())?;
    Some(format!("{{{label}={}}}", go_quote(value)))
}

/// 저장소 실행 하나(편집기 실행·트리 미리 보기)다. retry 는 비싼 질의를 확인받은 뒤 force 로 다시 보낼 요청이다.
pub struct SourceJob {
    pub tab: u64,
    pub conn: usize,
    /// 기록에 남길 질의 글. 비면 남기지 않는다(aws 노드)
    pub sql: String,
    pub retry: Option<SourceRunRequest>,
    pub started: Instant,
}

impl App {
    pub fn is_source(&self, c: usize) -> bool {
        is_source_kind(&self.conns[c].kind)
    }

    // ─── 트리 ───

    /// 접속 줄(path "")이나 저장소 노드의 자식을 읽는다.
    pub(crate) fn load_source_children(&mut self, n: usize) {
        let node = self.tree.node(n);
        let (conn, path) = match &node.kind {
            NodeKind::Conn(c) => (*c, String::new()),
            NodeKind::Source { conn, path, .. } => (*conn, path.clone()),
            _ => return,
        };
        let name = self.conns[conn].name.clone();
        self.call(Request::SourceChildren { conn: name, path }, Pend::SourceKids { conn, node: n });
    }

    pub(crate) fn source_kids_done(&mut self, conn: usize, node: usize, res: Result<Value, EngineError>) {
        match res.and_then(|v| serde_json::from_value::<Vec<idb_engine::Node>>(v).map_err(|e| EngineError::run(format!("bad result: {e}"))))
        {
            Ok(kids) => {
                let q = if matches!(self.tree.node(node).kind, NodeKind::Conn(_)) {
                    self.queries_for(conn)
                } else {
                    None
                };
                self.tree.set_source_children(node, conn, q, kids);
            }
            Err(e) => {
                // 다시 펼치면 다시 읽게 접어 둔다
                self.tree.nodes[node].expanded = false;
                self.tree.rebuild();
                self.status = format!("읽기 실패: {}", one_line(&e.error));
            }
        }
    }

    /// 트리 노드 Enter 미리 보기(source_open)를 결과 표에 연다. 쓰던 글·파일·다른 접속의 화면은 덮지 않게 새 탭에 연다.
    /// DynamoDB 테이블·Athena 파티션 테이블은 비싼 질의라 확인 뒤 그 질의를 force 로 보낸다(R63.1·R68.1).
    pub(crate) fn open_source_node(&mut self, n: usize) {
        let NodeKind::Source { conn, path, .. } = self.tree.node(n).kind.clone() else {
            return;
        };
        let ti = self.target_tab(conn);
        if self.tabs[ti].running.is_some() {
            self.status = "실행 중 (Ctrl+C 로 취소)".into();
            return;
        }
        self.set_conn(ti, Some(conn));
        let kind = self.conns[conn].kind.clone();
        let name = self.conns[conn].name.clone();
        let sql = open_text(&kind, &path);
        let retry = matches!(kind.as_str(), "dynamodb" | "athena").then(|| SourceRunRequest {
            conn: name.clone(),
            text: sql.clone(),
            limit: OPEN_LIMIT,
            force: true,
            ..Default::default()
        });
        let job = SourceJob {
            tab: self.tabs[ti].id,
            conn,
            sql,
            retry,
            started: Instant::now(),
        };
        self.tab = ti;
        self.start_source(Request::SourceOpen { conn: name, path }, job);
    }

    // ─── 편집기 ───

    /// 편집기 실행(Ctrl+R)을 source_run 으로 보낸다(R60.3). 읽기 전용이라 prod 확인·잠금(R6·R54)은 없고,
    /// 비싼 질의는 확인 뒤 force 로 다시 보낸다. Loki·Prometheus·Grafana 는 탭 범위를 싣는다(R62.2).
    pub(crate) fn run_source(&mut self, ti: usize, conn: usize, explain: bool) {
        if self.conns[conn].kind == "aws" {
            self.status = MSG_TREE_ONLY.into();
            return;
        }
        if explain {
            self.status = "이 접속 종류에서는 안 됨".into();
            return;
        }
        if self.tabs[ti].running.is_some() {
            self.status = "실행 중 (Ctrl+C 로 취소)".into();
            return;
        }
        let ed = &self.tabs[ti].editor;
        let text = match ed.selected_text() {
            Some(s) => s.trim().to_string(),
            None => statement_at(ed.value(), ed.cursor()).trim().to_string(),
        };
        if text.is_empty() {
            self.status = "실행할 문장이 없음".into();
            return;
        }
        let req = SourceRunRequest {
            conn: self.conns[conn].name.clone(),
            text: text.clone(),
            since_ns: if uses_range(&self.conns[conn].kind) {
                range_or_default(self.tabs[ti].since)
            } else {
                0
            },
            ..Default::default()
        };
        let job = SourceJob {
            tab: self.tabs[ti].id,
            conn,
            sql: text,
            retry: Some(SourceRunRequest {
                force: true,
                ..req.clone()
            }),
            started: Instant::now(),
        };
        self.start_source(Request::SourceRun(req), job);
    }

    /// Ctrl+] 로 탭의 범위를 15m·1h·6h·24h·7d 로 돌린다(R62.2).
    pub(crate) fn cycle_range(&mut self) {
        let t = self.cur();
        if !t.conn.is_some_and(|c| uses_range(&self.conns[c].kind)) {
            self.status = "범위는 Loki·Prometheus·Grafana 탭에서만 바꿈".into();
            return;
        }
        let next = next_range(t.since);
        self.cur_mut().since = next;
        self.status = format!("범위: 최근 {}", range_label(next));
    }

    // ─── 실행·답 ───

    pub(crate) fn start_source(&mut self, req: Request, job: SourceJob) {
        let Some(ti) = self.tab_index(job.tab) else { return };
        let conn = job.conn;
        let id = self.call(req, Pend::Source(Box::new(job)));
        let t = &mut self.tabs[ti];
        t.running = Some(id);
        t.running_read_only = true;
        t.cancelled = false;
        t.result_conn = Some(conn);
        t.view = None;
        t.redis = None;
    }

    /// 저장소 답이다. 읽기 전용 거절·요금 막음은 서버에 가지 않아 기록하지 않고 상태 줄에만 알린다(R60.2·R72).
    /// 비싼 질의는 `비싼 질의: <까닭>: y/N` 을 묻고 y 면 force 로 다시 보낸다. 다시 보낼 요청이 없으면 알리기만 한다.
    pub(crate) fn source_done(&mut self, id: u64, job: SourceJob, res: Result<Value, EngineError>) {
        let Some(ti) = self.tab_index(job.tab) else { return };
        if self.tabs[ti].running != Some(id) {
            return;
        }
        self.tabs[ti].running = None;
        let res = res.and_then(|v| serde_json::from_value::<ResultSet>(v).map_err(|e| EngineError::run(format!("bad result: {e}"))));
        match res {
            Ok(rs) => {
                if !job.sql.is_empty() {
                    self.record(job.conn, &job.sql, rs.elapsed_ms, Some(&rs), "");
                }
                self.show_source(ti, rs);
            }
            Err(e) if e.code == CODE_REFUSED && e.reason.as_deref() == Some(REASON_CONFIRM_EXPENSIVE) => {
                let Some(retry) = job.retry else {
                    let reason = e.error.strip_prefix(&format!("{MSG_EXPENSIVE}: ")).unwrap_or("");
                    self.status = format!("{MSG_EXPENSIVE}라 보내지 않음");
                    if !reason.is_empty() {
                        self.status.push_str(&format!(" ({reason})"));
                    }
                    return;
                };
                let q = e.error.trim_end_matches(": y/N").to_string();
                let again = SourceJob {
                    tab: job.tab,
                    conn: job.conn,
                    sql: retry.text.clone(),
                    retry: None,
                    started: Instant::now(),
                };
                self.confirm(q, move |a| a.start_source(Request::SourceRun(retry), again));
            }
            Err(e) if e.code == CODE_REFUSED => self.status = one_line(&e.error),
            Err(e) => {
                if !job.sql.is_empty() {
                    self.record(job.conn, &job.sql, job.started.elapsed().as_millis() as u64, None, &e.error);
                }
                let t = &mut self.tabs[ti];
                t.err = e.error.clone();
                t.grid = None;
                t.text.clear();
                t.summary = if t.cancelled { "취소됨".into() } else { "실패".into() };
                self.status = format!("실패: {}", one_line(&e.error));
            }
        }
    }

    fn show_source(&mut self, ti: usize, rs: ResultSet) {
        let cur_id = self.cur().id;
        let t = &mut self.tabs[ti];
        t.err.clear();
        t.subs.retain(|p| p.pinned);
        t.sub = None;
        t.summary = crate::actions::summary_of(&rs);
        // 서버 다시 읽기(R61.3·R62.3·R63.3)는 아직 없어 받은 행 안에서 정렬·거른다(R14.6)
        t.base = None;
        t.base_why = "이 저장소는 서버 정렬 없음";
        t.last_sql.clear();
        if rs.cols.is_empty() {
            t.grid = None;
            t.text = rs.text;
        } else {
            t.text.clear();
            t.grid = Some(Grid::new(rs));
        }
        let s = t.summary.clone();
        let title = t.display_name();
        self.status = if t.id == cur_id { s } else { format!("{title}: {s}") };
        if self.tab == ti && self.focus == Focus::Tree {
            self.set_focus(Focus::Result);
        }
    }
}

fn one_line(s: &str) -> String {
    idb_core::textutil::one_line(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 범위는_15m_1h_6h_24h_7d_로_돈다() {
        let mut d = 0;
        let mut seen = Vec::new();
        for _ in 0..6 {
            d = next_range(d);
            seen.push(range_label(d));
        }
        assert_eq!(seen, ["6h", "24h", "7d", "15m", "1h", "6h"]);
        assert_eq!(range_label(0), "1h");
        assert_eq!(next_range(3 * HOUR_NS), 15 * MINUTE_NS);
    }

    #[test]
    fn 미리_보기_질의_글은_종류마다_보내는_요청과_같다() {
        assert_eq!(
            open_text("opensearch", "logs-2024/field"),
            "POST logs-2024/_search\n{\"query\":{\"match_all\":{}},\"size\":50}"
        );
        assert_eq!(open_text("loki", "app/api"), "{app=\"api\"}");
        assert_eq!(open_text("grafana", "lk1/app/api"), "# datasource: lk1\n{app=\"api\"}");
        assert_eq!(open_text("grafana", "pm1/up"), "# datasource: pm1\nup");
        assert_eq!(open_text("dynamodb", "users/gsi"), "SELECT * FROM \"users\"");
        assert_eq!(open_text("athena", "logs/events"), "SELECT * FROM \"logs\".\"events\" LIMIT 50");
        assert_eq!(open_text("aws", "lambda/f"), "");
    }
}
