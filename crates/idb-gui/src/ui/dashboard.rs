//! 대시보드 탭: 접속 상태(R64)·흐름(R69)·보드(R70). 모두 읽기 전용 확인이라 확인 창 흐름(guarded) 없이 직접 띄우고,
//! 오류는 칸 안에 보인다. 열려 있는 동안 30초마다 다시 본다.

use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    Context, InteractiveElement as _, IntoElement, ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _, px, rems,
};
use gpui_component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_component::{ActiveTheme as _, IconName, Sizable as _, h_flex, v_flex};
use idb_engine::{BoardResult, BoardSummary, ConnSummary, Engine, FlowResult, HealthInfo, OutboxExhausted};

use crate::vm::dashboard::{self, PanelKind, State};
use crate::vm::runtime::Runtime;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Health,
    Flows,
    Boards,
}

pub struct DashboardView {
    rt: Rc<Runtime>,
    engine: Engine,
    conns: Vec<ConnSummary>,
    pub(super) section: Section,
    pub(super) health: HashMap<String, Result<HealthInfo, String>>,
    outbox: Vec<OutboxExhausted>,
    pub(super) checking: usize,
    flows: Option<Result<Vec<FlowResult>, String>>,
    selected_flow: usize,
    boards: Option<Result<Vec<BoardSummary>, String>>,
    board: Option<(String, Result<BoardResult, String>)>,
    board_prod: bool,
}

