//! 이벤트 흐름 탭(R69.4). 흐름 풀이는 Engine flows 요청 하나로 모두 동시에 받고, 그림(가로·세로)은 Go flow.go 를 옮겼다.
//! 상자 상세(Enter)의 경로·아웃박스 질의는 flows.json 설정이 있어야 해 로컬 프로젝트에서만 설정을 읽어 만든다.

use std::time::{Duration, Instant};

use idb_core::flows::{Flow, TypeStat, count_sql, retry_sql};
use idb_core::textutil::{printable, string_width, truncate};
use idb_engine::{EngineError, FlowBox, FlowResult, Request, RunRequest};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use serde_json::Value;

use crate::app::{App, Pane, Pend};
use crate::dash::{Segs, kst_now, put};
use crate::help::Focus;
use crate::sources::SourceJob;
use crate::theme;
use crate::tree::NodeKind;

/// 흐름 탭이 활성인 동안 다시 푸는 간격(R69.4)
pub const FLOW_EVERY: Duration = Duration::from_secs(30);
/// 토픽·큐가 있는 흐름 이름 옆 흐린 안내다. 큐·DLQ 수는 SQS 요청마다 요금이 붙어 묻지 않는다(R72)
const FLOW_NO_METRICS: &str = "수치는 요금이 붙어 보지 않음";
const FLOW_NO_FILE: &str = "흐름 없음 · .dbtui/flows.json 을 쓰세요 (DB_ALLOW_BILLED=1 이면 aws 트리 SNS 토픽에서 F)";
const STAGE_TYPE: &str = "outbox-type";

/// 트리 SNS 토픽 줄 F 로 더한 흐름이다(R69.4). 아웃박스·큐 없이 토픽 이름만이라 아무것도 묻지 않는다.
pub struct Adhoc {
    pub name: String,
    pub aws: String,
    pub arn: String,
}

pub struct FlowState {
    /// 마지막 풀이(flows.json 순서)
    pub items: Vec<FlowResult>,
    pub adhoc: Vec<Adhoc>,
    /// 답을 한 번이라도 받았다
    pub done: bool,
    pub file_err: String,
    pub job: Option<u64>,
    pub round: u64,
    pub cursor: usize,
    pub sel: usize,
    pub due: Instant,
    /// 답이 오면 고를 흐름 이름(트리 F·보드 흐름 패널)
    pub want: Option<String>,
}

impl FlowState {
    fn names(&self) -> Vec<String> {
        self.items
            .iter()
            .map(|f| f.flow.clone())
            .chain(self.adhoc.iter().map(|a| a.name.clone()))
            .collect()
    }

    fn pick_want(&mut self) {
        if let Some(w) = &self.want
            && let Some(i) = self.names().iter().position(|n| n == w)
        {
            self.cursor = i;
            self.sel = 0;
            self.want = None;
        }
    }

    /// i 번째 흐름의 그림 상자다. 트리에서 연 토픽 흐름은 SNS 상자 하나다.
    fn boxes(&self, i: usize) -> Vec<VBox> {
        if let Some(f) = self.items.get(i) {
            return vboxes(&f.boxes);
        }
        let Some(a) = self.adhoc.get(i - self.items.len()) else {
            return Vec::new();
        };
        vboxes(&[FlowBox {
            stage: "sns".into(),
            name: a.arn.rsplit(':').next().unwrap_or(&a.arn).to_string(),
            conn: a.aws.clone(),
            ..Default::default()
        }])
    }
}

/// 그림 상자 하나다. sub 는 구독(큐) 자리, short 는 그림에 보일 짧은 수치다.
#[derive(Clone, Debug, Default)]
struct VBox {
    b: FlowBox,
    short: String,
    sub: isize,
    pad: usize,
}

impl VBox {
    fn failed(&self) -> bool {
        !self.b.err.is_empty()
    }
}

