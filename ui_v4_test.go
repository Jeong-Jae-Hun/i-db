package main

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"testing"
	"time"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/redis/go-redis/v9"

	"github.com/Jeong-Jae-Hun/i-db/internal/complete"
	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/favorites"
	"github.com/Jeong-Jae-Hun/i-db/internal/plantree"
)

// ─── R25 위험 문장 ───

func TestDanger_stg에서_WHERE_없는_DELETE는_묻고_WHERE가_있으면_묻지_않는다(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 1
	if m.run("delete from t;"); m.cur().running != nil || m.overlay != oConfirm || !strings.Contains(m.View(), "위험: WHERE 없는 DELETE — 실행할까요? y/N") {
		t.Fatalf("stg 도 WHERE 없는 DELETE 는 묻는다:\n%s", m.View())
	}
	m.Update(key("n"))
	if m.cur().running != nil || m.status != "취소함" {
		t.Fatal("N 이면 실행하지 않는다")
	}
	m.run("delete from t;")
	if _, cmd := m.Update(key("y")); cmd == nil || m.cur().running == nil {
		t.Fatal("y 면 실행한다")
	}
	if cmd := m.run("delete from t where id = 1"); cmd == nil || m.overlay != oNone {
		t.Fatal("WHERE 가 있으면 stg 는 바로 실행")
	}
}

func TestDanger_prod_쓰기와_겹치면_한_창에_이유를_같이_보이고_한_번만_묻는다(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 0
	m.run("delete from customers")
	if m.overlay != oConfirm || !strings.Contains(m.View(), "prod(x-prod)에 쓰기 실행 · 위험: WHERE 없는 DELETE — 실행할까요? y/N") {
		t.Fatalf("prod 와 위험을 한 창에:\n%s", m.View())
	}
	if _, cmd := m.Update(key("y")); cmd == nil || m.overlay != oNone || m.cur().running == nil {
		t.Fatal("y 한 번이면 실행")
	}

	addRedis(m)
	m.cur().conn = 2
	m.run("FLUSHDB")
	if m.overlay != oConfirm || !strings.Contains(m.View(), "위험: FLUSHDB — 실행할까요? y/N") {
		t.Fatalf("Redis FLUSHDB 는 묻는다:\n%s", m.View())
	}
}

// ─── R26 정의로 이동 ───

func TestDefinition_CtrlB는_테이블이면_트리에서_고르고_구조를_연다(t *testing.T) {
	m := testModel(t)
	tn := addTable(m, 1)
	m.selectNode(m.roots[0])
	tab := m.cur()
	tab.conn = 1
	tab.editor.SetValue("select * from users")
	m.setFocus(fEditor)
	m.Update(tea.KeyMsg{Type: tea.KeyEnd})
	_, cmd := m.Update(tea.KeyMsg{Type: tea.KeyCtrlB})
	if cmd == nil {
		t.Fatalf("이름을 찾는 명령: %q", m.status)
	}
	m.Update(defMsg{tab: tab.id, conn: 1, name: "users", obj: db.Object{Kind: db.ObjTable, Schema: "public", Name: "users"}})
	if m.current() != tn || tab.running == nil || tab.tv != nil || m.focus != fEditor {
		t.Fatalf("트리에서 고르고 구조를 읽는다: cur=%v running=%v focus=%v", m.current(), tab.running != nil, m.focus)
	}

	m.Update(defMsg{tab: tab.id, conn: 1, name: "nope", err: fmt.Errorf("%w: nope", db.ErrNotFound)})
	if m.status != "정의를 찾지 못함: nope" {
		t.Fatalf("못 찾음: %q", m.status)
	}

	tab.editor.SetValue("   ")
	if _, cmd := m.Update(tea.KeyMsg{Type: tea.KeyF12}); cmd != nil || m.status != "커서 아래 이름이 없음" {
		t.Fatalf("F12 도 같은 동작, 이름이 없으면 안내: %q", m.status)
	}
}

func TestDefinition_함수는_정의를_텍스트_결과로_연다(t *testing.T) {
	m := testModel(t)
	tab := m.cur()
	tab.conn = 1
	_, cmd := m.Update(defMsg{tab: tab.id, conn: 1, name: "f", obj: db.Object{Kind: db.ObjFunction, Schema: "public", Name: "f"}})
	if cmd == nil || tab.running == nil {
		t.Fatal("함수 정의를 읽는다")
	}
	def := "CREATE OR REPLACE FUNCTION public.f()\n RETURNS integer\n"
	m.Update(resultMsg{tab: tab.id, seq: tab.seq, conn: 1, res: &db.Result{Text: def}, rev: tab.grid.Revision()})
	if tab.res == nil || tab.res.Text != def || !strings.Contains(m.View(), "CREATE OR REPLACE FUNCTION") {
		t.Fatalf("텍스트 결과:\n%s", m.View())
	}
}

func TestDefinition_실제_서버에서_테이블과_함수를_찾는다(t *testing.T) {
	m := pgModel(t, "it-local")
	p := pgTest{t, m}
	p.sql(`drop schema if exists v4ui cascade; create schema v4ui; create table v4ui.v4ui_t (id int primary key);
		create function v4ui.v4ui_fn() returns int language sql as 'select 1'`)
	t.Cleanup(func() { p.sql("drop schema if exists v4ui cascade") })
	cn := m.roots[0].children[0]
	m.selectNode(cn)
	drive(m, m.activate(cn, false))
	tab := m.cur()
	tab.editor.SetValue("select v4ui.v4ui_fn() from v4ui.v4ui_t")
	m.setFocus(fEditor)
	for range len("select v4ui.v4") {
		m.Update(tea.KeyMsg{Type: tea.KeyRight})
	}
	_, cmd := m.Update(tea.KeyMsg{Type: tea.KeyCtrlB})
	drive(m, cmd)
	if tab.res == nil || !strings.Contains(tab.res.Text, "v4ui_fn()") {
		t.Fatalf("함수 정의: status=%q res=%+v", m.status, tab.res)
	}
	m.Update(tea.KeyMsg{Type: tea.KeyEnd})
	_, cmd = m.Update(tea.KeyMsg{Type: tea.KeyCtrlB})
	drive(m, cmd)
	if n := m.current(); n == nil || n.label != "v4ui_t" || n.schema != "v4ui" || tab.res == nil || len(tab.res.Cols) == 0 {
		t.Fatalf("테이블 구조: cur=%v status=%q", m.current(), m.status)
	}
}

