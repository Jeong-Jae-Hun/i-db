//! 창 하나의 작업 공간. 접속 전에는 고르는 화면, 접속 뒤에는 접속 트리·탭·편집기·결과·상태 줄이다.
//! 엔진 요청은 모두 `guarded` 를 거쳐 거절 사유(prod 잠금·확인)를 확인 창으로 바꾸고 다시 보낸다.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    AppContext as _, Context, Entity, FocusHandle, Focusable, InteractiveElement as _, IntoElement, ParentElement as _, PathPromptOptions,
    Render, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_component::notification::Notification;
use gpui_component::{ActiveTheme as _, Sizable as _, WindowExt as _, h_flex, resizable_panel, v_flex, v_resizable};
use idb_engine::{Catalog, ConnSummary, Engine, EngineError, LockState, ProjectInfo, ResultSet, RunRequest, TablePageRequest};

use super::redis::RedisView;
use super::results::{ResultsView, Source};
use super::sql_completion::{CatalogCell, SqlCompletion};
use super::{CancelRun, CloseTab, ExplainQuery, NewTab, OpenFile, RunQuery, SaveFile, ToggleProdLock, TxBegin, TxCommit, TxRollback};
use crate::vm::picker;
use crate::vm::runtime::{Canceler, Prefs, Runtime, Target, connect};
use crate::vm::safety::{Ask, Confirms, Gate, Tone, lock_label, tone};
use crate::vm::tabs::{Tabs, target_sql};
use crate::vm::tree::{ConnTree, RowKind};

pub type Make<F> = Rc<dyn Fn(Engine, Confirms) -> F>;
pub type Done<T> = Rc<dyn Fn(&mut Workspace, T, &mut Window, &mut Context<Workspace>)>;

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
}

