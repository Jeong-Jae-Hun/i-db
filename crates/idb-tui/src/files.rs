//! queries 파일(R5·R43·R44·R45), 즐겨찾기(R29), 내보내기(R14.2), 긴 실행 알림(R30).
//! 쿼리 파일과 즐겨찾기는 엔진 op(`query_*`·`favorite*`)로만 읽고 쓴다. 로컬이면 이 기계의 프로젝트, `--remote` 면
//! serve 쪽 프로젝트다. 경계(queries/ 밖·링크 탈출·1MiB)는 엔진이 지킨다.
//! 열기·목록·자동 저장·Ctrl+S·첫 실행 저장·탭 닫기 저장은 일감으로 보내 원격이 느려도 화면이 서지 않는다.
//! 화면을 끝낼 때의 저장만 그 자리에서 기다리고, 모두 합쳐 20초 안에 끝낸다. 즐겨찾기 창은 사람이 고르는 동안
//! 한 번 부르는 것이라 기다리는 호출로 둔다.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use idb_core::favorites::Fav;
use idb_engine::{EngineError, FavoriteSaved, QueryFile, QueryText, QueryWritten, Request};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::app::{App, Pend};
use crate::help::Focus;

const AUTOSAVE_STOPPED: &str = "파일이 밖에서 바뀌어 자동 저장 멈춤 (Ctrl+S 로 덮어쓰기)";
const NO_FILES: &str = "쿼리 파일을 쓸 수 없음";
/// 기다리는 호출 하나의 상한이자 끝낼 때 저장 전체의 상한이다
const WAIT: Duration = Duration::from_secs(20);

/// 엔진에 요청 하나를 보내고 길어야 둘째 값만큼 답을 기다린다.
pub type Call = Box<dyn Fn(Request, Duration) -> Result<Value, EngineError>>;

pub struct Files {
    call: Call,
    /// 끝낼 때 저장의 마감이다. 있으면 기다리는 호출이 모두 이 안에서 끝난다
    deadline: Option<Instant>,
    /// 최근 수정 순
    pub list: Vec<QueryFile>,
}

fn autosave_on() -> bool {
    std::env::var("DB_AUTO_SAVE").as_deref() != Ok("0")
}

/// 화면이 마지막으로 읽거나 쓴 내용의 해시다(query_write 의 base_hash).
fn base_hash(disk: &str) -> String {
    idb_engine::queries::hash(disk)
}

fn write_req(rel: &str, text: &str, disk: &str, force: bool) -> Request {
    Request::QueryWrite {
        path: rel.into(),
        content: text.into(),
        base_hash: base_hash(disk),
        force,
    }
}

/// 보낸 쿼리 파일 일감과 답을 받아 할 일이다.
pub enum FileJob {
    Open {
        rel: String,
    },
    List {
        picker: bool,
    },
    /// 열린 탭의 자동 저장(force=false)·Ctrl+S(force=true)
    Save {
        tab: u64,
        text: String,
        force: bool,
    },
    /// 파일 없는 탭의 첫 실행 저장
    New {
        tab: u64,
        text: String,
        conn: usize,
    },
    /// 닫은 탭의 글을 남긴다. 실패하면 탭을 되살린다
    Keep(Kept),
}

pub struct Kept {
    rel: Option<String>,
    text: String,
    /// 그 탭이 마지막으로 읽거나 쓴 내용(덮기의 base_hash)
    disk: String,
    name: String,
    conn: Option<usize>,
    /// 덮기가 충돌해 사본을 쓰는 중이다
    copying: bool,
}

impl Files {
    pub fn new(call: Call) -> Files {
        let mut f = Files {
            call,
            deadline: None,
            list: Vec::new(),
        };
        f.reload();
        f
    }

    /// 엔진을 rt 위에서 부른다.
    pub fn engine(engine: idb_engine::Engine, rt: tokio::runtime::Handle) -> Files {
        Files::new(Box::new(move |req, wait| {
            rt.block_on(async {
                tokio::time::timeout(wait, engine.call(req))
                    .await
                    .unwrap_or_else(|_| Err(EngineError::run("쿼리 파일 요청이 시간 안에 끝나지 않음")))
            })
        }))
    }

