package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"path/filepath"
	"slices"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/history"
)

// ─── R73 연결된 곳 찾기 ───

func TestLinkedConns_같은_그룹_같은_환경의_PostgreSQL_접속만_고른다(t *testing.T) {
	proj := &db.Project{Conns: []db.Conn{
		{Name: "app-stg", Group: "app", Kind: db.KindPG},
		{Name: "app2-stg", Group: "app", Kind: db.KindPG},
		{Name: "app-prod", Group: "app", Kind: db.KindPG},
		{Name: "app-stg-redis", Group: "app", Kind: db.KindRedis},
		{Name: "other-stg", Group: "other", Kind: db.KindPG},
		{Name: "lone-stg", Kind: db.KindPG},
		{Name: "app2-prod", Group: "app", Kind: db.KindPG},
	}}
	if got := linkedConns(proj, 0); !slices.Equal(got, []int{0, 1}) {
		t.Errorf("stg 에서는 prod·Redis·다른 그룹이 빠진다: %v", got)
	}
	if got := linkedConns(proj, 2); !slices.Equal(got, []int{2, 6}) {
		t.Errorf("prod 는 prod 끼리: %v", got)
	}
	if got := linkedConns(proj, 5); !slices.Equal(got, []int{5}) {
		t.Errorf("그룹이 없으면 자기 하나: %v", got)
	}
}

// linkedSample 은 접속 둘의 찾기 결과다. 둘째 접속은 닿지 않았다.
func linkedSample() ([]db.Conn, []linkedFound) {
	conns := []db.Conn{{Name: "app-stg"}, {Name: "app2-stg", Password: "pw-secret"}}
	found := []linkedFound{{
		cols: []db.LinkedColumn{
			{Table: db.TableRef{Schema: "public", Table: "a_logs"}, Column: "user_id", Indexed: true},
			{Table: db.TableRef{Schema: "public", Table: "orders"}, Column: "user_id", Indexed: true},
			{Table: db.TableRef{Schema: "public", Table: "msgs"}, Column: "target_user_id"},
			{Table: db.TableRef{Schema: "public", Table: "users"}, Column: "id", Indexed: true, Owner: true},
			{Table: db.TableRef{Schema: "public", Table: "slow"}, Column: "user_id", Indexed: true},
			{Table: db.TableRef{Schema: "public", Table: "pays"}, Column: "payer_user_id", Indexed: true},
		},
		counts: []int{0, 101, -1, 1, 0, 3},
		errs:   []error{nil, nil, nil, nil, errors.New("canceling statement due to statement timeout"), nil},
	}, {err: errors.New("dial tcp: connection refused pw-secret")}}
	return conns, found
}

func TestLinkedTable_주인_행_수_순으로_늘어놓고_100행_넘으면_100플러스다(t *testing.T) {
	conns, found := linkedSample()
	res, targets, summary, failed := linkedTable(conns, found, func(i int, err error) string { return db.Redact(err.Error(), conns[i].Password) })
	var got []string
	for _, r := range res.Rows {
		got = append(got, strings.Join(r, "|"))
	}
	want := []string{
		"app2-stg|-|-|-|접속 실패: dial tcp: connection refused ***",
		"app-stg|public.users|id|1|주인 테이블",
		"app-stg|public.orders|user_id|100+|",
		"app-stg|public.pays|payer_user_id|3|",
		"app-stg|public.msgs|target_user_id|-|인덱스 없음 · 안 셈",
		"app-stg|public.slow|user_id|-|오류: canceling statement due to statement timeout",
		"app-stg|public.a_logs|user_id|0|",
	}
	if !slices.Equal(got, want) {
		t.Fatalf("줄:\n%s", strings.Join(got, "\n"))
	}
	if !failed || summary != "연결된 곳 3 · 접속 2곳 · 0행 1 · 인덱스 없어 안 셈 1 · 실패 2" {
		t.Fatalf("요약: %v %q", failed, summary)
	}
	if tg := targets["app-stg\x00public.orders\x00user_id"]; tg.conn != 0 || tg.ref.Table != "orders" || tg.col != "user_id" {
		t.Fatalf("줄 → 테이블: %+v", tg)
	}
	if _, ok := targets["app2-stg\x00-\x00-"]; ok {
		t.Fatal("접속 실패 줄은 열 테이블이 없다")
	}
}