/// Engine 상자에 그림 정보(구독 자리·짧은 수치·종류 줄 맞춤)를 더한다. 상자는 아웃박스 → SNS → 큐(→ DLQ·Lambda) → 종류 줄 순서다.
fn vboxes(bs: &[FlowBox]) -> Vec<VBox> {
    let mut sub: isize = -1;
    let mut out: Vec<VBox> = bs
        .iter()
        .map(|b| {
            if b.stage == "sqs" {
                sub += 1;
            }
            let mut v = VBox {
                b: b.clone(),
                sub: if matches!(b.stage.as_str(), "sqs" | "dlq" | "lambda") {
                    sub
                } else {
                    -1
                },
                ..Default::default()
            };
            match b.stage.as_str() {
                "lambda" => {
                    let (state, last) = b.metrics.split_once(" · ").unwrap_or((&b.metrics, ""));
                    v.short = if state == "Enabled" && !last.is_empty() {
                        last.to_string()
                    } else {
                        b.metrics.trim_end_matches(" · ").to_string()
                    };
                }
                STAGE_TYPE => v.short = b.metrics.replacen(" · → ", "  → ", 1),
                _ => {}
            }
            v
        })
        .collect();
    align_types(&mut out);
    out
}

fn align_types(bs: &mut [VBox]) {
    let w = bs
        .iter()
        .filter(|b| b.b.stage == STAGE_TYPE)
        .map(|b| string_width(&printable(&b.b.name, false)))
        .max()
        .unwrap_or(0);
    for b in bs.iter_mut().filter(|b| b.b.stage == STAGE_TYPE) {
        b.pad = w - string_width(&printable(&b.b.name, false));
    }
}

// ─── 그림 ───

/// 그림 한 줄의 조각이다. box 가 None 이면 잇는 선이다.
type Line = Vec<(String, Option<usize>)>;

fn line_width(l: &Line) -> usize {
    l.iter().map(|(s, _)| string_width(s)).sum()
}

fn stage_name(s: &str) -> &'static str {
    match s {
        "outbox" => "아웃박스",
        "sns" => "SNS",
        "sqs" => "SQS",
        "lambda" => "λ",
        _ => "",
    }
}

/// `[아웃박스 … | 대기 120 · …]`·`[SQS q1]` 같은 상자 글이다. DLQ 는 상자 없이 `DLQ q1-dlq` 다.
fn box_label(v: &VBox) -> String {
    let b = &v.b;
    let mut m = if v.short.is_empty() { b.metrics.clone() } else { v.short.clone() };
    if !b.err.is_empty() {
        m = format!("오류: {}", b.err);
    } else if !b.note.is_empty() {
        m = b.note.clone();
    }
    let name = printable(&b.name, false);
    let m = printable(&m, false);
    match b.stage.as_str() {
        STAGE_TYPE => format!("{name}{}  {m}", " ".repeat(v.pad)),
        "dlq" => format!("DLQ {name} {m}").trim().to_string(),
        "lambda" => format!("[λ {}]", format!("{name} {m}").trim()),
        "config" => format!("[{name} | {m}]"),
        st if m.is_empty() => format!("[{} {name}]", stage_name(st)),
        st => format!("[{} {name} | {m}]", stage_name(st)),
    }
}

fn seg(bs: &[VBox], i: usize) -> (String, Option<usize>) {
    (box_label(&bs[i]), Some(i))
}

/// 구독 줄마다의 상자 자리(첫째가 큐)다.
fn sub_groups(bs: &[VBox]) -> (Vec<usize>, Vec<Vec<usize>>) {
    let (mut head, mut subs): (Vec<usize>, Vec<Vec<usize>>) = (Vec::new(), Vec::new());
    for (i, b) in bs.iter().enumerate() {
        if b.b.stage == STAGE_TYPE {
            continue;
        }
        if b.sub < 0 {
            head.push(i);
            continue;
        }
        if b.sub as usize >= subs.len() {
            subs.push(Vec::new());
        }
        subs.last_mut().unwrap().push(i);
    }
    (head, subs)
}

