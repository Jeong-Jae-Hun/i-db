//! 접속 추가·수정·삭제(R59)와 폴더 옮기기(R66.3). 저장은 엔진 op(conn_save·conn_delete·conn_folder)로 한다.
//! 원격 serve 는 접속 파일을 고치지 않아 code 3 remote_conn_edit 로 거절하고 화면은 그 글을 그대로 보인다.
//! 비밀번호는 화면에 `•` 로만 보이고 요청에만 실린다.

use idb_engine::{ConnFields, ConnSaved, ConnTested, EngineError, ProjectInfo, Request};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use serde_json::Value;

use crate::app::{App, Pend};
use crate::input::LineInput;
use crate::theme;
use crate::tree::NodeKind;

pub const LABELS: [&str; 8] = ["이름(URL 붙여넣기)", "그룹", "종류", "호스트", "포트", "DB", "사용자", "비밀번호"];
const PASSWORD: usize = 7;

pub struct ConnForm {
    /// 고치는 접속 이름. 비면 새 접속이다
    pub old_name: String,
    pub fields: Vec<LineInput>,
    pub at: usize,
    pub msg: String,
    pub has_password: bool,
    /// 시험에 실패하면 저장할지 묻는 중이다
    pub ask_save: bool,
}

fn decode<T: serde::de::DeserializeOwned>(v: Value) -> Result<T, EngineError> {
    serde_json::from_value(v).map_err(|e| EngineError::run(format!("bad result: {e}")))
}

impl ConnForm {
    pub fn new(group: &str) -> ConnForm {
        let mut fields: Vec<LineInput> = LABELS.iter().map(|_| LineInput::default()).collect();
        fields[1].set(group);
        fields[2].set("postgresql");
        ConnForm {
            old_name: String::new(),
            fields,
            at: 0,
            msg: String::new(),
            has_password: false,
            ask_save: false,
        }
    }

    fn from(c: &ConnFields) -> ConnForm {
        let mut f = ConnForm::new(&c.group);
        f.old_name = c.name.clone();
        let host = if c.host.is_empty() { &c.url } else { &c.host };
        for (i, v) in [&c.name, &c.group, &c.kind, host, &c.port.to_string(), &c.db, &c.user]
            .into_iter()
            .enumerate()
        {
            f.fields[i].set(if i == 4 && c.port == 0 { "" } else { v });
        }
        f.has_password = c.has_password;
        f
    }

    /// 첫 칸에 붙인 URL 로 나머지 칸을 채운다. URL 이 아니면 false 다(이름으로 둔다).
    pub fn fill_url(&mut self) -> bool {
        let raw = self.fields[0].text.clone();
        if !raw.contains("://") {
            return false;
        }
        match idb_core::connstore::parse_conn_url(&raw) {
            Ok((c, pw)) => {
                self.fields[0].set("");
                self.fields[2].set(c.kind_str());
                self.fields[3].set(&c.host);
                self.fields[4].set(&c.port.to_string());
                self.fields[5].set(&c.db);
                self.fields[6].set(&c.user);
                self.fields[PASSWORD].set(&pw);
                self.msg = "URL 로 칸을 채움 · 이름을 쓰세요".into();
            }
            Err(e) => {
                self.fields[0].set("");
                self.msg = e;
            }
        }
        true
    }

    pub fn fields(&self) -> ConnFields {
        let v = |i: usize| self.fields[i].text.trim().to_string();
        let kind = v(2);
        let http = !matches!(kind.as_str(), "postgresql" | "redis" | "dynamodb" | "athena" | "aws");
        ConnFields {
            name: v(0),
            group: v(1),
            host: if http { String::new() } else { v(3) },
            url: if http { v(3) } else { String::new() },
            kind,
            port: v(4).parse().unwrap_or(0),
            db: v(5),
            user: v(6),
            password: self.fields[PASSWORD].text.clone(),
            ..ConnFields::default()
        }
    }
}

