//! 화면 사진 모드(개발용, vm::snapshot). 장면마다 새로 접속해 같은 상태에서 시작하고, 다 그려질 때까지 기다린 뒤
//! 자기 창만 PNG 로 찍는다. 자기 창은 화면 기록 권한 없이 찍힌다(CGWindowListCreateImage + 자기 창 번호).

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::{AnyWindowHandle, App, AsyncApp, Entity, Window};
use gpui_component::WindowExt as _;

use super::workspace::Workspace;
use crate::vm::runtime::Target;
use crate::vm::settings::ThemePref;
use crate::vm::snapshot::{REMOTE, Scene, Shot};
use crate::vm::tree::child_id;

/// 장면을 차례로 찍고 끝낸다. 실패해도 다음 장면으로 가고, 끝에 앱을 닫는다.
pub fn start(handle: AnyWindowHandle, ws: Entity<Workspace>, dir: PathBuf, shots: Vec<Shot>, cx: &mut App) {
    // 다른 창에 가리면 GPUI 가 그리기를 멈춘다(가림 상태). ssh 로 띄우면 앞으로 오지 못하니 떠 있는 창으로 둔다
    let _ = handle.update(cx, |_, window, _| float_window(window));
    cx.spawn(async move |cx| {
        let _ = std::fs::create_dir_all(&dir);
        for (i, shot) in shots.iter().enumerate() {
            let file = dir.join(shot.file_name(i + 1));
            if let Err(e) = run_shot(cx, handle, &ws, shot, &file).await {
                eprintln!("snapshot {}: {e}", file.display());
            } else {
                eprintln!("snapshot {}", file.display());
            }
        }
        cx.update(|cx| cx.quit());
    })
    .detach();
}

async fn sleep(cx: &mut AsyncApp, ms: u64) {
    cx.background_executor().timer(Duration::from_millis(ms)).await;
}

fn on_ws(
    cx: &mut AsyncApp,
    handle: AnyWindowHandle,
    ws: &Entity<Workspace>,
    f: impl FnOnce(&mut Workspace, &mut Window, &mut gpui::Context<Workspace>),
) {
    let ws = ws.clone();
    let _ = handle.update(cx, |_, window, cx| ws.update(cx, |w, cx| f(w, window, cx)));
}