/// 가로 그림이다: `[아웃박스 …] ──▶ [SNS t] ─┬─▶ [SQS q1] ──▶ [λ worker OK]`, DLQ 는 큐 아래 `└─ DLQ` 가지다.
fn flow_wide(bs: &[VBox]) -> Vec<Line> {
    let (head, subs) = sub_groups(bs);
    let mut first: Line = Vec::new();
    for (k, &i) in head.iter().enumerate() {
        if k > 0 {
            first.push((" ──▶ ".into(), None));
        }
        first.push(seg(bs, i));
    }
    if subs.is_empty() || head.first().is_some_and(|&i| bs[i].b.stage == "config") {
        return vec![first];
    }
    let pad = " ".repeat(line_width(&first));
    let mut out = Vec::new();
    for (r, g) in subs.iter().enumerate() {
        let last = r == subs.len() - 1;
        let (mut conn, mut under) = ("  ├─▶ ", "  │   ");
        if subs.len() == 1 {
            (conn, under) = (" ──▶ ", "     ");
        } else if r == 0 {
            conn = " ─┬─▶ ";
        } else if last {
            (conn, under) = ("  └─▶ ", "      ");
        }
        // 아웃박스·토픽 없이 큐만 있는 흐름은 큐가 줄 맨 앞이다
        if head.is_empty() {
            (conn, under) = ("", "  ");
        }
        let mut line: Line = if r == 0 {
            let mut l = first.clone();
            l.push((conn.into(), None));
            l
        } else {
            vec![(format!("{pad}{conn}"), None)]
        };
        line.push(seg(bs, g[0]));
        let mut dlq = Vec::new();
        let mut lambdas = 0;
        for &i in &g[1..] {
            if bs[i].b.stage == "dlq" {
                dlq.push(i);
                continue;
            }
            line.push((if lambdas == 0 { " ──▶ " } else { " " }.into(), None));
            lambdas += 1;
            line.push(seg(bs, i));
        }
        out.push(line);
        for i in dlq {
            out.push(vec![(format!("{pad}{under}└─ "), None), seg(bs, i)]);
        }
    }
    out
}

/// 좁을 때의 세로 그림이다(`▼`, 큐 아래 `├─ DLQ`·`└─▶ [λ …]`).
fn flow_tall(bs: &[VBox]) -> Vec<Line> {
    let (head, subs) = sub_groups(bs);
    let mut out = Vec::new();
    for (k, &i) in head.iter().enumerate() {
        if k > 0 {
            out.push(vec![("  ▼".to_string(), None)]);
        }
        out.push(vec![seg(bs, i)]);
    }
    for (r, g) in subs.iter().enumerate() {
        let (conn, under) = if r == subs.len() - 1 {
            ("  └─▶ ", "        ")
        } else {
            ("  ├─▶ ", "  │     ")
        };
        out.push(vec![(conn.into(), None), seg(bs, g[0])]);
        for (k, &i) in g[1..].iter().enumerate() {
            let mut branch = if k == g.len() - 2 { "└─" } else { "├─" }.to_string();
            if bs[i].b.stage != "dlq" {
                branch.push('▶');
            }
            out.push(vec![(format!("{under}{branch} "), None), seg(bs, i)]);
        }
    }
    out
}

/// 상자를 왼쪽 → 오른쪽으로 잇고, 가장 긴 줄이 w 를 넘으면 위 → 아래로 쌓는다. 아웃박스 종류 줄(R69.6)은 아래에 붙는다.
fn flow_layout(bs: &[VBox], w: usize) -> Vec<Line> {
    let mut lines = flow_wide(bs);
    if lines.iter().map(line_width).max().unwrap_or(0) > w {
        lines = flow_tall(bs);
    }
    let at: Vec<usize> = (0..bs.len()).filter(|&i| bs[i].b.stage == STAGE_TYPE).collect();
    for (k, &i) in at.iter().enumerate() {
        let branch = if k == at.len() - 1 { "  └─ " } else { "  ├─ " };
        lines.push(vec![(branch.into(), None), seg(bs, i)]);
    }
    lines
}

fn box_style(v: &VBox) -> Style {
    if v.failed() {
        theme::err()
    } else if v.b.warn {
        theme::warn()
    } else if !v.b.note.is_empty() || v.b.metrics == "…" || v.b.idle {
        theme::dim()
    } else {
        Style::new().fg(theme::TEXT)
    }
}

/// 그림 줄에 색을 입힌다. 고른 상자는 뒤집는다.
fn render_lines(lines: &[Line], bs: &[VBox], sel: usize) -> Vec<Segs> {
    lines
        .iter()
        .map(|l| {
            l.iter()
                .map(|(s, b)| match b {
                    None => (s.clone(), theme::dim()),
                    Some(i) => {
                        let mut st = box_style(&bs[*i]);
                        if *i == sel {
                            st = st.add_modifier(Modifier::REVERSED);
                        }
                        (s.clone(), st)
                    }
                })
                .collect()
        })
        .collect()
}