impl App {
    /// 트리 N: 새 접속 창(고른 줄의 폴더를 그룹으로 채움).
    pub(crate) fn conn_new(&mut self, n: usize) {
        let node = self.tree.node(n);
        let group = match &node.kind {
            NodeKind::Group => node.label.clone(),
            _ => node.conn().map(|c| self.conns[c].group.clone()).unwrap_or_default(),
        };
        self.conn_form = Some(ConnForm::new(&group));
    }

    /// 트리 E: db 전용 접속만 고친다.
    pub(crate) fn conn_edit(&mut self, n: usize) {
        let NodeKind::Conn(c) = self.tree.node(n).kind else { return };
        if self.conns[c].source != "db" {
            self.status = "DataGrip 접속은 DataGrip 에서 고침".into();
            return;
        }
        let name = self.conns[c].name.clone();
        self.call(Request::ConnGet { name }, Pend::ConnGet);
    }

    pub(crate) fn conn_got(&mut self, res: Result<Value, EngineError>) {
        match res.and_then(decode::<ConnFields>) {
            Ok(c) => self.conn_form = Some(ConnForm::from(&c)),
            Err(e) => self.status = e.error,
        }
    }

    /// 트리 X(PostgreSQL·저장소)·Alt+X: db 전용 접속을 묻고 지운다.
    pub(crate) fn conn_remove(&mut self, n: usize) {
        let NodeKind::Conn(c) = self.tree.node(n).kind else { return };
        if self.conns[c].source != "db" {
            self.status = "DataGrip 접속은 DataGrip 에서 고침".into();
            return;
        }
        let name = self.conns[c].name.clone();
        self.confirm(format!("{name} 접속을 지울까요(.env 비밀번호도)"), move |a| {
            a.call(
                Request::ConnDelete { name: name.clone() },
                Pend::ConnChanged {
                    what: format!("지움: {name}"),
                },
            );
        });
    }

    /// 트리 M: 접속을 다른 폴더로 옮긴다. 빈 값이면 원래 그룹으로 돌아간다(R66.2).
    pub(crate) fn conn_move(&mut self, n: usize) {
        let NodeKind::Conn(c) = self.tree.node(n).kind else { return };
        let (name, folder) = (self.conns[c].name.clone(), self.conns[c].folder.clone());
        self.prompt("옮길 폴더: ", &folder, move |a, f| {
            let f = f.trim().to_string();
            a.call(
                Request::ConnFolder {
                    names: vec![name.clone()],
                    folder: f.clone(),
                },
                Pend::ConnChanged {
                    what: format!("폴더 옮김: {name} → {}", if f.is_empty() { "원래 그룹" } else { &f }),
                },
            );
        });
    }