async fn run_shot(cx: &mut AsyncApp, handle: AnyWindowHandle, ws: &Entity<Workspace>, shot: &Shot, file: &Path) -> Result<(), String> {
    let (dark, zoom) = (shot.dark, shot.zoom);
    on_ws(cx, handle, ws, move |w, window, cx| {
        window.close_all_dialogs(cx);
        w.disconnect(cx);
        w.prefs.tree_memory.clear();
        w.prefs.theme = if dark { ThemePref::Dark } else { ThemePref::Light };
        w.prefs.zoom = zoom;
        super::apply_look(&w.prefs, &w.font, window, cx);
    });
    sleep(cx, 300).await;
    if shot.scene != Scene::Picker {
        on_ws(cx, handle, ws, |w, window, cx| {
            w.open_target(Target::Remote(REMOTE.into()), window, cx)
        });
        let mut ok = false;
        for _ in 0..100 {
            sleep(cx, 50).await;
            if ws.read_with(cx, |w, _| w.engine.is_some()) {
                ok = true;
                break;
            }
        }
        if !ok {
            return Err("가짜 serve 에 붙지 못함".into());
        }
    }
    let expand = |cx: &mut AsyncApp, node: String| on_ws(cx, handle, ws, move |w, window, cx| w.tree_toggle(node, window, cx));
    match shot.scene {
        Scene::Picker => {}
        Scene::Tree => {
            expand(cx, "app-stg".into());
            sleep(cx, 400).await;
            expand(cx, child_id("app-stg", "s", "public"));
            expand(cx, "cache-stg".into());
            sleep(cx, 400).await;
            expand(cx, child_id("cache-stg", "r", "user"));
        }
        Scene::Sql | Scene::Confirm => {
            let (conn, sql) = if shot.scene == Scene::Sql {
                (
                    "app-stg",
                    "select id, email, name, status, created_at, last_login_at, deleted_at\nfrom users\nwhere status <> 'banned'\norder by id\nlimit 200;",
                )
            } else {
                ("app-prod", "update users\nset status = 'active'\nwhere id = 1001;")
            };
            expand(cx, conn.into());
            sleep(cx, 300).await;
            on_ws(cx, handle, ws, move |w, window, cx| {
                w.new_tab(Some(conn.into()), window, cx);
                w.close_tab(0, cx);
                w.set_active_sql(sql, window, cx);
                w.run_query(false, window, cx);
            });
        }
        Scene::Table => {
            expand(cx, "app-stg".into());
            sleep(cx, 300).await;
            expand(cx, child_id("app-stg", "s", "public"));
            on_ws(cx, handle, ws, |w, window, cx| {
                w.open_table("app-stg".into(), "public".into(), "users".into(), window, cx);
                w.close_tab(0, cx);
            });
        }
        Scene::Redis => {
            expand(cx, "cache-stg".into());
            sleep(cx, 300).await;
            on_ws(cx, handle, ws, |w, window, cx| {
                w.open_redis_key("cache-stg".into(), "user:1002:profile".into(), window, cx);
                w.close_tab(0, cx);
            });
        }
        Scene::Search | Scene::Logs => {
            let (conn, path) = if shot.scene == Scene::Search {
                ("search-stg", "orders-2026.11")
            } else {
                ("logs", "app/api")
            };
            expand(cx, conn.into());
            sleep(cx, 300).await;
            on_ws(cx, handle, ws, move |w, window, cx| {
                w.open_source_path(conn.into(), path.into(), window, cx);
                w.close_tab(0, cx);
            });
        }
        Scene::Dashboard => on_ws(cx, handle, ws, |w, window, cx| {
            w.open_dashboard(window, cx);
            w.close_tab(0, cx);
        }),
        Scene::Settings => {
            expand(cx, "app-stg".into());
            on_ws(cx, handle, ws, super::dialogs::open_settings);
        }
        Scene::Shortcuts => {
            expand(cx, "app-stg".into());
            on_ws(cx, handle, ws, |_, window, cx| super::dialogs::open_shortcuts(window, cx));
        }
    }
    // 답·애니메이션이 끝날 때까지 기다린 뒤 한 번 더 그린다
    sleep(cx, 1500).await;
    on_ws(cx, handle, ws, |w, window, cx| {
        w.set_status("", false, cx);
        window.refresh();
    });
    sleep(cx, 400).await;
    let file = file.to_path_buf();
    handle
        .update(cx, move |_, window, _| capture(window, &file))
        .map_err(|e| e.to_string())?
}

// ─── 찍기 ───

#[repr(C)]
#[derive(Clone, Copy)]
struct CGRect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGWindowListCreateImage(rect: CGRect, list: u32, window: u32, image: u32) -> *const c_void;
    fn CGImageRelease(image: *const c_void);
}

#[link(name = "ImageIO", kind = "framework")]
unsafe extern "C" {
    fn CGImageDestinationCreateWithURL(url: *const c_void, ty: *const c_void, count: usize, opts: *const c_void) -> *mut c_void;
    fn CGImageDestinationAddImage(dest: *mut c_void, image: *const c_void, props: *const c_void);
    fn CGImageDestinationFinalize(dest: *mut c_void) -> bool;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFURLCreateFromFileSystemRepresentation(alloc: *const c_void, buf: *const u8, len: isize, dir: bool) -> *const c_void;
    fn CFStringCreateWithCString(alloc: *const c_void, s: *const std::ffi::c_char, enc: u32) -> *const c_void;
    fn CFRelease(p: *const c_void);
}

