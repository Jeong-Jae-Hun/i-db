#![allow(non_snake_case)]
//! db 의 터미널 화면(ratatui + crossterm). `db-rs` 를 하위 명령 없이 켜면 이 화면이다.
//! 데이터는 모두 idb_engine::Engine(Local 또는 ssh 너머 Remote)으로 읽고 쓴다. 화면은 접속 정의·비밀번호를 보지 않는다.

mod actions;
pub mod app;
mod complete;
pub mod editor;
mod files;
mod finder;
pub mod grid;
pub mod help;
mod history;
pub mod input;
pub mod keys;
mod render;
mod state;
pub mod tableview;
mod theme;
pub mod tree;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event};
use crossterm::{execute, terminal};
use idb_engine::{Engine, EngineError, LocalEngine, RemoteEngine, RemoteSpec};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use serde_json::Value;

pub use app::App;

pub struct Options {
    /// -p 프로젝트 폴더(비면 현재 폴더)
    pub project: Option<String>,
    /// --remote 이름(~/.config/db/remote.json)
    pub remote: Option<String>,
}

/// 끝난 Engine 요청이다.
struct Done {
    id: u64,
    res: Result<Value, EngineError>,
}

/// App 의 일감을 Engine 에 보내고 답을 받는다. 일감마다 tokio 작업 하나이고, 취소는 그 작업을 멈춘다(원격은 cancel 을 보낸다).
struct Runner {
    rt: tokio::runtime::Runtime,
    engine: Engine,
    tx: mpsc::Sender<Done>,
    rx: mpsc::Receiver<Done>,
    tasks: HashMap<u64, tokio::task::JoinHandle<()>>,
}

impl Runner {
    fn pump(&mut self, app: &mut App) {
        for id in app.take_cancels() {
            if let Some(h) = self.tasks.remove(&id) {
                h.abort();
            }
        }
        for job in app.take_jobs() {
            let (engine, tx) = (self.engine.clone(), self.tx.clone());
            let id = job.id;
            let h = self.rt.spawn(async move {
                let res = engine.call(job.req).await;
                let _ = tx.send(Done { id, res });
            });
            self.tasks.insert(id, h);
        }
        while let Ok(d) = self.rx.try_recv() {
            self.tasks.remove(&d.id);
            app.on_done(d.id, d.res);
        }
    }
}

fn project_dir(p: Option<String>) -> PathBuf {
    match p {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    }
}

/// 화면을 띄운다. 끝나면 종료 코드다.
pub fn run(opts: Options) -> i32 {
    let rt = match tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let (engine, files) = match &opts.remote {
        Some(name) => {
            let spec = match RemoteSpec::resolve(name) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("{e}");
                    return 2;
                }
            };
            match rt.block_on(RemoteEngine::connect(&spec)) {
                Ok(r) => (Engine::Remote(Arc::new(r)), None),
                Err(e) => {
                    eprintln!("{e}");
                    return 1;
                }
            }
        }
        None => {
            let dir = project_dir(opts.project.clone());
            let proj = match idb_core::project::load_project(&dir) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("{e}");
                    return 1;
                }
            };
            let files = files::Files::new(proj.dir.clone(), proj.mapping.clone());
            (Engine::Local(Arc::new(LocalEngine::new(proj, None))), Some(files))
        }
    };
    let info = match rt.block_on(engine.project()) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let session = state::SessionCtx::load(std::path::Path::new(&info.dir));
    let kitty = keys::kitty_wanted() && std::io::IsTerminal::is_terminal(&std::io::stdin());
    let mut app = App::new(info, files, keys::mac_style(kitty), session);
    match history::History::open() {
        Ok(h) => app.history = Some(h),
        Err(e) => {
            app.status = if app.status.is_empty() {
                e
            } else {
                format!("{} · {e}", app.status)
            };
        }
    }
    let (tx, rx) = mpsc::channel();
    let mut runner = Runner {
        rt,
        engine: engine.clone(),
        tx,
        rx,
        tasks: HashMap::new(),
    };
    // 패닉 값에는 SQL·서버 응답이 섞여 비밀번호가 나올 수 있어 찍지 않고, 화면만 되돌린다
    std::panic::set_hook(Box::new(|_| {
        let _ = restore_terminal(true);
        eprintln!("internal error");
    }));
    if let Err(e) = enter_terminal(kitty) {
        eprintln!("{e}");
        return 1;
    }
    let res = event_loop(&mut app, &mut runner);
    let saved = app.save_session();
    let _ = restore_terminal(kitty);
    runner.rt.block_on(engine.close());
    if let Err(e) = res {
        eprintln!("{e}");
        return 1;
    }
    if let Err(e) = saved {
        eprintln!("{e}");
    }
    0
}

// 마우스는 버튼을 누른 채 움직일 때만 받는다(1002 cell motion, R46.1). 1003(모든 움직임)은 켜지 않는다
const MOUSE_ON: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1006h";
const MOUSE_OFF: &str = "\x1b[?1006l\x1b[?1002l\x1b[?1000l";

fn enter_terminal(kitty: bool) -> std::io::Result<()> {
    terminal::enable_raw_mode()?;
    let mut out = std::io::stdout();
    execute!(out, terminal::EnterAlternateScreen, event::EnableBracketedPaste)?;
    out.write_all(MOUSE_ON.as_bytes())?;
    if kitty {
        execute!(
            out,
            event::PushKeyboardEnhancementFlags(event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
    }
    out.flush()
}

fn restore_terminal(kitty: bool) -> std::io::Result<()> {
    let mut out = std::io::stdout();
    if kitty {
        let _ = execute!(out, event::PopKeyboardEnhancementFlags);
    }
    out.write_all(MOUSE_OFF.as_bytes())?;
    execute!(out, event::DisableBracketedPaste, terminal::LeaveAlternateScreen)?;
    terminal::disable_raw_mode()
}

fn event_loop(app: &mut App, runner: &mut Runner) -> std::io::Result<()> {
    let mut term = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    let mut last_tick = Instant::now();
    let mut dirty = true;
    loop {
        runner.pump(app);
        if dirty {
            term.draw(|f| app.draw(f))?;
            dirty = false;
        }
        if app.quit {
            return Ok(());
        }
        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(k) => {
                    if let Some(name) = keys::name(&k) {
                        app.on_key(&name);
                    }
                }
                Event::Paste(s) => app.on_paste(&s),
                Event::Mouse(m) => app.on_mouse(m),
                Event::Resize(..) => {}
                _ => {}
            }
            dirty = true;
        }
        if !app.clips.is_empty() {
            for c in std::mem::take(&mut app.clips) {
                copy_osc52(&c);
            }
        }
        if last_tick.elapsed() >= Duration::from_secs(1) {
            app.tick();
            if app.history.as_ref().is_some_and(|h| h.take_off_note()) {
                app.status = "결과 사본을 끔: 쓰지 못함".into();
            }
            last_tick = Instant::now();
            dirty = true;
        }
        // 답이 오면 다시 그린다
        if runner.tasks.len() != app.pending.len() || !app.pending.is_empty() {
            dirty = true;
        }
    }
}

/// OSC 52 로 터미널 클립보드에 넣는다(R14.2·R71.4). DB_CLIPBOARD=system 이면 보내지 않는다.
fn copy_osc52(s: &str) {
    if std::env::var("DB_CLIPBOARD").as_deref() == Ok("system") {
        return;
    }
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]52;c;{}\x07", base64(s.as_bytes()));
    let _ = out.flush();
}

fn base64(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}