    /// 지금부터 기다리는 호출을 모두 합쳐 20초 안에 끝낸다(화면을 끝낼 때).
    pub fn start_deadline(&mut self) {
        self.deadline = Some(Instant::now() + WAIT);
    }

    fn req<T: DeserializeOwned>(&self, r: Request) -> Result<T, String> {
        let wait = match self.deadline {
            Some(d) => d.saturating_duration_since(Instant::now()),
            None => WAIT,
        };
        if wait.is_zero() {
            return Err("끝낼 때 저장이 20초를 넘겨 멈춤".into());
        }
        let v = (self.call)(r, wait).map_err(|e| e.error)?;
        serde_json::from_value(v).map_err(|e| e.to_string())
    }

    /// 목록을 다시 읽는다(켤 때). 읽지 못하면 비운다.
    pub fn reload(&mut self) {
        self.list = self.req(Request::QueryList).unwrap_or_default();
    }

    pub fn paths(&self) -> Vec<String> {
        self.list.iter().map(|f| f.path.clone()).collect()
    }

    /// rel 이 붙은 접속 uuid 다.
    pub fn mapped(&self, rel: &str) -> Option<&str> {
        self.list
            .iter()
            .find(|f| f.path == rel && !f.conn_uuid.is_empty())
            .map(|f| f.conn_uuid.as_str())
    }

    pub fn for_conn(&self, uuid: &str) -> Vec<String> {
        if uuid.is_empty() {
            return Vec::new();
        }
        self.list.iter().filter(|f| f.conn_uuid == uuid).map(|f| f.path.clone()).collect()
    }

    /// 어느 접속에도 붙지 않은 파일이다(R43 `쿼리 (접속 없음)`).
    pub fn loose(&self, conns: &[idb_engine::ConnSummary]) -> Vec<String> {
        self.list
            .iter()
            .filter(|f| !conns.iter().any(|c| !c.uuid.is_empty() && c.uuid == f.conn_uuid))
            .map(|f| f.path.clone())
            .collect()
    }

    /// 새로 만든 파일을 다시 읽지 않고 목록 맨 앞에 둔다.
    fn add(&mut self, w: &QueryWritten) {
        self.list.retain(|f| f.path != w.path);
        let conn_uuid = if w.mapping_error.is_empty() {
            w.uuid.clone()
        } else {
            String::new()
        };
        self.list.insert(
            0,
            QueryFile {
                path: w.path.clone(),
                conn_uuid,
                ..Default::default()
            },
        );
    }

    pub fn read(&self, rel: &str) -> Result<String, String> {
        self.req::<QueryText>(Request::QueryRead { path: rel.into() }).map(|t| t.content)
    }

    /// 원자적으로 쓴다(R5.2). disk 는 마지막으로 읽거나 쓴 내용이고 그사이 바뀌었으면 conflict 다. force 는 그래도 덮는다.
    pub fn write(&self, rel: &str, text: &str, disk: &str, force: bool) -> Result<QueryWritten, String> {
        self.req(write_req(rel, text, disk, force))
    }

    /// 새 쿼리 파일을 엔진이 정한 곳에 쓰고 그 접속을 붙인다(R45.1).
    pub fn create(&mut self, conn: &str, text: &str) -> Result<QueryWritten, String> {
        let w: QueryWritten = self.req(Request::QueryNew {
            conn: conn.into(),
            content: text.into(),
        })?;
        self.add(&w);
        Ok(w)
    }

    /// 덮을 수 없는 글을 rel 옆 충돌 사본에 남긴다(R45.3).
    pub fn conflict_copy(&mut self, rel: &str, text: &str, conn: &str) -> Result<QueryWritten, String> {
        let w: QueryWritten = self.req(Request::QueryConflict {
            path: rel.into(),
            content: text.into(),
            conn: conn.into(),
        })?;
        self.add(&w);
        Ok(w)
    }

    pub fn favorites(&self) -> Result<Vec<Fav>, String> {
        self.req(Request::FavoritesList)
    }

