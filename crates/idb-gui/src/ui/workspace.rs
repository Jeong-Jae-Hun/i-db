//! 창 하나의 작업 공간. 접속 전에는 고르는 화면, 접속 뒤에는 접속 트리·탭·편집기·결과·상태 줄이다.
//! 엔진 요청은 모두 `guarded` 를 거쳐 거절 사유(prod 잠금·확인)를 확인 창으로 바꾸고 다시 보낸다.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::vm::palette;
use gpui::{
    AppContext as _, Context, Entity, FocusHandle, Focusable, InteractiveElement as _, IntoElement, ParentElement as _, PathPromptOptions,
    Render, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use gpui::{ClickEvent, ClipboardItem, MouseButton, rems};
use gpui_component::Disableable as _;
use gpui_component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem};
use gpui_component::notification::Notification;
use gpui_component::tooltip::Tooltip;
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex, h_resizable, resizable_panel, v_flex, v_resizable,
};
use idb_engine::{Catalog, ConnSummary, Engine, EngineError, LockState, ProjectInfo, ResultSet, RunRequest, TablePageRequest};

use super::dashboard::DashboardView;
use super::redis::RedisView;
use super::results::{ResultsView, Source};
use super::source::SourceView;
use super::sql_completion::{CatalogCell, SqlCompletion};
use super::{
    About, CancelRun, CloseTab, ExplainQuery, Minimize, NewTab, OpenFile, OpenSettings, RunAll, RunQuery, SaveFile, ShowDashboard,
    ShowShortcuts, SwitchConn, ToggleProdLock, ToggleResults, ToggleSidebar, TxBegin, TxCommit, TxRollback, ZoomIn, ZoomOut, ZoomReset,
};
use crate::vm::font::FontChoice;
use crate::vm::picker;
use crate::vm::queries::{self, QueryFile};
use crate::vm::runtime::{Canceler, Prefs, Runtime, Target, connect};
use crate::vm::safety::{Ask, Confirms, Gate, Tone, lock_label, tone};
use crate::vm::tabs::{Tabs, target_sql};
use crate::vm::tree::{ConnTree, RowKind};
use crate::vm::zoom;
use gpui_component::ResizableState;
use idb_engine::SessionState;

pub type Make<F> = Rc<dyn Fn(Engine, Confirms) -> F>;
pub type Done<T> = Rc<dyn Fn(&mut Workspace, T, &mut Window, &mut Context<Workspace>)>;

/// `guarded` 의 덧붙임이다. precheck 는 (확인 뒤 다시 보낼 때도) 보내기 바로 전에 돌고, Err 면 보내지 않는다.
/// fail 은 답이 성공이 아니면(오류·거절·멈춤·확인 취소·precheck 실패) 한 번 돈다.
pub type Precheck = Rc<dyn Fn(&Workspace) -> Result<(), String>>;
pub type OnFail = Rc<dyn Fn(&mut Workspace, &mut Window, &mut Context<Workspace>)>;

#[derive(Clone, Default)]
pub struct Hooks {
    pub precheck: Option<Precheck>,
    pub fail: Option<OnFail>,
    /// 멈춤(⌘.)으로 답을 못 받았을 때. 엔진이 이미 했는지 모르는 상태라 확실한 실패(fail)와 따로 둔다.
    /// 없으면 fail 이 돈다
    pub cancelled: Option<OnFail>,
}

impl Hooks {
    fn failed(&self, ws: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        if let Some(f) = &self.fail {
            f(ws, window, cx);
        }
    }

    fn cancelled(&self, ws: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        match &self.cancelled {
            Some(f) => f(ws, window, cx),
            None => self.failed(ws, window, cx),
        }
    }
}

enum Body {
    Sql {
        editor: Entity<EditorState>,
        catalog: CatalogCell,
        result: Option<Entity<ResultsView>>,
    },
    Table {
        result: Option<Entity<ResultsView>>,
    },
    Redis {
        view: Entity<RedisView>,
    },
    Source {
        view: Entity<SourceView>,
    },
    Dashboard {
        view: Entity<DashboardView>,
    },
}

pub struct Workspace {
    pub(super) rt: Rc<Runtime>,
    focus: FocusHandle,
    pub(super) prefs: Prefs,
    /// 설정 파일 자리. 시험은 None 으로 두어 사용자 gui.json 을 건드리지 않는다
    prefs_path: Option<PathBuf>,
    /// 설정·설치 여부로 정한 글꼴(확대 전 크기)
    pub(super) font: FontChoice,
    pub(super) show_sidebar: bool,
    pub(super) show_results: bool,
    main_split: Entity<ResizableState>,
    editor_split: Entity<ResizableState>,
    /// switch_db·set_search_path 뒤의 세션(R27)
    sessions: HashMap<String, SessionState>,
    databases: HashMap<String, Vec<String>>,
    /// 로컬 프로젝트의 `queries/` 파일(R43). 원격이면 None
    queries: Option<Vec<QueryFile>>,
    /// 파일 탭마다 마지막으로 읽거나 쓴 내용(R45.2 자동 저장이 밖의 바뀜을 가른다)
    file_text: HashMap<u64, String>,
    /// 자동 저장을 멈춘 탭과 까닭
    autosave_stopped: HashMap<u64, String>,
    autosave_tasks: HashMap<u64, gpui::Task<()>>,
    /// 데이터베이스 바꾸기 요청이 도는 접속. 원격이면 엔진은 이미 바꿨는데 답이 안 왔을 수 있어
    /// 그동안 그 접속 결과의 고치기·반영·되돌리기·다음 쪽을 막는다
    switching: std::collections::HashSet<String>,
    /// 접속마다 데이터베이스를 바꾼 횟수. 요청은 보낼 때 값을 들고 가고, 답이 올 때 다르면 버린다(R27.1)
    db_gen: HashMap<String, u64>,
    /// 사이드바에서 고른 테이블(두 번 누르면 연다)
    selected_table: Option<(String, String, String)>,
    // ─── 접속 전 ───
    dir_input: Entity<InputState>,
    remotes: Result<Vec<String>, String>,
    connecting: bool,
    // ─── 접속 뒤 ───
    engine: Option<Engine>,
    target: Option<Target>,
    tree: ConnTree,
    tree_filter: Entity<InputState>,
    tabs: Tabs,
    bodies: HashMap<u64, Body>,
    catalogs: HashMap<String, Rc<Catalog>>,
    tx: HashMap<String, bool>,
    locks: HashMap<String, (LockState, Instant)>,
    params: BTreeMap<String, String>,
    status: (String, bool),
    busy: Option<(String, Canceler)>,
    _subs: Vec<Subscription>,
}

