//! 결과 표 화면. 그리기는 gpui-component DataTable(보이는 줄만 그린다)에 맡기고 순서·거르기·편집은
//! `vm::grid`·`vm::edits` 가 정한다. 테이블 보기는 끝에 닿으면 `table_page` 로 다음 쪽을 붙인다.

use std::rc::Rc;

use gpui::{
    App, AppContext as _, ClipboardItem, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, WeakEntity, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_component::table::{Column, ColumnSort, DataTable, TableDelegate, TableEvent, TableState};
use gpui_component::{ActiveTheme as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use idb_engine::{Applied, ChangeSet, Engine, EngineError, ResultSet, TablePage, TablePageRequest};

use super::workspace::{Hooks, Workspace};
use crate::vm::edits::Edits;
use crate::vm::grid::{Dir, Grid, NULL};
use crate::vm::runtime::{Job, Runtime};
use crate::vm::safety::Confirms;
use crate::vm::sqlcopy;

/// 결과가 어디서 왔나. 다시 읽기·다음 쪽에 쓴다.
#[derive(Clone)]
pub enum Source {
    Query {
        sql: String,
    },
    Table {
        schema: String,
        table: String,
        filter: String,
        order: String,
    },
    /// 다시 읽지 않는 결과(Redis 값·명령)
    Static,
}

type Loader = Rc<dyn Fn(u64) -> Job<Result<TablePage, EngineError>>>;

pub struct GridDelegate {
    pub grid: Grid,
    pub edits: Option<Edits>,
    more: bool,
    loading: bool,
    loader: Option<Loader>,
    /// 테이블 보기 한 쪽의 행 수(설정, 엔진 상한 1,000)
    page: u64,
    /// 데이터베이스를 바꾸는 중이면 다음 쪽을 읽지 않는다
    frozen: bool,
    /// 결과의 출처 테이블(INSERT 복사에 쓴다). 편집 상태와 달리 데이터베이스를 바꿔도 이름은 그대로라 남긴다
    origin: Option<idb_engine::Origin>,
    /// 마지막으로 오른쪽 단추를 누른 칸(열을 고르는 데 쓴다)
    ctx_cell: Option<(usize, usize)>,
    owner: Option<WeakEntity<ResultsView>>,
}

impl GridDelegate {
    fn display(&self, row: usize, col: usize) -> (String, CellMark) {
        let Some(src) = self.grid.source_row(row) else {
            return (String::new(), CellMark::Plain);
        };
        let base = self.grid.cell(row, col).to_string();
        let Some(e) = &self.edits else {
            return (base, CellMark::Plain);
        };
        if e.is_deleted(src) {
            return (base, CellMark::Deleted);
        }
        match e.value(src, col) {
            Some(Some(v)) => (v.to_string(), CellMark::Edited),
            Some(None) => (NULL.to_string(), CellMark::Edited),
            None => (base, CellMark::Plain),
        }
    }
}

#[derive(PartialEq)]
enum CellMark {
    Plain,
    Edited,
    Deleted,
}

/// 칸에 보일 한 줄이다. 긴 글은 앞부분만 둔다(칸 너비 밖은 어차피 잘린다).
fn one_line(s: &str) -> String {
    let s: String = s.chars().take(300).collect();
    s.replace(['\n', '\r', '\t'], " ")
}

impl TableDelegate for GridDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.grid.cols.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.grid.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        let name = &self.grid.cols[col_ix];
        let ty = self.grid.types.get(col_ix).cloned().unwrap_or_default();
        let w = (name.chars().count().max(ty.chars().count()) as f32 * 8.0 + 40.0).clamp(80.0, 320.0);
        let sort = match self.grid.sort_state() {
            Some((c, Dir::Asc)) if c == col_ix => ColumnSort::Ascending,
            Some((c, Dir::Desc)) if c == col_ix => ColumnSort::Descending,
            _ => ColumnSort::Default,
        };
        Column::new(SharedString::from(format!("c{col_ix}")), name.clone())
            .width(px(w))
            .sort(sort)
    }

    fn render_th(&mut self, col_ix: usize, _: &mut Window, cx: &mut Context<TableState<Self>>) -> impl IntoElement {
        let ty = self.grid.types.get(col_ix).cloned().unwrap_or_default();
        let editable = self.edits.as_ref().is_some_and(|e| e.editable(col_ix));
        v_flex()
            .size_full()
            .justify_center()
            .child(div().child(self.grid.cols[col_ix].clone()))
            .when(!ty.is_empty() || editable, |d| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(if editable { format!("{ty} ✎") } else { ty }),
                )
            })
    }

    fn perform_sort(&mut self, col_ix: usize, sort: ColumnSort, _: &mut Window, _: &mut Context<TableState<Self>>) {
        self.grid.sort_by(match sort {
            ColumnSort::Ascending => Some((col_ix, Dir::Asc)),
            ColumnSort::Descending => Some((col_ix, Dir::Desc)),
            ColumnSort::Default => None,
        });
    }

    fn render_td(&mut self, row_ix: usize, col_ix: usize, _: &mut Window, cx: &mut Context<TableState<Self>>) -> impl IntoElement {
        let (text, mark) = self.display(row_ix, col_ix);
        let is_null = text == NULL;
        div()
            .size_full()
            .overflow_hidden()
            .when(is_null, |d| d.text_color(cx.theme().muted_foreground).italic())
            .when(mark == CellMark::Edited, |d| d.bg(cx.theme().warning.opacity(0.25)))
            .when(mark == CellMark::Deleted, |d| d.bg(cx.theme().danger.opacity(0.2)).line_through())
            .child(one_line(&text))
    }

    fn context_menu(&mut self, row_ix: usize, menu: PopupMenu, _: &mut Window, _: &mut Context<TableState<Self>>) -> PopupMenu {
        let col = self.ctx_cell.filter(|(r, _)| *r == row_ix).map_or(0, |(_, c)| c);
        let cell = self.display(row_ix, col).0;
        let row = self.grid.tsv([row_ix], &self.grid.all_cols(), false);
        let src = self
            .grid
            .source_row(row_ix)
            .map(|r| self.grid.source(r).to_vec())
            .unwrap_or_default();
        let insert = self.origin.as_ref().and_then(|o| sqlcopy::insert_from_origin(o, &[src]));
        let col_vals: Vec<String> = (0..self.grid.len()).map(|i| self.grid.cell(i, col).to_string()).collect();
        let in_list = sqlcopy::in_list(&col_vals.iter().map(String::as_str).collect::<Vec<_>>());
        let col_name = self.grid.cols.get(col).cloned().unwrap_or_default();
        let (o1, o2) = (self.owner.clone(), self.owner.clone());
        let copy = |label: String, text: String| {
            PopupMenuItem::new(label).on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(text.clone())))
        };
        menu.item(copy("칸 복사".into(), cell))
            .item(copy("행 복사 (탭 구분)".into(), row))
            .item(match insert {
                Some(sql) => copy("INSERT 문으로 복사".into(), sql),
                // 한 테이블에서 PK 와 함께 온 결과가 아니면 진짜 열 이름을 몰라 막는다
                None => PopupMenuItem::new("INSERT 문으로 복사 (한 테이블 결과만)").disabled(true),
            })
            .item(copy(format!("{col_name} 열을 IN 목록으로 복사"), in_list))
            .separator()
            .item(PopupMenuItem::new("CSV 로 저장…").on_click(move |_, _, cx| {
                if let Some(o) = &o1 {
                    let _ = o.update(cx, |v, cx| v.export(false, cx));
                }
            }))
            .item(PopupMenuItem::new("JSON 으로 저장…").on_click(move |_, _, cx| {
                if let Some(o) = &o2 {
                    let _ = o.update(cx, |v, cx| v.export(true, cx));
                }
            }))
    }

    fn cell_text(&self, row_ix: usize, col_ix: usize, _: &App) -> String {
        self.display(row_ix, col_ix).0
    }

    fn has_more(&self, _: &App) -> bool {
        self.more && self.loader.is_some()
    }

    fn loading(&self, _: &App) -> bool {
        false
    }

    fn load_more(&mut self, _: &mut Window, cx: &mut Context<TableState<Self>>) {
        let Some(loader) = self.loader.clone() else { return };
        if self.loading || !self.more || self.frozen {
            return;
        }
        self.loading = true;
        let job = loader(self.grid.source_len() as u64);
        cx.spawn(async move |this, cx| {
            let r = job.rx.await;
            let _ = this.update(cx, |state, cx| {
                let d = state.delegate_mut();
                d.loading = false;
                match r {
                    Ok(Ok(page)) => {
                        d.more = page.result.rows.len() as u64 >= d.page;
                        d.grid.append(page.result.rows);
                        let n = d.grid.source_len();
                        if let Some(e) = d.edits.as_mut() {
                            e.rebase(n);
                        }
                    }
                    _ => d.more = false,
                }
                cx.notify();
            });
        })
        .detach();
    }
}

