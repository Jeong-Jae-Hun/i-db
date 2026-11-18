//! 눈금(vm::layout::Scale)으로 만든 공통 조각이다. 목록 줄은 고정 높이가 아니라 "최소 높이 + 위아래 여백"이라
//! 글이 커져도(확대) 줄이 내용만큼 늘어나 다음 줄과 겹치지 않는다.

use gpui::{Div, FontWeight, Hsla, ParentElement as _, Styled as _, div, prelude::FluentBuilder as _, px, svg};
use gpui_component::{Theme, h_flex};

use crate::vm::layout::Scale;

/// 테마 글자 크기(확대 × 설정 크기가 이미 곱해진 값)로 눈금을 얻는다. 작업 공간이 없는 뷰가 쓴다.
pub fn scale(theme: &Theme) -> Scale {
    Scale::with_font(1.0, f32::from(theme.font_size))
}

/// 한 줄 목록 줄(테이블·쿼리·키·트리 줄)
pub fn row(s: &Scale, theme: &Theme, selected: bool) -> Div {
    h_flex()
        .min_h(px(s.row()))
        .py(px(s.s4()))
        .px(px(s.s8()))
        .gap(px(s.s8()))
        .rounded(theme.radius)
        .text_size(px(s.text()))
        .line_height(px(s.line(s.text())))
        // 고른 줄은 강조색을 옅게 깐다(초점·고른 탭과 같은 색)
        .when(selected, |d| d.bg(super::brand::accent().opacity(0.16)))
}

/// 구역 머리(접속·테이블·쿼리): 11px 굵게, 흐린 색
pub fn section(s: &Scale, theme: &Theme, title: impl Into<gpui::SharedString>) -> Div {
    div()
        .px(px(s.s12()))
        .pt(px(s.s12()))
        .pb(px(s.s4()))
        .text_size(px(s.section()))
        .line_height(px(s.line(s.section())))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme.muted_foreground)
        .child(title.into())
}

/// 글 크기·줄 높이를 정한 한 줄 글
pub fn text(s: &Scale, size: f32, color: Hsla, t: impl Into<gpui::SharedString>) -> Div {
    div()
        .text_size(px(size))
        .line_height(px(s.line(size)))
        .text_color(color)
        .truncate()
        .child(t.into())
}

/// 접속 종류 아이콘(Lucide, assets/icons/kinds). 모르는 종류는 빈 자리를 둔다(줄이 어긋나지 않게).
pub fn kind_icon(s: &Scale, kind: &str, color: Hsla) -> Div {
    let size = px(s.icon());
    let d = div().flex_shrink_0().size(size);
    match super::brand::kind_icon_path(kind) {
        Some(p) => d.child(svg().path(p).size(size).text_color(color)),
        None => d,
    }
}

/// 상태 점(환경·건강). 글 높이 가운데에 앉는 6px 원이다
pub fn dot(s: &Scale, color: Hsla) -> Div {
    let _ = s;
    div().flex_shrink_0().size(px(6.)).rounded_full().bg(color)
}