// ─── R27 데이터베이스·스키마 ───

func TestDB_B로_고르면_열린_트랜잭션을_먼저_묻고_없으면_다시_연다(t *testing.T) {
	m := testModel(t)
	stg := m.roots[0].children[1]
	m.setFocus(fTree)
	m.selectNode(stg)
	_, cmd := m.Update(key("B"))
	if cmd == nil {
		t.Fatalf("데이터베이스 목록을 읽는다: %q", m.status)
	}
	msg := cmd().(pickListMsg)

	m.inTx[1] = true
	m.cur().conn = 1
	m.newTab() // 같은 접속을 쓰는 다른 탭이 있어도 세션을 닫으면 트랜잭션이 사라지므로 묻는다
	if msg.do("other"); m.overlay != oTx || m.txAsk.after != afterDB || m.conn(1).DB != "" {
		t.Fatalf("열린 트랜잭션을 먼저 묻는다: overlay=%v", m.overlay)
	}
	m.Update(tea.KeyMsg{Type: tea.KeyEsc})
	if m.conn(1).DB != "" {
		t.Fatal("esc 면 바꾸지 않는다")
	}

	m.inTx[1] = false
	m.schemaOf[1] = "app"
	drive(m, msg.do("other"))
	if m.conn(1).DB != "other" || m.dbOver[1] != "other" || m.schemaOf[1] != "" || !strings.Contains(m.View(), "x-stg/other") {
		t.Fatalf("다시 열고 이름 뒤에 DB 를 보인다: db=%q status=%q", m.conn(1).DB, m.status)
	}

	// 트랜잭션 확인 창에서 끝낸 뒤에는 미뤄 둔 DB 바꾸기를 한다
	drive(m, func() tea.Msg {
		return txDoneMsg{prompt: txPrompt{tab: m.cur().id, conns: []int{1}, after: afterDB, db: "third"}, done: []int{1}}
	})
	if m.conn(1).DB != "third" {
		t.Fatalf("트랜잭션을 끝낸 뒤 바꾼다: %q", m.conn(1).DB)
	}
}

func TestDB_실제_서버에서_B와_F6이_DB와_search_path를_바꾼다(t *testing.T) {
	m := pgModel(t, "it-local")
	p := pgTest{t, m}
	p.sql("drop schema if exists v4ui_sp cascade; create schema v4ui_sp; drop database if exists v4ui_db with (force); create database v4ui_db")
	t.Cleanup(func() { p.sql("drop schema if exists v4ui_sp cascade; drop database if exists v4ui_db with (force)") })
	orig := m.conn(0).DB
	cn := m.roots[0].children[0]
	m.selectNode(cn)
	drive(m, m.activate(cn, false))

	m.setFocus(fEditor)
	_, cmd := m.Update(tea.KeyMsg{Type: tea.KeyF6})
	msg := cmd().(pickListMsg)
	if msg.err != nil || !strings.Contains(strings.Join(msg.items, ","), "v4ui_sp") {
		t.Fatalf("스키마 목록: %v %v", msg.err, msg.items)
	}
	drive(m, msg.do("v4ui_sp"))
	if m.schemaOf[0] != "v4ui_sp" || !strings.Contains(m.View(), "it-local · v4ui_sp") {
		t.Fatalf("스키마를 보인다: %q", m.status)
	}
	drive(m, m.run("show search_path"))
	if res := m.cur().res; res == nil || !strings.Contains(res.Rows[0][0], "v4ui_sp") {
		t.Fatalf("편집기 세션의 search_path: %+v %q", res, m.status)
	}

	m.setFocus(fTree)
	m.selectNode(cn)
	_, cmd = m.Update(key("B"))
	msg = cmd().(pickListMsg)
	if msg.err != nil || len(msg.items) == 0 {
		t.Fatalf("데이터베이스 목록: %v", msg.err)
	}
	if !strings.Contains(strings.Join(msg.items, ","), "v4ui_db") {
		t.Fatalf("목록에 v4ui_db: %v", msg.items)
	}
	drive(m, msg.do("v4ui_db"))
	m.setFocus(fEditor)
	drive(m, m.run("select current_database()"))
	if res := m.cur().res; res == nil || res.Rows[0][0] != "v4ui_db" || !strings.Contains(m.View(), "it-local/v4ui_db") {
		t.Fatalf("다른 DB 로 다시 열림: %+v %q", res, m.status)
	}
	drive(m, msg.do(orig))
	if m.schemaOf[0] != "" || m.conn(0).DB != orig {
		t.Fatalf("다시 열면 고른 스키마 표시는 지운다: %q", m.schemaOf[0])
	}
	m.setFocus(fEditor)
	drive(m, m.run("select current_database(), current_setting('search_path')"))
	if res := m.cur().res; res == nil || res.Rows[0][0] != orig || strings.Contains(res.Rows[0][1], "v4ui_sp") {
		t.Fatalf("새 세션은 새 DB, 고른 search_path 없음: %+v %q", res, m.status)
	}
}

// ─── R29 즐겨찾기 ───