    pub fn favorite_add(&self, name: &str, conn: &str, sql: &str, overwrite: bool) -> Result<FavoriteSaved, String> {
        self.req(Request::FavoriteAdd {
            name: name.into(),
            conn: conn.into(),
            sql: sql.into(),
            overwrite,
        })
    }

    pub fn favorite_remove(&self, name: &str) -> Result<(), String> {
        self.req::<Value>(Request::FavoriteRemove { name: name.into() }).map(|_| ())
    }
}

/// ~/Downloads(없으면 임시 폴더)에 새 파일(0600)로 쓴다(R14.2).
pub fn export_file(title: &str, ext: &str, body: &[u8]) -> Result<String, String> {
    let (path, mut f) = create_export(title, ext)?;
    f.write_all(body).map_err(|e| e.to_string())?;
    Ok(path)
}

/// 내보내기 파일을 새로 만든다(~/Downloads, 없으면 임시 폴더, 0600, 새 파일만, R14.2·R33.1).
pub fn create_export(title: &str, ext: &str) -> Result<(String, fs::File), String> {
    let dir = std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join("Downloads"))
        .filter(|d| d.is_dir())
        .unwrap_or_else(std::env::temp_dir);
    let safe = idb_core::mapping::path_safe(title);
    let name = format!("db-{safe}-{}.{ext}", chrono::Local::now().format("%Y%m%d-%H%M%S"));
    let path = dir.join(name);
    use std::os::unix::fs::OpenOptionsExt;
    let f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| e.to_string())?;
    Ok((path.display().to_string(), f))
}

/// 긴 실행 알림(R30). OSC 9 에 SQL·값을 넣지 않는다. stdout 이 터미널일 때만 보낸다.
pub fn notify(conn: &str, ms: u64, rows: usize, failed: bool) {
    let secs = std::env::var("DB_NOTIFY_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&s| s <= 86_400)
        .unwrap_or(10);
    if secs == 0 || ms < secs * 1000 || !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        return;
    }
    let what = if failed {
        "실패".to_string()
    } else {
        format!("{}행", idb_core::textutil::group_digits(rows as i64))
    };
    let conn = idb_core::textutil::printable(conn, false);
    let _ = write!(std::io::stdout(), "\x1b]9;db: {conn} 끝 {:.1}s · {what}\x07", ms as f64 / 1000.0);
}

impl App {
    fn file_call(&mut self, req: Request, job: FileJob) -> u64 {
        self.call(req, Pend::File(Box::new(job)))
    }

    /// 파일을 연다(R5.1·R43). 이미 연 탭이면 그 탭으로 가고, 아니면 읽기 일감을 보낸다.
    pub fn open_query_file(&mut self, rel: &str) {
        if let Some(i) = self.tabs.iter().position(|t| t.file.as_deref() == Some(rel)) {
            self.switch_tab(i);
            self.set_focus(Focus::Editor);
            return;
        }
        if self.files.is_none() {
            return;
        }
        self.file_call(Request::QueryRead { path: rel.into() }, FileJob::Open { rel: rel.into() });
    }

    /// 읽은 파일을 빈 탭이면 그 탭에, 아니면 새 탭에 둔다.
    fn opened(&mut self, rel: String, text: String) {
        if let Some(i) = self.tabs.iter().position(|t| t.file.as_deref() == Some(rel.as_str())) {
            self.switch_tab(i);
            return;
        }
        let conn = self
            .files
            .as_ref()
            .and_then(|f| f.mapped(&rel))
            .and_then(|u| self.conns.iter().position(|c| c.uuid == u));
        let i = if self.cur().reusable() { self.tab } else { self.new_tab() };
        if conn.is_some() {
            self.set_conn(i, conn);
        }
        let t = &mut self.tabs[i];
        t.editor.set_value(&text);
        t.editor.typed = false;
        t.file = Some(rel.clone());
        t.disk = text;
        t.autosave_stopped = false;
        self.tab = i;
        self.session.touch_file(conn.map(|c| self.conns[c].uuid.clone()), Some(rel));
        self.set_focus(Focus::Editor);
    }