pub struct Workspace {
    rt: Rc<Runtime>,
    focus: FocusHandle,
    prefs: Prefs,
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
    pub fn new(rt: Rc<Runtime>, window: &mut Window, cx: &mut Context<Self>) -> Workspace {
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
        let tree_filter = cx.new(|cx| InputState::new(window, cx).placeholder("접속 거르기"));
        let subs = vec![
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
        Workspace {
            rt,
            focus: cx.focus_handle(),
            prefs,
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
        let Some(engine) = self.engine.clone() else { return };
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
                    Err(_) => ws.set_status(format!("{label}: 멈춤"), true, cx),
                    Ok(Ok(v)) => {
                        ws.status = (String::new(), false);
                        done(ws, v, window, cx)
                    }
                    Ok(Err(e)) => ws.refused(&label, e, make, done, confirms, window, cx),
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
            }
            Gate::Locked(msg) => {
                self.set_status(format!("{msg} (⌘L 로 prod 잠금을 연다)"), true, cx);
                window.push_notification(Notification::warning("prod 가 잠겨 있다. ⌘L 로 잠시 연다"), cx);
            }
            Gate::Ask { ask, question } => {
                let this = cx.entity().downgrade();
                let label = label.to_string();
                window.open_alert_dialog(cx, move |alert, _, _| {
                    let (this, make, done, label) = (this.clone(), make.clone(), done.clone(), label.clone());
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
                            let _ = this.update(cx, |ws, cx| ws.guarded(&label, make.clone(), done.clone(), c, window, cx));
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
        if let Some(p) = Prefs::path() {
            let _ = self.prefs.save(&p);
        }
        let warn = info.warnings.join("; ");
        self.tree = ConnTree::new(info.conns);
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
            k => self.set_status(format!("{k} 접속은 아직 GUI 에서 열지 않는다(터미널 db 를 쓴다)"), true, cx),
        }
        cx.notify();
    }

    /// 트랜잭션·잠금 상태를 읽는다.
    pub fn refresh_conn_state(&mut self, conn: &str, window: &mut Window, cx: &mut Context<Self>) {
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
        }
    }

    fn ensure_catalog(&mut self, conn: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.catalogs.contains_key(conn) {
            return;
        }
        let n = conn.to_string();
        let name = n.clone();
        self.guarded(
            "카탈로그",
            Rc::new(move |e: Engine, _| {
                let n = n.clone();
                async move { e.catalog(&n).await }
            }),
            Rc::new(move |ws: &mut Workspace, cat: Catalog, _, cx| {
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
                && !this.tabs.tabs[i].dirty
            {
                this.tabs.tabs[i].dirty = true;
                cx.notify();
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
        if let Some(t) = self.tabs.close(i) {
            self.bodies.remove(&t.id);
        }
        cx.notify();
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
            view.update(cx, |v, cx| v.run_command(window, cx));
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
        let view = cx.new(|cx| ResultsView::new(ws, self.rt.clone(), engine, conn, source, rs, more, window, cx));
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
                    limit: 500,
                    ..Default::default()
                };
                self.guarded(
                    "테이블 읽기",
                    Rc::new(move |e: Engine, _| {
                        let req = req.clone();
                        async move { e.table_page(req).await }
                    }),
                    Rc::new(move |ws: &mut Workspace, page: idb_engine::TablePage, window, cx| {
                        let more = page.result.rows.len() >= 500;
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
        match std::fs::write(&path, text) {
            Ok(()) => {
                if let Some(i) = self.tabs.index_of(id) {
                    self.tabs.tabs[i].dirty = false;
                    self.tabs.tabs[i].path = Some(path.clone());
                }
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
        let active = self.active_conn();
        let mut list = v_flex().id("conn-tree").flex_1().min_h_0().overflow_y_scroll().py_1();
        for (i, row) in self.tree.rows().into_iter().enumerate() {
            let color = match row.tone {
                Tone::Prod => theme.danger,
                Tone::Stg => theme.warning,
                Tone::Local => theme.foreground,
            };
            let el = match row.kind {
                RowKind::Folder { expanded, count } => {
                    let label = row.label.clone();
                    h_flex()
                        .id(("folder", i))
                        .px_2()
                        .py_0p5()
                        .gap_1()
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.list_hover))
                        .child(
                            div()
                                .w(px(12.))
                                .text_color(theme.muted_foreground)
                                .child(if expanded { "▾" } else { "▸" }),
                        )
                        .child(
                            div()
                                .text_color(if row.tone == Tone::Prod { theme.danger } else { theme.foreground })
                                .child(row.label),
                        )
                        .child(div().text_xs().text_color(theme.muted_foreground).child(count.to_string()))
                        .on_click(cx.listener(move |ws, _, _, cx| {
                            ws.tree.toggle(&label);
                            cx.notify();
                        }))
                }
                RowKind::Conn(ci) => {
                    let name = self.tree.conns[ci].name.clone();
                    let env = self.tree.conns[ci].env.clone();
                    let selected = active.as_deref() == Some(name.as_str());
                    let n2 = name.clone();
                    h_flex()
                        .id(("conn", i))
                        .pl(px(24.))
                        .pr_2()
                        .py_0p5()
                        .gap_2()
                        .cursor_pointer()
                        .when(selected, |d| d.bg(theme.list_active))
                        .hover(|s| s.bg(theme.list_hover))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(div().text_color(color).truncate().child(name))
                                .child(div().text_xs().text_color(theme.muted_foreground).truncate().child(row.detail)),
                        )
                        .when(tone(&env) != Tone::Local, |d| {
                            d.child(
                                div()
                                    .px_1()
                                    .rounded_sm()
                                    .text_xs()
                                    .text_color(theme.background)
                                    .bg(if tone(&env) == Tone::Prod { theme.danger } else { theme.warning })
                                    .child(env),
                            )
                        })
                        .on_click(cx.listener(move |ws, _, window, cx| ws.select_conn(n2.clone(), window, cx)))
                }
            };
            list = list.child(el);
        }

        // 지금 접속의 테이블(카탈로그)
        let mut objects = v_flex().id("objects").flex_1().min_h_0().overflow_y_scroll().py_1();
        let filter = self.tree_filter.read(cx).value().to_lowercase();
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
                        .px_2()
                        .pt_1()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(s.name.clone()),
                );
                for t in tables {
                    i += 1;
                    let (c, sn, tn) = (conn.clone(), s.name.clone(), t.name.clone());
                    objects = objects.child(
                        h_flex()
                            .id(("table", i))
                            .pl_4()
                            .pr_2()
                            .cursor_pointer()
                            .hover(|st| st.bg(theme.list_hover))
                            .child(
                                div()
                                    .truncate()
                                    .child(if t.view { format!("{} (view)", t.name) } else { t.name.clone() }),
                            )
                            .on_click(cx.listener(move |ws, _, window, cx| ws.open_table(c.clone(), sn.clone(), tn.clone(), window, cx))),
                    );
                }
            }
        }

        v_flex()
            .size_full()
            .bg(theme.sidebar)
            .child(
                h_flex()
                    .p_2()
                    .gap_1()
                    .child(div().flex_1().child(Input::new(&self.tree_filter).small().cleanable(true)))
                    .child(
                        Button::new("disconnect")
                            .xsmall()
                            .ghost()
                            .label("닫기")
                            .on_click(cx.listener(|ws, _, _, cx| ws.disconnect(cx))),
                    ),
            )
            .child(list)
            .when(active.as_ref().is_some_and(|c| self.catalogs.contains_key(c)), |d| {
                d.child(
                    div()
                        .px_2()
                        .py_1()
                        .border_t_1()
                        .border_color(theme.border)
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("테이블"),
                )
                .child(objects)
            })
    }

    fn render_toolbar(&mut self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let conn = self.active_conn();
        let summary = conn.as_ref().and_then(|c| self.conn(c)).cloned();
        let in_tx = conn.as_ref().is_some_and(|c| self.tx.get(c).copied().unwrap_or(false));
        let is_prod = summary.as_ref().is_some_and(|s| tone(&s.env) == Tone::Prod);
        let is_pg = summary.as_ref().is_some_and(|s| s.kind == "postgresql");
        let locked = conn.as_ref().and_then(|c| self.locks.get(c)).is_none_or(|(l, _)| l.locked);
        h_flex()
            .gap_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(theme.border)
            .when(is_prod, |d| d.bg(theme.danger.opacity(0.12)))
            .child(match &summary {
                Some(s) => h_flex()
                    .gap_2()
                    .child(
                        div()
                            .text_color(if is_prod { theme.danger } else { theme.foreground })
                            .child(s.name.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(format!("{} · {}", s.env, s.db)),
                    ),
                None => h_flex().child(div().text_color(theme.muted_foreground).child("접속 없음 — 왼쪽 트리에서 고른다")),
            })
            .child(div().flex_1())
            .when(is_pg, |d| {
                d.child(
                    Button::new("run")
                        .small()
                        .primary()
                        .label("실행 ⌘↵")
                        .on_click(cx.listener(|ws, _, w, cx| ws.run_query(false, w, cx))),
                )
                .child(
                    Button::new("explain")
                        .small()
                        .ghost()
                        .label("EXPLAIN ⌘E")
                        .on_click(cx.listener(|ws, _, w, cx| ws.run_query(true, w, cx))),
                )
                .child(
                    Button::new("tx")
                        .small()
                        .with_variant(if in_tx { ButtonVariant::Warning } else { ButtonVariant::Ghost })
                        .label(if in_tx { "트랜잭션 중" } else { "BEGIN" })
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
                            .label("COMMIT")
                            .on_click(cx.listener(|ws, _, w, cx| ws.tx_op("COMMIT", w, cx))),
                    )
                    .child(
                        Button::new("rollback")
                            .small()
                            .ghost()
                            .label("ROLLBACK")
                            .on_click(cx.listener(|ws, _, w, cx| ws.tx_op("ROLLBACK", w, cx))),
                    )
                })
            })
            .when(is_prod, |d| {
                d.child(
                    Button::new("lock")
                        .small()
                        .with_variant(if locked { ButtonVariant::Ghost } else { ButtonVariant::Danger })
                        .label(if locked { "prod 잠김 ⌘L" } else { "prod 열림 ⌘L" })
                        .on_click(cx.listener(|ws, _, w, cx| ws.toggle_lock(w, cx))),
                )
            })
            .child(
                Button::new("open")
                    .small()
                    .ghost()
                    .label("열기 ⌘O")
                    .on_click(cx.listener(|ws, _, w, cx| ws.open_file(w, cx))),
            )
            .child(
                Button::new("save")
                    .small()
                    .ghost()
                    .label("저장 ⌘S")
                    .on_click(cx.listener(|ws, _, w, cx| ws.save_file(w, cx))),
            )
    }

    fn render_tabs(&mut self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let mut bar = h_flex()
            .id("tabs")
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
            bar = bar.child(
                h_flex()
                    .id(("tab", i))
                    .px_3()
                    .py_1()
                    .gap_2()
                    .border_r_1()
                    .border_color(theme.border)
                    .cursor_pointer()
                    .when(active, |d| d.bg(theme.background))
                    .when(prod, |d| d.border_t_2().border_color(theme.danger))
                    .child(div().child(t.title()))
                    .child(
                        div()
                            .id(("close", i))
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("×")
                            .on_click(cx.listener(move |ws, _, _, cx| {
                                cx.stop_propagation();
                                ws.close_tab(i, cx)
                            })),
                    )
                    .on_click(cx.listener(move |ws, _, _, cx| {
                        ws.tabs.active = i;
                        cx.notify();
                    })),
            );
        }
        bar.child(
            div()
                .id("new-tab")
                .px_3()
                .py_1()
                .cursor_pointer()
                .child("+")
                .on_click(cx.listener(|ws, _, w, cx| ws.new_tab(None, w, cx))),
        )
    }

    fn render_body(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let empty = |msg: &str| {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme.muted_foreground)
                .child(msg.to_string())
                .into_any_element()
        };
        let Some(t) = self.tabs.active() else {
            return empty("⌘T 로 새 탭을 연다");
        };
        match self.bodies.get(&t.id) {
            Some(Body::Sql { editor, result, .. }) => v_resizable(SharedString::from(format!("split-{}", t.id)))
                .child(resizable_panel().size(px(260.)).child(Editor::new(editor).h_full().bordered(false)))
                .child(resizable_panel().child(match result {
                    Some(r) => r.clone().into_any_element(),
                    None => empty("⌘↵ 로 커서가 든 문장을 실행한다 (선택이 있으면 선택)"),
                }))
                .into_any_element(),
            Some(Body::Table { result: Some(r) }) => r.clone().into_any_element(),
            Some(Body::Table { result: None }) => empty("읽는 중…"),
            Some(Body::Redis { view }) => view.clone().into_any_element(),
            None => empty(""),
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
        h_flex()
            .px_2()
            .py_0p5()
            .gap_3()
            .border_t_1()
            .border_color(theme.border)
            .text_xs()
            .child(
                div()
                    .text_color(theme.muted_foreground)
                    .child(self.target.as_ref().map(Target::label).unwrap_or_default()),
            )
            .when(in_tx, |d| {
                d.child(
                    div()
                        .px_1()
                        .rounded_sm()
                        .bg(theme.warning)
                        .text_color(theme.background)
                        .child("트랜잭션 중"),
                )
            })
            .when_some(lock, |d, l| d.child(div().text_color(theme.danger).child(l)))
            .when_some(self.busy.as_ref().map(|b| b.0.clone()), |d, label| {
                d.child(div().text_color(theme.muted_foreground).child(format!("{label} 중…")))
                    .child(
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
                    .text_color(if self.status.1 { theme.danger } else { theme.foreground })
                    .child(self.status.0.clone()),
            )
    }
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
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
            .on_action(cx.listener(|ws, _: &ToggleProdLock, w, cx| ws.toggle_lock(w, cx)))
            .on_action(cx.listener(|ws, _: &CancelRun, _, cx| ws.cancel(cx)))
            .on_action(cx.listener(|ws, _: &TxBegin, w, cx| ws.tx_op("BEGIN", w, cx)))
            .on_action(cx.listener(|ws, _: &TxCommit, w, cx| ws.tx_op("COMMIT", w, cx)))
            .on_action(cx.listener(|ws, _: &TxRollback, w, cx| ws.tx_op("ROLLBACK", w, cx)));
        if self.engine.is_none() {
            return root.child(self.render_picker(cx));
        }
        let sidebar = self.render_sidebar(cx);
        let toolbar = self.render_toolbar(cx);
        let tabs = self.render_tabs(cx);
        let body = self.render_body(cx);
        let status = self.render_status(cx);
        root.child(
            v_flex()
                .size_full()
                .child(
                    h_flex()
                        .flex_1()
                        .min_h_0()
                        .child(div().w(px(280.)).h_full().border_r_1().border_color(theme.border).child(sidebar))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .h_full()
                                .child(tabs)
                                .child(toolbar)
                                .child(div().flex_1().min_h_0().child(body)),
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
            let view = cx.new(|cx| Workspace::new(rt, window, cx));
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
        cx.update(|_, cx| {
            let w = ws.read(cx);
            assert!(matches!(w.bodies.get(&w.tabs.tabs[0].id), Some(Body::Sql { result: Some(_), .. })));
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
