//! 개인 상태 파일: 세션 복원(R41·R46.2·R40.3), 최근 기록(R75), 파라미터 값(R55), 되돌리기 로그(R50.1).
//! 모두 idb-core 의 상태 모듈을 쓰고 레포가 아닌 $XDG_STATE_HOME/db 에 둔다. 시험은 disabled() 로 파일을 건드리지 않는다.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use idb_core::history::GoTime;
use idb_core::{paramstore, recent, session};
use idb_engine::{Applied, ChangeSet};

use crate::app::App;

const MAX_UNDO_LOG: u64 = 5 << 20;
const SECRET_MASK: &str = "***";

pub struct SessionCtx {
    project: PathBuf,
    path: Option<PathBuf>,
    restore: bool,
    pub state: session::State,
    recent: Option<recent::Store>,
    params_path: Option<PathBuf>,
    params: BTreeMap<String, String>,
    pub notes: Vec<String>,
}

impl SessionCtx {
    /// 파일을 읽지도 쓰지도 않는다(시험).
    pub fn disabled() -> SessionCtx {
        SessionCtx {
            project: PathBuf::new(),
            path: None,
            restore: false,
            state: session::State::default(),
            recent: None,
            params_path: None,
            params: BTreeMap::new(),
            notes: Vec::new(),
        }
    }

    /// 시험용: path 의 세션 파일에만 쓴다(최근 기록·파라미터 값 없음).
    #[cfg(test)]
    pub fn at(project: &Path, path: PathBuf) -> SessionCtx {
        let mut c = SessionCtx::disabled();
        c.project = project.to_path_buf();
        c.path = Some(path);
        c.restore = true;
        c
    }

    /// project 의 세션·최근 기록·파라미터 값을 읽는다. 원격이면 project 는 serve 쪽 경로이고 키로만 쓴다.
    pub fn load(project: &Path) -> SessionCtx {
        let mut c = SessionCtx::disabled();
        c.project = project.to_path_buf();
        c.restore = std::env::var("DB_RESTORE").as_deref() != Ok("0");
        c.path = session::path().ok();
        if let Some(p) = &c.path {
            match session::load(p, project) {
                Ok(Some(st)) => c.state = st,
                Ok(None) => {}
                Err(e) if session::is_corrupt(&e) => c
                    .notes
                    .push(format!("세션 파일이 깨져 새로 시작 ({})", session::bad_path(p).display())),
                Err(e) => c.notes.push(format!("세션을 읽지 못함: {e}")),
            }
        }
        if std::env::var("DB_RECENT").as_deref() != Ok("0")
            && let Ok(p) = recent::path()
            && let Ok((store, err)) = recent::load(&p, project)
        {
            if let Some(e) = err {
                c.notes.push(format!("최근 기록을 읽지 못해 새로 시작: {e}"));
            }
            c.recent = Some(store);
        }
        if let Ok(p) = paramstore::path() {
            c.params = paramstore::load(&p, project).unwrap_or_default();
            c.params_path = Some(p);
        }
        c
    }

    pub fn closed_groups(&self) -> Vec<String> {
        self.recent.as_ref().map(|r| r.tree().1).unwrap_or_default()
    }

    /// 테이블 보기의 지난 정렬·WHERE 다(R75.4). prod 는 남기지 않아 없다.
    pub fn table_view(&self, conn: &str, schema: &str, table: &str, prod: bool) -> Option<(String, String)> {
        let t = self.recent.as_ref()?.table(conn, schema, table)?;
        if prod || (t.order.is_empty() && t.where_.is_empty()) {
            return None;
        }
        Some((t.order, t.where_))
    }

    pub fn set_table_view(&mut self, conn: &str, schema: &str, table: &str, order: &str, where_: &str, prod: bool) {
        if let Some(r) = &mut self.recent
            && r.set_view(conn, schema, table, order, where_, prod, GoTime::now())
        {
            let _ = r.save();
        }
    }