func TestFavorites_저장_덮어쓰기_열기_지우기(t *testing.T) {
	m := testModel(t)
	tab := m.cur()
	tab.conn = 1
	tab.editor.SetValue("select 1")
	m.Update(tea.KeyMsg{Type: tea.KeyF4})
	if m.overlay != oPrompt {
		t.Fatal("F4 는 이름을 묻는다")
	}
	typeText(m, "오늘 가입자")
	m.Update(key("enter"))
	store, err := favorites.Load(m.proj.Dir)
	must(t, err)
	if f, ok := store.Get("오늘 가입자"); !ok || f.SQL != "select 1" || f.Conn != "s" || m.status != "즐겨찾기 저장함: 오늘 가입자" {
		t.Fatalf("저장: %+v status=%q", f, m.status)
	}

	tab.editor.SetValue("select 2")
	m.Update(tea.KeyMsg{Type: tea.KeyF4})
	typeText(m, "오늘 가입자")
	m.Update(key("enter"))
	if m.overlay != oConfirm || !strings.Contains(m.View(), `즐겨찾기 "오늘 가입자" 덮어쓸까요: y/N`) {
		t.Fatalf("같은 이름이면 묻는다:\n%s", m.View())
	}
	m.Update(key("n"))
	store, _ = favorites.Load(m.proj.Dir)
	if f, _ := store.Get("오늘 가입자"); f.SQL != "select 1" {
		t.Fatal("N 이면 덮어쓰지 않는다")
	}
	m.Update(tea.KeyMsg{Type: tea.KeyF4})
	typeText(m, "오늘 가입자")
	m.Update(key("enter"))
	m.Update(key("y"))
	store, _ = favorites.Load(m.proj.Dir)
	if f, _ := store.Get("오늘 가입자"); f.SQL != "select 2" {
		t.Fatal("y 면 덮어쓴다")
	}

	tab.conn = 0
	m.Update(tea.KeyMsg{Type: tea.KeyF3})
	if m.overlay != oPicker || !strings.Contains(m.View(), "오늘 가입자") || !strings.Contains(m.View(), "→ x-stg") {
		t.Fatalf("F3 은 목록:\n%s", m.View())
	}
	m.Update(key("enter"))
	if len(m.tabs) != 2 || m.cur().editor.Value() != "select 2" || m.cur().conn != 1 || m.focus != fEditor {
		t.Fatalf("새 탭에 저장한 접속으로: tabs=%d conn=%d", len(m.tabs), m.cur().conn)
	}

	m.Update(tea.KeyMsg{Type: tea.KeyF3})
	m.Update(key("d"))
	if m.overlay != oConfirm || !strings.Contains(m.View(), `즐겨찾기 "오늘 가입자" 지울까요`) {
		t.Fatalf("d 는 묻는다:\n%s", m.View())
	}
	m.Update(key("y"))
	store, _ = favorites.Load(m.proj.Dir)
	if len(store.List()) != 0 {
		t.Fatal("지운다")
	}
	b, err := os.ReadFile(favorites.Path(m.proj.Dir))
	must(t, err)
	if strings.TrimSpace(string(b)) != "[]" {
		t.Fatalf("파일: %q", b)
	}
}

func TestFavorites_목록에서_글자를_치면_거르고_저장한_접속이_없으면_지금_접속(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 0
	store, err := favorites.Load(m.proj.Dir)
	must(t, err)
	must(t, store.Put(favorites.Fav{Name: "today", Conn: "gone", SQL: "select 3"}))
	must(t, store.Put(favorites.Fav{Name: "weekly", Conn: "", SQL: "select 4"}))
	m.Update(tea.KeyMsg{Type: tea.KeyF3})
	typeText(m, "od")
	if m.overlay != oPicker || len(m.pickerMatches()) != 1 {
		t.Fatalf("거르기 글이 있으면 d 도 글자다: %v", m.pickerMatches())
	}
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlD})
	if m.overlay != oConfirm || !strings.Contains(m.View(), `"today" 지울까요`) {
		t.Fatalf("Ctrl+D 는 거르기 글이 있어도 지우기:\n%s", m.View())
	}
	m.Update(key("n"))
	m.Update(tea.KeyMsg{Type: tea.KeyF3})
	typeText(m, "oda")
	m.Update(key("enter"))
	if m.cur().editor.Value() != "select 3" || m.cur().conn != 0 {
		t.Fatalf("없는 접속이면 지금 접속: conn=%d", m.cur().conn)
	}
}

// ─── R30 긴 실행 알림 ───

func TestNotify_기준_시간을_넘긴_실행만_알리고_취소와_SQL은_넣지_않는다(t *testing.T) {
	var buf bytes.Buffer
	oldOut, oldTTY := clipboardOut, stdoutTTY
	clipboardOut, stdoutTTY = &buf, func() bool { return true }
	t.Cleanup(func() { clipboardOut, stdoutTTY = oldOut, oldTTY })
	t.Setenv("DB_NOTIFY_SECS", "10")

	m := testModel(t)
	tab := m.cur()
	tab.conn = 1
	finish := func(start time.Time, err error) {
		_, cmd := m.Update(resultMsg{tab: tab.id, seq: tab.seq, conn: 1, sql: "select secret_col from t", start: start, err: err,
			res: &db.Result{Cols: []string{"secret_col"}, Rows: [][]string{{"1"}, {"2"}}}, rev: tab.grid.Revision()})
		drive(m, cmd)
	}
	m.run("select secret_col from t")
	finish(time.Now().Add(-3*time.Second), nil)
	if buf.Len() != 0 {
		t.Fatalf("기준 아래는 알리지 않는다: %q", buf.String())
	}
	m.run("select secret_col from t")
	finish(time.Now().Add(-12*time.Second), nil)
	if out := buf.String(); !strings.HasPrefix(out, "\x1b]9;db: x-stg 끝 12.") || !strings.HasSuffix(out, " · 2행\a") || strings.Contains(out, "secret") {
		t.Fatalf("OSC 9, SQL 없이: %q", out)
	}
	buf.Reset()
	m.run("select secret_col from t")
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlC})
	finish(time.Now().Add(-12*time.Second), context.Canceled)
	if buf.Len() != 0 {
		t.Fatalf("취소는 알리지 않는다: %q", buf.String())
	}
	stdoutTTY = func() bool { return false }
	m.run("select secret_col from t")
	finish(time.Now().Add(-12*time.Second), errors.New("boom"))
	if buf.Len() != 0 {
		t.Fatalf("터미널이 아니면 보내지 않는다: %q", buf.String())
	}
}