impl DashboardView {
    pub fn new(rt: Rc<Runtime>, engine: Engine, conns: Vec<ConnSummary>, window: &mut Window, cx: &mut Context<Self>) -> DashboardView {
        cx.defer_in(window, |v, _, cx| v.refresh(cx));
        // 열려 있는 동안 30초마다(R64.2). 탭을 닫으면 뷰가 사라져 고리도 끝난다
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(30)).await;
                if this.update(cx, |v, cx| v.refresh(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        DashboardView {
            rt,
            engine,
            conns,
            section: Section::Health,
            health: HashMap::new(),
            outbox: Vec::new(),
            checking: 0,
            flows: None,
            selected_flow: 0,
            boards: None,
            board: None,
            board_prod: false,
        }
    }

    /// 지금 고른 칸을 다시 본다. 다시 보는 동안 앞 결과를 그대로 둔다.
    pub(super) fn refresh(&mut self, cx: &mut Context<Self>) {
        match self.section {
            Section::Health => self.refresh_health(cx),
            Section::Flows => self.refresh_flows(cx),
            Section::Boards => self.refresh_boards(cx),
        }
    }

    fn refresh_health(&mut self, cx: &mut Context<Self>) {
        for c in self.conns.clone() {
            let (e, name) = (self.engine.clone(), c.name.clone());
            let job = self.rt.spawn(async move { e.health(&name).await });
            self.checking += 1;
            let name = c.name.clone();
            cx.spawn(async move |this, cx| {
                let r = job.rx.await;
                let _ = this.update(cx, |v, cx| {
                    v.checking = v.checking.saturating_sub(1);
                    let r = match r {
                        Ok(Ok(h)) => Ok(h),
                        Ok(Err(e)) => Err(e.error),
                        Err(_) => Err("멈춤".into()),
                    };
                    v.health.insert(name, r);
                    cx.notify();
                });
            })
            .detach();
        }
        let e = self.engine.clone();
        let job = self.rt.spawn(async move { e.outbox_exhausted().await });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(o)) = job.rx.await {
                let _ = this.update(cx, |v, cx| {
                    v.outbox = o;
                    cx.notify();
                });
            }
        })
        .detach();
        cx.notify();
    }

    fn refresh_flows(&mut self, cx: &mut Context<Self>) {
        let e = self.engine.clone();
        // 대시보드처럼 prod 접속도 푼다(R69.4, 읽기 전용·요금 없는 호출만)
        let job = self.rt.spawn(async move { e.flows(Vec::new(), true).await });
        self.checking += 1;
        cx.spawn(async move |this, cx| {
            let r = job.rx.await;
            let _ = this.update(cx, |v, cx| {
                v.checking = v.checking.saturating_sub(1);
                v.flows = Some(match r {
                    Ok(Ok(f)) => Ok(f),
                    Ok(Err(e)) => Err(e.error),
                    Err(_) => Err("멈춤".into()),
                });
                cx.notify();
            });
        })
        .detach();
    }

    fn refresh_boards(&mut self, cx: &mut Context<Self>) {
        let e = self.engine.clone();
        let job = self.rt.spawn(async move { e.board_list().await });
        cx.spawn(async move |this, cx| {
            let r = job.rx.await;
            let _ = this.update(cx, |v, cx| {
                v.boards = Some(match r {
                    Ok(Ok(b)) => Ok(b),
                    Ok(Err(e)) => Err(e.error),
                    Err(_) => Err("멈춤".into()),
                });
                cx.notify();
            });
        })
        .detach();
        if let Some((name, _)) = self.board.clone() {
            self.run_board(name, cx);
        }
    }

    fn run_board(&mut self, name: String, cx: &mut Context<Self>) {
        let (e, n, prod) = (self.engine.clone(), name.clone(), self.board_prod);
        let job = self.rt.spawn(async move { e.board_run(&n, prod).await });
        self.checking += 1;
        cx.spawn(async move |this, cx| {
            let r = job.rx.await;
            let _ = this.update(cx, |v, cx| {
                v.checking = v.checking.saturating_sub(1);
                v.board = Some((
                    name,
                    match r {
                        Ok(Ok(b)) => Ok(b),
                        Ok(Err(e)) => Err(e.error),
                        Err(_) => Err("멈춤".into()),
                    },
                ));
                cx.notify();
            });
        })
        .detach();
    }

    fn state_color(&self, s: State, cx: &Context<Self>) -> gpui::Hsla {
        let t = cx.theme();
        match s {
            State::Ok => t.success,
            State::Warn => t.warning,
            State::Fail => t.danger,
            State::Pending => t.muted_foreground,
        }
    }

    fn render_health(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let exhausted: i64 = self.outbox.iter().map(|o| o.exhausted).sum();
        let mut cards = h_flex().gap_2().flex_wrap();
        for (title, value, warn) in dashboard::summary(&self.conns, &self.health, exhausted) {
            cards = cards.child(
                v_flex()
                    .p_3()
                    .min_w(px(180.))
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.popover)
                    .child(div().text_xs().text_color(theme.muted_foreground).child(title))
                    .child(div().text_color(if warn { theme.warning } else { theme.foreground }).child(value)),
            );
        }
        let mut rows = v_flex().gap_px();
        for c in &self.conns {
            let h = self.health.get(&c.name);
            let st = dashboard::health_state(h);
            let (latency, version, detail) = match h {
                Some(Ok(h)) => (
                    format!("{}ms", h.latency_ms),
                    h.version.clone(),
                    if h.err.is_empty() {
                        h.metrics
                            .iter()
                            .map(|m| format!("{} {}", m.name, m.value))
                            .collect::<Vec<_>>()
                            .join(" · ")
                    } else {
                        h.err.clone()
                    },
                ),
                Some(Err(e)) => (String::new(), String::new(), e.clone()),
                None => (String::new(), String::new(), String::new()),
            };
            rows = rows.child(
                h_flex()
                    .gap_3()
                    .px_3()
                    .h(rems(2.0))
                    .rounded(theme.radius)
                    .hover(|s| s.bg(theme.secondary.opacity(0.5)))
                    .child(div().w(rems(1.0)).text_color(self.state_color(st, cx)).child(st.symbol()))
                    .child(div().w(px(200.)).truncate().child(c.name.clone()))
                    .child(
                        div()
                            .w(px(60.))
                            .text_xs()
                            .text_color(if c.env == "prod" { theme.danger } else { theme.muted_foreground })
                            .child(c.env.clone()),
                    )
                    .child(div().w(px(90.)).text_xs().text_color(theme.muted_foreground).child(c.kind.clone()))
                    .child(div().w(px(70.)).text_xs().child(latency))
                    .child(
                        div()
                            .w(px(120.))
                            .text_xs()
                            .truncate()
                            .text_color(theme.muted_foreground)
                            .child(version),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .truncate()
                            .text_color(if st == State::Fail { theme.danger } else { theme.muted_foreground })
                            .child(detail),
                    ),
            );
        }
        v_flex().gap_3().child(cards).child(rows).into_any_element()
    }

    fn render_flows(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let flows = match &self.flows {
            None => return div().text_color(theme.muted_foreground).child("읽는 중…").into_any_element(),
            Some(Err(e)) => return div().text_color(theme.danger).child(e.clone()).into_any_element(),
            Some(Ok(f)) if f.is_empty() => {
                return div()
                    .text_color(theme.muted_foreground)
                    .child(".dbtui/flows.json 에 흐름이 없다")
                    .into_any_element();
            }
            Some(Ok(f)) => f,
        };
        let sel = self.selected_flow.min(flows.len() - 1);
        let mut list = v_flex().w(px(240.)).gap_px();
        for (i, f) in flows.iter().enumerate() {
            let st = dashboard::flow_state(Some(f));
            list = list.child(
                h_flex()
                    .id(("flow", i))
                    .gap_2()
                    .px_2()
                    .h(rems(1.9))
                    .rounded(theme.radius)
                    .cursor_pointer()
                    .when(i == sel, |d| d.bg(theme.secondary))
                    .hover(|s| s.bg(theme.secondary.opacity(0.5)))
                    .child(div().text_color(self.state_color(st, cx)).child(st.symbol()))
                    .child(div().truncate().child(f.flow.clone()))
                    .on_click(cx.listener(move |v, _, _, cx| {
                        v.selected_flow = i;
                        cx.notify();
                    })),
            );
        }
        let mut boxes = h_flex().gap_2().flex_wrap().items_start();
        for (i, b) in flows[sel].boxes.iter().enumerate() {
            if i > 0 {
                boxes = boxes.child(div().pt_4().text_color(theme.muted_foreground).child("→"));
            }
            let color = if !b.err.is_empty() {
                theme.danger
            } else if b.warn {
                theme.warning
            } else {
                theme.border
            };
            boxes = boxes.child(
                v_flex()
                    .p_3()
                    .min_w(px(160.))
                    .max_w(px(260.))
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(color)
                    .bg(theme.popover)
                    .child(div().text_xs().text_color(theme.muted_foreground).child(b.stage.clone()))
                    .child(div().truncate().child(b.name.clone()))
                    .when(!b.metrics.is_empty(), |d| d.child(div().text_xs().child(b.metrics.clone())))
                    .when(!b.err.is_empty(), |d| {
                        d.child(div().text_xs().text_color(theme.danger).child(b.err.clone()))
                    })
                    .when(!b.note.is_empty(), |d| {
                        d.child(div().text_xs().text_color(theme.muted_foreground).child(b.note.clone()))
                    }),
            );
        }
        h_flex()
            .gap_4()
            .items_start()
            .child(list)
            .child(div().flex_1().child(boxes))
            .into_any_element()
    }

    fn render_boards(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let boards = match &self.boards {
            None => return div().text_color(theme.muted_foreground).child("읽는 중…").into_any_element(),
            Some(Err(e)) => return div().text_color(theme.danger).child(e.clone()).into_any_element(),
            Some(Ok(b)) if b.is_empty() => {
                return div()
                    .text_color(theme.muted_foreground)
                    .child(".dbtui/boards/ 에 보드가 없다")
                    .into_any_element();
            }
            Some(Ok(b)) => b,
        };
        let cur = self.board.as_ref().map(|(n, _)| n.clone());
        let mut list = v_flex().w(px(240.)).gap_px();
        for (i, b) in boards.iter().enumerate() {
            let name = b.name.clone();
            list = list.child(
                v_flex()
                    .id(("board", i))
                    .px_2()
                    .py_1()
                    .rounded(theme.radius)
                    .cursor_pointer()
                    .when(cur.as_deref() == Some(b.name.as_str()), |d| d.bg(theme.secondary))
                    .hover(|s| s.bg(theme.secondary.opacity(0.5)))
                    .child(
                        div()
                            .truncate()
                            .child(if b.title.is_empty() { b.name.clone() } else { b.title.clone() }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(if b.error.is_empty() { theme.muted_foreground } else { theme.danger })
                            .child(if b.error.is_empty() {
                                format!("패널 {}개", b.panels)
                            } else {
                                b.error.clone()
                            }),
                    )
                    .on_click(cx.listener(move |v, _, _, cx| v.run_board(name.clone(), cx))),
            );
        }
        let body = match &self.board {
            None => div()
                .text_color(theme.muted_foreground)
                .child("왼쪽에서 보드를 고른다")
                .into_any_element(),
            Some((_, Err(e))) => div().text_color(theme.danger).child(e.clone()).into_any_element(),
            Some((_, Ok(b))) => {
                let mut grid = h_flex().gap_2().flex_wrap().items_start();
                if !b.error.is_empty() {
                    grid = grid.child(div().text_color(theme.danger).child(b.error.clone()));
                }
                for p in &b.panels {
                    let kind = dashboard::panel_kind(p);
                    let mut card = v_flex()
                        .p_3()
                        .min_w(px(200.))
                        .max_w(px(420.))
                        .rounded(theme.radius_lg)
                        .border_1()
                        .border_color(if p.warn || kind == PanelKind::Error {
                            theme.warning
                        } else {
                            theme.border
                        })
                        .bg(theme.popover)
                        .child(
                            h_flex()
                                .gap_2()
                                .child(div().text_xs().text_color(theme.muted_foreground).child(p.title.clone()))
                                .when(p.prod, |d| d.child(div().text_xs().text_color(theme.danger).child("prod"))),
                        );
                    card = match kind {
                        PanelKind::Error => card.child(div().text_xs().text_color(theme.danger).child(p.error.clone())),
                        PanelKind::Number => card.child(
                            div()
                                .text_2xl()
                                .text_color(if p.warn { theme.warning } else { theme.foreground })
                                .child(p.value.clone().unwrap_or_default()),
                        ),
                        PanelKind::Table => {
                            let rs = p.result.clone().unwrap_or_default();
                            let mut t = v_flex().text_xs();
                            t = t.child(
                                h_flex()
                                    .gap_3()
                                    .text_color(theme.muted_foreground)
                                    .children(rs.cols.iter().map(|c| div().w(px(90.)).truncate().child(c.clone()))),
                            );
                            for r in rs.rows.iter().take(20) {
                                t = t.child(
                                    h_flex()
                                        .gap_3()
                                        .children(r.iter().map(|c| div().w(px(90.)).truncate().child(c.clone()))),
                                );
                            }
                            card.child(t)
                        }
                        PanelKind::Flow => card.child(v_flex().text_xs().children(p.boxes.iter().map(|b| {
                            format!(
                                "{} {} · {}{}",
                                b.stage,
                                b.name,
                                b.metrics,
                                if b.err.is_empty() {
                                    String::new()
                                } else {
                                    format!(" · {}", b.err)
                                }
                            )
                        }))),
                        PanelKind::Note => card,
                    };
                    if !p.note.is_empty() {
                        card = card.child(div().text_xs().text_color(theme.muted_foreground).child(p.note.clone()));
                    }
                    grid = grid.child(card);
                }
                grid.into_any_element()
            }
        };
        h_flex()
            .gap_4()
            .items_start()
            .child(list)
            .child(
                v_flex()
                    .flex_1()
                    .gap_2()
                    .child(
                        Button::new("board-prod")
                            .xsmall()
                            .with_variant(if self.board_prod {
                                ButtonVariant::Danger
                            } else {
                                ButtonVariant::Ghost
                            })
                            .label(if self.board_prod {
                                "prod 패널도 실행 중"
                            } else {
                                "prod 패널도 실행"
                            })
                            .on_click(cx.listener(|v, _, _, cx| {
                                v.board_prod = !v.board_prod;
                                if let Some((name, _)) = v.board.clone() {
                                    v.run_board(name, cx);
                                }
                                cx.notify();
                            })),
                    )
                    .child(body),
            )
            .into_any_element()
    }
}

