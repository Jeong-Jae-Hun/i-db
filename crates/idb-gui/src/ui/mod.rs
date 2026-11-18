//! GPUI 화면. 무엇을 보일지는 `vm` 이 정하고 여기서는 그리고 입력을 넘긴다.

mod appkit_guard;
mod brand;
mod dashboard;
mod dialogs;
mod kit;
mod palette_view;
mod redis;
mod results;
mod snapshot;
mod source;
mod sql_completion;
mod workspace;

use std::borrow::Cow;
use std::rc::Rc;

use gpui::{
    Action, App, AppContext as _, Bounds, KeyBinding, Menu, MenuItem, OsAction, TitlebarOptions, Window, WindowBounds, WindowOptions,
    actions, px, size,
};
use gpui_component::{Root, Theme, ThemeConfig, ThemeMode};

use crate::vm::font::{self, FontChoice};
use crate::vm::keymap::{Bind, Cmd, ENTRIES, EXTRA_KEYS, MenuName, entries_in};
use crate::vm::palette;
use crate::vm::runtime::{Prefs, Runtime, Target};
use crate::vm::settings::ThemePref;
use crate::vm::zoom;
use workspace::Workspace;

actions!(
    idb,
    [
        Quit,
        About,
        OpenSettings,
        NewTab,
        OpenFile,
        SaveFile,
        CloseTab,
        SwitchConn,
        ZoomIn,
        ZoomOut,
        ZoomReset,
        ToggleSidebar,
        ToggleResults,
        ShowDashboard,
        RunQuery,
        RunAll,
        ExplainQuery,
        CancelRun,
        TxBegin,
        TxCommit,
        TxRollback,
        ToggleProdLock,
        Minimize,
        ShowShortcuts,
        OpenPalette,
        GoToTable
    ]
);

/// 명령 목록(vm::keymap)의 항목을 GPUI 동작으로 바꾼다. 편집 명령은 입력칸·표가 받는 gpui-component 동작이다.
fn action_for(cmd: Cmd) -> Box<dyn Action> {
    use gpui_component::input as i;
    match cmd {
        Cmd::About => Box::new(About),
        Cmd::Settings => Box::new(OpenSettings),
        Cmd::Quit => Box::new(Quit),
        Cmd::NewTab => Box::new(NewTab),
        Cmd::OpenFile => Box::new(OpenFile),
        Cmd::SaveFile => Box::new(SaveFile),
        Cmd::CloseTab => Box::new(CloseTab),
        Cmd::SwitchConn => Box::new(SwitchConn),
        Cmd::Undo => Box::new(i::Undo),
        Cmd::Redo => Box::new(i::Redo),
        Cmd::Cut => Box::new(i::Cut),
        Cmd::Copy => Box::new(i::Copy),
        Cmd::Paste => Box::new(i::Paste),
        Cmd::SelectAll => Box::new(i::SelectAll),
        Cmd::Find => Box::new(i::Search),
        Cmd::ZoomIn => Box::new(ZoomIn),
        Cmd::ZoomOut => Box::new(ZoomOut),
        Cmd::ZoomReset => Box::new(ZoomReset),
        Cmd::ToggleSidebar => Box::new(ToggleSidebar),
        Cmd::ToggleResults => Box::new(ToggleResults),
        Cmd::Dashboard => Box::new(ShowDashboard),
        Cmd::Run => Box::new(RunQuery),
        Cmd::RunAll => Box::new(RunAll),
        Cmd::Explain => Box::new(ExplainQuery),
        Cmd::Cancel => Box::new(CancelRun),
        Cmd::TxBegin => Box::new(TxBegin),
        Cmd::TxCommit => Box::new(TxCommit),
        Cmd::TxRollback => Box::new(TxRollback),
        Cmd::ProdLock => Box::new(ToggleProdLock),
        Cmd::Minimize => Box::new(Minimize),
        Cmd::Shortcuts => Box::new(ShowShortcuts),
        Cmd::Palette => Box::new(OpenPalette),
        Cmd::GoToTable => Box::new(GoToTable),
    }
}

fn os_action(cmd: Cmd) -> Option<OsAction> {
    Some(match cmd {
        Cmd::Undo => OsAction::Undo,
        Cmd::Redo => OsAction::Redo,
        Cmd::Cut => OsAction::Cut,
        Cmd::Copy => OsAction::Copy,
        Cmd::Paste => OsAction::Paste,
        Cmd::SelectAll => OsAction::SelectAll,
        _ => return None,
    })
}