// ─── R33 전체 내보내기·데이터 검색 ───

func TestExport_F7은_쓰기_문장과_결과_없음을_거절한다(t *testing.T) {
	m := testModel(t)
	tab := m.cur()
	tab.conn = 1
	if m.Update(tea.KeyMsg{Type: tea.KeyF7}); m.export != nil || !strings.Contains(m.status, "전체 내보낼 표 결과가 없음") {
		t.Fatalf("결과가 없으면 안내: %q", m.status)
	}
	m.run("delete from t where id = 1")
	m.Update(resultMsg{tab: tab.id, seq: tab.seq, conn: 1, sql: "delete from t where id = 1", res: &db.Result{Text: "DELETE 1"}, rev: tab.grid.Revision()})
	if m.Update(tea.KeyMsg{Type: tea.KeyF7}); m.export != nil || m.status != "읽기 문장만 전체 내보내기" {
		t.Fatalf("쓰기 문장은 거절: %q", m.status)
	}
	tab.exportSQL = "select * from t where id = :id"
	if m.Update(tea.KeyMsg{Type: tea.KeyF7}); m.export != nil || !strings.Contains(m.status, "파라미터") {
		t.Fatalf("파라미터 문장은 거절: %q", m.status)
	}
}

func TestExport_실제_서버에서_F7은_행_제한_없이_CSV로_쓰고_테이블_보기는_WHERE를_따른다(t *testing.T) {
	m := pgModel(t, "it-local")
	home := t.TempDir()
	t.Setenv("HOME", home)
	must(t, os.Mkdir(filepath.Join(home, "Downloads"), 0o700))
	p := pgTest{t, m}
	p.sql("drop table if exists v4ui_export; create table v4ui_export as select g as id from generate_series(1, 1500) g")
	t.Cleanup(func() { p.sql("drop table if exists v4ui_export") })
	cn := m.roots[0].children[0]
	m.selectNode(cn)
	drive(m, m.activate(cn, false))
	m.setFocus(fEditor)
	drive(m, m.run("select id from v4ui_export order by id"))
	_, cmd := m.Update(tea.KeyMsg{Type: tea.KeyF7})
	if m.export == nil {
		t.Fatalf("내보내기 시작: %q", m.status)
	}
	drive(m, cmd)
	path := strings.TrimSpace(m.status[strings.Index(m.status, ": ")+2:])
	if m.export != nil || !strings.HasPrefix(m.status, "1,500행 전체 내보냄") || filepath.Dir(path) != filepath.Join(home, "Downloads") {
		t.Fatalf("끝: %q", m.status)
	}
	b, err := os.ReadFile(path)
	must(t, err)
	st, err := os.Stat(path)
	must(t, err)
	if lines := strings.Split(strings.TrimSpace(string(b)), "\n"); len(lines) != 1501 || lines[0] != "id" || st.Mode().Perm() != 0o600 {
		t.Fatalf("CSV 머리+1500행, 0600: %d줄 %v", len(lines), st.Mode().Perm())
	}

	m.setFocus(fTree)
	schema := cn.children[slicesIndexLabel(cn.children, "public")]
	drive(m, m.expand(schema))
	m.selectNode(schema.children[slicesIndexLabel(schema.children, "v4ui_export")])
	drive(m, m.activate(m.current(), false))
	if m.cur().tv == nil {
		t.Fatalf("테이블 보기: %q", m.status)
	}
	m.setFocus(fResult)
	m.Update(key("w"))
	typeText(m, "id <= 10")
	_, cmd = m.Update(key("enter"))
	drive(m, cmd)
	_, cmd = m.Update(tea.KeyMsg{Type: tea.KeyF7})
	drive(m, cmd)
	if !strings.HasPrefix(m.status, "10행 전체 내보냄") {
		t.Fatalf("테이블 보기는 WHERE 그대로: %q", m.status)
	}
}

func slicesIndexLabel(ns []*node, label string) int {
	for i, n := range ns {
		if n.label == label {
			return i
		}
	}
	return 0
}

func TestSearch_F는_prod면_테이블_수를_세어_묻고_stg는_바로_찾는다(t *testing.T) {
	m := testModel(t)
	m.setFocus(fTree)
	m.selectNode(m.roots[0].children[0])
	m.Update(key("F"))
	if m.overlay != oPrompt {
		t.Fatal("찾을 글을 묻는다")
	}
	typeText(m, "abc")
	_, cmd := m.Update(key("enter"))
	if cmd == nil || m.cur().running != nil {
		t.Fatal("prod 는 테이블 수부터 센다")
	}
	msg := cmd().(searchCountMsg)
	msg.err, msg.n = nil, 12
	m.Update(msg)
	if m.overlay != oConfirm || !strings.Contains(m.View(), "prod(x-prod) 테이블 12개 전체 검색: y/N") {
		t.Fatalf("prod 확인:\n%s", m.View())
	}
	if _, cmd := m.Update(key("y")); cmd == nil || m.cur().running == nil || m.cur().conn != 0 {
		t.Fatal("y 면 찾는다")
	}

	m.setFocus(fTree)
	m.cur().running = nil
	m.selectNode(m.roots[0].children[1])
	m.Update(key("F"))
	typeText(m, "abc")
	if _, cmd := m.Update(key("enter")); cmd == nil || m.overlay != oNone || m.cur().running == nil {
		t.Fatal("stg 는 묻지 않고 찾는다")
	}
	m.selectNode(m.roots[0])
	if m.Update(key("F")); m.overlay != oNone || !strings.Contains(m.status, "접속·스키마 줄") {
		t.Fatalf("그룹 줄은 안내만: %q", m.status)
	}
}

