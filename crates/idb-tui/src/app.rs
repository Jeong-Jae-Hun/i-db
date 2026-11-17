//! 화면 상태와 키·마우스 처리. 데이터는 모두 Engine 요청(Job)으로 보내고 답(on_done)으로 받는다.
//! App 은 Engine 을 직접 부르지 않아 TestBackend 시험에서 답을 손으로 넣을 수 있다(lib.rs 의 Runner 가 Job 을 돌린다).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;

use idb_engine::{Catalog, ConnSummary, EngineError, LockState, ProjectInfo, Request, TableRef};
use serde_json::Value;

use crate::editor::Editor;
use crate::files::Files;
use crate::grid::Grid;
use crate::help::Focus;
use crate::input::LineInput;
use crate::tableview::TableView;
use crate::tree::Tree;

pub const MIN_W: u16 = 60;
pub const MIN_H: u16 = 14;
pub const DEFAULT_TREE_W: i64 = 34;
pub const MIN_TREE_W: i64 = 20;
pub const MAX_TREE_PERCENT: i64 = 60;
pub const MIN_RIGHT_W: i64 = 20;
pub const DEFAULT_EDITOR_RATIO: i64 = 35;
pub const MIN_EDITOR_LINES: i64 = 3;
const TREE_STEP: i64 = 2;
const EDITOR_STEP: i64 = 2;
pub const PLACEHOLDER: &str = "SQL 또는 Redis 명령 · Ctrl+R 실행 · Ctrl+Space 자동완성";
pub const UNDO_KEEP: usize = 20;

/// Engine 에 보낼 요청 하나다.
#[derive(Debug)]
pub struct Job {
    pub id: u64,
    pub req: Request,
}

/// 답이 오면 할 일이다.
pub enum Pend {
    Schemas {
        conn: usize,
        node: usize,
    },
    Tables {
        conn: usize,
        schema: String,
        node: usize,
    },
    Scan {
        conn: usize,
        node: usize,
    },
    Catalog {
        conn: usize,
    },
    Run(Box<crate::actions::Script>),
    Page {
        tab: u64,
    },
    Count {
        tab: u64,
    },
    Describe {
        tab: u64,
    },
    Ddl {
        conn: usize,
    },
    Tx {
        conn: usize,
        op: &'static str,
        then: Option<After>,
    },
    Lock {
        conn: usize,
        toggle: bool,
    },
    Apply {
        tab: u64,
        cs: idb_engine::ChangeSet,
    },
    Undo {
        tab: u64,
        /// 낸 묶음의 자리(tab.undo)
        at: usize,
    },
    RedisValue {
        tab: u64,
    },
    RedisRaw {
        tab: u64,
    },
    RedisEdit {
        tab: u64,
    },
    Enums {
        conn: usize,
    },
    Resolve {
        tab: u64,
        conn: usize,
    },
    FuncDef {
        tab: u64,
    },
    Ops {
        tab: u64,
        title: String,
    },
    Schemas2 {
        conn: usize,
    },
    SearchPath {
        conn: usize,
        schema: String,
    },
    Databases {
        conn: usize,
    },
    SwitchDb {
        conn: usize,
    },
    Preview {
        tab: u64,
        title: String,
    },
    Fk {
        tab: u64,
        conn: usize,
        incoming: bool,
        row: Vec<(String, String)>,
    },
    Signal {
        tab: u64,
    },
    Linked {
        tab: u64,
        conn: usize,
        value: String,
    },
    User360 {
        tab: u64,
    },
    PatternKeys {
        conn: usize,
        pattern: String,
    },
    Unlink,
    Diff {
        tab: u64,
    },
    /// 저장소 트리 한 단계(R60)
    SourceKids {
        conn: usize,
        node: usize,
    },
    /// 저장소 편집기 실행·트리 미리 보기(R60~R68)
    Source(Box<crate::sources::SourceJob>),
    /// 대시보드 확인 하나(R64)
    Outbox {
        tab: u64,
        round: u64,
    },
    Estimate,
    ConnGet,
    ConnTested {
        save: bool,
    },
    ConnSaved,
    ConnChanged {
        what: String,
    },
    Reload {
        changed: Option<String>,
    },
    ExportStart,
    ExportRead,
    /// 답을 쓰지 않는 요청
    Ignore,
    Decrypt {
        tab: u64,
        at: Vec<(usize, usize)>,
    },
    Import {
        conn: usize,
        table: TableRef,
        file: String,
    },
    Health {
        tab: u64,
        round: u64,
        conn: usize,
    },
    /// 흐름 풀이(R69)
    Flows {
        tab: u64,
        round: u64,
    },
    /// 보드 고르기 목록(R70.4)
    BoardList,
    /// 보드 한 회차(R70)
    Board {
        tab: u64,
        round: u64,
    },
}

/// 편집기·결과 자리를 한 영역으로 쓰는 탭 화면이다(대시보드 R64·흐름 R69·보드 R70).
pub enum Pane {
    Dash(crate::dash::DashState),
    Flow(crate::flow::FlowState),
    Board(crate::board::BoardState),
}

pub type Then = Box<dyn FnOnce(&mut App, String)>;
/// 답이 온 뒤 이어서 할 일이다
pub type After = Box<dyn FnOnce(&mut App)>;

pub enum Overlay {
    None,
    /// y 면 then, 그 밖의 키는 취소다. preview 는 결과 영역에 보일 SQL(R15.2)이다
    Confirm {
        q: String,
        then: Box<dyn FnOnce(&mut App)>,
        preview: Vec<String>,
        top: usize,
    },
    Prompt {
        label: String,
        input: LineInput,
        then: Then,
    },
    Picker {
        title: String,
        items: Vec<String>,
        filter: LineInput,
        sel: usize,
        then: Then,
        del: Option<Then>,
    },
    Help,
    Keys(Vec<usize>),
    /// 열린 트랜잭션 확인(R17.3)
    Tx {
        conns: Vec<usize>,
        then: Box<dyn FnOnce(&mut App)>,
    },
}

impl Overlay {
    pub fn is_none(&self) -> bool {
        matches!(self, Overlay::None)
    }
}

/// 지운 묶음 되돌리기 기록(R50).
pub struct UndoEntry {
    pub conn: usize,
    /// 제출할 때의 데이터베이스(R50: 바꾼 뒤에는 되돌리지 않는다)
    pub db: String,
    pub changes: idb_engine::ChangeSet,
    pub applied: idb_engine::Applied,
}

pub struct Pin {
    pub title: String,
    pub grid: Grid,
    pub pinned: bool,
}

