//! 실행 기록(R11): 기록 줄 붙이기, 결과 사본, Ctrl+H 기록 창. 기록은 이 기계의 개인 상태 폴더에 두고
//! (원격 serve 를 써도 화면 쪽에 남는다) idb-core history 모듈로 읽고 쓴다. 쓰기는 화면 밖 스레드에서 한다.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use idb_core::history::{self, Entry, GoTime, Results, Snapshot};
use idb_core::redact::{MASKED_SQL, mask_secrets};
use idb_engine::ResultSet;

use crate::app::App;
use crate::grid::Grid;
use crate::help::Focus;
use crate::input::LineInput;

const WINDOW: usize = 500;

pub struct History {
    store: Arc<history::Store>,
    results: Option<Arc<Results>>,
    /// 사본 쓰기를 끈 알림을 한 번만 낸다
    off_noted: Arc<AtomicBool>,
}

/// Ctrl+H 기록 창 상태다.
pub struct HistoryView {
    pub filter: LineInput,
    pub sel: usize,
    pub list: Vec<Entry>,
}

impl History {
    /// 기록 파일과 사본 폴더를 연다. 열지 못하면 그 까닭을 돌려준다(기록 없이 계속한다).
    pub fn open() -> Result<History, String> {
        let path = history::default_path().map_err(|e| e.to_string())?;
        let store = history::Store::open(&path).map_err(|e| format!("실행 기록을 열지 못함: {e}"))?;
        let results = if std::env::var("DB_HISTORY_RESULTS").as_deref() == Ok("0") {
            None
        } else {
            Results::open(&history::results_dir(&path)).ok().map(Arc::new)
        };
        if let Some(r) = &results {
            let r = r.clone();
            std::thread::spawn(move || {
                let _ = r.prune(GoTime::now());
            });
        }
        Ok(History {
            store: Arc::new(store),
            results,
            off_noted: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn list(&self, filter: &str) -> Vec<Entry> {
        let mut f = history::parse_query(filter, &chrono::Local::now());
        f.limit = WINDOW;
        self.store.list(&f)
    }

    pub fn snapshot(&self, id: &str) -> Option<Snapshot> {
        self.results.as_ref()?.read(id).ok()
    }

    pub fn has_snapshot(&self, id: &str) -> bool {
        self.results.as_ref().is_some_and(|r| r.has(id))
    }

    pub fn delete_snapshot(&self, id: &str) -> bool {
        self.results.as_ref().is_some_and(|r| r.has(id) && r.delete(id).is_ok())
    }

    /// 실행 하나를 기록한다. keep 이면(표·글 결과, prod 가 아니거나 DB_HISTORY_PROD=1) 사본을 먼저 쓰고 result 를 단다.
    pub fn record(&self, conn: &str, sql: &str, ms: u64, rs: Option<&ResultSet>, err: &str, prod: bool) {
        let sql = mask_secrets(sql);
        let now = GoTime::now();
        let id = history::new_id(now);
        let rows = rs.map_or(0, |r| r.rows.len() as i64);
        let mut e = Entry {
            id: id.clone(),
            time: now,
            conn: conn.to_string(),
            sql: sql.clone(),
            elapsed: ms as i64 * 1_000_000,
            rows,
            err: err.to_string(),
            result: false,
        };
        let keep_prod = !prod || std::env::var("DB_HISTORY_PROD").as_deref() == Ok("1");
        let snap = match (rs, &self.results) {
            (Some(r), Some(_)) if keep_prod && sql != MASKED_SQL && err.is_empty() && (!r.cols.is_empty() || !r.text.is_empty()) => {
                Some(Snapshot {
                    id,
                    time: now,
                    conn: conn.to_string(),
                    sql,
                    cols: r.cols.clone(),
                    types: r.types.clone().unwrap_or_default(),
                    rows: r.rows.clone(),
                    text: r.text.clone(),
                    elapsed: ms as i64 * 1_000_000,
                    row_count: r.rows.len() as i64,
                    truncated: false,
                    result_truncated: r.truncated,
                })
            }
            _ => None,
        };
        let (store, results, noted) = (self.store.clone(), self.results.clone(), self.off_noted.clone());
        std::thread::spawn(move || {
            if let (Some(s), Some(r)) = (snap, results)
                && !r.off()
            {
                match r.write(s) {
                    Ok(()) => {
                        e.result = true;
                        let _ = r.maybe_prune(GoTime::now());
                    }
                    Err(err) if history::is_too_large(&err) => {}
                    Err(_) => {
                        r.disable();
                        noted.store(true, Ordering::Relaxed);
                    }
                }
            }
            let _ = store.append(e);
        });
    }

    /// 사본 쓰기를 껐으면 한 번 참이다(상태 줄 알림).
    pub fn take_off_note(&self) -> bool {
        self.off_noted.swap(false, Ordering::Relaxed)
    }
}

/// 기록 창 한 줄: `◆ MM-DD HH:MM · 접속 · N행 · 0.12s · SQL 첫 줄`(실패는 ✗).
pub fn line(e: &Entry, has_snap: bool) -> String {
    let mark = if !e.err.is_empty() {
        "✗ "
    } else if has_snap {
        "◆ "
    } else {
        "  "
    };
    let t = e.time.0.with_timezone(&chrono::Local).format("%m-%d %H:%M");
    let first = mask_secrets(&e.sql).lines().next().unwrap_or("").to_string();
    format!(
        "{mark}{t} · {} · {}행 · {:.2}s · {}",
        e.conn,
        e.rows,
        e.elapsed as f64 / 1e9,
        idb_core::textutil::printable(&first, false)
    )
}

impl App {
    pub(crate) fn record(&self, conn: usize, sql: &str, ms: u64, rs: Option<&ResultSet>, err: &str) {
        if let Some(h) = &self.history {
            let c = &self.conns[conn];
            h.record(&c.name, sql, ms, rs, err, c.env == "prod");
        }
    }

    pub fn open_history(&mut self) {
        let Some(h) = &self.history else {
            self.status = "실행 기록을 쓰지 않음".into();
            return;
        };
        let list = h.list("");
        self.history_view = Some(HistoryView {
            filter: LineInput::default(),
            sel: 0,
            list,
        });
    }

    pub(crate) fn history_key(&mut self, k: &str) {
        let Some(h) = &self.history else { return };
        let Some(v) = &mut self.history_view else { return };
        match k {
            "esc" => self.history_view = None,
            "up" => v.sel = v.sel.saturating_sub(1),
            "down" => v.sel = (v.sel + 1).min(v.list.len().saturating_sub(1)),
            "enter" | "ctrl+e" => {
                let Some(e) = v.list.get(v.sel).cloned() else { return };
                self.history_view = None;
                let snap = if k == "enter" {
                    self.history.as_ref().and_then(|h| h.snapshot(&e.id))
                } else {
                    None
                };
                match snap {
                    Some(s) => self.open_snapshot(&e, s),
                    None => self.insert_history_sql(&e),
                }
            }
            "ctrl+d" => {
                let Some(e) = v.list.get(v.sel) else { return };
                let t = e.time.0.with_timezone(&chrono::Local).format("%m-%d %H:%M").to_string();
                self.status = if h.delete_snapshot(&e.id) {
                    format!("결과 사본을 지움: {t}")
                } else {
                    "이 기록에는 결과 사본이 없음".into()
                };
            }
            _ => {
                if v.filter.key(k) {
                    v.list = h.list(&v.filter.text);
                    v.sel = 0;
                }
            }
        }
    }

    /// 기록의 SQL 을 편집기에 넣는다. 빈 탭이면 그 탭에, 아니면 새 탭에. 탭에 접속이 없을 때만 기록의 접속으로 맞춘다.
    fn insert_history_sql(&mut self, e: &Entry) {
        let i = if self.cur().reusable() { self.tab } else { self.new_tab() };
        if self.tabs[i].conn.is_none() {
            let c = self.conns.iter().position(|c| c.name == e.conn);
            self.set_conn(i, c);
        }
        self.tabs[i].editor.set_value(&e.sql);
        self.tabs[i].editor.typed = false;
        self.set_focus(Focus::Editor);
    }

    /// 사본을 새 탭에 읽기 전용으로 연다(R11.4).
    fn open_snapshot(&mut self, e: &Entry, s: Snapshot) {
        let i = self.new_tab();
        let conn = self.conns.iter().position(|c| c.name == e.conn);
        self.set_conn(i, conn);
        let at = e.time.0.with_timezone(&chrono::Local).format("%m-%d %H:%M").to_string();
        let rs = ResultSet {
            cols: s.cols,
            types: if s.types.is_empty() { None } else { Some(s.types) },
            rows: s.rows,
            text: s.text,
            truncated: s.result_truncated,
            elapsed_ms: (s.elapsed / 1_000_000) as u64,
            ..ResultSet::default()
        };
        let what = if rs.cols.is_empty() {
            "글".to_string()
        } else {
            format!("{} rows", s.row_count)
        };
        let summary = format!(
            "기록 사본 {at} · {} · {what} · {} · 읽기 전용 (r 다시 실행)",
            idb_core::textutil::printable(&e.conn, false),
            crate::actions::fmt_dur(rs.elapsed_ms)
        );
        let t = &mut self.tabs[i];
        t.title = format!("기록 · {at}");
        t.editor.set_value(&e.sql);
        t.editor.typed = false;
        t.snapshot = true;
        if rs.cols.is_empty() {
            t.text = rs.text;
        } else {
            t.grid = Some(Grid::new(rs));
        }
        t.summary = summary.clone();
        self.status = summary;
        self.set_focus(Focus::Result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 기록_줄은_사본과_실패를_표시하고_비밀_문장은_가린다() {
        let t = GoTime::parse("2026-11-16T09:05:00+09:00").unwrap();
        let e = Entry {
            time: t,
            conn: "app-stg".into(),
            sql: "select 1\nfrom t".into(),
            elapsed: 120_000_000,
            rows: 3,
            ..Entry::default()
        };
        let l = line(&e, true);
        assert!(
            l.starts_with("◆ 11-16 ") && l.ends_with(" · app-stg · 3행 · 0.12s · select 1"),
            "{l}"
        );
        let bad = Entry {
            sql: "ALTER ROLE app PASSWORD 'x'".into(),
            err: "boom".into(),
            ..e
        };
        assert!(line(&bad, false).starts_with("✗ ") && line(&bad, false).ends_with(MASKED_SQL));
    }
}