func TestSearch_실제_서버에서_스키마_글자_열을_찾는다(t *testing.T) {
	m := pgModel(t, "it-local")
	p := pgTest{t, m}
	p.sql(`drop schema if exists v4ui_s cascade; create schema v4ui_s;
		create table v4ui_s.a (id int primary key, note text); insert into v4ui_s.a values (1, 'hello v4ui needle'), (2, 'x')`)
	t.Cleanup(func() { p.sql("drop schema if exists v4ui_s cascade") })
	cn := m.roots[0].children[0]
	m.selectNode(cn)
	drive(m, m.activate(cn, false))
	schema := cn.children[slicesIndexLabel(cn.children, "v4ui_s")]
	m.setFocus(fTree)
	m.selectNode(schema)
	m.Update(key("F"))
	typeText(m, "needle")
	_, cmd := m.Update(key("enter"))
	drive(m, cmd)
	res := m.cur().res
	if res == nil || len(res.Rows) != 1 || res.Rows[0][0] != "a" || res.Rows[0][2] != "id=1" || !strings.HasPrefix(res.Text, "데이터 검색 1건") {
		t.Fatalf("검색 결과: %+v %q", res, m.status)
	}
}

// ─── R34 Redis 모니터 ───

func TestRedisMonitor_A_L_S는_prod에서도_묻지_않고_결과를_읽는다(t *testing.T) {
	m := testModel(t)
	rn := addRedis(m)
	m.proj.Conns[rn.conn].Name = "r-prod"
	for _, k := range []string{"A", "L", "S"} {
		m.setFocus(fTree)
		m.selectNode(rn)
		m.cur().running = nil
		if _, cmd := m.Update(key(k)); cmd == nil || m.overlay != oNone || m.cur().running == nil || m.cur().conn != rn.conn {
			t.Fatalf("%s: 묻지 않고 읽는다 overlay=%v status=%q", k, m.overlay, m.status)
		}
	}
}

func TestRedisMonitor_실제_서버에서_INFO_SLOWLOG_큰_키(t *testing.T) {
	addr := os.Getenv("DBTUI_TEST_REDIS_ADDR")
	if addr == "" {
		t.Skip("DBTUI_TEST_REDIS_ADDR not set")
	}
	cl := redis.NewClient(&redis.Options{Addr: addr})
	defer cl.Close()
	ctx := context.Background()
	must(t, cl.Set(ctx, "it:v4ui:big", strings.Repeat("v", 5000), 0).Err())
	defer cl.Del(ctx, "it:v4ui:big")

	m := testModel(t)
	host, port, _ := strings.Cut(addr, ":")
	pn, err := strconv.Atoi(port)
	must(t, err)
	m.proj.Conns = append(m.proj.Conns, db.Conn{Name: "r-local", UUID: "rl", Kind: db.KindRedis, Host: host, Port: pn, DB: "0"})
	t.Cleanup(func() { m.stop(); m.clients.Close() })
	rn := &node{kind: nConn, label: "r-local", conn: 2, parent: m.roots[0]}
	m.roots[0].children = append(m.roots[0].children, rn)
	m.rebuild()
	for k, want := range map[string]string{"A": "", "L": "", "S": "표본"} {
		m.setFocus(fTree)
		m.selectNode(rn)
		_, cmd := m.Update(key(k))
		drive(m, cmd)
		res := m.cur().res
		if res == nil || len(res.Cols) == 0 || !strings.Contains(res.Text, want) {
			t.Fatalf("%s: %+v %q", k, res, m.status)
		}
	}
}

func TestCompare_고정_결과와_지금_결과를_비교한다(t *testing.T) {
	m := testModel(t)
	m.setFocus(fResult)
	show(m, &db.Result{Cols: []string{"a"}, Rows: [][]string{{"x"}, {"y"}}})
	m.Update(key("="))
	if !strings.Contains(m.status, "비교할 고정 결과가 없음") {
		t.Fatalf("고정이 없으면 알린다: %q", m.status)
	}
	m.Update(key("p"))
	show(m, &db.Result{Cols: []string{"a"}, Rows: [][]string{{"y"}, {"z"}}})
	m.Update(key("="))
	if v := m.View(); !strings.Contains(v, "+1 −1 ~0") {
		t.Fatalf("고정 하나면 바로 비교한다:\n%s", v)
	}
	m.Update(key("esc"))

	show(m, &db.Result{Cols: []string{"b"}, Rows: [][]string{{"1"}}})
	m.Update(key("p"))
	show(m, &db.Result{Cols: []string{"a"}, Rows: [][]string{{"q"}}})
	m.Update(key("="))
	if m.overlay != oPicker || len(m.pickerItems) != 2 {
		t.Fatalf("고정이 둘이면 고른다: overlay=%v items=%v", m.overlay, m.pickerItems)
	}
	m.Update(key("down"))
	m.Update(key("enter"))
	if !strings.Contains(m.status, "열이 달라 비교할 수 없음") {
		t.Fatalf("열이 다른 결과는 이유를 보인다: %q", m.status)
	}
}