/// 목록 줄의 상태 표시다: `✕` 설정 오류·실패, `!` 경고, `●` 문제 없음. 목록은 답이 온 뒤에만 있어 `…` 줄은 없다
/// (다시 푸는 동안은 이전 결과의 표시를 그대로 둔다).
fn flow_icon(bs: &[VBox]) -> (&'static str, Style) {
    if bs.iter().any(VBox::failed) {
        return ("✕", theme::err());
    }
    if bs.iter().any(|b| b.b.warn) {
        return ("!", theme::warn());
    }
    ("●", theme::ok())
}

/// 보드 흐름 패널 본문이다(R70.3): 상자마다 한 줄, 오류 빨강·경고·안내 흐리게.
pub(crate) fn panel_lines(bs: &[FlowBox]) -> Vec<Segs> {
    let vs = vboxes(bs);
    vs.iter()
        .map(|v| {
            let st = if v.failed() {
                theme::err()
            } else if v.b.warn {
                theme::warn()
            } else if !v.b.note.is_empty() {
                theme::dim()
            } else {
                Style::new().fg(theme::TEXT)
            };
            vec![(box_label(v), st)]
        })
        .collect()
}

impl App {
    /// Alt+F·팔레트 `흐름`: 흐름 탭을 하나만 열고 바로 다시 푼다. adhoc 이 있으면 목록 끝에 더하고 고른다.
    pub fn open_flows(&mut self, adhoc: Option<Adhoc>) {
        let ti = match self.find_pane_tab(|p| matches!(p, Pane::Flow(_))) {
            Some(i) => {
                self.switch_tab(i);
                i
            }
            None => self.new_pane_tab(
                "흐름",
                Pane::Flow(FlowState {
                    items: Vec::new(),
                    adhoc: Vec::new(),
                    done: false,
                    file_err: String::new(),
                    job: None,
                    round: 0,
                    cursor: 0,
                    sel: 0,
                    due: Instant::now(),
                    want: None,
                }),
            ),
        };
        self.set_focus(Focus::Result);
        if let Some(a) = adhoc
            && let Some(Pane::Flow(s)) = &mut self.tabs[ti].pane
        {
            s.want = Some(a.name.clone());
            if !s.adhoc.iter().any(|x| x.name == a.name) {
                s.adhoc.push(a);
            }
            s.pick_want();
        }
        self.flow_refresh(ti);
    }

    /// 흐름 탭에서 이름이 name 인 흐름을 고른다(보드 흐름 패널 Enter, R70.4). 아직 답이 없으면 답이 온 뒤 고른다.
    pub(crate) fn flow_want(&mut self, name: &str) {
        if let Some(Pane::Flow(s)) = &mut self.cur_mut().pane {
            s.want = Some(name.to_string());
            s.pick_want();
        }
    }

    /// 모든 흐름을 다시 푼다. 진행 중인 이전 회차는 취소한다(대시보드와 같다). 화면은 prod 접속도 푼다.
    pub(crate) fn flow_refresh(&mut self, ti: usize) {
        self.stop_pane(ti);
        let tab = self.tabs[ti].id;
        let Some(Pane::Flow(s)) = &mut self.tabs[ti].pane else { return };
        s.round += 1;
        s.due = Instant::now() + FLOW_EVERY;
        let round = s.round;
        let id = self.call(
            Request::Flows {
                names: Vec::new(),
                prod: true,
            },
            Pend::Flows { tab, round },
        );
        if let Some(Pane::Flow(s)) = &mut self.tabs[ti].pane {
            s.job = Some(id);
        }
        self.status = "흐름 확인 중…".into();
    }

    pub(crate) fn flows_done(&mut self, tab: u64, round: u64, res: Result<Value, EngineError>) {
        let Some(ti) = self.tab_index(tab) else { return };
        let Some(Pane::Flow(s)) = &mut self.tabs[ti].pane else { return };
        if s.round != round {
            return;
        }
        s.job = None;
        s.done = true;
        match res.and_then(|v| serde_json::from_value::<Vec<FlowResult>>(v).map_err(|e| EngineError::run(format!("bad result: {e}")))) {
            Ok(items) => {
                s.items = items;
                s.file_err.clear();
            }
            Err(e) => {
                s.items.clear();
                s.file_err = idb_core::textutil::one_line(&e.error);
            }
        }
        s.pick_want();
        let n = s.names().len();
        s.cursor = s.cursor.min(n.saturating_sub(1));
        if self.status.starts_with("흐름 확인 중") {
            self.status = format!("흐름 확인함 {}", kst_now());
        }
    }