fn key_bindings() -> Vec<KeyBinding> {
    let bind = |key: &str, cmd: Cmd, ctx: Option<&str>| {
        let pred = ctx.and_then(|c| gpui::KeyBindingContextPredicate::parse(c).ok()).map(Rc::new);
        KeyBinding::load(key, action_for(cmd), pred, false, None, &gpui::DummyKeyboardMapper)
    };
    let mut out = Vec::new();
    for e in ENTRIES {
        let Some(key) = e.key else { continue };
        match e.bind {
            Bind::Global => out.push(bind(key, e.cmd, None)),
            Bind::Workspace => out.push(bind(key, e.cmd, Some("Workspace"))),
            // 편집기(Input)가 cmd-enter 를 줄바꿈으로 먹지 않게 같은 맥락에 나중에 묶는다
            Bind::WorkspaceOverInput => {
                out.push(bind(key, e.cmd, Some("Input")));
                out.push(bind(key, e.cmd, Some("Workspace")));
            }
            Bind::Builtin => {}
        }
    }
    for (key, cmd) in EXTRA_KEYS {
        out.push(bind(key, *cmd, None));
    }
    // 표에 초점이 있을 때 ⌘C 는 결과 복사다(ResultsView 가 받는다)
    out.push(bind("cmd-c", Cmd::Copy, Some("DataTable")));
    out.into_iter().flatten().collect()
}

