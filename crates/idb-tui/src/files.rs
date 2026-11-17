//! queries 파일(R5·R43·R44·R45), 즐겨찾기(R29), 내보내기(R14.2), 긴 실행 알림(R30).
//! 쿼리 파일은 이 기계의 프로젝트 폴더에 있다. `--remote` 로 켜 프로젝트 폴더가 없으면 Files 가 없고 이 기능들은 쉰다.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::app::App;
use crate::help::Focus;

const AUTOSAVE_STOPPED: &str = "파일이 밖에서 바뀌어 자동 저장 멈춤 (Ctrl+S 로 덮어쓰기)";
const AUTOSAVE_OUTSIDE: &str = "파일의 실제 경로가 프로젝트 밖이라 자동 저장 멈춤";

pub struct Files {
    pub dir: PathBuf,
    /// queries 상대 경로(queries/…) → 접속 uuid
    pub mapping: HashMap<String, String>,
    /// 최근 수정 순
    pub list: Vec<String>,
}

fn autosave_on() -> bool {
    std::env::var("DB_AUTO_SAVE").as_deref() != Ok("0")
}

/// rel 의 실제 경로다. 심볼릭 링크를 따라간 경로가 프로젝트 밖이면 오류다(R45.2).
pub fn real_path(dir: &Path, rel: &str) -> Result<PathBuf, String> {
    let root = fs::canonicalize(dir).map_err(|e| e.to_string())?;
    let mut p = dir.join(rel);
    let mut rest = PathBuf::new();
    let real = loop {
        match fs::canonicalize(&p) {
            Ok(r) => break r.join(&rest),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let Some(name) = p.file_name().map(PathBuf::from) else {
                    return Err(e.to_string());
                };
                // 빈 PathBuf 를 join 하면 끝에 / 가 붙어 폴더로 읽힌다
                rest = if rest.as_os_str().is_empty() { name } else { name.join(rest) };
                if !p.pop() {
                    return Err(e.to_string());
                }
            }
            Err(e) => return Err(e.to_string()),
        }
    };
    if !real.starts_with(&root) {
        return Err(format!("real path is outside the project: {rel}"));
    }
    Ok(real)
}

impl Files {
    pub fn new(dir: PathBuf, mapping: HashMap<String, String>) -> Files {
        let mut f = Files {
            dir,
            mapping,
            list: Vec::new(),
        };
        f.reload();
        f
    }

    /// queries/ 아래 .sql 파일을 최근 수정 순으로 다시 읽는다. 읽지 못하는 곳과 밖을 가리키는 링크는 뺀다.
    pub fn reload(&mut self) {
        let mut out: Vec<(String, SystemTime)> = Vec::new();
        let mut stack = vec![self.dir.join("queries")];
        while let Some(d) = stack.pop() {
            let Ok(rd) = fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                let Ok(ft) = e.file_type() else { continue };
                let Ok(rel) = p.strip_prefix(&self.dir) else { continue };
                let rel = rel.to_string_lossy().replace('\\', "/");
                if ft.is_symlink() && real_path(&self.dir, &rel).is_err() {
                    continue;
                }
                let Ok(meta) = fs::metadata(&p) else { continue };
                if meta.is_dir() {
                    stack.push(p);
                } else if rel.ends_with(".sql") {
                    out.push((rel, meta.modified().unwrap_or(SystemTime::UNIX_EPOCH)));
                }
            }
        }
        out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        self.list = out.into_iter().map(|x| x.0).collect();
    }

    pub fn for_conn(&self, uuid: &str) -> Vec<String> {
        if uuid.is_empty() {
            return Vec::new();
        }
        self.list
            .iter()
            .filter(|f| self.mapping.get(*f).is_some_and(|u| u == uuid))
            .cloned()
            .collect()
    }

    /// 어느 접속에도 붙지 않은 파일이다(R43 `쿼리 (접속 없음)`).
    pub fn loose(&self, conns: &[idb_engine::ConnSummary]) -> Vec<String> {
        self.list
            .iter()
            .filter(|f| self.mapping.get(*f).is_none_or(|u| !conns.iter().any(|c| &c.uuid == u)))
            .cloned()
            .collect()
    }

    pub fn read(&self, rel: &str) -> Result<String, String> {
        let p = real_path(&self.dir, rel)?;
        fs::read_to_string(p).map_err(|e| e.to_string())
    }

    /// 원자적으로 쓴다(R5.2). 권한은 원본을 따르고 새 파일은 0644 다.
    pub fn write(&self, rel: &str, text: &str) -> Result<(), String> {
        let p = real_path(&self.dir, rel).map_err(|_| AUTOSAVE_OUTSIDE.to_string())?;
        let perm = fs::metadata(&p)
            .map(|m| std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o777)
            .unwrap_or(0o644);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        idb_core::atomicfile::write(&p, ".db-*", text.as_bytes(), perm).map_err(|e| e.to_string())
    }
}

