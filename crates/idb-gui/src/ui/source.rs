//! 읽기 전용 저장소 탭(OpenSearch·Loki·Prometheus·Grafana·DynamoDB·Athena·aws). 왼쪽은 `source_children` 트리,
//! 오른쪽은 질의 편집기·범위·결과다. 읽기 전용·비싼 질의·요금 판단은 엔진이 하고 화면은 거절 사유를 창·상태 줄로 바꾼다.

use std::rc::Rc;

use gpui::{
    AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _, Render, StatefulInteractiveElement as _,
    Styled as _, WeakEntity, Window, div, prelude::FluentBuilder as _, px, rems,
};
use gpui_component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_component::input::{Editor, EditorState};
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use idb_engine::{Engine, Node, ResultSet, SourceRunRequest};

use super::results::{ResultsView, Source};
use super::workspace::Workspace;
use crate::vm::runtime::Runtime;
use crate::vm::safety::Confirms;
use crate::vm::source::{self, SourceTree};

pub struct SourceView {
    ws: WeakEntity<Workspace>,
    rt: Rc<Runtime>,
    engine: Engine,
    pub conn: String,
    pub kind: String,
    pub(super) tree: SourceTree,
    editor: Entity<EditorState>,
    pub(super) range: usize,
    selected: Option<String>,
    value: Option<Entity<ResultsView>>,
}

impl SourceView {
    /// 작업 공간 일은 미룬다(이 뷰는 작업 공간 콜백 안에서 만들어진다, redis.rs 와 같은 까닭).
    fn with_ws(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
    ) {
        let ws = self.ws.clone();
        window.defer(cx, move |window, cx| {
            let _ = ws.update(cx, |ws, cx| f(ws, window, cx));
        });
    }

    pub fn new(
        ws: WeakEntity<Workspace>,
        rt: Rc<Runtime>,
        engine: Engine,
        conn: String,
        kind: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> SourceView {
        let ph = source::placeholder(&kind);
        let editor = cx.new(|cx| EditorState::new(window, cx).placeholder(ph));
        cx.defer_in(window, |v, window, cx| v.load_children(String::new(), window, cx));
        SourceView {
            ws,
            rt,
            engine,
            conn,
            kind,
            tree: SourceTree::default(),
            editor,
            range: source::DEFAULT_RANGE,
            selected: None,
            value: None,
        }
    }

    fn load_children(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        let (conn, this) = (self.conn.clone(), cx.entity().downgrade());
        self.with_ws(window, cx, move |ws, window, cx| {
            let p2 = path.clone();
            ws.guarded(
                "트리 읽기",
                Rc::new(move |e: Engine, _| {
                    let (conn, path) = (conn.clone(), p2.clone());
                    async move { e.source_children(&conn, &path).await }
                }),
                Rc::new(move |_: &mut Workspace, nodes: Vec<Node>, _, cx| {
                    let _ = this.update(cx, |v, cx| {
                        v.tree.set_children(&path, nodes);
                        cx.notify();
                    });
                }),
                Confirms::default(),
                window,
                cx,
            )
        });
    }

    fn toggle(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.tree.toggle(&path) {
            self.load_children(path, window, cx);
        }
        cx.notify();
    }

    pub(super) fn open(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(path.clone());
        let (conn, this) = (self.conn.clone(), cx.entity().downgrade());
        self.with_ws(window, cx, move |ws, window, cx| {
            ws.guarded(
                "미리 보기",
                Rc::new(move |e: Engine, _| {
                    let (conn, path) = (conn.clone(), path.clone());
                    async move { e.source_open(&conn, &path).await }
                }),
                Rc::new(move |ws: &mut Workspace, rs: ResultSet, window, cx| {
                    ws.set_status(format!("{}행 · {}ms", rs.rows.len(), rs.elapsed_ms), false, cx);
                    let _ = this.update(cx, |v, cx| v.show(rs, window, cx));
                }),
                Confirms::default(),
                window,
                cx,
            )
        });
        cx.notify();
    }

    /// 편집기 글을 그 저장소의 질의 말로 보낸다. 비싼 질의는 확인 뒤 force 로 다시 보낸다(R60).
    pub fn run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        let (since_ns, until_ns) = if source::uses_range(&self.kind) {
            let now = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
            source::range_ns(now, source::RANGES[self.range].1)
        } else {
            (0, 0)
        };
        let (conn, this) = (self.conn.clone(), cx.entity().downgrade());
        self.with_ws(window, cx, move |ws, window, cx| {
            ws.guarded(
                "질의",
                Rc::new(move |e: Engine, c: Confirms| {
                    let req = SourceRunRequest {
                        conn: conn.clone(),
                        text: text.clone(),
                        since_ns,
                        until_ns,
                        force: c.expensive,
                        ..Default::default()
                    };
                    async move { e.source_run(req).await }
                }),
                Rc::new(move |ws: &mut Workspace, rs: ResultSet, window, cx| {
                    ws.set_status(format!("{}행 · {}ms", rs.rows.len(), rs.elapsed_ms), false, cx);
                    let _ = this.update(cx, |v, cx| v.show(rs, window, cx));
                }),
                Confirms::default(),
                window,
                cx,
            )
        });
    }

