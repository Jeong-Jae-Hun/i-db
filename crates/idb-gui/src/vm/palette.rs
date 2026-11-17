//! 화면 색. Orca(github.com/stablyai/orca, MIT)의 shadcn new-york·neutral 디자인 토큰 값을 gpui-component 테마
//! 키로 옮긴 것이다(THIRD_PARTY_NOTICES.md). 코드는 가져오지 않고 값만 쓴다.
//! Orca 의 `rgb(255 255 255 / 0.07)` 같은 반투명 선은 #RRGGBBAA 로 적는다.

use serde_json::{Map, Value, json};

use super::safety::Tone;

/// (gpui-component 테마 키, 값). 적지 않은 키는 gpui-component 의 shadcn neutral 기본값을 따른다
pub const DARK: &[(&str, &str)] = &[
    ("background", "#0a0a0a"),
    ("foreground", "#f0f0f0"),
    ("caret", "#f0f0f0"),
    ("border", "#ffffff12"),
    ("input.border", "#ffffff26"),
    ("ring", "#0d9488"),
    ("popover.background", "#171717"),
    ("popover.foreground", "#f0f0f0"),
    // 주 버튼·초점 고리는 브랜드 강조색(assets/brand)이다. 나머지는 Orca 무채색 그대로
    ("primary.background", "#0d9488"),
    ("primary.foreground", "#ffffff"),
    ("primary.hover.background", "#0f766e"),
    ("primary.active.background", "#115e59"),
    ("secondary.background", "#262626"),
    ("secondary.foreground", "#f0f0f0"),
    ("muted.background", "#262626"),
    ("muted.foreground", "#acacac"),
    ("accent.background", "#404040"),
    ("accent.foreground", "#f0f0f0"),
    ("sidebar.background", "#171717"),
    ("sidebar.foreground", "#f0f0f0"),
    ("sidebar.accent.background", "#262626"),
    ("sidebar.accent.foreground", "#f0f0f0"),
    ("sidebar.border", "#ffffff12"),
    ("sidebar.primary.background", "#1447e6"),
    ("sidebar.primary.foreground", "#f0f0f0"),
    ("list.background", "#00000000"),
    ("list.hover.background", "#262626"),
    ("list.active.background", "#404040"),
    ("list.active.border", "#00000000"),
    ("table.background", "#0a0a0a"),
    ("table.head.background", "#0a0a0a"),
    ("table.head.foreground", "#acacac"),
    ("table.even.background", "#ffffff05"),
    ("table.hover.background", "#ffffff0a"),
    ("table.active.background", "#262626"),
    ("table.active.border", "#737373"),
    ("table.row.border", "#ffffff12"),
    ("tab_bar.background", "#0a0a0a"),
    ("tab.background", "#00000000"),
    ("tab.active.background", "#171717"),
    ("tab.foreground", "#acacac"),
    ("tab.active.foreground", "#f0f0f0"),
    ("title_bar.background", "#0a0a0a"),
    ("title_bar.border", "#ffffff12"),
    ("status_bar.background", "#0a0a0a"),
    ("status_bar.border", "#ffffff12"),
    ("scrollbar.background", "#00000000"),
    ("scrollbar.thumb.background", "#ffffff1f"),
    ("scrollbar.thumb.hover.background", "#ffffff33"),
    ("danger.background", "#e40014"),
    ("danger.foreground", "#ffffff"),
    ("warning.background", "#f59e0b"),
    ("warning.foreground", "#0a0a0a"),
    ("success.background", "#16a34a"),
    ("window.border", "#ffffff12"),
];

pub const LIGHT: &[(&str, &str)] = &[
    ("background", "#ffffff"),
    ("foreground", "#0a0a0a"),
    ("caret", "#0a0a0a"),
    ("border", "#e5e5e5"),
    ("input.border", "#e5e5e5"),
    ("ring", "#0d9488"),
    ("popover.background", "#ffffff"),
    ("popover.foreground", "#0a0a0a"),
    ("primary.background", "#0d9488"),
    ("primary.foreground", "#ffffff"),
    ("primary.hover.background", "#0f766e"),
    ("primary.active.background", "#115e59"),
    ("secondary.background", "#f5f5f5"),
    ("secondary.foreground", "#171717"),
    ("muted.background", "#f5f5f5"),
    ("muted.foreground", "#696969"),
    ("accent.background", "#f5f5f5"),
    ("accent.foreground", "#171717"),
    ("sidebar.background", "#fafafa"),
    ("sidebar.foreground", "#0a0a0a"),
    ("sidebar.accent.background", "#f5f5f5"),
    ("sidebar.accent.foreground", "#171717"),
    ("sidebar.border", "#e5e5e5"),
    ("sidebar.primary.background", "#171717"),
    ("sidebar.primary.foreground", "#fafafa"),
    ("list.background", "#00000000"),
    ("list.hover.background", "#f5f5f5"),
    ("list.active.background", "#eaeaea"),
    ("list.active.border", "#00000000"),
    ("table.background", "#ffffff"),
    ("table.head.background", "#ffffff"),
    ("table.head.foreground", "#696969"),
    ("table.even.background", "#00000005"),
    ("table.hover.background", "#0000000a"),
    ("table.active.background", "#f5f5f5"),
    ("table.active.border", "#a1a1a1"),
    ("table.row.border", "#e5e5e5"),
    ("tab_bar.background", "#fafafa"),
    ("tab.background", "#00000000"),
    ("tab.active.background", "#ffffff"),
    ("tab.foreground", "#696969"),
    ("tab.active.foreground", "#0a0a0a"),
    ("title_bar.background", "#ffffff"),
    ("title_bar.border", "#e5e5e5"),
    ("status_bar.background", "#fafafa"),
    ("status_bar.border", "#e5e5e5"),
    ("scrollbar.background", "#00000000"),
    ("scrollbar.thumb.background", "#0000001f"),
    ("scrollbar.thumb.hover.background", "#00000033"),
    ("danger.background", "#e40014"),
    ("danger.foreground", "#ffffff"),
    ("warning.background", "#f59e0b"),
    ("warning.foreground", "#0a0a0a"),
    ("success.background", "#16a34a"),
    ("window.border", "#e5e5e5"),
];