impl Workspace {
    pub fn new(rt: Rc<Runtime>, font: FontChoice, window: &mut Window, cx: &mut Context<Self>) -> Workspace {
        let prefs = Prefs::path().map(|p| Prefs::load(&p)).unwrap_or_default();
        let default_dir = std::env::var("DBTUI_PROJECT").ok().filter(|d| !d.is_empty()).unwrap_or_else(|| {
            prefs
                .recent
                .iter()
                .find_map(|t| match t {
                    Target::Local(p) => Some(p.display().to_string()),
                    _ => None,
                })
                .unwrap_or_default()
        });
        let dir_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(picker::DIR_PLACEHOLDER)
                .default_value(default_dir)
        });
        let tree_filter = cx.new(|cx| InputState::new(window, cx).placeholder("접속·테이블 찾기"));
        let main_split = cx.new(|_| ResizableState::default());
        let editor_split = cx.new(|_| ResizableState::default());
        let subs = vec![
            // 끌어 바꾼 칸 크기를 기억한다
            cx.subscribe(&main_split, |this, st, _: &gpui_component::ResizablePanelEvent, cx| {
                if let Some(w) = st.read(cx).sizes().first() {
                    this.prefs.sidebar_width = f32::from(*w);
                    this.save_prefs();
                }
            }),
            cx.subscribe(&editor_split, |this, st, _: &gpui_component::ResizablePanelEvent, cx| {
                if let Some(h) = st.read(cx).sizes().first() {
                    this.prefs.editor_height = f32::from(*h);
                    this.save_prefs();
                }
            }),
            // 시스템 테마를 따르면 macOS 밝기 바꿈을 따라간다
            cx.observe_window_appearance(window, |this, window, cx| {
                if this.prefs.theme == crate::vm::settings::ThemePref::System {
                    super::apply_look(&this.prefs, &this.font, window, cx);
                }
            }),
            cx.subscribe_in(&tree_filter, window, |this, input, ev: &InputEvent, _, cx| {
                if let InputEvent::Change = ev {
                    let f = input.read(cx).value().to_string();
                    this.tree.set_filter(&f);
                    cx.notify();
                }
            }),
            cx.subscribe_in(&dir_input, window, |this, input, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    let dir = expand_home(&input.read(cx).value());
                    this.open_target(Target::Local(dir), window, cx);
                }
            }),
        ];
        // 잠금이 열려 있으면 남은 시간을 1초마다 다시 그린다
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let Ok(()) = this.update(cx, |ws, cx| {
                    if ws.locks.values().any(|(l, _)| l.prod && !l.locked) {
                        cx.notify();
                    }
                }) else {
                    break;
                };
            }
        })
        .detach();
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        Workspace {
            rt,
            focus,
            prefs,
            prefs_path: Prefs::path(),
            font,
            show_sidebar: true,
            show_results: true,
            main_split,
            editor_split,
            sessions: HashMap::new(),
            databases: HashMap::new(),
            db_gen: HashMap::new(),
            switching: Default::default(),
            queries: None,
            file_text: HashMap::new(),
            autosave_stopped: HashMap::new(),
            autosave_tasks: HashMap::new(),
            selected_table: None,
            dir_input,
            remotes: crate::vm::runtime::remote_names(),
            connecting: false,
            engine: None,
            target: None,
            tree: ConnTree::default(),
            tree_filter,
            tabs: Tabs::default(),
            bodies: HashMap::new(),
            catalogs: HashMap::new(),
            tx: HashMap::new(),
            locks: HashMap::new(),
            params: BTreeMap::new(),
            status: (String::new(), false),
            busy: None,
            _subs: subs,
        }
    }

    pub(super) fn save_prefs(&self) {
        if let Some(p) = &self.prefs_path {
            let _ = self.prefs.save(p);
        }
    }

    // ─── 확대·보기 ───

    fn set_zoom(&mut self, z: f32, window: &mut Window, cx: &mut Context<Self>) {
        self.prefs.zoom = zoom::normalize(z);
        self.save_prefs();
        super::apply_look(&self.prefs, &self.font, window, cx);
        self.set_status(format!("확대 {}", zoom::label(self.prefs.zoom)), false, cx);
    }

    /// 설정 창에서 바꾼 값을 반영한다.
    pub(super) fn apply_settings(&mut self, prefs: Prefs, window: &mut Window, cx: &mut Context<Self>) {
        let font_changed = prefs.font != self.prefs.font || prefs.font_size != self.prefs.font_size;
        self.prefs = prefs;
        if font_changed {
            self.font = super::load_font(&self.prefs, cx);
        }
        self.save_prefs();
        super::apply_look(&self.prefs, &self.font, window, cx);
        self.set_status("설정을 저장함", false, cx);
    }

    pub fn set_status(&mut self, msg: impl Into<String>, error: bool, cx: &mut Context<Self>) {
        self.status = (msg.into(), error);
        if std::env::var_os("IDB_GUI_TRACE").is_some() {
            eprintln!("[{}] {}", if error { "오류" } else { "상태" }, self.status.0);
        }
        cx.notify();
    }

    // ─── 엔진 요청 ───

    /// 요청을 tokio 에서 돌리고 끝나면 done 을 부른다. 거절이면 사유대로 확인 창을 띄우고 확인하면 표시를 붙여 다시 보낸다.
    pub fn guarded<T, F>(
        &mut self,
        label: &str,
        make: Make<F>,
        done: Done<T>,
        confirms: Confirms,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) where
        T: Send + 'static,
        F: Future<Output = Result<T, EngineError>> + Send + 'static,
    {
        self.guarded_with(label, make, done, Hooks::default(), confirms, window, cx)
    }

    // 창(window)·cx 두 자리가 늘 붙는 GPUI 뷰 함수라 묶음 구조체를 따로 두지 않는다
    #[allow(clippy::too_many_arguments)]
    pub fn guarded_with<T, F>(
        &mut self,
        label: &str,
        make: Make<F>,
        done: Done<T>,
        hooks: Hooks,
        confirms: Confirms,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) where
        T: Send + 'static,
        F: Future<Output = Result<T, EngineError>> + Send + 'static,
    {
        if let Some(Err(msg)) = hooks.precheck.as_ref().map(|p| p(self)) {
            self.set_status(format!("{label}: {msg}"), true, cx);
            hooks.failed(self, window, cx);
            return;
        }
        let Some(engine) = self.engine.clone() else {
            hooks.failed(self, window, cx);
            return;
        };
        let job = self.rt.spawn(make(engine, confirms));
        self.busy = Some((label.to_string(), job.canceler()));
        self.status = (format!("{label} 중…"), false);
        cx.notify();
        let label = label.to_string();
        cx.spawn_in(window, async move |this, cx| {
            let r = job.rx.await;
            let _ = this.update_in(cx, move |ws, window, cx| {
                ws.busy = None;
                match r {
                    Err(_) => {
                        ws.set_status(format!("{label}: 멈춤"), true, cx);
                        hooks.cancelled(ws, window, cx);
                    }
                    Ok(Ok(v)) => {
                        ws.status = (String::new(), false);
                        done(ws, v, window, cx)
                    }
                    Ok(Err(e)) => ws.refused(&label, e, make, done, hooks, confirms, window, cx),
                }
                cx.notify();
            });
        })
        .detach();
    }

    // 창(window)·cx 두 자리가 늘 붙는 GPUI 뷰 함수라 묶음 구조체를 따로 두지 않는다
    #[allow(clippy::too_many_arguments)]
    fn refused<T, F>(
        &mut self,
        label: &str,
        e: EngineError,
        make: Make<F>,
        done: Done<T>,
        hooks: Hooks,
        confirms: Confirms,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) where
        T: Send + 'static,
        F: Future<Output = Result<T, EngineError>> + Send + 'static,
    {
        match confirms.gate(&e) {
            Gate::Fail(msg) => {
                if e.reason.as_deref() == Some("in_tx") {
                    window.push_notification(Notification::warning("트랜잭션이 열려 있다. 커밋하거나 되돌려야 한다"), cx);
                }
                self.set_status(format!("{label}: {msg}"), true, cx);
                hooks.failed(self, window, cx);
            }
            Gate::Locked(msg) => {
                self.set_status(format!("{msg} (⌘L 로 prod 잠금을 연다)"), true, cx);
                window.push_notification(Notification::warning("prod 가 잠겨 있다. ⌘L 로 잠시 연다"), cx);
                hooks.failed(self, window, cx);
            }
            Gate::Ask { ask, question } => {
                let this = cx.entity().downgrade();
                let label = label.to_string();
                window.open_alert_dialog(cx, move |alert, _, _| {
                    let (this, make, done, label, hooks) = (this.clone(), make.clone(), done.clone(), label.clone(), hooks.clone());
                    let (this_c, hooks_c) = (this.clone(), hooks.clone());
                    alert
                        .title(ask.title())
                        .description(question.clone())
                        .ok_text(ask.ok_text())
                        .ok_variant(if ask == Ask::Always {
                            ButtonVariant::Primary
                        } else {
                            ButtonVariant::Danger
                        })
                        .show_cancel(true)
                        .on_ok(move |_, window, cx| {
                            let mut c = confirms;
                            c.grant(ask);
                            let _ = this.update(cx, |ws, cx| {
                                ws.guarded_with(&label, make.clone(), done.clone(), hooks.clone(), c, window, cx)
                            });
                            true
                        })
                        .on_cancel(move |_, window, cx| {
                            let _ = this_c.update(cx, |ws, cx| hooks_c.failed(ws, window, cx));
                            true
                        })
                });
            }
        }
    }

    // ─── 접속 ───

    pub fn open_target(&mut self, target: Target, window: &mut Window, cx: &mut Context<Self>) {
        if self.connecting {
            return;
        }
        self.connecting = true;
        self.set_status(format!("{} 여는 중…", target.label()), false, cx);
        let t = target.clone();
        let job = self.rt.spawn(async move {
            let e = connect(&t).await?;
            let p = e.project().await?;
            Ok::<_, EngineError>((e, p))
        });
        cx.spawn_in(window, async move |this, cx| {
            let r = job.rx.await;
            let _ = this.update_in(cx, move |ws, window, cx| {
                ws.connecting = false;
                match r {
                    Ok(Ok((engine, info))) => ws.connected(target, engine, info, window, cx),
                    Ok(Err(e)) => ws.set_status(e.error, true, cx),
                    Err(_) => ws.set_status("멈춤", true, cx),
                }
            });
        })
        .detach();
    }

    fn connected(&mut self, target: Target, engine: Engine, info: ProjectInfo, window: &mut Window, cx: &mut Context<Self>) {
        self.prefs.touch(target.clone());
        self.save_prefs();
        let warn = info.warnings.join("; ");
        self.tree = ConnTree::new(info.conns);
        self.target = Some(target.clone());
        self.refresh_queries();
        self.engine = Some(engine);
        self.target = Some(target);
        self.new_tab(None, window, cx);
        self.set_status(
            if warn.is_empty() {
                "접속 목록을 읽음".into()
            } else {
                format!("경고: {warn}")
            },
            !warn.is_empty(),
            cx,
        );
    }

    fn disconnect(&mut self, cx: &mut Context<Self>) {
        if let Some(e) = self.engine.take() {
            self.rt.spawn(async move { e.close().await });
        }
        self.target = None;
        self.tabs = Tabs::default();
        self.bodies.clear();
        self.catalogs.clear();
        self.tx.clear();
        self.locks.clear();
        self.sessions.clear();
        self.databases.clear();
        self.db_gen.clear();
        self.switching.clear();
        self.queries = None;
        self.file_text.clear();
        self.autosave_stopped.clear();
        self.autosave_tasks.clear();
        self.selected_table = None;
        cx.notify();
    }

    fn conn(&self, name: &str) -> Option<&ConnSummary> {
        self.tree.conns.iter().find(|c| c.name == name)
    }

    fn active_conn(&self) -> Option<String> {
        self.tabs.active().and_then(|t| t.conn.clone())
    }

    /// 트리에서 접속을 누르면 지금 탭의 접속으로 삼는다. Redis 면 Redis 탭을 연다.
    fn select_conn(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(c) = self.conn(&name).cloned() else { return };
        match c.kind.as_str() {
            "postgresql" => {
                let reuse = self
                    .tabs
                    .active()
                    .is_some_and(|t| matches!(self.bodies.get(&t.id), Some(Body::Sql { .. })));
                if !reuse {
                    self.new_tab(Some(name.clone()), window, cx);
                } else if let Some(t) = self.tabs.active_mut() {
                    t.conn = Some(name.clone());
                }
                self.refresh_conn_state(&name, window, cx);
                self.ensure_catalog(&name, window, cx);
                self.sync_catalog_cells();
            }
            "redis" => {
                let id = self.tabs.open_named(Some(name.clone()), name.clone());
                let (rt, engine, ws) = (self.rt.clone(), self.engine.clone(), cx.entity().downgrade());
                let Some(engine) = engine else { return };
                let view = cx.new(|cx| RedisView::new(ws, rt, engine, name.clone(), window, cx));
                self.bodies.insert(id, Body::Redis { view });
                self.refresh_conn_state(&name, window, cx);
            }
            k => {
                // 읽기 전용 저장소: 이미 열린 탭이 있으면 그리로 간다
                if let Some(i) =
                    self.tabs.tabs.iter().position(|t| {
                        t.conn.as_deref() == Some(name.as_str()) && matches!(self.bodies.get(&t.id), Some(Body::Source { .. }))
                    })
                {
                    self.tabs.activate(i);
                } else {
                    let id = self.tabs.open_named(Some(name.clone()), name.clone());
                    let (rt, ws) = (self.rt.clone(), cx.entity().downgrade());
                    let Some(engine) = self.engine.clone() else { return };
                    let kind = k.to_string();
                    let view = cx.new(|cx| SourceView::new(ws, rt, engine, name.clone(), kind, window, cx));
                    self.bodies.insert(id, Body::Source { view });
                }
            }
        }
        cx.notify();
    }

    /// 트랜잭션·잠금 상태를 읽는다.
    pub fn refresh_conn_state(&mut self, conn: &str, window: &mut Window, cx: &mut Context<Self>) {
        // 전환을 멈춘 뒤 확인하지 못한 접속이면 다시 확인한다
        if self.is_switching(conn) && self.busy.is_none() {
            self.resync_db(conn, window, cx);
        }
        let name = conn.to_string();
        let kind_pg = self.conn(conn).is_some_and(|c| c.kind == "postgresql");
        let n = name.clone();
        self.guarded(
            "잠금 상태",
            Rc::new(move |e: Engine, _| {
                let n = n.clone();
                async move { e.prod_lock(&n, None).await }
            }),
            Rc::new(move |ws: &mut Workspace, l: LockState, _, cx| {
                ws.locks.insert(l.conn.clone(), (l, Instant::now()));
                cx.notify();
            }),
            Confirms::default(),
            window,
            cx,
        );
        if kind_pg {
            let n = name.clone();
            self.guarded(
                "트랜잭션 상태",
                Rc::new(move |e: Engine, _| {
                    let n = n.clone();
                    async move { e.tx_state(&n).await }
                }),
                Rc::new(move |ws: &mut Workspace, s: idb_engine::TxState, _, cx| {
                    ws.tx.insert(name.clone(), s.in_tx);
                    cx.notify();
                }),
                Confirms::default(),
                window,
                cx,
            );
            let (n, name) = (conn.to_string(), conn.to_string());
            self.guarded(
                "데이터베이스 목록",
                Rc::new(move |e: Engine, _| {
                    let n = n.clone();
                    async move { e.databases(&n).await }
                }),
                Rc::new(move |ws: &mut Workspace, dbs: Vec<String>, _, cx| {
                    ws.databases.insert(name.clone(), dbs);
                    cx.notify();
                }),
                Confirms::default(),
                window,
                cx,
            );
        }
    }

    // ─── 데이터베이스·스키마 ───

    pub(super) fn engine(&self) -> Option<&Engine> {
        self.engine.as_ref()
    }

    pub(super) fn is_switching(&self, conn: &str) -> bool {
        self.switching.contains(conn)
    }

    /// 결과로 쓰기(반영·되돌리기)를 보내도 되는가. 결과를 만든 세대와 지금 세대가 같고 바꾸는 중이 아니어야 한다.
    pub(super) fn write_ok(&self, conn: &str, gen0: u64) -> Result<(), String> {
        if self.is_switching(conn) {
            return Err("데이터베이스를 바꾸는 중이라 보내지 않는다".into());
        }
        if self.db_gen(conn) != gen0 {
            return Err("데이터베이스를 바꾸기 전 결과라 보내지 않는다 (다시 실행한다)".into());
        }
        Ok(())
    }

    fn set_frozen(&self, conn: &str, frozen: bool, cx: &mut Context<Self>) {
        for r in self.results_for(conn, cx) {
            r.update(cx, |v, cx| v.set_frozen(frozen, cx));
        }
    }

    pub(super) fn db_gen(&self, conn: &str) -> u64 {
        self.db_gen.get(conn).copied().unwrap_or(0)
    }

    /// 그 접속에서 만든 결과 화면들이다.
    fn results_for(&self, conn: &str, cx: &gpui::App) -> Vec<Entity<ResultsView>> {
        self.bodies
            .values()
            .filter_map(|b| match b {
                Body::Sql { result: Some(r), .. } | Body::Table { result: Some(r) } => Some(r.clone()),
                _ => None,
            })
            .filter(|r| r.read(cx).conn == conn)
            .collect()
    }

    /// 데이터베이스를 바꾸기 전에 묻는다. 반영 안 한 바꿈이 남은 채 바꾸면 그 바꿈(같은 테이블·PK·옛값)이
    /// 새 데이터베이스에 반영될 수 있어, 버릴지 먼저 묻는다. 요청이 도는 중에는 바꾸지 않는다.
    pub(super) fn request_switch_db(&mut self, conn: String, db: String, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((label, _)) = &self.busy {
            self.set_status(format!("{label} 이 끝난 뒤 데이터베이스를 바꾼다"), true, cx);
            return;
        }
        let pending: usize = self.results_for(&conn, cx).iter().map(|r| r.read(cx).pending(cx)).sum();
        if pending == 0 {
            self.switch_db(conn, db, window, cx);
            return;
        }
        let this = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (this, conn, db) = (this.clone(), conn.clone(), db.clone());
            alert
                .title("반영하지 않은 바꿈")
                .description(format!("{conn} 에 반영하지 않은 바꿈 {pending}건이 있다. 버리고 {db} 로 바꿀까?"))
                .ok_text("버리고 바꾸기")
                .ok_variant(ButtonVariant::Danger)
                .show_cancel(true)
                .on_ok(move |_, window, cx| {
                    let _ = this.update(cx, |ws, cx| {
                        for r in ws.results_for(&conn, cx) {
                            r.update(cx, |v, cx| v.discard(cx));
                        }
                        ws.switch_db(conn.clone(), db.clone(), window, cx)
                    });
                    true
                })
        });
    }

    /// 바꾼 뒤: 세대를 올리고 그 접속의 결과는 고치기·되돌리기·다음 쪽을 막는다.
    pub(super) fn on_db_switched(&mut self, conn: &str, cx: &mut Context<Self>) {
        *self.db_gen.entry(conn.to_string()).or_default() += 1;
        for r in self.results_for(conn, cx) {
            r.update(cx, |v, cx| v.invalidate(cx));
        }
    }

    /// R27.1: 같은 서버의 다른 데이터베이스로 세션을 옮긴다. 카탈로그는 다시 읽는다.
    fn switch_db(&mut self, conn: String, db: String, window: &mut Window, cx: &mut Context<Self>) {
        let (c2, d2) = (conn.clone(), db.clone());
        // 보내기 전에 얼린다. 실패하면 풀고, 성공하면 옛 결과로 무효화한다
        self.switching.insert(conn.clone());
        self.set_frozen(&conn, true, cx);
        let (c_fail, c_cancel) = (conn.clone(), conn.clone());
        let hooks = Hooks {
            precheck: None,
            // 엔진이 오류·거절로 답했다: 바뀌지 않은 것이 확실해 푼다
            fail: Some(Rc::new(move |ws: &mut Workspace, _: &mut Window, cx: &mut Context<Workspace>| {
                ws.switching.remove(&c_fail);
                ws.set_frozen(&c_fail, false, cx);
            })),
            // 멈춤: 원격은 이미 바꿨을 수 있다(cancel 은 되돌리지 않는다)
            cancelled: Some(Rc::new(
                move |ws: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>| ws.on_switch_cancelled(&c_cancel, window, cx),
            )),
        };
        self.guarded_with(
            "데이터베이스 바꾸기",
            Rc::new(move |e: Engine, _| {
                let (c, d) = (c2.clone(), d2.clone());
                async move { e.switch_db(&c, &d).await }
            }),
            Rc::new(move |ws: &mut Workspace, st: SessionState, window, cx| {
                ws.set_status(format!("{} 로 바꿈", st.db), false, cx);
                ws.sessions.insert(conn.clone(), st);
                ws.switching.remove(&conn);
                ws.on_db_switched(&conn, cx);
                ws.catalogs.remove(&conn);
                ws.ensure_catalog(&conn, window, cx);
            }),
            hooks,
            Confirms::default(),
            window,
            cx,
        );
    }

    /// 바꾸기를 멈췄다. 바뀌었는지 모르므로 옛 결과는 무효로 하고(세대 올림) 얼린 채,
    /// 엔진에 그 접속의 지금 데이터베이스를 물어 확인되면 푼다. 확인 못 하면 쓰기를 계속 막는다.
    pub(super) fn on_switch_cancelled(&mut self, conn: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.on_db_switched(conn, cx);
        self.switching.insert(conn.to_string());
        self.resync_db(conn, window, cx);
    }

    /// 엔진(serve)이 아는 그 접속의 데이터베이스를 읽는다(project 의 db 는 switch_db 를 반영한다).
    fn resync_db(&mut self, conn: &str, window: &mut Window, cx: &mut Context<Self>) {
        let (c_done, c_fail, c_cancel) = (conn.to_string(), conn.to_string(), conn.to_string());
        let hooks = Hooks {
            precheck: None,
            fail: Some(Rc::new(move |ws: &mut Workspace, _: &mut Window, cx: &mut Context<Workspace>| {
                ws.db_unconfirmed(&c_fail, cx)
            })),
            cancelled: Some(Rc::new(move |ws: &mut Workspace, _: &mut Window, cx: &mut Context<Workspace>| {
                ws.db_unconfirmed(&c_cancel, cx)
            })),
        };
        self.guarded_with(
            "데이터베이스 확인",
            Rc::new(|e: Engine, _| async move { e.project().await }),
            Rc::new(move |ws: &mut Workspace, info: ProjectInfo, window, cx| {
                let Some(found) = info.conns.into_iter().find(|c| c.name == c_done) else {
                    ws.db_unconfirmed(&c_done, cx);
                    return;
                };
                ws.db_confirmed(&c_done, found.db, window, cx);
            }),
            hooks,
            Confirms::default(),
            window,
            cx,
        );
    }

    /// 지금 데이터베이스를 알았다. 옛 결과는 이미 무효라 새로 실행한 결과부터 쓸 수 있다.
    pub(super) fn db_confirmed(&mut self, conn: &str, db: String, window: &mut Window, cx: &mut Context<Self>) {
        self.switching.remove(conn);
        if let Some(c) = self.tree.conns.iter_mut().find(|c| c.name == conn) {
            c.db = db.clone();
        }
        let search_path = self.sessions.get(conn).map(|s| s.search_path.clone()).unwrap_or_default();
        let in_tx = self.tx.get(conn).copied().unwrap_or(false);
        self.sessions.insert(
            conn.to_string(),
            SessionState {
                conn: conn.to_string(),
                db: db.clone(),
                search_path,
                in_tx,
            },
        );
        self.catalogs.remove(conn);
        self.ensure_catalog(conn, window, cx);
        self.set_status(
            format!("데이터베이스 전환을 멈췄다. 지금 {db} 이다 — 결과는 다시 실행한다"),
            true,
            cx,
        );
    }

    /// 확인하지 못했다. 쓰기는 계속 막는다(접속을 다시 누르면 또 확인한다).
    pub(super) fn db_unconfirmed(&mut self, conn: &str, cx: &mut Context<Self>) {
        self.switching.insert(conn.to_string());
        self.set_frozen(conn, true, cx);
        self.set_status("데이터베이스 전환 결과를 확인하지 못해 결과를 다시 실행한다", true, cx);
    }

    /// R27.2: 기본 스키마(search_path)를 바꾼다.
    fn set_schema(&mut self, conn: String, schema: String, window: &mut Window, cx: &mut Context<Self>) {
        let (c2, s2) = (conn.clone(), schema.clone());
        self.guarded(
            "스키마 바꾸기",
            Rc::new(move |e: Engine, _| {
                let (c, sc) = (c2.clone(), s2.clone());
                async move { e.set_search_path(&c, &sc).await }
            }),
            Rc::new(move |ws: &mut Workspace, st: SessionState, _, cx| {
                ws.set_status(format!("search_path = {}", st.search_path), false, cx);
                ws.sessions.insert(conn.clone(), st);
            }),
            Confirms::default(),
            window,
            cx,
        );
    }

    fn refresh_catalog(&mut self, conn: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.catalogs.remove(conn);
        self.ensure_catalog(conn, window, cx);
    }

    /// 테이블 DDL 을 새 SQL 탭에 연다.
    fn show_ddl(&mut self, conn: String, schema: String, table: String, window: &mut Window, cx: &mut Context<Self>) {
        let (c2, s2, t2) = (conn.clone(), schema.clone(), table.clone());
        self.guarded(
            "DDL",
            Rc::new(move |e: Engine, _| {
                let (c, sc, t) = (c2.clone(), s2.clone(), t2.clone());
                async move { e.ddl(&c, &sc, &t).await }
            }),
            Rc::new(move |ws: &mut Workspace, ddl: String, window, cx| {
                let id = ws.tabs.open_named(Some(conn.clone()), format!("{schema}.{table} DDL"));
                ws.insert_sql_body(id, ddl, window, cx);
                ws.sync_catalog_cells();
                cx.notify();
            }),
            Confirms::default(),
            window,
            cx,
        );
    }

    /// 편집기 문장을 차례로 실행한다. 거절·오류에서 멈추고 마지막 결과를 보인다.
    fn run_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(t) = self.tabs.active() else { return };
        let Some(conn) = t.conn.clone() else {
            self.set_status("왼쪽에서 접속을 먼저 고른다", true, cx);
            return;
        };
        let Some(Body::Sql { editor, .. }) = self.bodies.get(&t.id) else {
            return;
        };
        let text = editor.read(cx).value().to_string();
        let stmts: Vec<String> = idb_core::sqlsafe::split_statements(&text).into_iter().map(String::from).collect();
        if stmts.is_empty() {
            self.set_status("실행할 문장이 없다", true, cx);
            return;
        }
        if stmts.iter().any(|s| !idb_core::params::bind(s).1.is_empty()) {
            self.set_status("자리표시가 있는 문장은 ⌘↵ 로 하나씩 실행한다", true, cx);
            return;
        }
        let (tab_id, gen0) = (t.id, self.db_gen(&conn));
        self.run_chain(tab_id, conn, gen0, std::collections::VecDeque::from(stmts), 0, window, cx);
    }

    /// 시작한 탭이 그대로이고 그 탭의 접속·데이터베이스가 시작할 때와 같은가. 아니면 사슬을 멈춘다.
    fn chain_alive(&self, tab_id: u64, conn: &str, gen0: u64) -> bool {
        self.tabs
            .index_of(tab_id)
            .is_some_and(|i| self.tabs.tabs[i].conn.as_deref() == Some(conn))
            && self.db_gen(conn) == gen0
    }

    // 창(window)·cx 두 자리가 늘 붙는 GPUI 뷰 함수라 묶음 구조체를 따로 두지 않는다
    #[allow(clippy::too_many_arguments)]
    fn run_chain(
        &mut self,
        tab_id: u64,
        conn: String,
        gen0: u64,
        mut rest: std::collections::VecDeque<String>,
        done_n: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.chain_alive(tab_id, &conn, gen0) {
            self.set_status(
                format!("모두 실행 멈춤: 탭을 닫았거나 접속·데이터베이스가 바뀌었다 ({done_n}문장 실행함)"),
                true,
                cx,
            );
            return;
        }
        let Some(sql) = rest.pop_front() else { return };
        let (c2, s2) = (conn.clone(), sql.clone());
        let total = done_n + 1 + rest.len();
        self.guarded(
            &format!("모두 실행 {}/{total}", done_n + 1),
            Rc::new(move |e: Engine, c: Confirms| {
                let req = RunRequest {
                    conn: c2.clone(),
                    sql: s2.clone(),
                    params: BTreeMap::new(),
                    confirm_prod: c.prod,
                    confirm_danger: c.danger,
                    confirm_analyze: c.analyze,
                };
                async move { e.run(req).await }
            }),
            Rc::new(move |ws: &mut Workspace, rs: ResultSet, window, cx| {
                if let Some(in_tx) = rs.in_tx {
                    ws.tx.insert(conn.clone(), in_tx);
                }
                if rest.is_empty() {
                    ws.set_status(
                        format!("{total}문장 실행 · 마지막 {}행 · {}ms", rs.rows.len(), rs.elapsed_ms),
                        false,
                        cx,
                    );
                    if ws.chain_alive(tab_id, &conn, gen0) {
                        ws.show_result(tab_id, conn.clone(), Source::Query { sql: sql.clone() }, rs, false, window, cx);
                    }
                } else {
                    ws.run_chain(tab_id, conn.clone(), gen0, rest.clone(), done_n + 1, window, cx);
                }
            }),
            Confirms::default(),
            window,
            cx,
        );
    }

    fn ensure_catalog(&mut self, conn: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.catalogs.contains_key(conn) {
            return;
        }
        let n = conn.to_string();
        let name = n.clone();
        let gen0 = self.db_gen(conn);
        self.guarded(
            "카탈로그",
            Rc::new(move |e: Engine, _| {
                let n = n.clone();
                async move { e.catalog(&n).await }
            }),
            Rc::new(move |ws: &mut Workspace, cat: Catalog, _, cx| {
                // 바꾸기 전 데이터베이스의 카탈로그면 버린다(바꾼 뒤 다시 읽는다)
                if ws.db_gen(&name) != gen0 {
                    return;
                }
                ws.catalogs.insert(name.clone(), Rc::new(cat));
                ws.sync_catalog_cells();
                cx.notify();
            }),
            Confirms::default(),
            window,
            cx,
        );
    }

    fn sync_catalog_cells(&self) {
        for t in &self.tabs.tabs {
            if let Some(Body::Sql { catalog, .. }) = self.bodies.get(&t.id) {
                *catalog.borrow_mut() = t.conn.as_ref().and_then(|c| self.catalogs.get(c).cloned());
            }
        }
    }

    // ─── 탭 ───

    fn new_tab(&mut self, conn: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let conn = conn.or_else(|| self.active_conn());
        let id = self.tabs.open(conn, None);
        self.insert_sql_body(id, String::new(), window, cx);
        self.sync_catalog_cells();
        cx.notify();
    }

    fn insert_sql_body(&mut self, id: u64, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let catalog: CatalogCell = Default::default();
        let cell = catalog.clone();
        let editor = cx.new(|cx| {
            let mut e = EditorState::new(window, cx).language("sql").default_value(text);
            e.lsp_mut().completion_provider = Some(Rc::new(SqlCompletion { catalog: cell }));
            e
        });
        let sub = cx.subscribe_in(&editor, window, move |this, _, ev: &InputEvent, _, cx| {
            if let InputEvent::Change = ev
                && let Some(i) = this.tabs.index_of(id)
            {
                if !this.tabs.tabs[i].dirty {
                    this.tabs.tabs[i].dirty = true;
                    cx.notify();
                }
                this.schedule_autosave(id, cx);
            }
        });
        self._subs.push(sub);
        editor.update(cx, |e, cx| e.focus(window, cx));
        self.bodies.insert(
            id,
            Body::Sql {
                editor,
                catalog,
                result: None,
            },
        );
    }

    fn close_tab(&mut self, i: usize, cx: &mut Context<Self>) {
        // 닫기 전에 자동 저장한다(R45.2)
        if let Some(id) = self.tabs.tabs.get(i).map(|t| t.id) {
            self.autosave(id, cx);
        }
        if let Some(t) = self.tabs.close(i) {
            self.bodies.remove(&t.id);
            self.file_text.remove(&t.id);
            self.autosave_stopped.remove(&t.id);
            self.autosave_tasks.remove(&t.id);
        }
        cx.notify();
    }

    /// 대시보드 탭을 하나만 연다(R64.1).
    pub(super) fn open_dashboard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(i) = self
            .tabs
            .tabs
            .iter()
            .position(|t| matches!(self.bodies.get(&t.id), Some(Body::Dashboard { .. })))
        {
            self.tabs.activate(i);
            cx.notify();
            return;
        }
        let Some(engine) = self.engine.clone() else { return };
        let id = self.tabs.open_named(None, "대시보드".into());
        let (rt, conns) = (self.rt.clone(), self.tree.conns.clone());
        let view = cx.new(|cx| DashboardView::new(rt, engine, conns, window, cx));
        self.bodies.insert(id, Body::Dashboard { view });
        cx.notify();
    }

    // ─── 쿼리 파일(R43·R45) ───

    fn project_dir(&self) -> Option<PathBuf> {
        match &self.target {
            Some(Target::Local(d)) => Some(std::path::absolute(d).unwrap_or_else(|_| d.clone())),
            _ => None,
        }
    }

    /// 로컬 프로젝트의 queries/ 를 다시 훑는다. 매핑은 프로젝트를 다시 읽어 얻는다.
    pub(super) fn refresh_queries(&mut self) {
        self.queries = self.project_dir().map(|dir| {
            let mapping = idb_core::project::load_project(&dir).map(|p| p.mapping).unwrap_or_default();
            queries::scan(&dir, &mapping, &self.tree.conns)
        });
    }

    /// 쿼리 파일을 연다. 이미 열린 탭이면 그 탭으로 간다. 붙은 접속이 있으면 그 접속으로 연다.
    pub(super) fn open_query(&mut self, rel: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dir) = self.project_dir() else { return };
        let path = dir.join(&rel);
        if let Some(i) = self.tabs.tabs.iter().position(|t| t.path.as_deref() == Some(path.as_path())) {
            self.tabs.activate(i);
            cx.notify();
            return;
        }
        let conn = self
            .queries
            .as_ref()
            .and_then(|q| q.iter().find(|f| f.rel == rel))
            .and_then(|f| f.conn.clone());
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let id = self.tabs.open(conn.clone(), Some(path));
                self.file_text.insert(id, text.clone());
                self.insert_sql_body(id, text, window, cx);
                if let Some(c) = conn {
                    self.ensure_catalog(&c, window, cx);
                }
                self.sync_catalog_cells();
                cx.notify();
            }
            Err(e) => self.set_status(format!("열기 실패: {e}"), true, cx),
        }
    }

    fn schedule_autosave(&mut self, id: u64, cx: &mut Context<Self>) {
        let has_file = self.tabs.index_of(id).is_some_and(|i| self.tabs.tabs[i].path.is_some());
        if !has_file || self.autosave_stopped.contains_key(&id) || !self.file_text.contains_key(&id) {
            return;
        }
        // 마지막 고침 1초 뒤(R45.2). 새 고침이 오면 앞 작업을 버린다
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            let _ = this.update(cx, |ws, cx| ws.autosave(id, cx));
        });
        self.autosave_tasks.insert(id, task);
    }

    /// 파일 탭을 저장한다. 밖에서 바뀌었으면 멈추고 알린다(내용으로 가른다).
    pub(super) fn autosave(&mut self, id: u64, cx: &mut Context<Self>) {
        self.autosave_tasks.remove(&id);
        if self.autosave_stopped.contains_key(&id) {
            return;
        }
        let Some(i) = self.tabs.index_of(id) else { return };
        let Some(path) = self.tabs.tabs[i].path.clone() else { return };
        let Some(last) = self.file_text.get(&id).cloned() else { return };
        let Some(Body::Sql { editor, .. }) = self.bodies.get(&id) else {
            return;
        };
        let text = editor.read(cx).value().to_string();
        let disk = std::fs::read_to_string(&path).ok();
        match queries::before_autosave(disk.as_deref(), &last, &text) {
            queries::SaveDecision::Skip => {
                self.file_text.insert(id, text);
                self.tabs.tabs[i].dirty = false;
            }
            queries::SaveDecision::Write => match queries::write_query(&path, &text) {
                Ok(()) => {
                    self.file_text.insert(id, text);
                    self.tabs.tabs[i].dirty = false;
                }
                Err(e) => self.set_status(format!("자동 저장 실패: {e}"), true, cx),
            },
            queries::SaveDecision::Stop(why) => {
                self.autosave_stopped.insert(id, why.clone());
                self.set_status(why, true, cx);
            }
        }
        cx.notify();
    }

    /// 파일 없는 탭의 첫 실행이면 queries/<그룹>/<접속>-<시각>.sql 로 저장하고 매핑에 붙인다(R45.1).
    fn save_new_query_on_run(&mut self, cx: &mut Context<Self>) {
        let Some(dir) = self.project_dir() else { return };
        let Some(t) = self.tabs.active().cloned() else { return };
        if t.path.is_some() || t.name.is_some() {
            return;
        }
        let Some(c) = t.conn.as_ref().and_then(|c| self.conn(c)).cloned() else {
            return;
        };
        if c.kind != "postgresql" {
            return;
        }
        let Some(Body::Sql { editor, .. }) = self.bodies.get(&t.id) else {
            return;
        };
        let text = editor.read(cx).value().to_string();
        if text.trim().is_empty() {
            return;
        }
        match queries::save_new_query(&dir, &c.group, &c.name, &c.uuid, &text) {
            Ok(rel) => {
                if let Some(i) = self.tabs.index_of(t.id) {
                    self.tabs.tabs[i].path = Some(dir.join(&rel));
                    self.tabs.tabs[i].dirty = false;
                }
                self.file_text.insert(t.id, text);
                self.refresh_queries();
                self.set_status(format!("새 쿼리를 저장함: {rel}"), false, cx);
            }
            Err(e) => self.set_status(format!("새 쿼리 저장 실패: {e}"), true, cx),
        }
    }

    fn open_table(&mut self, conn: String, schema: String, table: String, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.tabs.open_named(Some(conn.clone()), format!("{schema}.{table}"));
        self.bodies.insert(id, Body::Table { result: None });
        self.load_source(
            conn,
            Source::Table {
                schema,
                table,
                filter: String::new(),
                order: String::new(),
            },
            window,
            cx,
        );
    }

    // ─── 실행 ───

    fn active_sql(&self, cx: &Context<Self>) -> Option<(String, String)> {
        let t = self.tabs.active()?;
        let Some(Body::Sql { editor, .. }) = self.bodies.get(&t.id) else {
            return None;
        };
        let e = editor.read(cx);
        let text = e.value().to_string();
        let sql = target_sql(&text, e.selected_range(), e.cursor())?;
        Some((t.conn.clone()?, sql))
    }

    fn run_query(&mut self, explain: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(Body::Redis { view }) = self.tabs.active().and_then(|t| self.bodies.get(&t.id)) {
            // RedisView 가 작업 공간을 다시 빌리므로 지금 대여가 끝난 뒤에 부른다
            let view = view.clone();
            window.defer(cx, move |window, cx| view.update(cx, |v, cx| v.run_command(window, cx)));
            return;
        }
        if let Some(Body::Source { view }) = self.tabs.active().and_then(|t| self.bodies.get(&t.id)) {
            let view = view.clone();
            window.defer(cx, move |window, cx| view.update(cx, |v, cx| v.run(window, cx)));
            return;
        }
        let Some(t) = self.tabs.active() else { return };
        if t.conn.is_none() {
            self.set_status("왼쪽 트리에서 접속을 먼저 고른다", true, cx);
            return;
        }
        let Some((conn, sql)) = self.active_sql(cx) else {
            self.set_status("실행할 문장이 없다", true, cx);
            return;
        };
        self.save_new_query_on_run(cx);
        let (_, names) = idb_core::params::bind(&sql);
        if names.is_empty() {
            self.run_sql(conn, sql, BTreeMap::new(), explain, window, cx);
        } else {
            self.ask_params(conn, sql, names, explain, window, cx);
        }
    }

    /// :name·$n 자리표시 값을 묻는다. 지난 값은 기억해 채운다.
    fn ask_params(&mut self, conn: String, sql: String, names: Vec<String>, explain: bool, window: &mut Window, cx: &mut Context<Self>) {
        let mut seen = Vec::new();
        for n in names {
            if !seen.contains(&n) {
                seen.push(n);
            }
        }
        let inputs: Vec<(String, Entity<InputState>)> = seen
            .into_iter()
            .map(|n| {
                let v = self.params.get(&n).cloned().unwrap_or_default();
                let label = n.clone();
                (n, cx.new(|cx| InputState::new(window, cx).placeholder(label).default_value(v)))
            })
            .collect();
        let this = cx.entity().downgrade();
        let first = std::cell::Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if first.replace(false)
                && let Some((_, i)) = inputs.first()
            {
                i.update(cx, |i, cx| i.focus(window, cx));
            }
            let mut d = dialog.title("자리표시 값").w(px(480.));
            for (n, i) in &inputs {
                d = d.child(
                    h_flex()
                        .gap_2()
                        .py_1()
                        .child(div().w(px(120.)).child(n.clone()))
                        .child(div().flex_1().child(Input::new(i))),
                );
            }
            let (this, inputs, conn, sql) = (this.clone(), inputs.clone(), conn.clone(), sql.clone());
            // 실행 버튼과 Enter(on_ok)가 같은 일을 한다. 기본 Dialog 는 버튼을 그리지 않는다
            let ok = Rc::new(move |window: &mut Window, cx: &mut gpui::App| {
                let vals: BTreeMap<String, String> = inputs.iter().map(|(n, i)| (n.clone(), i.read(cx).value().to_string())).collect();
                let _ = this.update(cx, |ws, cx| {
                    ws.params.extend(vals.clone());
                    ws.run_sql(conn.clone(), sql.clone(), vals.clone(), explain, window, cx)
                });
            });
            let ok2 = ok.clone();
            d.child(
                h_flex()
                    .pt_3()
                    .gap_2()
                    .child(div().flex_1())
                    .child(
                        Button::new("params-cancel")
                            .small()
                            .label("취소")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("params-ok")
                            .small()
                            .primary()
                            .label("실행")
                            .on_click(move |_, window, cx| {
                                ok2(window, cx);
                                window.close_dialog(cx);
                            }),
                    ),
            )
            .on_ok(move |_, window, cx| {
                ok(window, cx);
                true
            })
        });
    }

    fn run_sql(
        &mut self,
        conn: String,
        sql: String,
        params: BTreeMap<String, String>,
        explain: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab_id) = self.tabs.active().map(|t| t.id) else { return };
        let (c2, s2) = (conn.clone(), sql.clone());
        let gen0 = self.db_gen(&conn);
        self.guarded(
            if explain { "EXPLAIN" } else { "실행" },
            Rc::new(move |e: Engine, c: Confirms| {
                let req = RunRequest {
                    conn: c2.clone(),
                    sql: s2.clone(),
                    params: params.clone(),
                    confirm_prod: c.prod,
                    confirm_danger: c.danger,
                    confirm_analyze: c.analyze,
                };
                async move {
                    if explain {
                        e.explain(req, false, false, "").await
                    } else {
                        e.run(req).await
                    }
                }
            }),
            Rc::new(move |ws: &mut Workspace, rs: ResultSet, window, cx| {
                if let Some(in_tx) = rs.in_tx {
                    ws.tx.insert(conn.clone(), in_tx);
                }
                if ws.db_gen(&conn) != gen0 {
                    ws.set_status("실행 중에 데이터베이스를 바꿔 결과를 버림", true, cx);
                    return;
                }
                ws.set_status(format!("{}행 · {}ms", rs.rows.len(), rs.elapsed_ms), false, cx);
                ws.show_result(tab_id, conn.clone(), Source::Query { sql: sql.clone() }, rs, false, window, cx);
            }),
            Confirms::default(),
            window,
            cx,
        );
    }

    // 창(window)·cx 두 자리가 늘 붙는 GPUI 뷰 함수라 묶음 구조체를 따로 두지 않는다
    #[allow(clippy::too_many_arguments)]
    fn show_result(
        &mut self,
        tab_id: u64,
        conn: String,
        source: Source,
        rs: ResultSet,
        more: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(engine) = self.engine.clone() else { return };
        let ws = cx.entity().downgrade();
        let page = self.prefs.row_limit;
        let (cur_gen, frozen) = (self.db_gen(&conn), self.is_switching(&conn));
        let view = cx.new(|cx| {
            let mut v = ResultsView::new(ws, self.rt.clone(), engine, conn, source, rs, more, page, window, cx);
            v.db_gen = cur_gen;
            if frozen {
                v.set_frozen(true, cx);
            }
            v
        });
        match self.bodies.get_mut(&tab_id) {
            Some(Body::Sql { result, .. }) | Some(Body::Table { result }) => *result = Some(view),
            _ => {}
        }
        cx.notify();
    }

    /// 출처를 (다시) 읽어 그 출처를 보이는 탭에 넣는다.
    pub fn load_source(&mut self, conn: String, source: Source, window: &mut Window, cx: &mut Context<Self>) {
        let tab_id = self
            .tabs
            .tabs
            .iter()
            .find(|t| match self.bodies.get(&t.id) {
                Some(Body::Sql { result: Some(r), .. }) | Some(Body::Table { result: Some(r) }) => {
                    let r = r.read(cx);
                    r.conn == conn && same_source(&r.source, &source)
                }
                _ => false,
            })
            .or(self.tabs.active())
            .map(|t| t.id);
        let Some(tab_id) = tab_id else { return };
        match source.clone() {
            Source::Static => {}
            Source::Query { sql } => self.run_sql(conn, sql, self.params.clone(), false, window, cx),
            Source::Table {
                schema,
                table,
                filter,
                order,
            } => {
                let req = TablePageRequest {
                    conn: conn.clone(),
                    schema,
                    table,
                    filter,
                    order_by: order,
                    limit: self.prefs.row_limit,
                    ..Default::default()
                };
                let limit = self.prefs.row_limit as usize;
                let gen0 = self.db_gen(&conn);
                self.guarded(
                    "테이블 읽기",
                    Rc::new(move |e: Engine, _| {
                        let req = req.clone();
                        async move { e.table_page(req).await }
                    }),
                    Rc::new(move |ws: &mut Workspace, page: idb_engine::TablePage, window, cx| {
                        if ws.db_gen(&conn) != gen0 {
                            ws.set_status("읽는 중에 데이터베이스를 바꿔 결과를 버림", true, cx);
                            return;
                        }
                        let more = page.result.rows.len() >= limit;
                        ws.show_result(tab_id, conn.clone(), source.clone(), page.result, more, window, cx);
                    }),
                    Confirms::default(),
                    window,
                    cx,
                );
            }
        }
    }

    // ─── 트랜잭션·잠금 ───

    fn tx_op(&mut self, op: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(conn) = self.active_conn() else { return };
        let c2 = conn.clone();
        self.guarded(
            op,
            Rc::new(move |e: Engine, c: Confirms| {
                let conn = c2.clone();
                async move {
                    match op {
                        "BEGIN" => e.tx_begin(&conn).await,
                        "COMMIT" => e.tx_commit(&conn, c.prod).await,
                        _ => e.tx_rollback(&conn).await,
                    }
                }
            }),
            Rc::new(move |ws: &mut Workspace, s: idb_engine::TxState, _, cx| {
                ws.tx.insert(conn.clone(), s.in_tx);
                ws.set_status(if s.text.is_empty() { op.to_string() } else { s.text }, false, cx);
            }),
            Confirms::default(),
            window,
            cx,
        );
    }

    fn toggle_lock(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(conn) = self.active_conn() else { return };
        let locked = self.locks.get(&conn).is_none_or(|(l, _)| l.locked);
        let c2 = conn.clone();
        self.guarded(
            "prod 잠금",
            Rc::new(move |e: Engine, _| {
                let conn = c2.clone();
                async move { e.prod_lock(&conn, Some(locked)).await }
            }),
            Rc::new(move |ws: &mut Workspace, l: LockState, _, cx| {
                let msg = if !l.prod {
                    "prod 접속이 아니라 잠금이 없다".to_string()
                } else if l.locked {
                    "prod 를 잠갔다".to_string()
                } else {
                    format!("prod 를 {}분 동안 열었다", l.unlock_min)
                };
                ws.locks.insert(l.conn.clone(), (l, Instant::now()));
                ws.set_status(msg, false, cx);
            }),
            Confirms::default(),
            window,
            cx,
        );
    }

    // ─── 파일 ───

    fn queries_dir(&self) -> PathBuf {
        match &self.target {
            Some(Target::Local(d)) if d.join("queries").is_dir() => d.join("queries"),
            Some(Target::Local(d)) => d.clone(),
            _ => std::env::var_os("HOME").map(Into::into).unwrap_or_else(|| ".".into()),
        }
    }

    fn open_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("열기".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = rx.await else { return };
            let Some(path) = paths.into_iter().next() else { return };
            let text = std::fs::read_to_string(&path);
            let _ = this.update_in(cx, move |ws, window, cx| match text {
                Ok(text) => {
                    let conn = ws.active_conn();
                    let id = ws.tabs.open(conn, Some(path));
                    ws.file_text.insert(id, text.clone());
                    ws.insert_sql_body(id, text, window, cx);
                    ws.sync_catalog_cells();
                    cx.notify();
                }
                Err(e) => ws.set_status(format!("열기 실패: {e}"), true, cx),
            });
        })
        .detach();
    }

    fn save_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(t) = self.tabs.active().cloned() else { return };
        let Some(Body::Sql { editor, .. }) = self.bodies.get(&t.id) else {
            return;
        };
        let text = editor.read(cx).value().to_string();
        let id = t.id;
        match t.path {
            Some(p) => self.write_file(id, p, text, cx),
            None => {
                let rx = cx.prompt_for_new_path(&self.queries_dir(), Some("query.sql"));
                cx.spawn_in(window, async move |this, cx| {
                    let Ok(Ok(Some(path))) = rx.await else { return };
                    let _ = this.update(cx, move |ws, cx| ws.write_file(id, path, text, cx));
                })
                .detach();
            }
        }
    }

    fn write_file(&mut self, id: u64, path: PathBuf, text: String, cx: &mut Context<Self>) {
        // ⌘S 는 밖에서 바뀌었어도 덮어쓰고 자동 저장을 다시 켠다(R45.2)
        match queries::write_query(&path, &text) {
            Ok(()) => {
                if let Some(i) = self.tabs.index_of(id) {
                    self.tabs.tabs[i].dirty = false;
                    self.tabs.tabs[i].path = Some(path.clone());
                }
                self.file_text.insert(id, text);
                self.autosave_stopped.remove(&id);
                self.refresh_queries();
                self.set_status(format!("저장함: {}", path.display()), false, cx);
            }
            Err(e) => self.set_status(format!("저장 실패: {e}"), true, cx),
        }
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        if let Some((label, c)) = self.busy.take() {
            c.cancel();
            self.set_status(format!("{label}: 멈춤"), true, cx);
        }
    }
}

