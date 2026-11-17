//! queries 파일(R5·R43·R44·R45), 즐겨찾기(R29), 내보내기(R14.2), 긴 실행 알림(R30).
//! 쿼리 파일과 즐겨찾기는 엔진 op(`query_*`·`favorite*`)로만 읽고 쓴다. 로컬이면 이 기계의 프로젝트, `--remote` 면
//! serve 쪽 프로젝트다. 경계(queries/ 밖·링크 탈출·1MiB)는 엔진이 지킨다. 저장은 화면을 닫을 때도 끝나야 해서
//! 일감 대신 그 자리에서 기다리는 호출이다.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use idb_core::favorites::Fav;
use idb_engine::{EngineError, FavoriteSaved, QueryFile, QueryText, QueryWritten, Request};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::app::App;
use crate::help::Focus;

const AUTOSAVE_STOPPED: &str = "파일이 밖에서 바뀌어 자동 저장 멈춤 (Ctrl+S 로 덮어쓰기)";
const NO_FILES: &str = "쿼리 파일을 쓸 수 없음";

/// 엔진에 요청 하나를 보내고 답을 기다린다.
pub type Call = Box<dyn Fn(Request) -> Result<Value, EngineError>>;

pub struct Files {
    call: Call,
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

impl Files {
    pub fn new(call: Call) -> Files {
        let mut f = Files { call, list: Vec::new() };
        f.reload();
        f
    }

    /// 엔진을 rt 위에서 부른다. 원격이 멈춰도 화면이 굳지 않게 20초에서 끊는다.
    pub fn engine(engine: idb_engine::Engine, rt: tokio::runtime::Handle) -> Files {
        Files::new(Box::new(move |req| {
            rt.block_on(async {
                tokio::time::timeout(std::time::Duration::from_secs(20), engine.call(req))
                    .await
                    .unwrap_or_else(|_| Err(EngineError::run("쿼리 파일 요청이 20초 안에 끝나지 않음")))
            })
        }))
    }

    fn req<T: DeserializeOwned>(&self, r: Request) -> Result<T, String> {
        let v = (self.call)(r).map_err(|e| e.error)?;
        serde_json::from_value(v).map_err(|e| e.to_string())
    }

    /// 목록을 다시 읽는다. 읽지 못하면 비운다.
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

    pub fn read(&self, rel: &str) -> Result<String, String> {
        self.req::<QueryText>(Request::QueryRead { path: rel.into() }).map(|t| t.content)
    }

    /// 원자적으로 쓴다(R5.2). disk 는 마지막으로 읽거나 쓴 내용이고 그사이 바뀌었으면 conflict 다. force 는 그래도 덮는다.
    pub fn write(&self, rel: &str, text: &str, disk: &str, force: bool) -> Result<QueryWritten, String> {
        self.req(Request::QueryWrite {
            path: rel.into(),
            content: text.into(),
            base_hash: base_hash(disk),
            force,
        })
    }

    fn after_new(&mut self, w: Result<QueryWritten, String>) -> Result<QueryWritten, String> {
        if w.is_ok() {
            self.reload();
        }
        w
    }

    /// 새 쿼리 파일을 엔진이 정한 곳에 쓰고 그 접속을 붙인다(R45.1).
    pub fn create(&mut self, conn: &str, text: &str) -> Result<QueryWritten, String> {
        let w = self.req(Request::QueryNew {
            conn: conn.into(),
            content: text.into(),
        });
        self.after_new(w)
    }