impl Render for DashboardView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let tab = |id: &'static str, label: &'static str, s: Section, cur: Section| {
            Button::new(id)
                .small()
                .with_variant(if s == cur { ButtonVariant::Secondary } else { ButtonVariant::Ghost })
                .label(label)
        };
        let cur = self.section;
        let body = match self.section {
            Section::Health => self.render_health(cx),
            Section::Flows => self.render_flows(cx),
            Section::Boards => self.render_boards(cx),
        };
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .gap_1()
                    .px_3()
                    .h(rems(2.5))
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        tab("sec-health", "접속 상태", Section::Health, cur).on_click(cx.listener(|v, _, _, cx| {
                            v.section = Section::Health;
                            v.refresh(cx);
                        })),
                    )
                    .child(tab("sec-flows", "흐름", Section::Flows, cur).on_click(cx.listener(|v, _, _, cx| {
                        v.section = Section::Flows;
                        v.refresh(cx);
                    })))
                    .child(tab("sec-boards", "보드", Section::Boards, cur).on_click(cx.listener(|v, _, _, cx| {
                        v.section = Section::Boards;
                        v.refresh(cx);
                    })))
                    .child(div().flex_1())
                    .when(self.checking > 0, |d| {
                        d.child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(format!("확인 중 {}", self.checking)),
                        )
                    })
                    .child(
                        Button::new("dash-refresh")
                            .small()
                            .ghost()
                            .icon(IconName::RefreshCw)
                            .label("다시 보기")
                            .on_click(cx.listener(|v, _, _, cx| v.refresh(cx))),
                    ),
            )
            .child(div().id("dash-body").flex_1().min_h_0().overflow_y_scroll().p_4().child(body))
    }
}