/// Redis 값 편집기 상태(R20).
pub struct RedisView {
    pub key: String,
    pub raw: Option<idb_engine::RawValue>,
    pub sel: usize,
    pub input: Option<(String, LineInput)>,
    pub ask: Option<String>,
}

pub struct Tab {
    pub id: u64,
    pub title: String,
    pub name: String,
    pub pinned: bool,
    pub conn: Option<usize>,
    pub editor: Editor,
    /// queries/… 상대 경로
    pub file: Option<String>,
    /// 마지막으로 읽거나 쓴 파일 내용(R45.2)
    pub disk: String,
    pub autosave_stopped: bool,
    pub dirty_at: Option<Instant>,
    pub grid: Option<Grid>,
    pub text: String,
    pub err: String,
    pub summary: String,
    pub running: Option<u64>,
    pub running_read_only: bool,
    pub cancelled: bool,
    pub view: Option<TableView>,
    /// 결과를 만든 접속(R3.1 다른 접속 결과 막기)
    pub result_conn: Option<usize>,
    /// 표 결과를 만든 마지막 읽기 문장(R33.1·R14.5)
    pub last_sql: String,
    /// F7 이 내보낼 문장이다(지금 표를 만든 성공한 문장, 테이블 보기는 LIMIT 없는 SELECT). 비면 없다
    pub export_sql: String,
    pub undo: Vec<UndoEntry>,
    pub redis: Option<RedisView>,
    pub subs: Vec<Pin>,
    pub sub: Option<usize>,
    /// 결과 영역 글 스크롤
    pub text_top: usize,
    /// 기록 사본 탭(R11.4): 읽기 전용, r 로 다시 실행
    pub snapshot: bool,
    /// 실행 계획 나무(R22.2). 있으면 결과 영역이 나무다(esc 로 닫으면 표)
    pub plan: Option<crate::plan::PlanView>,
    /// 연결된 곳 표(R73.5)의 찾은 값이다. Enter 로 그 줄의 테이블을 연다
    pub linked: Option<String>,
    /// 남기지 못한 글을 버리고 닫기로 했다
    pub close_ok: bool,
    /// 차트(R28)·비교(R31.2) 화면. 있으면 결과 영역이 그것이다
    pub chart: Option<crate::viz::Chart>,
    pub diff: Option<crate::viz::DiffView>,
    /// 서버로 다시 읽을 원래 문장(R14.5). 없으면 base_why 가 까닭이다
    pub base: Option<String>,
    pub base_why: &'static str,
    /// Loki·Prometheus·Grafana 편집기 범위(나노초, 0 이면 1시간, R62.2)
    pub since: i64,
    pub pane: Option<Pane>,
}

impl Tab {
    pub fn new(id: u64) -> Tab {
        Tab {
            id,
            title: format!("Query {id}"),
            name: String::new(),
            pinned: false,
            conn: None,
            editor: Editor::new(),
            file: None,
            disk: String::new(),
            autosave_stopped: false,
            dirty_at: None,
            grid: None,
            text: String::new(),
            err: String::new(),
            summary: String::new(),
            running: None,
            running_read_only: true,
            cancelled: false,
            view: None,
            result_conn: None,
            last_sql: String::new(),
            export_sql: String::new(),
            undo: Vec::new(),
            redis: None,
            subs: Vec::new(),
            sub: None,
            text_top: 0,
            snapshot: false,
            plan: None,
            linked: None,
            close_ok: false,
            chart: None,
            diff: None,
            base: None,
            base_why: "편집기 실행 결과가 아님",
            since: 0,
            pane: None,
        }
    }

    pub fn display_name(&self) -> String {
        if !self.name.is_empty() {
            return self.name.clone();
        }
        if let Some(f) = &self.file {
            return f.rsplit('/').next().unwrap_or(f).to_string();
        }
        self.title.clone()
    }

    /// 빈 탭이다(R5.1): 연 파일·글·결과·테이블 보기·값 편집기·보류 중인 편집이 없다.
    pub fn reusable(&self) -> bool {
        self.file.is_none()
            && self.editor.value().trim().is_empty()
            && self.grid.is_none()
            && self.text.is_empty()
            && self.err.is_empty()
            && self.view.is_none()
            && self.redis.is_none()
            && self.running.is_none()
            && self.pane.is_none()
    }

    pub fn pending(&self) -> usize {
        self.grid.as_ref().map_or(0, Grid::pending)
    }

    /// 지금 보이는 표(고정·문장별 결과를 골랐으면 그것)다.
    pub fn shown(&self) -> Option<&Grid> {
        match self.sub {
            Some(i) => self.subs.get(i).map(|p| &p.grid),
            None => self.grid.as_ref(),
        }
    }