fn same_source(a: &Source, b: &Source) -> bool {
    match (a, b) {
        (Source::Query { sql: x }, Source::Query { sql: y }) => x == y,
        (Source::Table { schema: s1, table: t1, .. }, Source::Table { schema: s2, table: t2, .. }) => s1 == s2 && t1 == t2,
        _ => false,
    }
}

fn expand_home(s: &str) -> PathBuf {
    let s = s.trim();
    match (s.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(h)) => PathBuf::from(h).join(rest),
        _ => PathBuf::from(s),
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

// ─── 그리기 ───

impl Workspace {
    fn render_picker(&mut self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let mut remotes = v_flex().gap_1();
        match &self.remotes {
            Ok(names) if names.is_empty() => {
                remotes = remotes.child(div().text_color(theme.muted_foreground).child(picker::NO_REMOTES));
            }
            Ok(names) => {
                for (i, c) in picker::remote_choices(names).into_iter().enumerate() {
                    let t = c.target;
                    remotes = remotes.child(
                        Button::new(("remote", i))
                            .label(c.label)
                            .on_click(cx.listener(move |ws, _, w, cx| ws.open_target(t.clone(), w, cx))),
                    );
                }
            }
            Err(e) => remotes = remotes.child(div().text_color(theme.danger).child(e.clone())),
        }
        let mut recent = v_flex().gap_1();
        for (i, t) in self.prefs.recent.iter().enumerate() {
            let t2 = t.clone();
            recent = recent.child(
                Button::new(("recent", i))
                    .ghost()
                    .label(t.label())
                    .on_click(cx.listener(move |ws, _, w, cx| ws.open_target(t2.clone(), w, cx))),
            );
        }
        let section = |title: &str| div().pt_4().pb_1().text_color(theme.muted_foreground).child(title.to_string());
        v_flex().size_full().items_center().justify_center().child(
            v_flex()
                .w(px(600.))
                .gap_1()
                .child(div().text_2xl().child(picker::TITLE))
                .child(div().text_color(theme.muted_foreground).child(picker::SUBTITLE))
                .child(section(picker::REMOTE_SECTION))
                .child(remotes)
                .child(section(picker::LOCAL_SECTION))
                .child(
                    h_flex()
                        .gap_2()
                        .child(div().flex_1().child(Input::new(&self.dir_input)))
                        .child(
                            Button::new("pick-dir")
                                .label(picker::PICK_DIR)
                                .on_click(cx.listener(|_, _, window, cx| {
                                    let rx = cx.prompt_for_paths(PathPromptOptions {
                                        files: false,
                                        directories: true,
                                        multiple: false,
                                        prompt: Some("프로젝트 폴더".into()),
                                    });
                                    cx.spawn_in(window, async move |this, cx| {
                                        let Ok(Ok(Some(paths))) = rx.await else { return };
                                        let Some(p) = paths.into_iter().next() else { return };
                                        let _ = this.update_in(cx, move |ws, window, cx| ws.open_target(Target::Local(p), window, cx));
                                    })
                                    .detach();
                                })),
                        )
                        .child(
                            Button::new("open-dir")
                                .primary()
                                .label(picker::OPEN)
                                .loading(self.connecting)
                                .on_click(cx.listener(|ws, _, window, cx| {
                                    let dir = expand_home(&ws.dir_input.read(cx).value());
                                    ws.open_target(Target::Local(dir), window, cx);
                                })),
                        ),
                )
                .when(!self.prefs.recent.is_empty(), |d| {
                    d.child(section(picker::RECENT_SECTION)).child(recent)
                })
                .child(
                    div()
                        .pt_4()
                        .text_color(if self.status.1 { theme.danger } else { theme.muted_foreground })
                        .child(self.status.0.clone()),
                ),
        )
    }

    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let dark = theme.is_dark();
        let active = self.active_conn();
        let mut list = v_flex().id("conn-tree").py_1().gap_px();
        let rows = self.tree.rows();
        if rows.is_empty() {
            list = list.child(
                div()
                    .px_3()
                    .py_2()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("맞는 접속이 없다"),
            );
        }
        for (i, row) in rows.into_iter().enumerate() {
            let el = match row.kind {
                RowKind::Folder { expanded, count } => {
                    let label = row.label.clone();
                    h_flex()
                        .id(("folder", i))
                        .mx_1()
                        .px_2()
                        .h(rems(1.6))
                        .gap_1()
                        .rounded(theme.radius)
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.sidebar_accent))
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(Icon::new(if expanded { IconName::ChevronDown } else { IconName::ChevronRight }).xsmall())
                        .child(div().flex_1().truncate().child(row.label))
                        .child(div().child(count.to_string()))
                        .on_click(cx.listener(move |ws, _, _, cx| {
                            ws.tree.toggle(&label);
                            cx.notify();
                        }))
                        .into_any_element()
                }
                RowKind::Conn(ci) => {
                    let c = self.tree.conns[ci].clone();
                    let selected = active.as_deref() == Some(c.name.as_str());
                    let (n_open, n_ref, n_copy) = (c.name.clone(), c.name.clone(), c.name.clone());
                    let tip = format!("{} · {} {}\n{}", c.kind, c.host, c.db, c.group);
                    let ws = cx.entity().downgrade();
                    h_flex()
                        .id(("conn", i))
                        .mx_1()
                        .pl(rems(1.2))
                        .pr_2()
                        .h(rems(2.0))
                        .gap_2()
                        .rounded(theme.radius)
                        .cursor_pointer()
                        .when(selected, |d| d.bg(theme.sidebar_accent))
                        .hover(|s| s.bg(theme.sidebar_accent))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(div().truncate().child(c.name.clone()))
                                .child(div().text_xs().text_color(theme.muted_foreground).truncate().child(row.detail)),
                        )
                        .child(env_pill(&c.env, dark))
                        .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                        .on_click(cx.listener(move |ws, _, window, cx| ws.select_conn(n_open.clone(), window, cx)))
                        .context_menu(move |menu, _, _| {
                            let (ws1, ws2) = (ws.clone(), ws.clone());
                            let (n1, n2, n3) = (n_ref.clone(), n_ref.clone(), n_copy.clone());
                            menu.item(PopupMenuItem::new("열기").on_click(move |_, window, cx| {
                                let _ = ws1.update(cx, |ws, cx| ws.select_conn(n1.clone(), window, cx));
                            }))
                            .item(PopupMenuItem::new("새로 고침(테이블 목록)").on_click(move |_, window, cx| {
                                let _ = ws2.update(cx, |ws, cx| ws.refresh_catalog(&n2, window, cx));
                            }))
                            .separator()
                            .item(PopupMenuItem::new("이름 복사").on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(n3.clone()));
                            }))
                        })
                        .into_any_element()
                }
            };
            list = list.child(el);
        }

        // 지금 접속의 테이블(카탈로그). 한 번 누르면 고르고 두 번 누르면 데이터를 연다
        let mut objects = v_flex().id("objects").py_1().gap_px();
        let filter = self.tree_filter.read(cx).value().to_lowercase();
        let has_catalog = active.as_ref().is_some_and(|c| self.catalogs.contains_key(c));
        if let Some(conn) = &active
            && let Some(cat) = self.catalogs.get(conn)
        {
            let mut i: usize = 0;
            for s in &cat.schemas {
                let tables: Vec<_> = s
                    .tables
                    .iter()
                    .filter(|t| filter.is_empty() || t.name.to_lowercase().contains(&filter))
                    .collect();
                if tables.is_empty() {
                    continue;
                }
                objects = objects.child(
                    div()
                        .px_3()
                        .pt_2()
                        .pb_0p5()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(s.name.clone()),
                );
                for t in tables {
                    i += 1;
                    let key = (conn.clone(), s.name.clone(), t.name.clone());
                    let selected = self.selected_table.as_ref() == Some(&key);
                    let k_click = key.clone();
                    let k_menu = key.clone();
                    let ws = cx.entity().downgrade();
                    let tip = format!("{}.{} · 열 {}개 · 두 번 눌러 데이터 보기", s.name, t.name, t.columns.len());
                    objects = objects.child(
                        h_flex()
                            .id(("table", i))
                            .mx_1()
                            .px_3()
                            .h(rems(1.75))
                            .gap_2()
                            .rounded(theme.radius)
                            .cursor_pointer()
                            .when(selected, |d| d.bg(theme.sidebar_accent))
                            .hover(|st| st.bg(theme.sidebar_accent))
                            .child(div().flex_1().truncate().child(t.name.clone()))
                            .when(t.view, |d| {
                                d.child(div().text_xs().text_color(theme.muted_foreground).child("view"))
                            })
                            .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                            .on_click(cx.listener(move |ws, ev: &ClickEvent, window, cx| {
                                ws.selected_table = Some(k_click.clone());
                                if ev.click_count() >= 2 {
                                    let (c, sn, tn) = k_click.clone();
                                    ws.open_table(c, sn, tn, window, cx);
                                }
                                cx.notify();
                            }))
                            .context_menu(move |menu, _, _| {
                                let (ws1, ws2, ws3) = (ws.clone(), ws.clone(), ws.clone());
                                let (k1, k2, k3) = (k_menu.clone(), k_menu.clone(), k_menu.clone());
                                let name = format!("{}.{}", k_menu.1, k_menu.2);
                                menu.item(PopupMenuItem::new("데이터 열기").on_click(move |_, window, cx| {
                                    let (c, s, t) = k1.clone();
                                    let _ = ws1.update(cx, |ws, cx| ws.open_table(c, s, t, window, cx));
                                }))
                                .item(PopupMenuItem::new("DDL 보기").on_click(move |_, window, cx| {
                                    let (c, s, t) = k2.clone();
                                    let _ = ws2.update(cx, |ws, cx| ws.show_ddl(c, s, t, window, cx));
                                }))
                                .item(PopupMenuItem::new("이름 복사").on_click(move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(name.clone()));
                                }))
                                .separator()
                                .item(PopupMenuItem::new("새로 고침").on_click(move |_, window, cx| {
                                    let c = k3.0.clone();
                                    let _ = ws3.update(cx, |ws, cx| ws.refresh_catalog(&c, window, cx));
                                }))
                            }),
                    );
                }
            }
        }

        let queries_el = self.render_queries(&filter, cx);
        let section = |title: &'static str| div().px_3().pt_3().pb_1().text_xs().text_color(theme.muted_foreground).child(title);
        v_flex()
            .size_full()
            .bg(theme.sidebar)
            .child(
                h_flex()
                    .p_2()
                    .gap_1()
                    .child(div().flex_1().child(Input::new(&self.tree_filter).small().cleanable(true)))
                    .child(
                        Button::new("dashboard")
                            .xsmall()
                            .ghost()
                            .icon(IconName::LayoutDashboard)
                            .tooltip("대시보드 (⌘G)")
                            .on_click(cx.listener(|ws, _, w, cx| ws.open_dashboard(w, cx))),
                    )
                    .child(
                        Button::new("switch-conn")
                            .xsmall()
                            .ghost()
                            .icon(IconName::Folder)
                            .tooltip("접속 바꾸기 (⇧⌘O)")
                            .on_click(cx.listener(|ws, _, _, cx| ws.disconnect(cx))),
                    ),
            )
            .child(
                v_flex()
                    .id("sidebar-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(section("접속"))
                    .child(list)
                    .when(has_catalog, |d| d.child(section("테이블")).child(objects))
                    .child(section("쿼리"))
                    .child(queries_el)
                    .when(active.is_some() && !has_catalog, |d| {
                        d.child(
                            div()
                                .px_3()
                                .py_2()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("테이블 목록을 읽는 중…"),
                        )
                    }),
            )
    }

    fn render_queries(&self, filter: &str, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let Some(files) = &self.queries else {
            return div()
                .px_3()
                .py_1()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("원격 접속에서는 serve 쪽 queries/ 를 열지 않는다 (⌘O 로 이 Mac 의 파일을 연다)")
                .into_any_element();
        };
        let shown: Vec<QueryFile> = files
            .iter()
            .filter(|f| filter.is_empty() || f.name.to_lowercase().contains(filter))
            .cloned()
            .collect();
        if shown.is_empty() {
            return div()
                .px_3()
                .py_1()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(if files.is_empty() {
                    "queries/ 에 파일이 없다 — 새 탭에서 처음 실행하면 저장된다"
                } else {
                    "맞는 파일이 없다"
                })
                .into_any_element();
        }
        let mut list = v_flex().py_1().gap_px();
        let mut i = 0usize;
        for (conn, group) in queries::group(&shown, &self.tree.conns) {
            list = list.child(
                div()
                    .px_3()
                    .pt_1()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(conn.clone().unwrap_or_else(|| "접속 없음".into())),
            );
            for f in group {
                i += 1;
                let rel = f.rel.clone();
                let open = self.tabs.tabs.iter().any(|t| t.path.as_ref().is_some_and(|p| p.ends_with(&f.rel)));
                list = list.child(
                    h_flex()
                        .id(("query", i))
                        .mx_1()
                        .px_3()
                        .h(rems(1.75))
                        .rounded(theme.radius)
                        .cursor_pointer()
                        .when(open, |d| d.bg(theme.sidebar_accent))
                        .hover(|st| st.bg(theme.sidebar_accent))
                        .child(div().flex_1().truncate().child(f.name.clone()))
                        .on_click(cx.listener(move |ws, _, window, cx| ws.open_query(rel.clone(), window, cx))),
                );
            }
        }
        list.into_any_element()
    }

    fn conn_label(&self) -> String {
        let Some(c) = self.active_conn().and_then(|c| self.conn(&c).cloned()) else {
            return "접속 고르기".into();
        };
        match self.sessions.get(&c.name) {
            Some(s) => format!("{} · {} · {}", c.name, s.db, s.search_path),
            None if c.db.is_empty() => c.name,
            None => format!("{} · {}", c.name, c.db),
        }
    }

    fn render_toolbar(&mut self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let conn = self.active_conn();
        let summary = conn.as_ref().and_then(|c| self.conn(c)).cloned();
        let in_tx = conn.as_ref().is_some_and(|c| self.tx.get(c).copied().unwrap_or(false));
        let is_prod = summary.as_ref().is_some_and(|s| tone(&s.env) == Tone::Prod);
        let is_pg = summary.as_ref().is_some_and(|s| s.kind == "postgresql");
        let lock = conn.as_ref().and_then(|c| self.locks.get(c)).map(|(l, at)| {
            let left = l.remaining_secs.saturating_sub(at.elapsed().as_secs());
            (l.locked || left == 0, left)
        });
        let locked = lock.is_none_or(|(l, _)| l);
        let busy = self.busy.is_some();

        // 접속·데이터베이스·스키마 고르기
        let conns: Vec<(String, String)> = self
            .tree
            .conns
            .iter()
            .filter(|c| c.kind == "postgresql" || c.kind == "redis")
            .map(|c| (c.name.clone(), c.env.clone()))
            .collect();
        let dbs = conn.as_ref().and_then(|c| self.databases.get(c)).cloned().unwrap_or_default();
        let schemas: Vec<String> = conn
            .as_ref()
            .and_then(|c| self.catalogs.get(c))
            .map(|cat| cat.schemas.iter().map(|s| s.name.clone()).collect())
            .unwrap_or_default();
        let ws = cx.entity().downgrade();
        let cur = conn.clone();
        let selector = Button::new("conn-selector")
            .small()
            .ghost()
            .icon(IconName::ChevronsUpDown)
            .label(self.conn_label())
            .when(is_prod, |b| b.text_color(theme.danger))
            .tooltip("접속·데이터베이스·스키마 바꾸기")
            .dropdown_menu(move |mut menu, _, _| {
                menu = menu.label("접속");
                for (name, env) in &conns {
                    let (ws, n) = (ws.clone(), name.clone());
                    let label = if env == "local" { name.clone() } else { format!("{name}  [{env}]") };
                    menu = menu.item(PopupMenuItem::new(label).checked(cur.as_deref() == Some(name.as_str())).on_click(
                        move |_, window, cx| {
                            let _ = ws.update(cx, |ws, cx| ws.select_conn(n.clone(), window, cx));
                        },
                    ));
                }
                if let Some(c) = &cur {
                    if !dbs.is_empty() {
                        menu = menu.separator().label("데이터베이스");
                        for d in &dbs {
                            let (ws, c, d2) = (ws.clone(), c.clone(), d.clone());
                            menu = menu.item(PopupMenuItem::new(d.clone()).on_click(move |_, window, cx| {
                                let _ = ws.update(cx, |ws, cx| ws.request_switch_db(c.clone(), d2.clone(), window, cx));
                            }));
                        }
                    }
                    if !schemas.is_empty() {
                        menu = menu.separator().label("스키마 (search_path)");
                        for s in &schemas {
                            let (ws, c, s2) = (ws.clone(), c.clone(), s.clone());
                            menu = menu.item(PopupMenuItem::new(s.clone()).on_click(move |_, window, cx| {
                                let _ = ws.update(cx, |ws, cx| ws.set_schema(c.clone(), s2.clone(), window, cx));
                            }));
                        }
                    }
                }
                menu
            });

        h_flex()
            .gap_1()
            .px_2()
            .h(rems(2.5))
            .border_b_1()
            .border_color(theme.border)
            .when(is_prod, |d| d.bg(theme.danger.opacity(0.08)))
            .child(selector)
            .when_some(summary.as_ref(), |d, s| d.child(env_pill(&s.env, theme.is_dark())))
            .child(div().flex_1())
            .when(is_pg, |d| {
                d.child(
                    Button::new("run")
                        .small()
                        .ghost()
                        .icon(IconName::Play)
                        .label("실행")
                        .tooltip("커서 문장 또는 선택 실행 (⌘↵)")
                        .on_click(cx.listener(|ws, _, w, cx| ws.run_query(false, w, cx))),
                )
                .child(
                    Button::new("run-all")
                        .small()
                        .ghost()
                        .icon(IconName::ArrowRight)
                        .label("모두")
                        .tooltip("편집기 문장 모두 실행 (⇧⌘↵)")
                        .on_click(cx.listener(|ws, _, w, cx| ws.run_all(w, cx))),
                )
                .child(
                    Button::new("explain")
                        .small()
                        .ghost()
                        .icon(IconName::Search)
                        .label("EXPLAIN")
                        .tooltip("실행 계획 (⌘E)")
                        .on_click(cx.listener(|ws, _, w, cx| ws.run_query(true, w, cx))),
                )
            })
            .child(
                Button::new("stop")
                    .small()
                    .ghost()
                    .icon(IconName::Square)
                    .label("멈춤")
                    .disabled(!busy)
                    .tooltip("진행 중인 요청 멈춤 (⌘.)")
                    .on_click(cx.listener(|ws, _, _, cx| ws.cancel(cx))),
            )
            .when(is_pg, |d| {
                d.child(div().w_px().h(rems(1.2)).bg(theme.border).mx_1())
                    .child(
                        Button::new("tx")
                            .small()
                            .with_variant(if in_tx { ButtonVariant::Warning } else { ButtonVariant::Ghost })
                            .label(if in_tx { "트랜잭션 중" } else { "BEGIN" })
                            .tooltip(if in_tx {
                                "열린 트랜잭션이 있다"
                            } else {
                                "트랜잭션 시작"
                            })
                            .on_click(cx.listener(move |ws, _, w, cx| {
                                if !in_tx {
                                    ws.tx_op("BEGIN", w, cx)
                                }
                            })),
                    )
                    .when(in_tx, |d| {
                        d.child(
                            Button::new("commit")
                                .small()
                                .ghost()
                                .icon(IconName::Check)
                                .label("COMMIT")
                                .on_click(cx.listener(|ws, _, w, cx| ws.tx_op("COMMIT", w, cx))),
                        )
                        .child(
                            Button::new("rollback")
                                .small()
                                .ghost()
                                .icon(IconName::Undo2)
                                .label("ROLLBACK")
                                .on_click(cx.listener(|ws, _, w, cx| ws.tx_op("ROLLBACK", w, cx))),
                        )
                    })
            })
            .when(is_prod, |d| {
                let label = match lock {
                    Some((false, left)) => format!("prod 열림 {}:{:02}", left / 60, left % 60),
                    _ => "prod 잠김".into(),
                };
                d.child(
                    Button::new("lock")
                        .small()
                        .with_variant(if locked { ButtonVariant::Ghost } else { ButtonVariant::Danger })
                        .icon(IconName::TriangleAlert)
                        .label(label)
                        .tooltip("prod 쓰기 잠금 열기/잠그기 (⌘L)")
                        .on_click(cx.listener(|ws, _, w, cx| ws.toggle_lock(w, cx))),
                )
            })
    }

    fn render_tabs(&mut self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let mut bar = h_flex()
            .id("tabs")
            .h(rems(2.25))
            .overflow_x_scroll()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.tab_bar);
        for (i, t) in self.tabs.tabs.iter().enumerate() {
            let active = i == self.tabs.active;
            let prod = t
                .conn
                .as_ref()
                .and_then(|c| self.conn(c))
                .is_some_and(|c| tone(&c.env) == Tone::Prod);
            let mut base = t.clone();
            base.dirty = false;
            bar = bar.child(
                h_flex()
                    .id(("tab", i))
                    .group("tab")
                    .h_full()
                    .px_3()
                    .gap_2()
                    .border_r_1()
                    .border_color(theme.border)
                    .cursor_pointer()
                    .text_color(if active { theme.foreground } else { theme.muted_foreground })
                    .when(active, |d| d.bg(theme.background))
                    .when(!active, |d| d.hover(|s| s.bg(theme.secondary.opacity(0.5))))
                    .when(prod, |d| d.border_t_2().border_color(theme.danger))
                    .child(div().child(base.title()))
                    .child(
                        div()
                            .id(("close", i))
                            .w(rems(1.0))
                            .flex()
                            .justify_center()
                            .rounded(theme.radius)
                            .text_xs()
                            .hover(|s| s.bg(theme.secondary))
                            // 고친 탭은 점을 보이고, 올리면 닫기 단추가 된다
                            .child(if t.dirty { "●" } else { "×" })
                            .tooltip(|window, cx| Tooltip::new("탭 닫기 (⌘W)").build(window, cx))
                            .on_click(cx.listener(move |ws, _, _, cx| {
                                cx.stop_propagation();
                                ws.close_tab(i, cx)
                            })),
                    )
                    .on_mouse_down(MouseButton::Middle, cx.listener(move |ws, _, _, cx| ws.close_tab(i, cx)))
                    .on_click(cx.listener(move |ws, _, _, cx| {
                        // 탭을 옮길 때 자동 저장한다(R45.2)
                        if let Some(id) = ws.tabs.active().map(|t| t.id) {
                            ws.autosave(id, cx);
                        }
                        ws.tabs.activate(i);
                        cx.notify();
                    })),
            );
        }
        bar.child(
            Button::new("new-tab")
                .xsmall()
                .ghost()
                .icon(IconName::Plus)
                .tooltip("새 탭 (⌘T)")
                .on_click(cx.listener(|ws, _, w, cx| ws.new_tab(None, w, cx))),
        )
    }

    fn render_body(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let empty = |title: &str, hint: &str| {
            v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_1()
                .child(div().text_color(theme.muted_foreground).child(title.to_string()))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground.opacity(0.7))
                        .child(hint.to_string()),
                )
                .into_any_element()
        };
        let Some(t) = self.tabs.active() else {
            return empty("열린 탭이 없다", "⌘T 로 새 탭을 연다");
        };
        let no_conn = t.conn.is_none();
        match self.bodies.get(&t.id) {
            Some(Body::Sql { editor, result, .. }) => {
                let result_el = match result {
                    Some(r) => r.clone().into_any_element(),
                    None if no_conn => empty("왼쪽에서 접속을 고르세요", "접속을 누르면 이 탭에서 쓴다"),
                    None => empty("⌘↵ 로 실행", "커서가 든 문장(선택이 있으면 선택)을 실행한다 · ⇧⌘↵ 모두 실행"),
                };
                v_resizable(SharedString::from("editor-split"))
                    .with_state(&self.editor_split)
                    .child(
                        resizable_panel()
                            .size(px(self.prefs.editor_height))
                            .child(Editor::new(editor).h_full().bordered(false)),
                    )
                    .child(resizable_panel().visible(self.show_results).child(result_el))
                    .into_any_element()
            }
            Some(Body::Table { result: Some(r) }) => r.clone().into_any_element(),
            Some(Body::Table { result: None }) => empty("읽는 중…", ""),
            Some(Body::Redis { view }) => view.clone().into_any_element(),
            Some(Body::Source { view }) => view.clone().into_any_element(),
            Some(Body::Dashboard { view }) => view.clone().into_any_element(),
            None => empty("", ""),
        }
    }

    fn render_status(&mut self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let conn = self.active_conn();
        let lock = conn.as_ref().and_then(|c| self.locks.get(c)).and_then(|(l, at)| {
            let mut l = l.clone();
            l.remaining_secs = l.remaining_secs.saturating_sub(at.elapsed().as_secs());
            if !l.locked && l.remaining_secs == 0 {
                l.locked = true;
            }
            lock_label(&l)
        });
        let in_tx = conn.as_ref().is_some_and(|c| self.tx.get(c).copied().unwrap_or(false));
        let summary = self.tabs.active().and_then(|t| match self.bodies.get(&t.id) {
            Some(Body::Sql { result: Some(r), .. }) | Some(Body::Table { result: Some(r) }) => Some(r.read(cx).summary(cx)),
            _ => None,
        });
        let pill = |text: String, bg: gpui::Hsla, fg: gpui::Hsla| div().px_2().rounded_full().bg(bg).text_color(fg).child(text);
        h_flex()
            .px_3()
            .h(rems(1.6))
            .gap_3()
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.status_bar)
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(div().child(self.target.as_ref().map(Target::label).unwrap_or_default()))
            .when(in_tx, |d| {
                d.child(pill("트랜잭션 중".into(), theme.warning.opacity(0.2), theme.warning))
            })
            .when_some(lock, |d, l| d.child(pill(l, theme.danger.opacity(0.15), theme.danger)))
            .when_some(self.busy.as_ref().map(|b| b.0.clone()), |d, label| {
                d.child(div().child(format!("{label} 중…"))).child(
                    Button::new("cancel")
                        .xsmall()
                        .ghost()
                        .label("멈춤 ⌘.")
                        .on_click(cx.listener(|ws, _, _, cx| ws.cancel(cx))),
                )
            })
            .child(
                div()
                    .flex_1()
                    .truncate()
                    .text_color(if self.status.1 { theme.danger } else { theme.muted_foreground })
                    .child(self.status.0.clone()),
            )
            .when_some(summary, |d, s| d.child(div().child(s)))
            .when(self.prefs.zoom != zoom::DEFAULT, |d| {
                d.child(div().child(zoom::label(self.prefs.zoom)))
            })
    }
}