fn menus() -> Vec<Menu> {
    MenuName::ALL
        .iter()
        .map(|&m| {
            let mut items = Vec::new();
            for e in entries_in(m) {
                if e.sep_before && !items.is_empty() {
                    items.push(MenuItem::separator());
                }
                items.push(MenuItem::Action {
                    name: e.label.into(),
                    action: action_for(e.cmd),
                    os_action: os_action(e.cmd),
                    checked: false,
                    disabled: false,
                });
            }
            Menu {
                name: m.title().into(),
                items,
                disabled: false,
            }
        })
        .collect()
}

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
fn load_font(prefs: &Prefs, cx: &mut App) -> FontChoice {
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

/// 화면 글·편집기·결과 표가 같은 글꼴을 쓰게 테마에 넣는다. 크기는 확대 배율을 곱한다.
fn apply_font(f: &FontChoice, ui: &str, z: f32, cx: &mut App) {
    let installed = if ui.eq_ignore_ascii_case(font::SYSTEM_UI) {
        Vec::new()
    } else {
        cx.text_system().all_font_names()
    };
    let ui_family = font::ui_family(ui, &installed);
    let size = px(zoom::font_px(f.size, z));
    Theme::update(cx, |t| {
        if !f.family.is_empty() {
            t.font_family = f.family.clone().into();
            t.mono_font_family = f.family.clone().into();
        }
        // 화면 글을 시스템 글꼴로 두면 고정폭은 편집기·표 값·코드에만 남는다
        t.font_family = ui_family.clone().into();
        t.font_size = size;
        t.mono_font_size = size;
    });
}

/// Orca 토큰 색(vm::palette)을 밝은·어두운 테마로 등록한다. 구문 강조는 gpui-component 기본 것을 그대로 둔다.
fn install_palette(cx: &mut App) {
    let parse = |dark: bool| serde_json::from_value::<ThemeConfig>(palette::theme_config(dark)).ok();
    let (Some(mut light), Some(mut dark)) = (parse(false), parse(true)) else {
        return;
    };
    let t = Theme::global(cx);
    light.highlight = t.light_theme.highlight.clone();
    dark.highlight = t.dark_theme.highlight.clone();
    Theme::update(cx, |t| {
        t.light_theme = Rc::new(light);
        t.dark_theme = Rc::new(dark);
    });
}

/// 테마 모드·글꼴·확대를 한 번에 맞춘다. 설정 창·확대·macOS 밝기 바꿈이 부른다.
fn apply_look(prefs: &Prefs, font: &FontChoice, window: &mut Window, cx: &mut App) {
    match prefs.theme {
        ThemePref::System => Theme::sync_system_appearance(Some(window), cx),
        ThemePref::Light => Theme::change(ThemeMode::Light, Some(window), cx),
        ThemePref::Dark => Theme::change(ThemeMode::Dark, Some(window), cx),
    }
    apply_font(font, &prefs.ui_font, prefs.zoom, cx);
    // rem 을 키워 여백·높이·작은 글(text_xs)도 같은 배율로 커지게 한다
    window.set_rem_size(px(zoom::rem_px(prefs.zoom)));
    cx.refresh_windows();
}

pub fn run() {
    if std::env::args().nth(1).as_deref() == Some(crate::vm::snapshot::SERVE_ARG) {
        crate::vm::snapshot::serve_stdio();
        return;
    }
    crate::vm::crashlog::install();
    // AppKit 이 Touch Bar 관찰을 풀다 죽는 버그를 피한다(appkit_guard 머리 주석의 업스트림 이슈)
    appkit_guard::install(crate::vm::crashlog::log_path());
    let initial = match initial_target() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    // 화면 사진 모드(개발용): 가짜 serve 를 원격으로 띄워 정해 둔 장면을 찍고 끝낸다
    let snap = std::env::var_os(crate::vm::snapshot::ENV_DIR).map(std::path::PathBuf::from);
    let shots = match (&snap, std::env::var_os(crate::vm::snapshot::ENV_SCRIPT).filter(|s| !s.is_empty())) {
        (None, _) => Vec::new(),
        (Some(_), None) if std::env::var_os(crate::vm::snapshot::ENV_REMOTE).is_some() => crate::vm::snapshot::real_shots(),
        (Some(_), None) => crate::vm::snapshot::default_shots(),
        (Some(_), Some(f)) => match std::fs::read_to_string(&f)
            .map_err(|e| e.to_string())
            .and_then(|t| crate::vm::snapshot::parse_script(&t))
        {
            Ok(s) => s,
            Err(e) => {
                eprintln!("i-db: {e}");
                std::process::exit(2);
            }
        },
    };
    let initial = if snap.is_some() { None } else { initial };
    // 실제 원격을 찍을 때는 remote.json 의 그 원격에 붙는다(가짜 serve 를 띄우지 않는다)
    let real = snap
        .as_ref()
        .and_then(|_| std::env::var(crate::vm::snapshot::ENV_REMOTE).ok())
        .filter(|r| !r.is_empty());
    if snap.is_some() && real.is_none() {
        let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
        // SAFETY: 런타임·창 스레드를 띄우기 전, 아직 스레드가 하나일 때 바꾼다
        unsafe {
            std::env::set_var(
                idb_engine::remote::REMOTE_CMD_ENV,
                format!("{exe} {}", crate::vm::snapshot::SERVE_ARG),
            )
        };
    }
    let rt = match Runtime::new() {
        Ok(rt) => Rc::new(rt),
        Err(e) => {
            eprintln!("i-db: {e}");
            std::process::exit(1);
        }
    };
    gpui_platform::application().with_assets(brand::Assets).run(move |cx: &mut App| {
        gpui_component::init(cx);
        appkit_guard::quiet_app_touch_bar();
        install_palette(cx);
        let prefs = Prefs::path().map(|p| Prefs::load(&p)).unwrap_or_default();
        let font = load_font(&prefs, cx);
        cx.bind_keys(key_bindings());
        cx.on_action(|_: &Quit, cx| workspace::quit_after_saves(cx));
        cx.set_menus(menus());
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        // 사진 모드는 주 화면이 아닌 화면에 띄우고 초점을 뺏지 않는다
        let display = snap.as_ref().and_then(|_| {
            let displays = cx.displays();
            let primary = cx.primary_display().map(|p| p.id());
            let pi = displays.iter().position(|d| Some(d.id()) == primary).unwrap_or(0);
            let chosen = std::env::var(crate::vm::snapshot::ENV_DISPLAY).ok().and_then(|v| v.parse().ok());
            let i = crate::vm::snapshot::pick_display(displays.len(), pi, chosen);
            let d = displays.get(i)?;
            eprintln!("snapshot display {i}/{} {:?}", displays.len(), d.bounds());
            Some(d.id())
        });
        let opts = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(display, size(px(1320.), px(860.)), cx))),
            display_id: display,
            focus: snap.is_none(),
            // 제목 줄을 투명하게 두고 탭·단추를 그 안에 그린다(Orca). 신호등은 그 위에 앉는다
            titlebar: Some(TitlebarOptions {
                title: Some(crate::vm::brand::NAME.into()),
                ..gpui_component::TitleBar::title_bar_options()
            }),
            ..Default::default()
        };
        let rt = rt.clone();
        let made: Rc<std::cell::RefCell<Option<gpui::Entity<Workspace>>>> = Default::default();
        let made2 = made.clone();
        let snapping = snap.is_some();
        let opened = cx.open_window(opts, move |window, cx| {
            apply_look(&prefs, &font, window, cx);
            appkit_guard::quiet_window_touch_bar(window);
            let ws = cx.new(|cx| {
                let mut ws = Workspace::new(rt, font, window, cx);
                if snapping {
                    // 사진 모드는 사용자 설정을 읽지도 쓰지도 않는다. 고르는 화면에 보일 최근 항목만 둔다
                    ws.prefs_path = None;
                    ws.prefs = Prefs::default();
                    ws.prefs.recent = vec![
                        Target::Remote("devbox".into()),
                        Target::Local("~/Projects/shop".into()),
                        Target::Local("~/Projects/billing".into()),
                    ];
                }
                if let Some(t) = initial {
                    ws.open_target(t, window, cx);
                }
                ws
            });
            *made2.borrow_mut() = Some(ws.clone());
            cx.new(|cx| Root::new(ws, window, cx))
        });
        match opened {
            Err(e) => {
                eprintln!("i-db: {e}");
                cx.quit();
            }
            Ok(handle) => {
                if let (Some(dir), Some(ws)) = (snap.clone(), made.borrow_mut().take()) {
                    snapshot::start(handle.into(), ws, dir, shots.clone(), real.clone(), cx);
                }
            }
        }
        if snap.is_none() {
            cx.activate(true);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use gpui_component::ActiveTheme as _;

    #[gpui::test]
    fn 화면은_시스템_글꼴_편집기와_표_값은_D2Coding_이다(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            let f = font::resolve(font::DEFAULT_FONT, font::DEFAULT_SIZE, &[]);
            apply_font(&f, font::SYSTEM_UI, zoom::DEFAULT, cx);
            let t = cx.theme();
            assert_eq!(t.font_family.as_ref(), font::SYSTEM_UI_FAMILY);
            assert_eq!(t.mono_font_family.as_ref(), "D2Coding");
            assert_eq!((t.font_size, t.mono_font_size), (px(14.), px(14.)));
            apply_font(&f, font::SYSTEM_UI, 1.25, cx);
            assert_eq!(cx.theme().font_size, px(18.));
        });
    }

    #[gpui::test]
    fn Orca_색을_어두운_테마로_등록한다(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            install_palette(cx);
            Theme::change(ThemeMode::Dark, None, cx);
            let t = cx.theme();
            assert_eq!(t.background, workspace::hex("#0a0a0a"));
            assert_eq!(t.foreground, workspace::hex("#f0f0f0"));
            assert_eq!(t.muted_foreground, workspace::hex("#acacac"));
            assert_eq!(t.sidebar, workspace::hex("#171717"));
            Theme::change(ThemeMode::Light, None, cx);
            assert_eq!(cx.theme().background, workspace::hex("#ffffff"));
        });
    }

    #[test]
    fn 자산은_브랜드_마크와_아이콘을_함께_준다() {
        use gpui::AssetSource as _;
        let a = brand::Assets;
        for p in [
            "brand/mark-body.svg",
            "brand/mark-dot.svg",
            "icons/play.svg",
            "kind/postgresql.svg",
            "kind/aws.svg",
        ] {
            assert!(a.load(p).unwrap().is_some_and(|b| !b.is_empty()), "{p}");
        }
        let body = String::from_utf8(a.load("brand/mark-body.svg").unwrap().unwrap().to_vec()).unwrap();
        assert!(
            body.contains("<ellipse") && !body.contains("circle"),
            "몸통 조각에 점이 섞이면 한 색으로 칠해진다"
        );
    }

    #[test]
    fn 메뉴는_일곱_개이고_묶음은_모두_읽힌다() {
        assert_eq!(menus().len(), 7);
        let n_keys = ENTRIES.iter().filter(|e| e.key.is_some() && e.bind != Bind::Builtin).count();
        assert!(key_bindings().len() >= n_keys);
    }
}