    pub(crate) fn flow_key(&mut self, k: &str) {
        let ti = self.tab;
        let Some(Pane::Flow(s)) = &mut self.tabs[ti].pane else { return };
        let n = s.names().len();
        let (mut cur, mut sel) = (s.cursor as isize, s.sel as isize);
        match k {
            "r" => return self.flow_refresh(ti),
            "up" | "k" => (cur, sel) = (cur - 1, 0),
            "down" | "j" => (cur, sel) = (cur + 1, 0),
            "home" | "g" => (cur, sel) = (0, 0),
            "end" | "G" => (cur, sel) = (n as isize - 1, 0),
            "left" | "h" => sel -= 1,
            "right" | "l" => sel += 1,
            "enter" => {
                if n == 0 {
                    return;
                }
                let bs = s.boxes(s.cursor);
                let Some(v) = bs.get(s.sel.min(bs.len().saturating_sub(1))).cloned() else {
                    return;
                };
                let name = s.names()[s.cursor].clone();
                let arn = s.adhoc.get(s.cursor.wrapping_sub(s.items.len())).map(|a| a.arn.clone());
                return self.open_flow_box(&name, arn, &v);
            }
            _ => return,
        }
        s.cursor = cur.clamp(0, n.saturating_sub(1) as isize) as usize;
        let nb = s.boxes(s.cursor).len();
        s.sel = sel.clamp(0, nb.saturating_sub(1) as isize) as usize;
    }

    /// 상자 상세를 새 결과 탭에 연다(R69.4). 아웃박스는 종류별 수, 종류 줄은 그 종류의 재시도·한도 소진 행, Lambda 는 매핑 표,
    /// 큐·DLQ 는 속성, ARN 토픽은 구독 표다(SQS·SNS 는 요금이 붙어 serve 가 막으면 R72 안내). 이름만 준 토픽은 상세가 없다.
    fn open_flow_box(&mut self, flow: &str, adhoc_arn: Option<String>, v: &VBox) {
        let b = &v.b;
        let none = |a: &mut App| a.status = format!("열 상세가 없음: {}", printable(&b.name, false));
        let Some(ci) = self.conns.iter().position(|c| c.name == b.conn) else {
            return none(self);
        };
        let conf = self.flow_conf(flow);
        if matches!(b.stage.as_str(), "outbox" | STAGE_TYPE) {
            let Some(o) = conf.as_ref().and_then(|f| f.outbox.clone()) else {
                self.status = if self.files.is_none() {
                    "원격 접속에서는 아웃박스 상세를 열지 않음".into()
                } else {
                    format!("열 상세가 없음: {}", printable(&b.name, false))
                };
                return;
            };
            let sql = if b.stage == STAGE_TYPE {
                let null = b.name == "(NULL)";
                retry_sql(
                    &o,
                    &TypeStat {
                        name: if null { String::new() } else { b.name.clone() },
                        null,
                        ..Default::default()
                    },
                )
            } else {
                count_sql(&o)
            };
            let ti = self.new_tab();
            self.set_conn(ti, Some(ci));
            let tab = self.tabs[ti].id;
            let req = Request::Run(RunRequest {
                conn: self.conns[ci].name.clone(),
                sql,
                ..Default::default()
            });
            let id = self.call(
                req,
                Pend::Preview {
                    tab,
                    title: "아웃박스".into(),
                },
            );
            let t = &mut self.tabs[ti];
            t.running = Some(id);
            t.result_conn = Some(ci);
            self.set_focus(Focus::Result);
            return;
        }
        let q = conf.as_ref().and_then(|f| f.queues.get(v.sub.max(0) as usize));
        let path = match b.stage.as_str() {
            "lambda" => Some(format!("lambda/{}", b.name)),
            "sns" => adhoc_arn
                .or_else(|| conf.as_ref().map(|f| f.topic.clone()))
                .filter(|t| t.starts_with("arn:"))
                .map(|t| format!("sns/{t}")),
            "sqs" => q.map(|q| format!("sqs/{}", q.name)),
            // DLQ 는 다른 계정일 수 있어 ARN 으로만 연다(같은 이름의 다른 큐를 열지 않게)
            "dlq" => q.filter(|q| q.dlq.starts_with("arn:")).map(|q| format!("sqs/{}", q.dlq)),
            _ => None,
        };
        let Some(path) = path else { return none(self) };
        let ti = self.new_tab();
        self.set_conn(ti, Some(ci));
        let job = SourceJob {
            tab: self.tabs[ti].id,
            conn: ci,
            sql: String::new(),
            retry: None,
            started: Instant::now(),
        };
        self.set_focus(Focus::Result);
        let name = self.conns[ci].name.clone();
        self.start_source(Request::SourceOpen { conn: name, path }, job);
    }

