//! 브랜드 마크 그리기와 마크 SVG 를 실은 자산 원천이다. GPUI 의 svg 는 한 색으로만 칠해,
//! 원통(잉크)과 점(강조)을 두 조각으로 겹쳐 그린다.

use std::borrow::Cow;

use gpui::{AssetSource, Hsla, IntoElement, ParentElement as _, Pixels, SharedString, Styled as _, div, svg};

const MARK_BODY: &[u8] = include_bytes!("../../../../assets/brand/mark-body.svg");
const MARK_DOT: &[u8] = include_bytes!("../../../../assets/brand/mark-dot.svg");

/// 앱 자산: 브랜드 마크 조각 + gpui-kit 아이콘
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        match path {
            "brand/mark-body.svg" => Ok(Some(Cow::Borrowed(MARK_BODY))),
            "brand/mark-dot.svg" => Ok(Some(Cow::Borrowed(MARK_DOT))),
            _ => gpui_kit_assets::Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        gpui_kit_assets::Assets.list(path)
    }
}

/// 마크(원통 위 점 = i). size 는 한 변이다.
pub fn mark(size: Pixels, ink: Hsla, accent: Hsla) -> impl IntoElement {
    div()
        .relative()
        .flex_shrink_0()
        .size(size)
        .child(
            svg()
                .path("brand/mark-body.svg")
                .absolute()
                .top_0()
                .left_0()
                .size(size)
                .text_color(ink),
        )
        .child(
            svg()
                .path("brand/mark-dot.svg")
                .absolute()
                .top_0()
                .left_0()
                .size(size)
                .text_color(accent),
        )
}

pub fn accent() -> Hsla {
    super::workspace::hex(crate::vm::brand::ACCENT)
}

/// 글자표 `i-db`: D2Coding Bold, 가운데 `-` 만 강조색이다. size 는 글자 크기다.
pub fn wordmark(size: Pixels, ink: Hsla) -> impl IntoElement {
    use gpui::FontWeight;
    gpui::div()
        .flex()
        .flex_row()
        .text_size(size)
        .font_weight(FontWeight::BOLD)
        .text_color(ink)
        .child("i")
        .child(gpui::div().text_color(accent()).child("-"))
        .child("db")
}
