//! 화면 시험. Engine 대신 손으로 답을 넣고 ratatui TestBackend 로 그린 글을 본다(Go ui_*_test.go 의 뜻을 옮김).

use idb_engine::{ConnSummary, EngineError, ProjectInfo};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};

use crate::app::{App, Overlay};
use crate::help::Focus;
use crate::state::SessionCtx;

fn conn(name: &str, kind: &str, env: &str, folder: &str) -> ConnSummary {
    ConnSummary {
        name: name.into(),
        group: folder.into(),
        folder: folder.into(),
        kind: kind.into(),
        env: env.into(),
        host: "h".into(),
        db: "d".into(),
        source: "datagrip".into(),
        uuid: format!("uuid-{name}"),
        ..ConnSummary::default()
    }
}

fn app() -> App {
    let info = ProjectInfo {
        dir: "/tmp/proj".into(),
        conns: vec![
            conn("app-stg", "postgresql", "stg", "app"),
            conn("shop-prod", "postgresql", "prod", "app"),
            conn("cache", "redis", "local", "app"),
        ],
        warnings: vec![],
    };
    let mut a = App::new(info, None, false, SessionCtx::disabled());
    a.w = 120;
    a.h = 32;
    a
}

fn screen(a: &mut App) -> String {
    let mut t = Terminal::new(TestBackend::new(a.w, a.h)).unwrap();
    t.draw(|f| a.draw(f)).unwrap();
    let buf = t.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..buf.area.height {
        let mut line = String::new();
        let mut skip = 0;
        for x in 0..buf.area.width {
            // 넓은 글자 뒤 칸은 그 글자가 덮는다
            if skip > 0 {
                skip -= 1;
                continue;
            }
            let sym = buf[(x, y)].symbol();
            skip = unicode_width::UnicodeWidthStr::width(sym).saturating_sub(1);
            line.push_str(sym);
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// 쌓인 요청을 꺼내 op·필드 JSON 으로 돌려준다.
fn jobs(a: &mut App) -> Vec<(u64, Value)> {
    a.take_jobs()
        .into_iter()
        .map(|j| (j.id, serde_json::to_value(&j.req).unwrap()))
        .collect()
}

fn keys(a: &mut App, ks: &[&str]) {
    for k in ks {
        a.on_key(k);
    }
}

fn typed(a: &mut App, s: &str) {
    for c in s.chars() {
        a.on_key(&if c == ' ' { "space".to_string() } else { c.to_string() });
    }
}

fn rs(cols: &[&str], rows: &[&[&str]]) -> Value {
    json!({"cols": cols, "types": cols.iter().map(|_| "text").collect::<Vec<_>>(),
        "rows": rows.iter().map(|r| r.to_vec()).collect::<Vec<_>>(), "text": "", "truncated": false, "elapsed_ms": 3})
}

fn refused(reason: &str, msg: &str) -> EngineError {
    EngineError::refused(reason, msg)
}

/// 트리에서 접속 줄을 고른다.
fn select_conn(a: &mut App, name: &str) {
    let i = a.conns.iter().position(|c| c.name == name).unwrap();
    let n = a.tree.conn_node(i).unwrap();
    a.tree.select(n);
}

#[test]
fn 화면_틀은_헤더_트리_탭_편집기_결과_상태_줄이다() {
    let mut a = app();
    let s = screen(&mut a);
    let lines: Vec<&str> = s.lines().collect();
    assert!(lines[0].starts_with(" db  접속 없음 · 트리에서 고르세요"), "{}", lines[0]);
    assert!(lines[0].ends_with("proj"));
    assert!(lines[1].contains("Explorer") && lines[1].contains(" Query 1 "), "{}", lines[1]);
    assert!(s.contains("▾ app") && s.contains("◆ app-stg") && s.contains("◈ cache"), "{s}");
    assert!(s.contains("Result"));
    assert!(s.contains("결과가 여기에 나옵니다"));
    // 트리 포커스의 줄 종류별 키 안내(R74.1)
    assert!(lines[31].ends_with("Enter 펼치기 · N 새 접속 · / 거르기 · ? 키"), "{}", lines[31]);
    select_conn(&mut a, "app-stg");
    let s = screen(&mut a);
    assert!(
        s.lines().last().unwrap().contains("Enter 펼치기 · N 새 접속 · A/L/S 운영 · B DB"),
        "{s}"
    );
}

#[test]
fn 작은_터미널은_크기_안내만_보인다() {
    let mut a = app();
    a.w = 50;
    a.h = 10;
    assert!(screen(&mut a).starts_with("터미널이 너무 작습니다 (50x10). 최소 60x14."));
}

#[test]
fn 접속을_펼치면_시스템_스키마를_빼고_테이블은_테이블_보기로_연다() {
    let mut a = app();
    select_conn(&mut a, "app-stg");
    a.on_key("enter");
    let js = jobs(&mut a);
    let (sid, _) = js.iter().find(|(_, r)| r["op"] == "schemas").cloned().unwrap();
    assert!(js.iter().any(|(_, r)| r["op"] == "catalog"), "자동완성 목록도 읽는다");
    assert_eq!(a.cur().conn, Some(0), "접속을 고르면 탭 접속이 된다");
    a.on_done(sid, Ok(json!(["public", "pg_catalog", "information_schema"])));
    let s = screen(&mut a);
    assert!(s.contains("▣ public") && !s.contains("pg_catalog"), "{s}");
    keys(&mut a, &["j", "enter"]);
    let (tid, req) = jobs(&mut a).pop().unwrap();
    assert_eq!(req["op"], "tables");
    assert_eq!(req["schema"], "public");
    a.on_done(tid, Ok(json!(["orders"])));
    keys(&mut a, &["j", "enter"]);
    let (pid, req) = jobs(&mut a).pop().unwrap();
    assert_eq!(req["op"], "table_page");
    assert_eq!(
        (req["table"].as_str(), req["offset"].as_u64(), req["estimate"].as_bool()),
        (Some("orders"), Some(0), Some(true))
    );
    assert_eq!(a.focus, Focus::Result);
    a.on_done(
        pid,
        Ok(json!({"result": rs(&["id", "name"], &[&["1", "a"], &["2", "NULL"]]), "sql": "SELECT …", "estimate": 2})),
    );
    let s = screen(&mut a);
    assert!(s.contains("WHERE (없음)   ORDER BY (없음) · rows 1–2 of 2 · public.orders"), "{s}");
    assert!(s.contains("id") && s.contains("NULL"));
    assert!(s.contains("[ ] 페이지 · w WHERE · O 정렬식"), "테이블 보기 키 안내가 앞이다\n{s}");
}

#[test]
fn 테이블_보기_열_머리_정렬과_WHERE_는_서버로_다시_읽는다() {
    let mut a = app();
    a.open_table(
        0,
        idb_engine::TableRef {
            schema: "public".into(),
            table: "t".into(),
        },
        String::new(),
    );
    let (pid, _) = jobs(&mut a).into_iter().find(|(_, r)| r["op"] == "table_page").unwrap();
    a.on_done(pid, Ok(json!({"result": rs(&["id", "status"], &[&["1", "A"]]), "sql": ""})));
    keys(&mut a, &["l", "s"]);
    let (_, req) = jobs(&mut a).pop().unwrap();
    assert_eq!(req["order_by"], "\"status\" ASC");
    a.pending.clear();
    a.cur_mut().running = None;
    keys(&mut a, &["w"]);
    typed(&mut a, "status=A");
    a.on_key("enter");
    let (_, req) = jobs(&mut a).pop().unwrap();
    assert_eq!(req["where"], "\"status\"::text = 'A'");
}

#[test]
fn 실행은_커서_문장_하나만_보낸다() {
    let mut a = app();
    a.set_conn(0, Some(0));
    a.take_jobs();
    a.set_focus(Focus::Editor);
    typed(&mut a, "select 1; select 2");
    a.on_key("ctrl+r");
    let (id, req) = jobs(&mut a).pop().unwrap();
    assert_eq!(
        (req["op"].as_str(), req["sql"].as_str(), req["conn"].as_str()),
        (Some("run"), Some("select 2"), Some("app-stg"))
    );
    a.on_done(id, Ok(rs(&["?column?"], &[&["2"]])));
    assert_eq!(a.cur().summary, "1 rows · 3ms");
    assert!(screen(&mut a).contains("?column?"));
}

#[test]
fn prod_쓰기는_확인을_받아_confirm_prod_로_다시_보낸다() {
    let mut a = app();
    a.set_conn(0, Some(1));
    a.take_jobs();
    a.set_focus(Focus::Editor);
    typed(&mut a, "update t set a = 1 where id = 1");
    a.on_key("ctrl+r");
    let (id, req) = jobs(&mut a).pop().unwrap();
    assert_eq!(req["confirm_prod"], false);
    a.on_done(id, Err(refused("confirm_prod", "prod(shop-prod)에 쓰기 실행: y/N")));
    // UPDATE 하나라 예상 행 수(R39)를 먼저 묻는다(실행하지 않음)
    let (eid, ereq) = jobs(&mut a).pop().unwrap();
    assert_eq!(
        (ereq["op"].as_str(), ereq["sql"].as_str()),
        (Some("estimate_rows"), Some("update t set a = 1 where id = 1"))
    );
    assert!(a.overlay.is_none());
    a.on_done(eid, Ok(json!({"rows": 120000})));
    let s = screen(&mut a);
    assert!(
        s.lines()
            .last()
            .unwrap()
            .contains(" prod(shop-prod)에 쓰기 실행 · 약 120,000행 (추정): y/N"),
        "{s}"
    );
    a.on_key("y");
    let (_, req) = jobs(&mut a).pop().unwrap();
    assert_eq!(
        (req["confirm_prod"].as_bool(), req["confirm_danger"].as_bool()),
        (Some(true), Some(false))
    );
    // 그 밖의 키는 취소다
    let mut b = app();
    b.set_conn(0, Some(1));
    b.take_jobs();
    b.set_focus(Focus::Editor);
    typed(&mut b, "update t set a = 1 where id = 1");
    b.on_key("ctrl+r");
    let (id, _) = jobs(&mut b).pop().unwrap();
    b.on_done(id, Err(refused("confirm_prod", "prod(shop-prod)에 쓰기 실행: y/N")));
    // 추정이 실패하면 빼고 묻는다
    let (eid, _) = jobs(&mut b).pop().unwrap();
    b.on_done(eid, Err(EngineError::run("x")));
    assert!(matches!(&b.overlay, Overlay::Confirm { q, .. } if q == "prod(shop-prod)에 쓰기 실행"));
    b.on_key("n");
    assert!(jobs(&mut b).is_empty() && b.overlay.is_none());
}

#[test]
fn prod_와_위험_문장이_겹치면_한_창에_이유를_같이_묻는다() {
    let mut a = app();
    a.set_conn(0, Some(1));
    a.take_jobs();
    a.set_focus(Focus::Editor);
    typed(&mut a, "delete from customers");
    a.on_key("ctrl+r");
    let (id, _) = jobs(&mut a).pop().unwrap();
    a.on_done(id, Err(refused("confirm_prod", "prod(shop-prod)에 쓰기 실행: y/N")));
    let (eid, _) = jobs(&mut a).pop().unwrap();
    a.on_done(eid, Ok(json!({"rows": 3})));
    let Overlay::Confirm { q, .. } = &a.overlay else {
        panic!("확인 창이 아님")
    };
    assert_eq!(
        q,
        "prod(shop-prod)에 쓰기 실행 · 위험: WHERE 없는 DELETE · 약 3행 (추정) — 실행할까요?"
    );
    a.on_key("y");
    let (_, req) = jobs(&mut a).pop().unwrap();
    assert_eq!(
        (req["confirm_prod"].as_bool(), req["confirm_danger"].as_bool()),
        (Some(true), Some(true))
    );
}

#[test]
fn 잠긴_prod_는_확인_없이_거절을_보인다() {
    let mut a = app();
    a.set_conn(0, Some(1));
    a.take_jobs();
    a.set_focus(Focus::Editor);
    typed(&mut a, "update t set a = 1 where id = 1");
    a.on_key("ctrl+r");
    let (id, _) = jobs(&mut a).pop().unwrap();
    a.on_done(id, Err(refused("prod_locked", "prod(shop-prod) 쓰기 잠김 — 10분 열기")));
    assert!(a.overlay.is_none());
    assert!(screen(&mut a).contains("prod(shop-prod) 쓰기 잠김 — 10분 열기"));
    // Ctrl+L 은 잠금을 연다
    a.on_key("ctrl+l");
    let (_, req) = jobs(&mut a).pop().unwrap();
    assert_eq!((req["op"].as_str(), req["unlock"].as_bool()), (Some("prod_lock"), Some(true)));
}

#[test]
fn 실행_중_Ctrl_C_는_취소이고_아니면_종료다() {
    let mut a = app();
    a.set_conn(0, Some(0));
    a.take_jobs();
    a.set_focus(Focus::Editor);
    typed(&mut a, "update t set a = 1 where id = 1");
    a.on_key("ctrl+r");
    let (id, _) = jobs(&mut a).pop().unwrap();
    a.on_key("ctrl+c");
    assert_eq!(a.take_cancels(), vec![id]);
    assert_eq!(a.status, "취소 요청함 (쓰기는 이미 반영됐을 수 있음)");
    assert!(!a.quit);
    a.on_key("ctrl+c");
    assert!(a.quit);
}

#[test]
fn 열린_트랜잭션은_종료_전에_묻고_끝낸_뒤에만_나간다() {
    let mut a = app();
    a.set_conn(0, Some(0));
    a.take_jobs();
    a.set_focus(Focus::Editor);
    typed(&mut a, "begin");
    a.on_key("ctrl+r");
    let (id, _) = jobs(&mut a).pop().unwrap();
    let mut res = json!({"cols": [], "rows": [], "text": "BEGIN", "truncated": false, "elapsed_ms": 1, "in_tx": true});
    a.on_done(id, Ok(res.take()));
    assert!(screen(&mut a).lines().next().unwrap().contains("Auto · tx 00:0"));
    a.request_quit();
    assert!(
        screen(&mut a)
            .lines()
            .last()
            .unwrap()
            .contains("열린 트랜잭션 app-stg: c=Commit r=Rollback esc=취소")
    );
    a.on_key("r");
    assert!(!a.quit);
    let (id, req) = jobs(&mut a).pop().unwrap();
    assert_eq!(req["op"], "tx_rollback");
    a.on_done(id, Ok(json!({"in_tx": false, "text": "ROLLBACK"})));
    assert!(a.quit);
}

#[test]
fn 고친_칸은_미리_보기를_보이고_y_면_apply_changes_로_낸다() {
    let mut a = app();
    a.open_table(
        0,
        idb_engine::TableRef {
            schema: "public".into(),
            table: "t".into(),
        },
        String::new(),
    );
    let (pid, _) = jobs(&mut a).into_iter().find(|(_, r)| r["op"] == "table_page").unwrap();
    let mut page = rs(&["id", "name"], &[&["1", "a"]]);
    page["origin"] = json!({"table": {"schema": "public", "table": "t"}, "pk": ["id"], "cols": ["id", "name"]});
    a.on_done(pid, Ok(json!({"result": page, "sql": ""})));
    assert!(
        screen(&mut a).contains("a 행 추가 · Enter 고치기 · d 삭제"),
        "고칠 수 있는 결과 안내"
    );
    keys(&mut a, &["l", "enter"]);
    keys(&mut a, &["backspace", "z", "enter"]);
    assert!(screen(&mut a).contains("변경 1건 · Ctrl+S 제출"));
    a.on_key("ctrl+s");
    let s = screen(&mut a);
    assert!(s.contains("UPDATE \"public\".\"t\" SET \"name\" = 'z' WHERE \"id\" = '1'"), "{s}");
    assert!(s.lines().last().unwrap().contains(" 1건 제출: y/N"));
    a.on_key("y");
    let (_, req) = jobs(&mut a).pop().unwrap();
    assert_eq!(req["op"], "apply_changes");
    assert_eq!(req["changes"]["changes"][0]["values"]["name"], "z");
    assert_eq!(req["changes"]["changes"][0]["old"]["name"], "a");
}

#[test]
fn 경계를_끌면_트리_폭이_바뀌고_놓을_때_저장_대상이_된다() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    let mut a = app();
    let ev = |kind, column| MouseEvent {
        kind,
        column,
        row: 5,
        modifiers: KeyModifiers::NONE,
    };
    let border = a.layout().tree_w + 1;
    a.on_mouse(ev(MouseEventKind::Down(MouseButton::Left), border));
    a.on_mouse(ev(MouseEventKind::Drag(MouseButton::Left), 41));
    assert!(screen(&mut a).contains('┃'), "끄는 동안 경계를 굵게 그린다");
    a.on_mouse(ev(MouseEventKind::Up(MouseButton::Left), 41));
    assert_eq!(a.layout().tree_w, 40);
    assert!(a.panes_dirty);
    // 20칸 밑으로는 줄지 않는다
    a.on_mouse(ev(MouseEventKind::Down(MouseButton::Left), 41));
    a.on_mouse(ev(MouseEventKind::Drag(MouseButton::Left), 3));
    assert_eq!(a.layout().tree_w, 20);
}

#[test]
fn 트리_포커스의_Ctrl_방향키는_영역_크기다() {
    let mut a = app();
    let (w, h) = (a.layout().tree_w, a.layout().ed_h);
    keys(&mut a, &["ctrl+right", "ctrl+down"]);
    assert_eq!((a.layout().tree_w, a.layout().ed_h), (w + 2, h + 2));
    a.set_focus(Focus::Editor);
    a.on_key("ctrl+right");
    assert_eq!(a.layout().tree_w, w + 2, "편집기에서는 단어 이동이다");
}

#[test]
fn 팔레트로_동작을_고르면_그_키를_누른_것과_같다() {
    let mut a = app();
    a.on_key("ctrl+p");
    // R40.1 예: ctrl+t 로 걸러 Enter → 새 탭
    typed(&mut a, "ctrl+t");
    a.on_key("enter");
    assert_eq!(a.tabs.len(), 2);
    assert_eq!(a.focus, Focus::Editor);
}

#[test]
fn 맥_글자로_키_안내를_보인다() {
    let mut a = app();
    a.mac = true;
    a.set_focus(Focus::Editor);
    let s = screen(&mut a);
    assert!(s.lines().last().unwrap().contains("⌘R 실행 · ⌘E 실행 계획 · ⌥⌘L 정렬"), "{s}");
}

#[test]
fn 물음표는_이_화면의_키를_보이고_esc_로_닫는다() {
    let mut a = app();
    a.on_key("?");
    let s = screen(&mut a);
    assert!(s.contains("이 화면의 키") && s.contains("트리") && s.contains("esc·? 닫기"), "{s}");
    a.on_key("f1");
    assert!(matches!(a.overlay, Overlay::Keys(_)), "다른 키는 아무것도 하지 않는다");
    a.on_key("esc");
    assert!(a.overlay.is_none());
}

#[test]
fn 레디스_키는_이름공간_폴더로_보이고_Enter_는_값_편집기다() {
    let mut a = app();
    select_conn(&mut a, "cache");
    a.on_key("enter");
    let (id, req) = jobs(&mut a).pop().unwrap();
    assert_eq!((req["op"].as_str(), req["pattern"].as_str()), (Some("redis_scan"), Some("*")));
    a.on_done(id, Ok(json!({"keys": ["user:1", "user:2", "solo"], "more": false})));
    let s = screen(&mut a);
    assert!(s.contains("▤ user (2)") && s.contains("◇ solo"), "{s}");
    assert!(a.cur().editor.redis, "Redis 탭은 Redis 색칠");
    keys(&mut a, &["j", "j", "enter"]);
    let (id, req) = jobs(&mut a).pop().unwrap();
    assert_eq!((req["op"].as_str(), req["key"].as_str()), (Some("redis_raw_value"), Some("solo")));
    a.on_done(id, Ok(json!({"key": "solo", "type": "string", "ttl_secs": -1, "string": "hello"})));
    let s = screen(&mut a);
    assert!(s.contains("solo · string · TTL -1") && s.contains("hello"), "{s}");
    assert!(s.contains("e 고치기 · t TTL"), "{s}");
    keys(&mut a, &["e", "end"]);
    typed(&mut a, "!");
    a.on_key("enter");
    let (_, req) = jobs(&mut a).pop().unwrap();
    assert_eq!(req["op"], "redis_edit");
    assert_eq!(
        (req["edit"]["op"].as_str(), req["edit"]["text"].as_str()),
        (Some("edit"), Some("hello!"))
    );
}

#[test]
fn 파라미터_자리는_값을_묻고_바인딩해_보낸다() {
    let mut a = app();
    a.set_conn(0, Some(0));
    a.take_jobs();
    a.set_focus(Focus::Editor);
    typed(&mut a, "select * from t where id = :id");
    a.on_key("ctrl+r");
    assert!(matches!(a.overlay, Overlay::Prompt { .. }));
    typed(&mut a, "7");
    a.on_key("enter");
    let (_, req) = jobs(&mut a).pop().unwrap();
    assert_eq!(req["params"]["id"], "7");
    assert_eq!(req["sql"], "select * from t where id = :id");
}

#[test]
fn 여러_문장은_차례로_돌고_첫_오류에서_멈춘다() {
    let mut a = app();
    a.set_conn(0, Some(0));
    a.take_jobs();
    a.set_focus(Focus::Editor);
    typed(&mut a, "select 1; select nope; select 3");
    a.on_key("ctrl+a");
    a.on_key("ctrl+r");
    let (id, req) = jobs(&mut a).pop().unwrap();
    assert_eq!(req["sql"], "select 1");
    a.on_done(id, Ok(rs(&["a"], &[&["1"]])));
    let (id, req) = jobs(&mut a).pop().unwrap();
    assert_eq!(req["sql"], "select nope");
    a.on_done(id, Err(EngineError::run("column \"nope\" does not exist")));
    assert!(jobs(&mut a).is_empty());
    assert!(screen(&mut a).contains("statement 2 of 3: column \"nope\" does not exist"));
}

#[test]
fn 일반_결과의_정렬_거르기_더_읽기는_감싼_SQL_로_서버에서_읽는다() {
    let mut a = app();
    a.set_conn(0, Some(0));
    a.take_jobs();
    a.set_focus(Focus::Editor);
    typed(&mut a, "select a from t");
    a.on_key("ctrl+r");
    let (id, _) = jobs(&mut a).pop().unwrap();
    let mut r = rs(&["a"], &[&["2"], &["1"]]);
    r["truncated"] = json!(true);
    a.on_done(id, Ok(r));
    a.set_focus(Focus::Result);
    a.on_key("s");
    let (id, req) = jobs(&mut a).pop().unwrap();
    assert_eq!(
        req["sql"],
        "SELECT * FROM (\nselect a from t\n) AS _q ORDER BY \"a\" ASC NULLS LAST"
    );
    a.on_done(id, Ok(rs(&["a"], &[&["1"], &["2"]])));
    keys(&mut a, &["/", "x", "enter"]);
    let (id, req) = jobs(&mut a).pop().unwrap();
    assert_eq!(
        req["sql"],
        "SELECT * FROM (\nselect a from t\n) AS _q WHERE _q::text ILIKE '%x%' ORDER BY \"a\" ASC NULLS LAST"
    );
    let mut r = rs(&["a"], &[&["1x"]]);
    r["truncated"] = json!(true);
    a.on_done(id, Ok(r));
    a.on_key("]");
    let (id, req) = jobs(&mut a).pop().unwrap();
    assert!(req["sql"].as_str().unwrap().ends_with("NULLS LAST OFFSET 1"), "{req}");
    a.on_done(id, Ok(rs(&["a"], &[&["2x"]])));
    assert_eq!(a.cur().grid.as_ref().unwrap().res.rows.len(), 2);
    assert_eq!(a.cur().summary, "2행");
    // 다시 읽기가 실패하면 보던 결과를 둔다
    a.on_key("s");
    let (id, _) = jobs(&mut a).pop().unwrap();
    a.on_done(id, Err(EngineError::run("boom")));
    assert_eq!(a.status, "다시 읽지 못함: boom");
    assert!(a.cur().grid.is_some());
}

#[test]
fn 쓰기_결과는_받은_행만_정렬하고_까닭을_알린다() {
    let mut a = app();
    a.set_conn(0, Some(0));
    a.take_jobs();
    a.set_focus(Focus::Editor);
    typed(&mut a, "insert into t values (1) returning a");
    a.on_key("ctrl+r");
    let (id, _) = jobs(&mut a).pop().unwrap();
    a.on_done(id, Ok(rs(&["a"], &[&["1"]])));
    a.set_focus(Focus::Result);
    a.on_key("s");
    assert!(jobs(&mut a).is_empty());
    assert_eq!(a.status, "받은 행만 정렬: 쓰기 문장");
}

#[test]
fn 패턴_일괄_삭제는_세고_미리_보인_뒤_500개씩_지운다() {
    let mut a = app();
    select_conn(&mut a, "cache");
    a.on_key("X");
    typed(&mut a, ":x");
    a.on_key("enter");
    let (id, req) = jobs(&mut a).pop().unwrap();
    assert_eq!(
        (req["op"].as_str(), req["pattern"].as_str()),
        (Some("redis_pattern_keys"), Some("*:x"))
    );
    let keys: Vec<String> = (0..600).map(|i| format!("k{i}:x")).collect();
    a.on_done(id, Ok(json!({"keys": keys, "truncated": false})));
    let s = screen(&mut a);
    assert!(s.contains("미리 보기 21줄") && s.contains("k0:x"), "{s}");
    assert!(s.lines().last().unwrap().contains("cache에서 키 600개 지움: y/N"));
    a.on_key("y");
    let (id, req) = jobs(&mut a).pop().unwrap();
    assert_eq!(
        (req["op"].as_str(), req["keys"].as_array().unwrap().len(), req["confirm"].as_bool()),
        (Some("redis_unlink"), 500, Some(true))
    );
    a.on_key("q");
    assert!(!a.quit && a.status == "키를 지우는 중 — esc 로 멈춘 뒤 종료");
    a.on_key("esc");
    a.on_done(id, Ok(json!({"rows": 500})));
    assert_eq!(a.status, "멈춤: 500개 지움");
    assert!(a.deleting.is_none());
    // 100,000개를 넘으면 묻지 않는다
    a.on_key("X");
    a.on_key("enter");
    let (id, _) = jobs(&mut a).into_iter().find(|(_, r)| r["op"] == "redis_pattern_keys").unwrap();
    a.on_done(id, Ok(json!({"keys": ["a"], "truncated": true})));
    assert!(a.overlay.is_none());
    assert_eq!(a.status, "키가 100,000개를 넘어 지우지 않음 (패턴을 좁히세요)");
}

#[test]
fn 복사_꼴_고르기는_대상과_꼴을_고르고_마지막_꼴을_기억한다() {
    let mut a = app();
    a.open_table(
        0,
        idb_engine::TableRef {
            schema: "public".into(),
            table: "t".into(),
        },
        String::new(),
    );
    let (pid, _) = jobs(&mut a).into_iter().find(|(_, r)| r["op"] == "table_page").unwrap();
    let mut page = rs(&["id", "name"], &[&["1", "a|b"], &["2", "c"]]);
    page["types"] = json!(["int4", "text"]);
    page["origin"] = json!({"table": {"schema": "public", "table": "t"}, "pk": ["id"], "cols": ["id", "name"]});
    a.on_done(pid, Ok(json!({"result": page, "sql": ""})));
    a.on_key("ctrl+k");
    let s = screen(&mut a);
    assert!(
        s.contains("현재 행 · TSV (헤더)") && s.contains("현재 열 · IN 목록") && !s.contains("현재 열 · SQL INSERT"),
        "{s}"
    );
    typed(&mut a, "보이는 markdown");
    a.on_key("enter");
    assert_eq!(a.clips.pop().unwrap(), "| id | name |\n| ---: | --- |\n| 1 | a\\|b |\n| 2 | c |");
    assert_eq!(a.status, "복사함 (4줄) · 보이는 행 전체 · Markdown 표");
    a.on_key("ctrl+k");
    let Overlay::Picker { items, sel, .. } = &a.overlay else { panic!() };
    assert_eq!(items[*sel], "현재 행 · Markdown 표", "기본 대상의 마지막 꼴에 커서");
    a.on_key("esc");
    a.on_key("ctrl+k");
    typed(&mut a, "보이는 insert");
    a.on_key("enter");
    assert_eq!(
        a.clips.pop().unwrap(),
        "INSERT INTO \"public\".\"t\" (\"id\", \"name\") VALUES ('1', 'a|b'), ('2', 'c');"
    );
}

#[test]
fn JSON_칸_값은_나무로_보이고_경로를_복사한다() {
    let mut a = app();
    a.set_conn(0, Some(0));
    a.take_jobs();
    a.set_focus(Focus::Editor);
    typed(&mut a, "select doc from t");
    a.on_key("ctrl+r");
    let (id, _) = jobs(&mut a).pop().unwrap();
    let mut r = rs(&["doc"], &[&[r#"{"items":[{"name":"x"}]}"#]]);
    r["types"] = json!(["jsonb"]);
    a.on_done(id, Ok(r));
    a.set_focus(Focus::Result);
    a.on_key("o");
    let s = screen(&mut a);
    assert!(s.contains("▾ $: {1}") && s.contains("▾ items: [1]"), "{s}");
    assert!(s.lines().last().unwrap().contains("Enter 접기·펴기 · y 값 복사 · Y 경로 복사"));
    keys(&mut a, &["j", "j", "j", "Y"]);
    assert_eq!(a.clips.pop().unwrap(), "$.items[0].name");
    a.on_key("t");
    assert!(screen(&mut a).contains("\"items\": ["), "t 는 원문");
}

// ─── 읽기 전용 저장소 (R60~R68) ───

fn src_app() -> App {
    let info = ProjectInfo {
        dir: "/tmp/proj".into(),
        conns: vec![
            conn("app-stg", "postgresql", "stg", "app"),
            conn("logs-stg", "loki", "stg", "obs"),
            conn("search-stg", "opensearch", "stg", "obs"),
            conn("ddb-stg", "dynamodb", "stg", "obs"),
            conn("infra-stg", "aws", "stg", "obs"),
            conn("cache", "redis", "local", "app"),
        ],
        warnings: vec![],
    };
    let mut a = App::new(info, None, false, SessionCtx::disabled());
    a.w = 120;
    a.h = 32;
    a
}

fn snode(name: &str, path: &str, info: &str, kids: bool, open: bool) -> Value {
    json!({"name": name, "path": path, "info": info, "has_children": kids, "openable": open, "warn": false})
}

/// 접속 줄을 펼쳐 맨 위 자식을 답으로 넣는다.
fn expand_source(a: &mut App, name: &str, kids: Value) {
    select_conn(a, name);
    a.on_key("enter");
    let (id, req) = jobs(a).into_iter().find(|(_, r)| r["op"] == "source_children").unwrap();
    assert_eq!((req["conn"].as_str(), req["path"].as_str()), (Some(name), Some("")));
    a.on_done(id, Ok(kids));
}

#[test]
fn 저장소_접속은_source_children_으로_펼치고_Enter_는_미리_보기다() {
    let mut a = src_app();
    expand_source(&mut a, "logs-stg", json!([snode("app", "app", "라벨", true, false)]));
    assert_eq!(a.cur().conn, Some(1), "펼친 저장소 접속이 탭 접속이 된다");
    a.on_key("j");
    let s = screen(&mut a);
    assert!(s.contains("app 라벨"), "오른쪽 흐린 정보\n{s}");
    assert!(s.lines().last().unwrap().ends_with("l 펼치기 · r 새로고침 · ? 키"), "{s}");
    a.on_key("l");
    let (id, req) = jobs(&mut a).pop().unwrap();
    assert_eq!((req["op"].as_str(), req["path"].as_str()), (Some("source_children"), Some("app")));
    a.on_done(id, Ok(json!([snode("api", "app/api", "", false, true)])));
    a.on_key("j");
    assert!(
        screen(&mut a)
            .lines()
            .last()
            .unwrap()
            .ends_with("Enter 미리 보기 · r 새로고침 · ? 키")
    );
    a.on_key("enter");
    let (id, req) = jobs(&mut a).pop().unwrap();
    assert_eq!(
        (req["op"].as_str(), req["conn"].as_str(), req["path"].as_str()),
        (Some("source_open"), Some("logs-stg"), Some("app/api"))
    );
    assert!(a.cur().running.is_some());
    a.on_done(id, Ok(rs(&["time", "line"], &[&["2024-01-01 09:00:00", "hello"]])));
    let s = screen(&mut a);
    assert!(s.contains("hello"), "{s}");
    assert_eq!(a.focus, Focus::Result);
    assert_eq!(a.cur().base_why, "이 저장소는 서버 정렬 없음");
}

#[test]
fn 빈_저장소_목록은_비어_있음_한_줄이고_읽기_실패는_접어_둔다() {
    let mut a = src_app();
    expand_source(&mut a, "search-stg", json!([]));
    assert!(screen(&mut a).contains("(비어 있음)"));
    let mut b = src_app();
    select_conn(&mut b, "search-stg");
    b.on_key("enter");
    let (id, _) = jobs(&mut b).pop().unwrap();
    b.on_done(id, Err(EngineError::run("connection refused")));
    assert_eq!(b.status, "읽기 실패: connection refused");
    let n = b.tree.current().unwrap();
    assert!(!b.tree.node(n).expanded, "다시 펼치면 다시 읽는다");
}

#[test]
fn 다이나모_테이블_미리_보기는_비싼_질의_확인_뒤_force_로_스캔한다() {
    let mut a = src_app();
    expand_source(&mut a, "ddb-stg", json!([snode("users", "users", "항목 ≈12", true, true)]));
    keys(&mut a, &["j", "enter"]);
    let (id, req) = jobs(&mut a).pop().unwrap();
    assert_eq!(req["op"], "source_open");
    a.on_done(id, Err(refused("confirm_expensive", "비싼 질의: 전체 스캔")));
    let Overlay::Confirm { q, .. } = &a.overlay else {
        panic!("확인 창이 아님")
    };
    assert_eq!(q, "비싼 질의: 전체 스캔");
    assert!(screen(&mut a).lines().last().unwrap().contains("비싼 질의: 전체 스캔: y/N"));
    a.on_key("y");
    let (id, req) = jobs(&mut a).pop().unwrap();
    assert_eq!(
        (
            req["op"].as_str(),
            req["text"].as_str(),
            req["force"].as_bool(),
            req["limit"].as_u64()
        ),
        (Some("source_run"), Some("SELECT * FROM \"users\""), Some(true), Some(50))
    );
    a.on_done(id, Ok(rs(&["pk"], &[&["u1"]])));
    assert!(screen(&mut a).contains("u1"));
}

#[test]
fn 다시_보낼_질의가_없는_비싼_미리_보기는_알리기만_한다() {
    let mut a = src_app();
    expand_source(&mut a, "search-stg", json!([snode("logs", "logs", "문서 12", true, true)]));
    keys(&mut a, &["j", "enter"]);
    let (id, _) = jobs(&mut a).pop().unwrap();
    a.on_done(id, Err(refused("confirm_expensive", "비싼 질의: size 1만 초과")));
    assert!(a.overlay.is_none());
    assert_eq!(a.status, "비싼 질의라 보내지 않음 (size 1만 초과)");
}

#[test]
fn 편집기_실행은_탭_범위를_싣고_Ctrl_대괄호로_범위를_돈다() {
    let mut a = src_app();
    a.set_conn(0, Some(1));
    a.set_focus(Focus::Editor);
    assert!(screen(&mut a).contains("최근 1h · Ctrl+]"), "편집기 위 줄에 범위");
    a.on_key("ctrl+]");
    assert_eq!(a.status, "범위: 최근 6h");
    assert!(screen(&mut a).contains("최근 6h · Ctrl+]"));
    typed(&mut a, "{app=\"api\"}");
    a.on_key("ctrl+r");
    let (id, req) = jobs(&mut a).pop().unwrap();
    assert_eq!(
        (req["op"].as_str(), req["conn"].as_str(), req["text"].as_str()),
        (Some("source_run"), Some("logs-stg"), Some("{app=\"api\"}"))
    );
    assert_eq!(req["since_ns"].as_i64(), Some(6 * 3_600_000_000_000));
    assert_eq!(req["force"], false);
    a.on_done(id, Err(refused("confirm_expensive", "비싼 질의: 범위 7일 초과")));
    a.on_key("y");
    let (_, req) = jobs(&mut a).pop().unwrap();
    assert_eq!(
        (req["force"].as_bool(), req["since_ns"].as_i64()),
        (Some(true), Some(6 * 3_600_000_000_000))
    );
    // 범위 없는 종류는 범위를 싣지 않는다
    let mut b = src_app();
    b.set_conn(0, Some(2));
    b.set_focus(Focus::Editor);
    assert!(!screen(&mut b).contains("최근 1h"));
    b.on_key("ctrl+]");
    assert_eq!(b.status, "범위는 Loki·Prometheus·Grafana 탭에서만 바꿈");
    typed(&mut b, "GET logs/_search");
    b.on_key("ctrl+r");
    let (_, req) = jobs(&mut b).pop().unwrap();
    assert_eq!(req["since_ns"], 0);
}

#[test]
fn 읽기_전용_요금_거절은_상태_줄에만_보이고_aws_는_보내지_않는다() {
    for (reason, msg) in [
        ("read_only", "읽기 전용 저장소라 보내지 않음"),
        ("billed", "요금이 붙는 조회라 막음 (DB_ALLOW_BILLED=1 로 허용)"),
    ] {
        let mut a = src_app();
        a.set_conn(0, Some(3));
        a.set_focus(Focus::Editor);
        typed(&mut a, "SELECT * FROM \"users\"");
        a.on_key("ctrl+r");
        let (id, _) = jobs(&mut a).pop().unwrap();
        a.on_done(id, Err(refused(reason, msg)));
        assert_eq!(a.status, msg);
        assert!(a.overlay.is_none() && jobs(&mut a).is_empty());
        assert!(a.cur().running.is_none() && a.cur().err.is_empty());
    }
    let mut a = src_app();
    a.set_conn(0, Some(4));
    a.set_focus(Focus::Editor);
    typed(&mut a, "x");
    a.on_key("ctrl+r");
    assert_eq!(a.status, "aws 접속은 트리로 본다");
    assert!(jobs(&mut a).is_empty());
}

// ─── 대시보드 (R64) ───

#[test]
fn 대시보드는_모든_접속을_트리_순서로_확인하고_요약과_표를_그린다() {
    let mut a = app();
    a.on_key("ctrl+g");
    assert_eq!(a.cur().title, "대시보드");
    assert_eq!(a.status, "대시보드 확인 중…");
    let mut js = jobs(&mut a);
    let (ob, _) = js.pop().unwrap();
    assert_eq!(js.len(), 3);
    let names: Vec<&str> = js.iter().map(|(_, r)| r["conn"].as_str().unwrap()).collect();
    assert!(js.iter().all(|(_, r)| r["op"] == "health"));
    // 아웃박스 한도 소진 카드는 세었을 때만 보인다(R64)
    a.on_done(
        ob,
        Ok(json!([{"flow": "hub", "conn": "app-stg", "exhausted": 2}, {"flow": "x", "conn": "app-stg", "exhausted": -1}])),
    );
    assert!(screen(&mut a).contains("아웃박스 한도 소진"));
    assert_eq!(names, ["cache", "app-stg", "shop-prod"], "트리 순서(환경 local→stg→prod)");
    let s = screen(&mut a);
    assert!(s.contains("확인 중 3"), "{s}");
    a.on_done(
        js[1].0,
        Ok(json!({"ok": true, "latency_ms": 12, "version": "16.2", "summary": "",
            "metrics": [{"name": "세션", "value": "12/200", "warn": false}, {"name": "활성", "value": "3", "warn": false}], "err": ""})),
    );
    a.on_done(js[0].0, Err(EngineError::run("dial tcp: refused")));
    let s = screen(&mut a);
    assert!(s.contains("PG 활성 세션") && s.contains("● 1") && s.contains("✕ 1"), "{s}");
    assert!(
        s.contains("세션 12/200 · 활성 3") && s.contains("12ms") && s.contains("16.2"),
        "{s}"
    );
    assert!(s.contains("dial tcp: refused"), "{s}");
    assert!(s.contains("확인 중 1"), "{s}");
    assert!(
        s.lines()
            .last()
            .unwrap()
            .ends_with("r 새로고침 · Enter 트리에서 고르기 · / 거르기 · ? 키 · Ctrl+W 닫기"),
        "{s}"
    );
    a.on_done(
        js[2].0,
        Ok(json!({"ok": true, "latency_ms": 1, "version": "", "summary": "", "metrics": [], "err": ""})),
    );
    assert!(a.status.starts_with("대시보드 확인함 "), "{}", a.status);
    // ? 는 대시보드 묶음만 보인다
    a.on_key("?");
    let Overlay::Keys(secs) = &a.overlay else {
        panic!("키 창이 아님")
    };
    assert_eq!(secs.len(), 1);
    assert_eq!(crate::help::SECTIONS[secs[0]].title, "대시보드 (Ctrl+G)");
}

#[test]
fn 대시보드_r_과_30초_타이머는_이전_확인을_취소하고_다시_본다() {
    let mut a = app();
    a.on_key("ctrl+g");
    let first: Vec<u64> = jobs(&mut a).into_iter().map(|(id, _)| id).collect();
    a.on_key("r");
    let cancels = a.take_cancels();
    assert_eq!(cancels.len(), 4, "진행 중인 확인(접속 3·아웃박스 1)을 멈춘다");
    assert!(first.iter().all(|id| cancels.contains(id)));
    let second = jobs(&mut a);
    assert_eq!(second.len(), 4);
    // 늦게 온 이전 결과는 버린다
    a.on_done(
        first[0],
        Ok(json!({"ok": true, "latency_ms": 1, "version": "", "summary": "", "metrics": [], "err": ""})),
    );
    assert!(screen(&mut a).contains("● 0"));
    // 때가 되면 활성 탭일 때만 다시 본다
    if let Some(crate::app::Pane::Dash(d)) = &mut a.cur_mut().pane {
        d.due = std::time::Instant::now() - std::time::Duration::from_secs(1);
    }
    a.tick();
    assert_eq!(jobs(&mut a).len(), 4);
    let dash = a.tab;
    a.new_tab();
    a.take_jobs();
    if let Some(crate::app::Pane::Dash(d)) = &mut a.tabs[dash].pane {
        d.due = std::time::Instant::now() - std::time::Duration::from_secs(1);
    }
    a.tick();
    assert!(jobs(&mut a).is_empty(), "다른 탭이 활성이면 다음 때만 미룬다");
    let Some(crate::app::Pane::Dash(d)) = &a.tabs[dash].pane else {
        panic!()
    };
    assert!(d.due > std::time::Instant::now());
    // 탭을 닫으면 진행 중인 확인을 멈춘다
    a.take_cancels();
    a.close_tab(dash);
    assert_eq!(a.take_cancels().len(), 4);
    assert!(a.pending.is_empty());
}

#[test]
fn 대시보드_Enter_는_트리에서_접속을_고르고_거르기는_그룹_이름_환경이다() {
    let mut a = app();
    a.on_key("ctrl+g");
    a.take_jobs();
    a.on_key("/");
    typed(&mut a, "prod");
    a.on_key("enter");
    let s = screen(&mut a);
    assert!(
        s.contains("거르기 prod · esc 해제") && s.contains("shop-prod") && !s.contains("▸ app-stg"),
        "{s}"
    );
    a.on_key("enter");
    assert_eq!(a.focus, Focus::Tree);
    let n = a.tree.current().unwrap();
    assert_eq!(a.tree.node(n).kind, crate::tree::NodeKind::Conn(1));
}

// ─── 흐름 (R69) ───

fn flow_answer() -> Value {
    json!([
        {"flow": "hub", "boxes": [
            {"stage": "outbox", "name": "app-stg public.t_outbox", "metrics": "대기 120 · 재시도 3", "warn": true, "idle": false, "err": "", "note": "", "conn": "app-stg"},
            {"stage": "sns", "name": "hub", "metrics": "", "warn": false, "idle": false, "err": "", "note": "", "conn": "infra-stg"},
            {"stage": "sqs", "name": "q1", "metrics": "", "warn": false, "idle": false, "err": "", "note": "", "conn": "infra-stg"},
            {"stage": "lambda", "name": "worker", "metrics": "Enabled · OK", "warn": false, "idle": false, "err": "", "note": "", "conn": "infra-stg"}
        ]},
        {"flow": "broken", "boxes": [
            {"stage": "config", "name": "broken", "metrics": "", "warn": false, "idle": false, "err": "설정 오류: no queues", "note": "", "conn": ""}
        ]}
    ])
}

#[test]
fn 흐름_탭은_모든_흐름을_한_번에_풀고_목록과_그림을_그린다() {
    let mut a = src_app();
    a.w = 200;
    a.on_key("alt+f");
    let (id, req) = jobs(&mut a).pop().unwrap();
    assert_eq!(req, json!({"op": "flows", "names": [], "prod": true}));
    assert!(screen(&mut a).contains("흐름 확인 중…"));
    a.on_done(id, Ok(flow_answer()));
    let s = screen(&mut a);
    assert!(s.contains("▸ ! hub") && s.contains("✕ broken"), "{s}");
    assert!(s.contains("수치는 요금이 붙어 보지 않음"), "{s}");
    assert!(
        s.contains("[아웃박스 app-stg public.t_outbox | 대기 120 · 재시도 3] ──▶ [SNS hub] ──▶ [SQS q1] ──▶ [λ worker OK]"),
        "{s}"
    );
    assert!(
        s.lines()
            .last()
            .unwrap()
            .ends_with("r 새로고침 · ↑↓ 흐름 · ←→ 상자 · Enter 상세 · ? 키 · Ctrl+W 닫기")
    );
    assert!(a.status.starts_with("흐름 확인함 "));
    // Lambda 상자 Enter 는 aws 매핑 표다
    keys(&mut a, &["l", "l", "l", "enter"]);
    let (_, req) = jobs(&mut a).pop().unwrap();
    assert_eq!(
        (req["op"].as_str(), req["conn"].as_str(), req["path"].as_str()),
        (Some("source_open"), Some("infra-stg"), Some("lambda/worker"))
    );
    // 이름만 아는 토픽은 상세가 없다(설정을 읽을 로컬 파일 없음)
    a.switch_tab(a.tabs.len() - 2);
    keys(&mut a, &["h", "h", "enter"]);
    assert_eq!(a.status, "열 상세가 없음: hub");
    a.remote = true;
    keys(&mut a, &["h", "enter"]);
    assert_eq!(a.status, "원격 접속에서는 아웃박스 상세를 열지 않음");
}

#[test]
fn 흐름_파일_오류와_빈_목록은_안내를_보인다() {
    let mut a = src_app();
    a.on_key("alt+f");
    let (id, _) = jobs(&mut a).pop().unwrap();
    a.on_done(id, Ok(json!([])));
    assert!(screen(&mut a).contains("흐름 없음 · .dbtui/flows.json 을 쓰세요"));
    a.on_key("r");
    let (id, _) = jobs(&mut a).pop().unwrap();
    a.on_done(id, Err(EngineError::run(".dbtui/flows.json: invalid character")));
    assert!(screen(&mut a).contains("설정 오류: .dbtui/flows.json: invalid character"));
}

#[test]
fn aws_트리_SNS_토픽_F_는_그_토픽만의_흐름을_더해_고른다() {
    let mut a = src_app();
    expand_source(&mut a, "infra-stg", json!([snode("SNS 토픽", "sns", "", true, false)]));
    keys(&mut a, &["j", "l"]);
    let (id, _) = jobs(&mut a).pop().unwrap();
    let arn = "arn:aws:sns:ap-northeast-2:1:hub";
    a.on_done(id, Ok(json!([snode("hub", &format!("sns/{arn}"), "구독 2", true, true)])));
    a.on_key("j");
    assert!(
        screen(&mut a)
            .lines()
            .last()
            .unwrap()
            .ends_with("F 흐름 · Enter 미리 보기 · l 펼치기 · r 새로고침 · ? 키")
    );
    a.on_key("F");
    let (id, req) = jobs(&mut a).pop().unwrap();
    assert_eq!(req["op"], "flows");
    a.on_done(id, Ok(json!([])));
    let s = screen(&mut a);
    assert!(s.contains("▸ ● hub · infra-stg") && s.contains("[SNS hub]"), "{s}");
    a.on_key("enter");
    let (_, req) = jobs(&mut a).pop().unwrap();
    assert_eq!(
        (req["op"].as_str(), req["path"].as_str()),
        (Some("source_open"), Some(&*format!("sns/{arn}")))
    );
}

// ─── 보드 (R70) ───

fn board_answer() -> Value {
    json!({"title": "운영", "panels": [
        {"title": "대기", "conn": "app-stg", "flow": "", "view": "number", "prod": false, "value": "42", "warn": true},
        {"title": "최근", "conn": "shop-prod", "flow": "", "view": "table", "prod": true, "warn": false,
            "result": rs(&["id", "state"], &[&["1", "ok"]])},
        {"title": "틀림", "conn": "x", "flow": "", "view": "number", "prod": false, "warn": false, "error": "설정 오류: no such connection: x"},
        {"title": "흐름", "conn": "", "flow": "hub", "view": "flow", "prod": false, "warn": false,
            "boxes": [{"stage": "sns", "name": "hub", "metrics": "", "warn": false, "idle": false, "err": "", "note": "", "conn": "infra"}]}
    ]})
}

#[test]
fn 보드는_목록에서_골라_한_회차를_돌리고_패널_격자를_그린다() {
    let mut a = app();
    a.on_key("alt+b");
    let (id, req) = jobs(&mut a).pop().unwrap();
    assert_eq!(req["op"], "board_list");
    a.on_done(id, Ok(json!([])));
    assert_eq!(a.status, "보드 없음 · .dbtui/boards/<이름>.json 을 쓰세요");
    a.on_key("alt+b");
    let (id, _) = jobs(&mut a).pop().unwrap();
    a.on_done(id, Ok(json!([{"name": "ops", "title": "운영", "panels": 4}])));
    assert!(matches!(a.overlay, Overlay::Picker { .. }));
    a.on_key("enter");
    let (id, req) = jobs(&mut a).pop().unwrap();
    assert_eq!(req, json!({"op": "board_run", "name": "ops", "prod": true}));
    assert!(screen(&mut a).contains("보드 확인 중…"));
    a.on_done(id, Ok(board_answer()));
    assert_eq!(a.cur().title, "운영");
    let s = screen(&mut a);
    assert!(s.contains("보드 · 패널 4 · 1m0s마다"), "{s}");
    assert!(s.contains("대기 · app-stg") && s.contains("42"), "{s}");
    assert!(s.contains("최근 · shop-prod prod") && s.contains("state"), "{s}");
    assert!(s.contains("설정 오류: no such connection: x"), "{s}");
    assert!(s.contains("흐름 · 흐름 hub") && s.contains("[SNS hub]"), "{s}");
    assert!(
        s.lines()
            .last()
            .unwrap()
            .ends_with("r 새로고침 · 방향키·hjkl 패널 · Enter 쿼리 탭에 열기 · ? 키 · Ctrl+W 닫기")
    );
    // 120칸 오른쪽 영역은 2열이다: j 는 아래 줄, Enter 는 설정 오류면 까닭만 보인다
    keys(&mut a, &["j", "enter"]);
    assert_eq!(a.status, "설정 오류: no such connection: x");
    a.remote = true;
    keys(&mut a, &["k", "l", "enter"]);
    assert_eq!(a.status, "원격 접속에서는 패널 질의를 열지 않음");
    // 흐름 패널은 흐름 탭에서 그 흐름을 고른다
    keys(&mut a, &["j", "enter"]);
    assert!(matches!(a.cur().pane, Some(crate::app::Pane::Flow(_))));
    let (id, req) = jobs(&mut a).pop().unwrap();
    assert_eq!(req["op"], "flows");
    a.on_done(id, Ok(flow_answer()));
    let Some(crate::app::Pane::Flow(f)) = &a.cur().pane else { panic!() };
    assert_eq!(f.cursor, 0);
    // 같은 보드를 다시 열면 그 탭으로 옮겨 다시 돈다
    let n = a.tabs.len();
    a.open_board("ops");
    assert_eq!(a.tabs.len(), n);
    assert_eq!(jobs(&mut a).pop().unwrap().1["op"], "board_run");
}

#[test]
fn 로컬_보드는_파일의_refresh_와_패널_질의로_쿼리_탭을_연다() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".dbtui/boards")).unwrap();
    std::fs::write(
        dir.path().join(".dbtui/boards/ops.json"),
        r#"{"refresh":"15s","panels":[{"title":"오류","conn":"logs-stg","query":"{app=\"api\"} |= \"error\"","view":"table","since":"6h"}]}"#,
    )
    .unwrap();
    let info = ProjectInfo {
        dir: dir.path().to_string_lossy().into_owned(),
        conns: vec![conn("logs-stg", "loki", "stg", "obs")],
        warnings: vec![],
    };
    let mut a = App::new(info, None, false, SessionCtx::disabled());
    a.w = 120;
    a.h = 32;
    a.take_jobs();
    a.open_board("ops");
    let (id, _) = jobs(&mut a).pop().unwrap();
    a.on_done(
        id,
        Ok(json!({"title": "ops", "panels": [{"title": "오류", "conn": "logs-stg", "flow": "", "view": "table", "prod": false, "warn": false,
            "result": rs(&["time", "line"], &[])}]})),
    );
    assert!(screen(&mut a).contains("패널 1 · 15s마다"));
    a.on_key("enter");
    assert_eq!(a.cur().title, "오류");
    assert_eq!(a.focus, Focus::Editor);
    let (_, req) = jobs(&mut a).pop().unwrap();
    assert_eq!(
        (req["op"].as_str(), req["text"].as_str(), req["since_ns"].as_i64()),
        (Some("source_run"), Some("{app=\"api\"} |= \"error\""), Some(6 * 3_600_000_000_000))
    );
}

#[test]
fn 세션에는_대시보드_흐름_보드_탭을_남기지_않고_활성_자리를_맞춘다() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.json");
    let info = ProjectInfo {
        dir: dir.path().to_string_lossy().into_owned(),
        conns: vec![conn("app-stg", "postgresql", "stg", "app")],
        warnings: vec![],
    };
    let mut a = App::new(info, None, false, SessionCtx::at(dir.path(), path.clone()));
    a.on_key("ctrl+g");
    a.on_key("alt+f");
    a.new_tab();
    a.cur_mut().editor.set_value("select 1");
    a.save_session().unwrap();
    let st = idb_core::session::load(&path, dir.path()).unwrap().unwrap();
    let titles: Vec<&str> = st.tabs.iter().map(|t| t.title.as_str()).collect();
    assert_eq!(titles, ["Query 1", "Query 4"]);
    assert_eq!(st.active, 1);
}

#[test]
fn 원격에서도_serve_가_보낸_상세_길과_패널_질의로_연다() {
    let mut a = src_app();
    a.on_key("alt+f");
    let (id, _) = jobs(&mut a).pop().unwrap();
    let mut ans = flow_answer();
    let n = ans[0]["boxes"].as_array().unwrap().len();
    let mut opens = vec![json!({}); n];
    opens[0] = json!({"sql": "select 1 from public.t_outbox"});
    ans[0]["opens"] = json!(opens);
    a.on_done(id, Ok(ans));
    a.on_key("enter");
    let (_, req) = jobs(&mut a).pop().unwrap();
    assert_eq!(
        (req["op"].as_str(), req["sql"].as_str()),
        (Some("run"), Some("select 1 from public.t_outbox"))
    );

    let mut b = app();
    b.open_board("ops");
    let (id, _) = jobs(&mut b).pop().unwrap();
    let mut ans = board_answer();
    ans["every_secs"] = json!(30);
    ans["panels"][1]["query"] = json!("select now()");
    ans["panels"][1]["since"] = json!("1h");
    b.on_done(id, Ok(ans));
    assert!(screen(&mut b).contains("30s마다"), "{}", screen(&mut b));
    keys(&mut b, &["l", "enter"]);
    assert_eq!(b.cur().editor.value(), "select now()");
}

#[test]
fn F7_은_마지막_성공한_문장을_조각으로_받아_파일에_쓰고_esc_로_멈춘다() {
    let mut a = app();
    a.on_key("f7");
    assert_eq!(a.status, "전체 내보낼 표 결과가 없음 (성공한 SELECT 문 결과에서)");
    a.set_conn(0, Some(0));
    a.take_jobs();
    a.set_focus(Focus::Editor);
    typed(&mut a, "select a from t");
    a.on_key("ctrl+r");
    let (id, _) = jobs(&mut a).pop().unwrap();
    a.on_done(id, Ok(rs(&["a"], &[&["1"]])));
    a.on_key("f7");
    let (id, req) = jobs(&mut a).pop().unwrap();
    assert_eq!(
        (req["op"].as_str(), req["sql"].as_str()),
        (Some("export_start"), Some("select a from t"))
    );
    let path = a.exporting.as_ref().unwrap().path.clone();
    a.on_done(id, Ok(json!({"export": 7})));
    let (id, req) = jobs(&mut a).pop().unwrap();
    assert_eq!(req, json!({"op": "export_read", "export": 7}));
    // "a\n1\n" base64
    a.on_done(id, Ok(json!({"data": "YQoxCg==", "done": false, "bytes": 4})));
    assert!(a.status.starts_with("전체 내보내기 4 B 받음"));
    let (id, _) = jobs(&mut a).pop().unwrap();
    a.on_done(id, Ok(json!({"data": "", "done": true, "bytes": 4, "rows": 1})));
    assert_eq!(a.status, format!("1행 내보냄: {path}"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\n1\n");
    let _ = std::fs::remove_file(&path);
    // esc 는 멈추고 쓰던 파일을 지운다
    a.set_focus(Focus::Result);
    a.on_key("f7");
    let (id, _) = jobs(&mut a).pop().unwrap();
    let path = a.exporting.as_ref().unwrap().path.clone();
    a.on_done(id, Ok(json!({"export": 8})));
    a.take_jobs();
    a.on_key("esc");
    assert_eq!(a.status, "전체 내보내기 취소함");
    assert_eq!(jobs(&mut a).pop().unwrap().1, json!({"op": "export_cancel", "export": 8}));
    assert!(!std::path::Path::new(&path).exists());
}

#[test]
fn D_는_암호문_칸을_serve_에서_풀어_보이고_편집을_막는다() {
    let mut a = app();
    a.set_conn(0, Some(0));
    a.take_jobs();
    a.set_focus(Focus::Editor);
    typed(&mut a, "select e from t");
    a.on_key("ctrl+r");
    let (id, _) = jobs(&mut a).pop().unwrap();
    let enc = "0123456789abcdef0123456789abcdef^|^00112233445566778899aabbccddeeff";
    let mut r = rs(&["id", "e"], &[&["1", enc], &["2", "plain"]]);
    r["origin"] = json!({"table": {"schema": "public", "table": "t"}, "pk": ["id"], "cols": ["id", "e"]});
    a.on_done(id, Ok(r));
    a.set_focus(Focus::Result);
    a.on_key("D");
    let (id, req) = jobs(&mut a).pop().unwrap();
    assert_eq!((req["op"].as_str(), req["cells"].clone()), (Some("decrypt"), json!([enc])));
    a.on_done(id, Ok(json!({"values": ["secret@x"], "failed": 0})));
    assert!(screen(&mut a).contains("secret@x"));
    keys(&mut a, &["l", "y"]);
    assert_eq!(a.clips.pop().unwrap(), "secret@x", "복사는 보이는 값");
    a.on_key("enter");
    assert_eq!(a.cur().grid.as_ref().unwrap().screen, crate::grid::Screen::Value);
    a.on_key("esc");
    a.on_key("f2");
    assert_eq!(a.status, "편집할 수 없음: 복호화 보기 중 (D 로 원래 값)");
    a.on_key("D");
    assert!(!screen(&mut a).contains("secret@x"));
}

#[test]
fn I_는_CSV_머리를_열에_맞춰_미리_보이고_import_csv_로_보낸다() {
    let mut a = app();
    a.catalogs.insert(
        0,
        serde_json::from_value(json!({"schemas":[{"name":"public","tables":[{"name":"t","view":false,
            "columns":[{"name":"id","type":"int4"},{"name":"Name","type":"text"}]}]}],"functions":[]}))
        .unwrap(),
    );
    let n = a.tree.conn_node(0).unwrap();
    a.tree.set_children(
        n,
        vec![(
            crate::tree::NodeKind::Schema {
                conn: 0,
                name: "public".into(),
            },
            "public".into(),
        )],
    );
    let sn = a.tree.nodes[n].children[0];
    a.tree.set_children(
        sn,
        vec![(
            crate::tree::NodeKind::Table {
                conn: 0,
                schema: "public".into(),
                name: "t".into(),
            },
            "t".into(),
        )],
    );
    let tn = a.tree.nodes[sn].children[0];
    a.tree.select(tn);
    let dir = tempfile::tempdir().unwrap();
    let csv = dir.path().join("x.csv");
    std::fs::write(&csv, "NAME,extra,id\nalice,z,1\n,y,2\n").unwrap();
    a.on_key("I");
    a.on_key("ctrl+u");
    a.on_paste(csv.to_str().unwrap());
    a.on_key("enter");
    // 파일은 화면 밖에서 읽는다
    assert_eq!(a.status, "CSV 를 읽는 중…");
    for _ in 0..200 {
        a.poll_bg();
        if a.csv_job.is_none() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let s = screen(&mut a);
    assert!(
        s.contains("열: NAME → Name, extra (버림), id → id") && s.contains("alice | 1"),
        "{s}"
    );
    assert!(s.lines().last().unwrap().contains("\"public\".\"t\" 에 2행 가져오기: y/N"));
    a.on_key("y");
    let (id, req) = jobs(&mut a).pop().unwrap();
    assert_eq!(req["op"], "import_csv");
    assert_eq!(req["columns"], json!(["Name", "id"]));
    assert_eq!(req["rows"], json!([["alice", "1"], [null, "2"]]));
    a.on_done(id, Ok(json!({"rows": 2})));
    assert_eq!(a.status, "2행 가져옴");
}

#[test]
fn 접속_창은_URL_로_채우고_시험한_뒤_저장하고_목록을_다시_읽는다() {
    let mut a = app();
    a.on_key("N");
    assert!(screen(&mut a).contains("새 접속"));
    a.on_paste("postgres://u:pw@db.local:5432/app");
    a.on_key("tab");
    assert!(!screen(&mut a).contains("pw"), "비밀번호는 • 로만 보인다");
    a.on_key("up");
    typed(&mut a, "app-new");
    a.on_key("enter");
    let (id, req) = jobs(&mut a).pop().unwrap();
    assert_eq!(req["op"], "conn_test");
    assert_eq!(
        (req["conn"]["name"].as_str(), req["conn"]["password"].as_str()),
        (Some("app-new"), Some("pw"))
    );
    a.on_done(id, Ok(json!({"version": "PostgreSQL 16.4", "warning": ""})));
    let (id, req) = jobs(&mut a).pop().unwrap();
    assert_eq!((req["op"].as_str(), req["old_name"].as_str()), (Some("conn_save"), Some("")));
    let mut c = serde_json::to_value(conn("app-new", "postgresql", "local", "app")).unwrap();
    c["source"] = json!("db");
    a.on_done(id, Ok(json!({"conn": c, "warning": "", "env_error": ""})));
    assert!(a.conn_form.is_none() && a.status == "접속 더함: app-new");
    let (id, req) = jobs(&mut a).pop().unwrap();
    assert_eq!(req["op"], "project");
    let mut conns: Vec<Value> = a.conns.iter().map(|c| serde_json::to_value(c).unwrap()).collect();
    conns.push(c);
    a.on_done(id, Ok(json!({"dir": "/tmp/proj", "conns": conns, "warnings": []})));
    assert!(screen(&mut a).contains("app-new (db)"));
    // DataGrip 접속은 고치지 않는다
    select_conn(&mut a, "app-stg");
    a.on_key("E");
    assert_eq!(a.status, "DataGrip 접속은 DataGrip 에서 고침");
    // 원격 serve 의 거절은 그대로 보이고 저장을 묻지 않는다
    a.on_key("N");
    typed(&mut a, "x");
    a.on_key("enter");
    let (id, _) = jobs(&mut a).pop().unwrap();
    a.on_done(
        id,
        Err(EngineError::refused("remote_conn_edit", "원격 serve 에서는 접속을 고치지 않음")),
    );
    let f = a.conn_form.as_ref().unwrap();
    assert!(!f.ask_save && f.msg.starts_with("원격 serve"));
}

#[test]
fn Ctrl_E_는_JSON_계획을_나무로_보이고_t_는_원문_esc_는_닫기다() {
    let mut a = app();
    a.set_conn(0, Some(0));
    a.take_jobs();
    a.set_focus(Focus::Editor);
    typed(&mut a, "select * from orders");
    a.on_key("ctrl+e");
    let (id, req) = jobs(&mut a).pop().unwrap();
    assert_eq!(
        (req["op"].as_str(), req["format"].as_str(), req["analyze"].as_bool()),
        (Some("explain"), Some("JSON"), Some(false))
    );
    let plan = r#"[{"Plan":{"Node Type":"Seq Scan","Relation Name":"orders","Alias":"orders","Total Cost":12.5,"Plan Rows":100}}]"#;
    a.on_done(id, Ok(rs(&["QUERY PLAN"], &[&[plan]])));
    a.set_focus(Focus::Result);
    let s = screen(&mut a);
    assert!(
        s.contains("100% Seq Scan on orders") && s.contains("Enter 접기·펴기 · t JSON · esc 닫기"),
        "{s}"
    );
    a.on_key("t");
    assert!(screen(&mut a).contains("\"Node Type\": \"Seq Scan\""));
    a.on_key("esc");
    assert!(a.cur().plan.is_none());
    assert!(screen(&mut a).contains("QUERY PLAN"));
}

#[test]
fn c_는_차트_등호는_고정_결과와_비교다() {
    let mut a = app();
    a.set_conn(0, Some(0));
    a.take_jobs();
    a.set_focus(Focus::Editor);
    typed(&mut a, "select ch, n from t");
    a.on_key("ctrl+r");
    let (id, _) = jobs(&mut a).pop().unwrap();
    let mut r = rs(&["ch", "n"], &[&["web", "3"], &["app", "5"]]);
    r["types"] = json!(["text", "int8"]);
    a.on_done(id, Ok(r.clone()));
    a.set_focus(Focus::Result);
    a.on_key("c");
    let s = screen(&mut a);
    assert!(s.contains("막대 · n") && s.contains("web") && s.contains('█'), "{s}");
    a.on_key("c");
    a.on_key("=");
    assert_eq!(a.status, "비교할 고정 결과가 없음 (p 로 고정)");
    a.on_key("p");
    a.on_key("ctrl+r");
    let (id, _) = jobs(&mut a).pop().unwrap();
    r["rows"] = json!([["web", "3"], ["app", "6"], ["new", "1"]]);
    a.on_done(id, Ok(r));
    a.on_key("=");
    a.on_key("enter");
    let s = screen(&mut a);
    assert!(
        s.contains("+2 −1 ~0") && s.contains("+ new"),
        "PK 가 없으면 행 전체로 짝짓는다\n{s}"
    );
    a.on_key("esc");
    assert!(a.cur().diff.is_none());
}

#[test]
fn 연결된_곳_표의_Enter_는_그_줄의_테이블을_값_조건으로_연다() {
    let mut a = app();
    a.set_conn(0, Some(0));
    a.take_jobs();
    a.set_focus(Focus::Editor);
    typed(&mut a, "select user_id from t");
    a.on_key("ctrl+r");
    let (id, _) = jobs(&mut a).pop().unwrap();
    a.on_done(id, Ok(rs(&["user_id"], &[&["1315"]])));
    a.set_focus(Focus::Result);
    a.on_key("ctrl+b");
    assert_eq!(a.status, "연결된 곳 찾는 중: user_id = 1315");
    let (id, _) = jobs(&mut a).pop().unwrap();
    let res = rs(
        &["connection", "table", "column", "rows", "note"],
        &[&["app-stg", "public.orders", "user_id", "3", ""], &["x", "", "", "", "접속 실패"]],
    );
    a.on_done(id, Ok(json!({"result": res, "failed": false})));
    a.on_key("enter");
    let (_, req) = jobs(&mut a).into_iter().find(|(_, r)| r["op"] == "table_page").unwrap();
    assert_eq!(
        (req["table"].as_str(), req["where"].as_str()),
        (Some("orders"), Some("\"user_id\" = '1315'"))
    );
}

#[test]
fn 탭_접속을_바꾸면_이_탭만_쓰던_트랜잭션을_먼저_묻는다() {
    let mut a = app();
    a.set_conn(0, Some(0));
    a.take_jobs();
    a.in_tx.insert(0, std::time::Instant::now());
    select_conn(&mut a, "cache");
    a.on_key("enter");
    assert!(matches!(a.overlay, Overlay::Tx { .. }));
    assert_eq!(a.cur().conn, Some(0));
    a.on_key("r");
    let (id, req) = jobs(&mut a).into_iter().find(|(_, r)| r["op"] == "tx_rollback").unwrap();
    assert_eq!(req["conn"], "app-stg");
    a.on_done(id, Ok(json!({"in_tx": false, "text": "ROLLBACK"})));
    assert_eq!(a.cur().conn, Some(2));
}

#[test]
fn 남길_수_없는_친_글이_있는_탭은_닫기_전에_묻는다() {
    let mut a = app();
    a.set_conn(0, Some(2));
    a.set_focus(Focus::Editor);
    typed(&mut a, "GET k");
    a.new_tab();
    a.close_tab(0);
    assert!(matches!(&a.overlay, Overlay::Confirm { q, .. } if q == "저장하지 않은 글이 있는 탭을 닫을까요"));
    a.on_key("y");
    assert_eq!(a.tabs.len(), 1);
}

#[test]
fn 되돌리기는_데이터베이스를_바꾼_뒤의_묶음을_건너뛴다() {
    let mut a = app();
    let cs: idb_engine::ChangeSet = serde_json::from_value(json!({"table": {"schema": "public", "table": "t"}, "pk": ["id"],
        "changes": [{"op": "delete", "key": {"id": "1"}, "values": {}, "old": {}}]}))
    .unwrap();
    let ap: idb_engine::Applied = serde_json::from_value(json!({"rows": [{"id": "1", "v": "a"}]})).unwrap();
    let entry = |db: &str| crate::app::UndoEntry {
        conn: 0,
        db: db.into(),
        changes: cs.clone(),
        applied: ap.clone(),
    };
    a.tabs[0].undo = vec![entry("d"), entry("other")];
    a.set_focus(Focus::Result);
    a.on_key("ctrl+z");
    assert_eq!(a.status, "다른 데이터베이스에서 낸 묶음 1개 건너뜀");
    a.on_key("y");
    let (id, req) = jobs(&mut a).pop().unwrap();
    assert_eq!(req["op"], "undo_changes");
    a.on_done(id, Ok(json!({"result": rs(&[], &[]), "applied": {"rows": []}, "sql": []})));
    assert_eq!(a.tabs[0].undo.len(), 1);
    assert_eq!(a.tabs[0].undo[0].db, "other");
    a.on_key("ctrl+z");
    assert!(matches!(&a.overlay, Overlay::Confirm { q, .. } if q.ends_with("그 묶음 1개 버리기")));
}

#[test]
fn 칸_고치는_중_Ctrl_O_는_큰_편집기이고_JSON_열은_검사한다() {
    let mut a = app();
    a.open_table(
        0,
        idb_engine::TableRef {
            schema: "public".into(),
            table: "t".into(),
        },
        String::new(),
    );
    let (pid, _) = jobs(&mut a).into_iter().find(|(_, r)| r["op"] == "table_page").unwrap();
    let mut page = rs(&["id", "doc"], &[&["1", "{\"a\":1}"]]);
    page["types"] = json!(["int4", "jsonb"]);
    page["origin"] = json!({"table": {"schema": "public", "table": "t"}, "pk": ["id"], "cols": ["id", "doc"]});
    a.on_done(pid, Ok(json!({"result": page, "sql": ""})));
    keys(&mut a, &["l", "enter", "ctrl+o"]);
    assert!(screen(&mut a).contains("칸 값 편집 (JSON)"));
    a.on_key("f8");
    assert_eq!(a.big_edit.as_ref().unwrap().0.value(), "{\n  \"a\": 1\n}");
    // 정렬만 하고 값이 같아도 글이 다르면 바꿈이다. JSON 이 아니면 남긴다
    keys(&mut a, &["ctrl+a", "x", "ctrl+s"]);
    assert!(a.big_edit.is_some() && a.status.starts_with("JSON 이 아님"));
    keys(&mut a, &["ctrl+a"]);
    a.on_paste("[1]");
    a.on_key("ctrl+s");
    assert!(a.big_edit.is_none());
    assert_eq!(
        a.cur().grid.as_ref().unwrap().change_set().unwrap().changes[0].values["doc"],
        Some("[1]".into())
    );
}

#[test]
fn 편집기_더블클릭은_낱말을_고른다() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    let mut a = app();
    a.cur_mut().editor.set_value("select name from t");
    let x = a.layout().tree_w + 3 + 4 + 8;
    let ev = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: x,
        row: 3,
        modifiers: KeyModifiers::NONE,
    };
    a.on_mouse(ev);
    a.on_mouse(ev);
    assert_eq!(a.cur().editor.selected_text(), Some("name"));
}

#[test]
fn 거르기_입력의_위아래는_이전_거르기를_고르고_끝까지_내리면_치던_글이다() {
    let mut a = app();
    a.history_seed = vec!["status=GONE".into(), "id > 1".into()];
    a.set_conn(0, Some(0));
    a.take_jobs();
    a.set_focus(Focus::Editor);
    typed(&mut a, "update t set a=1 where id=1 returning *");
    a.on_key("ctrl+r");
    let (id, _) = jobs(&mut a).pop().unwrap();
    a.on_done(id, Ok(rs(&["status"], &[&["GONE"], &["A"]])));
    a.set_focus(Focus::Result);
    keys(&mut a, &["/", "x"]);
    assert!(screen(&mut a).contains("↑↓ 이전 거르기"));
    let filt = |a: &App| a.cur().grid.as_ref().unwrap().input.text.clone();
    a.on_key("up");
    assert_eq!(filt(&a), "status=GONE");
    a.on_key("up");
    assert_eq!(filt(&a), "id > 1");
    a.on_key("up");
    assert_eq!(filt(&a), "id > 1");
    a.on_key("down");
    a.on_key("down");
    assert_eq!(filt(&a), "x");
}

#[test]
fn 처음_보는_화면의_팁은_한_번만_보이고_다음_키에서_사라진다() {
    let mut a = app();
    a.session.tips_on = true;
    a.set_conn(0, Some(0));
    a.take_jobs();
    a.set_focus(Focus::Editor);
    typed(&mut a, "select 1");
    a.on_key("ctrl+r");
    let (id, _) = jobs(&mut a).pop().unwrap();
    a.on_done(id, Ok(rs(&["a"], &[&["1"]])));
    a.set_focus(Focus::Result);
    a.w = 160;
    assert!(screen(&mut a).contains("팁: ? 로 이 화면의 키를 볼 수 있음"));
    a.on_key("j");
    assert!(!screen(&mut a).contains("팁:"));
    assert_eq!(a.session.tips_shown, ["readonly"]);
}

#[test]
fn 트리_펼침_기억은_Go_와_같은_id_로_남긴다() {
    let mut a = app();
    select_conn(&mut a, "app-stg");
    a.on_key("enter");
    let (sid, _) = jobs(&mut a).into_iter().find(|(_, r)| r["op"] == "schemas").unwrap();
    a.on_done(sid, Ok(json!(["public", "etc"])));
    keys(&mut a, &["j", "enter"]);
    let (open, closed) = a.tree_snapshot();
    assert_eq!(open, ["s:uuid-app-stg:public"], "펼친 스키마만, 접속 줄은 남기지 않는다");
    assert!(closed.is_empty());
    a.tree.cursor = 0;
    a.on_key("h");
    assert_eq!(a.tree_snapshot().1, ["g:app"]);
}

#[test]
fn Alt_C_는_stg_prod_를_바꾼_접속을_기본으로_두_곳을_읽기_전용으로_비교한다() {
    let mut a = app();
    a.set_conn(0, Some(0));
    a.take_jobs();
    a.set_focus(Focus::Editor);
    typed(&mut a, "delete from t");
    a.on_key("alt+c");
    assert_eq!(a.status, "읽기 문장만 비교");
    a.cur_mut().editor.set_value("select id from t");
    a.on_key("alt+c");
    let Overlay::Picker { items, sel, .. } = &a.overlay else { panic!() };
    assert_eq!(items[*sel], "shop-prod");
    a.on_key("enter");
    let js = jobs(&mut a);
    assert!(js.iter().all(|(_, r)| r["op"] == "read_query" && r["sql"] == "select id from t"));
    let (x, y) = (js[js.len() - 2].0, js[js.len() - 1].0);
    a.on_done(y, Ok(rs(&["id"], &[&["1"], &["2"]])));
    a.on_done(x, Ok(rs(&["id"], &[&["1"]])));
    assert_eq!(a.cur().summary, "app-stg 기준 shop-prod 비교 +1 −0 ~0");
    assert_eq!(a.cur().subs.len(), 2);
}

#[test]
fn 제출이_confirm_prod_로_거절되면_엔진_질문을_보이고_y_뒤에만_다시_보낸다() {
    let mut a = app();
    a.open_table(
        0,
        idb_engine::TableRef {
            schema: "public".into(),
            table: "t".into(),
        },
        String::new(),
    );
    let (pid, _) = jobs(&mut a).into_iter().find(|(_, r)| r["op"] == "table_page").unwrap();
    let mut page = rs(&["id", "name"], &[&["1", "a"]]);
    page["origin"] = json!({"table": {"schema": "public", "table": "t"}, "pk": ["id"], "cols": ["id", "name"]});
    a.on_done(pid, Ok(json!({"result": page, "sql": ""})));
    keys(&mut a, &["l", "enter", "backspace", "z", "enter", "ctrl+s", "y"]);
    let (id, req) = jobs(&mut a).pop().unwrap();
    assert_eq!(
        (req["op"].as_str(), req["confirm_prod"].as_bool()),
        (Some("apply_changes"), Some(false))
    );
    a.on_done(id, Err(refused("confirm_prod", "prod(app-stg)에 제출 1건: y/N")));
    assert!(jobs(&mut a).is_empty(), "묻지 않고 다시 보내지 않는다");
    assert!(matches!(&a.overlay, Overlay::Confirm { q, .. } if q == "prod(app-stg)에 제출 1건"));
    a.on_key("y");
    let (_, req) = jobs(&mut a).pop().unwrap();
    assert_eq!(req["confirm_prod"], true);
}

#[test]
fn 접속_URL_을_붙이면_다음_그림_전에_비밀번호를_가린_칸으로_옮긴다() {
    let mut a = app();
    a.on_key("N");
    a.on_paste("postgres://u:s3cret@db.local:5432/app");
    let s = screen(&mut a);
    assert!(!s.contains("s3cret") && s.contains("db.local") && s.contains("••••••"), "{s}");
}

#[test]
fn 원격에서는_접속_창을_열지_않고_까닭을_알린다() {
    let mut a = app();
    a.remote = true;
    a.on_key("N");
    assert!(a.conn_form.is_none());
    assert_eq!(a.status, "원격 접속에서는 접속을 고치지 않음 (그 기계에서 db conn 으로 고침)");
    assert!(jobs(&mut a).is_empty());
    assert!(!screen(&mut a).contains("N 새 접속"));
}

/// CSV 가져오기 준비 일감을 손으로 걸어 둔다(파일·스레드 없이).
fn fake_csv_job(a: &mut App, conn: usize) -> std::sync::mpsc::Sender<Result<crate::export::CsvPrep, String>> {
    let (tx, rx) = std::sync::mpsc::channel();
    let c = &a.conns[conn];
    a.csv_job = Some(crate::export::CsvJob {
        rx,
        conn_uuid: c.uuid.clone(),
        conn_name: c.name.clone(),
        table: idb_engine::TableRef {
            schema: "public".into(),
            table: "t".into(),
        },
        path: "/x.csv".into(),
    });
    tx
}

#[test]
fn CSV_를_읽는_사이_접속이_바뀌면_다른_접속에_가져오지_않는다() {
    let mut a = app();
    let tx = fake_csv_job(&mut a, 1);
    // 읽는 사이 접속 목록이 바뀌어 shop-prod 가 지워졌다
    a.conns.remove(1);
    tx.send(Ok(crate::export::CsvPrep {
        columns: vec!["id".into()],
        data: vec![vec![Some("1".into())]],
        preview: vec![],
    }))
    .unwrap();
    a.poll_bg();
    assert!(a.overlay.is_none());
    assert_eq!(a.status, "가져오기를 멈춤: 읽는 사이 접속이 바뀌거나 지워짐 (shop-prod)");
    // 자리만 옮겼으면 uuid 로 다시 찾아 그 접속에 묻는다
    let tx = fake_csv_job(&mut a, 1);
    a.conns.swap(0, 1);
    tx.send(Ok(crate::export::CsvPrep {
        columns: vec!["id".into()],
        data: vec![vec![Some("1".into())]],
        preview: vec![],
    }))
    .unwrap();
    a.poll_bg();
    assert!(matches!(&a.overlay, Overlay::Confirm { q, .. } if q.contains("에 1행 가져오기")));
}

#[test]
fn CSV_를_읽는_중에는_새_가져오기를_받지_않는다() {
    let mut a = app();
    let _tx = fake_csv_job(&mut a, 0);
    a.csv_preview(
        0,
        idb_engine::TableRef {
            schema: "public".into(),
            table: "t".into(),
        },
        vec!["id".into()],
        "/other.csv".into(),
    );
    assert_eq!(a.status, "CSV 를 읽는 중 — 끝난 뒤 다시 I");
    assert_eq!(a.csv_job.as_ref().unwrap().path, "/x.csv");
}

#[test]
fn 클립보드_복사가_실패하면_복사함_대신_까닭을_보인다() {
    let mut a = app();
    a.copy("x".into(), "칸 복사함".into());
    assert_eq!(a.status, "칸 복사함");
    a.clip_failed("no system clipboard command");
    assert_eq!(a.status, "클립보드 복사 실패: no system clipboard command");
}

#[test]
fn 새_판_알림은_상태_글_뒤에_잇는다() {
    let mut a = app();
    a.status = "탭 1개 되살림".into();
    a.update_available("v0.13.0");
    assert_eq!(a.status, "탭 1개 되살림 · 새 판 v0.13.0 — db update");
    a.status.clear();
    a.update_available("v1.0.1");
    assert_eq!(a.status, "새 판 v1.0.1 — db update");
}

// ─── 쿼리 파일·즐겨찾기 (엔진 op) ───

/// 골든 offline 프로젝트 사본을 LocalEngine 으로 열고 Files 가 그 엔진의 op 로 읽고 쓰게 한다.
fn file_app() -> (App, tempfile::TempDir) {
    fn copy(src: &std::path::Path, dst: &std::path::Path) {
        std::fs::create_dir_all(dst).unwrap();
        for e in std::fs::read_dir(src).unwrap().flatten() {
            if e.file_type().unwrap().is_dir() {
                copy(&e.path(), &dst.join(e.file_name()));
            } else {
                std::fs::copy(e.path(), dst.join(e.file_name())).unwrap();
            }
        }
    }
    let dir = tempfile::tempdir().unwrap();
    copy(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/golden/offline"),
        dir.path(),
    );
    let rt = std::sync::Arc::new(tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap());
    let proj = idb_core::project::load_project(dir.path()).unwrap();
    let engine = idb_engine::Engine::Local(std::sync::Arc::new(idb_engine::LocalEngine::new(proj, None)));
    let info = rt.block_on(engine.project()).unwrap();
    let e = engine.clone();
    let files = crate::files::Files::new(Box::new(move |req| rt.block_on(e.call(req))));
    let mut a = App::new(info, Some(files), false, SessionCtx::disabled());
    a.w = 120;
    a.h = 32;
    (a, dir)
}

#[test]
fn 쿼리_파일은_엔진으로_열고_자동_저장하고_밖에서_바뀌면_멈춘_뒤_사본을_남긴다() {
    let (mut a, dir) = file_app();
    let path = dir.path().join("queries/app/a.sql");
    let before = std::fs::read_to_string(&path).unwrap();
    a.open_query_file("queries/app/a.sql");
    assert_eq!(a.cur().editor.value(), before);
    assert_eq!(
        a.cur().conn.map(|c| a.conns[c].name.as_str()),
        Some("app-stg"),
        "매핑된 접속으로 연다"
    );

    a.cur_mut().editor.set_value("select 2;\n");
    let ti = a.tab;
    a.autosave(ti);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "select 2;\n");

    // 그 기계에서 파일을 바꾸면 자동 저장은 덮지 않고 멈춘다
    std::fs::write(&path, "select 9;\n").unwrap();
    a.cur_mut().editor.set_value("select 3;\n");
    a.autosave(ti);
    assert!(a.cur().autosave_stopped);
    assert!(a.status.starts_with("파일이 밖에서 바뀌어 자동 저장 멈춤"), "{}", a.status);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "select 9;\n");

    // 닫을 때는 충돌 사본에 남기고 원래 접속을 붙인다
    let note = a.keep_text(ti).unwrap();
    let copy = a
        .files
        .as_ref()
        .unwrap()
        .paths()
        .into_iter()
        .find(|p| p.contains("a-conflict-"))
        .unwrap();
    assert!(note.contains(&copy), "{note}");
    assert_eq!(std::fs::read_to_string(dir.path().join(&copy)).unwrap(), "select 3;\n");
    assert_eq!(
        a.files.as_ref().unwrap().mapped(&copy),
        Some(a.conns[a.cur().conn.unwrap()].uuid.as_str())
    );

    // Ctrl+S 는 덮어쓴다
    a.on_key("ctrl+s");
    assert_eq!(a.status, "저장함: queries/app/a.sql");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "select 3;\n");
    assert!(!a.cur().autosave_stopped);
}

