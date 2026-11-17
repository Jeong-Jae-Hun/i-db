//! 설정(⌘,)·단축키(⌘/)·정보 창. 안전 규칙은 엔진이 정해 설정 창에서도 읽기만 한다.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{AppContext as _, Context, ParentElement as _, Styled as _, Window, div, px};
use gpui_component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_component::input::{Input, InputState};
use gpui_component::{ActiveTheme as _, WindowExt as _, h_flex, v_flex};

use super::workspace::Workspace;
use crate::vm::keymap::{MenuName, display_key, entries_in};
use crate::vm::settings::{SAFETY_RULES, ThemePref, parse_row_limit};

pub fn open_settings(ws: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let p = ws.prefs.clone();
    let font = cx.new(|cx| InputState::new(window, cx).default_value(p.font.clone()));
    let size = cx.new(|cx| InputState::new(window, cx).default_value(format!("{}", p.font_size)));
    let rows = cx.new(|cx| InputState::new(window, cx).default_value(p.row_limit.to_string()));
    let theme = Rc::new(Cell::new(p.theme));
    let this = cx.entity().downgrade();
    window.open_dialog(cx, move |dialog, _, cx| {
        let t = cx.theme().clone();
        let label = |s: &str| div().w(px(120.)).text_color(t.muted_foreground).child(s.to_string());
        let row = |name: &str| h_flex().gap_3().py_1().child(label(name));
        let mut themes = h_flex().gap_1();
        for (i, choice) in ThemePref::ALL.into_iter().enumerate() {
            let cell = theme.clone();
            themes = themes.child(
                Button::new(("theme", i))
                    .with_variant(if theme.get() == choice {
                        ButtonVariant::Primary
                    } else {
                        ButtonVariant::Ghost
                    })
                    .label(choice.label())
                    .on_click(move |_, window, _| {
                        cell.set(choice);
                        window.refresh();
                    }),
            );
        }
        let mut rules = v_flex().gap_1();
        for r in SAFETY_RULES {
            rules = rules.child(div().text_xs().text_color(t.muted_foreground).child(format!("· {r}")));
        }
        let (font, size, rows, theme, this, base) = (font.clone(), size.clone(), rows.clone(), theme.clone(), this.clone(), p.clone());
        let (font_i, size_i, rows_i) = (font.clone(), size.clone(), rows.clone());
        dialog
            .title("설정")
            .w(px(560.))
            .child(row("글꼴").child(div().flex_1().child(Input::new(&font_i))))
            .child(row("글자 크기(px)").child(div().w(px(100.)).child(Input::new(&size_i))))
            .child(row("테마").child(themes))
            .child(row("테이블 한 쪽 행 수").child(div().w(px(100.)).child(Input::new(&rows_i))))
            .child(div().pt_3().pb_1().text_color(t.muted_foreground).child("안전 규칙 (바꿀 수 없음)"))
            .child(rules)
            .child(
                h_flex()
                    .pt_3()
                    .gap_2()
                    .child(div().flex_1())
                    .child(
                        Button::new("settings-cancel")
                            .label("취소")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(Button::new("settings-ok").primary().label("저장").on_click(move |_, window, cx| {
                        let mut next = base.clone();
                        next.font = font.read(cx).value().trim().to_string();
                        if let Ok(v) = size.read(cx).value().trim().parse::<f32>() {
                            next.font_size = crate::vm::font::resolve(&next.font, v, &[]).size;
                        }
                        if let Some(n) = parse_row_limit(&rows.read(cx).value()) {
                            next.row_limit = n;
                        }
                        next.theme = theme.get();
                        let _ = this.update(cx, |ws, cx| ws.apply_settings(next, window, cx));
                        window.close_dialog(cx);
                    })),
            )
    });
}

pub fn open_shortcuts(window: &mut Window, cx: &mut Context<Workspace>) {
    window.open_dialog(cx, |dialog, _, cx| {
        let t = cx.theme().clone();
        let mut body = v_flex().gap_1();
        for m in MenuName::ALL {
            let keyed: Vec<_> = entries_in(m).filter(|e| e.key.is_some()).collect();
            if keyed.is_empty() {
                continue;
            }
            body = body.child(div().pt_2().text_xs().text_color(t.muted_foreground).child(m.title()));
            for e in keyed {
                body = body.child(
                    h_flex().gap_3().child(div().flex_1().child(e.label)).child(
                        div()
                            .px_2()
                            .rounded(t.radius)
                            .bg(t.secondary)
                            .child(display_key(e.key.unwrap_or_default())),
                    ),
                );
            }
        }
        dialog.title("단축키").w(px(460.)).child(body).child(
            div()
                .pt_2()
                .text_xs()
                .text_color(t.muted_foreground)
                .child("표: 칸을 두 번 눌러 고치고, 오른쪽 단추로 복사·내보내기"),
        )
    });
}

pub fn open_about(window: &mut Window, cx: &mut Context<Workspace>) {
    use crate::vm::brand;
    window.open_dialog(cx, |dialog, _, cx| {
        let t = cx.theme().clone();
        let muted = |s: String| div().text_xs().text_color(t.muted_foreground).child(s);
        dialog.w(px(420.)).child(
            v_flex()
                .items_center()
                .gap_2()
                .py_4()
                .child(super::brand::mark(px(56.), t.foreground, super::brand::accent()))
                .child(super::brand::wordmark(px(28.), t.foreground))
                .child(div().text_color(t.muted_foreground).child(brand::TAGLINE))
                .child(muted(format!("판 {}", brand::version())))
                .child(div().pt_3().text_xs().text_color(t.primary).child(brand::REPO))
                .child(
                    v_flex()
                        .pt_3()
                        .gap_1()
                        .items_center()
                        .child(muted("글꼴 D2Coding — NAVER, SIL Open Font License 1.1".into()))
                        .child(muted("색·간격 토큰 — Orca (MIT, Lovecast Inc.)".into()))
                        .child(muted("UI — GPUI · gpui-component (Apache-2.0)".into()))
                        .child(muted("전문: THIRD_PARTY_NOTICES.md".into())),
                ),
        )
    });
}