func TestLinkedTable_0행이_50개를_넘으면_빼고_요약에_센다(t *testing.T) {
	f := linkedFound{}
	for i := range 51 {
		f.cols = append(f.cols, db.LinkedColumn{Table: db.TableRef{Schema: "s", Table: fmt.Sprintf("t%02d", i)}, Column: "user_id", Indexed: true})
		f.counts, f.errs = append(f.counts, 0), append(f.errs, nil)
	}
	f.cols = append(f.cols, db.LinkedColumn{Table: db.TableRef{Schema: "s", Table: "hit"}, Column: "user_id", Indexed: true})
	f.counts, f.errs = append(f.counts, 2), append(f.errs, nil)
	f.truncated = true
	res, _, summary, failed := linkedTable([]db.Conn{{Name: "c"}}, []linkedFound{f}, nil)
	if len(res.Rows) != 1 || res.Rows[0][1] != "s.hit" || failed {
		t.Fatalf("0행은 뺀다: %v", res.Rows)
	}
	if summary != "연결된 곳 1 · 접속 1곳 · 0행 51개 뺌 · 후보 200열에서 끊음: c" {
		t.Fatalf("요약: %q", summary)
	}
}

// linkedOrigin 은 orders 테이블 보기처럼 Origin 이 있는 결과다.
func linkedOrigin() *db.Result {
	return &db.Result{Cols: []string{"id", "uid"}, Types: []string{"int4", "int8"}, Rows: [][]string{{"7", "1315"}, {"8", "NULL"}},
		Origin: &db.Origin{Table: db.TableRef{Schema: "public", Table: "orders"}, PK: []string{"id"}, Cols: []string{"id", "user_id"}}}
}

func TestLinked_CtrlB는_새_탭에서_찾고_값은_제목_편집기에_남기지_않는다(t *testing.T) {
	m := testModel(t)
	h, err := history.Open(filepath.Join(t.TempDir(), "history.jsonl"))
	must(t, err)
	m.hist = h
	m.cur().conn = 1
	show(m, linkedOrigin())
	m.setFocus(fResult)
	m.Update(key("l"))
	_, cmd := m.Update(tea.KeyMsg{Type: tea.KeyCtrlB})
	tab := m.cur()
	if cmd == nil || len(m.tabs) != 2 || tab.linked == nil || tab.running == nil {
		t.Fatalf("새 탭에서 찾는다: tabs=%d status=%q", len(m.tabs), m.status)
	}
	// 원본을 알면 별칭(uid)이 아니라 테이블 열 이름(user_id)으로 찾는다
	if tab.title != "연결된 곳: user_id" || strings.Contains(tab.title+tab.editor.Value()+tab.displayName(), "1315") {
		t.Fatalf("제목에 값이 없다: %q", tab.title)
	}
	if !strings.Contains(m.status, "user_id = 1315") || tab.linked.value != "1315" || !slices.Equal(tab.linked.conns, []int{1}) {
		t.Fatalf("상태 줄에만 한 번: %q %+v", m.status, tab.linked)
	}
	if list := m.historyMatches(); len(list) != 0 {
		t.Fatalf("기록에 남기지 않는다: %v", list)
	}

	// esc 는 Ctrl+C 처럼 멈춘다
	m.Update(key("esc"))
	if tab.running != nil || !tab.cancelled || m.status != "취소 요청함" {
		t.Fatalf("esc 취소: %q", m.status)
	}
}