pub struct ResultsView {
    ws: WeakEntity<Workspace>,
    rt: Rc<Runtime>,
    engine: Engine,
    pub conn: String,
    pub source: Source,
    table: Entity<TableState<GridDelegate>>,
    filter: Entity<InputState>,
    /// 표가 아닌 결과(UPDATE 3 같은 명령 태그, EXPLAIN 글)
    text: String,
    note: String,
    elapsed_ms: u64,
    truncated: bool,
    /// 마지막 반영. 되돌리기에 쓴다(R50)
    last_applied: Option<(ChangeSet, Applied)>,
    /// 테이블 보기의 WHERE·ORDER BY 입력칸
    pub(super) table_inputs: Option<(Entity<InputState>, Entity<InputState>)>,
    /// 데이터베이스를 바꾼 뒤의 옛 결과다. 고치기·되돌리기·다음 쪽을 막는다(R27.1·R50)
    pub stale: bool,
    /// 그 접속의 데이터베이스 바꾸기 요청이 도는 중이다(답이 오면 stale 이 되거나 풀린다)
    pub frozen: bool,
    /// 이 결과를 만든 데이터베이스 세대. 반영·되돌리기를 보내기 바로 전에 작업 공간 세대와 견준다
    pub db_gen: u64,
    _subs: Vec<Subscription>,
}