    /// 목록을 다시 읽고 고르게 한다.
    pub fn open_file_picker(&mut self) {
        if self.files.is_none() {
            self.status = NO_FILES.into();
            return;
        }
        self.file_call(Request::QueryList, FileJob::List { picker: true });
    }

    /// 트리 r: 쿼리 파일 목록을 다시 읽는다.
    pub(crate) fn reload_files(&mut self) {
        if self.files.is_some() {
            self.file_call(Request::QueryList, FileJob::List { picker: false });
        }
    }

    /// 그 접속을 쓰는 탭이 없으면 가장 최근 파일을 연다(R44).
    pub fn auto_open(&mut self, conn: usize) {
        if std::env::var("DB_AUTO_OPEN").as_deref() == Ok("0") {
            return;
        }
        let Some(f) = &self.files else { return };
        if self.tabs.iter().filter(|t| t.conn == Some(conn)).count() > 1 || self.tabs.iter().any(|t| t.conn == Some(conn) && !t.reusable())
        {
            return;
        }
        let uuid = self.conns[conn].uuid.clone();
        let recent: Vec<String> = self.session.recent_file(&uuid);
        let pick = recent
            .into_iter()
            .find(|r| f.list.iter().any(|q| &q.path == r))
            .or_else(|| f.for_conn(&uuid).into_iter().next());
        if let Some(rel) = pick {
            self.open_query_file(&rel);
        }
    }

    /// 파일 탭을 자동 저장한다(R45.2). 밖에서 바뀌었으면(엔진의 conflict) 답에서 멈춘다. 저장이 가는 중이면 그 답 뒤에 다시 한다.
    pub fn autosave(&mut self, i: usize) {
        if !autosave_on() || self.files.is_none() {
            return;
        }
        let t = &self.tabs[i];
        let (Some(rel), false, None) = (t.file.clone(), t.autosave_stopped, t.saving) else {
            return;
        };
        let text = t.editor.value().to_string();
        if text == t.disk {
            return;
        }
        let (tab, req) = (t.id, write_req(&rel, &text, &t.disk, false));
        let id = self.file_call(req, FileJob::Save { tab, text, force: false });
        self.tabs[i].saving = Some(id);
    }

    /// 마지막 고침 1초 뒤 자동 저장한다(R45.2). Runner 가 틱마다 부른다.
    pub fn tick(&mut self) {
        let due: Vec<usize> = (0..self.tabs.len())
            .filter(|&i| self.tabs[i].dirty_at.is_some_and(|d| d.elapsed().as_millis() >= 1000) && self.tabs[i].saving.is_none())
            .collect();
        for i in due {
            self.tabs[i].dirty_at = None;
            self.autosave(i);
        }
        self.pane_tick();
        // 예상 행 수가 3초 안에 안 오면 빼고 묻는다(R39)
        if self.estimating.as_ref().is_some_and(|e| e.1.elapsed().as_secs() >= 3) {
            let (job, _, parts, then) = self.estimating.take().unwrap();
            self.cancel(job);
            self.pending.remove(&job);
            self.confirm(parts.question(""), then);
        }
    }

    /// Ctrl+S: 연 파일에 덮어쓴다(자동 저장이 멈췄어도). 연 파일이 없으면 알린다(R5.2).
    pub fn save_file(&mut self, ti: usize) {
        if self.files.is_none() {
            self.status = NO_FILES.into();
            return;
        }
        let t = &self.tabs[ti];
        let Some(rel) = t.file.clone() else {
            self.status = "연 파일이 없음 (실행하면 queries/ 에 저장)".into();
            return;
        };
        let text = t.editor.value().to_string();
        let (tab, req) = (t.id, write_req(&rel, &text, &t.disk, true));
        let id = self.file_call(req, FileJob::Save { tab, text, force: true });
        self.tabs[ti].saving = Some(id);
    }