func TestCompare_JSON_값_보기에서는_t가_계획_나무로_가지_않는다(t *testing.T) {
	m := testModel(t)
	m.setFocus(fResult)
	show(m, &db.Result{Cols: []string{"j"}, Types: []string{"jsonb"}, Rows: [][]string{{`{"a":1}`}}})
	tab := m.cur()
	root, err := plantree.Parse(`[{"Plan":{"Node Type":"Seq Scan"}}]`)
	must(t, err)
	pm := plantree.New(root)
	tab.plan = &pm
	m.Update(key("o"))
	m.Update(key("t"))
	if tab.planOn {
		t.Fatal("값 보기 중 t 는 grid 의 원문 전환이다")
	}
}

// ─── 리뷰 고침: DB 바꾸기·정의로 이동·비교·내보내기·검색·자동완성 ───

func TestDB_B로_바꾸면_옛_DB의_테이블_보기와_결과로_새_DB에_보내지_않는다(t *testing.T) {
	m := testModel(t)
	tv := m.cur()
	drive(m, m.activate(addTable(m, 1), false))
	if tv.tv == nil || tv.viewConn != 1 {
		t.Fatalf("stg 테이블 보기: viewConn=%d", tv.viewConn)
	}
	edit := m.newTab()
	show(m, editable())

	m.setFocus(fTree)
	m.selectNode(m.roots[0].children[1])
	_, cmd := m.Update(key("B"))
	drive(m, cmd().(pickListMsg).do("other"))
	if m.conn(1).DB != "other" {
		t.Fatalf("DB 를 바꾼다: %q", m.status)
	}

	m.setFocus(fResult)
	editCell(m, "q")
	if m.Update(key("ctrl+s")); m.overlay != oNone || edit.running != nil || !strings.Contains(m.status, "데이터베이스를 바꾸기 전") {
		t.Fatalf("옛 DB 결과의 편집을 새 DB 에 제출하지 않는다: overlay=%v %q", m.overlay, m.status)
	}
	m.switchTab(0)
	if _, cmd := m.Update(key("]")); cmd != nil || tv.running != nil || !strings.Contains(m.status, "데이터베이스를 바꾸기 전") {
		t.Fatalf("옛 DB 테이블 보기의 다음 페이지를 새 DB 에서 읽지 않는다: %q", m.status)
	}
}

func TestDB_실행_중_보류_편집_바꾸는_중에는_B를_막고_내보내기는_취소한다(t *testing.T) {
	m := testModel(t)
	tab := m.cur()
	tab.conn = 1
	m.manual[1] = true
	m.setFocus(fTree)
	m.selectNode(m.roots[0].children[1])
	_, cmd := m.Update(key("B"))
	pick := cmd().(pickListMsg)

	// Manual 의 첫 문장은 BEGIN 을 보냈어도 결과가 오기 전이라 inTx 가 아직 거짓이다
	m.run("update t set a = 1 where id = 1")
	if tab.running == nil || m.inTx[1] {
		t.Fatal("실행 중, 트랜잭션 표시는 아직 없음")
	}
	if pick.do("other"); m.conn(1).DB != "" || m.overlay != oNone || m.status != "실행 중인 쿼리를 먼저 끝내세요" {
		t.Fatalf("실행 중이면 막는다: db=%q %q", m.conn(1).DB, m.status)
	}
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlC})

	show(m, editable())
	m.setFocus(fResult)
	editCell(m, "q")
	if pick.do("other"); m.conn(1).DB != "" || !strings.Contains(m.status, "변경 1건을 먼저 제출") {
		t.Fatalf("보류 중인 편집이 있으면 막는다: %q", m.status)
	}
	m.Update(key("u"))
	if tab.grid.Pending() != 0 {
		t.Fatal("u 로 되돌림")
	}

	canceled := false
	job := &exportJob{cancel: func() { canceled = true }, conn: 1, start: time.Now()}
	m.export = job
	switching := pick.do("other")
	if switching == nil || m.conn(1).DB != "other" || !canceled || !job.canceled {
		t.Fatalf("바꾸기 전에 그 접속의 내보내기를 취소한다: canceled=%v", canceled)
	}
	if pick.do("third"); m.conn(1).DB != "other" || m.status != "데이터베이스를 바꾸는 중" {
		t.Fatalf("바꾸는 중의 두 번째 B 는 막는다: db=%q %q", m.conn(1).DB, m.status)
	}
	drive(m, switching)
	if m.switching || m.dbOver[1] != "other" {
		t.Fatalf("끝나면 다시 바꿀 수 있다: %q", m.status)
	}
}

func TestDefinition_CtrlB_결과는_요청한_탭에_붙고_그_탭을_닫았으면_버린다(t *testing.T) {
	m := testModel(t)
	addTable(m, 1)
	tab := m.cur()
	tab.conn = 1
	other := m.newTab()
	m.Update(defMsg{tab: tab.id, conn: 1, name: "users", obj: db.Object{Kind: db.ObjTable, Schema: "public", Name: "users"}})
	if tab.running == nil || other.running != nil {
		t.Fatalf("요청한 탭에 구조를 연다: tab=%v other=%v", tab.running != nil, other.running != nil)
	}

	// 부모를 읽는 사이 요청한 탭을 닫으면 다른 탭에 열지 않는다
	cn := m.roots[0].children[0]
	cn.loaded, cn.children = false, nil
	gone := m.newTab()
	gone.conn = 0
	_, cmd := m.Update(defMsg{tab: gone.id, conn: 0, name: "users", obj: db.Object{Kind: db.ObjTable, Schema: "public", Name: "users"}})
	if cmd == nil || m.jump == nil {
		t.Fatal("트리를 읽고 이어 찾는다")
	}
	m.closeTab(gone)
	schema := &node{kind: nSchema, label: "public", conn: 0, parent: cn, loaded: true}
	schema.children = []*node{{kind: nTable, label: "users", schema: "public", conn: 0, parent: schema}}
	m.Update(childrenMsg{n: cn, gen: cn.gen, children: []*node{schema}})
	for _, tb := range m.tabs {
		if tb != tab && tb.running != nil {
			t.Fatalf("닫힌 탭의 정의를 %s 에 열지 않는다", tb.title)
		}
	}
}