func TestLinked_NULL_Redis_원본_모르는_id는_찾지_않는다(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 1
	show(m, linkedOrigin())
	m.setFocus(fResult)
	m.Update(key("j"))
	m.Update(key("l"))
	if _, cmd := m.Update(tea.KeyMsg{Type: tea.KeyCtrlB}); cmd != nil || m.status != "값이 NULL" || len(m.tabs) != 1 {
		t.Fatalf("NULL: %q", m.status)
	}

	show(m, &db.Result{Cols: []string{"id"}, Types: []string{"int4"}, Rows: [][]string{{"1"}}})
	if _, cmd := m.Update(tea.KeyMsg{Type: tea.KeyCtrlB}); cmd != nil || m.status != "원본 테이블을 모르는 결과의 id 는 찾지 않음" {
		t.Fatalf("원본 모르는 id: %q", m.status)
	}

	addRedis(m)
	m.cur().conn = len(m.proj.Conns) - 1
	m.Update(resultMsg{tab: m.cur().id, seq: m.cur().seq, conn: m.cur().conn, res: &db.Result{Cols: []string{"k"}, Rows: [][]string{{"v"}}}})
	m.setFocus(fResult)
	if _, cmd := m.Update(tea.KeyMsg{Type: tea.KeyCtrlB}); cmd != nil || m.status != "PostgreSQL 결과에서만" {
		t.Fatalf("Redis: %q", m.status)
	}
}

func TestLinked_Enter는_그_줄의_테이블을_WHERE로_연다(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 1
	show(m, linkedOrigin())
	m.setFocus(fResult)
	m.Update(key("l"))
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlB})
	tab := m.cur()
	conns, found := linkedSample()
	conns[0].Name = m.conn(1).Name
	found = found[:1]
	m.Update(linkedMsg{tab: tab.id, seq: tab.seq, found: found})
	if tab.running != nil || len(tab.res.Rows) != 6 || !strings.HasPrefix(m.status, "연결된 곳 3 · 접속 1곳") {
		t.Fatalf("표를 붙인다: %q %v", m.status, tab.res)
	}
	if strings.Contains(m.status, "1315") {
		t.Fatalf("끝난 상태 줄에는 값이 없다: %q", m.status)
	}
	m.Update(key("j")) // public.orders 줄
	m.Update(key("enter"))
	nt := m.cur()
	if len(m.tabs) != 3 || nt.tv == nil || nt.title != "orders" || nt.conn != 1 {
		t.Fatalf("새 탭 테이블 보기: tabs=%d status=%q", len(m.tabs), m.status)
	}
	if sql := nt.tv.SQL(); !strings.Contains(sql, `"public"."orders" WHERE "user_id" = '1315'`) {
		t.Fatalf("WHERE: %q", sql)
	}

	// 접속을 다른 데이터베이스로 바꾼 뒤에는 열지 않는다
	m.switchTab(1)
	m.setFocus(fResult)
	m.proj.Conns[1].DB = "other"
	m.Update(key("enter"))
	if len(m.tabs) != 3 || m.status != "찾는 사이 데이터베이스를 바꿔 열지 않음" {
		t.Fatalf("DB 바뀜: %q", m.status)
	}
}

func TestLinked_오른쪽_클릭_창에_복사_항목과_연결된_곳_찾기가_있다(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 1
	show(m, linkedOrigin())
	m.setFocus(fResult)
	m.openCopyMenu(m.cur(), 0, true)
	if m.pickerItems[len(m.pickerItems)-1] != linkedMenuItem || !slices.Contains(m.pickerItems, "현재 행 · TSV (헤더)") {
		t.Fatalf("항목: %v", m.pickerItems)
	}
	m.Update(key("esc"))
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlK})
	if m.overlay != oPicker || slices.Contains(m.pickerItems, linkedMenuItem) {
		t.Fatal("Ctrl+K 복사 창에는 넣지 않는다")
	}
	m.Update(key("esc"))

	// 커서 칸은 id 이고 원본 orders 에서 이름 뿌리 order 를 뽑는다
	m.openCopyMenu(m.cur(), 0, true)
	cmd := pickLabel(t, m, linkedMenuItem)
	if cmd == nil || len(m.tabs) != 2 || m.cur().title != "연결된 곳: id" {
		t.Fatalf("고르면 Ctrl+B 와 같다: tabs=%d title=%q status=%q", len(m.tabs), m.cur().title, m.status)
	}
}