    pub fn touch_table(&mut self, conn: &str, schema: &str, table: &str) {
        self.state.touch_table(conn, schema, table, GoTime::now());
        if let Some(r) = &mut self.recent {
            r.touch_table(conn, schema, table, GoTime::now());
            r.use_conn(conn, GoTime::now());
            let _ = r.save();
        }
    }

    pub fn add_filter(&mut self, f: &str) {
        if let Some(r) = &mut self.recent {
            r.add_filter(f);
            let _ = r.save();
        }
    }

    pub fn use_action(&mut self, key: &str) {
        if let Some(r) = &mut self.recent {
            r.use_action(key, GoTime::now());
            let _ = r.save();
        }
    }

    pub fn top_actions(&self, n: usize) -> Vec<String> {
        self.recent.as_ref().map(|r| r.top_actions(GoTime::now(), n)).unwrap_or_default()
    }

    pub fn recent_tables(&self) -> Vec<session::RecentTable> {
        self.state.recent_tables.clone()
    }

    pub fn param(&self, name: &str) -> String {
        self.params.get(name).cloned().unwrap_or_default()
    }

    pub fn set_param(&mut self, name: &str, v: &str) {
        self.params.insert(name.into(), v.into());
        if let Some(p) = &self.params_path {
            let one = BTreeMap::from([(name.to_string(), v.to_string())]);
            let _ = paramstore::save(p, &self.project, &one);
        }
    }

    /// 접속의 최근 파일 맨 앞에 file 을 둔다(R44).
    pub fn touch_file(&mut self, conn: Option<String>, file: Option<String>) {
        if let (Some(c), Some(f)) = (conn, file) {
            self.state.touch(&c, &f, GoTime::now());
        }
    }

    pub fn recent_file(&self, conn: &str) -> Vec<String> {
        self.state
            .recent
            .get(conn)
            .map(|v| v.iter().map(|r| r.file.clone()).collect())
            .unwrap_or_default()
    }
}

impl App {
    /// 세션의 탭·창 크기를 되살린다(R41·R46.2). 결과·테이블 보기·트랜잭션은 되살리지 않는다.
    pub(crate) fn restore_session(&mut self) {
        let st = self.session.state.clone();
        self.tree_w = st.panes.tree;
        self.ed_h = st.panes.editor;
        if let Some(n) = self.session.notes.first() {
            self.status = n.clone();
        }
        if !self.session.restore {
            return;
        }
        let mut dropped = 0;
        for t in &st.tabs {
            let i = self.new_tab();
            let conn = self.conns.iter().position(|c| !t.conn.is_empty() && c.uuid == t.conn);
            self.set_conn(i, conn);
            let tab = &mut self.tabs[i];
            tab.title = t.title.clone();
            tab.name = t.name.clone();
            tab.pinned = t.pinned;
            dropped += usize::from(t.dropped);
            let mut text = t.text.clone();
            if !t.file.is_empty()
                && let Some(f) = &self.files
            {
                // 연 파일이 지워졌으면 저장 안 한 글만 되살리고 파일 연결은 끊는다
                if let Ok(disk) = f.read(&t.file) {
                    tab.file = Some(t.file.clone());
                    tab.disk = disk.clone();
                    if text.is_empty() {
                        text = disk;
                    } else if !t.disk.is_empty() && session::disk_hash(disk.as_bytes()) != t.disk {
                        // 그사이 파일이 바뀌었으면 파일 내용으로 열고 저장 안 한 글은 뒤 새 탭에 둔다
                        let name = tab.display_name();
                        let kept = std::mem::replace(&mut text, disk);
                        tab.editor.set_value(&text);
                        let j = self.new_tab();
                        self.tabs[j].name = format!("{name} (저장 안 한 글)");
                        self.tabs[j].editor.set_value(&kept);
                        self.status = format!("{name} 파일이 바뀌어 저장 안 한 글을 새 탭에 둠");
                        continue;
                    }
                }
            }
            let tab = &mut self.tabs[i];
            tab.editor.set_value(&text);
            tab.editor.set_cursor(t.cursor.max(0) as usize);
            tab.editor.typed = false;
        }
        if dropped > 0 {
            self.status = format!("글이 커서 비운 탭 {dropped}개");
        }
        if !self.tabs.is_empty() {
            self.tab = (st.active.max(0) as usize).min(self.tabs.len() - 1);
        }
    }