func TestCompare_고정_이름이_겹쳐도_고른_결과와_비교한다(t *testing.T) {
	m := testModel(t)
	m.setFocus(fResult)
	show(m, &db.Result{Cols: []string{"a"}, Rows: [][]string{{"1"}}})
	m.Update(key("p"))
	show(m, &db.Result{Cols: []string{"b"}, Rows: [][]string{{"1"}}})
	m.Update(key("p"))
	m.Update(key("}"))
	m.Update(key("P")) // 고정 1 을 풀면 다음 고정도 "고정 2" 다
	show(m, &db.Result{Cols: []string{"a"}, Rows: [][]string{{"2"}}})
	m.Update(key("p"))
	tab := m.cur()
	if len(tab.subs) != 2 || tab.subs[0].title != tab.subs[1].title {
		t.Fatalf("이름이 겹치는 고정 둘: %+v", tab.subs)
	}
	show(m, &db.Result{Cols: []string{"a"}, Rows: [][]string{{"3"}}})
	m.Update(key("="))
	if m.overlay != oPicker || len(m.pickerItems) != 2 {
		t.Fatalf("고른다: %v", m.pickerItems)
	}
	m.Update(key("enter")) // 첫 항목은 열이 b 인 결과다
	if !strings.Contains(m.status, "열이 달라 비교할 수 없음") {
		t.Fatalf("이름이 같아도 고른 결과와 비교한다: %q", m.status)
	}
}

func TestExport_F7은_실패한_실행과_고정_결과를_내보내지_않는다(t *testing.T) {
	m := testModel(t)
	tab := m.cur()
	tab.conn = 1
	m.run("select * from a")
	m.Update(resultMsg{tab: tab.id, seq: tab.seq, conn: 1, sql: "select * from a", res: &db.Result{Cols: []string{"x"}, Rows: [][]string{{"1"}}}, rev: tab.grid.Revision()})
	m.run("select * from broken")
	m.Update(resultMsg{tab: tab.id, seq: tab.seq, conn: 1, sql: "select * from broken", err: errors.New("relation does not exist"), rev: tab.grid.Revision()})
	if m.Update(tea.KeyMsg{Type: tea.KeyF7}); m.export != nil || !strings.Contains(m.status, "전체 내보낼 표 결과가 없음") {
		t.Fatalf("실패한 문장은 내보내지 않는다: %q", m.status)
	}

	m.run("select * from a")
	m.Update(resultMsg{tab: tab.id, seq: tab.seq, conn: 1, sql: "select * from a", res: &db.Result{Cols: []string{"x"}, Rows: [][]string{{"1"}}}, rev: tab.grid.Revision()})
	m.setFocus(fResult)
	m.Update(key("p"))
	m.Update(key("}"))
	if m.Update(tea.KeyMsg{Type: tea.KeyF7}); m.export != nil || !strings.Contains(m.status, "고정·문장별 결과는 전체 내보내지 않음") {
		t.Fatalf("고정 결과를 보는 중이면 거절: %q", m.status)
	}
}

func TestExport_esc는_차트를_먼저_닫고_내보내기는_이어진다(t *testing.T) {
	m := testModel(t)
	m.setFocus(fResult)
	show(m, &db.Result{Cols: []string{"name", "n"}, Types: []string{"text", "int4"}, Rows: [][]string{{"a", "1"}, {"b", "2"}}})
	m.Update(key("c"))
	tab := m.cur()
	if !tab.grid.Viewing() {
		t.Fatalf("차트: %q", m.status)
	}
	canceled := false
	job := &exportJob{cancel: func() { canceled = true }, conn: 1, start: time.Now()}
	m.export = job
	m.Update(tea.KeyMsg{Type: tea.KeyEsc})
	if tab.grid.Viewing() || canceled || m.export != job {
		t.Fatalf("esc 는 차트를 닫고 내보내기는 그대로: viewing=%v canceled=%v", tab.grid.Viewing(), canceled)
	}
	m.Update(tea.KeyMsg{Type: tea.KeyEsc})
	if !canceled || !job.canceled {
		t.Fatal("다른 일이 없으면 esc 는 내보내기를 취소한다")
	}
}

func TestExport_취소한_내보내기는_오류_글과_상관없이_취소로_보이고_알리지_않는다(t *testing.T) {
	var buf bytes.Buffer
	oldOut, oldTTY := clipboardOut, stdoutTTY
	clipboardOut, stdoutTTY = &buf, func() bool { return true }
	t.Cleanup(func() { clipboardOut, stdoutTTY = oldOut, oldTTY })
	t.Setenv("DB_NOTIFY_SECS", "10")

	m := testModel(t)
	job := &exportJob{cancel: func() {}, conn: 1, start: time.Now().Add(-time.Minute)}
	m.export = job
	m.setFocus(fEditor)
	m.Update(tea.KeyMsg{Type: tea.KeyEsc})
	// 드라이버는 취소를 context.Canceled 가 아닌 연결 끊김으로 알리기도 한다
	_, cmd := m.Update(exportDoneMsg{job: job, err: errors.New("conn closed")})
	drive(m, cmd)
	if m.status != "전체 내보내기 취소함" || buf.Len() != 0 || m.export != nil {
		t.Fatalf("취소로 보이고 알리지 않는다: %q notify=%q", m.status, buf.String())
	}
}