func TestLinked_팔레트와_도움말에_결과_Ctrl_B가_있다(t *testing.T) {
	m := testModel(t)
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlP})
	typeText(m, "연결된 곳 찾기")
	found := false
	for _, it := range m.pickerItems {
		found = found || strings.Contains(it, "연결된 곳 찾기") && strings.Contains(it, "결과")
	}
	if !found {
		t.Fatalf("팔레트: %v", m.pickerItems)
	}
}

func TestLinked_찾은_뒤_접속_호스트를_바꾸거나_지우면_Enter로_열지_않는다(t *testing.T) {
	for _, change := range []func(c *db.Conn){
		func(c *db.Conn) { c.Host = "db.prod.example.internal" },
		func(c *db.Conn) { *c = db.Conn{} },
	} {
		m := testModel(t)
		m.cur().conn = 1
		show(m, linkedOrigin())
		m.setFocus(fResult)
		m.Update(key("l"))
		m.Update(tea.KeyMsg{Type: tea.KeyCtrlB})
		tab := m.cur()
		_, found := linkedSample()
		m.Update(linkedMsg{tab: tab.id, seq: tab.seq, found: found[:1]})
		change(&m.proj.Conns[1])
		m.Update(key("j"))
		m.Update(key("enter"))
		if len(m.tabs) != 2 || m.status != "찾은 뒤 접속을 고치거나 지워 열지 않음 (다시 찾으세요)" {
			t.Fatalf("바뀐 접속에 값을 보내지 않는다: tabs=%d %q", len(m.tabs), m.status)
		}
	}
}

func TestLinked_찾은_접속을_파일에서_고치면_결과를_막는다(t *testing.T) {
	m := connTestModel(t)
	i := connByName(m.proj, "mine-stg")
	m.cur().conn = i
	m.Update(resultMsg{tab: m.cur().id, seq: m.cur().seq, conn: i, res: linkedOrigin(), rev: m.cur().grid.Revision()})
	m.setFocus(fResult)
	m.Update(key("l"))
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlB})
	tab := m.cur()
	m.Update(linkedMsg{tab: tab.id, seq: tab.seq, found: []linkedFound{{
		cols: []db.LinkedColumn{{Table: db.TableRef{Schema: "public", Table: "orders"}, Column: "user_id", Indexed: true}}, counts: []int{1}, errs: []error{nil},
	}}})
	// 서버·사용자·환경이 같은 칸(sslmode)만 고쳐도 고친 접속이라 막는다
	c := m.conn(i)
	c.SSLMode, c.Password = "disable", ""
	_, err := db.UpdateConn(m.proj.Dir, c.Name, c)
	must(t, err)
	m.reloadConns()
	m.switchTab(1)
	m.setFocus(fResult)
	m.Update(key("enter"))
	if len(m.tabs) != 2 || m.status != "찾은 뒤 접속을 고치거나 지워 열지 않음 (다시 찾으세요)" {
		t.Fatalf("고친 접속: tabs=%d %q", len(m.tabs), m.status)
	}
}

