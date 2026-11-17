//! 창 하나의 작업 공간. 접속 전에는 고르는 화면, 접속 뒤에는 접속 트리·탭·편집기·결과·상태 줄이다.
//! 엔진 요청은 모두 `guarded` 를 거쳐 거절 사유(prod 잠금·확인)를 확인 창으로 바꾸고 다시 보낸다.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use super::kit;
use crate::vm::brand;
use crate::vm::palette;
use gpui::{
    AppContext as _, Context, Entity, FocusHandle, Focusable, InteractiveElement as _, IntoElement, ParentElement as _, PathPromptOptions,
    Render, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use gpui::{ClickEvent, ClipboardItem, MouseButton, rems};
use gpui_component::Disableable as _;
use gpui_component::TitleBar;
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
use crate::vm::tabs::{TabFile, Tabs, target_sql};
use crate::vm::tree::{Child, ChildKind, ConnTree, LoadReq, RowKind};
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
    /// 프로젝트 `queries/` 파일(R43). 로컬·원격 모두 엔진 query_list 로 읽는다. None 이면 읽는 중이다
    queries: Option<Vec<QueryFile>>,
    queries_err: Option<String>,
    /// 쿼리 파일 저장이 도는 탭. 한 탭은 한 번에 하나만 보내고 그사이 고침은 queued 에 마지막 글로 모은다
    saving: HashSet<u64>,
    queued: HashMap<u64, QueuedSave>,
    /// 끝나지 않은 쿼리 파일 쓰기(저장·새 쿼리·충돌 사본). 닫을 때 이것이 0 이 되길 20초까지 기다린다
    pub(super) pending_saves: usize,
    /// 저장을 기다린 뒤라 창을 그냥 닫아도 된다
    close_ready: bool,
    /// 파일 탭마다 마지막으로 읽거나 쓴 내용(R45.2 자동 저장이 밖의 바뀜을 가른다)
    file_text: HashMap<u64, String>,
    /// 자동 저장을 멈춘 탭과 까닭
    autosave_stopped: HashMap<u64, String>,
    autosave_tasks: HashMap<u64, gpui::Task<()>>,
    /// `DB_AUTO_SAVE=0` 이면 끈다(첫 실행 저장·1초 뒤·탭 옮김·닫기). ⌘S 는 그대로다
    pub(super) autosave_on: bool,
    /// 엔진(프로젝트 접속) 세대. 열고 닫을 때마다 오른다. db_gen 은 다시 열면 비워져 0 이 겹치므로,
    /// 트리·카탈로그 답은 이 값까지 맞아야 받는다(다시 연 엔진에 옛 답이 앉지 않게)
    pub(super) session: u64,
    /// 데이터베이스 바꾸기 요청이 도는 접속. 원격이면 엔진은 이미 바꿨는데 답이 안 왔을 수 있어
    /// 그동안 그 접속 결과의 고치기·반영·되돌리기·다음 쪽을 막는다
    switching: std::collections::HashSet<String>,
    /// 접속마다 데이터베이스를 바꾼 횟수. 요청은 보낼 때 값을 들고 가고, 답이 올 때 다르면 버린다(R27.1)
    db_gen: HashMap<String, u64>,
    /// 트리에서 고른 줄(접속 이름 또는 자식 id)
    selected_node: Option<String>,
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
        // 창을 닫거나 앱을 끝낼 때 남은 쿼리 파일 저장을 기다린다(20초까지)
        let weak = cx.entity().downgrade();
        window.on_window_should_close(cx, {
            let weak = weak.clone();
            move |window, cx| weak.update(cx, |ws, cx| ws.should_close(window, cx)).unwrap_or(true)
        });
        let mut open = cx.try_global::<OpenWorkspaces>().map(|o| o.0.clone()).unwrap_or_default();
        open.retain(|w| w.upgrade().is_some());
        open.push(weak);
        cx.set_global(OpenWorkspaces(open));
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
            session: 0,
            switching: Default::default(),
            queries: None,
            queries_err: None,
            saving: HashSet::new(),
            queued: HashMap::new(),
            pending_saves: 0,
            close_ready: false,
            file_text: HashMap::new(),
            autosave_stopped: HashMap::new(),
            autosave_tasks: HashMap::new(),
            autosave_on: queries::autosave_enabled(std::env::var("DB_AUTO_SAVE").ok().as_deref()),
            selected_node: None,
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

    /// 화면 눈금(확대 배율 × 설정 글자 크기)
    pub(super) fn scale(&self) -> crate::vm::layout::Scale {
        crate::vm::layout::Scale::with_font(self.prefs.zoom, self.font.size)
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
        self.session += 1;
        self.prefs.touch(target.clone());
        self.save_prefs();
        let warn = info.warnings.join("; ");
        // 옛 판이 남긴 prod 접속 기록을 지운다(R75.5)
        let prod_keys: Vec<String> = info
            .conns
            .iter()
            .filter(|c| tone(&c.env) == Tone::Prod)
            .flat_map(|c| [c.uuid.clone(), c.name.clone()])
            .filter(|k| !k.is_empty())
            .collect();
        if crate::vm::history::purge_prod(&mut self.prefs.table_history, &prod_keys) {
            self.save_prefs();
        }
        self.tree = ConnTree::new(info.conns);
        // prod 접속의 Redis 이름공간 펼침은 남기지 않는다. 옛 판이 남긴 것도 여기서 지운다
        let prod: Vec<String> = self
            .tree
            .conns
            .iter()
            .filter(|c| tone(&c.env) == Tone::Prod)
            .map(|c| c.name.clone())
            .collect();
        let key = target.label();
        if let Some(m) = self.prefs.tree_memory.get_mut(&key) {
            let kept = ConnTree::persistable(&m.expanded, &prod);
            if kept.len() != m.expanded.len() {
                m.expanded = kept;
                self.save_prefs();
            }
        }
        if let Some(m) = self.prefs.tree_memory.get(&key) {
            self.tree.restore(&m.collapsed, &m.expanded);
        }
        self.engine = Some(engine);
        self.target = Some(target);
        self.refresh_queries(cx);
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
        // 닫기 전에 저장을 보내고, 엔진은 그 쓰기가 끝난 뒤(20초까지) 닫는다
        self.flush_saves(cx);
        self.session += 1;
        if let Some(e) = self.engine.take() {
            let rt = self.rt.clone();
            let wait = self.wait_saves(cx);
            cx.spawn(async move |_, _| {
                wait.await;
                rt.spawn(async move { e.close().await });
            })
            .detach();
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
        self.queries_err = None;
        self.queued.clear();
        self.file_text.clear();
        self.autosave_stopped.clear();
        self.autosave_tasks.clear();
        self.selected_node = None;
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
                if let Some(i) =
                    self.tabs.tabs.iter().position(|t| {
                        t.conn.as_deref() == Some(name.as_str()) && matches!(self.bodies.get(&t.id), Some(Body::Redis { .. }))
                    })
                {
                    self.tabs.activate(i);
                    cx.notify();
                    return;
                }
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

    /// 테이블 보기 기록 자리와 prod 여부다(R75.5: prod 기록은 남기지 않는다).
    pub(super) fn history_key(&self, conn: &str, schema: &str, table: &str) -> (String, bool) {
        let c = self.conn(conn);
        let conn_key = c
            .map(|c| if c.uuid.is_empty() { c.name.clone() } else { c.uuid.clone() })
            .unwrap_or_else(|| conn.to_string());
        let prod = c.is_none_or(|c| tone(&c.env) == Tone::Prod);
        (crate::vm::history::table_key(&conn_key, schema, table), prod)
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

    /// 답이 지금 엔진·데이터베이스의 것인지(보낼 때 잡은 세대와 같은지)
    pub(super) fn fresh(&self, conn: &str, session: u64, db_gen: u64) -> bool {
        self.session == session && self.db_gen(conn) == db_gen
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
        // 트리도 옛 데이터베이스 것이라 버린다(펼쳐 있으면 다음에 펼칠 때 새로 읽는다)
        self.tree.forget_conn(conn);
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
        self.tree_reload_conn(conn, window, cx);
    }

    // ─── 접속 트리(펼쳐 읽기) ───

    /// 펼치거나 접는다. 처음 펼치면 읽는다(탭이 없어도 된다).
    pub(super) fn tree_toggle(&mut self, node: String, window: &mut Window, cx: &mut Context<Self>) {
        let req = self.tree.toggle_node(&node);
        self.save_tree_memory();
        if let Some(r) = req {
            self.tree_dispatch(r, window, cx);
        }
        cx.notify();
    }

    pub(super) fn tree_retry(&mut self, node: String, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(r) = self.tree.retry(&node) {
            self.tree_dispatch(r, window, cx);
        }
        cx.notify();
    }

    /// 접속의 읽은 것을 버리고, 펼쳐 있으면 다시 읽는다(새로 고침·데이터베이스 바꿈).
    fn tree_reload_conn(&mut self, conn: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.tree.forget_conn(conn);
        if self.tree.is_expanded(conn)
            && let Some(r) = self.tree.retry(conn)
        {
            self.tree_dispatch(r, window, cx);
        }
    }

    /// 트리 읽기를 보낸다. 메타데이터만 읽어 확인 창·요청 칸(busy)을 쓰지 않고, 실패는 그 자리에 오류 줄로 남긴다.
    /// 그사이 데이터베이스를 바꿨으면 답을 버린다.
    fn tree_dispatch(&mut self, req: LoadReq, window: &mut Window, cx: &mut Context<Self>) {
        let Some(engine) = self.engine.clone() else { return };
        let conn = match &req {
            LoadReq::Catalog { conn } | LoadReq::RedisScan { conn } | LoadReq::SourceChildren { conn, .. } => conn.clone(),
        };
        let (gen0, s0) = (self.db_gen(&conn), self.session);
        let job = self.rt.spawn({
            let req = req.clone();
            async move {
                match req {
                    LoadReq::Catalog { conn } => engine.catalog(&conn).await.map(TreeData::Catalog),
                    LoadReq::RedisScan { conn } => engine.redis_scan(&conn, "*").await.map(TreeData::Keys),
                    LoadReq::SourceChildren { conn, path, .. } => engine.source_children(&conn, &path).await.map(TreeData::Nodes),
                }
            }
        });
        cx.spawn_in(window, async move |this, cx| {
            let r = job.rx.await;
            let _ = this.update_in(cx, move |ws, window, cx| {
                if !ws.fresh(&conn, s0, gen0) {
                    return;
                }
                let node = match &req {
                    LoadReq::Catalog { conn } | LoadReq::RedisScan { conn } => conn.clone(),
                    LoadReq::SourceChildren { node, .. } => node.clone(),
                };
                let more = match r {
                    Ok(Ok(TreeData::Catalog(cat))) => {
                        let more = ws.tree.set_catalog(&conn, &cat);
                        // 자동 완성도 같은 카탈로그를 쓴다
                        ws.catalogs.insert(conn.clone(), Rc::new(cat));
                        ws.sync_catalog_cells();
                        more
                    }
                    Ok(Ok(TreeData::Keys(s))) => ws.tree.set_redis_keys(&conn, s.keys, s.more),
                    Ok(Ok(TreeData::Nodes(n))) => ws.tree.set_source_children(&node, n),
                    Ok(Err(e)) => {
                        ws.tree.set_failed(&node, e.error);
                        Vec::new()
                    }
                    Err(_) => {
                        ws.tree.set_failed(&node, "멈춤".into());
                        Vec::new()
                    }
                };
                for r in more {
                    ws.tree_dispatch(r, window, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 자식 줄을 눌렀다. 묶음은 펼치고, 테이블은 두 번 눌러 열고, Redis 키·저장소 항목은 그 탭에서 연다.
    pub(super) fn tree_child_click(&mut self, conn: String, child: Child, clicks: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.selected_node = Some(child.id.clone());
        match child.kind {
            ChildKind::Schema
            | ChildKind::RedisNs { .. }
            | ChildKind::Source {
                has_children: true,
                openable: false,
                ..
            } => self.tree_toggle(child.id, window, cx),
            ChildKind::Table | ChildKind::View if clicks >= 2 => self.open_table(conn, child.path, child.label, window, cx),
            ChildKind::RedisKey => self.open_redis_key(conn, child.path, window, cx),
            ChildKind::Source { openable: true, .. } => self.open_source_path(conn, child.path, window, cx),
            _ => {}
        }
        cx.notify();
    }

    fn open_redis_key(&mut self, conn: String, key: String, window: &mut Window, cx: &mut Context<Self>) {
        self.select_conn(conn.clone(), window, cx);
        if let Some(Body::Redis { view }) = self.tabs.active().and_then(|t| self.bodies.get(&t.id)) {
            let view = view.clone();
            window.defer(cx, move |window, cx| view.update(cx, |v, cx| v.open_key(key, window, cx)));
        }
    }

    fn open_source_path(&mut self, conn: String, path: String, window: &mut Window, cx: &mut Context<Self>) {
        self.select_conn(conn, window, cx);
        if let Some(Body::Source { view }) = self.tabs.active().and_then(|t| self.bodies.get(&t.id)) {
            let view = view.clone();
            window.defer(cx, move |window, cx| view.update(cx, |v, cx| v.open(path, window, cx)));
        }
    }

    /// 트리 펼침을 프로젝트마다 기억한다(R75.6).
    fn save_tree_memory(&mut self) {
        let Some(t) = &self.target else { return };
        let (collapsed, expanded) = self.tree.memory();
        let prod: Vec<String> = self
            .tree
            .conns
            .iter()
            .filter(|c| tone(&c.env) == Tone::Prod)
            .map(|c| c.name.clone())
            .collect();
        let expanded = ConnTree::persistable(&expanded, &prod);
        self.prefs
            .tree_memory
            .insert(t.label(), crate::vm::runtime::TreeMemory { collapsed, expanded });
        self.save_prefs();
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
        let (gen0, s0) = (self.db_gen(conn), self.session);
        self.guarded(
            "카탈로그",
            Rc::new(move |e: Engine, _| {
                let n = n.clone();
                async move { e.catalog(&n).await }
            }),
            Rc::new(move |ws: &mut Workspace, cat: Catalog, _, cx| {
                // 바꾸기 전 데이터베이스·다시 열기 전 엔진의 카탈로그면 버린다
                if !ws.fresh(&name, s0, gen0) {
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
        // 닫기 전에 자동 저장하고(R45.2), 덮을 수 없는 글은 충돌 사본에 남긴다(R45.3)
        if let Some(id) = self.tabs.tabs.get(i).map(|t| t.id) {
            self.autosave(id, cx);
            self.keep_stopped_text(id, cx);
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
    // 로컬이든 원격이든 엔진 op(query_*)로만 읽고 쓴다. 원격이면 serve 쪽 프로젝트 파일이고 경계는 엔진이 지킨다.
    // 원격이 느려도 화면이 굳지 않게 모두 런타임에 띄우고 답이 오면 반영한다.

    fn project_dir(&self) -> Option<PathBuf> {
        match &self.target {
            Some(Target::Local(d)) => Some(std::path::absolute(d).unwrap_or_else(|_| d.clone())),
            _ => None,
        }
    }

    /// queries/ 목록을 엔진에 다시 묻는다. 다른 프로젝트로 옮긴 뒤 온 답은 버린다.
    pub(super) fn refresh_queries(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.engine.clone() else {
            self.queries = None;
            return;
        };
        let s0 = self.session;
        let job = self.rt.spawn(async move { queries::list(&engine).await });
        cx.spawn(async move |this, cx| {
            let r = job.rx.await;
            let _ = this.update(cx, |ws, cx| {
                if ws.session != s0 {
                    return;
                }
                match r {
                    Ok(Ok(v)) => {
                        ws.queries = Some(v);
                        ws.queries_err = None;
                    }
                    Ok(Err(e)) => ws.queries_err = Some(e),
                    Err(_) => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn query_tab(&self, rel: &str) -> Option<usize> {
        self.tabs
            .tabs
            .iter()
            .position(|t| t.file.as_ref().and_then(TabFile::query) == Some(rel))
    }

    /// 쿼리 파일을 연다. 이미 열린 탭이면 그 탭으로 간다. 붙은 접속이 있으면 그 접속으로 연다.
    pub(super) fn open_query(&mut self, rel: String, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(i) = self.query_tab(&rel) {
            self.tabs.activate(i);
            cx.notify();
            return;
        }
        let Some(engine) = self.engine.clone() else { return };
        let conn = self
            .queries
            .as_ref()
            .and_then(|q| q.iter().find(|f| f.rel == rel))
            .and_then(|f| f.conn.clone());
        let s0 = self.session;
        let job = self.rt.spawn({
            let rel = rel.clone();
            async move { queries::read(&engine, &rel).await }
        });
        cx.spawn_in(window, async move |this, cx| {
            let r = job.rx.await;
            let _ = this.update_in(cx, move |ws, window, cx| {
                if ws.session != s0 {
                    return;
                }
                // 기다리는 사이 같은 파일을 또 열었으면 그 탭으로 간다
                if let Some(i) = ws.query_tab(&rel) {
                    ws.tabs.activate(i);
                    cx.notify();
                    return;
                }
                match r {
                    Ok(Ok(text)) => {
                        let id = ws.tabs.open(conn.clone(), Some(TabFile::Query(rel)));
                        ws.file_text.insert(id, text.clone());
                        ws.insert_sql_body(id, text, window, cx);
                        if let Some(c) = conn {
                            ws.ensure_catalog(&c, window, cx);
                        }
                        ws.sync_catalog_cells();
                        cx.notify();
                    }
                    Ok(Err(e)) => ws.set_status(format!("열기 실패: {e}"), true, cx),
                    Err(_) => {}
                }
            });
        })
        .detach();
    }

    fn schedule_autosave(&mut self, id: u64, cx: &mut Context<Self>) {
        if !self.autosave_on {
            return;
        }
        let has_file = self.tabs.index_of(id).is_some_and(|i| self.tabs.tabs[i].file.is_some());
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

    fn editor_text(&self, id: u64, cx: &Context<Self>) -> Option<String> {
        match self.bodies.get(&id) {
            Some(Body::Sql { editor, .. }) => Some(editor.read(cx).value().to_string()),
            _ => None,
        }
    }

    /// 파일 탭을 저장한다. 쿼리 파일은 엔진이 마지막으로 읽거나 쓴 내용과 견줘 밖에서 바뀌었으면 쓰지 않고(conflict)
    /// 멈춘다. ⌘O 로 연 이 Mac 의 파일은 여기서 내용으로 가른다.
    pub(super) fn autosave(&mut self, id: u64, cx: &mut Context<Self>) {
        self.autosave_tasks.remove(&id);
        if !self.autosave_on || self.autosave_stopped.contains_key(&id) {
            return;
        }
        let Some(i) = self.tabs.index_of(id) else { return };
        let Some(file) = self.tabs.tabs[i].file.clone() else { return };
        let Some(last) = self.file_text.get(&id).cloned() else { return };
        let Some(text) = self.editor_text(id, cx) else { return };
        match file {
            TabFile::Query(rel) => {
                if text == last && !self.saving.contains(&id) {
                    self.tabs.tabs[i].dirty = false;
                    cx.notify();
                    return;
                }
                let conn = self.tabs.tabs[i].conn.clone().unwrap_or_default();
                self.send_save(
                    id,
                    QueuedSave {
                        rel,
                        text,
                        force: false,
                        conn,
                    },
                    cx,
                );
            }
            TabFile::Local(path) => {
                let disk = std::fs::read_to_string(&path).ok();
                match queries::before_autosave(disk.as_deref(), &last, &text) {
                    queries::SaveDecision::Skip => {
                        self.file_text.insert(id, text);
                        self.tabs.tabs[i].dirty = false;
                    }
                    queries::SaveDecision::Write => match queries::write_local(&path, &text) {
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
        }
    }

    /// 쿼리 파일 쓰기를 보낸다. 그 탭의 앞 쓰기가 도는 중이면 끝난 뒤 마지막 글로 한 번 더 쓴다(⌘S 는 덮기를 지킨다).
    fn send_save(&mut self, id: u64, q: QueuedSave, cx: &mut Context<Self>) {
        if self.saving.contains(&id) {
            let force = q.force || self.queued.get(&id).is_some_and(|p| p.force);
            self.queued.insert(id, QueuedSave { force, ..q });
            return;
        }
        let Some(engine) = self.engine.clone() else { return };
        let disk = self.file_text.get(&id).cloned();
        self.saving.insert(id);
        self.pending_saves += 1;
        let s0 = self.session;
        let job = self.rt.spawn({
            let (rel, text, force) = (q.rel.clone(), q.text.clone(), q.force);
            async move { queries::write(&engine, &rel, &text, disk.as_deref(), force).await }
        });
        cx.spawn(async move |this, cx| {
            let r = job.rx.await.unwrap_or_else(|_| Err("멈춤".into()));
            let _ = this.update(cx, |ws, cx| ws.save_done(id, s0, q, r, cx));
        })
        .detach();
        cx.notify();
    }

    fn save_done(&mut self, id: u64, s0: u64, q: QueuedSave, r: Result<idb_engine::QueryWritten, String>, cx: &mut Context<Self>) {
        self.pending_saves = self.pending_saves.saturating_sub(1);
        self.saving.remove(&id);
        if self.session != s0 {
            cx.notify();
            return;
        }
        let open = self.tabs.index_of(id);
        match r {
            Ok(w) if w.conflict => {
                self.autosave_stopped.insert(id, queries::AUTOSAVE_STOPPED.into());
                self.set_status(queries::AUTOSAVE_STOPPED, true, cx);
                // 닫으며 보낸 저장이 막혔으면 글을 사본에 남긴다(R45.3)
                if open.is_none() {
                    self.conflict_copy(q.rel.clone(), q.text.clone(), q.conn.clone(), cx);
                }
            }
            Ok(w) => {
                self.file_text.insert(id, q.text.clone());
                if let Some(i) = open {
                    let now = self.editor_text(id, cx);
                    self.tabs.tabs[i].dirty = now.as_deref() != Some(q.text.as_str());
                }
                if q.force {
                    self.autosave_stopped.remove(&id);
                    self.set_status(queries::saved_note(&w), false, cx);
                }
                if q.force || w.created {
                    self.refresh_queries(cx);
                }
            }
            Err(e) => {
                let what = if q.force { "저장" } else { "자동 저장" };
                self.set_status(format!("{what} 실패: {e}"), true, cx);
            }
        }
        if let Some(next) = self.queued.remove(&id) {
            if next.force || !self.autosave_stopped.contains_key(&id) {
                self.send_save(id, next, cx);
            } else if open.is_none() {
                self.conflict_copy(next.rel, next.text, next.conn, cx);
            }
        }
        cx.notify();
    }

    /// 자동 저장이 멈춘(밖에서 바뀐) 쿼리 파일 탭의 글이 파일과 다르면 충돌 사본에 남긴다(R45.3). 닫을 때 부른다.
    fn keep_stopped_text(&mut self, id: u64, cx: &mut Context<Self>) {
        if !self.autosave_on || !self.autosave_stopped.contains_key(&id) || self.saving.contains(&id) {
            return;
        }
        let Some(i) = self.tabs.index_of(id) else { return };
        let Some(TabFile::Query(rel)) = self.tabs.tabs[i].file.clone() else {
            return;
        };
        let Some(text) = self.editor_text(id, cx) else { return };
        if text.trim().is_empty() || self.file_text.get(&id) == Some(&text) {
            return;
        }
        let conn = self.tabs.tabs[i].conn.clone().unwrap_or_default();
        // 같은 글을 두 번 남기지 않는다
        self.file_text.insert(id, text.clone());
        self.conflict_copy(rel, text, conn, cx);
    }

    fn conflict_copy(&mut self, rel: String, text: String, conn: String, cx: &mut Context<Self>) {
        let Some(engine) = self.engine.clone() else { return };
        self.pending_saves += 1;
        let s0 = self.session;
        let job = self
            .rt
            .spawn(async move { queries::conflict_copy(&engine, &rel, &text, &conn).await });
        cx.spawn(async move |this, cx| {
            let r = job.rx.await.unwrap_or_else(|_| Err("멈춤".into()));
            let _ = this.update(cx, |ws, cx| {
                ws.pending_saves = ws.pending_saves.saturating_sub(1);
                if ws.session != s0 {
                    return;
                }
                match r {
                    Ok(w) => {
                        let note = format!("저장 안 한 글을 {} 에 남김", w.path);
                        ws.set_status(note, false, cx);
                        ws.refresh_queries(cx);
                    }
                    Err(e) => ws.set_status(format!("충돌 사본을 남기지 못함: {e}"), true, cx),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 파일 없는 탭의 첫 실행이면 엔진이 queries/<그룹>/<접속>-<시각>.sql 로 저장하고 매핑에 붙인다(R45.1).
    fn save_new_query_on_run(&mut self, cx: &mut Context<Self>) {
        if !self.autosave_on {
            return;
        }
        let Some(t) = self.tabs.active().cloned() else { return };
        if t.file.is_some() || t.name.is_some() {
            return;
        }
        let Some(c) = t.conn.as_ref().and_then(|c| self.conn(c)).cloned() else {
            return;
        };
        if c.kind != "postgresql" {
            return;
        }
        self.create_query(t.id, c.name, cx);
    }

    /// 파일 없는 탭의 글을 엔진이 정한 새 쿼리 파일에 쓴다. 같은 탭에서 겹쳐 부르면 한 번만 만든다.
    fn create_query(&mut self, id: u64, conn: String, cx: &mut Context<Self>) {
        if self.saving.contains(&id) {
            return;
        }
        let Some(engine) = self.engine.clone() else { return };
        let Some(text) = self.editor_text(id, cx) else { return };
        if text.trim().is_empty() {
            return;
        }
        self.saving.insert(id);
        self.pending_saves += 1;
        let s0 = self.session;
        let job = self.rt.spawn({
            let text = text.clone();
            async move { queries::create(&engine, &conn, &text).await }
        });
        cx.spawn(async move |this, cx| {
            let r = job.rx.await.unwrap_or_else(|_| Err("멈춤".into()));
            let _ = this.update(cx, |ws, cx| {
                ws.pending_saves = ws.pending_saves.saturating_sub(1);
                ws.saving.remove(&id);
                if ws.session != s0 {
                    return;
                }
                match r {
                    Ok(w) => {
                        if let Some(i) = ws.tabs.index_of(id)
                            && ws.tabs.tabs[i].file.is_none()
                        {
                            ws.tabs.tabs[i].file = Some(TabFile::Query(w.path.clone()));
                            let now = ws.editor_text(id, cx);
                            ws.tabs.tabs[i].dirty = now.as_deref() != Some(text.as_str());
                            ws.file_text.insert(id, text);
                        }
                        ws.set_status(queries::saved_note(&w), !w.mapping_error.is_empty(), cx);
                        ws.refresh_queries(cx);
                    }
                    Err(e) => ws.set_status(format!("새 쿼리 저장 실패: {e}"), true, cx),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 열린 파일 탭을 모두 저장하고 덮을 수 없는 글은 사본에 남긴다. 창을 닫거나 끝내거나 접속을 닫기 전에 부른다.
    pub(super) fn flush_saves(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<u64> = self.tabs.tabs.iter().filter(|t| t.file.is_some()).map(|t| t.id).collect();
        for id in ids {
            self.autosave(id, cx);
            self.keep_stopped_text(id, cx);
        }
    }

    /// 쿼리 파일 쓰기가 다 끝나거나 20초가 지나면 끝나는 기다림이다(TUI 와 같은 한도).
    pub(super) fn wait_saves(&self, cx: &mut Context<Self>) -> gpui::Task<()> {
        let started = Instant::now();
        cx.spawn(async move |this, cx| {
            while started.elapsed() < queries::SAVE_TIMEOUT {
                let pending = this.update(cx, |ws, _| ws.pending_saves).unwrap_or(0);
                if pending == 0 {
                    return;
                }
                cx.background_executor().timer(Duration::from_millis(50)).await;
            }
        })
    }

    /// 창을 닫아도 되는지다. 저장이 남았으면 보내고 기다린 뒤 창을 닫는다(20초까지).
    pub(super) fn should_close(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.close_ready {
            return true;
        }
        self.flush_saves(cx);
        if self.pending_saves == 0 {
            return true;
        }
        self.set_status("저장 중… 끝나면 닫는다", false, cx);
        let wait = self.wait_saves(cx);
        cx.spawn_in(window, async move |this, cx| {
            wait.await;
            let _ = this.update_in(cx, |ws, window, _| {
                ws.close_ready = true;
                window.remove_window();
            });
        })
        .detach();
        false
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
                            .label("취소")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(Button::new("params-ok").primary().label("실행").on_click(move |_, window, cx| {
                        ok2(window, cx);
                        window.close_dialog(cx);
                    })),
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
                    let id = ws.tabs.open(conn, Some(TabFile::Local(path)));
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

    /// ⌘S. 쿼리 파일은 밖에서 바뀌었어도 덮고 자동 저장을 다시 켠다(R45.2). 파일 없는 탭은 원격이면 새 쿼리로,
    /// 로컬이면 고른 자리에 쓴다(프로젝트 queries/ 안이면 엔진으로 쓴다).
    fn save_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(t) = self.tabs.active().cloned() else { return };
        let Some(text) = self.editor_text(t.id, cx) else { return };
        let id = t.id;
        let conn = t.conn.clone().unwrap_or_default();
        match t.file {
            Some(TabFile::Query(rel)) => self.send_save(
                id,
                QueuedSave {
                    rel,
                    text,
                    force: true,
                    conn,
                },
                cx,
            ),
            Some(TabFile::Local(p)) => self.write_local_file(id, p, text, cx),
            None if matches!(self.target, Some(Target::Remote(_))) => match t.conn {
                Some(c) => self.create_query(id, c, cx),
                None => self.set_status("접속을 고르면 queries/ 에 새 쿼리로 저장한다 (⌘O 는 이 Mac 의 파일)", true, cx),
            },
            None => {
                let rx = cx.prompt_for_new_path(&self.queries_dir(), Some("query.sql"));
                cx.spawn_in(window, async move |this, cx| {
                    let Ok(Ok(Some(path))) = rx.await else { return };
                    let _ = this.update(cx, move |ws, cx| {
                        let rel = ws.project_dir().and_then(|d| project_query_rel(&d, &path));
                        match rel {
                            Some(rel) => {
                                if let Some(i) = ws.tabs.index_of(id) {
                                    ws.tabs.tabs[i].file = Some(TabFile::Query(rel.clone()));
                                }
                                ws.send_save(
                                    id,
                                    QueuedSave {
                                        rel,
                                        text,
                                        force: true,
                                        conn,
                                    },
                                    cx,
                                );
                            }
                            None => ws.write_local_file(id, path, text, cx),
                        }
                    });
                })
                .detach();
            }
        }
    }

    /// ⌘O 로 고른(또는 프로젝트 밖에 새로 고른) 이 Mac 의 파일을 쓴다.
    fn write_local_file(&mut self, id: u64, path: PathBuf, text: String, cx: &mut Context<Self>) {
        match queries::write_local(&path, &text) {
            Ok(()) => {
                if let Some(i) = self.tabs.index_of(id) {
                    self.tabs.tabs[i].dirty = false;
                    self.tabs.tabs[i].file = Some(TabFile::Local(path.clone()));
                }
                self.file_text.insert(id, text);
                self.autosave_stopped.remove(&id);
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
        let sc = self.scale();
        // Orca 첫 화면 문법: 가운데 마크·글자표·한 줄 소개, 아래 고를 것들은 둥근 줄(28px)
        let row = |id: (&'static str, usize), label: String, detail: &'static str| {
            h_flex()
                .id(id)
                .min_h(px(sc.row()))
                .py(px(sc.s4()))
                .px_3()
                .gap_2()
                .rounded(theme.radius)
                .cursor_pointer()
                .hover(|s| s.bg(theme.secondary))
                .child(div().flex_1().truncate().child(label))
                .child(div().text_xs().text_color(theme.muted_foreground).child(detail))
        };
        let header = |title: &'static str| {
            div()
                .px_3()
                .pt_4()
                .pb_1()
                .text_xs()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme.muted_foreground)
                .child(title)
        };
        let mut remotes = v_flex().gap_px();
        match &self.remotes {
            Ok(names) if names.is_empty() => {
                remotes = remotes.child(div().px_3().text_xs().text_color(theme.muted_foreground).child(picker::NO_REMOTES));
            }
            Ok(names) => {
                for (i, c) in picker::remote_choices(names).into_iter().enumerate() {
                    let t = c.target;
                    let name = match &t {
                        Target::Remote(n) => n.clone(),
                        Target::Local(p) => p.display().to_string(),
                    };
                    remotes = remotes
                        .child(row(("remote", i), name, "ssh").on_click(cx.listener(move |ws, _, w, cx| ws.open_target(t.clone(), w, cx))));
                }
            }
            Err(e) => remotes = remotes.child(div().px_3().text_xs().text_color(theme.danger).child(e.clone())),
        }
        let mut recent = v_flex().gap_px();
        for (i, t) in self.prefs.recent.iter().enumerate() {
            let t2 = t.clone();
            let detail = if matches!(t, Target::Remote(_)) { "원격" } else { "로컬" };
            recent = recent
                .child(row(("recent", i), t.label(), detail).on_click(cx.listener(move |ws, _, w, cx| ws.open_target(t2.clone(), w, cx))));
        }
        v_flex()
            .id("picker")
            .size_full()
            .overflow_y_scroll()
            .items_center()
            .justify_center()
            .gap_6()
            .py_8()
            .child(
                v_flex()
                    .items_center()
                    .gap_3()
                    .child(super::brand::mark(px(64.), theme.foreground, super::brand::accent()))
                    .child(super::brand::wordmark(px(40.), theme.foreground))
                    .child(div().text_color(theme.muted_foreground).child(brand::TAGLINE)),
            )
            .child(
                v_flex()
                    .w(px(520.))
                    .child(header(picker::REMOTE_SECTION))
                    .child(remotes)
                    .child(header(picker::LOCAL_SECTION))
                    .child(
                        h_flex()
                            .px_3()
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
                        d.child(header(picker::RECENT_SECTION)).child(recent)
                    }),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(if self.status.1 { theme.danger } else { theme.muted_foreground })
                    .child(if self.status.0.is_empty() {
                        format!("{} · ⌘, 설정 · ⌘/ 단축키", brand::version())
                    } else {
                        self.status.0.clone()
                    }),
            )
    }

    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let dark = theme.is_dark();
        let active = self.active_conn();
        let sc = self.scale();
        let mut list = v_flex().id("conn-tree").py(px(sc.s4())).gap(px(sc.row_gap()));
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
            let el =
                match row.kind {
                    RowKind::Folder { expanded, count } => {
                        let label = row.label.clone();
                        // 묶음 머리: 12px 굵게, 펼침 화살표, 오른쪽 개수 알약. 묶음 사이 위 여백 12
                        kit::row(&sc, &theme, false)
                            .id(("folder", i))
                            .mx(px(sc.s4()))
                            .when(i > 0, |d| d.mt(px(sc.s12())))
                            .cursor_pointer()
                            .hover(|s| s.bg(theme.sidebar_accent))
                            .text_size(px(sc.group()))
                            .line_height(px(sc.line(sc.group())))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(if row.tone == Tone::Prod {
                                theme.danger
                            } else {
                                theme.muted_foreground
                            })
                            .debug_selector(move || format!("folder-row-{i}"))
                            .child(div().flex_shrink_0().size(px(sc.icon())).child(
                                Icon::new(if expanded { IconName::ChevronDown } else { IconName::ChevronRight }).size(px(sc.icon())),
                            ))
                            .child(div().flex_1().min_w_0().truncate().child(row.label))
                            .child(kit::pill(&sc, theme.secondary, theme.muted_foreground, count.to_string()))
                            .on_click(cx.listener(move |ws, _, _, cx| {
                                ws.tree.toggle(&label);
                                ws.save_tree_memory();
                                cx.notify();
                            }))
                            .into_any_element()
                    }
                    RowKind::Conn { index: ci, expanded } => {
                        let c = self.tree.conns[ci].clone();
                        let selected = self.selected_node.as_deref() == Some(c.name.as_str()) || active.as_deref() == Some(c.name.as_str());
                        let (n_toggle, n_click) = (c.name.clone(), c.name.clone());
                        let is_pg = c.kind == "postgresql";
                        let (n_open, n_ref, n_copy) = (c.name.clone(), c.name.clone(), c.name.clone());
                        let tip = format!("{} · {} {}\n{}", c.kind, c.host, c.db, c.group);
                        let ws = cx.entity().downgrade();
                        let (pbg, pfg) = palette::pill(tone(&c.env), dark);
                        let name_color = if tone(&c.env) == Tone::Prod {
                            theme.danger
                        } else {
                            theme.foreground
                        };
                        // 접속 줄: 아이콘·이름(14 중간 굵기)·환경 알약이 한 줄, 아래 12px 호스트 줄. 높이는 내용에서 셈한다.
                        // 사이드바가 좁으면 둘째 줄을 빼고 툴팁에만 둔다
                        let two_line = self.prefs.sidebar_width >= NARROW_SIDEBAR;
                        v_flex()
                            .id(("conn", i))
                            .mx(px(sc.s4()))
                            .min_h(px(if two_line { sc.row2() } else { sc.row() }))
                            .py(px(if two_line { sc.s8() } else { sc.s4() }))
                            .px(px(sc.s8()))
                            .gap(px(sc.line_gap()))
                            .justify_center()
                            .rounded(theme.radius)
                            .cursor_pointer()
                            .when(selected, |d| d.bg(theme.sidebar_accent))
                            .hover(|s| s.bg(theme.sidebar_accent))
                            .debug_selector(move || format!("conn-row-{i}"))
                            .child(
                                h_flex()
                                    .gap(px(sc.s8()))
                                    .debug_selector(move || format!("conn-line1-{i}"))
                                    .child(
                                        div()
                                            .id(("conn-chevron", i))
                                            .debug_selector(move || format!("conn-chevron-{i}"))
                                            .flex_shrink_0()
                                            .size(px(sc.icon()))
                                            .cursor_pointer()
                                            .child(
                                                Icon::new(if expanded { IconName::ChevronDown } else { IconName::ChevronRight })
                                                    .size(px(sc.icon())),
                                            )
                                            .on_click(cx.listener(move |ws, _, window, cx| {
                                                cx.stop_propagation();
                                                ws.tree_toggle(n_toggle.clone(), window, cx);
                                            })),
                                    )
                                    .child(kit::kind_icon(&sc, &c.kind, theme.muted_foreground))
                                    .child(
                                        kit::text(&sc, sc.text(), name_color, c.name.clone())
                                            .flex_1()
                                            .min_w_0()
                                            .font_weight(gpui::FontWeight::MEDIUM),
                                    )
                                    .child(kit::pill(&sc, hex(pbg), hex(pfg), c.env.clone())),
                            )
                            .when(two_line, |d| {
                                d.child(
                                    kit::text(&sc, sc.small(), theme.muted_foreground, row.detail.clone())
                                        .pl(px(2.0 * (sc.icon() + sc.s8())))
                                        .debug_selector(move || format!("conn-line2-{i}")),
                                )
                            })
                            .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                            // 한 번: 고른다(PG 는 지금 SQL 탭의 접속으로 삼는다). 두 번: 펼치거나 접는다
                            .on_click(cx.listener(move |ws, ev: &ClickEvent, window, cx| {
                                if ev.click_count() >= 2 {
                                    ws.tree_toggle(n_click.clone(), window, cx);
                                } else if is_pg {
                                    ws.select_conn(n_open.clone(), window, cx);
                                } else {
                                    ws.selected_node = Some(n_open.clone());
                                    cx.notify();
                                }
                            }))
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
                    RowKind::Child { conn: ci, child, expanded } => {
                        let conn = self.tree.conns[ci].name.clone();
                        let indent = sc.s8() + (row.depth.saturating_sub(1)) as f32 * sc.s12();
                        let selected = self.selected_node.as_deref() == Some(child.id.as_str());
                        let icon: gpui::AnyElement = match &child.kind {
                            ChildKind::Table => kit::kind_icon(&sc, "dynamodb", theme.muted_foreground).into_any_element(),
                            ChildKind::View => Icon::new(IconName::Eye)
                                .size(px(sc.icon()))
                                .text_color(theme.muted_foreground)
                                .into_any_element(),
                            ChildKind::Schema | ChildKind::RedisNs { .. } | ChildKind::Source { has_children: true, .. } => {
                                Icon::new(if expanded { IconName::FolderOpen } else { IconName::Folder })
                                    .size(px(sc.icon()))
                                    .text_color(theme.muted_foreground)
                                    .into_any_element()
                            }
                            ChildKind::RedisKey | ChildKind::Source { .. } => Icon::new(IconName::File)
                                .size(px(sc.icon()))
                                .text_color(theme.muted_foreground)
                                .into_any_element(),
                            ChildKind::More(_) => div().size(px(sc.icon())).into_any_element(),
                            ChildKind::Partial => Icon::new(IconName::TriangleAlert)
                                .size(px(sc.icon()))
                                .text_color(theme.warning)
                                .into_any_element(),
                        };
                        let expandable = child.expandable();
                        let (c_click, c_toggle) = (child.clone(), child.clone());
                        let (conn_click, conn_menu) = (conn.clone(), conn.clone());
                        let ws = cx.entity().downgrade();
                        let warn = matches!(child.kind, ChildKind::Source { warn: true, .. });
                        let is_table = matches!(child.kind, ChildKind::Table | ChildKind::View);
                        let menu_child = child.clone();
                        let base = kit::row(&sc, &theme, selected)
                            .id(("child", i))
                            .mx(px(sc.s4()))
                            .pl(px(indent))
                            .cursor_pointer()
                            .hover(|st| st.bg(theme.sidebar_accent))
                            .debug_selector(move || format!("child-row-{i}"))
                            .child(
                                div()
                                    .id(("child-chevron", i))
                                    .debug_selector(move || format!("child-chevron-{i}"))
                                    .flex_shrink_0()
                                    .size(px(sc.icon()))
                                    .when(expandable, |d| {
                                        d.child(
                                            Icon::new(if expanded { IconName::ChevronDown } else { IconName::ChevronRight })
                                                .size(px(sc.icon())),
                                        )
                                        .on_click(cx.listener(move |ws, _, window, cx| {
                                            cx.stop_propagation();
                                            ws.tree_toggle(c_toggle.id.clone(), window, cx);
                                        }))
                                    }),
                            )
                            .child(div().flex_shrink_0().size(px(sc.icon())).child(icon))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .when(matches!(child.kind, ChildKind::More(_)), |d| d.text_color(theme.muted_foreground))
                                    .when(child.kind == ChildKind::Partial, |d| d.text_color(theme.warning))
                                    .child(child.label.clone()),
                            )
                            .when(!child.info.is_empty(), |d| {
                                d.child(
                                    kit::text(
                                        &sc,
                                        sc.small(),
                                        if warn { theme.warning } else { theme.muted_foreground },
                                        child.info.clone(),
                                    )
                                    .flex_shrink_0(),
                                )
                            })
                            .on_click(cx.listener(move |ws, ev: &ClickEvent, window, cx| {
                                ws.tree_child_click(conn_click.clone(), c_click.clone(), ev.click_count(), window, cx)
                            }));
                        if is_table {
                            base.context_menu(move |menu, _, _| {
                                let (ws1, ws2, ws3) = (ws.clone(), ws.clone(), ws.clone());
                                let (c1, c2, c3) = (conn_menu.clone(), conn_menu.clone(), conn_menu.clone());
                                let (s1, t1) = (menu_child.path.clone(), menu_child.label.clone());
                                let (s2, t2) = (s1.clone(), t1.clone());
                                let name = format!("{s1}.{t1}");
                                menu.item(PopupMenuItem::new("데이터 열기").on_click(move |_, window, cx| {
                                    let _ = ws1.update(cx, |ws, cx| ws.open_table(c1.clone(), s1.clone(), t1.clone(), window, cx));
                                }))
                                .item(PopupMenuItem::new("DDL 보기").on_click(move |_, window, cx| {
                                    let _ = ws2.update(cx, |ws, cx| ws.show_ddl(c2.clone(), s2.clone(), t2.clone(), window, cx));
                                }))
                                .item(PopupMenuItem::new("이름 복사").on_click(move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(name.clone()));
                                }))
                                .separator()
                                .item(PopupMenuItem::new("새로 고침").on_click(move |_, window, cx| {
                                    let _ = ws3.update(cx, |ws, cx| ws.refresh_catalog(&c3, window, cx));
                                }))
                            })
                            .into_any_element()
                        } else {
                            base.into_any_element()
                        }
                    }
                    RowKind::Loading { .. } => {
                        let indent = sc.s8() + (row.depth.saturating_sub(1)) as f32 * sc.s12() + sc.icon() + sc.s8();
                        kit::row(&sc, &theme, false)
                            .mx(px(sc.s4()))
                            .pl(px(indent))
                            .debug_selector(move || format!("child-row-{i}"))
                            .child(kit::text(&sc, sc.small(), theme.muted_foreground, "읽는 중…"))
                            .into_any_element()
                    }
                    RowKind::Failed { node, msg } => {
                        let indent = sc.s8() + (row.depth.saturating_sub(1)) as f32 * sc.s12() + sc.icon() + sc.s8();
                        kit::row(&sc, &theme, false)
                            .id(("failed", i))
                            .mx(px(sc.s4()))
                            .pl(px(indent))
                            .debug_selector(move || format!("child-row-{i}"))
                            .child(kit::text(&sc, sc.small(), theme.danger, msg.clone()).flex_1().min_w_0())
                            .child(
                                Button::new(("retry", i))
                                    .ghost()
                                    .label("다시 시도")
                                    .on_click(cx.listener(move |ws, _, window, cx| ws.tree_retry(node.clone(), window, cx))),
                            )
                            .tooltip(move |window, cx| Tooltip::new(msg.clone()).build(window, cx))
                            .into_any_element()
                    }
                };
            list = list.child(el);
        }

        let filter = self.tree_filter.read(cx).value().to_lowercase();
        let queries_el = self.render_queries(&filter, cx);
        let section = |title: &'static str| kit::section(&sc, &theme, title);
        v_flex()
            .size_full()
            .bg(theme.sidebar)
            .child(
                h_flex()
                    .p_2()
                    .gap_1()
                    .child(div().flex_1().child(Input::new(&self.tree_filter).cleanable(true)))
                    .child(
                        Button::new("dashboard")
                            .ghost()
                            .icon(IconName::LayoutDashboard)
                            .tooltip("대시보드 (⌘G)")
                            .on_click(cx.listener(|ws, _, w, cx| ws.open_dashboard(w, cx))),
                    )
                    .child(
                        Button::new("switch-conn")
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
                    .child(section("쿼리"))
                    .child(queries_el),
            )
    }

    fn render_queries(&self, filter: &str, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        if let Some(e) = &self.queries_err {
            return div()
                .px_3()
                .py_1()
                .text_xs()
                .text_color(theme.danger)
                .child(format!("쿼리 목록을 읽지 못함: {e}"))
                .into_any_element();
        }
        let Some(files) = &self.queries else {
            return h_flex()
                .px_3()
                .py_1()
                .gap_2()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(gpui_component::spinner::Spinner::new().xsmall())
                .child("queries/ 읽는 중…")
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
        let sc = self.scale();
        let mut list = v_flex().py(px(sc.s4())).gap(px(sc.row_gap()));
        let mut i = 0usize;
        for (conn, group) in queries::group(&shown, &self.tree.conns) {
            list = list.child(
                kit::text(
                    &sc,
                    sc.small(),
                    theme.muted_foreground,
                    conn.clone().unwrap_or_else(|| "접속 없음".into()),
                )
                .px(px(sc.s12()))
                .pt(px(sc.s8())),
            );
            for f in group {
                i += 1;
                let rel = f.rel.clone();
                let open = self.query_tab(&f.rel).is_some();
                list = list.child(
                    kit::row(&sc, &theme, open)
                        .id(("query", i))
                        .mx(px(sc.s4()))
                        .cursor_pointer()
                        .hover(|st| st.bg(theme.sidebar_accent))
                        .debug_selector(move || format!("query-row-{i}"))
                        .child(div().flex_1().min_w_0().truncate().child(f.name.clone()))
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

    /// 상태 줄 왼쪽의 접속·데이터베이스·스키마 고르기다(Orca 처럼 무거운 도구 줄 없이).
    fn render_conn_selector(&mut self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let conn = self.active_conn();
        let summary = conn.as_ref().and_then(|c| self.conn(c)).cloned();
        let is_prod = summary.as_ref().is_some_and(|s| tone(&s.env) == Tone::Prod);

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
            .child(selector)
            .when_some(summary.as_ref(), |d, s| d.child(env_pill(&s.env, theme.is_dark())))
    }

    /// 제목 줄 오른쪽의 작은 아이콘 버튼들이다(실행·EXPLAIN·멈춤·트랜잭션·prod 잠금).
    fn render_actions(&mut self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
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
        let sc = self.scale();
        h_flex()
            .gap_0p5()
            .pr_2()
            .when(is_pg, |d| {
                d.child(
                    Button::new("run")
                        .with_size(px(sc.hit()))
                        .ghost()
                        .icon(IconName::Play)
                        .tooltip("커서 문장 또는 선택 실행 (⌘↵)")
                        .on_click(cx.listener(|ws, _, w, cx| ws.run_query(false, w, cx))),
                )
                .child(
                    Button::new("run-all")
                        .with_size(px(sc.hit()))
                        .ghost()
                        .icon(IconName::ArrowRight)
                        .tooltip("편집기 문장 모두 실행 (⇧⌘↵)")
                        .on_click(cx.listener(|ws, _, w, cx| ws.run_all(w, cx))),
                )
                .child(
                    Button::new("explain")
                        .with_size(px(sc.hit()))
                        .ghost()
                        .icon(IconName::Search)
                        .tooltip("실행 계획 (⌘E)")
                        .on_click(cx.listener(|ws, _, w, cx| ws.run_query(true, w, cx))),
                )
            })
            .child(
                Button::new("stop")
                    .with_size(px(sc.hit()))
                    .ghost()
                    .icon(IconName::Square)
                    .disabled(!busy)
                    .tooltip("진행 중인 요청 멈춤 (⌘.)")
                    .on_click(cx.listener(|ws, _, _, cx| ws.cancel(cx))),
            )
            .when(is_pg, |d| {
                d.child(div().w_px().h(rems(1.0)).bg(theme.border).mx_1())
                    .child(
                        Button::new("tx")
                            .h(px(sc.hit()))
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
                                .with_size(px(sc.hit()))
                                .ghost()
                                .icon(IconName::Check)
                                .tooltip("COMMIT")
                                .on_click(cx.listener(|ws, _, w, cx| ws.tx_op("COMMIT", w, cx))),
                        )
                        .child(
                            Button::new("rollback")
                                .with_size(px(sc.hit()))
                                .ghost()
                                .icon(IconName::Undo2)
                                .tooltip("ROLLBACK")
                                .on_click(cx.listener(|ws, _, w, cx| ws.tx_op("ROLLBACK", w, cx))),
                        )
                    })
            })
            .when(is_prod, |d| {
                let label = match lock {
                    Some((false, left)) => format!("prod 열림 {}:{:02}", left / 60, left % 60),
                    _ => "prod".into(),
                };
                d.child(
                    Button::new("lock")
                        .h(px(sc.hit()))
                        .with_variant(if locked { ButtonVariant::Ghost } else { ButtonVariant::Danger })
                        .icon(IconName::TriangleAlert)
                        .label(label)
                        .tooltip("prod 쓰기 잠금 열기/잠그기 (⌘L)")
                        .on_click(cx.listener(|ws, _, w, cx| ws.toggle_lock(w, cx))),
                )
            })
            .child(div().w_px().h(rems(1.0)).bg(theme.border).mx_1())
            .child(
                Button::new("dashboard-top")
                    .with_size(px(sc.hit()))
                    .ghost()
                    .icon(IconName::LayoutDashboard)
                    .tooltip("대시보드 (⌘G)")
                    .on_click(cx.listener(|ws, _, w, cx| ws.open_dashboard(w, cx))),
            )
            .child(
                Button::new("settings-top")
                    .with_size(px(sc.hit()))
                    .ghost()
                    .icon(IconName::Settings)
                    .tooltip("설정 (⌘,)")
                    .on_click(cx.listener(|ws, _, w, cx| super::dialogs::open_settings(ws, w, cx))),
            )
    }

    fn render_tabs(&mut self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let sc = self.scale();
        // 제목 줄 안의 탭(Orca): 작은 높이, 고른 탭만 바탕을 깔고 아래 강조선을 둔다
        let mut bar = h_flex().id("tabs").h_full().gap_0p5().items_center().overflow_x_scroll();
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
                    .group(SharedString::from(format!("tab-{i}")))
                    .min_h(px(sc.tab()))
                    .pl_2p5()
                    .pr_1()
                    .gap_1()
                    .rounded(theme.radius)
                    .cursor_pointer()
                    .text_xs()
                    .text_color(if active { theme.foreground } else { theme.muted_foreground })
                    .when(active, |d| {
                        d.bg(theme.secondary)
                            .border_b_2()
                            .border_color(if prod { theme.danger } else { theme.ring })
                    })
                    .when(!active, |d| d.hover(|s| s.bg(theme.secondary.opacity(0.5))))
                    .when(prod && !active, |d| d.text_color(theme.danger.opacity(0.8)))
                    .child(div().max_w(px(180.)).truncate().child(base.title()))
                    .child(
                        div()
                            .id(("close", i))
                            .size(px(sc.icon() + sc.s4()))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(theme.radius)
                            .hover(|s| s.bg(theme.accent))
                            // 고친 탭은 점을 늘 보이고, 아니면 올렸을 때만 닫기 단추가 보인다
                            .when(!t.dirty && !active, |d| {
                                d.opacity(0.).group_hover(format!("tab-{i}"), |s| s.opacity(1.))
                            })
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
                .with_size(px(sc.hit()))
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
        let selector = self.render_conn_selector(cx);
        let sc = self.scale();
        h_flex()
            .px_1()
            .min_h(px(sc.status_bar()))
            .gap_3()
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.status_bar)
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(selector)
            .when(in_tx, |d| {
                d.child(pill("트랜잭션 중".into(), theme.warning.opacity(0.2), theme.warning))
            })
            .when_some(lock, |d, l| d.child(pill(l, theme.danger.opacity(0.15), theme.danger)))
            .when(self.pending_saves > 0, |d| {
                d.child(
                    h_flex()
                        .gap_1()
                        .child(gpui_component::spinner::Spinner::new().xsmall())
                        .child("저장 중"),
                )
            })
            .when_some(self.busy.as_ref().map(|b| b.0.clone()), |d, label| {
                d.child(div().child(format!("{label} 중…"))).child(
                    Button::new("cancel")
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
            .child(div().pr_2().child(self.target.as_ref().map(Target::label).unwrap_or_default()))
            .when(self.prefs.zoom != zoom::DEFAULT, |d| {
                d.child(div().child(zoom::label(self.prefs.zoom)))
            })
    }
}

/// 열린 작업 공간들이다. 끝내기(⌘Q)가 저장을 기다릴 곳을 찾는다.
#[derive(Default)]
pub struct OpenWorkspaces(pub Vec<gpui::WeakEntity<Workspace>>);

impl gpui::Global for OpenWorkspaces {}

/// 모든 창의 쿼리 파일을 저장하고 쓰기가 끝나거나 20초가 지나면 끝낸다(TUI 와 같다).
pub fn quit_after_saves(cx: &mut gpui::App) {
    let open = cx.try_global::<OpenWorkspaces>().map(|o| o.0.clone()).unwrap_or_default();
    let waits: Vec<gpui::Task<()>> = open
        .into_iter()
        .filter_map(|w| {
            w.update(cx, |ws, cx| {
                ws.flush_saves(cx);
                ws.close_ready = true;
                ws.wait_saves(cx)
            })
            .ok()
        })
        .collect();
    cx.spawn(async move |cx| {
        for t in waits {
            t.await;
        }
        cx.update(|cx| cx.quit());
    })
    .detach();
}

/// 보낼 쿼리 파일 쓰기다. conn 은 충돌 사본에 붙일 접속이다.
#[derive(Clone, Debug)]
struct QueuedSave {
    rel: String,
    text: String,
    force: bool,
    conn: String,
}

/// 고른 경로가 프로젝트 `queries/` 아래 .sql 이면 그 상대 경로다(엔진으로 쓴다).
fn project_query_rel(project: &std::path::Path, path: &std::path::Path) -> Option<String> {
    let rel = path.strip_prefix(project).ok()?.to_string_lossy().replace('\\', "/");
    (rel.starts_with("queries/") && rel.ends_with(".sql")).then_some(rel)
}

enum TreeData {
    Catalog(Catalog),
    Keys(idb_engine::ScanResult),
    Nodes(Vec<idb_engine::Node>),
}

/// 이보다 좁은 사이드바는 접속 줄을 한 줄로 접는다(호스트는 툴팁)
const NARROW_SIDEBAR: f32 = 220.0;

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
            let bar = TitleBar::new().h(px(self.scale().titlebar())).child(
                h_flex()
                    .gap_2()
                    .child(super::brand::mark(px(14.), theme.foreground, super::brand::accent()))
                    .child(div().text_xs().text_color(theme.muted_foreground).child(brand::NAME)),
            );
            return root.child(
                v_flex()
                    .size_full()
                    .child(bar)
                    .child(div().flex_1().min_h_0().child(self.render_picker(cx))),
            );
        }
        let sidebar = self.render_sidebar(cx);
        let actions = self.render_actions(cx);
        let tabs = self.render_tabs(cx);
        let body = self.render_body(cx);
        let status = self.render_status(cx);
        // 제목 줄(Orca): 신호등 자리 뒤에 사이드바 단추·마크, 가운데 탭, 오른쪽 작은 아이콘 단추
        let sc = self.scale();
        let titlebar = TitleBar::new().h(px(sc.titlebar())).child(
            h_flex()
                .size_full()
                .gap_1()
                .child(
                    Button::new("toggle-sidebar")
                        .with_size(px(sc.hit()))
                        .ghost()
                        .icon(if self.show_sidebar {
                            IconName::PanelLeftClose
                        } else {
                            IconName::PanelLeftOpen
                        })
                        .tooltip("사이드바 (⌘B)")
                        .on_click(cx.listener(|ws, _, _, cx| {
                            ws.show_sidebar = !ws.show_sidebar;
                            cx.notify();
                        })),
                )
                .child(super::brand::mark(px(14.), theme.foreground, super::brand::accent()))
                .child(div().flex_1().min_w_0().h_full().child(tabs))
                .child(actions),
        );
        let main = v_flex().size_full().child(div().flex_1().min_h_0().child(body));
        root.child(
            v_flex()
                .size_full()
                .child(titlebar)
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

    fn bounds(cx: &mut VisualTestContext, sel: String) -> Option<gpui::Bounds<gpui::Pixels>> {
        cx.debug_bounds(Box::leak(sel.into_boxed_str()))
    }

    /// v1.1.0 사이드바: 두 줄 접속 줄을 28px 로 묶어 둘째 줄이 다음 줄 이름과 겹쳤다.
    /// 배율마다 줄이 내용(두 줄)을 담고, 줄끼리 겹치지 않고, 눌리기에 충분한지 실제 배치로 본다.
    #[gpui::test]
    fn 사이드바_줄은_배율마다_내용을_담고_서로_겹치지_않는다(cx: &mut TestAppContext) {
        let (ws, cx) = setup(cx);
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_target(Target::Local(offline_project()), window, cx)));
        wait(cx, &ws, |w| w.engine.is_some());
        cx.simulate_resize(gpui::size(px(1600.), px(4000.)));
        // 접속 하나를 펼쳐 스키마·테이블(두 단계 들여쓰기)도 잰다. 엔진 대신 카탈로그를 바로 넣는다
        cx.update(|_, cx| {
            ws.update(cx, |w, _| {
                let cat: Catalog = serde_json::from_value(serde_json::json!({"schemas": [{"name": "public", "tables": [
                    {"name": "users", "view": false, "columns": []}, {"name": "v_users", "view": true, "columns": []}]}], "functions": []}))
                .unwrap();
                w.tree.toggle_node("app-stg");
                w.tree.set_catalog("app-stg", &cat);
                w.tree.toggle_node("app-stg\u{1f}s:public");
            })
        });
        let n_rows = cx.update(|_, cx| ws.read(cx).tree.rows().len());
        let depths: Vec<usize> = cx.update(|_, cx| ws.read(cx).tree.rows().iter().map(|r| r.depth).collect());
        let conn_rows: Vec<usize> = cx.update(|_, cx| {
            ws.read(cx)
                .tree
                .rows()
                .iter()
                .enumerate()
                .filter(|(_, r)| matches!(r.kind, RowKind::Conn { .. }))
                .map(|(i, _)| i)
                .collect()
        });
        assert!(conn_rows.len() >= 3, "시험할 접속 줄이 모자라다");
        for z in crate::vm::zoom::STEPS {
            cx.update(|window, cx| ws.update(cx, |w, cx| w.set_zoom(z, window, cx)));
            draw(cx);
            let sc = cx.update(|_, cx| ws.read(cx).scale());
            let mut prev_bottom: Option<gpui::Pixels> = None;
            for i in 0..n_rows {
                let row = bounds(cx, format!("conn-row-{i}"))
                    .or_else(|| bounds(cx, format!("folder-row-{i}")))
                    .or_else(|| bounds(cx, format!("child-row-{i}")));
                let Some(row) = row else { continue };
                assert!(
                    f32::from(row.size.height) + 0.5 >= sc.hit(),
                    "{z}배 {i}번 줄 높이 {:?} < {}",
                    row.size.height,
                    sc.hit()
                );
                if let Some(pb) = prev_bottom {
                    assert!(
                        row.top() >= pb - px(0.5),
                        "{z}배 {i}번 줄이 앞 줄과 겹친다: {:?} < {:?}",
                        row.top(),
                        pb
                    );
                }
                prev_bottom = Some(row.bottom());
                if conn_rows.contains(&i) {
                    let l1 = bounds(cx, format!("conn-line1-{i}")).expect("첫 줄");
                    let l2 = bounds(cx, format!("conn-line2-{i}")).expect("둘째 줄");
                    assert!(
                        l1.top() >= row.top() - px(0.5) && l2.bottom() <= row.bottom() + px(0.5),
                        "{z}배 {i}번 줄 글이 줄 밖으로 나간다: {row:?} {l1:?} {l2:?}"
                    );
                    assert!(l1.bottom() <= l2.top() + px(0.5), "{z}배 {i}번 줄 두 줄이 겹친다: {l1:?} {l2:?}");
                }
            }
            assert!(prev_bottom.is_some(), "{z}배: 줄을 하나도 못 쟀다");
            // 들여쓰기: 깊을수록 글이 오른쪽에서 시작한다(아이콘 자리가 겹치지 않게)
            let chevron_left = |cx: &mut VisualTestContext, d: usize| -> f32 {
                let i = depths.iter().position(|x| *x == d).expect("그 깊이 줄");
                let sel = if d == 1 {
                    format!("conn-chevron-{i}")
                } else {
                    format!("child-chevron-{i}")
                };
                f32::from(bounds(cx, sel).expect("화살표").left())
            };
            let (l1, l2, l3) = (chevron_left(cx, 1), chevron_left(cx, 2), chevron_left(cx, 3));
            assert!(l1 < l2 && l2 < l3, "{z}배 들여쓰기가 깊이를 따르지 않는다: {l1} {l2} {l3}");
        }
    }

    /// v1.1.1: 테이블 목록이 활성 탭 접속에만, 맨 아래에 있었고 실패하면 끝없이 "읽는 중…" 이었다.
    #[gpui::test]
    fn 탭이_없어도_접속을_펼치고_실패하면_오류_줄과_다시_시도가_된다(cx: &mut TestAppContext) {
        let (ws, cx) = setup(cx);
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_target(Target::Local(offline_project()), window, cx)));
        wait(cx, &ws, |w| w.engine.is_some());
        cx.update(|window, cx| {
            ws.update(cx, |w, cx| {
                w.close_tab(0, cx);
                assert!(w.tabs.tabs.is_empty());
                w.tree_toggle("app-stg".into(), window, cx);
                assert!(
                    w.tree
                        .rows()
                        .iter()
                        .any(|r| matches!(&r.kind, RowKind::Loading { node } if node == "app-stg"))
                );
            })
        });
        draw(cx);
        // 오프라인 프로젝트의 PG 는 닿지 않는다 → 오류 줄
        wait(cx, &ws, |w| {
            w.tree
                .rows()
                .iter()
                .any(|r| matches!(&r.kind, RowKind::Failed { node, .. } if node == "app-stg"))
        });
        cx.update(|window, cx| {
            ws.update(cx, |w, cx| {
                assert!(w.tabs.tabs.is_empty(), "펼치기는 탭을 열지 않는다");
                w.tree_retry("app-stg".into(), window, cx);
                assert!(
                    w.tree
                        .rows()
                        .iter()
                        .any(|r| matches!(&r.kind, RowKind::Loading { node } if node == "app-stg"))
                );
            })
        });
        wait(cx, &ws, |w| {
            !w.tree.rows().iter().any(|r| matches!(r.kind, RowKind::Loading { .. }))
        });
    }

    /// 다시 연 엔진에 옛 트리·카탈로그 답이 앉으면 안 된다(db_gen 은 다시 열면 0 으로 겹친다).
    #[gpui::test]
    fn 다시_연_엔진은_옛_답을_받지_않는다(cx: &mut TestAppContext) {
        let (ws, cx) = setup(cx);
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_target(Target::Local(offline_project()), window, cx)));
        wait(cx, &ws, |w| w.engine.is_some());
        let (s0, g0) = cx.update(|_, cx| (ws.read(cx).session, ws.read(cx).db_gen("app-stg")));
        assert!(cx.update(|_, cx| ws.read(cx).fresh("app-stg", s0, g0)));
        cx.update(|_, cx| ws.update(cx, |w, cx| w.disconnect(cx)));
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_target(Target::Local(offline_project()), window, cx)));
        wait(cx, &ws, |w| w.engine.is_some());
        cx.update(|_, cx| {
            let w = ws.read(cx);
            assert_eq!(w.db_gen("app-stg"), g0, "db_gen 만으로는 가를 수 없다");
            assert!(!w.fresh("app-stg", s0, g0), "옛 엔진 답은 버린다");
            assert!(w.fresh("app-stg", w.session, g0));
        });
    }

    /// prod Redis 이름공간 경로엔 이메일 같은 개인정보가 들 수 있어 gui.json 에 두지 않는다. 옛 판이 남긴 것도 열 때 지운다.
    #[gpui::test]
    fn prod_Redis_펼침_기억은_열_때_지우고_나머지는_둔다(cx: &mut TestAppContext) {
        let dir = redis_project();
        std::fs::write(
            dir.path().join(".dbtui/connections.json"),
            r#"[{"uuid": "r1", "name": "cache-prod", "group": "c", "kind": "redis", "host": "127.0.0.1", "port": 1}]"#,
        )
        .unwrap();
        let (ws, cx) = setup(cx);
        let target = Target::Local(dir.path().to_path_buf());
        let pii = "cache-prod\u{1f}r:user:a@b.com".to_string();
        let keep = vec!["cache-prod".to_string(), "app-stg\u{1f}r:user".to_string()];
        cx.update(|_, cx| {
            ws.update(cx, |w, _| {
                let expanded = [keep.clone(), vec![pii.clone()]].concat();
                w.prefs.tree_memory.insert(
                    target.label(),
                    crate::vm::runtime::TreeMemory {
                        collapsed: vec![],
                        expanded,
                    },
                );
            })
        });
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_target(target.clone(), window, cx)));
        wait(cx, &ws, |w| w.engine.is_some());
        cx.update(|_, cx| {
            let w = ws.read(cx);
            assert_eq!(w.prefs.tree_memory[&target.label()].expanded, keep);
        });
        cx.update(|_, cx| ws.update(cx, |w, _| w.save_tree_memory()));
        cx.update(|_, cx| assert!(!ws.read(cx).prefs.tree_memory[&target.label()].expanded.contains(&pii)));
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
                v.selected = Some("k".into());
                v.raw = Some(raw);
                assert!(!v.apply_edit("k", crate::vm::redisedit::Action::SetString("x".into()), window, cx));
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

    /// 리뷰: A 를 고르고 곧 B 를 고르면 A 의 늦은 답이 B 자리에 앉아, B 로 보이는 창에서 A 를 지웠다.
    #[gpui::test]
    fn Redis_늦은_답은_버리고_다른_키_편집은_보내지_않는다(cx: &mut TestAppContext) {
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
        let raw = |k: &str| -> idb_engine::RawValue {
            serde_json::from_value(serde_json::json!({"key": k, "type": "string", "ttl_secs": -1, "string": "v"})).unwrap()
        };
        cx.update(|window, cx| {
            view.update(cx, |v, cx| {
                v.open_key("a".into(), window, cx);
                v.open_key("b".into(), window, cx);
                // A 를 고를 때의 세대(1)로 A 의 답이 늦게 온다
                v.on_raw(1, raw("a"), cx);
                assert!(v.raw.is_none(), "늦은 A 답은 버린다");
                v.on_raw(2, raw("b"), cx);
                assert_eq!(v.raw.as_ref().map(|r| r.key.as_str()), Some("b"));
                // 창은 a 로 열렸는데 그사이 b 를 골랐다면 보내지 않는다
                assert!(!v.apply_edit("a", crate::vm::redisedit::Action::DeleteKey, window, cx));
                assert!(
                    v.apply_edit("b", crate::vm::redisedit::Action::DeleteKey, window, cx),
                    "확인한 키는 보낸다"
                );
            })
        });
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

    fn set_text(cx: &mut VisualTestContext, ws: &Entity<Workspace>, id: u64, text: &str) {
        let text = text.to_string();
        cx.update(|window, cx| {
            ws.update(cx, |w, cx| {
                if let Some(Body::Sql { editor, .. }) = w.bodies.get(&id) {
                    editor.update(cx, |e, cx| e.set_value(text, window, cx));
                }
            })
        });
    }

    fn saves_done(w: &Workspace) -> bool {
        w.pending_saves == 0 && w.saving.is_empty()
    }

    /// 화면의 쿼리 파일 흐름이다. 로컬·원격 엔진 모두 같은 결과여야 한다(dir 은 serve 쪽 프로젝트).
    fn query_flow(cx: &mut VisualTestContext, ws: &Entity<Workspace>, dir: &std::path::Path) {
        wait(cx, ws, |w| w.queries.is_some());
        let rel = "queries/app/a.sql".to_string();
        let conn = cx.update(|_, cx| {
            let q = ws.read(cx).queries.clone().unwrap();
            q.iter().find(|f| f.rel == rel).expect("a.sql").conn.clone()
        });
        assert!(conn.is_some(), "매핑으로 접속을 잇는다");
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_query(rel.clone(), window, cx)));
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_query(rel.clone(), window, cx)));
        wait(cx, ws, |w| w.query_tab("queries/app/a.sql").is_some());
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_query(rel.clone(), window, cx)));
        wait(cx, ws, |_| true);
        let id = cx.update(|_, cx| {
            let w = ws.read(cx);
            let opened: Vec<_> = w.tabs.tabs.iter().filter(|t| t.file == Some(TabFile::Query(rel.clone()))).collect();
            assert_eq!(opened.len(), 1, "이미 열린 파일은 그 탭으로 간다");
            assert_eq!(opened[0].conn, conn);
            opened[0].id
        });
        let file = dir.join(&rel);
        set_text(cx, ws, id, "select 42");
        cx.update(|_, cx| ws.update(cx, |w, cx| w.autosave(id, cx)));
        wait(cx, ws, saves_done);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "select 42");
        cx.update(|_, cx| assert!(!ws.read(cx).tabs.active().unwrap().dirty));
        // 밖에서 바꾸면 멈춘다
        std::fs::write(&file, "-- 다른 편집기").unwrap();
        set_text(cx, ws, id, "select 43");
        cx.update(|_, cx| ws.update(cx, |w, cx| w.autosave(id, cx)));
        wait(cx, ws, saves_done);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "-- 다른 편집기", "밖의 바뀜을 덮지 않는다");
        cx.update(|_, cx| assert!(ws.read(cx).autosave_stopped.contains_key(&id)));
        // 닫으면 덮을 수 없는 글을 충돌 사본에 남긴다(R45.3)
        cx.update(|_, cx| {
            ws.update(cx, |w, cx| {
                let i = w.tabs.index_of(id).unwrap();
                w.close_tab(i, cx);
            })
        });
        wait(cx, ws, saves_done);
        let copies: Vec<String> = std::fs::read_dir(dir.join("queries/app"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("a-conflict-"))
            .collect();
        assert_eq!(copies.len(), 1, "{copies:?}");
        assert_eq!(
            std::fs::read_to_string(dir.join("queries/app").join(&copies[0])).unwrap(),
            "select 43"
        );
        // 첫 실행 저장(R45.1): 엔진이 경로를 정하고 매핑을 붙인다
        let id2 = cx.update(|window, cx| {
            ws.update(cx, |w, cx| {
                w.new_tab(Some("app-stg".into()), window, cx);
                w.tabs.active().unwrap().id
            })
        });
        set_text(cx, ws, id2, "select 1");
        cx.update(|_, cx| ws.update(cx, |w, cx| w.save_new_query_on_run(cx)));
        wait(cx, ws, |w| saves_done(w) && w.tabs.active().is_some_and(|t| t.file.is_some()));
        let new_rel = cx.update(|_, cx| {
            ws.read(cx)
                .tabs
                .active()
                .unwrap()
                .file
                .clone()
                .unwrap()
                .query()
                .unwrap()
                .to_string()
        });
        assert!(new_rel.starts_with("queries/") && new_rel.ends_with(".sql"), "{new_rel}");
        assert_eq!(std::fs::read_to_string(dir.join(&new_rel)).unwrap(), "select 1");
        wait(cx, ws, |w| {
            w.queries
                .as_ref()
                .is_some_and(|q| q.iter().any(|f| f.rel == new_rel && f.conn.as_deref() == Some("app-stg")))
        });
        // ⌘S 는 밖의 바뀜도 덮는다
        std::fs::write(dir.join(&new_rel), "-- 밖").unwrap();
        set_text(cx, ws, id2, "select 2");
        cx.update(|window, cx| ws.update(cx, |w, cx| w.save_file(window, cx)));
        wait(cx, ws, saves_done);
        assert_eq!(std::fs::read_to_string(dir.join(&new_rel)).unwrap(), "select 2");
    }

    #[gpui::test]
    fn 쿼리_파일을_로컬_엔진으로_열고_저장하고_충돌_사본을_남긴다(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        copy_dir(&offline_project(), dir.path());
        let (ws, cx) = setup(cx);
        let p = dir.path().to_path_buf();
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_target(Target::Local(p), window, cx)));
        wait(cx, &ws, |w| w.engine.is_some());
        query_flow(cx, &ws, dir.path());
    }

    /// devbox 같은 원격도 사이드바·열기·저장·충돌 사본·첫 실행 저장이 로컬과 같다. `db` 가 없으면 건너뛴다.
    #[gpui::test]
    fn 쿼리_파일을_원격_엔진으로도_같게_열고_저장한다(cx: &mut TestAppContext) {
        let Some(bin) = crate::vm::queries::test_db_bin() else {
            eprintln!("skip: target 에 db 가 없다 (cargo build -p idb-cli)");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        copy_dir(&offline_project(), dir.path());
        let (ws, cx) = setup(cx);
        // 원격 연결의 읽기·쓰기 일감이 도는 런타임. 시험 끝까지 둔다
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let spec = idb_engine::RemoteSpec {
            name: "test".into(),
            program: bin.display().to_string(),
            args: vec!["serve".into(), "-p".into(), dir.path().display().to_string()],
        };
        let (engine, info) = rt.block_on(async {
            let e = Engine::Remote(std::sync::Arc::new(idb_engine::RemoteEngine::connect(&spec).await.unwrap()));
            let info = e.project().await.unwrap();
            (e, info)
        });
        cx.update(|window, cx| ws.update(cx, |w, cx| w.connected(Target::Remote("test".into()), engine, info, window, cx)));
        query_flow(cx, &ws, dir.path());
        cx.update(|_, cx| ws.update(cx, |w, cx| w.disconnect(cx)));
        wait(cx, &ws, saves_done);
    }

    #[gpui::test]
    fn DB_AUTO_SAVE_0_이면_자동_저장과_첫_실행_저장을_하지_않는다(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        copy_dir(&offline_project(), dir.path());
        let (ws, cx) = setup(cx);
        let p = dir.path().to_path_buf();
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_target(Target::Local(p), window, cx)));
        wait(cx, &ws, |w| w.engine.is_some());
        let rel = "queries/app/a.sql".to_string();
        let file = dir.path().join(&rel);
        let before = std::fs::read_to_string(&file).unwrap();
        cx.update(|window, cx| {
            ws.update(cx, |w, cx| {
                w.autosave_on = false;
                w.open_query(rel.clone(), window, cx);
            })
        });
        wait(cx, &ws, |w| w.query_tab("queries/app/a.sql").is_some());
        let id = cx.update(|_, cx| ws.read(cx).tabs.active().unwrap().id);
        set_text(cx, &ws, id, "select 99");
        cx.update(|window, cx| {
            ws.update(cx, |w, cx| {
                w.autosave(id, cx);
                // 첫 실행 저장도 끈다: 파일 없는 새 탭
                w.new_tab(Some("app-stg".into()), window, cx);
            })
        });
        let id2 = cx.update(|_, cx| ws.read(cx).tabs.active().unwrap().id);
        set_text(cx, &ws, id2, "select 1");
        cx.update(|_, cx| ws.update(cx, |w, cx| w.save_new_query_on_run(cx)));
        wait(cx, &ws, saves_done);
        cx.update(|_, cx| assert!(ws.read(cx).tabs.active().unwrap().file.is_none(), "첫 실행 저장을 하지 않는다"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before, "자동 저장하지 않는다");
    }

    /// 창을 닫을 때 남은 저장을 보내고, 끝날 때까지 닫지 않는다.
    #[gpui::test]
    fn 창을_닫으면_남은_저장을_보내고_끝난_뒤_닫는다(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        copy_dir(&offline_project(), dir.path());
        let (ws, cx) = setup(cx);
        let p = dir.path().to_path_buf();
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_target(Target::Local(p), window, cx)));
        wait(cx, &ws, |w| w.engine.is_some());
        cx.update(|window, cx| ws.update(cx, |w, cx| w.open_query("queries/app/a.sql".into(), window, cx)));
        wait(cx, &ws, |w| w.query_tab("queries/app/a.sql").is_some());
        let id = cx.update(|_, cx| ws.read(cx).tabs.active().unwrap().id);
        set_text(cx, &ws, id, "select 7");
        let close_now = cx.update(|window, cx| ws.update(cx, |w, cx| w.should_close(window, cx)));
        assert!(!close_now, "저장이 도는 동안은 닫지 않는다");
        // 닫힌 뒤에는 창이 없으니 창 없이 엔터티만 읽는다
        let mut closed = false;
        for _ in 0..500 {
            cx.run_until_parked();
            if ws.read_with(cx, |w, _| saves_done(w) && w.close_ready) {
                closed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(closed, "저장이 끝나면 닫는다");
        assert_eq!(std::fs::read_to_string(dir.path().join("queries/app/a.sql")).unwrap(), "select 7");
    }

    /// 리뷰: prod WHERE 가 gui.json 에 남았다(R75.5 는 prod 기록을 막는다).
    #[gpui::test]
    fn prod_테이블_WHERE_는_기록하지_않고_옛_prod_기록은_지운다(cx: &mut TestAppContext) {
        let (ws, cx) = setup(cx);
        cx.update(|_, cx| {
            ws.update(cx, |w, _| {
                // 옛 판이 남긴 app-prod 기록(접속을 열 때 지워져야 한다)
                let stale = crate::vm::history::table_key("00000000-0000-0000-0000-000000000002", "public", "users");
                w.prefs.table_history.insert(
                    stale,
                    crate::vm::history::TableHistory {
                        where_: vec!["email = 'x'".into()],
                        order: vec![],
                    },
                );
                w.prefs.table_history.insert(
                    "app-prod\tpublic.users".into(),
                    crate::vm::history::TableHistory {
                        where_: vec!["y".into()],
                        order: vec![],
                    },
                );
            })
        });
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
                let (_, prod) = w.history_key("app-prod", "public", "users");
                assert!(prod);
                assert!(
                    !w.prefs.table_history.keys().any(|k| k.contains("public.users")
                        && (k.starts_with("app-prod") || k.starts_with("00000000-0000-0000-0000-000000000002"))),
                    "옛 prod 기록이 남았다: {:?}",
                    w.prefs.table_history.keys().collect::<Vec<_>>()
                );
                let id = w.tabs.tabs[0].id;
                w.show_result(id, "app-prod".into(), src, rs, false, window, cx);
                match w.bodies.get(&id) {
                    Some(Body::Sql { result: Some(r), .. }) => r.clone(),
                    _ => panic!("결과 없음"),
                }
            })
        });
        cx.update(|window, cx| {
            let (wi, _) = r.read(cx).table_inputs.clone().unwrap();
            wi.update(cx, |i, cx| i.set_value("email = 'secret'", window, cx));
            r.update(cx, |v, cx| v.apply_table_query(window, cx));
        });
        draw(cx);
        cx.update(|_, cx| {
            let w = ws.read(cx);
            assert!(
                w.prefs
                    .table_history
                    .values()
                    .all(|h| !h.where_.iter().any(|x| x.contains("secret")))
            );
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
                w.prefs.table_history.clear();
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
            let (key, prod) = w.history_key("app-stg", "public", "users");
            assert!(!prod);
            assert_eq!(w.prefs.table_history[&key].where_, vec!["id > 1"]);
            assert_eq!(w.prefs.table_history[&key].order, vec!["id desc"]);
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
