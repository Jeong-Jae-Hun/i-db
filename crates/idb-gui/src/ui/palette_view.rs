//! 명령 팔레트(⌘K·⇧⌘P)와 테이블로 가기(⌘P) 창이다. 고르기·순위는 vm::finder 가 하고, 여기는 그리기와 키만 맡는다.
//! 고른 항목은 작업 공간이 실행한다(확인이 필요한 명령은 작업 공간의 guarded 흐름을 그대로 탄다).

use gpui::{
    AppContext as _, Context, Entity, FocusHandle, Focusable, FontWeight, HighlightStyle, InteractiveElement as _, IntoElement,
    KeyDownEvent, ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, StyledText, Subscription, WeakEntity, Window,
    div, prelude::FluentBuilder as _, px,
};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{ActiveTheme as _, Icon, IconName, h_flex, v_flex};

use super::kit;
use super::workspace::Workspace;
use crate::vm::finder::{self, Group, Hit, Item, Uses};

pub struct PaletteView {
    ws: WeakEntity<Workspace>,
    input: Entity<InputState>,
    items: Vec<Item>,
    uses: Uses,
    hits: Vec<Hit>,
    selected: usize,
    _sub: Subscription,
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

impl PaletteView {
    pub fn new(
        ws: WeakEntity<Workspace>,
        items: Vec<Item>,
        uses: Uses,
        placeholder: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> PaletteView {
        let ph = placeholder.to_string();
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(ph));
        let sub = cx.subscribe_in(&input, window, |this, _, ev: &InputEvent, window, cx| match ev {
            InputEvent::Change => this.refilter(cx),
            InputEvent::PressEnter { .. } => this.confirm(false, window, cx),
            _ => {}
        });
        input.update(cx, |i, cx| i.focus(window, cx));
        let hits = finder::rank(&items, "", &uses, now());
        PaletteView {
            ws,
            input,
            items,
            uses,
            hits,
            selected: 0,
            _sub: sub,
        }
    }

    /// 질의를 넣는다(화면 사진 모드·시험)
    pub fn set_query(&mut self, q: &str, window: &mut Window, cx: &mut Context<Self>) {
        let q = q.to_string();
        self.input.update(cx, |i, cx| i.set_value(q, window, cx));
        self.refilter(cx);
    }

    fn refilter(&mut self, cx: &mut Context<Self>) {
        let q = self.input.read(cx).value().to_string();
        self.hits = finder::rank(&self.items, &q, &self.uses, now());
        self.selected = 0;
        cx.notify();
    }

    fn move_by(&mut self, d: isize, cx: &mut Context<Self>) {
        if self.hits.is_empty() {
            return;
        }
        let n = self.hits.len() as isize;
        self.selected = ((self.selected as isize + d).rem_euclid(n)) as usize;
        cx.notify();
    }

    fn confirm(&mut self, alt: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(h) = self.hits.get(self.selected) else { return };
        let item = self.items[h.index].clone();
        let ws = self.ws.clone();
        // 작업 공간이 이 창을 닫고(이 엔터티를 버리고) 실행하므로 지금 대여가 끝난 뒤에 부른다
        window.defer(cx, move |window, cx| {
            let _ = ws.update(cx, |w, cx| w.palette_pick(item, alt, window, cx));
        });
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ws = self.ws.clone();
        window.defer(cx, move |window, cx| {
            let _ = ws.update(cx, |w, cx| w.close_palette(window, cx));
        });
    }

    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        match k.key.as_str() {
            "up" => self.move_by(-1, cx),
            "down" => self.move_by(1, cx),
            "enter" => self.confirm(k.modifiers.alt, window, cx),
            "escape" => self.close(window, cx),
            _ => return,
        }
        cx.stop_propagation();
    }
}

impl Focusable for PaletteView {
    fn focus_handle(&self, cx: &gpui::App) -> FocusHandle {
        self.input.read(cx).focus_handle(cx)
    }
}

fn group_icon(g: Group) -> Icon {
    match g {
        Group::Frequent => Icon::new(IconName::Star),
        Group::Command => Icon::empty().path("icons/square-terminal.svg"),
        Group::Conn => Icon::empty().path("kind/postgresql.svg"),
        Group::Table => Icon::empty().path("kind/dynamodb.svg"),
        Group::File => Icon::new(IconName::File),
        Group::Tab => Icon::empty().path("icons/panel-left.svg"),
        Group::Key => Icon::new(IconName::Folder),
    }
}

