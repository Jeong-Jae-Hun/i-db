//! Redis 탭. 왼쪽은 패턴으로 찾은 키(1,000개까지), 오른쪽은 고른 키의 값이고 아래 줄은 편집기 명령이다.
//! 명령은 `redis_run` 으로 가서 읽기 전용·prod·위험 판단을 엔진이 한다.

use std::rc::Rc;

use gpui::{
    AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _, Render, StatefulInteractiveElement as _,
    Styled as _, Subscription, WeakEntity, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use idb_core::sqlsafe::split_args;
use idb_engine::{Engine, ResultSet, ScanResult};

use super::results::{ResultsView, Source};
use super::workspace::Workspace;
use crate::vm::runtime::Runtime;
use crate::vm::safety::Confirms;

pub struct RedisView {
    ws: WeakEntity<Workspace>,
    rt: Rc<Runtime>,
    engine: Engine,
    conn: String,
    pattern: Entity<InputState>,
    command: Entity<InputState>,
    keys: Vec<String>,
    more: bool,
    selected: Option<String>,
    value: Option<Entity<ResultsView>>,
    _subs: Vec<Subscription>,
}

impl RedisView {
    pub fn new(
        ws: WeakEntity<Workspace>,
        rt: Rc<Runtime>,
        engine: Engine,
        conn: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> RedisView {
        let pattern = cx.new(|cx| InputState::new(window, cx).placeholder("키 패턴 (예: user:*)").default_value("*"));
        let command = cx.new(|cx| InputState::new(window, cx).placeholder("Redis 명령 (예: GET key) — Enter 또는 ⌘↵"));
        let subs = vec![
            cx.subscribe_in(&pattern, window, |this, _, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.scan(window, cx);
                }
            }),
            cx.subscribe_in(&command, window, |this, _, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.run_command(window, cx);
                }
            }),
        ];
        let mut v = RedisView {
            ws,
            rt,
            engine,
            conn,
            pattern,
            command,
            keys: Vec::new(),
            more: false,
            selected: None,
            value: None,
            _subs: subs,
        };
        v.scan(window, cx);
        v
    }

    fn scan(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (conn, pat) = (self.conn.clone(), self.pattern.read(cx).value().to_string());
        let this = cx.entity().downgrade();
        let _ = self.ws.update(cx, |ws, cx| {
            ws.guarded(
                "키 찾기",
                Rc::new(move |e: Engine, _| {
                    let (conn, pat) = (conn.clone(), pat.clone());
                    async move { e.redis_scan(&conn, &pat).await }
                }),
                Rc::new(move |ws: &mut Workspace, r: ScanResult, _, cx| {
                    ws.set_status(
                        format!("키 {}개{}", r.keys.len(), if r.more { " (더 있음)" } else { "" }),
                        false,
                        cx,
                    );
                    let _ = this.update(cx, |v, cx| {
                        v.keys = r.keys;
                        v.more = r.more;
                        cx.notify();
                    });
                }),
                Confirms::default(),
                window,
                cx,
            )
        });
    }

    fn open_key(&mut self, key: String, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(key.clone());
        let conn = self.conn.clone();
        let this = cx.entity().downgrade();
        let _ = self.ws.update(cx, |ws, cx| {
            ws.guarded(
                "값 읽기",
                Rc::new(move |e: Engine, _| {
                    let (conn, key) = (conn.clone(), key.clone());
                    async move { e.redis_value(&conn, &key).await }
                }),
                Rc::new(move |_: &mut Workspace, rs: ResultSet, window, cx| {
                    let _ = this.update(cx, |v, cx| v.show(rs, window, cx));
                }),
                Confirms::default(),
                window,
                cx,
            )
        });
        cx.notify();
    }

    fn show(&mut self, rs: ResultSet, window: &mut Window, cx: &mut Context<Self>) {
        let (ws, rt, engine, conn) = (self.ws.clone(), self.rt.clone(), self.engine.clone(), self.conn.clone());
        self.value = Some(cx.new(|cx| ResultsView::new(ws, rt, engine, conn, Source::Static, rs, false, window, cx)));
        cx.notify();
    }

    /// 명령 줄을 보낸다. prod 쓰기·위험 명령이면 엔진이 거절하고 확인 창을 거쳐 다시 보낸다.
    pub fn run_command(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let args = split_args(&self.command.read(cx).value());
        if args.is_empty() {
            return;
        }
        let conn = self.conn.clone();
        let this = cx.entity().downgrade();
        let _ = self.ws.update(cx, |ws, cx| {
            ws.guarded(
                "Redis 명령",
                Rc::new(move |e: Engine, c: Confirms| {
                    let (conn, args) = (conn.clone(), args.clone());
                    async move { e.redis_run(&conn, args, c.prod, c.danger).await }
                }),
                Rc::new(move |ws: &mut Workspace, rs: ResultSet, window, cx| {
                    ws.set_status(format!("{}ms", rs.elapsed_ms), false, cx);
                    let _ = this.update(cx, |v, cx| v.show(rs, window, cx));
                }),
                Confirms::default(),
                window,
                cx,
            )
        });
    }
}

impl Render for RedisView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let mut list = v_flex().id("redis-keys").flex_1().min_h_0().overflow_y_scroll();
        for (i, k) in self.keys.iter().enumerate() {
            let key = k.clone();
            let selected = self.selected.as_deref() == Some(k.as_str());
            list = list.child(
                div()
                    .id(("key", i))
                    .px_2()
                    .text_sm()
                    .truncate()
                    .cursor_pointer()
                    .when(selected, |d| d.bg(theme.list_active))
                    .hover(|s| s.bg(theme.list_hover))
                    .child(k.clone())
                    .on_click(cx.listener(move |v, _, w, cx| v.open_key(key.clone(), w, cx))),
            );
        }
        if self.more {
            list = list.child(
                div()
                    .px_2()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("1,000개까지 보인다. 패턴을 좁힌다"),
            );
        }
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        v_flex()
                            .w(px(300.))
                            .h_full()
                            .border_r_1()
                            .border_color(theme.border)
                            .child(
                                h_flex()
                                    .p_1()
                                    .gap_1()
                                    .child(div().flex_1().child(Input::new(&self.pattern).small()))
                                    .child(
                                        Button::new("scan")
                                            .xsmall()
                                            .label("찾기")
                                            .on_click(cx.listener(|v, _, w, cx| v.scan(w, cx))),
                                    ),
                            )
                            .child(list),
                    )
                    .child(
                        div().flex_1().min_w_0().h_full().child(match &self.value {
                            Some(v) => v.clone().into_any_element(),
                            None => div()
                                .p_3()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child("키를 고르면 값을 보인다")
                                .into_any_element(),
                        }),
                    ),
            )
            .child(
                h_flex()
                    .p_1()
                    .gap_1()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(div().flex_1().child(Input::new(&self.command).small()))
                    .child(
                        Button::new("redis-run")
                            .xsmall()
                            .primary()
                            .label("보내기")
                            .on_click(cx.listener(|v, _, w, cx| v.run_command(w, cx))),
                    ),
            )
    }
}