    pub(crate) fn conn_form_key(&mut self, k: &str) {
        let Some(f) = &mut self.conn_form else { return };
        if f.ask_save {
            f.ask_save = false;
            if k == "y" {
                let (old, c) = (f.old_name.clone(), f.fields());
                self.call(Request::ConnSave { old_name: old, conn: c }, Pend::ConnSaved);
            } else {
                f.msg = "저장하지 않음".into();
            }
            return;
        }
        match k {
            "esc" => self.conn_form = None,
            "down" | "tab" => {
                f.fill_url();
                f.at = (f.at + 1) % LABELS.len();
            }
            "up" | "shift+tab" => f.at = (f.at + LABELS.len() - 1) % LABELS.len(),
            "ctrl+t" | "enter" => {
                if f.fill_url() {
                    return;
                }
                let c = f.fields();
                if c.name.is_empty() || !c.name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
                    f.msg = "접속 이름은 영숫자·-·_ 만 받음".into();
                    return;
                }
                f.msg = "시험하는 중…".into();
                let old = f.old_name.clone();
                // Enter 는 먼저 시험하고 성공하면 저장한다(R59.2)
                self.call(
                    Request::ConnTest {
                        name: String::new(),
                        conn: Some(c),
                        old_name: old,
                    },
                    Pend::ConnTested { save: k == "enter" },
                );
            }
            _ => {
                f.fields[f.at].key(k);
            }
        }
    }

    pub(crate) fn conn_tested(&mut self, save: bool, res: Result<Value, EngineError>) {
        let Some(f) = &mut self.conn_form else { return };
        match res.and_then(decode::<ConnTested>) {
            Ok(t) => {
                f.msg = format!("접속됨 ({})", t.version);
                if !t.warning.is_empty() {
                    f.msg.push_str(&format!(" · {}", t.warning));
                }
                if save {
                    let (old, c) = (f.old_name.clone(), f.fields());
                    self.call(Request::ConnSave { old_name: old, conn: c }, Pend::ConnSaved);
                }
            }
            Err(e) => {
                f.msg = e.error.replace('\n', " ");
                if save && e.reason.as_deref() != Some(idb_engine::REASON_REMOTE_CONN_EDIT) {
                    f.msg.push_str(" · 저장할까요? y/N");
                    f.ask_save = true;
                }
            }
        }
    }

    pub(crate) fn conn_saved(&mut self, res: Result<Value, EngineError>) {
        match res.and_then(decode::<ConnSaved>) {
            Ok(s) => {
                let old = self.conn_form.take().map(|f| f.old_name).unwrap_or_default();
                let mut st = if old.is_empty() {
                    format!("접속 더함: {}", s.conn.name)
                } else {
                    format!("접속 고침: {}", s.conn.name)
                };
                for w in [&s.warning, &s.env_error] {
                    if !w.is_empty() {
                        st.push_str(&format!(" · {w}"));
                    }
                }
                self.reload_project(st, Some(if old.is_empty() { s.conn.name.clone() } else { old }));
            }
            Err(e) => {
                if let Some(f) = &mut self.conn_form {
                    f.msg = e.error.replace('\n', " ");
                }
            }
        }
    }

    pub(crate) fn conn_changed(&mut self, what: String, res: Result<Value, EngineError>) {
        match res {
            Ok(_) => self.reload_project(what, None),
            Err(e) => self.status = e.error,
        }
    }

    /// 접속 목록을 다시 읽어 트리와 탭 접속 자리를 uuid 로 다시 맞춘다. changed 접속의 결과는 막는다(R59.3).
    fn reload_project(&mut self, status: String, changed: Option<String>) {
        self.status = status;
        self.call(Request::Project, Pend::Reload { changed });
    }

    pub(crate) fn reloaded(&mut self, changed: Option<String>, res: Result<Value, EngineError>) {
        let info = match res.and_then(decode::<ProjectInfo>) {
            Ok(i) => i,
            Err(e) => {
                self.status = e.error;
                return;
            }
        };
        let old = std::mem::take(&mut self.conns);
        let map: Vec<Option<usize>> = old
            .iter()
            .map(|c| {
                if changed.as_deref() == Some(c.name.as_str()) {
                    return None;
                }
                info.conns
                    .iter()
                    .position(|n| !c.uuid.is_empty() && n.uuid == c.uuid || c.uuid.is_empty() && n.name == c.name)
            })
            .collect();
        let remap = |i: Option<usize>| i.and_then(|i| map.get(i).copied().flatten());
        for t in &mut self.tabs {
            let keep = t.conn.and_then(|c| info.conns.iter().position(|n| n.uuid == old[c].uuid));
            t.conn = keep;
            t.result_conn = remap(t.result_conn);
        }
        moved(&mut self.catalogs, &map);
        moved(&mut self.in_tx, &map);
        moved(&mut self.locks, &map);
        moved(&mut self.lock_at, &map);
        moved(&mut self.schema_of, &map);
        moved(&mut self.db_over, &map);
        self.manual = self.manual.iter().filter_map(|&c| remap(Some(c))).collect();
        self.enums = std::mem::take(&mut self.enums)
            .into_iter()
            .filter_map(|((c, t), v)| remap(Some(c)).map(|n| ((n, t), v)))
            .collect();
        self.catalog_loading.clear();
        for t in &mut self.tabs {
            t.undo.retain(|u| remap(Some(u.conn)).is_some());
            for u in &mut t.undo {
                u.conn = remap(Some(u.conn)).unwrap();
            }
        }
        self.conns = info.conns;
        let loose = self.files.as_ref().map_or(0, |f| f.loose(&self.conns).len());
        self.tree = crate::tree::Tree::build(&self.conns, &self.tree.closed_groups(), loose);
    }

    pub(crate) fn draw_conn_form(&self, buf: &mut Buffer, r: Rect) {
        let Some(f) = &self.conn_form else { return };
        let w = r.width.min(80);
        let x = r.x + (r.width - w) / 2;
        let h = (LABELS.len() + 5) as u16;
        let y = r.y + r.height.saturating_sub(h) / 2;
        let area = Rect::new(x, y, w, h);
        for yy in area.y..area.y + area.height {
            buf.set_stringn(area.x, yy, " ".repeat(w as usize), w as usize, Style::new().bg(theme::BAR));
        }
        let title = if f.old_name.is_empty() {
            "새 접속".to_string()
        } else {
            format!("접속 고치기 · {}", f.old_name)
        };
        buf.set_stringn(x + 2, y, &title, w as usize - 4, theme::help_title().bg(theme::BAR));
        for (i, l) in LABELS.iter().enumerate() {
            let yy = y + 2 + i as u16;
            let st = if i == f.at {
                theme::help_key().bg(theme::BAR)
            } else {
                theme::hint()
            };
            buf.set_stringn(x + 2, yy, l, 20, st);
            let fx = x + 23;
            let fw = (w as usize).saturating_sub(25);
            if i == PASSWORD {
                let mut dots = "•".repeat(f.fields[i].text.chars().count());
                if dots.is_empty() && f.has_password {
                    dots = "(저장된 비밀번호 그대로)".into();
                }
                let st = Style::new().bg(theme::BAR).fg(theme::TEXT);
                buf.set_stringn(
                    fx,
                    yy,
                    &dots,
                    fw,
                    if i == f.at { st.add_modifier(Modifier::UNDERLINED) } else { st },
                );
            } else {
                f.fields[i].render(buf, fx, yy, fw, Style::new().bg(theme::BAR).fg(theme::TEXT), i == f.at);
            }
        }
        let foot = y + 3 + LABELS.len() as u16;
        let msg_st = if f.msg.starts_with("접속됨") {
            theme::ok().bg(theme::BAR)
        } else {
            theme::hint()
        };
        buf.set_stringn(x + 2, foot, &f.msg, w as usize - 4, msg_st);
        buf.set_stringn(
            x + 2,
            foot + 1,
            self.label("↑↓·Tab 칸 · Ctrl+T 시험 · Enter 시험 뒤 저장 · esc 닫기"),
            w as usize - 4,
            theme::dim().bg(theme::BAR),
        );
    }
}

/// 접속 자리 열쇠를 새 자리로 옮긴다. 없어진(또는 고친) 접속의 값은 버린다.
fn moved<V>(m: &mut std::collections::HashMap<usize, V>, map: &[Option<usize>]) {
    for (k, v) in std::mem::take(m) {
        if let Some(n) = map.get(k).copied().flatten() {
            m.insert(n, v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn URL_을_붙이면_칸을_채우고_비밀번호는_요청에만_싣는다() {
        let mut f = ConnForm::new("app");
        f.fields[0].set("postgres://app_ro:p%40ss@db.stg.local:6543/app?sslmode=require");
        assert!(f.fill_url());
        f.fields[0].set("app-stg2");
        let c = f.fields();
        assert_eq!(
            (c.kind.as_str(), c.host.as_str(), c.port, c.db.as_str(), c.user.as_str()),
            ("postgresql", "db.stg.local", 6543, "app", "app_ro")
        );
        assert_eq!((c.password.as_str(), c.group.as_str()), ("p@ss", "app"));
        let mut h = ConnForm::new("");
        h.fields[2].set("opensearch");
        h.fields[3].set("https://os.local");
        assert_eq!(h.fields().url, "https://os.local");
    }
}
