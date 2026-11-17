//! GPUI 화면. 무엇을 보일지는 `vm` 이 정하고 여기서는 그리고 입력을 넘긴다.

mod redis;
mod results;
mod sql_completion;
mod workspace;

use std::borrow::Cow;
use std::rc::Rc;

use gpui::{App, AppContext as _, Bounds, KeyBinding, Menu, MenuItem, TitlebarOptions, WindowBounds, WindowOptions, actions, px, size};
use gpui_component::{Root, Theme};

use crate::vm::font::{self, FontChoice};
use crate::vm::runtime::{Prefs, Runtime, Target};
use workspace::Workspace;

actions!(
    idb,
    [
        Quit,
        RunQuery,
        ExplainQuery,
        NewTab,
        CloseTab,
        OpenFile,
        SaveFile,
        ToggleProdLock,
        CancelRun,
        TxBegin,
        TxCommit,
        TxRollback
    ]
);

/// `i-db [-p 폴더 | --remote 이름]`. 인자가 없으면 고르는 화면으로 연다.
fn initial_target() -> Result<Option<Target>, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => Ok(None),
        [f, v] if f == "-p" => Ok(Some(Target::Local(v.into()))),
        [f, v] if f == "--remote" => Ok(Some(Target::Remote(v.clone()))),
        _ => Err("사용법: i-db [-p 프로젝트 폴더 | --remote 이름]".into()),
    }
}

/// gui.json 의 글꼴을 정하고, 설치돼 있지 않으면 넣어 둔 D2Coding 을 등록한다.
fn load_font(cx: &mut App) -> FontChoice {
    let prefs = Prefs::path().map(|p| Prefs::load(&p)).unwrap_or_default();
    let installed = cx.text_system().all_font_names();
    let mut choice = font::resolve(&prefs.font, prefs.font_size, &installed);
    if choice.embed {
        let fonts = vec![Cow::Borrowed(font::D2CODING_REGULAR), Cow::Borrowed(font::D2CODING_BOLD)];
        if let Err(e) = cx.text_system().add_fonts(fonts) {
            // 등록도 못 하면 gpui-component 기본 글꼴로 둔다
            eprintln!("i-db: D2Coding 등록 실패: {e}");
            choice.family = String::new();
        }
    }
    choice
}

/// 화면 글·편집기·결과 표가 같은 글꼴을 쓰게 테마에 넣는다.
fn apply_font(f: &FontChoice, cx: &mut App) {
    Theme::update(cx, |t| {
        if !f.family.is_empty() {
            t.font_family = f.family.clone().into();
            t.mono_font_family = f.family.clone().into();
        }
        t.font_size = px(f.size);
        t.mono_font_size = px(f.size);
    });
}

pub fn run() {
    let initial = match initial_target() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let rt = match Runtime::new() {
        Ok(rt) => Rc::new(rt),
        Err(e) => {
            eprintln!("i-db: {e}");
            std::process::exit(1);
        }
    };
    gpui_platform::application()
        .with_assets(gpui_kit_assets::Assets)
        .run(move |cx: &mut App| {
            gpui_component::init(cx);
            let font = load_font(cx);
            cx.bind_keys([
                KeyBinding::new("cmd-q", Quit, None),
                // 편집기(Input)가 cmd-enter 를 줄바꿈으로 먹지 않게 같은 맥락에 나중에 묶는다
                KeyBinding::new("cmd-enter", RunQuery, Some("Input")),
                KeyBinding::new("cmd-enter", RunQuery, Some("Workspace")),
                KeyBinding::new("cmd-e", ExplainQuery, Some("Workspace")),
                KeyBinding::new("cmd-t", NewTab, Some("Workspace")),
                KeyBinding::new("cmd-w", CloseTab, Some("Workspace")),
                KeyBinding::new("cmd-o", OpenFile, Some("Workspace")),
                KeyBinding::new("cmd-s", SaveFile, Some("Workspace")),
                KeyBinding::new("cmd-l", ToggleProdLock, Some("Workspace")),
                KeyBinding::new("cmd-.", CancelRun, Some("Workspace")),
            ]);
            cx.on_action(|_: &Quit, cx| cx.quit());
            cx.set_menus([Menu {
                name: "i-db".into(),
                items: vec![
                    MenuItem::action("새 탭", NewTab),
                    MenuItem::action("열기…", OpenFile),
                    MenuItem::action("저장", SaveFile),
                    MenuItem::separator(),
                    MenuItem::action("BEGIN", TxBegin),
                    MenuItem::action("COMMIT", TxCommit),
                    MenuItem::action("ROLLBACK", TxRollback),
                    MenuItem::action("prod 잠금 바꾸기", ToggleProdLock),
                    MenuItem::separator(),
                    MenuItem::action("끝내기", Quit),
                ],
                disabled: false,
            }]);
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            let opts = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(1320.), px(860.)), cx))),
                titlebar: Some(TitlebarOptions {
                    title: Some("i-db".into()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let rt = rt.clone();
            let opened = cx.open_window(opts, move |window, cx| {
                Theme::sync_system_appearance(Some(window), cx);
                apply_font(&font, cx);
                let ws = cx.new(|cx| {
                    let mut ws = Workspace::new(rt, window, cx);
                    if let Some(t) = initial {
                        ws.open_target(t, window, cx);
                    }
                    ws
                });
                cx.new(|cx| Root::new(ws, window, cx))
            });
            if let Err(e) = opened {
                eprintln!("i-db: {e}");
                cx.quit();
            }
            cx.activate(true);
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use gpui_component::ActiveTheme as _;

    #[gpui::test]
    fn 기본_글꼴을_테마의_글과_편집기에_넣는다(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            let f = font::resolve(font::DEFAULT_FONT, font::DEFAULT_SIZE, &[]);
            apply_font(&f, cx);
            let t = cx.theme();
            assert_eq!(t.font_family.as_ref(), "D2Coding");
            assert_eq!(t.mono_font_family.as_ref(), "D2Coding");
            assert_eq!((t.font_size, t.mono_font_size), (px(13.), px(13.)));
        });
    }
}