#[test]
fn 첫_실행_저장은_엔진이_정한_길에_쓰고_트리_쿼리_묶음에_붙는다() {
    let (mut a, dir) = file_app();
    let c = a.conns.iter().position(|c| c.name == "app-stg").unwrap();
    let before = a.files.as_ref().unwrap().for_conn(&a.conns[c].uuid).len();
    let i = a.new_tab();
    a.set_conn(i, Some(c));
    a.tabs[i].editor.set_value("select 42;\n");
    a.save_new_query(i);
    let rel = a.tabs[i].file.clone().expect("파일이 붙는다");
    assert!(rel.starts_with("queries/") && rel.contains("app-stg-"), "{rel}");
    assert_eq!(a.status, format!("저장함: {rel}"));
    assert_eq!(std::fs::read_to_string(dir.path().join(&rel)).unwrap(), "select 42;\n");
    let f = a.files.as_ref().unwrap();
    assert_eq!(f.for_conn(&a.conns[c].uuid).len(), before + 1);
    assert_eq!(a.queries_for(c).unwrap().0, format!("쿼리 ({})", before + 1));
}

#[test]
fn queries_밖의_길은_화면에서도_열지_않는다() {
    let (mut a, _dir) = file_app();
    a.open_query_file("../.env");
    assert!(a.status.starts_with("열지 못함: 쿼리 파일 경로가 아님"), "{}", a.status);
    assert!(a.tabs.iter().all(|t| t.file.is_none()));
}

#[test]
fn 즐겨찾기는_엔진으로_저장하고_같은_이름은_물은_뒤_덮고_골라_연다() {
    let (mut a, _dir) = file_app();
    a.cur_mut().editor.set_value("select 1");
    a.save_favorite();
    typed(&mut a, "q1");
    a.on_key("enter");
    assert_eq!(a.status, "즐겨찾기 저장: q1");
    a.cur_mut().editor.set_value("select 2");
    a.save_favorite();
    typed(&mut a, "q1");
    a.on_key("enter");
    assert!(matches!(a.overlay, Overlay::Confirm { .. }), "같은 이름은 묻는다");
    a.on_key("y");
    assert_eq!(a.status, "즐겨찾기 저장: q1");
    a.open_favorites();
    a.on_key("enter");
    assert_eq!((a.cur().name.as_str(), a.cur().editor.value()), ("q1", "select 2"));
}