func TestFindLinked_접속은_두_개씩만_동시에_찾는다(t *testing.T) {
	var cur, peak, calls atomic.Int32
	old := linkedSearchConn
	linkedSearchConn = func(context.Context, *db.Clients, db.Conn, string, string, string) linkedFound {
		n := cur.Add(1)
		for p := peak.Load(); n > p && !peak.CompareAndSwap(p, n); p = peak.Load() {
		}
		time.Sleep(20 * time.Millisecond)
		cur.Add(-1)
		calls.Add(1)
		return linkedFound{}
	}
	t.Cleanup(func() { linkedSearchConn = old })
	found := findLinked(context.Background(), nil, make([]db.Conn, 5), "user_id", "user", "1")
	if len(found) != 5 || calls.Load() != 5 || peak.Load() != 2 {
		t.Fatalf("동시 접속 %d (상한 2), 부름 %d", peak.Load(), calls.Load())
	}
}

// ─── CLI ───

func TestCLI_linked_는_prod_플래그_사용법을_지킨다(t *testing.T) {
	dir := fakeCLIProject(t)
	if code, _, errw := runCLITest(t, "", "linked", "-p", dir, "-c", "app-prod", "--column", "user_id", "--value", "1"); code != exitRefused || errw != "prod 접속은 --prod 가 필요함\n" {
		t.Fatalf("prod: %d %q", code, errw)
	}
	for _, args := range [][]string{
		{"linked", "-p", dir, "-c", "app-stg", "--column", "user_id"},
		{"linked", "-p", dir, "-c", "app-stg", "--column", "id", "--value", "1"},
		{"linked", "-p", dir, "-c", "app-stg-redis", "--column", "user_id", "--value", "1"},
		{"query", "-p", dir, "-c", "app-stg", "--column", "x", "select 1"},
	} {
		if code, _, errw := runCLITest(t, "", args...); code != exitUsage {
			t.Errorf("%v: %d %q", args, code, errw)
		}
	}
	// 닿지 않는 접속은 오류 줄 하나로 찍고 1 이다. stg 에서는 같은 그룹의 prod 를 묻지 않는다
	code, out, errw := runCLITest(t, "", "linked", "-p", dir, "-c", "app-stg", "--column", "user_id", "--value", "1", "--format", "json")
	var m map[string]any
	if code != exitErr || json.Unmarshal([]byte(out), &m) != nil || len(m["rows"].([]any)) != 1 || strings.Contains(out, "app-prod") {
		t.Fatalf("접속 실패: %d %q %q", code, out, errw)
	}
	if strings.Contains(out+errw, cliPassword) {
		t.Fatal("비밀번호가 나왔다")
	}
}

// ─── 실제 서버 ───