func TestSearch_prod에서_N이면_빈_결과_탭을_남기지_않는다(t *testing.T) {
	m := testModel(t)
	tab := m.cur()
	tab.conn = 1
	show(m, editable())
	m.setFocus(fResult)
	editCell(m, "q") // 다른 접속의 편집을 든 탭이라 검색 결과는 새 탭에 연다
	ask := func() {
		m.setFocus(fTree)
		m.selectNode(m.roots[0].children[0])
		m.Update(key("F"))
		typeText(m, "abc")
		_, cmd := m.Update(key("enter"))
		msg := cmd().(searchCountMsg)
		msg.err, msg.n = nil, 3
		m.Update(msg)
	}
	ask()
	if m.Update(key("n")); len(m.tabs) != 1 || m.cur() != tab {
		t.Fatalf("N 이면 탭을 만들지 않는다: tabs=%d", len(m.tabs))
	}
	ask()
	if _, cmd := m.Update(key("y")); cmd == nil || len(m.tabs) != 2 || m.cur().conn != 0 || m.cur().running == nil || tab.grid.Pending() != 1 {
		t.Fatalf("y 면 새 탭에서 찾는다: tabs=%d", len(m.tabs))
	}
}

func TestComplete_DB를_바꾼_뒤_늦게_온_옛_목록은_되살리지_않는다(t *testing.T) {
	m := testModel(t)
	m.ensureCatalog(1)
	old := m.catGen[1]
	m.Update(dbSwitchMsg{conn: 1, db: "other"})
	m.Update(catalogMsg{conn: 1, gen: old, cat: &complete.Catalog{Schemas: []complete.Schema{{Name: "old_db"}}}})
	if m.catalogs[1] != nil {
		t.Fatal("옛 DB 목록이 새 DB 자리를 채우지 않는다")
	}
	m.Update(catalogMsg{conn: 1, gen: m.catGen[1], cat: &complete.Catalog{Schemas: []complete.Schema{{Name: "new_db"}}}})
	if cat := m.catalogs[1]; cat == nil || cat.Schemas[0].Name != "new_db" {
		t.Fatal("새 요청의 목록은 붙인다")
	}
}

// ─── R25 위험 문장: 실행 경로마다 ───

func TestDanger_선택_여러_문장_파라미터_AltE_Redis_여러_줄도_묻는다(t *testing.T) {
	m := testModel(t)
	tab := m.cur()
	tab.conn = 1
	m.setFocus(fEditor)
	asks := func(name, reason string) {
		t.Helper()
		if m.overlay != oConfirm || !strings.Contains(m.View(), "위험: "+reason) {
			t.Fatalf("%s: 위험을 묻는다 overlay=%v\n%s", name, m.overlay, m.View())
		}
		m.Update(key("n"))
		if tab.running != nil {
			t.Fatalf("%s: N 이면 실행하지 않는다", name)
		}
	}

	tab.editor.SetValue("select 1;\ndelete from t")
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlA})
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlR})
	asks("선택 실행", "WHERE 없는 DELETE")

	m.run("select 1; delete from t")
	asks("여러 문장", "WHERE 없는 DELETE")

	m.run("delete from t returning :tag")
	if m.overlay != oParams {
		t.Fatal("파라미터 값을 먼저 묻는다")
	}
	typeText(m, "x")
	m.Update(key("enter"))
	asks("파라미터", "WHERE 없는 DELETE")

	tab.editor.SetValue("delete from t")
	m.Update(tea.KeyMsg{Type: tea.KeyHome})
	m.Update(tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("e"), Alt: true})
	if m.overlay != oConfirm {
		t.Fatalf("Alt+E 는 ANALYZE 를 묻는다: %q", m.status)
	}
	m.Update(key("y"))
	asks("Alt+E", "WHERE 없는 DELETE")

	addRedis(m)
	tab.conn = 2
	m.run("GET a\nFLUSHALL")
	asks("Redis 여러 줄", "FLUSHALL")
}

func TestErrText_SELECT_아닌_내보내기는_한국어로_알린다(t *testing.T) {
	m := testModel(t)
	if got := m.errText(0, fmt.Errorf("copy: %w", db.ErrExportNotSelect)); got != "SELECT 문만 전체 내보내기" {
		t.Fatalf("got %q", got)
	}
}

func TestSwitchDB_실패하면_DB와_내보내기_문장을_되돌리고_목록은_버린다(t *testing.T) {
	m := testModel(t)
	tab := m.cur()
	tab.exportConn, tab.exportSQL = 1, "select 1"
	prev := m.proj.Conns[1].DB
	m.ensureCatalog(1)
	old := m.catGen[1]
	m.proj.Conns[1].DB = "other" // switchDB 가 먼저 바꿔 둔 상태
	m.Update(dbSwitchMsg{conn: 1, db: "other", prev: prev, err: errors.New("boom")})
	if m.proj.Conns[1].DB != prev || tab.exportSQL != "select 1" {
		t.Fatalf("실패면 되돌린다: db=%q sql=%q", m.proj.Conns[1].DB, tab.exportSQL)
	}
	m.Update(catalogMsg{conn: 1, gen: old, cat: &complete.Catalog{}})
	if m.catalogs[1] != nil {
		t.Fatal("전환 중 시작한 목록 요청은 실패해도 버린다")
	}
}

func TestExport_esc는_편집기_선택을_먼저_풀고_내보내기는_이어진다(t *testing.T) {
	m := testModel(t)
	m.setFocus(fEditor)
	tab := m.cur()
	tab.editor.SetValue("select 1")
	m.Update(tea.KeyMsg{Type: tea.KeyShiftRight})
	if _, _, ok := tab.editor.Selection(); !ok {
		t.Fatal("shift+right 로 선택")
	}
	canceled := false
	m.export = &exportJob{cancel: func() { canceled = true }, conn: 1, start: time.Now()}
	m.Update(tea.KeyMsg{Type: tea.KeyEsc})
	if _, _, ok := tab.editor.Selection(); ok || canceled {
		t.Fatalf("esc 는 선택을 풀고 내보내기는 그대로: selected=%v canceled=%v", ok, canceled)
	}
}
