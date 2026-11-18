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
use crate::vm::snapshot::{REAL_QUERY_FILE, REMOTE, Scene, Shot, pick_real};
use crate::vm::tree::child_id;

/// 찍을 접속 이름들이다. 가짜 serve 는 정해 둔 이름, 실제 원격은 붙은 뒤 stg·local 에서 고른다.
#[derive(Clone, Debug, Default)]
struct Names {
    pg: Option<String>,
    prod: Option<String>,
    redis: Option<String>,
    search: Option<String>,
    logs: Option<String>,
}

fn fake_names() -> Names {
    let s = |n: &str| Some(n.to_string());
    Names {
        pg: s("app-stg"),
        prod: s("app-prod"),
        redis: s("cache-stg"),
        search: s("search-stg"),
        logs: s("logs"),
    }
}

/// 장면을 차례로 찍고 끝낸다. 실패해도 다음 장면으로 가고, 끝에 앱을 닫는다.
/// real 이 있으면 가짜 serve 대신 그 원격에 실제로 붙는다(stg·local 만, 읽기만).
pub fn start(handle: AnyWindowHandle, ws: Entity<Workspace>, dir: PathBuf, shots: Vec<Shot>, real: Option<String>, cx: &mut App) {
    // 다른 창에 가리면 GPUI 가 그리기를 멈춘다(가림 상태). ssh 로 띄우면 앞으로 오지 못하니 떠 있는 창으로 둔다
    let _ = handle.update(cx, |_, window, _| float_window(window));
    cx.spawn(async move |cx| {
        let _ = std::fs::create_dir_all(&dir);
        for (i, shot) in shots.iter().enumerate() {
            let file = dir.join(shot.file_name(i + 1));
            let started = std::time::Instant::now();
            match run_shot(cx, handle, &ws, shot, real.as_deref(), &file).await {
                Err(e) => eprintln!("snapshot {}: {e}", file.display()),
                Ok(()) => eprintln!("snapshot {} ({}ms)", file.display(), started.elapsed().as_millis()),
            }
            let status = ws.read_with(cx, |w, _| w.status_text());
            if !status.is_empty() {
                eprintln!("  status: {status}");
            }
        }
        cx.update(|cx| cx.quit());
    })
    .detach();
}

async fn sleep(cx: &mut AsyncApp, ms: u64) {
    cx.background_executor().timer(Duration::from_millis(ms)).await;
}

