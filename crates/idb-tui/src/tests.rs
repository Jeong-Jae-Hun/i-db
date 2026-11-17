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
    assert!(lines[31].ends_with("Enter 펼치기 · / 거르기 · ? 키"), "{}", lines[31]);
    select_conn(&mut a, "app-stg");
    let s = screen(&mut a);
    assert!(s.lines().last().unwrap().contains("Enter 펼치기 · A/L/S 운영 · B DB"), "{s}");
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
    let s = screen(&mut a);
    assert!(s.lines().last().unwrap().contains(" prod(shop-prod)에 쓰기 실행: y/N"), "{s}");
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
    let Overlay::Confirm { q, .. } = &a.overlay else {
        panic!("확인 창이 아님")
    };
    assert_eq!(q, "prod(shop-prod)에 쓰기 실행 · 위험: WHERE 없는 DELETE — 실행할까요?");
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
