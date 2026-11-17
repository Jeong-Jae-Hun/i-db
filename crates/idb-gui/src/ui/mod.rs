//! GPUI 화면. 무엇을 보일지는 `vm` 이 정하고 여기서는 그리고 입력을 넘긴다.

mod redis;
mod results;
mod sql_completion;
mod workspace;

use std::rc::Rc;

use gpui::{App, AppContext as _, Bounds, KeyBinding, Menu, MenuItem, TitlebarOptions, WindowBounds, WindowOptions, actions, px, size};
use gpui_component::{Root, Theme};

use crate::vm::runtime::{Runtime, Target};
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