    /// 로컬 프로젝트의 flows.json 에서 이름이 name 인 흐름 설정이다. 원격(파일 없음)·없음·깨짐이면 None 이다.
    fn flow_conf(&self, name: &str) -> Option<Flow> {
        self.files.as_ref()?;
        let data = std::fs::read(std::path::Path::new(&self.dir).join(idb_core::flows::FILE)).ok()?;
        let list: Vec<Flow> = serde_json::from_slice::<Option<Vec<Flow>>>(&data).ok()??;
        list.into_iter().find(|f| f.name == name)
    }

    /// 트리 aws SNS 토픽 줄의 F: 그 토픽만의 흐름을 연다(R69.4). 다른 줄이면 false 다.
    pub(crate) fn tree_flow(&mut self, n: usize) -> bool {
        let Some(arn) = self.sns_topic(n) else { return false };
        let node = self.tree.node(n);
        let Some(c) = node.conn() else { return false };
        let aws = self.conns[c].name.clone();
        let name = format!("{} · {aws}", node.label);
        self.open_flows(Some(Adhoc { name, aws, arn }));
        true
    }

    /// n 이 aws 트리의 SNS 토픽 줄이면 그 ARN 이다. 트리 F 와 키 안내(R74.1)가 같이 본다.
    pub(crate) fn sns_topic(&self, n: usize) -> Option<String> {
        let NodeKind::Source { conn, path, .. } = &self.tree.node(n).kind else {
            return None;
        };
        if self.conns[*conn].kind != "aws" {
            return None;
        }
        path.strip_prefix("sns/").filter(|a| !a.contains('/')).map(String::from)
    }

    // ─── 그리기 ───