    /// 덮을 수 없는 글을 rel 옆 충돌 사본에 남긴다(R45.3).
    pub fn conflict_copy(&mut self, rel: &str, text: &str, conn: &str) -> Result<QueryWritten, String> {
        let w = self.req(Request::QueryConflict {
            path: rel.into(),
            content: text.into(),
            conn: conn.into(),
        });
        self.after_new(w)
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
    /// 파일을 연다(R5.1·R43). 이미 연 탭이면 그 탭으로, 빈 탭이면 그 탭에, 아니면 새 탭에 연다.
    pub fn open_query_file(&mut self, rel: &str) {
        if let Some(i) = self.tabs.iter().position(|t| t.file.as_deref() == Some(rel)) {
            self.switch_tab(i);
            self.set_focus(Focus::Editor);
            return;
        }
        let Some(f) = &self.files else { return };
        let text = match f.read(rel) {
            Ok(t) => t,
            Err(e) => {
                self.status = format!("열지 못함: {e}");
                return;
            }
        };
        let conn = f.mapped(rel).and_then(|u| self.conns.iter().position(|c| c.uuid == u));
        let i = if self.cur().reusable() { self.tab } else { self.new_tab() };
        if conn.is_some() {
            self.set_conn(i, conn);
        }
        let t = &mut self.tabs[i];
        t.editor.set_value(&text);
        t.editor.typed = false;
        t.file = Some(rel.to_string());
        t.disk = text;
        t.autosave_stopped = false;
        self.tab = i;
        self.session
            .touch_file(conn.map(|c| self.conns[c].uuid.clone()), Some(rel.to_string()));
        self.set_focus(Focus::Editor);
    }

    pub fn open_file_picker(&mut self) {
        let Some(f) = &mut self.files else {
            self.status = NO_FILES.into();
            return;
        };
        f.reload();
        let items = f.paths();
        if items.is_empty() {
            self.status = "queries/ 에 .sql 파일이 없음".into();
            return;
        }
        self.picker("쿼리 파일 열기", items, |a, rel| a.open_query_file(&rel));
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

    /// 파일 탭을 디스크에 맞춰 자동 저장한다(R45.2). 밖에서 바뀌었으면(엔진의 conflict) 멈춘다.
    pub fn autosave(&mut self, i: usize) {
        if !autosave_on() {
            return;
        }
        let Some(f) = &self.files else { return };
        let t = &self.tabs[i];
        let (Some(rel), false) = (t.file.clone(), t.autosave_stopped) else {
            return;
        };
        let text = t.editor.value().to_string();
        if text == t.disk {
            return;
        }
        match f.write(&rel, &text, &t.disk, false) {
            Ok(w) if w.conflict => {
                self.tabs[i].autosave_stopped = true;
                self.status = AUTOSAVE_STOPPED.into();
            }
            Ok(_) => {
                self.tabs[i].disk = text;
                self.tabs[i].dirty_at = None;
            }
            Err(e) => {
                self.tabs[i].autosave_stopped = true;
                self.status = format!("자동 저장 멈춤: {e}");
            }
        }
    }

    /// 마지막 고침 1초 뒤 자동 저장한다(R45.2). Runner 가 틱마다 부른다.
    pub fn tick(&mut self) {
        let due: Vec<usize> = (0..self.tabs.len())
            .filter(|&i| self.tabs[i].dirty_at.is_some_and(|d| d.elapsed().as_millis() >= 1000))
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
        let Some(f) = &mut self.files else {
            self.status = NO_FILES.into();
            return;
        };
        let Some(rel) = self.tabs[ti].file.clone() else {
            self.status = "연 파일이 없음 (실행하면 queries/ 에 저장)".into();
            return;
        };
        let text = self.tabs[ti].editor.value().to_string();
        match f.write(&rel, &text, &self.tabs[ti].disk, true) {
            Ok(w) => {
                f.reload();
                let t = &mut self.tabs[ti];
                t.disk = text;
                t.autosave_stopped = false;
                t.dirty_at = None;
                self.status = if w.created {
                    format!("지워진 파일을 다시 만듦: {rel}")
                } else {
                    format!("저장함: {rel}")
                };
            }
            Err(e) => self.status = format!("저장 실패: {e}"),
        }
    }

    /// 파일 없는 PostgreSQL 탭의 첫 실행이면 queries/<그룹>/<접속>-<시각>.sql 에 저장하고 매핑에 붙인다(R45.1).
    pub fn save_new_query(&mut self, ti: usize) {
        if !autosave_on() || self.files.is_none() {
            return;
        }
        let t = &self.tabs[ti];
        let Some(c) = t.conn else { return };
        if t.file.is_some() || !self.is_pg(c) || t.editor.value().trim().is_empty() {
            return;
        }
        let conn = self.conns[c].clone();
        let text = t.editor.value().to_string();
        let f = self.files.as_mut().unwrap();
        let w = match f.create(&conn.name, &text) {
            Ok(w) => w,
            Err(e) => {
                self.status = format!("새 쿼리 저장 실패: {e}");
                return;
            }
        };
        let rel = w.path;
        let t = &mut self.tabs[ti];
        t.file = Some(rel.clone());
        t.disk = text;
        self.status = if w.mapping_error.is_empty() {
            format!("저장함: {rel}")
        } else {
            w.mapping_error
        };
        self.session.touch_file(Some(conn.uuid), Some(rel));
        self.reattach_queries(c);
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

    /// 탭을 닫을 때 남길 글을 충돌 사본·새 쿼리에 둔다(R45.3). 남긴 경로 알림을 돌려준다.
    pub fn keep_text(&mut self, i: usize) -> Option<String> {
        if !autosave_on() {
            return None;
        }
        let t = &self.tabs[i];
        if t.file.is_none() && t.editor.typed {
            self.save_new_query(i);
            return self.tabs[i].file.as_ref().map(|f| format!("저장함: {f}"));
        }
        let rel = t.file.clone()?;
        let text = t.editor.value().to_string();
        let conn = t.conn.map(|c| self.conns[c].name.clone()).unwrap_or_default();
        let name = t.display_name();
        let stopped = t.autosave_stopped;
        let f = self.files.as_mut()?;
        if !stopped || text.trim().is_empty() || f.read(&rel).ok().as_deref() == Some(text.as_str()) {
            return None;
        }
        match f.conflict_copy(&rel, &text, &conn) {
            Ok(w) => {
                let copy = w.path;
                // 화면을 닫은 뒤 터미널에도 알린다(R45.3)
                self.quit_notes.push(format!("saved unsaved text of {rel} to {copy}"));
                Some(format!("저장 안 한 글을 {copy} 에 남김"))
            }
            Err(e) => {
                self.quit_notes.push(format!("could not save {name}: {e}"));
                None
            }
        }
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