/// 글자 번호를 바이트 범위로 바꿔 맞은 글자를 강조한다.
fn highlighted(text: &str, hits: &[usize], color: gpui::Hsla) -> StyledText {
    let mut ranges = Vec::new();
    for (ci, (bi, ch)) in text.char_indices().enumerate() {
        if hits.contains(&ci) {
            ranges.push((
                bi..bi + ch.len_utf8(),
                HighlightStyle {
                    color: Some(color),
                    font_weight: Some(FontWeight::SEMIBOLD),
                    ..Default::default()
                },
            ));
        }
    }
    StyledText::new(text.to_string()).with_highlights(ranges)
}

impl Render for PaletteView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let sc = kit::scale(&theme);
        let accent = super::brand::accent();
        let mut list = v_flex().id("palette-list").max_h(px(440.)).overflow_y_scroll().py(px(sc.s4()));
        let mut last: Option<Group> = None;
        for (i, h) in self.hits.iter().enumerate() {
            let it = &self.items[h.index];
            if last != Some(h.group) {
                list = list.child(
                    div()
                        .px(px(sc.s12()))
                        .pt(px(if last.is_some() { sc.s12() } else { sc.s4() }))
                        .pb(px(sc.s4()))
                        .text_size(px(sc.section()))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.muted_foreground)
                        .child(h.group.title()),
                );
                last = Some(h.group);
            }
            let selected = i == self.selected;
            let env_color = match it.env.as_str() {
                "prod" => Some(theme.danger),
                "stg" => Some(theme.warning),
                _ => None,
            };
            list = list.child(
                h_flex()
                    .id(("pal", i))
                    .mx(px(sc.s4()))
                    .px(px(sc.s8()))
                    .min_h(px(sc.row()))
                    .gap(px(sc.s8()))
                    .rounded(theme.radius)
                    .cursor_pointer()
                    .when(selected, |d| d.bg(accent.opacity(0.16)))
                    .hover(|s| s.bg(theme.secondary))
                    .child(group_icon(it.group).size(px(sc.icon())).text_color(if selected {
                        theme.foreground
                    } else {
                        theme.muted_foreground
                    }))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(sc.text()))
                            .child(highlighted(&it.title, &h.hits, accent)),
                    )
                    .when_some(env_color, |d, c| d.child(kit::dot(&sc, c)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(sc.small()))
                            .text_color(theme.muted_foreground)
                            .child(it.detail.clone()),
                    )
                    .when(!it.hint.is_empty(), |d| {
                        d.child(
                            div()
                                .flex_shrink_0()
                                .px(px(sc.s4()))
                                .rounded(theme.radius)
                                .bg(theme.secondary)
                                .text_size(px(sc.small()))
                                .text_color(theme.muted_foreground)
                                .child(it.hint.clone()),
                        )
                    })
                    .on_click(cx.listener(move |v, _, window, cx| {
                        v.selected = i;
                        v.confirm(false, window, cx);
                    })),
            );
        }
        if self.hits.is_empty() {
            list = list.child(
                div()
                    .px(px(sc.s12()))
                    .py(px(sc.s12()))
                    .text_color(theme.muted_foreground)
                    .child(crate::vm::copy::EMPTY_PALETTE),
            );
        }
        // 바탕을 누르면 닫는다. 창은 가운데 위쪽, 그림자로 띄운다
        div()
            .id("palette-backdrop")
            .absolute()
            .inset_0()
            .bg(gpui::black().opacity(0.25))
            .flex()
            .justify_center()
            // 창은 내용 높이만큼만(늘이지 않는다)
            .items_start()
            .on_click(cx.listener(|v, _, window, cx| v.close(window, cx)))
            .child(
                v_flex()
                    .id("palette")
                    .mt(px(96.))
                    .w(px(640.))
                    .max_h(px(560.))
                    .bg(theme.popover)
                    .border_1()
                    .border_color(theme.border)
                    .rounded(px(10.))
                    .shadow_lg()
                    .overflow_hidden()
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .capture_key_down(cx.listener(Self::on_key))
                    .child(
                        h_flex()
                            .px(px(sc.s12()))
                            .py(px(sc.s8()))
                            .gap(px(sc.s8()))
                            .border_b_1()
                            .border_color(theme.border)
                            .child(Icon::new(IconName::Search).size(px(sc.icon())).text_color(theme.muted_foreground))
                            .child(div().flex_1().child(Input::new(&self.input).appearance(false))),
                    )
                    .child(list)
                    .child(
                        h_flex()
                            .px(px(sc.s12()))
                            .py(px(sc.s8()))
                            .gap(px(sc.s12()))
                            .border_t_1()
                            .border_color(theme.border)
                            .text_size(px(sc.small()))
                            .text_color(theme.muted_foreground)
                            .child("↑↓ 고르기")
                            .child("↵ 열기")
                            .child("⌥↵ 테이블 DDL")
                            .child(div().flex_1())
                            .child("esc 닫기"),
                    ),
            )
    }
}