impl ResultsView {
    /// 작업 공간을 고친다. 이 뷰는 작업 공간의 콜백(select_conn·실행 done) 안에서 만들어지거나 불릴 때가 많다.
    /// 그때 바로 `update` 하면 GPUI 가 같은 엔티티를 두 번 빌려 패닉하고, AppKit 콜백 안이라 앱이 abort 된다.
    /// 그래서 늘 효과 주기 끝으로 미룬다.
    fn with_ws(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
    ) {
        let ws = self.ws.clone();
        window.defer(cx, move |window, cx| {
            let _ = ws.update(cx, |ws, cx| f(ws, window, cx));
        });
    }
    // 창(window)·cx 두 자리가 늘 붙는 GPUI 뷰 함수라 묶음 구조체를 따로 두지 않는다
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        ws: WeakEntity<Workspace>,
        rt: Rc<Runtime>,
        engine: Engine,
        conn: String,
        source: Source,
        rs: ResultSet,
        more: bool,
        page: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ResultsView {
        let loader: Option<Loader> = match &source {
            Source::Table {
                schema,
                table,
                filter,
                order,
            } => {
                let (rt, engine, conn) = (rt.clone(), engine.clone(), conn.clone());
                let req = TablePageRequest {
                    conn,
                    schema: schema.clone(),
                    table: table.clone(),
                    filter: filter.clone(),
                    order_by: order.clone(),
                    limit: page,
                    ..Default::default()
                };
                Some(Rc::new(move |offset| {
                    let (engine, mut req) = (engine.clone(), req.clone());
                    req.offset = offset;
                    rt.spawn(async move { engine.table_page(req).await })
                }))
            }
            Source::Query { .. } | Source::Static => None,
        };
        let origin = rs.origin.clone();
        let edits = origin.clone().and_then(|o| Edits::new(o, rs.rows.len()));
        let (text, note, elapsed_ms, truncated) = (rs.text.clone(), rs.note.clone(), rs.elapsed_ms, rs.truncated);
        let delegate = GridDelegate {
            grid: Grid::new(rs),
            edits,
            more,
            loading: false,
            loader,
            page,
            frozen: false,
            origin,
            ctx_cell: None,
            owner: Some(cx.weak_entity()),
        };
        let table = cx.new(|cx| {
            TableState::new(delegate, window, cx)
                .cell_selectable(true)
                .col_movable(false)
                .row_header(true)
        });
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("결과 거르기"));
        // 테이블 보기면 WHERE·ORDER BY 입력칸을 둔다(R16)
        let table_inputs = match &source {
            Source::Table { filter: w, order: o, .. } => {
                let (w, o) = (w.clone(), o.clone());
                let wi = cx.new(|cx| {
                    InputState::new(window, cx)
                        .placeholder("WHERE 조건 (예: id > 100)")
                        .default_value(w)
                });
                let oi = cx.new(|cx| {
                    InputState::new(window, cx)
                        .placeholder("ORDER BY (예: created_at desc)")
                        .default_value(o)
                });
                Some((wi, oi))
            }
            _ => None,
        };
        let mut subs = Vec::new();
        if let Some((wi, oi)) = &table_inputs {
            for i in [wi, oi] {
                subs.push(cx.subscribe_in(i, window, |this, _, ev: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { .. } = ev {
                        this.apply_table_query(window, cx);
                    }
                }));
            }
        }
        subs.extend([
            cx.subscribe_in(&filter, window, |this, input, ev: &InputEvent, _, cx| {
                if let InputEvent::Change = ev {
                    let f = input.read(cx).value().to_string();
                    this.table.update(cx, |t, cx| {
                        t.delegate_mut().grid.set_filter(&f);
                        // 줄 번호가 바뀌므로 옛 선택을 버린다(넘는 줄 번호로 복사·삭제하지 않게)
                        t.clear_selection(cx);
                        t.delegate_mut().ctx_cell = None;
                        t.refresh(cx);
                    });
                }
            }),
            cx.subscribe_in(&table, window, |this, _, ev: &TableEvent, window, cx| match ev {
                TableEvent::DoubleClickedCell(row, col) => this.edit_cell(*row, *col, window, cx),
                TableEvent::RightClickedCell(row, col) | TableEvent::SelectCell(row, col) => {
                    let cell = (*row, *col);
                    this.table.update(cx, |t, _| t.delegate_mut().ctx_cell = Some(cell));
                }
                _ => {}
            }),
        ]);
        ResultsView {
            table_inputs,
            ws,
            rt,
            engine,
            conn,
            source,
            table,
            filter,
            text,
            note,
            elapsed_ms,
            truncated,
            last_applied: None,
            stale: false,
            frozen: false,
            db_gen: 0,
            _subs: subs,
        }
    }

    /// 상태 줄에 보일 행 수·걸린 시간이다.
    pub fn summary(&self, cx: &App) -> String {
        let g = &self.table.read(cx).delegate().grid;
        if g.cols.is_empty() {
            return format!("{}ms", self.elapsed_ms);
        }
        let rows = if g.len() == g.source_len() {
            format!("{}행", g.len())
        } else {
            format!("{}/{}행", g.len(), g.source_len())
        };
        format!("{rows} · {}ms{}", self.elapsed_ms, if self.truncated { " · 잘림" } else { "" })
    }

    /// 접속의 데이터베이스가 바뀌었다. 이 결과의 바꿈·되돌리기 묶음은 옛 데이터베이스 것이라 버리고,
    /// 고치기와 다음 쪽 읽기를 막는다. 보기·복사는 그대로 둔다.
    pub fn set_frozen(&mut self, frozen: bool, cx: &mut Context<Self>) {
        self.frozen = frozen;
        self.table.update(cx, |t, _| t.delegate_mut().frozen = frozen);
        cx.notify();
    }

    /// 옛 결과이거나 데이터베이스를 바꾸는 중이면 고치기·반영·되돌리기를 막고 까닭을 보인다.
    fn blocked(&self, cx: &mut Context<Self>) -> bool {
        let why = if self.stale {
            "데이터베이스를 바꾸기 전 결과라 고치거나 되돌리지 않는다 (다시 실행한다)"
        } else if self.frozen {
            "데이터베이스를 바꾸는 중이라 고치거나 되돌리지 않는다"
        } else {
            return false;
        };
        self.status(why, true, cx);
        true
    }

    pub fn invalidate(&mut self, cx: &mut Context<Self>) {
        self.stale = true;
        self.frozen = false;
        self.last_applied = None;
        self.note = "데이터베이스를 바꾸기 전 결과다. 고치거나 더 읽으려면 다시 실행한다".into();
        self.table.update(cx, |t, cx| {
            let d = t.delegate_mut();
            d.edits = None;
            d.more = false;
            d.loader = None;
            t.refresh(cx);
        });
        cx.notify();
    }

    pub(super) fn pending(&self, cx: &App) -> usize {
        self.table.read(cx).delegate().edits.as_ref().map_or(0, Edits::pending)
    }

    // ─── 복사·내보내기 ───

    fn copy(&mut self, cx: &mut Context<Self>) {
        let t = self.table.read(cx);
        let g = &t.delegate().grid;
        let text = match t.selected_cell() {
            Some((r, c)) => t.delegate().display(r, c).0,
            None => match t.selected_row() {
                Some(r) => g.tsv([r], &g.all_cols(), false),
                None => g.tsv(0..g.len(), &g.all_cols(), true),
            },
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.status("복사함", false, cx);
    }

    fn export(&mut self, json: bool, cx: &mut Context<Self>) {
        let (body, ext) = {
            let g = &self.table.read(cx).delegate().grid;
            if json { (g.json(), "json") } else { (g.csv(), "csv") }
        };
        let dir = dirs_home();
        let rx = cx.prompt_for_new_path(&dir, Some(&format!("result.{ext}")));
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(path))) = rx.await else { return };
            let msg = match std::fs::write(&path, body) {
                Ok(()) => (format!("저장함: {}", path.display()), false),
                Err(e) => (format!("저장 실패: {e}"), true),
            };
            let _ = this.update(cx, |this, cx| this.status(&msg.0, msg.1, cx));
        })
        .detach();
    }

    fn status(&self, msg: &str, error: bool, cx: &mut Context<Self>) {
        let (msg, ws) = (msg.to_string(), self.ws.clone());
        // with_ws 와 같은 까닭으로 미룬다(창이 없는 자리에서도 부른다)
        gpui::App::defer(cx, move |cx| {
            let _ = ws.update(cx, |ws, cx| ws.set_status(msg, error, cx));
        });
    }

    // ─── 편집 ───

    fn edit_cell(&mut self, row: usize, col: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.blocked(cx) {
            return;
        }
        let (src, base, shown, editable) = {
            let d = self.table.read(cx).delegate();
            let Some(e) = &d.edits else {
                self.status("이 결과는 고칠 수 없다(한 테이블의 PK 열이 모두 있어야 한다)", true, cx);
                return;
            };
            let Some(src) = d.grid.source_row(row) else { return };
            (src, d.grid.cell(row, col).to_string(), d.display(row, col).0, e.editable(col))
        };
        if !editable {
            self.status("이 열은 고칠 수 없다(식·생성 열)", true, cx);
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx).default_value(shown));
        let this = cx.entity().downgrade();
        let col_name = self.table.read(cx).delegate().grid.cols.get(col).cloned().unwrap_or_default();
        let first = std::cell::Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if first.replace(false) {
                input.update(cx, |i, cx| i.focus(window, cx));
            }
            let (this2, input2, base2) = (this.clone(), input.clone(), base.clone());
            let (this3, base3) = (this.clone(), base.clone());
            // 확인 버튼과 Enter(on_ok)가 같은 일을 한다. 기본 Dialog 는 버튼을 그리지 않는다
            let ok = Rc::new(move |cx: &mut App| {
                let v = input2.read(cx).value().to_string();
                let _ = this2.update(cx, |view, cx| view.set_cell(src, col, Some(v), &base2, cx));
            });
            let ok2 = ok.clone();
            dialog
                .title(format!("{col_name} 고치기"))
                .w(px(520.))
                .child(Input::new(&input))
                .child(
                    h_flex()
                        .pt_3()
                        .gap_2()
                        .child(
                            Button::new("set-null")
                                .small()
                                .ghost()
                                .label("NULL 로")
                                .on_click(move |_, window, cx| {
                                    let _ = this3.update(cx, |v, cx| v.set_cell(src, col, None, &base3, cx));
                                    window.close_dialog(cx);
                                }),
                        )
                        .child(div().flex_1())
                        .child(
                            Button::new("cell-cancel")
                                .small()
                                .label("취소")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("cell-ok")
                                .small()
                                .primary()
                                .label("확인")
                                .on_click(move |_, window, cx| {
                                    ok2(cx);
                                    window.close_dialog(cx);
                                }),
                        ),
                )
                .on_ok(move |_, _, cx| {
                    ok(cx);
                    true
                })
        });
    }

    pub(super) fn set_cell(&mut self, src: usize, col: usize, v: Option<String>, base: &str, cx: &mut Context<Self>) {
        if self.blocked(cx) {
            return;
        }
        let r = self.table.update(cx, |t, cx| {
            let d = t.delegate_mut();
            let inserted = d.edits.as_ref().is_some_and(|e| e.is_inserted(src));
            let r = d.edits.as_mut().map(|e| e.set(src, col, v, (!inserted).then_some(base)));
            t.refresh(cx);
            r
        });
        if let Some(Err(e)) = r {
            self.status(&e, true, cx);
        }
        cx.notify();
    }

    fn toggle_delete(&mut self, cx: &mut Context<Self>) {
        if self.blocked(cx) {
            return;
        }
        self.table.update(cx, |t, cx| {
            let row = t.selected_cell().map(|(r, _)| r).or(t.selected_row());
            if let Some(src) = row.and_then(|r| t.delegate().grid.source_row(r))
                && let Some(e) = t.delegate_mut().edits.as_mut()
            {
                e.toggle_delete(src);
            }
            t.refresh(cx);
        });
        cx.notify();
    }

    fn undo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let undone = self.table.update(cx, |t, cx| {
            let r = t.delegate_mut().edits.as_mut().is_some_and(Edits::undo);
            t.refresh(cx);
            r
        });
        if !undone {
            self.undo_applied(window, cx);
        }
        cx.notify();
    }

    pub(super) fn discard(&mut self, cx: &mut Context<Self>) {
        self.table.update(cx, |t, cx| {
            if let Some(e) = t.delegate_mut().edits.as_mut() {
                e.clear();
            }
            t.refresh(cx);
        });
        cx.notify();
    }

    /// 보류 중인 바꿈을 미리 보이고 확인하면 반영한다. prod 면 엔진이 confirm_prod 로 한 번 더 묻는다.
    fn apply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.blocked(cx) {
            return;
        }
        let cs = {
            let d = self.table.read(cx).delegate();
            let Some(e) = &d.edits else { return };
            let rows: Vec<Vec<String>> = (0..d.grid.source_len()).map(|i| d.grid.source(i).to_vec()).collect();
            e.change_set(&rows)
        };
        if cs.changes.is_empty() {
            self.status("반영할 바꿈이 없다", false, cx);
            return;
        }
        let (engine, preview_cs) = (self.engine.clone(), cs.clone());
        let job = self.rt.spawn(async move { engine.changes_sql(preview_cs, None).await });
        let this = cx.entity().downgrade();
        cx.spawn_in(window, async move |_, cx| {
            let r = job.rx.await;
            let _ = cx.update(|window, cx| {
                let sql = match r {
                    Ok(Ok(p)) => p.sql.join("\n"),
                    Ok(Err(e)) => format!("-- 미리 보기 실패: {e}"),
                    Err(_) => return,
                };
                let n = cs.changes.len();
                window.open_alert_dialog(cx, move |alert, _, _| {
                    let (this, cs) = (this.clone(), cs.clone());
                    alert
                        .title(format!("{n}건 반영"))
                        .description(sql.clone())
                        .ok_text("반영")
                        .show_cancel(true)
                        .on_ok(move |_, window, cx| {
                            let _ = this.update(cx, |v, cx| v.submit(cs.clone(), window, cx));
                            true
                        })
                });
            });
        })
        .detach();
    }

    pub(super) fn submit(&mut self, cs: ChangeSet, window: &mut Window, cx: &mut Context<Self>) {
        if self.blocked(cx) {
            return;
        }
        let conn = self.conn.clone();
        let this = cx.entity().downgrade();
        let sent = cs.clone();
        let hooks = write_hooks(conn.clone(), self.db_gen);
        self.with_ws(window, cx, move |ws, window, cx| {
            ws.guarded_with(
                "반영",
                Rc::new(move |e: Engine, c: Confirms| {
                    let (conn, cs) = (conn.clone(), cs.clone());
                    async move { e.apply_changes(&conn, cs, c.prod).await }
                }),
                Rc::new(move |ws: &mut Workspace, r: idb_engine::AppliedResult, window, cx| {
                    ws.set_status(format!("반영함: {}건", r.sql.len()), false, cx);
                    let sent = sent.clone();
                    let _ = this.update(cx, |v, cx| {
                        v.last_applied = (!r.applied.no_returning).then(|| (sent, r.applied.clone()));
                        v.reload(window, cx);
                    });
                }),
                hooks,
                Confirms::default(),
                window,
                cx,
            )
        });
    }

    /// 마지막 반영을 되돌린다(R50). 다시 읽은 뒤에도 묶음이 남아 있으면 엔진이 옛값으로 확인한다.
    fn undo_applied(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.blocked(cx) {
            return;
        }
        let Some((cs, applied)) = self.last_applied.clone() else {
            self.status("되돌릴 것이 없다", false, cx);
            return;
        };
        let conn = self.conn.clone();
        let this = cx.entity().downgrade();
        let hooks = write_hooks(conn.clone(), self.db_gen);
        self.with_ws(window, cx, move |ws, window, cx| {
            ws.guarded_with(
                "반영 되돌리기",
                Rc::new(move |e: Engine, c: Confirms| {
                    let (conn, cs, applied) = (conn.clone(), cs.clone(), applied.clone());
                    async move { e.undo_changes(&conn, cs, applied, c.prod).await }
                }),
                Rc::new(move |ws: &mut Workspace, _r: idb_engine::AppliedResult, window, cx| {
                    ws.set_status("반영을 되돌림".to_string(), false, cx);
                    let _ = this.update(cx, |v, cx| {
                        v.last_applied = None;
                        v.reload(window, cx);
                    });
                }),
                hooks,
                Confirms::default(),
                window,
                cx,
            )
        });
    }

    /// WHERE·ORDER BY 로 테이블 보기를 다시 읽고 기록에 남긴다.
    pub(super) fn apply_table_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some((wi, oi)), Source::Table { schema, table, .. }) = (&self.table_inputs, &self.source) else {
            return;
        };
        let (w, o) = (wi.read(cx).value().trim().to_string(), oi.read(cx).value().trim().to_string());
        let source = Source::Table {
            schema: schema.clone(),
            table: table.clone(),
            filter: w.clone(),
            order: o.clone(),
        };
        let conn = self.conn.clone();
        self.with_ws(window, cx, move |ws, window, cx| {
            crate::vm::history::push(&mut ws.prefs.where_history, &w);
            crate::vm::history::push(&mut ws.prefs.order_history, &o);
            ws.save_prefs();
            ws.load_source(conn, source, window, cx);
        });
    }

    fn render_table_inputs(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let (wi, oi) = self.table_inputs.clone()?;
        let theme = cx.theme().clone();
        let (wh, oh) = self
            .ws
            .upgrade()
            .map(|w| {
                let p = &w.read(cx).prefs;
                (p.where_history.clone(), p.order_history.clone())
            })
            .unwrap_or_default();
        let recent = |id: &'static str, input: Entity<InputState>, items: Vec<String>| {
            Button::new(id)
                .xsmall()
                .ghost()
                .icon(gpui_component::IconName::ChevronDown)
                .tooltip("최근 기록")
                .dropdown_menu(move |mut menu, _, _| {
                    if items.is_empty() {
                        return menu.item(PopupMenuItem::new("기록 없음").disabled(true));
                    }
                    for it in &items {
                        let (input, it2) = (input.clone(), it.clone());
                        menu = menu.item(PopupMenuItem::new(it.clone()).on_click(move |_, window, cx| {
                            input.update(cx, |i, cx| i.set_value(it2.clone(), window, cx));
                        }));
                    }
                    menu
                })
        };
        Some(
            h_flex()
                .gap_1()
                .px_2()
                .py_1()
                .border_b_1()
                .border_color(theme.border)
                .child(div().text_xs().text_color(theme.muted_foreground).child("WHERE"))
                .child(div().flex_1().child(Input::new(&wi).small()))
                .child(recent("where-recent", wi.clone(), wh))
                .child(div().text_xs().text_color(theme.muted_foreground).child("ORDER BY"))
                .child(div().w(px(220.)).child(Input::new(&oi).small()))
                .child(recent("order-recent", oi.clone(), oh))
                .child(
                    Button::new("table-apply")
                        .xsmall()
                        .primary()
                        .label("적용 ↵")
                        .on_click(cx.listener(|v, _, w, cx| v.apply_table_query(w, cx))),
                )
                .into_any_element(),
        )
    }

    /// 같은 출처를 다시 읽어 표를 갈아 끼운다.
    pub fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.source, Source::Static) {
            return;
        }
        let source = self.source.clone();
        let conn = self.conn.clone();
        self.with_ws(window, cx, move |ws, window, cx| ws.load_source(conn, source, window, cx));
    }

    fn add_row(&mut self, cx: &mut Context<Self>) {
        if self.blocked(cx) {
            return;
        }
        self.table.update(cx, |t, cx| {
            if let Some(e) = t.delegate_mut().edits.as_mut() {
                e.add_row();
            }
            t.refresh(cx);
        });
        self.status("추가 행은 반영 뒤 다시 읽을 때 보인다. 값은 칸을 두 번 눌러 정한다", false, cx);
        cx.notify();
    }
}