    /// 끝낼 때 탭과 창 크기를 저장한다. 창 크기는 DB_RESTORE=0 이어도 남긴다(R46.2).
    pub fn save_session(&mut self) -> Result<(), String> {
        for i in 0..self.tabs.len() {
            self.autosave(i);
        }
        let Some(path) = self.session.path.clone() else { return Ok(()) };
        let mut st = self.session.state.clone();
        st.panes.tree = self.tree_w;
        st.panes.editor = self.ed_h;
        st.tabs.clear();
        if self.session.restore {
            // 대시보드·흐름·보드 탭(R64·R69·R70)은 글이 없어 되살리지 않고, 활성 탭 자리는 남은 탭 기준으로 맞춘다.
            // 기록 사본 탭은 사용자가 글을 고쳤을 때만 남긴다(R11.4)
            st.active = 0;
            for (i, t) in self.tabs.iter().enumerate() {
                if t.pane.is_some() || t.snapshot && !t.editor.typed {
                    continue;
                }
                if i == self.tab {
                    st.active = st.tabs.len() as i64;
                }
                let text = t.editor.value().to_string();
                let unchanged = t.file.is_some() && text == t.disk;
                st.tabs.push(session::Tab {
                    title: t.title.clone(),
                    text: if unchanged { String::new() } else { text },
                    file: t.file.clone().unwrap_or_default(),
                    disk: if t.file.is_some() && !unchanged {
                        session::disk_hash(t.disk.as_bytes())
                    } else {
                        String::new()
                    },
                    conn: t.conn.map(|c| self.conns[c].uuid.clone()).unwrap_or_default(),
                    cursor: t.editor.cursor() as i64,
                    dropped: false,
                    name: t.name.clone(),
                    pinned: t.pinned,
                });
            }
        }
        if let Some(r) = &mut self.session.recent {
            let closed = self.tree.closed_groups();
            let (open, _) = r.tree();
            if r.set_tree(&open, &closed) {
                let _ = r.save();
            }
        }
        session::save(&path, &self.session.project, &st).map_err(|e| format!("session save failed: {e}"))
    }
}

/// 되돌리기 묶음을 undo.jsonl 에 한 줄 덧붙인다(R50.1). 비밀 같은 열의 값은 가린다. 실패해도 화면은 멈추지 않는다.
pub fn log_undo(conn_uuid: &str, cs: &ChangeSet, applied: &Applied) {
    let Ok(sp) = session::path() else { return };
    let path = sp.with_file_name("undo.jsonl");
    let mut masked = cs.clone();
    let hide = |m: &mut BTreeMap<String, Option<String>>| {
        for (k, v) in m.iter_mut() {
            if paramstore::looks_secret(k) && v.is_some() {
                *v = Some(SECRET_MASK.into());
            }
        }
    };
    for ch in &mut masked.changes {
        for (k, v) in ch.key.iter_mut() {
            if paramstore::looks_secret(k) {
                *v = SECRET_MASK.into();
            }
        }
        hide(&mut ch.values);
        hide(&mut ch.old);
    }
    let mut ap = applied.clone();
    for r in ap.rows.iter_mut().flatten() {
        hide(r);
    }
    let Ok(undo) = idb_engine::changes_preview(&masked, Some(&ap)) else {
        return;
    };
    let line = serde_json::json!({
        "at": GoTime::now(),
        "conn": conn_uuid,
        "table": format!("{}.{}", cs.table.schema, cs.table.table),
        "undo": undo,
    });
    let _ = std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")));
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_UNDO_LOG) {
        let _ = std::fs::rename(&path, path.with_extension("jsonl.1"));
    }
    use std::os::unix::fs::OpenOptionsExt;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(&path) {
        let _ = f.write_all(format!("{line}\n").as_bytes());
    }
}