/// pred 가 맞을 때까지 기다린다. 시간 안에 안 되면 false.
async fn wait_for(cx: &mut AsyncApp, ws: &Entity<Workspace>, ms: u64, pred: impl Fn(&Workspace) -> bool) -> bool {
    let steps = ms / 50;
    for _ in 0..steps {
        if ws.read_with(cx, |w, _| pred(w)) {
            return true;
        }
        sleep(cx, 50).await;
    }
    false
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

async fn run_shot(
    cx: &mut AsyncApp,
    handle: AnyWindowHandle,
    ws: &Entity<Workspace>,
    shot: &Shot,
    real: Option<&str>,
    file: &Path,
) -> Result<(), String> {
    if real.is_some() && matches!(shot.scene, Scene::Dashboard | Scene::Confirm) {
        return Err("실제 원격에서는 prod 를 건드리는 장면(대시보드·확인 창)을 찍지 않는다".into());
    }
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
    let mut names = fake_names();
    if shot.scene != Scene::Picker {
        let target = Target::Remote(real.unwrap_or(REMOTE).to_string());
        on_ws(cx, handle, ws, |w, window, cx| w.open_target(target, window, cx));
        // 실제 원격은 ssh·VPN 을 거쳐 늦다
        if !wait_for(cx, ws, if real.is_some() { 30_000 } else { 5_000 }, |w| w.engine.is_some()).await {
            return Err(format!("원격에 붙지 못함: {}", ws.read_with(cx, |w, _| w.status_text())));
        }
        if real.is_some() {
            // 실제 원격에서는 첫 실행 저장·자동 저장으로 사용자 queries/ 에 파일을 만들지 않는다
            on_ws(cx, handle, ws, |w, _, _| w.autosave_on = false);
            let p = ws.read_with(cx, |w, _| pick_real(&w.tree.conns));
            names = Names {
                pg: p.pg,
                prod: None,
                redis: p.redis,
                search: p.search,
                logs: p.logs,
            };
        }
    }
    let limit = if real.is_some() { 20_000 } else { 3_000 };
    let expand = |cx: &mut AsyncApp, node: String| on_ws(cx, handle, ws, move |w, window, cx| w.tree_toggle(node, window, cx));
    let need = |n: &Option<String>, what: &str| n.clone().ok_or_else(|| format!("찍을 {what} 접속이 없다(stg·local)"));
    match shot.scene {
        Scene::Picker => {}
        Scene::Tree => {
            let pg = need(&names.pg, "PostgreSQL")?;
            expand(cx, pg.clone());
            let node = pg.clone();
            wait_for(cx, ws, limit, move |w| w.tree.is_settled(&node)).await;
            expand(cx, child_id(&pg, "s", "public"));
            // 실제 원격의 Redis 트리는 전체 SCAN 이라 펼치지 않는다
            if real.is_none() {
                let r = need(&names.redis, "Redis")?;
                expand(cx, r.clone());
                let node = r.clone();
                wait_for(cx, ws, limit, move |w| w.tree.is_settled(&node)).await;
                expand(cx, child_id(&r, "r", "user"));
            }
        }
        Scene::Sql | Scene::Confirm | Scene::Table if real.is_some() || shot.scene != Scene::Table => {
            let (conn, sql) = match (shot.scene, real.is_some()) {
                (Scene::Confirm, _) => (need(&names.prod, "prod")?, "update users\nset status = 'active'\nwhere id = 1001;"),
                (Scene::Sql, true) => (
                    need(&names.pg, "PostgreSQL")?,
                    "select now() as now, current_database() as db, current_user as role, version() as version;",
                ),
                (Scene::Table, true) => (
                    need(&names.pg, "PostgreSQL")?,
                    "select table_schema, count(*) as tables\nfrom information_schema.tables\ngroup by 1\norder by 1;",
                ),
                _ => (
                    need(&names.pg, "PostgreSQL")?,
                    "select id, email, name, status, created_at, last_login_at, deleted_at\nfrom users\nwhere status <> 'banned'\norder by id\nlimit 200;",
                ),
            };
            expand(cx, conn.clone());
            on_ws(cx, handle, ws, move |w, window, cx| {
                w.new_tab(Some(conn), window, cx);
                w.close_tab(0, cx);
                w.set_active_sql(sql, window, cx);
                w.run_query(false, window, cx);
            });
            sleep(cx, 200).await;
            wait_for(cx, ws, limit, |w| !w.is_busy()).await;
        }
        Scene::Table | Scene::Sql | Scene::Confirm => {
            let pg = need(&names.pg, "PostgreSQL")?;
            expand(cx, pg.clone());
            let node = pg.clone();
            wait_for(cx, ws, limit, move |w| w.tree.is_settled(&node)).await;
            expand(cx, child_id(&pg, "s", "public"));
            on_ws(cx, handle, ws, move |w, window, cx| {
                w.open_table(pg, "public".into(), "users".into(), window, cx);
                w.close_tab(0, cx);
            });
        }
        Scene::Files => {
            let rel = if real.is_some() {
                // 실제 원격은 시험 폴더에만 쓴다(찍은 뒤 serve 쪽에서 지운다)
                let engine = ws.read_with(cx, |w, _| w.engine.clone()).ok_or("엔진 없음")?;
                let rt = ws.read_with(cx, |w, _| w.rt.clone());
                let job = rt.spawn(async move {
                    crate::vm::queries::write(
                        &engine,
                        REAL_QUERY_FILE,
                        "-- i-db 화면 사진 시험 파일(지워도 된다)\nselect 1;\n",
                        None,
                        true,
                    )
                    .await
                });
                job.rx.await.map_err(|_| "멈춤".to_string())??;
                REAL_QUERY_FILE.to_string()
            } else {
                "queries/app/daily-orders.sql".to_string()
            };
            on_ws(cx, handle, ws, |w, _, cx| w.refresh_queries(cx));
            let r2 = rel.clone();
            wait_for(cx, ws, limit, move |w| w.has_query(&r2)).await;
            on_ws(cx, handle, ws, move |w, window, cx| w.open_query(rel, window, cx));
            sleep(cx, 300).await;
            on_ws(cx, handle, ws, |w, _, cx| w.close_tab(0, cx));
        }
        Scene::Redis => {
            let r = need(&names.redis, "Redis")?;
            if real.is_some() {
                let pat = std::env::var(crate::vm::snapshot::ENV_REDIS_PATTERN).unwrap_or_else(|_| "config*".into());
                on_ws(cx, handle, ws, move |w, window, cx| {
                    w.redis_pattern = pat;
                    w.select_conn(r, window, cx);
                    w.close_tab(0, cx);
                });
                sleep(cx, 300).await;
                wait_for(cx, ws, limit, |w| !w.is_busy()).await;
            } else {
                expand(cx, r.clone());
                on_ws(cx, handle, ws, move |w, window, cx| {
                    w.open_redis_key(r, "user:1002:profile".into(), window, cx);
                    w.close_tab(0, cx);
                });
            }
        }
        Scene::Search | Scene::Logs => {
            let (conn, path) = if shot.scene == Scene::Search {
                (need(&names.search, "OpenSearch")?, "orders-2026.11")
            } else {
                (need(&names.logs, "Loki")?, "app/api")
            };
            expand(cx, conn.clone());
            let node = conn.clone();
            wait_for(cx, ws, limit, move |w| w.tree.is_settled(&node)).await;
            let real_mode = real.is_some();
            on_ws(cx, handle, ws, move |w, window, cx| {
                // 실제 원격은 목록(인덱스·라벨)만 보고 내용은 열지 않는다
                if real_mode {
                    w.select_conn(conn, window, cx);
                } else {
                    w.open_source_path(conn, path.into(), window, cx);
                }
                w.close_tab(0, cx);
            });
            sleep(cx, 300).await;
            wait_for(cx, ws, limit, |w| !w.is_busy()).await;
        }
        Scene::Palette | Scene::PaletteQuery | Scene::PaletteKorean | Scene::GoTo => {
            // 카탈로그를 읽어 테이블이 팔레트에 들게 하고, 자주 쓴 것을 몇 개 둔다
            if let Some(pg) = names.pg.clone() {
                expand(cx, pg.clone());
                let node = pg.clone();
                wait_for(cx, ws, limit, move |w| w.tree.is_settled(&node)).await;
            }
            let (q, tables_only) = match shot.scene {
                Scene::PaletteQuery => ("users", false),
                Scene::PaletteKorean => ("ㄷㅅ", false),
                Scene::GoTo => ("ord", true),
                _ => ("", false),
            };
            let pg = names.pg.clone().unwrap_or_default();
            on_ws(cx, handle, ws, move |w, window, cx| {
                if w.prefs.palette_uses.is_empty() {
                    let now = chrono::Utc::now().timestamp();
                    for (k, n) in [("cmd:Run", 5), ("cmd:Dashboard", 3), ("cmd:ToggleSidebar", 2)] {
                        for _ in 0..n {
                            crate::vm::finder::record(&mut w.prefs.palette_uses, k, now);
                        }
                    }
                    let _ = pg;
                }
                w.open_palette(tables_only, window, cx);
                if let Some(p) = w.palette.clone() {
                    let q = q.to_string();
                    window.defer(cx, move |window, cx| p.update(cx, |p, cx| p.set_query(&q, window, cx)));
                }
            });
        }
        Scene::Titlebar => {
            // 탭 꼴 모음: 이름 없는 쿼리(고침), 자동 저장 파일, 데이터 탭, prod 탭
            if let Some(pg) = names.pg.clone() {
                expand(cx, pg.clone());
                let p2 = pg.clone();
                on_ws(cx, handle, ws, move |w, window, cx| {
                    w.new_tab(Some(p2.clone()), window, cx);
                    w.set_active_sql("select 1;", window, cx);
                    w.open_table(p2, "public".into(), "orders".into(), window, cx);
                });
            }
            if let Some(prod) = names.prod.clone() {
                on_ws(cx, handle, ws, move |w, window, cx| w.new_tab(Some(prod), window, cx));
            }
            on_ws(cx, handle, ws, |w, _, cx| {
                // 두 번째 탭(쿼리)을 고른 채 고친 표시를 둔다
                if w.tabs.tabs.len() > 1 {
                    w.tabs.tabs[1].dirty = true;
                    w.tabs.activate(1);
                }
                cx.notify();
            });
        }
        Scene::Dashboard => on_ws(cx, handle, ws, |w, window, cx| {
            w.open_dashboard(window, cx);
            w.close_tab(0, cx);
        }),
        Scene::Settings => {
            if let Some(pg) = names.pg.clone() {
                expand(cx, pg);
            }
            on_ws(cx, handle, ws, super::dialogs::open_settings);
        }
        Scene::Shortcuts => {
            if let Some(pg) = names.pg.clone() {
                expand(cx, pg);
            }
            on_ws(cx, handle, ws, |_, window, cx| super::dialogs::open_shortcuts(window, cx));
        }
    }
    // 답·애니메이션이 끝날 때까지 기다린 뒤 한 번 더 그린다
    sleep(cx, 1500).await;
    on_ws(cx, handle, ws, |_, window, _| window.refresh());
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