    fn show(&mut self, rs: ResultSet, window: &mut Window, cx: &mut Context<Self>) {
        let (ws, rt, engine, conn) = (self.ws.clone(), self.rt.clone(), self.engine.clone(), self.conn.clone());
        self.value = Some(cx.new(|cx| {
            ResultsView::new(
                ws,
                rt,
                engine,
                conn,
                Source::Static,
                rs,
                false,
                crate::vm::settings::DEFAULT_ROW_LIMIT,
                window,
                cx,
            )
        }));
        cx.notify();
    }
}

impl Render for SourceView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let sc = super::kit::scale(&theme);
        let mut list = v_flex().id("source-tree").flex_1().min_h_0().overflow_y_scroll().py_1().gap_px();
        let rows = self.tree.rows();
        if rows.is_empty() {
            list = list.child(div().px_3().py_2().text_xs().text_color(theme.muted_foreground).child("읽는 중…"));
        }
        for (i, row) in rows.into_iter().enumerate() {
            let n = row.node.clone();
            let (p_toggle, p_open) = (n.path.clone(), n.path.clone());
            let selected = self.selected.as_deref() == Some(n.path.as_str());
            list = list.child(
                h_flex()
                    .id(("src", i))
                    .mx_1()
                    .pl(rems(0.5 + row.depth as f32 * 0.9))
                    .pr_2()
                    .min_h(px(sc.row()))
                    .py(px(sc.s4()))
                    .gap_1()
                    .rounded(theme.radius)
                    .cursor_pointer()
                    .when(selected, |d| d.bg(theme.sidebar_accent))
                    .hover(|s| s.bg(theme.sidebar_accent))
                    .child(div().w(rems(1.0)).when(n.has_children, |d| {
                        d.child(
                            Icon::new(if row.expanded {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            })
                            .xsmall(),
                        )
                    }))
                    .child(div().flex_1().truncate().child(n.name.clone()))
                    .child(
                        div()
                            .text_xs()
                            .text_color(if n.warn { theme.warning } else { theme.muted_foreground })
                            .child(if row.loading { "…".to_string() } else { n.info.clone() }),
                    )
                    .on_click(cx.listener(move |v, _, window, cx| {
                        if n.openable {
                            v.open(p_open.clone(), window, cx);
                        } else if n.has_children {
                            v.toggle(p_toggle.clone(), window, cx);
                        }
                    })),
            );
        }
        let mut ranges = h_flex().gap_px();
        if source::uses_range(&self.kind) {
            for (i, (label, _)) in source::RANGES.iter().enumerate() {
                ranges = ranges.child(
                    Button::new(("range", i))
                        .with_variant(if i == self.range {
                            ButtonVariant::Primary
                        } else {
                            ButtonVariant::Ghost
                        })
                        .label(*label)
                        .on_click(cx.listener(move |v, _, _, cx| {
                            v.range = i;
                            cx.notify();
                        })),
                );
            }
        }
        h_flex()
            .size_full()
            .child(
                v_flex()
                    .w(px(300.))
                    .h_full()
                    .border_r_1()
                    .border_color(theme.border)
                    .bg(theme.sidebar)
                    .child(
                        h_flex()
                            .px_3()
                            .py_1()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(div().flex_1().child(format!("{} · 읽기 전용", self.kind)))
                            .child(
                                Button::new("src-refresh")
                                    .ghost()
                                    .icon(IconName::RefreshCw)
                                    .tooltip("다시 읽기")
                                    .on_click(cx.listener(|v, _, window, cx| {
                                        v.tree = SourceTree::default();
                                        v.load_children(String::new(), window, cx);
                                    })),
                            ),
                    )
                    .child(list),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(
                        h_flex()
                            .gap_1()
                            .px_2()
                            .min_h(px(sc.hit() + sc.s8()))
                            .border_b_1()
                            .border_color(theme.border)
                            .child(ranges)
                            .child(div().flex_1())
                            .child(
                                Button::new("src-run")
                                    .ghost()
                                    .icon(IconName::Play)
                                    .label("실행")
                                    .tooltip("질의 실행 (⌘↵)")
                                    .on_click(cx.listener(|v, _, w, cx| v.run(w, cx))),
                            ),
                    )
                    .child(
                        div()
                            .h(px(140.))
                            .border_b_1()
                            .border_color(theme.border)
                            .child(Editor::new(&self.editor).h_full().bordered(false)),
                    )
                    .child(
                        div().flex_1().min_h_0().child(match &self.value {
                            Some(v) => v.clone().into_any_element(),
                            None => v_flex()
                                .size_full()
                                .items_center()
                                .justify_center()
                                .text_color(theme.muted_foreground)
                                .child(crate::vm::copy::EMPTY_SOURCE)
                                .into_any_element(),
                        }),
                    ),
            )
    }
}
