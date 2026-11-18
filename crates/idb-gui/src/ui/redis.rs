//! Redis 탭. 왼쪽은 패턴으로 찾은 키(1,000개까지), 오른쪽은 고른 키의 값이고 아래 줄은 편집기 명령이다.
//! 명령은 `redis_run` 으로 가서 읽기 전용·prod·위험 판단을 엔진이 한다.

use std::rc::Rc;

use gpui::{
    AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _, Render, StatefulInteractiveElement as _,
    Styled as _, Subscription, WeakEntity, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{ActiveTheme as _, WindowExt as _, h_flex, v_flex};
use idb_core::sqlsafe::split_args;
use idb_engine::{Engine, PatternKeys, RawValue, RedisEditResult, ResultSet, ScanResult};

use super::results::{ResultsView, Source};
use super::workspace::Workspace;
use crate::vm::redisedit;
use crate::vm::runtime::Runtime;
use crate::vm::safety::Confirms;

pub struct RedisView {
    ws: WeakEntity<Workspace>,
    rt: Rc<Runtime>,
    engine: Engine,
    conn: String,
    pub(super) pattern: Entity<InputState>,
    command: Entity<InputState>,
    keys: Vec<String>,
    /// 한 번이라도 찾았다(빈 목록과 아직 안 찾음을 가른다)
    scanned: bool,
    more: bool,
    pub(super) selected: Option<String>,
    /// 키를 고를 때마다 오른다. 값 읽기 답은 이 값과 키가 맞을 때만 받는다
    load_gen: u64,
    value: Option<Entity<ResultsView>>,
    /// 고른 키의 원값(편집에 쓴다)
    pub(super) raw: Option<RawValue>,
    _subs: Vec<Subscription>,
}

impl RedisView {
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
    pub fn new(
        ws: WeakEntity<Workspace>,
        rt: Rc<Runtime>,
        engine: Engine,
        conn: String,
        pattern: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> RedisView {
        let first = pattern.to_string();
        let pattern = cx.new(|cx| InputState::new(window, cx).placeholder("키 패턴 (예: user:*)").default_value(first));
        let command = cx.new(|cx| InputState::new(window, cx).placeholder("Redis 명령 (예: GET key) — Enter 또는 ⌘↵"));
        let subs = vec![
            cx.subscribe_in(&pattern, window, |this, _, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.scan(window, cx);
                }
            }),
            cx.subscribe_in(&command, window, |this, _, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.run_command(window, cx);
                }
            }),
        ];
        let v = RedisView {
            ws,
            rt,
            engine,
            conn,
            pattern,
            command,
            keys: Vec::new(),
            scanned: false,
            more: false,
            selected: None,
            value: None,
            raw: None,
            load_gen: 0,
            _subs: subs,
        };
        // 작업 공간의 select_conn 안에서 만들어진다. 지금 scan 하면 작업 공간을 두 번 빌려 패닉한다
        cx.defer_in(window, |v, window, cx| v.scan(window, cx));
        v
    }

    fn scan(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (conn, pat) = (self.conn.clone(), self.pattern.read(cx).value().to_string());
        let this = cx.entity().downgrade();
        self.with_ws(window, cx, move |ws, window, cx| {
            ws.guarded(
                "키 찾기",
                Rc::new(move |e: Engine, _| {
                    let (conn, pat) = (conn.clone(), pat.clone());
                    async move { e.redis_scan(&conn, &pat).await }
                }),
                Rc::new(move |ws: &mut Workspace, r: ScanResult, _, cx| {
                    ws.set_status(
                        format!("키 {}개{}", r.keys.len(), if r.more { " (더 있음)" } else { "" }),
                        false,
                        cx,
                    );
                    let _ = this.update(cx, |v, cx| {
                        v.keys = r.keys;
                        v.scanned = true;
                        v.more = r.more;
                        cx.notify();
                    });
                }),
                Confirms::default(),
                window,
                cx,
            )
        });
    }

    pub(super) fn open_key(&mut self, key: String, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(key.clone());
        self.load_gen += 1;
        self.raw = None;
        let req_gen = self.load_gen;
        let conn = self.conn.clone();
        let this = cx.entity().downgrade();
        let k2 = key.clone();
        self.with_ws(window, cx, move |ws, window, cx| {
            ws.guarded(
                "값 읽기",
                Rc::new(move |e: Engine, _| {
                    let (conn, key) = (conn.clone(), key.clone());
                    async move { e.redis_value(&conn, &key).await }
                }),
                Rc::new(move |_: &mut Workspace, rs: ResultSet, window, cx| {
                    let _ = this.update(cx, |v, cx| {
                        if redisedit::accept_response(v.load_gen, v.selected.as_deref(), req_gen, &k2) {
                            v.show(rs, window, cx)
                        }
                    });
                }),
                Confirms::default(),
                window,
                cx,
            )
        });
        self.load_raw(window, cx);
        cx.notify();
    }

    /// 편집용 원값을 읽는다(R34).
    fn load_raw(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = self.selected.clone() else { return };
        let (conn, this) = (self.conn.clone(), cx.entity().downgrade());
        self.raw = None;
        let req_gen = self.load_gen;
        self.with_ws(window, cx, move |ws, window, cx| {
            ws.guarded(
                "값 읽기",
                Rc::new(move |e: Engine, _| {
                    let (conn, key) = (conn.clone(), key.clone());
                    async move { e.redis_raw_value(&conn, &key).await }
                }),
                Rc::new(move |_: &mut Workspace, raw: RawValue, _, cx| {
                    let _ = this.update(cx, |v, cx| v.on_raw(req_gen, raw, cx));
                }),
                Confirms::default(),
                window,
                cx,
            )
        });
    }

    /// 창을 여는 지금 보이는 값의 키다(편집 창이 이 키에 묶인다).
    fn raw_key(&self) -> String {
        self.raw.as_ref().map(|r| r.key.clone()).unwrap_or_default()
    }

    /// 원값 답을 받는다. 그사이 다른 키를 골랐으면 버린다(늦게 온 A 가 B 자리에 앉지 않게).
    pub(super) fn on_raw(&mut self, resp_gen: u64, raw: RawValue, cx: &mut Context<Self>) {
        if redisedit::accept_response(self.load_gen, self.selected.as_deref(), resp_gen, &raw.key) {
            self.raw = Some(raw);
            cx.notify();
        }
    }

    // ─── 편집(R34)·일괄 삭제(R42) ───

    /// 편집 하나를 보낸다. prod·위험이면 엔진이 거절하고 확인 창을 거쳐 다시 보낸다.
    /// key 는 사용자가 창에서 본(확인한) 키다. 지금 읽어 둔 값과 고른 키가 그 키일 때만 보낸다.
    pub(super) fn apply_edit(&mut self, key: &str, action: redisedit::Action, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if let Err(msg) = redisedit::check_target(self.raw.as_ref().map(|r| r.key.as_str()), self.selected.as_deref(), key) {
            self.status(&msg, true, cx);
            return false;
        }
        let Some(raw) = &self.raw else { return false };
        let edit = match redisedit::build(raw, action) {
            Ok(e) => e,
            Err(msg) => {
                self.status(&msg, true, cx);
                return false;
            }
        };
        let (conn, this) = (self.conn.clone(), cx.entity().downgrade());
        self.with_ws(window, cx, move |ws, window, cx| {
            ws.guarded(
                "Redis 편집",
                Rc::new(move |e: Engine, c: Confirms| {
                    let (conn, edit) = (conn.clone(), edit.clone());
                    async move { e.redis_edit(&conn, edit, c.prod, c.danger).await }
                }),
                Rc::new(move |ws: &mut Workspace, r: RedisEditResult, window, cx| {
                    ws.set_status(
                        if r.status.is_empty() {
                            "고침".to_string()
                        } else {
                            r.status.clone()
                        },
                        false,
                        cx,
                    );
                    let _ = this.update(cx, |v, cx| {
                        if r.key.is_empty() {
                            // 키를 지웠다
                            v.selected = None;
                            v.raw = None;
                            v.value = None;
                            v.scan(window, cx);
                        } else {
                            if v.selected.as_deref() != Some(r.key.as_str()) {
                                v.scan(window, cx);
                            }
                            v.open_key(r.key.clone(), window, cx);
                        }
                    });
                }),
                Confirms::default(),
                window,
                cx,
            )
        });
        true
    }

    fn status(&self, msg: &str, error: bool, cx: &mut Context<Self>) {
        let (msg, ws) = (msg.to_string(), self.ws.clone());
        gpui::App::defer(cx, move |cx| {
            let _ = ws.update(cx, |ws, cx| ws.set_status(msg, error, cx));
        });
    }

    /// 입력칸 몇 개를 받는 창을 띄우고 확인하면 동작으로 바꿔 보낸다.
    fn open_form(
        &mut self,
        key: String,
        title: &str,
        fields: Vec<(&'static str, String)>,
        make: Rc<dyn Fn(Vec<String>) -> redisedit::Action>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let inputs: Vec<(&'static str, Entity<InputState>)> = fields
            .into_iter()
            .map(|(label, v)| (label, cx.new(|cx| InputState::new(window, cx).placeholder(label).default_value(v))))
            .collect();
        let (this, title) = (cx.entity().downgrade(), title.to_string());
        let first = std::cell::Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if first.replace(false)
                && let Some((_, i)) = inputs.first()
            {
                i.update(cx, |i, cx| i.focus(window, cx));
            }
            let mut d = dialog.title(format!("{title} — {key}")).w(px(480.));
            for (label, i) in &inputs {
                d = d.child(
                    h_flex()
                        .gap_2()
                        .py_1()
                        .child(div().w(px(90.)).child(*label))
                        .child(div().flex_1().child(Input::new(i))),
                );
            }
            let (this, inputs, make, key) = (this.clone(), inputs.clone(), make.clone(), key.clone());
            let ok = Rc::new(move |window: &mut Window, cx: &mut gpui::App| {
                let vals: Vec<String> = inputs.iter().map(|(_, i)| i.read(cx).value().to_string()).collect();
                let action = make(vals);
                let _ = this.update(cx, |v, cx| v.apply_edit(&key, action, window, cx));
            });
            let ok2 = ok.clone();
            d.child(
                h_flex()
                    .pt_3()
                    .gap_2()
                    .child(div().flex_1())
                    .child(
                        Button::new("redis-form-cancel")
                            .label("취소")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(Button::new("redis-form-ok").primary().label("확인").on_click(move |_, window, cx| {
                        ok2(window, cx);
                        window.close_dialog(cx);
                    })),
            )
            .on_ok(move |_, window, cx| {
                ok(window, cx);
                true
            })
        });
    }

    fn confirm_delete_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 창에 보이는 키 = 읽어 둔 값의 키 = 보낼 키
        let Some(key) = self.raw.as_ref().map(|r| r.key.clone()) else {
            return;
        };
        let this = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (this, key2) = (this.clone(), key.clone());
            alert
                .title("키 지우기")
                .description(format!("{key} 를 지울까?"))
                .ok_text("지우기")
                .ok_variant(gpui_component::button::ButtonVariant::Danger)
                .show_cancel(true)
                .cancel_text(crate::vm::copy::CANCEL)
                .on_ok(move |_, window, cx| {
                    let k = key2.clone();
                    let _ = this.update(cx, |v, cx| v.apply_edit(&k, redisedit::Action::DeleteKey, window, cx));
                    true
                })
        });
    }

    /// R42: 패턴에 맞는 키를 먼저 세어 보이고, 확인하면 지운다. prod 면 엔진이 한 번 더 묻는다.
    pub(super) fn pattern_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pat = self.pattern.read(cx).value().trim().to_string();
        if pat.is_empty() || pat == "*" {
            self.status("패턴을 좁힌다 (전체 * 는 지우지 않는다)", true, cx);
            return;
        }
        let (conn, this) = (self.conn.clone(), cx.entity().downgrade());
        self.with_ws(window, cx, move |ws, window, cx| {
            let p2 = pat.clone();
            ws.guarded(
                "지울 키 세기",
                Rc::new(move |e: Engine, _| {
                    let (conn, pat) = (conn.clone(), p2.clone());
                    async move { e.redis_pattern_keys(&conn, &pat).await }
                }),
                Rc::new(move |ws: &mut Workspace, pk: PatternKeys, window, cx| {
                    if pk.keys.is_empty() {
                        ws.set_status(format!("{pat} 에 맞는 키가 없다"), false, cx);
                        return;
                    }
                    if pk.truncated {
                        ws.set_status(redisedit::UNLINK_OVER_LIMIT, true, cx);
                        return;
                    }
                    let q = redisedit::unlink_question(&pat, pk.keys.len(), pk.truncated);
                    let (this, keys) = (this.clone(), pk.keys);
                    window.open_alert_dialog(cx, move |alert, _, _| {
                        let (this, keys) = (this.clone(), keys.clone());
                        alert
                            .title("패턴으로 지우기")
                            .description(q.clone())
                            .ok_text(format!("{}개 지우기", keys.len()))
                            .ok_variant(gpui_component::button::ButtonVariant::Danger)
                            .show_cancel(true)
                            .cancel_text(crate::vm::copy::CANCEL)
                            .on_ok(move |_, window, cx| {
                                let keys = keys.clone();
                                let _ = this.update(cx, |v, cx| v.unlink(keys, window, cx));
                                true
                            })
                    });
                }),
                Confirms::default(),
                window,
                cx,
            )
        });
    }

    fn unlink(&mut self, keys: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        let (conn, this) = (self.conn.clone(), cx.entity().downgrade());
        self.with_ws(window, cx, move |ws, window, cx| {
            ws.guarded(
                "키 지우기",
                // 사용자가 개수를 보고 확인했다(confirm). prod 는 엔진이 confirm_prod 로 따로 묻는다
                Rc::new(move |e: Engine, c: Confirms| {
                    let (conn, keys) = (conn.clone(), keys.clone());
                    async move { e.redis_unlink(&conn, keys, true, c.prod).await }
                }),
                Rc::new(move |ws: &mut Workspace, n: idb_engine::Count, window, cx| {
                    ws.set_status(format!("키 {}개를 지웠다", n.rows), false, cx);
                    let _ = this.update(cx, |v, cx| {
                        v.selected = None;
                        v.raw = None;
                        v.value = None;
                        v.scan(window, cx);
                    });
                }),
                Confirms::default(),
                window,
                cx,
            )
        });
    }

    fn render_edit_bar(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let raw = self.raw.as_ref()?;
        let theme = cx.theme().clone();
        let typ = raw.typ.clone();
        let btn = |id: &'static str, label: &'static str| Button::new(id).ghost().label(label);
        let mut bar = h_flex()
            .gap_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(theme.border)
            .child(div().truncate().child(raw.key.clone()))
            .child(div().px_2().rounded_full().bg(theme.secondary).text_xs().child(typ.clone()))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!("TTL {}", redisedit::ttl_label(raw.ttl_secs))),
            )
            .child(div().flex_1());
        use redisedit::Action as A;
        let cur = raw.string.clone();
        bar = match typ.as_str() {
            "string" => bar.child(btn("r-edit", "값 고치기").on_click(cx.listener(move |v, _, w, cx| {
                v.open_form(
                    v.raw_key(),
                    "값 고치기",
                    vec![("값", cur.clone())],
                    Rc::new(|x| A::SetString(x[0].clone())),
                    w,
                    cx,
                )
            }))),
            "hash" => bar
                .child(btn("r-hset", "필드 설정").on_click(cx.listener(|v, _, w, cx| {
                    v.open_form(
                        v.raw_key(),
                        "필드 설정",
                        vec![("필드", String::new()), ("값", String::new())],
                        Rc::new(|x| A::HashSet {
                            field: x[0].clone(),
                            value: x[1].clone(),
                        }),
                        w,
                        cx,
                    )
                })))
                .child(btn("r-hdel", "필드 지우기").on_click(cx.listener(|v, _, w, cx| {
                    v.open_form(
                        v.raw_key(),
                        "필드 지우기",
                        vec![("필드", String::new())],
                        Rc::new(|x| A::HashDel(x[0].clone())),
                        w,
                        cx,
                    )
                }))),
            "list" => bar
                .child(btn("r-lpush", "끝에 추가").on_click(cx.listener(|v, _, w, cx| {
                    v.open_form(
                        v.raw_key(),
                        "끝에 추가",
                        vec![("값", String::new())],
                        Rc::new(|x| A::ListPush(x[0].clone())),
                        w,
                        cx,
                    )
                })))
                .child(btn("r-lset", "칸 고치기").on_click(cx.listener(|v, _, w, cx| {
                    v.open_form(
                        v.raw_key(),
                        "칸 고치기",
                        vec![("번호(0부터)", String::new()), ("값", String::new())],
                        Rc::new(|x| A::ListSet {
                            index: x[0].trim().parse().unwrap_or(usize::MAX),
                            value: x[1].clone(),
                        }),
                        w,
                        cx,
                    )
                })))
                .child(btn("r-ldel", "칸 지우기").on_click(cx.listener(|v, _, w, cx| {
                    v.open_form(
                        v.raw_key(),
                        "칸 지우기",
                        vec![("번호(0부터)", String::new())],
                        Rc::new(|x| A::ListDel(x[0].trim().parse().unwrap_or(usize::MAX))),
                        w,
                        cx,
                    )
                }))),
            "set" => bar
                .child(btn("r-sadd", "멤버 추가").on_click(cx.listener(|v, _, w, cx| {
                    v.open_form(
                        v.raw_key(),
                        "멤버 추가",
                        vec![("멤버", String::new())],
                        Rc::new(|x| A::SetAdd(x[0].clone())),
                        w,
                        cx,
                    )
                })))
                .child(btn("r-srem", "멤버 지우기").on_click(cx.listener(|v, _, w, cx| {
                    v.open_form(
                        v.raw_key(),
                        "멤버 지우기",
                        vec![("멤버", String::new())],
                        Rc::new(|x| A::SetDel(x[0].clone())),
                        w,
                        cx,
                    )
                }))),
            "zset" => bar
                .child(btn("r-zadd", "점수 설정").on_click(cx.listener(|v, _, w, cx| {
                    v.open_form(
                        v.raw_key(),
                        "점수 설정",
                        vec![("멤버", String::new()), ("점수", String::new())],
                        Rc::new(|x| A::ZSet {
                            member: x[0].clone(),
                            score: x[1].clone(),
                        }),
                        w,
                        cx,
                    )
                })))
                .child(btn("r-zrem", "멤버 지우기").on_click(cx.listener(|v, _, w, cx| {
                    v.open_form(
                        v.raw_key(),
                        "멤버 지우기",
                        vec![("멤버", String::new())],
                        Rc::new(|x| A::ZDel(x[0].clone())),
                        w,
                        cx,
                    )
                }))),
            _ => bar,
        };
        let ttl = if raw.ttl_secs > 0 {
            raw.ttl_secs.to_string()
        } else {
            String::new()
        };
        let key = raw.key.clone();
        Some(
            bar.child(btn("r-ttl", "TTL").on_click(cx.listener(move |v, _, w, cx| {
                v.open_form(
                    v.raw_key(),
                    "TTL (초, 비우면 만료 없음)",
                    vec![("초", ttl.clone())],
                    Rc::new(|x| A::Ttl(x[0].clone())),
                    w,
                    cx,
                )
            })))
            .child(btn("r-rename", "이름 바꾸기").on_click(cx.listener(move |v, _, w, cx| {
                v.open_form(
                    v.raw_key(),
                    "이름 바꾸기",
                    vec![("새 이름", key.clone())],
                    Rc::new(|x| A::Rename(x[0].clone())),
                    w,
                    cx,
                )
            })))
            .child(
                Button::new("r-del")
                    .danger()
                    .label("키 지우기")
                    .on_click(cx.listener(|v, _, w, cx| v.confirm_delete_key(w, cx))),
            )
            .into_any_element(),
        )
    }

    fn show(&mut self, rs: ResultSet, window: &mut Window, cx: &mut Context<Self>) {
        let (ws, rt, engine, conn) = (self.ws.clone(), self.rt.clone(), self.engine.clone(), self.conn.clone());
        self.value = Some(cx.new(|cx| {
            ResultsView::new(
                ws,
                rt,
                engine,
                conn,
                Source::Static,
                rs,
                false,
                crate::vm::settings::DEFAULT_ROW_LIMIT,
                window,
                cx,
            )
        }));
        cx.notify();
    }

    /// 명령 줄을 보낸다. prod 쓰기·위험 명령이면 엔진이 거절하고 확인 창을 거쳐 다시 보낸다.
    pub fn run_command(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let args = split_args(&self.command.read(cx).value());
        if args.is_empty() {
            return;
        }
        let conn = self.conn.clone();
        let this = cx.entity().downgrade();
        self.with_ws(window, cx, move |ws, window, cx| {
            ws.guarded(
                "Redis 명령",
                Rc::new(move |e: Engine, c: Confirms| {
                    let (conn, args) = (conn.clone(), args.clone());
                    async move { e.redis_run(&conn, args, c.prod, c.danger).await }
                }),
                Rc::new(move |ws: &mut Workspace, rs: ResultSet, window, cx| {
                    ws.set_status(format!("{}ms", rs.elapsed_ms), false, cx);
                    let _ = this.update(cx, |v, cx| v.show(rs, window, cx));
                }),
                Confirms::default(),
                window,
                cx,
            )
        });
    }
}