    /// 파일 없는 PostgreSQL 탭의 첫 실행이면 queries/<그룹>/<접속>-<시각>.sql 에 저장하고 매핑에 붙인다(R45.1).
    pub fn save_new_query(&mut self, ti: usize) {
        if !autosave_on() || self.files.is_none() {
            return;
        }
        let t = &self.tabs[ti];
        let Some(c) = t.conn else { return };
        if t.file.is_some() || t.saving.is_some() || !self.is_pg(c) || t.editor.value().trim().is_empty() {
            return;
        }
        let text = t.editor.value().to_string();
        let tab = t.id;
        let req = Request::QueryNew {
            conn: self.conns[c].name.clone(),
            content: text.clone(),
        };
        let id = self.file_call(req, FileJob::New { tab, text, conn: c });
        self.tabs[ti].saving = Some(id);
    }

    /// 쿼리 파일 일감의 답이다.
    pub(crate) fn file_done(&mut self, job: FileJob, res: Result<Value, EngineError>) {
        let res = res.map_err(|e| e.error);
        match job {
            FileJob::Open { rel } => match res.and_then(|v| serde_json::from_value::<QueryText>(v).map_err(|e| e.to_string())) {
                Ok(t) => self.opened(rel, t.content),
                Err(e) => self.status = format!("열지 못함: {e}"),
            },
            FileJob::List { picker } => {
                let list: Vec<QueryFile> = match res.and_then(|v| serde_json::from_value(v).map_err(|e| e.to_string())) {
                    Ok(l) => l,
                    Err(e) => {
                        self.status = format!("쿼리 파일 목록을 읽지 못함: {e}");
                        return;
                    }
                };
                let items: Vec<String> = list.iter().map(|f| f.path.clone()).collect();
                if let Some(f) = &mut self.files {
                    f.list = list;
                }
                for c in 0..self.conns.len() {
                    self.reattach_queries(c);
                }
                if picker {
                    if items.is_empty() {
                        self.status = "queries/ 에 .sql 파일이 없음".into();
                    } else {
                        self.picker("쿼리 파일 열기", items, |a, rel| a.open_query_file(&rel));
                    }
                }
            }
            FileJob::Save { tab, text, force } => {
                let w = res.and_then(|v| serde_json::from_value::<QueryWritten>(v).map_err(|e| e.to_string()));
                let Some(i) = self.tab_index(tab) else { return };
                self.tabs[i].saving = None;
                let rel = self.tabs[i].file.clone().unwrap_or_default();
                match w {
                    Ok(w) if w.conflict => {
                        self.tabs[i].autosave_stopped = true;
                        self.status = AUTOSAVE_STOPPED.into();
                    }
                    Ok(w) => {
                        let t = &mut self.tabs[i];
                        // 보내는 사이 더 친 글은 다음 틱에 다시 저장한다
                        if t.editor.value() != text {
                            t.dirty_at = Some(Instant::now());
                        }
                        t.disk = text;
                        if force {
                            t.autosave_stopped = false;
                            self.status = if w.created {
                                format!("지워진 파일을 다시 만듦: {rel}")
                            } else {
                                format!("저장함: {rel}")
                            };
                        }
                    }
                    Err(e) if force => self.status = format!("저장 실패: {e}"),
                    Err(e) => {
                        self.tabs[i].autosave_stopped = true;
                        self.status = format!("자동 저장 멈춤: {e}");
                    }
                }
            }
            FileJob::New { tab, text, conn } => {
                let w = res.and_then(|v| serde_json::from_value::<QueryWritten>(v).map_err(|e| e.to_string()));
                let Some(i) = self.tab_index(tab) else { return };
                self.tabs[i].saving = None;
                let w = match w {
                    Ok(w) => w,
                    Err(e) => {
                        self.status = format!("새 쿼리 저장 실패: {e}");
                        return;
                    }
                };
                if let Some(f) = &mut self.files {
                    f.add(&w);
                }
                let t = &mut self.tabs[i];
                t.file = Some(w.path.clone());
                if t.editor.value() != text {
                    t.dirty_at = Some(Instant::now());
                }
                t.disk = text;
                self.status = if w.mapping_error.is_empty() {
                    format!("저장함: {}", w.path)
                } else {
                    w.mapping_error
                };
                self.session.touch_file(Some(self.conns[conn].uuid.clone()), Some(w.path));
                self.reattach_queries(conn);
            }
            FileJob::Keep(k) => self.kept(k, res),
        }
    }