    /// 흐름 탭을 편집기·결과 자리에 그린다. 왼쪽은 흐름 목록, 오른쪽은 고른 흐름의 그림이다.
    pub(crate) fn draw_flow(&self, buf: &mut Buffer, r: Rect, s: &FlowState) {
        let (w, h) = (r.width as usize, r.height as usize);
        let names = s.names();
        if names.is_empty() {
            let line = if !s.done {
                ("흐름 확인 중…".to_string(), theme::dim())
            } else if !s.file_err.is_empty() {
                (printable(&format!("설정 오류: {}", s.file_err), false), theme::err())
            } else {
                (FLOW_NO_FILE.to_string(), theme::dim())
            };
            put(buf, r.x, r.y, w, &[line]);
            return;
        }
        let list_w = 28.min((w / 4).max(12));
        let mut left: Vec<Segs> = Vec::new();
        if !s.file_err.is_empty() {
            left.push(vec![(printable(&format!("설정 오류: {}", s.file_err), false), theme::err())]);
        }
        // 고른 흐름·상자가 보이게 그 줄이 맨 아래에 오도록 내린다(대시보드 표와 같은 방식)
        let cur = s.cursor.min(names.len() - 1);
        let top = (cur + 1).saturating_sub(h.saturating_sub(left.len()).max(1));
        for (i, name) in names.iter().enumerate().skip(top) {
            let mark = if i == cur {
                ("▸ ".to_string(), Style::new().fg(theme::ACCENT).add_modifier(Modifier::BOLD))
            } else {
                ("  ".to_string(), Style::new())
            };
            let (icon, st) = flow_icon(&s.boxes(i));
            left.push(vec![
                mark,
                (icon.into(), st),
                (
                    format!(" {}", truncate(&printable(name, false), list_w.saturating_sub(4), "…")),
                    Style::new().fg(theme::TEXT),
                ),
            ]);
        }
        let diag_w = w.saturating_sub(list_w + 3).max(10);
        let mut right: Vec<Segs> = Vec::new();
        let mut bs = s.boxes(cur);
        if cur < s.items.len() || !bs.is_empty() {
            let mut title = vec![(
                printable(&names[cur], false),
                Style::new().fg(theme::TEXT).add_modifier(Modifier::BOLD),
            )];
            if bs.iter().any(|b| matches!(b.b.stage.as_str(), "sns" | "sqs")) {
                title.push((format!("  {FLOW_NO_METRICS}"), theme::dim()));
            }
            right.push(title);
            right.push(Vec::new());
            // 행 없는 설정 종류는 맨 뒤라 빼도 앞 상자 자리가 그대로다. 높이를 넘으면 뺀다
            if bs.iter().any(|b| b.b.idle) && flow_layout(&bs, diag_w).len() + 2 > h {
                bs.retain(|b| !b.b.idle);
                align_types(&mut bs);
            }
            let sel = s.sel.min(bs.len().saturating_sub(1));
            let lines = flow_layout(&bs, diag_w);
            let at = lines.iter().position(|l| l.iter().any(|(_, b)| *b == Some(sel))).unwrap_or(0);
            let top = (at + 1).saturating_sub(h.saturating_sub(right.len()).max(1));
            right.extend(render_lines(&lines[top..], &bs, sel));
        }
        let n = h.min(left.len().max(right.len()));
        for i in 0..n {
            let y = r.y + i as u16;
            if let Some(l) = left.get(i) {
                put(buf, r.x, y, list_w, l);
            }
            put(buf, r.x + list_w as u16, y, 3, &[(" │ ".into(), theme::dim())]);
            if let Some(l) = right.get(i) {
                put(buf, r.x + list_w as u16 + 3, y, diag_w.min(w.saturating_sub(list_w + 3)), l);
            }
        }
    }

    /// 흐름 탭 머리의 `확인 중 N` 수다.
    pub(crate) fn flow_pending(s: &FlowState) -> usize {
        if s.job.is_none() { 0 } else { s.items.len().max(1) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fb(stage: &str, name: &str, metrics: &str) -> FlowBox {
        FlowBox {
            stage: stage.into(),
            name: name.into(),
            metrics: metrics.into(),
            ..Default::default()
        }
    }

    fn text(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| l.iter().map(|(s, _)| s.as_str()).collect()).collect()
    }

    #[test]
    fn 가로_그림은_큐가_여럿이면_갈라지고_dlq_는_큐_아래_가지다() {
        let bs = vboxes(&[
            fb("outbox", "shop-stg public.t_outbox", "대기 120 · 재시도 3"),
            fb("sns", "hub", ""),
            fb("sqs", "q1", ""),
            fb("dlq", "q1-dlq", ""),
            fb("lambda", "worker", "Enabled · OK"),
            fb("sqs", "q2", ""),
        ]);
        let got = text(&flow_layout(&bs, 200));
        assert_eq!(
            got[0],
            "[아웃박스 shop-stg public.t_outbox | 대기 120 · 재시도 3] ──▶ [SNS hub] ─┬─▶ [SQS q1] ──▶ [λ worker OK]"
        );
        assert!(got[1].ends_with("  │   └─ DLQ q1-dlq"), "{got:?}");
        assert!(got[2].ends_with("  └─▶ [SQS q2]"), "{got:?}");
    }

    #[test]
    fn 좁으면_세로로_쌓고_종류_줄은_아래에_붙는다() {
        let bs = vboxes(&[
            fb("outbox", "c t", "대기 1"),
            fb("sqs", "q1", ""),
            fb("lambda", "w", "Disabled · "),
            fb(STAGE_TYPE, "CLIP", "대기 0 · → consume-clip"),
            fb(STAGE_TYPE, "CREATE_X", "대기 3 · → ?"),
        ]);
        let got = text(&flow_layout(&bs, 20));
        assert_eq!(
            got,
            [
                "[아웃박스 c t | 대기 1]",
                "  └─▶ [SQS q1]",
                "        └─▶ [λ w Disabled]",
                "  ├─ CLIP      대기 0  → consume-clip",
                "  └─ CREATE_X  대기 3  → ?",
            ]
        );
    }
}