/// 환경 알약 배지. prod 빨강·stg 호박색 기운, local 은 무채색이다(vm::palette).
fn env_pill(env: &str, dark: bool) -> impl IntoElement + use<> {
    let (bg, fg) = palette::pill(tone(env), dark);
    div()
        .px_2()
        .rounded_full()
        .text_xs()
        .bg(hex(bg))
        .text_color(hex(fg))
        .child(env.to_string())
}

/// `#rrggbb`·`#rrggbbaa` 를 색으로 바꾼다. 팔레트 시험이 꼴을 지킨다.
pub(super) fn hex(s: &str) -> gpui::Hsla {
    let h = s.trim_start_matches('#');
    let v = u32::from_str_radix(h, 16).unwrap_or(0);
    gpui::rgba(if h.len() == 6 { (v << 8) | 0xff } else { v }).into()
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let _ = window;
        let root = div()
            .id("workspace")
            .key_context("Workspace")
            .track_focus(&self.focus)
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .font_family(theme.font_family.clone())
            .text_size(theme.font_size)
            .on_action(cx.listener(|ws, _: &RunQuery, w, cx| ws.run_query(false, w, cx)))
            .on_action(cx.listener(|ws, _: &RunAll, w, cx| ws.run_all(w, cx)))
            .on_action(cx.listener(|ws, _: &ExplainQuery, w, cx| ws.run_query(true, w, cx)))
            .on_action(cx.listener(|ws, _: &NewTab, w, cx| {
                if ws.engine.is_some() {
                    ws.new_tab(None, w, cx)
                }
            }))
            .on_action(cx.listener(|ws, _: &CloseTab, _, cx| {
                let i = ws.tabs.active;
                ws.close_tab(i, cx)
            }))
            .on_action(cx.listener(|ws, _: &OpenFile, w, cx| ws.open_file(w, cx)))
            .on_action(cx.listener(|ws, _: &SaveFile, w, cx| ws.save_file(w, cx)))
            .on_action(cx.listener(|ws, _: &SwitchConn, _, cx| ws.disconnect(cx)))
            .on_action(cx.listener(|ws, _: &ToggleProdLock, w, cx| ws.toggle_lock(w, cx)))
            .on_action(cx.listener(|ws, _: &CancelRun, _, cx| ws.cancel(cx)))
            .on_action(cx.listener(|ws, _: &TxBegin, w, cx| ws.tx_op("BEGIN", w, cx)))
            .on_action(cx.listener(|ws, _: &TxCommit, w, cx| ws.tx_op("COMMIT", w, cx)))
            .on_action(cx.listener(|ws, _: &TxRollback, w, cx| ws.tx_op("ROLLBACK", w, cx)))
            .on_action(cx.listener(|ws, _: &ZoomIn, w, cx| ws.set_zoom(zoom::zoom_in(ws.prefs.zoom), w, cx)))
            .on_action(cx.listener(|ws, _: &ZoomOut, w, cx| ws.set_zoom(zoom::zoom_out(ws.prefs.zoom), w, cx)))
            .on_action(cx.listener(|ws, _: &ZoomReset, w, cx| ws.set_zoom(zoom::DEFAULT, w, cx)))
            .on_action(cx.listener(|ws, _: &ToggleSidebar, _, cx| {
                ws.show_sidebar = !ws.show_sidebar;
                cx.notify();
            }))
            .on_action(cx.listener(|ws, _: &ShowDashboard, w, cx| ws.open_dashboard(w, cx)))
            .on_action(cx.listener(|ws, _: &ToggleResults, _, cx| {
                ws.show_results = !ws.show_results;
                cx.notify();
            }))
            .on_action(cx.listener(|ws, _: &OpenSettings, w, cx| super::dialogs::open_settings(ws, w, cx)))
            .on_action(cx.listener(|_, _: &ShowShortcuts, w, cx| super::dialogs::open_shortcuts(w, cx)))
            .on_action(cx.listener(|_, _: &About, w, cx| super::dialogs::open_about(w, cx)))
            .on_action(cx.listener(|_, _: &Minimize, w, _| w.minimize_window()));
        if self.engine.is_none() {
            return root.child(self.render_picker(cx));
        }
        let sidebar = self.render_sidebar(cx);
        let toolbar = self.render_toolbar(cx);
        let tabs = self.render_tabs(cx);
        let body = self.render_body(cx);
        let status = self.render_status(cx);
        let main = v_flex()
            .size_full()
            .child(tabs)
            .child(toolbar)
            .child(div().flex_1().min_h_0().child(body));
        root.child(
            v_flex()
                .size_full()
                .child(
                    div().flex_1().min_h_0().child(
                        h_resizable(SharedString::from("main-split"))
                            .with_state(&self.main_split)
                            .child(
                                resizable_panel()
                                    .size(px(self.prefs.sidebar_width))
                                    .size_range(px(180.)..px(600.))
                                    .visible(self.show_sidebar)
                                    .child(sidebar),
                            )
                            .child(resizable_panel().child(main)),
                    ),
                )
                .child(status),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext, size};
    use gpui_component::Root;

    fn setup(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
        cx.update(gpui_component::init);
        // 엔진 요청은 tokio 스레드에서 끝나므로 시험 실행기가 기다릴 수 있게 한다
        cx.executor().allow_parking();
        let rt = Rc::new(Runtime::new().unwrap());
        let mut ws = None;
        let (_, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|cx| {
                let mut w = Workspace::new(rt, crate::vm::font::resolve("D2Coding", 13.0, &[]), window, cx);
                // 시험은 사용자 설정 파일(~/.config/db/gui.json)을 읽지도 쓰지도 않는다
                w.prefs_path = None;
                w.prefs = Prefs::default();
                w
            });
            ws = Some(view.clone());
            Root::new(view, window, cx)
        });
        cx.simulate_resize(size(px(1200.), px(800.)));
        draw(cx);
        (ws.unwrap(), cx)
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
    }

    /// tokio 쪽 요청이 끝날 때까지 돌린다.
    fn wait(cx: &mut VisualTestContext, ws: &Entity<Workspace>, done: impl Fn(&Workspace) -> bool) {
        for _ in 0..500 {
            cx.run_until_parked();
            if cx.update(|_, cx| done(ws.read(cx))) {
                draw(cx);
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("시간 안에 끝나지 않았다: {:?}", cx.update(|_, cx| ws.read(cx).status.clone()));
    }

    fn offline_project() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/golden/offline")
    }

    #[gpui::test]
    fn 고르는_화면을_그리고_로컬_프로젝트를_열면_트리와_탭이_생긴다(cx: &mut TestAppContext) {
        let (ws, cx) = setup(cx);
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_target(Target::Local(offline_project()), window, cx)));
        wait(cx, &ws, |w| w.engine.is_some());
        cx.update(|_, cx| {
            let w = ws.read(cx);
            assert!(!w.tree.conns.is_empty());
            assert_eq!(w.tabs.tabs.len(), 1);
            assert!(matches!(w.bodies.get(&w.tabs.tabs[0].id), Some(Body::Sql { .. })));
        });
    }

    #[gpui::test]
    fn 결과와_편집_표시를_그린다(cx: &mut TestAppContext) {
        let (ws, cx) = setup(cx);
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_target(Target::Local(offline_project()), window, cx)));
        wait(cx, &ws, |w| w.engine.is_some());
        let rs: ResultSet = serde_json::from_value(serde_json::json!({
            "cols": ["id", "name"], "types": ["int8", "text"],
            "rows": [["1", "kim"], ["2", "NULL"]], "text": "", "truncated": false, "elapsed_ms": 3,
            "origin": {"table": {"schema": "public", "table": "users"}, "pk": ["id"], "cols": ["id", "name"]}
        }))
        .unwrap();
        cx.update(|window, cx| {
            ws.update(cx, |w, cx| {
                let id = w.tabs.tabs[0].id;
                w.show_result(id, "x".into(), Source::Query { sql: "select 1".into() }, rs, false, window, cx);
            })
        });
        draw(cx);
        let result = cx.update(|_, cx| {
            let w = ws.read(cx);
            match w.bodies.get(&w.tabs.tabs[0].id) {
                Some(Body::Sql { result: Some(r), .. }) => r.clone(),
                _ => panic!("결과가 없다"),
            }
        });
        // 0.1.1 의 abort: 결과의 "다시 읽기"가 자기를 빌린 채 작업 공간이 그 결과를 읽어 패닉했다
        cx.update(|window, cx| result.update(cx, |v, cx| v.reload(window, cx)));
        draw(cx);
    }

    /// 0.1.1 의 abort: Redis 접속을 누르면 RedisView 가 만들어지며 작업 공간을 다시 빌려(scan) 패닉했다.
    #[gpui::test]
    fn Redis_접속을_눌러도_작업_공간을_두_번_빌리지_않는다(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        // 오프라인 프로젝트의 DataGrip 접속 위에 Redis 접속 하나를 더한다
        std::fs::create_dir_all(dir.path().join(".idea")).unwrap();
        std::fs::create_dir_all(dir.path().join(".dbtui")).unwrap();
        for f in ["dataSources.xml", "dataSources.local.xml"] {
            std::fs::copy(offline_project().join(".idea").join(f), dir.path().join(".idea").join(f)).unwrap();
        }
        std::fs::write(
            dir.path().join(".dbtui/connections.json"),
            r#"[{"uuid": "r1", "name": "cache-local", "group": "c", "kind": "redis", "host": "127.0.0.1", "port": 1}]"#,
        )
        .unwrap();
        let (ws, cx) = setup(cx);
        let p = dir.path().to_path_buf();
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_target(Target::Local(p), window, cx)));
        wait(cx, &ws, |w| w.engine.is_some());
        cx.update(|window, cx| ws.update(cx, |w, cx| w.select_conn("cache-local".into(), window, cx)));
        draw(cx);
        // ⌘↵ 도 Redis 탭에서는 RedisView 로 간다(같은 이중 대여 길)
        cx.update(|window, cx| ws.update(cx, |w, cx| w.run_query(false, window, cx)));
        draw(cx);
        cx.update(|_, cx| {
            let w = ws.read(cx);
            let t = w.tabs.active().unwrap();
            assert!(matches!(w.bodies.get(&t.id), Some(Body::Redis { .. })));
        });
    }

    #[gpui::test]
    fn 저장소_접속을_누르면_트리_탭을_열고_다시_누르면_그_탭으로_간다(cx: &mut TestAppContext) {
        let (ws, cx) = setup(cx);
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_target(Target::Local(offline_project()), window, cx)));
        wait(cx, &ws, |w| w.engine.is_some());
        cx.update(|window, cx| ws.update(cx, |w, cx| w.select_conn("aws-stg".into(), window, cx)));
        wait(cx, &ws, |w| w.busy.is_none());
        cx.update(|window, cx| ws.update(cx, |w, cx| w.select_conn("logs-stg".into(), window, cx)));
        wait(cx, &ws, |w| w.busy.is_none());
        cx.update(|window, cx| ws.update(cx, |w, cx| w.select_conn("aws-stg".into(), window, cx)));
        draw(cx);
        let view = cx.update(|_, cx| {
            let w = ws.read(cx);
            let n = w
                .tabs
                .tabs
                .iter()
                .filter(|t| matches!(w.bodies.get(&t.id), Some(Body::Source { .. })))
                .count();
            assert_eq!(n, 2, "같은 저장소는 탭 하나");
            match w.tabs.active().and_then(|t| w.bodies.get(&t.id)) {
                Some(Body::Source { view }) => view.clone(),
                _ => panic!("저장소 탭이 아니다"),
            }
        });
        cx.update(|_, cx| {
            let v = view.read(cx);
            assert_eq!((v.conn.as_str(), v.kind.as_str()), ("aws-stg", "aws"));
        });
        // ⌘↵: 빈 편집기는 보내지 않는다
        cx.update(|window, cx| ws.update(cx, |w, cx| w.run_query(false, window, cx)));
        draw(cx);
        cx.update(|_, cx| assert!(ws.read(cx).busy.is_none()));
    }

    #[gpui::test]
    fn 대시보드는_한_탭만_열고_모든_접속을_확인한다(cx: &mut TestAppContext) {
        let (ws, cx) = setup(cx);
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_target(Target::Local(offline_project()), window, cx)));
        wait(cx, &ws, |w| w.engine.is_some());
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_dashboard(window, cx)));
        draw(cx);
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_dashboard(window, cx)));
        draw(cx);
        let (view, n_conns) = cx.update(|_, cx| {
            let w = ws.read(cx);
            let dash: Vec<_> = w
                .tabs
                .tabs
                .iter()
                .filter_map(|t| match w.bodies.get(&t.id) {
                    Some(Body::Dashboard { view }) => Some(view.clone()),
                    _ => None,
                })
                .collect();
            assert_eq!(dash.len(), 1);
            (dash[0].clone(), w.tree.conns.len())
        });
        // 확인은 접속마다 하나씩 동시에 간다(읽기 전용, 확인 창 없음)
        cx.update(|_, cx| {
            let v = view.read(cx);
            assert!(
                v.checking + v.health.len() >= n_conns,
                "확인 {} + 결과 {} < 접속 {n_conns}",
                v.checking,
                v.health.len()
            );
            assert!(ws.read(cx).busy.is_none(), "대시보드 확인은 작업 공간 요청 칸을 쓰지 않는다");
        });
    }

    fn redis_project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".idea")).unwrap();
        std::fs::create_dir_all(dir.path().join(".dbtui")).unwrap();
        for f in ["dataSources.xml", "dataSources.local.xml"] {
            std::fs::copy(offline_project().join(".idea").join(f), dir.path().join(".idea").join(f)).unwrap();
        }
        std::fs::write(
            dir.path().join(".dbtui/connections.json"),
            r#"[{"uuid": "r1", "name": "cache-local", "group": "c", "kind": "redis", "host": "127.0.0.1", "port": 1}]"#,
        )
        .unwrap();
        dir
    }

    #[gpui::test]
    fn Redis_편집은_뻔한_입력_오류를_보내지_않고_전체_패턴_삭제를_막는다(cx: &mut TestAppContext) {
        let dir = redis_project();
        let (ws, cx) = setup(cx);
        let p = dir.path().to_path_buf();
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_target(Target::Local(p), window, cx)));
        wait(cx, &ws, |w| w.engine.is_some());
        cx.update(|window, cx| ws.update(cx, |w, cx| w.select_conn("cache-local".into(), window, cx)));
        wait(cx, &ws, |w| w.busy.is_none());
        let view = cx.update(|_, cx| {
            let w = ws.read(cx);
            match w.tabs.active().and_then(|t| w.bodies.get(&t.id)) {
                Some(Body::Redis { view }) => view.clone(),
                _ => panic!("Redis 탭이 아니다"),
            }
        });
        let raw: idb_engine::RawValue = serde_json::from_value(serde_json::json!({
            "key": "k", "type": "string", "ttl_secs": -1, "string": "v", "truncated": true
        }))
        .unwrap();
        cx.update(|window, cx| {
            view.update(cx, |v, cx| {
                v.raw = Some(raw);
                v.apply_edit(crate::vm::redisedit::Action::SetString("x".into()), window, cx);
            })
        });
        draw(cx);
        cx.update(|_, cx| {
            let w = ws.read(cx);
            assert!(w.busy.is_none(), "잘린 값은 보내지 않는다");
            assert!(w.status.0.contains("일부만 읽었다"), "{:?}", w.status);
        });
        cx.update(|window, cx| {
            view.update(cx, |v, cx| {
                v.pattern.update(cx, |i, cx| i.set_value("*", window, cx));
                v.pattern_delete(window, cx);
            })
        });
        draw(cx);
        cx.update(|_, cx| assert!(ws.read(cx).status.0.contains("패턴을 좁힌다"), "{:?}", ws.read(cx).status));
    }

    fn edited_result(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> Entity<ResultsView> {
        edited_result_on(ws, "app-stg", cx)
    }

    fn edited_result_on(ws: &Entity<Workspace>, conn: &str, cx: &mut VisualTestContext) -> Entity<ResultsView> {
        let rs: ResultSet = serde_json::from_value(serde_json::json!({
            "cols": ["id", "name"], "rows": [["1", "kim"]], "text": "", "truncated": false, "elapsed_ms": 1,
            "origin": {"table": {"schema": "public", "table": "users"}, "pk": ["id"], "cols": ["id", "name"]}
        }))
        .unwrap();
        cx.update(|window, cx| {
            ws.update(cx, |w, cx| {
                let id = w.tabs.tabs[0].id;
                w.tabs.tabs[0].conn = Some(conn.into());
                w.show_result(id, conn.into(), Source::Query { sql: "select 1".into() }, rs, false, window, cx);
            })
        });
        draw(cx);
        let r = cx.update(|_, cx| {
            let w = ws.read(cx);
            match w.bodies.get(&w.tabs.tabs[0].id) {
                Some(Body::Sql { result: Some(r), .. }) => r.clone(),
                _ => panic!("결과가 없다"),
            }
        });
        cx.update(|_, cx| r.update(cx, |v, cx| v.set_cell(0, 1, Some("lee".into()), "kim", cx)));
        assert_eq!(cx.update(|_, cx| r.read(cx).pending(cx)), 1);
        r
    }

    /// 리뷰 1: DB A 에서 고친 바꿈이 B 로 바꾼 뒤 B 에 반영될 수 있었다.
    #[gpui::test]
    fn 반영_안_한_바꿈이_있으면_데이터베이스를_바로_바꾸지_않는다(cx: &mut TestAppContext) {
        let (ws, cx) = setup(cx);
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_target(Target::Local(offline_project()), window, cx)));
        wait(cx, &ws, |w| w.engine.is_some());
        edited_result(&ws, cx);
        cx.update(|window, cx| ws.update(cx, |w, cx| w.request_switch_db("app-stg".into(), "other".into(), window, cx)));
        draw(cx);
        cx.update(|window, cx| {
            assert!(window.has_active_dialog(cx), "버릴지 묻는 창이 떠야 한다");
            let w = ws.read(cx);
            assert!(w.busy.is_none(), "엔진에 바꾸기를 보내지 않는다");
            assert_eq!(w.db_gen("app-stg"), 0);
        });
    }

    #[gpui::test]
    fn 데이터베이스를_바꾸면_옛_결과의_바꿈을_버리고_반영을_막는다(cx: &mut TestAppContext) {
        let (ws, cx) = setup(cx);
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_target(Target::Local(offline_project()), window, cx)));
        wait(cx, &ws, |w| w.engine.is_some());
        let r = edited_result(&ws, cx);
        let cs = cx.update(|_, _| idb_engine::ChangeSet {
            table: idb_engine::TableRef {
                schema: "public".into(),
                table: "users".into(),
            },
            pk: vec!["id".into()],
            changes: vec![],
            override_system: false,
        });
        cx.update(|_, cx| ws.update(cx, |w, cx| w.on_db_switched("app-stg", cx)));
        draw(cx);
        cx.update(|window, cx| {
            assert!(r.read(cx).stale);
            assert_eq!(r.read(cx).pending(cx), 0);
            assert_eq!(ws.read(cx).db_gen("app-stg"), 1);
            r.update(cx, |v, cx| v.submit(cs, window, cx));
        });
        draw(cx);
        cx.update(|_, cx| {
            let w = ws.read(cx);
            assert!(w.busy.is_none(), "반영을 보내지 않는다");
            assert!(w.status.0.contains("데이터베이스를 바꾸기 전"), "{:?}", w.status);
        });
    }

    /// 재리뷰: 바꾸기 요청이 도는 동안(원격이면 엔진은 이미 B 일 수 있다) A 결과로 반영하면 안 된다.
    #[gpui::test]
    fn 데이터베이스_바꾸기가_도는_동안_반영과_고치기를_막는다(cx: &mut TestAppContext) {
        let (ws, cx) = setup(cx);
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_target(Target::Local(offline_project()), window, cx)));
        wait(cx, &ws, |w| w.engine.is_some());
        let r = edited_result(&ws, cx);
        let cs = idb_engine::ChangeSet {
            table: idb_engine::TableRef {
                schema: "public".into(),
                table: "users".into(),
            },
            pk: vec!["id".into()],
            changes: vec![],
            override_system: false,
        };
        cx.update(|window, cx| {
            // 바꾸기를 보낸 바로 뒤, 답이 오기 전이다
            ws.update(cx, |w, cx| w.switch_db("app-stg".into(), "other".into(), window, cx));
            assert!(ws.read(cx).is_switching("app-stg"));
            assert!(r.read(cx).frozen);
            r.update(cx, |v, cx| v.submit(cs.clone(), window, cx));
            r.update(cx, |v, cx| v.set_cell(0, 1, Some("park".into()), "kim", cx));
            assert_eq!(r.read(cx).pending(cx), 1, "고치기도 막는다");
            let w = ws.read(cx);
            assert_eq!(
                w.busy.as_ref().map(|b| b.0.as_str()),
                Some("데이터베이스 바꾸기"),
                "반영을 보내지 않았다"
            );
            assert!(w.write_ok("app-stg", 0).is_err());
        });
        // 바꾸기가 성공하면 옛 결과가 되고 세대가 오른다
        wait(cx, &ws, |w| w.busy.is_none());
        cx.update(|_, cx| {
            let w = ws.read(cx);
            assert!(!w.is_switching("app-stg"));
            assert_eq!(w.db_gen("app-stg"), 1);
            assert!(w.write_ok("app-stg", 0).is_err(), "세대가 다르면 막는다");
            assert!(w.write_ok("app-stg", 1).is_ok());
            assert!(!r.read(cx).frozen && r.read(cx).stale);
        });
    }

    #[gpui::test]
    fn 데이터베이스_바꾸기가_실패하면_결과를_다시_푼다(cx: &mut TestAppContext) {
        let (ws, cx) = setup(cx);
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_target(Target::Local(offline_project()), window, cx)));
        wait(cx, &ws, |w| w.engine.is_some());
        // 프로젝트에 없는 접속이라 엔진이 바꾸기를 거절한다
        let r = edited_result_on(&ws, "ghost", cx);
        cx.update(|window, cx| {
            ws.update(cx, |w, cx| w.switch_db("ghost".into(), "other".into(), window, cx));
            assert!(r.read(cx).frozen);
        });
        wait(cx, &ws, |w| w.busy.is_none());
        cx.update(|_, cx| {
            let w = ws.read(cx);
            assert!(!w.is_switching("ghost"));
            assert_eq!(w.db_gen("ghost"), 0);
            assert!(w.write_ok("ghost", 0).is_ok());
            assert!(!r.read(cx).frozen && !r.read(cx).stale);
            assert_eq!(r.read(cx).pending(cx), 1, "실패하면 바꿈을 그대로 둔다");
        });
    }

    /// 재리뷰: 바꾸기를 멈추면(⌘.) 원격은 이미 바꿨을 수 있다. 옛 결과는 무효로 하고 엔진에 물어 확인한 뒤에 푼다.
    #[gpui::test]
    fn 바꾸기를_멈추면_옛_결과를_무효로_하고_지금_데이터베이스를_확인한_뒤_푼다(cx: &mut TestAppContext) {
        let (ws, cx) = setup(cx);
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_target(Target::Local(offline_project()), window, cx)));
        wait(cx, &ws, |w| w.engine.is_some());
        let r = edited_result(&ws, cx);
        cx.update(|window, cx| {
            ws.update(cx, |w, cx| w.on_switch_cancelled("app-stg", window, cx));
            let w = ws.read(cx);
            assert_eq!(w.db_gen("app-stg"), 1, "옛 결과는 무효다");
            assert!(w.is_switching("app-stg"), "확인 전에는 쓰기를 막는다");
            assert!(w.write_ok("app-stg", 1).is_err());
            assert!(r.read(cx).stale);
            assert_eq!(r.read(cx).pending(cx), 0);
        });
        wait(cx, &ws, |w| !w.is_switching("app-stg"));
        cx.update(|_, cx| {
            let w = ws.read(cx);
            assert_eq!(
                w.sessions.get("app-stg").map(|s| s.db.as_str()),
                Some("app"),
                "엔진이 아는 DB 로 맞춘다"
            );
            assert!(w.write_ok("app-stg", 1).is_ok(), "확인한 뒤 새 결과는 쓸 수 있다");
            assert!(w.write_ok("app-stg", 0).is_err(), "옛 결과는 여전히 막는다");
        });
    }

    #[gpui::test]
    fn 전환_결과를_확인하지_못하면_쓰기를_계속_막는다(cx: &mut TestAppContext) {
        let (ws, cx) = setup(cx);
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_target(Target::Local(offline_project()), window, cx)));
        wait(cx, &ws, |w| w.engine.is_some());
        let r = edited_result(&ws, cx);
        cx.update(|_, cx| ws.update(cx, |w, cx| w.db_unconfirmed("app-stg", cx)));
        cx.update(|_, cx| {
            let w = ws.read(cx);
            assert!(w.is_switching("app-stg"));
            assert!(w.write_ok("app-stg", 0).is_err());
            assert_eq!(w.status.0, "데이터베이스 전환 결과를 확인하지 못해 결과를 다시 실행한다");
            assert!(r.read(cx).frozen);
        });
    }

    /// 멈춤은 확실한 실패(fail)와 따로 간다. cancelled 가 없을 때만 fail 로 떨어진다.
    #[gpui::test]
    fn 멈춤은_실패와_다른_길로_간다(cx: &mut TestAppContext) {
        let (ws, cx) = setup(cx);
        let log = Rc::new(std::cell::RefCell::new(Vec::<&str>::new()));
        let (l1, l2) = (log.clone(), log.clone());
        let both = Hooks {
            precheck: None,
            fail: Some(Rc::new(move |_: &mut Workspace, _: &mut Window, _: &mut Context<Workspace>| {
                l1.borrow_mut().push("fail")
            })),
            cancelled: Some(Rc::new(move |_: &mut Workspace, _: &mut Window, _: &mut Context<Workspace>| {
                l2.borrow_mut().push("cancel")
            })),
        };
        let l3 = log.clone();
        let only_fail = Hooks {
            precheck: None,
            fail: Some(Rc::new(move |_: &mut Workspace, _: &mut Window, _: &mut Context<Workspace>| {
                l3.borrow_mut().push("fail-only")
            })),
            cancelled: None,
        };
        cx.update(|window, cx| {
            ws.update(cx, |w, cx| {
                both.cancelled(w, window, cx);
                both.failed(w, window, cx);
                only_fail.cancelled(w, window, cx);
            })
        });
        assert_eq!(*log.borrow(), vec!["cancel", "fail", "fail-only"]);
    }

    fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
        std::fs::create_dir_all(to).unwrap();
        for e in std::fs::read_dir(from).unwrap().flatten() {
            let (src, dst) = (e.path(), to.join(e.file_name()));
            if e.file_type().unwrap().is_dir() {
                copy_dir(&src, &dst);
            } else {
                std::fs::copy(&src, &dst).unwrap();
            }
        }
    }

    #[gpui::test]
    fn 쿼리_파일을_매핑된_접속으로_열고_자동_저장은_밖의_바뀜에서_멈춘다(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        copy_dir(&offline_project(), dir.path());
        let (ws, cx) = setup(cx);
        let p = dir.path().to_path_buf();
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_target(Target::Local(p), window, cx)));
        wait(cx, &ws, |w| w.engine.is_some());
        let (rel, conn) = cx.update(|_, cx| {
            let q = ws.read(cx).queries.clone().expect("로컬이면 쿼리 목록이 있다");
            let f = q.iter().find(|f| f.rel == "queries/app/a.sql").expect("a.sql").clone();
            (f.rel, f.conn)
        });
        assert!(conn.is_some(), "매핑으로 접속을 잇는다");
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_query(rel.clone(), window, cx)));
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_query(rel.clone(), window, cx)));
        draw(cx);
        let id = cx.update(|_, cx| {
            let w = ws.read(cx);
            let opened: Vec<_> = w
                .tabs
                .tabs
                .iter()
                .filter(|t| t.path.as_ref().is_some_and(|p| p.ends_with(&rel)))
                .collect();
            assert_eq!(opened.len(), 1, "이미 열린 파일은 그 탭으로 간다");
            assert_eq!(opened[0].conn, conn);
            opened[0].id
        });
        let file = dir.path().join(&rel);
        // 편집기 글을 바꾸고 자동 저장
        cx.update(|window, cx| {
            ws.update(cx, |w, cx| {
                if let Some(Body::Sql { editor, .. }) = w.bodies.get(&id) {
                    editor.update(cx, |e, cx| e.set_value("select 42", window, cx));
                }
                w.autosave(id, cx);
            })
        });
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "select 42");
        // 밖에서 바꾸면 멈춘다
        std::fs::write(&file, "-- 다른 편집기").unwrap();
        cx.update(|window, cx| {
            ws.update(cx, |w, cx| {
                if let Some(Body::Sql { editor, .. }) = w.bodies.get(&id) {
                    editor.update(cx, |e, cx| e.set_value("select 43", window, cx));
                }
                w.autosave(id, cx);
            })
        });
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "-- 다른 편집기", "밖의 바뀜을 덮지 않는다");
        cx.update(|_, cx| assert!(ws.read(cx).autosave_stopped.contains_key(&id)));
    }

    #[gpui::test]
    fn 원격이면_쿼리_목록이_없다(cx: &mut TestAppContext) {
        let (ws, cx) = setup(cx);
        cx.update(|_, cx| {
            ws.update(cx, |w, _| {
                w.target = Some(Target::Remote("devbox".into()));
                w.refresh_queries();
                assert!(w.queries.is_none());
            })
        });
    }

    #[gpui::test]
    fn 테이블_보기_WHERE_ORDER_를_적용하면_기록에_남긴다(cx: &mut TestAppContext) {
        let (ws, cx) = setup(cx);
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_target(Target::Local(offline_project()), window, cx)));
        wait(cx, &ws, |w| w.engine.is_some());
        let rs: ResultSet =
            serde_json::from_value(serde_json::json!({"cols": ["id"], "rows": [["1"]], "text": "", "truncated": false, "elapsed_ms": 1}))
                .unwrap();
        let src = Source::Table {
            schema: "public".into(),
            table: "users".into(),
            filter: String::new(),
            order: String::new(),
        };
        let r = cx.update(|window, cx| {
            ws.update(cx, |w, cx| {
                w.prefs.where_history.clear();
                w.prefs.order_history.clear();
                let id = w.tabs.tabs[0].id;
                w.show_result(id, "app-stg".into(), src, rs, false, window, cx);
                match w.bodies.get(&id) {
                    Some(Body::Sql { result: Some(r), .. }) => r.clone(),
                    _ => panic!("결과 없음"),
                }
            })
        });
        cx.update(|window, cx| {
            let (wi, oi) = r.read(cx).table_inputs.clone().expect("테이블 보기면 입력칸이 있다");
            wi.update(cx, |i, cx| i.set_value("id > 1", window, cx));
            oi.update(cx, |i, cx| i.set_value("id desc", window, cx));
            r.update(cx, |v, cx| v.apply_table_query(window, cx));
        });
        draw(cx);
        cx.update(|_, cx| {
            let w = ws.read(cx);
            assert_eq!(w.prefs.where_history, vec!["id > 1"]);
            assert_eq!(w.prefs.order_history, vec!["id desc"]);
        });
    }

    #[gpui::test]
    fn 복호화_보기는_암호문_칸만_평문으로_바꾸고_끄면_돌아온다(cx: &mut TestAppContext) {
        let (ws, cx) = setup(cx);
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_target(Target::Local(offline_project()), window, cx)));
        wait(cx, &ws, |w| w.engine.is_some());
        let enc = "000102030405060708090a0b0c0d0e0f^|^00112233445566778899aabbccddeeff";
        let rs: ResultSet = serde_json::from_value(
            serde_json::json!({"cols": ["email", "name"], "rows": [[enc, "kim"]], "text": "", "truncated": false, "elapsed_ms": 1}),
        )
        .unwrap();
        let r = cx.update(|window, cx| {
            ws.update(cx, |w, cx| {
                let id = w.tabs.tabs[0].id;
                w.show_result(
                    id,
                    "app-stg".into(),
                    Source::Query { sql: "select 1".into() },
                    rs,
                    false,
                    window,
                    cx,
                );
                match w.bodies.get(&id) {
                    Some(Body::Sql { result: Some(r), .. }) => r.clone(),
                    _ => panic!("결과 없음"),
                }
            })
        });
        cx.update(|window, cx| {
            r.update(cx, |v, cx| {
                v.set_decrypted(Some([(enc.to_string(), "a@b.com".to_string())].into()), cx)
            });
            assert_eq!(r.read(cx).shown_cell(0, 0, cx), "a@b.com");
            assert_eq!(r.read(cx).shown_cell(0, 1, cx), "kim");
            // 켜져 있으면 끈다
            r.update(cx, |v, cx| v.toggle_decrypt(window, cx));
            assert_eq!(r.read(cx).shown_cell(0, 0, cx), enc);
            // 끈 상태에서 켜면 serve 에 묻는다. 오프라인 프로젝트엔 그룹 키가 없어 실패하고 보기는 그대로다
            r.update(cx, |v, cx| v.toggle_decrypt(window, cx));
        });
        draw(cx);
        wait(cx, &ws, |w| w.busy.is_none());
        cx.update(|_, cx| {
            assert_eq!(r.read(cx).shown_cell(0, 0, cx), enc);
            assert!(ws.read(cx).status.1, "{:?}", ws.read(cx).status);
        });
    }

    #[gpui::test]
    fn 전체_내보내기가_실패하면_파일을_만들지_않는다(cx: &mut TestAppContext) {
        let (ws, cx) = setup(cx);
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_target(Target::Local(offline_project()), window, cx)));
        wait(cx, &ws, |w| w.engine.is_some());
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("x.csv");
        let o2 = out.clone();
        cx.update(|window, cx| {
            ws.update(cx, |w, cx| {
                super::super::results::run_export(w, "app-stg".into(), "select 1".into(), o2, 0, window, cx)
            })
        });
        wait(cx, &ws, |w| w.busy.is_none());
        cx.update(|_, cx| assert!(ws.read(cx).status.1));
        assert!(!out.exists(), "시작이 실패하면 파일을 만들지 않는다");
        // 데이터베이스를 바꾼 뒤(세대가 다르면)는 보내지도 않는다
        cx.update(|window, cx| {
            ws.update(cx, |w, cx| {
                w.on_db_switched("app-stg", cx);
                super::super::results::run_export(w, "app-stg".into(), "select 1".into(), dir.path().join("y.csv"), 0, window, cx);
                assert!(w.busy.is_none());
                assert!(w.status.0.contains("데이터베이스가 바뀌어"), "{:?}", w.status);
            })
        });
    }

    /// 리뷰 3: 모두 실행 중 탭을 바꾸면 남은 결과가 새 탭에 붙었다.
    #[gpui::test]
    fn 모두_실행_사슬은_시작한_탭과_접속이_그대로일_때만_이어진다(cx: &mut TestAppContext) {
        let (ws, cx) = setup(cx);
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_target(Target::Local(offline_project()), window, cx)));
        wait(cx, &ws, |w| w.engine.is_some());
        cx.update(|window, cx| {
            ws.update(cx, |w, cx| {
                w.tabs.tabs[0].conn = Some("app-stg".into());
                let id = w.tabs.tabs[0].id;
                w.new_tab(Some("app-prod".into()), window, cx);
                // 다른 탭을 골라도 사슬은 시작한 탭을 따른다
                assert!(w.chain_alive(id, "app-stg", 0));
                w.tabs.tabs[0].conn = Some("app-prod".into());
                assert!(!w.chain_alive(id, "app-stg", 0), "접속이 바뀌면 멈춘다");
                w.tabs.tabs[0].conn = Some("app-stg".into());
                w.on_db_switched("app-stg", cx);
                assert!(!w.chain_alive(id, "app-stg", 0), "데이터베이스가 바뀌면 멈춘다");
                assert!(w.chain_alive(id, "app-stg", 1));
                w.close_tab(0, cx);
                assert!(!w.chain_alive(id, "app-stg", 1), "탭을 닫으면 멈춘다");
            })
        });
    }

    #[gpui::test]
    fn 없는_폴더는_오류를_상태_줄에_남기고_고르는_화면에_머문다(cx: &mut TestAppContext) {
        let (ws, cx) = setup(cx);
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_target(Target::Local("/nonexistent/i-db".into()), window, cx)));
        wait(cx, &ws, |w| !w.connecting);
        cx.update(|_, cx| {
            let w = ws.read(cx);
            assert!(w.engine.is_none());
            assert!(w.status.1, "{:?}", w.status);
        });
    }
}