    fn reattach_queries(&mut self, c: usize) {
        if let Some(n) = self.tree.conn_node(c)
            && self.tree.nodes[n].loaded
        {
            let q = self.queries_for(c);
            let kids: Vec<usize> = self.tree.nodes[n]
                .children
                .iter()
                .copied()
                .filter(|&k| !matches!(self.tree.nodes[k].kind, crate::tree::NodeKind::Queries(_)))
                .collect();
            let rest: Vec<(crate::tree::NodeKind, String)> = kids
                .iter()
                .map(|&k| (self.tree.nodes[k].kind.clone(), self.tree.nodes[k].label.clone()))
                .collect();
            // 다시 붙이면 펼친 자식 상태를 잃으므로 쿼리 묶음만 앞에 새로 둔다
            let expanded: Vec<bool> = kids.iter().map(|&k| self.tree.nodes[k].expanded).collect();
            let children: Vec<Vec<usize>> = kids.iter().map(|&k| self.tree.nodes[k].children.clone()).collect();
            self.tree.set_conn_children(n, q, rest);
            let new: Vec<usize> = self.tree.nodes[n]
                .children
                .iter()
                .copied()
                .filter(|&k| !matches!(self.tree.nodes[k].kind, crate::tree::NodeKind::Queries(_)))
                .collect();
            for (j, k) in new.into_iter().enumerate() {
                self.tree.nodes[k].expanded = expanded[j];
                self.tree.nodes[k].children = children[j].clone();
                self.tree.nodes[k].loaded = true;
            }
            self.tree.rebuild();
        }
    }

    /// 사용자가 친 글이 파일·새 쿼리 어디에도 남지 않는지다(R45.3 닫기 확인).
    pub fn unsaved_text(&self, i: usize) -> bool {
        let t = &self.tabs[i];
        t.editor.typed && t.file.is_none() && !t.editor.value().trim().is_empty()
    }

    /// 탭을 닫을 때 글을 남기는 일감을 보낸다(R45.2·R45.3). 연 파일은 덮어 보고(밖에서 바뀌었으면 충돌 사본),
    /// 파일 없는 PostgreSQL 탭의 친 글은 새 쿼리로 둔다. 보냈으면 true 다. 실패하면 답에서 탭을 되살린다.
    pub fn keep_on_close(&mut self, i: usize) -> bool {
        if !autosave_on() || self.files.is_none() {
            return false;
        }
        let t = &self.tabs[i];
        let text = t.editor.value().to_string();
        let kept = Kept {
            rel: t.file.clone(),
            text: text.clone(),
            disk: t.disk.clone(),
            name: t.display_name(),
            conn: t.conn,
            copying: false,
        };
        let req = match &t.file {
            Some(rel) if text != t.disk && !text.trim().is_empty() => write_req(rel, &text, &t.disk, false),
            None if t.editor.typed && !text.trim().is_empty() && t.conn.is_some_and(|c| self.is_pg(c)) => Request::QueryNew {
                conn: self.conns[t.conn.unwrap()].name.clone(),
                content: text,
            },
            _ => return false,
        };
        self.file_call(req, FileJob::Keep(kept));
        true
    }

    fn kept(&mut self, mut k: Kept, res: Result<Value, String>) {
        let w = res.and_then(|v| serde_json::from_value::<QueryWritten>(v).map_err(|e| e.to_string()));
        match (w, &k.rel) {
            (Ok(w), Some(rel)) if w.conflict && !k.copying => {
                let req = Request::QueryConflict {
                    path: rel.clone(),
                    content: k.text.clone(),
                    conn: k.conn.map(|c| self.conns[c].name.clone()).unwrap_or_default(),
                };
                k.copying = true;
                self.file_call(req, FileJob::Keep(k));
            }
            (Ok(w), _) => {
                let saved = w.created || k.copying;
                if saved && let Some(f) = &mut self.files {
                    f.add(&w);
                }
                self.status = match (&k.rel, k.copying) {
                    (Some(rel), true) => format!("{rel} 이 밖에서 바뀌어 닫은 탭의 글을 {} 에 남김", w.path),
                    (Some(rel), false) => format!("저장함: {rel}"),
                    (None, _) => format!("저장함: {}", w.path),
                };
                if k.rel.is_none()
                    && let Some(c) = k.conn
                {
                    self.reattach_queries(c);
                }
            }
            (Err(e), _) => {
                self.restore_kept(&k);
                self.status = format!("닫은 탭의 글을 저장하지 못해 되살림: {e}");
            }
        }
    }