    pub fn shown_mut(&mut self) -> Option<&mut Grid> {
        match self.sub {
            Some(i) => self.subs.get_mut(i).map(|p| &mut p.grid),
            None => self.grid.as_mut(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Drag {
    Tree,
    Split,
}

pub struct Layout {
    pub body_h: u16,
    pub tree_w: u16,
    pub right_w: u16,
    pub tree_h: u16,
    pub ed_h: u16,
    pub res_h: u16,
}

pub struct App {
    pub conns: Vec<ConnSummary>,
    pub dir: String,
    pub warnings: Vec<String>,
    pub files: Option<Files>,
    pub mac: bool,
    pub w: u16,
    pub h: u16,
    pub focus: Focus,
    pub tree: Tree,
    pub tree_filtering: Option<LineInput>,
    pub tabs: Vec<Tab>,
    pub tab: usize,
    next_tab: u64,
    pub status: String,
    pub overlay: Overlay,
    /// 세션에 남기는 창 크기(0 이면 기본, R46.2)
    pub tree_w: i64,
    pub ed_h: i64,
    pub drag: Option<Drag>,
    pub panes_dirty: bool,
    outbox: Vec<Job>,
    cancels: Vec<u64>,
    next_job: u64,
    pub pending: HashMap<u64, Pend>,
    pub catalogs: HashMap<usize, Catalog>,
    pub catalog_loading: HashSet<usize>,
    pub in_tx: HashMap<usize, Instant>,
    pub manual: HashSet<usize>,
    pub locks: HashMap<usize, LockState>,
    pub schema_of: HashMap<usize, String>,
    pub db_over: HashMap<usize, String>,
    /// 접속·타입(OID 글) → enum 값(enum 이 아니면 빈 목록, R49.2)
    pub enums: HashMap<(usize, String), Vec<String>>,
    pub quit: bool,
    /// 터미널로 보낼 복사(OSC 52)
    pub clips: Vec<String>,
    pub session: crate::state::SessionCtx,
    pub last_click: Option<(Instant, u16, u16)>,
    pub help_top: usize,
    /// 거절되면 확인 뒤 다시 보낼 요청(요청 id → 묶음)
    pub redis_retry: HashMap<u64, (u64, usize, idb_engine::RedisEdit, bool, bool)>,
    pub signal_retry: HashMap<u64, (u64, usize, i64, bool)>,
    pub history: Option<crate::history::History>,
    pub history_view: Option<crate::history::HistoryView>,
    pub deleting: Option<crate::ops::Deleting>,
    /// 이 세션에 마지막으로 고른 복사 꼴(R71.1)
    pub last_copy_fmt: &'static str,
    /// 확인 창 전에 기다리는 예상 행 수(R39): 요청 id·시작·질문·확인하면 할 일
    pub estimating: Option<(u64, Instant, crate::actions::ConfirmParts, After)>,
    pub exporting: Option<crate::export::Exporting>,
    pub conn_form: Option<crate::connform::ConnForm>,
    /// 잠금 상태를 받은 때(R54 남은 시간 세기)
    pub lock_at: HashMap<usize, Instant>,
    /// 화면을 닫은 뒤 터미널에 알릴 글(R45.3 충돌 사본 등)
    pub quit_notes: Vec<String>,
    /// 칸 값 큰 편집기(R49.3): 편집기·json 열인지·고친 결과 탭
    pub big_edit: Option<(Editor, bool, u64)>,
    /// 입력 기록 고르기(R75.5): 기록·지금 자리(-1 은 치던 글)·치던 글. 다른 키를 누르면 처음부터다
    pub hist_pick: Option<(Vec<String>, isize, String)>,
    /// 파일 기록 뒤에 붙는 기록이다. 상태 파일을 쓰지 않는 시험이 기록을 넣는 자리다
    pub history_seed: Vec<String>,
    /// 지금 보이는 팁(R74.3). 다음 키에서 사라진다
    pub tip: Option<String>,
}

impl App {
    pub fn new(info: ProjectInfo, files: Option<Files>, mac: bool, session: crate::state::SessionCtx) -> App {
        let loose = files.as_ref().map_or(0, |f| f.loose(&info.conns).len());
        let tree = Tree::build(&info.conns, &session.closed_groups(), loose);
        let mut app = App {
            conns: info.conns,
            dir: info.dir,
            warnings: info.warnings,
            files,
            mac,
            w: 120,
            h: 40,
            focus: Focus::Tree,
            tree,
            tree_filtering: None,
            tabs: Vec::new(),
            tab: 0,
            next_tab: 1,
            status: String::new(),
            overlay: Overlay::None,
            tree_w: 0,
            ed_h: 0,
            drag: None,
            panes_dirty: false,
            outbox: Vec::new(),
            cancels: Vec::new(),
            next_job: 1,
            pending: HashMap::new(),
            catalogs: HashMap::new(),
            catalog_loading: HashSet::new(),
            in_tx: HashMap::new(),
            manual: HashSet::new(),
            locks: HashMap::new(),
            schema_of: HashMap::new(),
            db_over: HashMap::new(),
            enums: HashMap::new(),
            quit: false,
            clips: Vec::new(),
            session,
            last_click: None,
            help_top: 0,
            redis_retry: HashMap::new(),
            signal_retry: HashMap::new(),
            history: None,
            history_view: None,
            deleting: None,
            last_copy_fmt: "tsv",
            estimating: None,
            exporting: None,
            conn_form: None,
            lock_at: HashMap::new(),
            quit_notes: Vec::new(),
            big_edit: None,
            hist_pick: None,
            history_seed: Vec::new(),
            tip: None,
        };
        if !app.warnings.is_empty() {
            app.status = format!("건너뜀: {}", app.warnings.join(", "));
        }
        app.restore_session();
        if app.tabs.is_empty() {
            app.new_tab();
        }
        app
    }

    // ─── 일감 ───

    pub fn call(&mut self, req: Request, p: Pend) -> u64 {
        let id = self.next_job;
        self.next_job += 1;
        self.outbox.push(Job { id, req });
        self.pending.insert(id, p);
        id
    }

    pub fn take_jobs(&mut self) -> Vec<Job> {
        std::mem::take(&mut self.outbox)
    }

    pub fn take_cancels(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.cancels)
    }

    pub fn cancel(&mut self, id: u64) {
        self.cancels.push(id);
    }

    pub fn on_done(&mut self, id: u64, res: Result<Value, EngineError>) {
        let Some(p) = self.pending.remove(&id) else { return };
        self.handle(id, p, res);
    }

    // ─── 접속 ───

    pub fn conn(&self, i: usize) -> &ConnSummary {
        &self.conns[i]
    }

    pub fn is_pg(&self, i: usize) -> bool {
        self.conns[i].kind == "postgresql"
    }

    pub fn is_redis(&self, i: usize) -> bool {
        self.conns[i].kind == "redis"
    }

    // ─── 탭 ───

    pub fn cur(&self) -> &Tab {
        &self.tabs[self.tab]
    }

    pub fn cur_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.tab]
    }

    pub fn tab_index(&self, id: u64) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == id)
    }

    pub fn tab_by_id(&mut self, id: u64) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.id == id)
    }

    /// 새 탭을 만들어 옮긴다. 지금 접속을 이어받는다(R13.1).
    pub fn new_tab(&mut self) -> usize {
        let id = self.next_tab;
        self.next_tab += 1;
        let mut t = Tab::new(id);
        if let Some(cur) = self.tabs.get(self.tab) {
            t.conn = cur.conn;
            t.editor.redis = cur.editor.redis;
        }
        self.tabs.push(t);
        self.switch_tab(self.tabs.len() - 1);
        self.tab
    }

    pub fn switch_tab(&mut self, i: usize) {
        if i >= self.tabs.len() || i == self.tab && self.tabs.len() > 1 {
            self.tab = i.min(self.tabs.len().saturating_sub(1));
            return;
        }
        // 떠나는 파일 탭은 자동 저장한다(R45.2)
        let leaving = self.tab;
        if leaving < self.tabs.len() && leaving != i {
            self.autosave(leaving);
        }
        self.tab = i;
    }

    pub fn set_conn(&mut self, ti: usize, conn: Option<usize>) {
        let redis = conn.is_some_and(|c| self.is_redis(c));
        let t = &mut self.tabs[ti];
        t.conn = conn;
        t.editor.redis = redis;
        if let Some(c) = conn {
            self.ensure_catalog(c);
            if self.conns[c].env == "prod" && !self.locks.contains_key(&c) {
                let name = self.conns[c].name.clone();
                self.call(
                    Request::ProdLock { conn: name, unlock: None },
                    Pend::Lock { conn: c, toggle: false },
                );
            }
        }
    }

    /// 탭 접속의 자동완성 목록을 한 번 읽는다(R8.1).
    pub fn ensure_catalog(&mut self, c: usize) {
        if !self.is_pg(c) || self.catalogs.contains_key(&c) || self.catalog_loading.contains(&c) {
            return;
        }
        self.catalog_loading.insert(c);
        let name = self.conns[c].name.clone();
        self.call(Request::Catalog { conn: name }, Pend::Catalog { conn: c });
    }

    // ─── 배치 ───

    fn clamp_tree(&self, w: i64) -> i64 {
        let ww = self.w as i64;
        let hi = MIN_TREE_W.max((ww * MAX_TREE_PERCENT / 100).min(ww - 4 - MIN_RIGHT_W));
        w.clamp(MIN_TREE_W, hi)
    }

    fn clamp_editor(&self, h: i64) -> i64 {
        let avail = self.h as i64 - 2 - 5;
        let h = if h <= 0 { avail * DEFAULT_EDITOR_RATIO / 100 } else { h };
        h.max(MIN_EDITOR_LINES).min(avail - MIN_EDITOR_LINES).max(1)
    }

    pub fn layout(&self) -> Layout {
        let body_h = self.h.saturating_sub(2);
        let tree_w = self.clamp_tree(if self.tree_w == 0 { DEFAULT_TREE_W } else { self.tree_w }) as u16;
        let right_w = (self.w as i64 - tree_w as i64 - 4).max(10) as u16;
        let ed_h = self.clamp_editor(self.ed_h) as u16;
        Layout {
            body_h,
            tree_w,
            right_w,
            tree_h: body_h.saturating_sub(2).max(1),
            ed_h,
            res_h: (body_h as i64 - 5 - ed_h as i64).max(1) as u16,
        }
    }

    pub fn resize_tree(&mut self, d: i64) {
        let cur = self.layout().tree_w as i64;
        let next = self.clamp_tree(cur + d);
        if next != cur {
            self.tree_w = next;
            self.panes_dirty = true;
        }
    }

    pub fn resize_editor(&mut self, d: i64) {
        let cur = self.layout().ed_h as i64;
        let next = self.clamp_editor(cur + d);
        if next != cur {
            self.ed_h = next;
            self.panes_dirty = true;
        }
    }

    pub fn set_focus(&mut self, f: Focus) {
        if f != Focus::Result
            && let Some(g) = self.cur_mut().shown_mut()
            && g.screen == crate::grid::Screen::Filter
        {
            // 영역을 떠나면 거르기 입력은 끝나고 거른 글은 남는다(R14.1)
            g.screen = crate::grid::Screen::Table;
        }
        self.focus = f;
    }

    /// 복사한다(R71.4·R71.5). 10MiB 를 넘는 글은 어디에도 쓰지 않는다.
    pub fn copy(&mut self, s: String, status: String) {
        if s.len() > 10 << 20 {
            self.status = format!(
                "너무 커서 복사하지 않음 ({:.1} MB) — e/E 로 내보내기",
                s.len() as f64 / (1 << 20) as f64
            );
            return;
        }
        self.clips.push(s);
        self.status = status;
    }

    /// 큰 편집기 키: Ctrl+S 바꿈으로 두기(json 열은 JSON 검사), esc 버림, F8 JSON 정렬(R49.3).
    fn big_edit_key(&mut self, k: &str) {
        let Some((ed, json, tab)) = &mut self.big_edit else { return };
        let (json, tab) = (*json, *tab);
        match k {
            "ctrl+s" => {
                let text = ed.value().to_string();
                if json && let Err(e) = serde_json::from_str::<serde_json::Value>(&text) {
                    self.status = format!("JSON 이 아님: {e}");
                    return;
                }
                self.big_edit = None;
                if let Some(g) = self.tab_by_id(tab).and_then(|t| t.grid.as_mut()) {
                    g.finish_big_edit(Some(text));
                }
            }
            "esc" => {
                self.big_edit = None;
                if let Some(g) = self.tab_by_id(tab).and_then(|t| t.grid.as_mut()) {
                    g.finish_big_edit(None);
                }
            }
            "f8" if json => match serde_json::from_str::<serde_json::Value>(ed.value()) {
                Ok(v) => {
                    let pretty = serde_json::to_string_pretty(&v).unwrap_or_default();
                    let len = ed.value().len();
                    ed.replace_range(0, len, &pretty);
                }
                Err(e) => self.status = format!("JSON 이 아님: {e}"),
            },
            _ => {
                ed.key(k);
            }
        }
    }

    pub fn label(&self, s: &str) -> String {
        crate::keys::label(s, self.mac)
    }

    // ─── 키 ───

    /// 붙여넣기(bracketed paste)다. 입력 줄·편집기·결과 표(R48) 순으로 받는다.
    pub fn on_paste(&mut self, s: &str) {
        if let Some((ed, _, _)) = &mut self.big_edit {
            ed.paste(s);
            return;
        }
        if let Some(f) = &mut self.conn_form {
            let at = f.at;
            f.fields[at].insert(s);
            return;
        }
        match &mut self.overlay {
            Overlay::Prompt { input, .. } => return input.insert(s),
            Overlay::Picker { filter, sel, .. } => {
                filter.insert(s);
                *sel = 0;
                return;
            }
            Overlay::None => {}
            _ => return,
        }
        if let Some(f) = &mut self.tree_filtering {
            f.insert(s);
            return;
        }
        match self.focus {
            Focus::Editor => {
                self.cur_mut().editor.paste(s);
                self.edited();
            }
            Focus::Result => {
                let t = self.cur_mut();
                if let Some(v) = &mut t.view
                    && v.typing.is_some()
                {
                    v.input.insert(s);
                    return;
                }
                if let Some(g) = t.shown_mut() {
                    match g.screen {
                        crate::grid::Screen::Filter => {
                            g.input.insert(s);
                            let f = g.input.text.clone();
                            g.set_filter(&f);
                        }
                        crate::grid::Screen::Edit => g.input.insert(s),
                        _ => {
                            if t.sub.is_some() {
                                self.status = "고정·문장별 결과는 고칠 수 없음".into();
                                return;
                            }
                            let act = t.grid.as_mut().unwrap().paste(s);
                            self.grid_act(act);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    pub fn on_key(&mut self, k: &str) {
        self.tip = None;
        if !self.overlay.is_none() {
            return self.overlay_key(k);
        }
        if self.conn_form.is_some() {
            return self.conn_form_key(k);
        }
        if self.big_edit.is_some() {
            return self.big_edit_key(k);
        }
        // 키를 지우는 동안은 esc 로 멈추고, 종료는 멈춘 뒤에 한다(R42.1)
        if let Some(d) = &mut self.deleting {
            match k {
                "esc" => {
                    d.stop = true;
                    self.status = "멈추는 중…".into();
                    return;
                }
                "q" | "ctrl+c" => {
                    self.status = "키를 지우는 중 — esc 로 멈춘 뒤 종료".into();
                    return;
                }
                _ => {}
            }
        }
        if self.history_view.is_some() {
            return self.history_key(k);
        }
        if k == "esc" && self.exporting.is_some() && self.tree_filtering.is_none() && !self.esc_busy() {
            return self.export_cancel();
        }
        if let Some(input) = &mut self.tree_filtering {
            match k {
                "enter" => {
                    let f = input.text.clone();
                    self.tree_filtering = None;
                    self.apply_tree_filter(f);
                }
                "esc" => {
                    self.tree_filtering = None;
                }
                _ => {
                    input.key(k);
                    if !self.redis_key_stage() {
                        self.tree.filter = self.tree_filtering.as_ref().unwrap().text.clone();
                        self.tree.rebuild();
                    }
                }
            }
            return;
        }
        // 글을 받는 중이면 Ctrl+C 말고는 그 입력이 먼저다
        if self.focus == Focus::Result && self.typing_in_result() && k != "ctrl+c" {
            return self.result_key(k);
        }
        if self.global_key(k) {
            return;
        }
        // 대시보드·흐름·보드 탭은 편집기·결과 자리를 한 영역으로 쓴다
        if self.focus != Focus::Tree && self.cur().pane.is_some() {
            return self.pane_key(k);
        }
        match self.focus {
            Focus::Tree => self.tree_key(k),
            Focus::Editor => self.editor_key(k),
            Focus::Result => self.result_key(k),
            Focus::Keep => {}
        }
    }

    /// esc 가 다른 일(자동완성·선택·템플릿·값 보기·표 선택·입력·트리 거르기)을 하는지다. 그러면 내보내기는 이어진다(R33.1 단).
    fn esc_busy(&self) -> bool {
        let t = self.cur();
        match self.focus {
            Focus::Editor => t.editor.popup_open() || t.editor.selection().is_some() || t.editor.wants_tab(),
            Focus::Tree => !self.tree.filter.is_empty(),
            _ => {
                self.typing_in_result()
                    || t.redis.is_some()
                    || t.shown().is_some_and(|g| g.screen != crate::grid::Screen::Table)
                    || t.view.as_ref().is_some_and(|v| v.typing.is_some())
            }
        }
    }

    fn typing_in_result(&self) -> bool {
        let t = self.cur();
        if t.view.as_ref().is_some_and(|v| v.typing.is_some()) {
            return true;
        }
        if t.redis.as_ref().is_some_and(|r| r.input.is_some() || r.ask.is_some()) {
            return true;
        }
        t.shown()
            .is_some_and(|g| matches!(g.screen, crate::grid::Screen::Filter | crate::grid::Screen::Edit))
    }

    /// 어느 포커스에서나 듣는 키다. 썼으면 true 다.
    fn global_key(&mut self, k: &str) -> bool {
        let editor_tab = self.focus == Focus::Editor && self.cur().editor.wants_tab();
        match k {
            "ctrl+c" => self.ctrl_c(),
            "f1" => {
                self.overlay = Overlay::Help;
                self.help_top = 0;
            }
            "ctrl+p" if !(self.focus == Focus::Editor && self.cur().editor.popup_open()) => self.open_palette(),
            "ctrl+n" if !(self.focus == Focus::Editor && self.cur().editor.popup_open()) => self.open_finder(),
            "tab" if !editor_tab => {
                let next = match self.focus {
                    Focus::Tree => Focus::Editor,
                    Focus::Editor => Focus::Result,
                    _ => Focus::Tree,
                };
                self.set_focus(next);
            }
            "shift+tab" => {
                let next = match self.focus {
                    Focus::Tree => Focus::Result,
                    Focus::Editor => Focus::Tree,
                    _ => Focus::Editor,
                };
                self.set_focus(next);
            }
            "ctrl+t" => {
                self.new_tab();
                self.set_focus(Focus::Editor);
            }
            "ctrl+w" => self.close_tab(self.tab),
            "ctrl+pgdown" => self.switch_tab((self.tab + 1) % self.tabs.len()),
            "ctrl+pgup" => self.switch_tab((self.tab + self.tabs.len() - 1) % self.tabs.len()),
            "ctrl+r" | "f5" => self.run_current(None),
            "ctrl+e" if self.focus != Focus::Result || self.cur().grid.is_none() => self.run_current(Some(false)),
            "alt+e" => self.run_current(Some(true)),
            "f8" => self.format_sql(),
            "f9" => self.toggle_manual(),
            "f10" => self.tx_end(true),
            "f11" => self.tx_end(false),
            "ctrl+l" => self.toggle_lock(),
            "ctrl+s" => self.ctrl_s(),
            "ctrl+o" => self.open_file_picker(),
            "f6" => self.pick_schema(),
            "f3" => self.open_favorites(),
            "f4" => self.save_favorite(),
            "ctrl+u" => self.open_user360(),
            "alt+r" => self.rename_tab(),
            "alt+p" => {
                let t = self.cur_mut();
                t.pinned = !t.pinned;
                let pinned = t.pinned;
                self.sort_pinned();
                self.status = if pinned { "탭 고정" } else { "탭 고정 풂" }.into();
            }
            "ctrl+b" | "f12" if self.focus == Focus::Editor => self.goto_definition(),
            "ctrl+left" | "ctrl+right" if self.focus != Focus::Editor => {
                self.resize_tree(if k == "ctrl+left" { -TREE_STEP } else { TREE_STEP });
            }
            "ctrl+up" | "ctrl+down" if self.focus != Focus::Editor => {
                self.resize_editor(if k == "ctrl+up" { -EDITOR_STEP } else { EDITOR_STEP });
            }
            "ctrl+h" => self.open_history(),
            "ctrl+g" => self.open_dash(),
            "alt+f" => self.open_flows(None),
            "alt+b" => self.pick_board(),
            "ctrl+]" => self.cycle_range(),
            "?" if self.focus != Focus::Tree && self.cur().pane.is_some() => self.open_keys(),
            "f7" => self.full_export(),
            "alt+c" => {
                self.status = format!("{} 은 Rust 화면에 아직 옮기지 않음", self.label(&key_label(k)));
            }
            _ => {
                if let Some(n) = k.strip_prefix("alt+").and_then(|d| d.parse::<usize>().ok())
                    && (1..=9).contains(&n)
                {
                    if n <= self.tabs.len() {
                        self.switch_tab(n - 1);
                    }
                    return true;
                }
                return false;
            }
        }
        true
    }

    fn sort_pinned(&mut self) {
        let id = self.cur().id;
        self.tabs.sort_by_key(|t| !t.pinned);
        self.tab = self.tab_index(id).unwrap_or(0);
    }

    fn ctrl_c(&mut self) {
        let t = self.cur_mut();
        if let Some(id) = t.running.take() {
            t.cancelled = true;
            let ro = t.running_read_only;
            self.cancel(id);
            self.pending.remove(&id);
            let t = self.cur_mut();
            t.summary = if ro {
                "취소됨".into()
            } else {
                "취소됨 (쓰기는 이미 반영됐을 수 있음)".into()
            };
            self.status = if ro {
                "취소 요청함".into()
            } else {
                "취소 요청함 (쓰기는 이미 반영됐을 수 있음)".into()
            };
            return;
        }
        self.request_quit();
    }

    /// 종료한다. 열린 트랜잭션이 있으면 먼저 묻는다(R17.3).
    pub fn request_quit(&mut self) {
        let open: Vec<usize> = self.in_tx.keys().copied().collect();
        if open.is_empty() {
            self.quit = true;
            return;
        }
        self.overlay = Overlay::Tx {
            conns: open,
            then: Box::new(|a| a.quit = true),
        };
    }

    pub fn close_tab(&mut self, i: usize) {
        let t = &self.tabs[i];
        if t.pinned {
            let name = t.display_name();
            self.confirm(format!("고정한 탭 {name} 을 닫을까요"), move |a| {
                a.tabs[i].pinned = false;
                a.close_tab(i);
            });
            return;
        }
        let (running, conn) = (t.running, t.conn);
        if let Some(id) = running {
            self.cancel(id);
            self.pending.remove(&id);
        }
        // 닫는 대시보드·흐름·보드의 진행 중인 회차를 멈춘다(R64.2·R69.4·R70.4)
        self.stop_pane(i);
        // 그 접속을 쓰는 다른 탭이 없고 트랜잭션이 열려 있으면 먼저 끝낸다
        if let Some(c) = conn
            && self.in_tx.contains_key(&c)
            && !self.tabs.iter().enumerate().any(|(j, o)| j != i && o.conn == Some(c))
        {
            self.overlay = Overlay::Tx {
                conns: vec![c],
                then: Box::new(move |a| a.close_tab(i)),
            };
            return;
        }
        self.autosave(i);
        if let Some(note) = self.keep_text(i) {
            self.status = note;
        } else if self.unsaved_text(i) && !self.tabs[i].close_ok {
            // 친 글을 남길 수 없으면(Redis·접속 없음·쓰기 실패) 묻는다(R45.3)
            self.confirm("저장하지 않은 글이 있는 탭을 닫을까요", move |a| {
                a.tabs[i].close_ok = true;
                a.close_tab(i);
            });
            return;
        }
        self.tabs.remove(i);
        if self.tabs.is_empty() {
            self.tab = 0;
            self.new_tab();
        } else if self.tab >= self.tabs.len() || self.tab > i {
            self.tab = self.tab.saturating_sub(1).min(self.tabs.len() - 1);
        }
    }

    pub fn confirm(&mut self, q: impl Into<String>, then: impl FnOnce(&mut App) + 'static) {
        self.overlay = Overlay::Confirm {
            q: q.into(),
            then: Box::new(then),
            preview: Vec::new(),
            top: 0,
        };
    }

    pub fn prompt(&mut self, label: impl Into<String>, value: &str, then: impl FnOnce(&mut App, String) + 'static) {
        self.overlay = Overlay::Prompt {
            label: label.into(),
            input: LineInput::new(value),
            then: Box::new(then),
        };
    }

    pub fn picker(&mut self, title: impl Into<String>, items: Vec<String>, then: impl FnOnce(&mut App, String) + 'static) {
        self.overlay = Overlay::Picker {
            title: title.into(),
            items,
            filter: LineInput::default(),
            sel: 0,
            then: Box::new(then),
            del: None,
        };
    }

    pub fn picker_matches(items: &[String], filter: &str) -> Vec<usize> {
        let f = filter.to_lowercase();
        let words: Vec<&str> = f.split_whitespace().collect();
        (0..items.len())
            .filter(|&i| {
                let s = items[i].to_lowercase();
                words.iter().all(|w| s.contains(w))
            })
            .collect()
    }

    fn overlay_key(&mut self, k: &str) {
        let ov = std::mem::replace(&mut self.overlay, Overlay::None);
        match ov {
            Overlay::None => {}
            Overlay::Help => {
                if matches!(k, "down" | "j" | "pgdown") {
                    self.help_top += if k == "pgdown" { 10 } else { 1 };
                    self.overlay = Overlay::Help;
                } else if matches!(k, "up" | "k" | "pgup") {
                    self.help_top = self.help_top.saturating_sub(if k == "pgup" { 10 } else { 1 });
                    self.overlay = Overlay::Help;
                }
            }
            Overlay::Keys(secs) => {
                // esc·?·q 만 닫고 다른 키(F1 포함)는 아무것도 하지 않는다(R74.2)
                if !matches!(k, "esc" | "?" | "q") {
                    self.overlay = Overlay::Keys(secs);
                }
            }
            Overlay::Confirm { q, then, preview, top } => match k {
                "y" | "Y" => then(self),
                "up" | "k" | "down" | "j" | "pgup" | "pgdown" if !preview.is_empty() => {
                    let d: isize = match k {
                        "up" | "k" => -1,
                        "down" | "j" => 1,
                        "pgup" => -10,
                        _ => 10,
                    };
                    let top = (top as isize + d).clamp(0, preview.len().saturating_sub(1) as isize) as usize;
                    self.overlay = Overlay::Confirm { q, then, preview, top };
                }
                _ => self.status = "취소함".into(),
            },
            Overlay::Prompt { label, mut input, then } => match k {
                "enter" => then(self, input.text.clone()),
                "esc" => self.status = "취소함".into(),
                _ => {
                    input.key(k);
                    self.overlay = Overlay::Prompt { label, input, then };
                }
            },
            Overlay::Picker {
                title,
                items,
                mut filter,
                mut sel,
                then,
                del,
            } => {
                let m = Self::picker_matches(&items, &filter.text);
                match k {
                    "enter" => {
                        if let Some(&i) = m.get(sel) {
                            then(self, items[i].clone());
                        }
                        return;
                    }
                    "esc" => return,
                    "up" | "ctrl+p" => sel = sel.saturating_sub(1),
                    "down" | "ctrl+n" => sel = (sel + 1).min(m.len().saturating_sub(1)),
                    "ctrl+d" if del.is_some() => {
                        if let Some(&i) = m.get(sel) {
                            (del.unwrap())(self, items[i].clone());
                        }
                        return;
                    }
                    "d" if del.is_some() && filter.text.is_empty() => {
                        if let Some(&i) = m.get(sel) {
                            (del.unwrap())(self, items[i].clone());
                        }
                        return;
                    }
                    _ => {
                        if filter.key(k) {
                            sel = 0;
                        }
                    }
                }
                self.overlay = Overlay::Picker {
                    title,
                    items,
                    filter,
                    sel,
                    then,
                    del,
                };
            }
            Overlay::Tx { conns, then } => match k {
                "c" | "r" => self.finish_txs(conns, k == "c", then),
                "ctrl+c" => self.quit = true,
                "esc" => self.status = "취소함".into(),
                _ => self.overlay = Overlay::Tx { conns, then },
            },
        }
    }

    // ─── 트리 ───

    fn redis_key_stage(&self) -> bool {
        self.tree.current().is_some_and(|n| {
            let node = self.tree.node(n);
            node.conn().is_some_and(|c| self.is_redis(c))
                && !matches!(node.kind, crate::tree::NodeKind::Conn(_) | crate::tree::NodeKind::Queries(_))
        })
    }

    fn apply_tree_filter(&mut self, f: String) {
        if self.redis_key_stage() {
            // 키 단계는 SCAN 패턴이다(R2.5)
            let n = self.tree.current().unwrap();
            let c = self.tree.node(n).conn().unwrap();
            let pat = if f.is_empty() { "*".to_string() } else { f };
            self.tree.patterns.insert(c, pat);
            if let Some(cn) = self.tree.conn_node(c) {
                self.tree.select(cn);
                self.load_keys(c, cn);
            }
            return;
        }
        self.tree.filter = f;
        self.tree.rebuild();
    }

    pub fn tree_key(&mut self, k: &str) {
        let Some(n) = self.tree.current() else {
            if k == "q" {
                self.request_quit();
            }
            return;
        };
        match k {
            "down" | "j" => self.tree.move_by(1),
            "up" | "k" => self.tree.move_by(-1),
            "pgdown" => self.tree.move_by(10),
            "pgup" => self.tree.move_by(-10),
            "g" | "home" => self.tree.cursor = 0,
            "G" | "end" => self.tree.cursor = self.tree.rows.len().saturating_sub(1),
            "enter" | "l" | "right" => self.activate(n, k == "enter"),
            "h" | "left" => {
                let node = self.tree.node(n);
                if node.expanded {
                    self.tree.nodes[n].expanded = false;
                    self.tree.rebuild();
                } else if let Some(p) = node.parent {
                    self.tree.select(p);
                }
            }
            "/" => {
                let cur = if self.redis_key_stage() {
                    let c = self.tree.node(n).conn().unwrap();
                    self.tree.patterns.get(&c).cloned().unwrap_or_default()
                } else {
                    self.tree.filter.clone()
                };
                self.tree_filtering = Some(LineInput::new(&cur));
            }
            "esc" => {
                if !self.tree.filter.is_empty() {
                    self.tree.filter.clear();
                    self.tree.rebuild();
                    self.tree.select(n);
                }
            }
            "r" => self.refresh_node(n),
            "d" => self.describe_node(n),
            "D" => self.ddl_node(n),
            "A" | "L" | "S" => self.ops_node(n, k),
            "B" => self.pick_database(n),
            // Redis 접속·이름공간 줄 X 는 패턴 삭제(R42), 그 밖의 접속 줄은 db 전용 접속 지우기(R59.3)
            "X" if matches!(self.tree.node(n).kind, crate::tree::NodeKind::Conn(c) if !self.is_redis(c)) => self.conn_remove(n),
            "X" => self.pattern_delete(n),
            "?" => self.open_keys(),
            "q" => self.request_quit(),
            "F" => {
                if !self.tree_flow(n) {
                    self.data_search(n);
                }
            }
            "=" => self.schema_compare(n),
            "I" => self.csv_import(n),
            "N" => self.conn_new(n),
            "E" => self.conn_edit(n),
            "M" => self.conn_move(n),
            "alt+X" => self.conn_remove(n),
            _ => {}
        }
    }

    pub fn editor_key(&mut self, k: &str) {
        if k == "esc" && self.cur().editor.popup_open() {
            self.cur_mut().editor.close_popup();
            return;
        }
        let eff = self.cur_mut().editor.key(k);
        match eff {
            crate::editor::Effect::Copy(s) => self.copy(s, "복사함".into()),
            crate::editor::Effect::Complete => self.complete(),
            crate::editor::Effect::Changed => self.edited(),
            _ => {}
        }
    }

    pub fn edited(&mut self) {
        let t = self.cur_mut();
        if t.file.is_some() {
            t.dirty_at = Some(Instant::now());
        }
    }

    pub fn open_keys(&mut self) {
        let pane = if self.focus == Focus::Tree {
            None
        } else {
            self.cur().pane.as_ref()
        };
        let want: &[&str] = match (self.focus, pane) {
            (_, Some(Pane::Dash(_))) => &["대시보드 (Ctrl+G)"],
            (_, Some(Pane::Flow(_))) => &["흐름 (Alt+F)"],
            (_, Some(Pane::Board(_))) => &["보드 (Alt+B)"],
            (Focus::Tree, _) => &["트리"],
            (Focus::Result, _) if self.cur().redis.is_some() => &["Redis 값 편집", "결과"],
            (Focus::Result, _)
                if self
                    .cur()
                    .shown()
                    .is_some_and(|g| g.value_mode().1 && g.screen == crate::grid::Screen::Value) =>
            {
                &["JSON 값 보기 (o)", "결과"]
            }
            (Focus::Result, _) => &["결과"],
            _ => return,
        };
        let secs = crate::help::SECTIONS
            .iter()
            .enumerate()
            .filter(|(_, s)| want.contains(&s.title))
            .map(|(i, _)| i)
            .collect();
        self.overlay = Overlay::Keys(secs);
    }

    // ─── 마우스 ───

    pub fn on_mouse(&mut self, ev: crossterm::event::MouseEvent) {
        use crossterm::event::{MouseButton, MouseEventKind};
        if !self.overlay.is_none() || self.conn_form.is_some() {
            self.drag = None;
            return;
        }
        let l = self.layout();
        let (x, y) = (ev.column, ev.row);
        let split_y = 1 + 1 + l.ed_h + 1;
        match ev.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.drag = None;
                if y >= 1 && y <= l.body_h && x == l.tree_w + 1 {
                    self.drag = Some(Drag::Tree);
                    return;
                }
                if x > l.tree_w + 1 && y == split_y {
                    self.drag = Some(Drag::Split);
                    return;
                }
                let double = self
                    .last_click
                    .is_some_and(|(t, px, py)| px == x && py == y && t.elapsed().as_millis() < 400);
                self.last_click = Some((Instant::now(), x, y));
                self.click(x, y, double, ev.modifiers.contains(crossterm::event::KeyModifiers::SHIFT));
            }
            MouseEventKind::Drag(MouseButton::Left) => match self.drag {
                Some(Drag::Tree) => {
                    let w = self.clamp_tree(x as i64 - 1);
                    if w != l.tree_w as i64 {
                        self.tree_w = w;
                    }
                }
                Some(Drag::Split) => {
                    let h = self.clamp_editor(y as i64 - 3);
                    if h != l.ed_h as i64 {
                        self.ed_h = h;
                    }
                }
                None => {
                    if self.focus == Focus::Editor && x > l.tree_w + 2 && y >= 3 && y < 3 + l.ed_h {
                        self.cur_mut().editor.click((x - l.tree_w - 3) as usize, (y - 3) as usize, true);
                    }
                }
            },
            MouseEventKind::Up(_) => {
                if self.drag.take().is_some() {
                    self.panes_dirty = true;
                }
            }
            MouseEventKind::Moved => {
                // 버튼 없는 움직임은 창 밖에서 놓은 것이다(R46.1)
                if self.drag.take().is_some() {
                    self.panes_dirty = true;
                }
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let d: isize = if ev.kind == MouseEventKind::ScrollDown { 3 } else { -3 };
                if x <= l.tree_w + 1 {
                    self.tree.move_by(d);
                } else if y < split_y {
                    self.cur_mut().editor.scroll(d);
                } else if let Some(g) = self.cur_mut().shown_mut() {
                    g.scroll_by(d);
                } else {
                    let t = self.cur_mut();
                    t.text_top = (t.text_top as isize + d).max(0) as usize;
                }
            }
            _ => {}
        }
    }

    fn click(&mut self, x: u16, y: u16, double: bool, shift: bool) {
        let l = self.layout();
        if y == 0 || y > l.body_h {
            return;
        }
        if x <= l.tree_w + 1 {
            self.set_focus(Focus::Tree);
            if y >= 2 {
                let row = self.tree.top + (y - 2) as usize;
                if row < self.tree.rows.len() {
                    self.tree.cursor = row;
                    let (n, depth) = self.tree.rows[row];
                    let toggle_x = 1 + depth as u16 * 2;
                    if double || (x >= toggle_x && x < toggle_x + 2 && self.tree.node(n).expandable()) {
                        self.activate(n, double);
                    }
                }
            }
            return;
        }
        let rx = x - l.tree_w - 2;
        if y == 1 {
            self.click_tab_bar(rx as usize);
            return;
        }
        if y >= 3 && y < 3 + l.ed_h {
            self.set_focus(Focus::Editor);
            self.cur_mut().editor.click(rx.saturating_sub(1) as usize, (y - 3) as usize, false);
            if double {
                self.cur_mut().editor.select_word();
            }
            return;
        }
        let res_top = 3 + l.ed_h + 2;
        if y >= res_top && y < res_top + l.res_h {
            self.set_focus(Focus::Result);
            let lines = self.result_header_lines() as u16;
            let ry = y - res_top;
            let w = l.right_w as usize;
            let t = self.cur_mut();
            if ry == 0
                && lines > 0
                && let Some(v) = &mut t.view
            {
                v.bar_click(rx.saturating_sub(1) as usize, w);
                return;
            }
            if ry < lines {
                return;
            }
            let gy = (ry - lines) as usize;
            if let Some(g) = t.shown_mut()
                && let Some(c) = g.click(rx.saturating_sub(1) as usize, gy, w)
            {
                self.header_click(c, shift);
            }
        }
    }

    fn click_tab_bar(&mut self, x: usize) {
        let labels = self.tab_labels();
        let mut at = 0;
        for (i, l) in labels.iter().enumerate() {
            let w = unicode_width::UnicodeWidthStr::width(l.as_str());
            if x >= at && x < at + w {
                self.switch_tab(i);
                return;
            }
            at += w + 1;
        }
        if x >= at && x < at + 3 {
            self.new_tab();
            self.set_focus(Focus::Editor);
        }
    }

    pub fn tab_labels(&self) -> Vec<String> {
        self.tabs
            .iter()
            .map(|t| {
                let mut name = idb_core::textutil::printable(&t.display_name(), false);
                if t.pinned {
                    name = format!("📌{name}");
                }
                if t.running.is_some() {
                    name.push_str(" …");
                }
                format!(" {name} ")
            })
            .collect()
    }

    /// 결과 표 위 줄 수(테이블 보기 줄·결과 탭 줄·설명 줄)다.
    pub fn result_header_lines(&self) -> usize {
        self.result_lines(self.layout().right_w as usize).len()
    }

    pub fn state_dir_project(&self) -> PathBuf {
        PathBuf::from(&self.dir)
    }
}

/// 키 이름을 안내 글자(Ctrl+H)로 되돌린다.
pub fn key_label(k: &str) -> String {
    k.split('+')
        .map(|p| match p {
            "ctrl" => "Ctrl".to_string(),
            "alt" => "Alt".to_string(),
            "shift" => "Shift".to_string(),
            p if p.len() > 1 && p.starts_with('f') => p.to_uppercase(),
            p => p.to_string(),
        })
        .collect::<Vec<_>>()
        .join("+")
}