func TestLinked_실제_서버에서_CtrlB로_찾고_Enter로_연다(t *testing.T) {
	m := pgModel(t, "it-local")
	it := pgTest{t, m}
	it.sql(`drop schema if exists lnkui cascade; create schema lnkui;
		create table lnkui.lnkui_members (id bigint primary key);
		create table lnkui.lnkui_orders (id int primary key, lnkui_member_id bigint);
		create index on lnkui.lnkui_orders (lnkui_member_id);
		create table lnkui.lnkui_plain (lnkui_member_id bigint);
		insert into lnkui.lnkui_members values (1315);
		insert into lnkui.lnkui_orders values (1, 1315), (2, 1315), (3, 7);
		insert into lnkui.lnkui_plain values (1315)`)
	t.Cleanup(func() { it.sql("drop schema if exists lnkui cascade") })
	h, err := history.Open(filepath.Join(t.TempDir(), "history.jsonl"))
	must(t, err)
	m.hist = h

	m.cur().conn = 0
	m.cur().editor.SetValue("select * from lnkui.lnkui_orders order by id")
	drive(m, m.run(m.target(m.cur())))
	m.setFocus(fResult)
	m.Update(key("l"))
	_, cmd := m.Update(tea.KeyMsg{Type: tea.KeyCtrlB})
	drive(m, cmd)
	tab := m.cur()
	if tab.title != "연결된 곳: lnkui_member_id" || tab.res == nil {
		t.Fatalf("찾기: %q status=%q", tab.title, m.status)
	}
	rows := map[string][]string{}
	at := -1
	for i, r := range tab.res.Rows {
		if strings.HasPrefix(r[1], "lnkui.") {
			rows[r[1]+"."+r[2]] = r
		}
		if r[1] == "lnkui.lnkui_orders" {
			at = i
		}
	}
	if r := rows["lnkui.lnkui_members.id"]; r == nil || r[3] != "1" || r[4] != "주인 테이블" {
		t.Errorf("주인: %v", r)
	}
	if r := rows["lnkui.lnkui_orders.lnkui_member_id"]; r == nil || r[3] != "2" {
		t.Errorf("orders: %v", r)
	}
	if r := rows["lnkui.lnkui_plain.lnkui_member_id"]; r == nil || r[3] != "-" || r[4] != "인덱스 없음 · 안 셈" {
		t.Errorf("인덱스 없음: %v", r)
	}
	for range at {
		m.Update(key("j"))
	}
	_, cmd = m.Update(key("enter"))
	drive(m, cmd)
	if nt := m.cur(); len(m.tabs) != 3 || nt.tv == nil || nt.res == nil || len(nt.res.Rows) != 2 {
		t.Fatalf("테이블 보기: tabs=%d status=%q", len(m.tabs), m.status)
	}
	// 연 보기의 다음 페이지·정렬도 WHERE 에 값이 들어 기록에 남기지 않는다
	m.setFocus(fResult)
	_, cmd = m.Update(key("]"))
	drive(m, cmd)
	_, cmd = m.Update(key("s"))
	drive(m, cmd)
	if !strings.Contains(m.cur().tv.SQL(), "ORDER BY") {
		t.Fatalf("정렬로 다시 읽었다: %q", m.cur().tv.SQL())
	}
	for _, e := range m.historyMatches() {
		if strings.Contains(e.SQL, "lnkui_orders") && strings.Contains(e.SQL, "1315") {
			t.Fatalf("기록에 찾은 값: %q", e.SQL)
		}
	}
	if !slices.ContainsFunc(m.historyMatches(), func(e history.Entry) bool { return strings.Contains(e.SQL, "from lnkui.lnkui_orders order by id") }) {
		t.Fatal("편집기 실행은 그대로 기록한다")
	}
}

func TestCLIIntegration_linked_json_은_표_한_개이고_인덱스_없는_열은_실패가_아니다(t *testing.T) {
	dir := integrationCLIProject(t)
	pc := []string{"-p", dir, "-c", "clitest-local"}
	must(t, cliRun(t, cliQuery(pc, "drop schema if exists lnkcli cascade", "--write", "--force")))
	t.Cleanup(func() { _ = cliRun(t, cliQuery(pc, "drop schema if exists lnkcli cascade", "--write", "--force")) })
	for _, sql := range []string{
		"create schema lnkcli",
		"create table lnkcli.lnkcli_items (id int primary key, lnkcli_shop_id text)",
		"create index on lnkcli.lnkcli_items (lnkcli_shop_id)",
		"create table lnkcli.lnkcli_notes (owner_lnkcli_shop_id varchar(20))",
		"insert into lnkcli.lnkcli_items values (1, 'S-1'), (2, 'S-1')",
	} {
		must(t, cliRun(t, cliQuery(pc, sql, "--write")))
	}
	m := cliJSON(t, "linked", "-p", dir, "-c", "clitest-local", "--column", "lnkcli_shop_id", "--value", "S-1")
	if fmt.Sprint(m["cols"]) != "[connection table column rows note]" || m["conn"] != "clitest-local" {
		t.Fatalf("모양: %v", m)
	}
	got := fmt.Sprint(m["rows"])
	if !strings.Contains(got, "[clitest-local lnkcli.lnkcli_items lnkcli_shop_id 2 ]") ||
		!strings.Contains(got, "[clitest-local lnkcli.lnkcli_notes owner_lnkcli_shop_id - 인덱스 없음 · 안 셈]") {
		t.Fatalf("줄: %s", got)
	}
}