const INCLUDING_WINDOW: u32 = 1 << 3;
const BOUNDS_IGNORE_FRAMING: u32 = 1 << 0;
const UTF8: u32 = 0x0800_0100;

fn window_number(window: &Window) -> Option<u32> {
    ns_window(window).map(|w| w.windowNumber() as u32)
}

fn ns_window(window: &Window) -> Option<objc2::rc::Retained<objc2_app_kit::NSWindow>> {
    let h = raw_window_handle::HasWindowHandle::window_handle(window).ok()?;
    let raw_window_handle::RawWindowHandle::AppKit(h) = h.as_raw() else {
        return None;
    };
    // SAFETY: 창 핸들이 살아 있는 동안 받은 NSView 다(메인 스레드).
    let view = unsafe { &*(h.ns_view.as_ptr() as *const objc2_app_kit::NSView) };
    view.window()
}

fn float_window(window: &Window) {
    if let Some(w) = ns_window(window) {
        // 마우스가 창 위에 있으면 hover·툴팁이 찍힌다. 화면 네 귀퉁이 중 마우스가 들지 않는 곳에 둔다
        if let (Some(screen), Some(mtm)) = (w.screen(), objc2::MainThreadMarker::new()) {
            let _ = mtm;
            let vis = screen.visibleFrame();
            let f = w.frame();
            let m = objc2_app_kit::NSEvent::mouseLocation();
            let xs = [vis.origin.x, vis.origin.x + vis.size.width - f.size.width];
            let ys = [vis.origin.y, vis.origin.y + vis.size.height - f.size.height];
            let inside = |x: f64, y: f64| m.x >= x && m.x <= x + f.size.width && m.y >= y && m.y <= y + f.size.height;
            let corner = xs
                .iter()
                .flat_map(|&x| ys.iter().map(move |&y| (x, y)))
                .find(|&(x, y)| !inside(x, y));
            if let Some((x, y)) = corner {
                w.setFrameOrigin(objc2_foundation::NSPoint::new(x, y));
            }
        }
        w.setLevel(objc2_app_kit::NSFloatingWindowLevel);
        // 실제 마우스가 창 위에 있어도 hover·툴팁이 찍히지 않게 한다
        w.setIgnoresMouseEvents(true);
        w.orderFrontRegardless();
    }
}

/// 자기 창 하나를 PNG 로 쓴다.
fn capture(window: &Window, path: &Path) -> Result<(), String> {
    let id = window_number(window).ok_or("창 번호를 얻지 못함")?;
    let null = CGRect {
        x: f64::INFINITY,
        y: f64::INFINITY,
        w: 0.0,
        h: 0.0,
    };
    let bytes = path.as_os_str().as_encoded_bytes();
    // SAFETY: CoreGraphics·ImageIO 의 C 함수에 살아 있는 값만 넘기고, 만든 것은 모두 놓는다.
    unsafe {
        let image = CGWindowListCreateImage(null, INCLUDING_WINDOW, id, BOUNDS_IGNORE_FRAMING);
        if image.is_null() {
            return Err("CGWindowListCreateImage 가 빈 그림을 줌".into());
        }
        let url = CFURLCreateFromFileSystemRepresentation(std::ptr::null(), bytes.as_ptr(), bytes.len() as isize, false);
        let ty = CFStringCreateWithCString(std::ptr::null(), c"public.png".as_ptr(), UTF8);
        let dest = CGImageDestinationCreateWithURL(url, ty, 1, std::ptr::null());
        let ok = !dest.is_null() && {
            CGImageDestinationAddImage(dest, image, std::ptr::null());
            CGImageDestinationFinalize(dest)
        };
        if !dest.is_null() {
            CFRelease(dest);
        }
        CFRelease(ty);
        CFRelease(url);
        CGImageRelease(image);
        if ok { Ok(()) } else { Err("PNG 를 쓰지 못함".into()) }
    }
}