    /// 남기지 못한 닫은 탭의 글을 탭으로 되살린다. 끝낼 때면 세션에 남아 다음에 다시 열린다.
    fn restore_kept(&mut self, k: &Kept) {
        let i = self.new_tab();
        self.set_conn(i, k.conn);
        let t = &mut self.tabs[i];
        t.name = format!("{} (저장 못 함)", k.name);
        t.editor.set_value(&k.text);
        t.editor.typed = true;
        t.close_ok = false;
    }

    /// 끝낼 때 답을 못 받은 닫은 탭의 남기기를 여기서 다시 한다. 같은 글을 쓰는 덮기는 두 번 해도 같고,
    /// 이미 끝난 사본·새 쿼리를 다시 하면 같은 글의 파일이 하나 더 생길 수 있다(잃는 것보다 낫다).
    fn keep_closed_sync(&mut self, k: Kept) {
        let conn = k.conn.map(|c| self.conns[c].name.clone()).unwrap_or_default();
        let Some(f) = self.files.as_mut() else { return };
        let res = match &k.rel {
            Some(rel) if k.copying => f.conflict_copy(rel, &k.text, &conn).map(|w| Some(w.path)),
            Some(rel) => match f.write(rel, &k.text, &k.disk, false) {
                Ok(w) if !w.conflict => Ok(None),
                Ok(_) => f.conflict_copy(rel, &k.text, &conn).map(|w| Some(w.path)),
                Err(e) => Err(e),
            },
            None => f.create(&conn, &k.text).map(|_| None),
        };
        match res {
            Ok(Some(copy)) => {
                let rel = k.rel.clone().unwrap_or_default();
                self.quit_notes.push(format!("saved unsaved text of {rel} to {copy}"));
            }
            Ok(None) => {}
            Err(e) => {
                self.quit_notes
                    .push(format!("could not save {}: {e} (kept in the session)", k.name));
                self.restore_kept(&k);
                // 세션에만 남긴다. 아래 keep_sync 가 같은 글을 새 쿼리로 다시 쓰려다 또 실패하지 않게 한다
                let last = self.tabs.len() - 1;
                self.tabs[last].editor.typed = false;
            }
        }
    }

    /// 끝낼 때 모든 탭의 글을 그 자리에서 남긴다(R45.1~R45.3). 기다림은 모두 합쳐 20초다.
    pub fn save_all_on_quit(&mut self) {
        if !autosave_on() {
            return;
        }
        let Some(f) = &mut self.files else { return };
        f.start_deadline();
        // 닫은 탭의 남기기와 그 충돌 사본은 답이 오지 않으니 같은 마감 안에서 여기서 다시 한다
        let keeps: Vec<u64> = self
            .pending
            .iter()
            .filter(|(_, p)| matches!(p, Pend::File(j) if matches!(**j, FileJob::Keep(_))))
            .map(|(id, _)| *id)
            .collect();
        for id in keeps {
            if let Some(Pend::File(j)) = self.pending.remove(&id) {
                self.drop_job(id);
                if let FileJob::Keep(k) = *j {
                    self.keep_closed_sync(k);
                }
            }
        }
        for i in 0..self.tabs.len() {
            self.keep_sync(i);
        }
    }