/// ~/Downloads(없으면 임시 폴더)에 새 파일(0600)로 쓴다(R14.2).
pub fn export_file(title: &str, ext: &str, body: &[u8]) -> Result<String, String> {
    let dir = std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join("Downloads"))
        .filter(|d| d.is_dir())
        .unwrap_or_else(std::env::temp_dir);
    let safe = idb_core::mapping::path_safe(title);
    let name = format!("db-{safe}-{}.{ext}", chrono::Local::now().format("%Y%m%d-%H%M%S"));
    let path = dir.join(name);
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| e.to_string())?;
    f.write_all(body).map_err(|e| e.to_string())?;
    Ok(path.display().to_string())
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
        let conn = f.mapping.get(rel).and_then(|u| self.conns.iter().position(|c| &c.uuid == u));
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
        let Some(f) = &self.files else {
            self.status = "원격 접속에서는 쿼리 파일을 쓰지 않음".into();
            return;
        };
        let items = f.list.clone();
        if items.is_empty() {
            self.status = "queries/ 에 .sql 파일이 없음".into();
            return;
        }
        self.picker("쿼리 파일 열기", items, |a, rel| a.open_query_file(&rel));
    }

    /// 그 접속을 쓰는 탭이 없으면 가장 최근 파일을 연다(R44).
    pub fn auto_open(&mut self, conn: usize) {
        if std::env::var("DB_AUTO_OPEN").as_deref() == Ok("0") || self.files.is_none() {
            return;
        }
        if self.tabs.iter().filter(|t| t.conn == Some(conn)).count() > 1 || self.tabs.iter().any(|t| t.conn == Some(conn) && !t.reusable())
        {
            return;
        }
        let uuid = self.conns[conn].uuid.clone();
        let f = self.files.as_ref().unwrap();
        let recent: Vec<String> = self.session.recent_file(&uuid);
        let pick = recent
            .into_iter()
            .find(|r| f.list.contains(r))
            .or_else(|| f.for_conn(&uuid).into_iter().next());
        if let Some(rel) = pick {
            self.open_query_file(&rel);
        }
    }

    /// 파일 탭을 디스크에 맞춰 자동 저장한다(R45.2). 밖에서 바뀌었으면 멈춘다.
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
        let on_disk = f.read(&rel).ok();
        if on_disk.as_deref() == Some(text.as_str()) {
            self.tabs[i].disk = text;
            return;
        }
        if on_disk.as_deref() != Some(self.tabs[i].disk.as_str()) {
            self.tabs[i].autosave_stopped = true;
            self.status = AUTOSAVE_STOPPED.into();
            return;
        }
        match f.write(&rel, &text) {
            Ok(()) => {
                self.tabs[i].disk = text;
                self.tabs[i].dirty_at = None;
            }
            Err(e) => {
                self.tabs[i].autosave_stopped = true;
                self.status = e;
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
    }

    /// Ctrl+S: 연 파일에 덮어쓴다(자동 저장이 멈췄어도). 연 파일이 없으면 알린다(R5.2).
    pub fn save_file(&mut self, ti: usize) {
        let Some(f) = &self.files else {
            self.status = "원격 접속에서는 쿼리 파일을 쓰지 않음".into();
            return;
        };
        let Some(rel) = self.tabs[ti].file.clone() else {
            self.status = "연 파일이 없음 (실행하면 queries/ 에 저장)".into();
            return;
        };
        let text = self.tabs[ti].editor.value().to_string();
        let existed = f.read(&rel).is_ok();
        match f.write(&rel, &text) {
            Ok(()) => {
                let t = &mut self.tabs[ti];
                t.disk = text;
                t.autosave_stopped = false;
                t.dirty_at = None;
                self.status = if existed {
                    format!("저장함: {rel}")
                } else {
                    format!("지워진 파일을 다시 만듦: {rel}")
                };
                if let Some(f) = &mut self.files {
                    f.reload();
                }
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
        let group = if conn.group.is_empty() { String::new() } else { conn.group.clone() };
        let text = t.editor.value().to_string();
        let f = self.files.as_mut().unwrap();
        let rel = match idb_core::mapping::unique_query_path(&f.dir, &group, &conn.name, chrono::Local::now()) {
            Ok(r) => r,
            Err(e) => {
                self.status = format!("새 쿼리 저장 실패: {e}");
                return;
            }
        };
        if let Err(e) = f.write(&rel, &text) {
            self.status = format!("새 쿼리 저장 실패: {e}");
            return;
        }
        let mut note = format!("저장함: {rel}");
        if !conn.uuid.is_empty() {
            match real_path(&f.dir, ".idea/data_source_mapping.xml")
                .map_err(|e| e.to_string())
                .and_then(|_| idb_core::mapping::add_mapping(&f.dir, &rel, &conn.uuid).map_err(|e| e.to_string()))
            {
                Ok(()) => {
                    f.mapping.insert(rel.clone(), conn.uuid.clone());
                }
                Err(e) => note = format!("saved {rel} but mapping failed: {e}"),
            }
        }
        f.reload();
        let t = &mut self.tabs[ti];
        t.file = Some(rel.clone());
        t.disk = text;
        self.status = note;
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
        let f = self.files.as_ref()?;
        if !t.autosave_stopped || text.trim().is_empty() || f.read(&rel).ok().as_deref() == Some(text.as_str()) {
            return None;
        }
        let copy = idb_core::mapping::conflict_path(&f.dir, &rel, chrono::Local::now()).ok()?;
        f.write(&copy, &text).ok()?;
        Some(format!("저장 안 한 글을 {copy} 에 남김"))
    }

    // ─── 즐겨찾기 (R29) ───

    pub fn open_favorites(&mut self) {
        let Some(f) = &self.files else {
            self.status = "원격 접속에서는 즐겨찾기를 쓰지 않음".into();
            return;
        };
        let store = match idb_core::favorites::Store::load(&f.dir) {
            Ok(s) => s,
            Err(e) => {
                self.status = format!("즐겨찾기를 읽지 못함: {e}");
                return;
            }
        };
        let favs = store.list();
        if favs.is_empty() {
            self.status = "즐겨찾기 없음 (F4 로 저장)".into();
            return;
        }
        let names: Vec<String> = favs.iter().map(|f| f.name.clone()).collect();
        let dir = f.dir.clone();
        let dir2 = dir.clone();
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
                    let r = idb_core::favorites::Store::load(&dir2).and_then(|s| s.delete(&name));
                    a.status = match r {
                        Ok(()) => format!("지움: {name}"),
                        Err(e) => format!("지우지 못함: {e}"),
                    };
                });
            }));
        }
        let _ = dir;
    }

    pub fn save_favorite(&mut self) {
        let Some(f) = &self.files else {
            self.status = "원격 접속에서는 즐겨찾기를 쓰지 않음".into();
            return;
        };
        let dir = f.dir.clone();
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
            let store = match idb_core::favorites::Store::load(&dir) {
                Ok(s) => s,
                Err(e) => {
                    a.status = format!("즐겨찾기를 읽지 못함: {e}");
                    return;
                }
            };
            let fav = idb_core::favorites::Fav {
                name: name.clone(),
                conn,
                sql,
                saved: idb_core::history::GoTime::now(),
            };
            let exists = store.get(&name).is_some();
            let put = move |a: &mut App| {
                a.status = match store.put(fav) {
                    Ok(()) => format!("즐겨찾기 저장: {name}"),
                    Err(e) => format!("저장 실패: {e}"),
                };
            };
            if exists {
                a.confirm("같은 이름이 있어 덮어쓸까요", put);
            } else {
                put(a);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 새_파일은_없는_폴더까지_만들어_쓴다() {
        let d = tempfile::tempdir().unwrap();
        let f = Files::new(d.path().to_path_buf(), HashMap::new());
        f.write("queries/g/a.sql", "select 1").unwrap();
        assert_eq!(fs::read_to_string(d.path().join("queries/g/a.sql")).unwrap(), "select 1");
    }
}