/// Orca `--radius: 0.625rem`(10px)의 md(×0.8)·lg(×1) 단계다
pub const RADIUS: usize = 8;
pub const RADIUS_LG: usize = 10;

/// gpui-component `ThemeConfig` 꼴 JSON 이다. 화면 쪽이 그대로 읽어 밝은·어두운 테마로 등록한다.
pub fn theme_config(dark: bool) -> Value {
    let colors: Map<String, Value> = (if dark { DARK } else { LIGHT })
        .iter()
        .map(|(k, v)| (k.to_string(), Value::from(*v)))
        .collect();
    json!({
        "name": if dark { "i-db Dark" } else { "i-db Light" },
        "mode": if dark { "dark" } else { "light" },
        "radius": RADIUS,
        "radius.lg": RADIUS_LG,
        "shadow": false,
        "colors": colors,
    })
}

/// 환경 배지 색: (배경, 글자). prod 는 빨강, stg 는 호박색 기운의 알약이고 local 은 무채색이다.
/// 배경은 같은 색을 옅게(알파) 깔아 어느 테마에서나 튀지 않게 한다.
pub fn pill(t: Tone, dark: bool) -> (&'static str, &'static str) {
    match (t, dark) {
        (Tone::Prod, true) => ("#e4001433", "#ff6b6b"),
        (Tone::Prod, false) => ("#e400141a", "#c10010"),
        (Tone::Stg, true) => ("#f59e0b33", "#fbbf24"),
        (Tone::Stg, false) => ("#f59e0b1f", "#b45309"),
        (Tone::Local, true) => ("#262626", "#acacac"),
        (Tone::Local, false) => ("#f5f5f5", "#696969"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(p: &'static [(&'static str, &'static str)], k: &str) -> &'static str {
        p.iter()
            .find(|(key, _)| *key == k)
            .map(|(_, v)| *v)
            .unwrap_or_else(|| panic!("{k} 없음"))
    }

    #[test]
    fn 어두운_테마는_Orca_토큰_값을_쓴다() {
        assert_eq!(get(DARK, "background"), "#0a0a0a");
        assert_eq!(get(DARK, "foreground"), "#f0f0f0");
        assert_eq!(get(DARK, "popover.background"), "#171717");
        assert_eq!(get(DARK, "secondary.background"), "#262626");
        assert_eq!(get(DARK, "muted.background"), "#262626");
        assert_eq!(get(DARK, "muted.foreground"), "#acacac");
        assert_eq!(get(DARK, "accent.background"), "#404040");
        assert_eq!(get(DARK, "sidebar.background"), "#171717");
        // rgb(255 255 255 / 0.07) = 흰색 알파 0x12
        assert_eq!(get(DARK, "border"), "#ffffff12");
        assert_eq!(get(DARK, "danger.background"), "#e40014");
    }

    #[test]
    fn 브랜드_강조는_주_버튼과_초점_고리에만_쓴다() {
        let accent = crate::vm::brand::ACCENT;
        for p in [DARK, LIGHT] {
            let using: Vec<&str> = p.iter().filter(|(_, v)| v.eq_ignore_ascii_case(accent)).map(|(k, _)| *k).collect();
            assert_eq!(using, vec!["ring", "primary.background"], "{using:?}");
            assert_eq!(get(p, "primary.hover.background"), crate::vm::brand::ACCENT_HOVER);
        }
    }

    #[test]
    fn 밝은_테마도_Orca_토큰_값을_쓴다() {
        assert_eq!(get(LIGHT, "background"), "#ffffff");
        assert_eq!(get(LIGHT, "foreground"), "#0a0a0a");
        assert_eq!(get(LIGHT, "border"), "#e5e5e5");
        assert_eq!(get(LIGHT, "muted.foreground"), "#696969");
        assert_eq!(get(LIGHT, "sidebar.background"), "#fafafa");
    }

    #[test]
    fn 두_테마는_같은_키를_갖고_값은_모두_16진_색이다() {
        let keys = |p: &'static [(&'static str, &'static str)]| p.iter().map(|(k, _)| *k).collect::<Vec<_>>();
        assert_eq!(keys(DARK), keys(LIGHT));
        for (k, v) in DARK.iter().chain(LIGHT) {
            let hex = v.strip_prefix('#').unwrap_or_else(|| panic!("{k}={v}"));
            assert!(matches!(hex.len(), 6 | 8) && hex.chars().all(|c| c.is_ascii_hexdigit()), "{k}={v}");
        }
    }

    #[test]
    fn 테마_JSON_은_모드와_반지름과_색을_싣는다() {
        let v = theme_config(true);
        assert_eq!(v["mode"], "dark");
        assert_eq!(v["radius"], 8);
        assert_eq!(v["radius.lg"], 10);
        assert_eq!(v["colors"]["background"], "#0a0a0a");
        assert_eq!(theme_config(false)["mode"], "light");
    }

    #[test]
    fn 환경_배지는_prod_빨강_stg_호박_local_무채색이다() {
        assert!(pill(Tone::Prod, true).1.starts_with("#ff"));
        assert_eq!(pill(Tone::Stg, false).0, "#f59e0b1f");
        assert_eq!(pill(Tone::Local, true), ("#262626", "#acacac"));
    }
}