    fn keep_sync(&mut self, i: usize) {
        let t = &self.tabs[i];
        let text = t.editor.value().to_string();
        let conn = t.conn.map(|c| self.conns[c].name.clone()).unwrap_or_default();
        let name = t.display_name();
        let pg = t.conn.is_some_and(|c| self.is_pg(c));
        let (rel, disk, typed) = (t.file.clone(), t.disk.clone(), t.editor.typed);
        let Some(f) = self.files.as_mut() else { return };
        let note = match rel {
            Some(rel) if text != disk && !text.trim().is_empty() => match f.write(&rel, &text, &disk, false) {
                Ok(w) if !w.conflict => None,
                Ok(_) => Some(match f.conflict_copy(&rel, &text, &conn) {
                    Ok(w) => format!("saved unsaved text of {rel} to {}", w.path),
                    Err(e) => format!("could not save {name}: {e}"),
                }),
                Err(e) => Some(format!("could not save {name}: {e}")),
            },
            None if typed && pg && !text.trim().is_empty() => match f.create(&conn, &text) {
                Ok(_) => None,
                Err(e) => Some(format!("could not save {name}: {e}")),
            },
            _ => None,
        };
        // 화면을 닫은 뒤 터미널에 알린다(R45.3)
        self.quit_notes.extend(note);
    }

    // ─── 즐겨찾기 (R29) ───

    pub fn open_favorites(&mut self) {
        let Some(f) = &self.files else {
            self.status = NO_FILES.into();
            return;
        };
        let favs = match f.favorites() {
            Ok(v) => v,
            Err(e) => {
                self.status = format!("즐겨찾기를 읽지 못함: {e}");
                return;
            }
        };
        if favs.is_empty() {
            self.status = "즐겨찾기 없음 (F4 로 저장)".into();
            return;
        }
        let names: Vec<String> = favs.iter().map(|f| f.name.clone()).collect();
        self.picker("즐겨찾기", names, move |a, name| {
            let Some(fav) = favs.into_iter().find(|f| f.name == name) else {
                return;
            };
            let conn = a
                .conns
                .iter()
                .position(|c| !fav.conn.is_empty() && c.uuid == fav.conn)
                .or(a.cur().conn);
            let i = a.new_tab();
            a.set_conn(i, conn);
            a.tabs[i].editor.set_value(&fav.sql);
            a.tabs[i].editor.typed = false;
            a.tabs[i].name = fav.name.clone();
            a.set_focus(Focus::Editor);
        });
        if let crate::app::Overlay::Picker { del, .. } = &mut self.overlay {
            *del = Some(Box::new(move |a: &mut App, name: String| {
                a.confirm(format!("즐겨찾기 {name} 지움"), move |a| {
                    let r = a.files.as_ref().map_or(Err(NO_FILES.to_string()), |f| f.favorite_remove(&name));
                    a.status = match r {
                        Ok(()) => format!("지움: {name}"),
                        Err(e) => format!("지우지 못함: {e}"),
                    };
                });
            }));
        }
    }

    pub fn save_favorite(&mut self) {
        if self.files.is_none() {
            self.status = NO_FILES.into();
            return;
        }
        let t = self.cur();
        let sql = t.editor.selected_text().unwrap_or(t.editor.value()).to_string();
        if sql.trim().is_empty() {
            self.status = "저장할 글이 없음".into();
            return;
        }
        let conn = t.conn.map(|c| self.conns[c].uuid.clone()).unwrap_or_default();
        self.prompt("즐겨찾기 이름: ", "", move |a, name| {
            let name = match idb_core::favorites::normalize_name(&name) {
                Ok(n) => n,
                Err(e) => {
                    a.status = e.to_string();
                    return;
                }
            };
            let put = move |a: &mut App, overwrite: bool| -> bool {
                let r = a
                    .files
                    .as_ref()
                    .map_or(Err(NO_FILES.to_string()), |f| f.favorite_add(&name, &conn, &sql, overwrite));
                match r {
                    Ok(s) if s.exists => return true,
                    Ok(_) => a.status = format!("즐겨찾기 저장: {name}"),
                    Err(e) => a.status = format!("저장 실패: {e}"),
                }
                false
            };
            let put = std::rc::Rc::new(put);
            let again = put.clone();
            if put(a, false) {
                a.confirm("같은 이름이 있어 덮어쓸까요", move |a| {
                    again(a, true);
                });
            }
        });
    }
}