/// 반영·되돌리기는 (확인 뒤 다시 보낼 때도) 보내기 바로 전에 데이터베이스 세대·바꾸는 중을 다시 본다.
fn write_hooks(conn: String, gen0: u64) -> Hooks {
    Hooks {
        precheck: Some(Rc::new(move |ws: &Workspace| ws.write_ok(&conn, gen0))),
        fail: None,
        cancelled: None,
    }
}

fn dirs_home() -> std::path::PathBuf {
    std::env::var_os("HOME").map(Into::into).unwrap_or_else(|| ".".into())
}

impl Render for ResultsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let (rows, total, editable, has_cols, inserts) = {
            let d = self.table.read(cx).delegate();
            (
                d.grid.len(),
                d.grid.source_len(),
                d.edits.is_some(),
                !d.grid.cols.is_empty(),
                d.edits.as_ref().map_or(0, Edits::inserted_rows),
            )
        };
        let pending = self.pending(cx);
        let mut info = if rows == total {
            format!("{total}행")
        } else {
            format!("{rows}/{total}행")
        };
        info.push_str(&format!(" · {}ms", self.elapsed_ms));
        if self.truncated {
            info.push_str(" · 잘림");
        }
        if inserts > 0 {
            info.push_str(&format!(" · 추가 {inserts}"));
        }
        let table_inputs_el = self.render_table_inputs(cx);
        let toolbar = h_flex()
            .gap_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(theme.border)
            .child(div().w(px(220.)).child(Input::new(&self.filter).small().cleanable(true)))
            .child(div().text_xs().text_color(theme.muted_foreground).px_2().child(info))
            .child(div().flex_1())
            .when(editable, |d| {
                d.child(
                    Button::new("add-row")
                        .xsmall()
                        .ghost()
                        .label("행 추가")
                        .on_click(cx.listener(|v, _, _, cx| v.add_row(cx))),
                )
                .child(
                    Button::new("del-row")
                        .xsmall()
                        .ghost()
                        .label("행 삭제")
                        .on_click(cx.listener(|v, _, _, cx| v.toggle_delete(cx))),
                )
                .child(
                    Button::new("undo")
                        .xsmall()
                        .ghost()
                        .label("되돌리기")
                        .on_click(cx.listener(|v, _, w, cx| v.undo(w, cx))),
                )
                .when(pending > 0, |d| {
                    d.child(
                        Button::new("discard")
                            .xsmall()
                            .ghost()
                            .label("버리기")
                            .on_click(cx.listener(|v, _, _, cx| v.discard(cx))),
                    )
                    .child(
                        Button::new("apply")
                            .xsmall()
                            .primary()
                            .label(format!("반영 ({pending})"))
                            .on_click(cx.listener(|v, _, w, cx| v.apply(w, cx))),
                    )
                })
            })
            .child(
                Button::new("copy")
                    .xsmall()
                    .ghost()
                    .label("복사")
                    .on_click(cx.listener(|v, _, _, cx| v.copy(cx))),
            )
            .child(
                Button::new("csv")
                    .xsmall()
                    .ghost()
                    .label("CSV")
                    .on_click(cx.listener(|v, _, _, cx| v.export(false, cx))),
            )
            .child(
                Button::new("json")
                    .xsmall()
                    .ghost()
                    .label("JSON")
                    .on_click(cx.listener(|v, _, _, cx| v.export(true, cx))),
            )
            .when(!matches!(self.source, Source::Static), |d| {
                d.child(
                    Button::new("reload")
                        .xsmall()
                        .ghost()
                        .label("다시 읽기")
                        .on_click(cx.listener(|v, _, w, cx| v.reload(w, cx))),
                )
            });

        v_flex()
            .size_full()
            // 메뉴 편집 > 복사(⌘C)가 표에 초점이 있을 때 여기로 온다
            .on_action(cx.listener(|v, _: &gpui_component::input::Copy, _, cx| v.copy(cx)))
            .child(toolbar)
            .children(table_inputs_el)
            .when(!self.note.is_empty(), |d| {
                d.child(div().px_2().py_1().text_xs().text_color(theme.warning).child(self.note.clone()))
            })
            .child(if has_cols {
                div()
                    .flex_1()
                    .min_h_0()
                    .child(DataTable::new(&self.table).stripe(true))
                    .into_any_element()
            } else {
                div()
                    .id("result-text")
                    .flex_1()
                    .p_3()
                    .overflow_y_scroll()
                    .font_family(theme.mono_font_family.clone())
                    .child(if self.text.is_empty() {
                        "결과 없음".to_string()
                    } else {
                        self.text.clone()
                    })
                    .into_any_element()
            })
    }
}