impl Render for RedisView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let sc = super::kit::scale(&theme);
        let edit_bar = self.render_edit_bar(cx);
        let mut list = v_flex().id("redis-keys").flex_1().min_h_0().overflow_y_scroll();
        for (i, k) in self.keys.iter().enumerate() {
            let key = k.clone();
            let selected = self.selected.as_deref() == Some(k.as_str());
            list = list.child(
                div()
                    .id(("key", i))
                    .px_2()
                    .min_h(px(sc.row()))
                    .py(px(sc.s4()))
                    .rounded(theme.radius)
                    .truncate()
                    .cursor_pointer()
                    .when(selected, |d| d.bg(theme.list_active))
                    .hover(|s| s.bg(theme.list_hover))
                    .child(k.clone())
                    .on_click(cx.listener(move |v, _, w, cx| v.open_key(key.clone(), w, cx))),
            );
        }
        if self.scanned && self.keys.is_empty() {
            list = list.child(
                div()
                    .px_2()
                    .py(px(sc.s8()))
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(crate::vm::copy::EMPTY_KEYS),
            );
        }
        if self.more {
            list = list.child(
                div()
                    .px_2()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("1,000개까지 보인다. 패턴을 좁힌다"),
            );
        }
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        v_flex()
                            .w(px(300.))
                            .h_full()
                            .border_r_1()
                            .border_color(theme.border)
                            .child(
                                h_flex()
                                    .p_1()
                                    .gap_1()
                                    .child(div().flex_1().child(Input::new(&self.pattern)))
                                    .child(Button::new("scan").label("찾기").on_click(cx.listener(|v, _, w, cx| v.scan(w, cx))))
                                    .child(
                                        Button::new("pattern-del")
                                            .ghost()
                                            .label("지우기…")
                                            .tooltip("패턴에 맞는 키를 세어 보이고 확인하면 지운다")
                                            .on_click(cx.listener(|v, _, w, cx| v.pattern_delete(w, cx))),
                                    ),
                            )
                            .child(list),
                    )
                    .child(
                        v_flex().flex_1().min_w_0().h_full().children(edit_bar).child(match &self.value {
                            Some(v) => v.clone().into_any_element(),
                            None => div()
                                .p_3()
                                .text_color(theme.muted_foreground)
                                .child(crate::vm::copy::EMPTY_VALUE)
                                .into_any_element(),
                        }),
                    ),
            )
            .child(
                h_flex()
                    .p_1()
                    .gap_1()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(div().flex_1().child(Input::new(&self.command)))
                    .child(
                        Button::new("redis-run")
                            .primary()
                            .label("보내기")
                            .on_click(cx.listener(|v, _, w, cx| v.run_command(w, cx))),
                    ),
            )
    }
}
