package main

import (
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"
	"time"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/Jeong-Jae-Hun/i-db/internal/complete"
	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/recent"
	"github.com/Jeong-Jae-Hun/i-db/internal/tableview"
)

// ─── R75 최근 기록 ───

// recentModel 은 빈 개인 상태 폴더에서 최근 기록을 읽은 팔레트 모델이다.
func recentModel(t *testing.T) *model {
	t.Helper()
	t.Setenv("XDG_STATE_HOME", t.TempDir())
	m := paletteModel(t)
	m.loadRecent()
	if m.recent == nil {
		t.Fatal("최근 기록을 켠다")
	}
	return m
}

func recentPath(t *testing.T) string {
	t.Helper()
	p, err := recent.Path()
	must(t, err)
	return p
}

// reopen 은 같은 프로젝트를 다시 켠 모델이다(최근 기록은 프로젝트 경로로 가른다).
func reopen(m *model) *model {
	r := newModel(m.proj)
	r.prodLock = false
	r.Update(tea.WindowSizeMsg{Width: 120, Height: 40})
	for i, c := range m.catalogs {
		r.catalogs[i] = c
	}
	r.loadRecent()
	return r
}

// emptyPalette 는 빈 팔레트에 보이는 줄이다.
func emptyPalette(m *model) []string {
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlP})
	list := m.pickerMatches()
	m.Update(tea.KeyMsg{Type: tea.KeyEsc})
	return list
}

// usersPage 는 public.users 테이블 보기 한 페이지다.
func usersPage() *db.Result {
	return &db.Result{Cols: []string{"id", "status"}, Rows: [][]string{{"1", "ACTIVE"}, {"2", "GONE"}}}
}

// openUsers 는 conn 의 public.users 테이블 보기를 지금 탭에 열고 첫 페이지를 붙인다.
func openUsers(m *model, conn int) *queryTab {
	t := m.cur()
	m.openTable(t, conn, db.TableRef{Schema: "public", Table: "users"}, "", tableview.Options{})
	tablePage(m, t)
	return t
}

// tablePage 는 지금 돌고 있는 테이블 보기 읽기의 결과로 usersPage 를 붙인다.
func tablePage(m *model, t *queryTab) {
	m.Update(resultMsg{tab: t.id, seq: t.seq, conn: t.viewConn, kind: rkTable, res: usersPage(), rev: t.grid.Revision()})
}

// apply 는 결과 영역에서 key(w·O)로 입력 줄을 열고 글을 비운 뒤 text 를 쳐 적용하고 다음 페이지를 붙인다.
func apply(m *model, t *queryTab, key0, text string) {
	m.setFocus(fResult)
	m.Update(key(key0))
	*t.tv = t.tv.SetInput("")
	typeText(m, text)
	m.Update(key("enter"))
	tablePage(m, t)
}

func TestRecentPalette_기록이_없으면_R40_순서_그대로이고_고른_동작이_자주_쓴_순으로_맨_위에_온다(t *testing.T) {
	plain := paletteModel(t)
	m := recentModel(t)
	if got, want := emptyPalette(m), emptyPalette(plain); !slices.Equal(got, want) {
		t.Fatalf("기록이 없으면 예전 순서:\n got %v\nwant %v", got[:3], want[:3])
	}
	palette(m, "Ctrl+T · 쿼리 탭")
	palette(m, "Alt+P · 쿼리 탭")
	palette(m, "Alt+P · 쿼리 탭")
	list := emptyPalette(m)
	if !strings.Contains(list[0], "Alt+P") || !strings.Contains(list[1], "Ctrl+T") {
		t.Fatalf("두 번 쓴 Alt+P, 한 번 쓴 Ctrl+T 순: %v", list[:3])
	}
	if n := len(slices.DeleteFunc(slices.Clone(list), func(s string) bool { return !strings.Contains(s, "Ctrl+T · 쿼리 탭") })); n != 1 {
		t.Fatalf("맨 위로 올린 동작은 한 줄만: %d", n)
	}
	if len(m.tabs) != 2 || m.cur().pinned { // 처음 탭 + Ctrl+T, Alt+P 두 번이면 고정이 풀린다
		t.Fatalf("팔레트 동작은 그대로 실행된다: tabs=%d", len(m.tabs))
	}
	// 다시 켜도 남는다
	again := reopen(m)
	if list := emptyPalette(again); !strings.Contains(list[0], "Alt+P") {
		t.Fatalf("파일에 남는다: %v", list[:2])
	}
}

func TestRecentPalette_최근_테이블_뒤에_최근_접속이_오고_고르면_트리에서_그_접속을_연다(t *testing.T) {
	m := recentModel(t)
	m.sess.TouchTable("s", "public", "users", time.Now())
	m.recent.UseConn("s", time.Now())
	list := emptyPalette(m)
	if list[0] != "[stg] x-stg · public.users" || list[1] != "접속 x-stg" {
		t.Fatalf("최근 테이블 다음에 최근 접속: %v", list[:3])
	}
	if _, ok := m.pickerTables[list[2]]; ok || strings.HasPrefix(list[2], "접속 ") {
		t.Fatalf("그 뒤는 동작: %q", list[2])
	}
	cmd := palette(m, "접속 x-stg")
	if cn := m.connNode(1); m.current() != cn || !cn.expanded || m.focus != fTree || m.cur().conn != 1 || cmd == nil {
		t.Fatalf("트리에서 접속을 골라 펼친다: cur=%v focus=%v conn=%d", m.current(), m.focus, m.cur().conn)
	}
	if got := m.recent.TopConns(time.Now(), 1); !slices.Equal(got, []string{"s"}) {
		t.Fatalf("고른 접속을 센다: %v", got)
	}
}

func TestRecentFinder_빈_입력이면_최근_연_테이블이_맨_앞이다(t *testing.T) {
	m := recentModel(t)
	addTable(m, 1)
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlN})
	m.Update(tea.KeyMsg{Type: tea.KeyEsc})
	first := m.finder.View()
	if !strings.Contains(first, "x-prod") {
		t.Fatal("기록이 없으면 접속 순서 그대로 x-prod 가 먼저다")
	}
	m.recent.TouchTable("s", "public", "users", time.Now())
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlN})
	m.Update(key("enter"))
	if tb := m.cur(); tb.tv == nil || tb.title != "users" || tb.viewConn != 1 {
		t.Fatalf("빈 입력의 첫 후보는 최근 연 x-stg public.users: tv=%v title=%q conn=%d status=%q", tb.tv != nil, tb.title, tb.viewConn, m.status)
	}
}

func TestRecentTable_열_폭_정렬_WHERE_를_기억하고_다시_열면_되살린다(t *testing.T) {
	m := recentModel(t)
	tab := openUsers(m, 1)
	apply(m, tab, "w", "status=ACTIVE")
	apply(m, tab, "O", "id desc")
	m.Update(key("l"))
	base := tab.grid.Width("status")
	m.Update(key(">"))
	m.Update(key(">"))
	if tab.grid.Width("status") != base+4 {
		t.Fatalf("열 폭: %d", tab.grid.Width("status"))
	}

	r := reopen(m)
	rt := r.cur()
	r.openTable(rt, 1, db.TableRef{Schema: "public", Table: "users"}, "", tableview.Options{})
	sql := rt.tv.SQL()
	if !strings.Contains(sql, `WHERE "status"::text = 'ACTIVE'`) || !strings.Contains(sql, "ORDER BY id DESC") {
		t.Fatalf("다시 열면 정렬·WHERE: %s", sql)
	}
	tablePage(r, rt)
	if rt.grid.Width("status") != base+4 {
		t.Fatalf("다시 열면 열 폭: %d want %d", rt.grid.Width("status"), base+4)
	}
	if !strings.Contains(r.status, "이전 정렬·조건 적용") {
		t.Fatalf("상태 줄 알림: %q", r.status)
	}
	if !strings.Contains(rt.tv.SQL(), `ORDER BY "id" DESC`) {
		t.Fatalf("열을 알면 열 정렬로 읽어 s·열 머리가 이어 돈다: %s", rt.tv.SQL())
	}
	// WHERE 칸을 비우고 Enter 면 해제되고 그것도 기억한다
	apply(r, rt, "w", "")
	if strings.Contains(rt.tv.SQL(), "WHERE") {
		t.Fatalf("해제: %s", rt.tv.SQL())
	}
	if tb, _ := r.recent.Table("s", "public", "users"); tb.Where != "" || tb.Order == "" {
		t.Fatalf("해제한 WHERE 를 기억: %+v", tb)
	}

	// 외래 키 따라가기처럼 조건을 주고 열면 기억을 쓰지 않는다
	r.newTab()
	r.openTable(r.cur(), 1, db.TableRef{Schema: "public", Table: "users"}, `"id" = 1`, tableview.Options{})
	if strings.Contains(r.cur().tv.SQL(), "ORDER BY") {
		t.Fatalf("조건을 주고 열면 기억을 쓰지 않는다: %s", r.cur().tv.SQL())
	}
}

func TestRecentTable_조건을_주고_연_보기는_사용자가_WHERE_를_적용하기_전까지_정렬_WHERE_를_남기지_않는다(t *testing.T) {
	m := recentModel(t)
	tab := openUsers(m, 1)
	apply(m, tab, "w", "status=ACTIVE")

	// 외래 키 따라가기(R36)처럼 값이 든 조건으로 연 보기
	m.newTab()
	ft := m.cur()
	m.openTable(ft, 1, db.TableRef{Schema: "public", Table: "users"}, `"email" = 'fk@x.com'`, tableview.Options{})
	tablePage(m, ft)
	apply(m, ft, "O", "id desc")
	data, err := os.ReadFile(recentPath(t))
	must(t, err)
	if strings.Contains(string(data), "fk@x.com") {
		t.Fatalf("받은 조건은 파일에 없다: %s", data)
	}
	if tb, _ := m.recent.Table("s", "public", "users"); tb.Where != `"status"::text = 'ACTIVE'` || tb.Order != "" {
		t.Fatalf("이전 기억 그대로: %+v", tb)
	}

	apply(m, ft, "w", "id > 3")
	if tb, _ := m.recent.Table("s", "public", "users"); tb.Where != `"id"::text > '3'` || tb.Order == "" {
		t.Fatalf("사용자가 친 WHERE 를 적용한 뒤부터 남긴다: %+v", tb)
	}
}

func TestRecentTable_prod_는_정렬_WHERE_를_남기지_않고_열_폭만_되살린다(t *testing.T) {
	m := recentModel(t)
	tab := openUsers(m, 0)
	apply(m, tab, "w", "status = 'secret@x.com'")
	apply(m, tab, "O", "email = 'order@x.com' desc")
	m.Update(key(">"))
	data, err := os.ReadFile(recentPath(t))
	must(t, err)
	if strings.Contains(string(data), "secret@x.com") || strings.Contains(string(data), "order@x.com") {
		t.Fatalf("prod 정렬·WHERE 는 파일에 없다: %s", data)
	}
	if !strings.Contains(string(data), `"widths"`) {
		t.Fatalf("열 폭은 남긴다: %s", data)
	}
	m.Update(key("w"))
	before := tab.tv.Input()
	m.Update(tea.KeyMsg{Type: tea.KeyUp})
	if tab.tv.Input() != before || strings.Contains(hintText(m), "이전 조건") {
		t.Fatalf("prod WHERE 입력은 기록이 없다: %q", tab.tv.Input())
	}

	r := reopen(m)
	rt := r.cur()
	r.recent.SetView("p", "public", "users", "id DESC", "", false, time.Now()) // 예전 판이 남긴 prod 정렬도 쓰지 않는다
	r.openTable(rt, 0, db.TableRef{Schema: "public", Table: "users"}, "", tableview.Options{})
	if sql := rt.tv.SQL(); strings.Contains(sql, "WHERE") || strings.Contains(sql, "ORDER BY") {
		t.Fatalf("prod 는 정렬·WHERE 를 되살리지 않는다: %s", sql)
	}
	tablePage(r, rt)
	if strings.Contains(r.status, "이전") {
		t.Fatalf("알림이 없다: %q", r.status)
	}
}

func TestRecentRecall_WHERE_입력의_위아래는_이_테이블의_이전_조건이고_ORDER_BY_입력은_그대로다(t *testing.T) {
	m := recentModel(t)
	tab := openUsers(m, 1)
	apply(m, tab, "w", "id > 1")
	apply(m, tab, "w", "status=GONE")
	m.Update(key("w"))
	*tab.tv = tab.tv.SetInput("draft")
	if !strings.Contains(hintText(m), "↑↓ 이전 조건") {
		t.Fatalf("키 안내: %q", hintText(m))
	}
	up, down := tea.KeyMsg{Type: tea.KeyUp}, tea.KeyMsg{Type: tea.KeyDown}
	steps := []struct {
		k    tea.KeyMsg
		want string
	}{{up, "status=GONE"}, {up, "id > 1"}, {up, "id > 1"}, {down, "status=GONE"}, {down, "draft"}, {down, "draft"}}
	for i, s := range steps {
		m.Update(s.k)
		if got := tab.tv.Input(); got != s.want {
			t.Fatalf("%d번째: %q want %q", i, got, s.want)
		}
	}
	m.Update(up)
	m.Update(key("x")) // 다른 키는 기록 자리를 처음으로 돌린다
	m.Update(up)
	if got := tab.tv.Input(); got != "status=GONE" {
		t.Fatalf("다시 ↑ 는 가장 최근: %q", got)
	}
	m.Update(tea.KeyMsg{Type: tea.KeyEsc})
	m.Update(key("O"))
	m.Update(up)
	if got := tab.tv.Input(); got != "" {
		t.Fatalf("ORDER BY 입력의 ↑ 는 아무것도 하지 않는다: %q", got)
	}
}

func TestRecentRecall_거르기_입력의_위아래는_이전_거르기를_고르고_기록이_없으면_예전처럼_아무것도_안_한다(t *testing.T) {
	plain := testModel(t)
	show(plain, channels())
	plain.setFocus(fResult)
	plain.Update(key("/"))
	typeText(plain, "13")
	plain.Update(tea.KeyMsg{Type: tea.KeyUp})
	if q := plain.cur().grid.Query().Filter; q != "13" || !plain.cur().grid.FilterTyping() {
		t.Fatalf("기록을 끄면 ↑ 는 예전처럼 무시: %q", q)
	}

	m := recentModel(t)
	show(m, channels())
	m.setFocus(fResult)
	for _, f := range []string{"1315", "owner_id=null"} {
		m.Update(tea.KeyMsg{Type: tea.KeyEsc}) // 걸린 거르기를 지우고 새로 친다
		m.Update(key("/"))
		typeText(m, f)
		m.Update(key("enter"))
	}
	m.Update(key("/"))
	if !strings.Contains(hintText(m), "↑↓ 이전 거르기") {
		t.Fatalf("키 안내: %q", hintText(m))
	}
	m.Update(tea.KeyMsg{Type: tea.KeyEsc})
	up, down := tea.KeyMsg{Type: tea.KeyUp}, tea.KeyMsg{Type: tea.KeyDown}
	m.Update(key("/"))
	typeText(m, "y")
	m.Update(up)
	if got := m.cur().grid.Query().Filter; got != "owner_id=null" {
		t.Fatalf("첫 ↑ 는 가장 최근: %q", got)
	}
	m.Update(up)
	if got := m.cur().grid.Query().Filter; got != "1315" {
		t.Fatalf("다음 ↑: %q", got)
	}
	m.Update(down)
	m.Update(down)
	if got := m.cur().grid.Query().Filter; got != "y" || !m.cur().grid.FilterTyping() {
		t.Fatalf("끝까지 ↓ 면 치던 글: %q", got)
	}
	m.Update(key("enter"))
	if got := m.recent.Filters(); !slices.Equal(got, []string{"y", "owner_id=null", "1315"}) {
		t.Fatalf("확정한 거르기를 최근 것부터: %v", got)
	}
}

// filterOnce 는 지금 탭 결과에서 / 로 f 를 쳐 Enter 로 확정한다.
func filterOnce(m *model, f string) {
	m.setFocus(fResult)
	m.Update(tea.KeyMsg{Type: tea.KeyEsc})
	m.Update(key("/"))
	typeText(m, f)
	m.Update(key("enter"))
}

func TestRecentRecall_prod_사용자_찾기_연결된_곳_기록_사본_결과의_거르기는_남기지_않는다(t *testing.T) {
	m := recentModel(t)
	m.Update(resultMsg{tab: m.cur().id, seq: m.cur().seq, conn: 0, res: channels(), rev: m.cur().grid.Revision()})
	filterOnce(m, "prod@x.com")

	ut, _ := m.startMulti("사용자 찾기")
	m.showMulti(multiMsg{tab: ut.id, seq: ut.seq, titles: []string{"a", "b"}, conns: []int{1, 1}, subs: []*db.Result{channels(), channels()}, errs: make([]error, 2)})
	filterOnce(m, "user360@x.com")
	ut.view = 1 // 둘째 결과도 같다
	filterOnce(m, "sub@x.com")

	lt := m.newTab()
	show(m, channels())
	lt.linked = &linkedView{}
	filterOnce(m, "linked@x.com")

	st := m.newTab()
	show(m, channels())
	st.snap = &snapTab{}
	filterOnce(m, "snap@x.com")
	if got := m.recent.Filters(); len(got) != 0 {
		t.Fatalf("값이 든 결과의 거르기는 남기지 않는다: %v", got)
	}

	m.newTab()
	show(m, channels())
	filterOnce(m, "1315")
	if got := m.recent.Filters(); !slices.Equal(got, []string{"1315"}) {
		t.Fatalf("prod 가 아닌 보통 결과는 남긴다: %v", got)
	}
}

func TestRecentRecall_WHERE_입력의_위아래는_기록을_옮기지_못하면_키를_넘긴다(t *testing.T) {
	m := recentModel(t)
	tab := openUsers(m, 1)
	m.setFocus(fResult)
	m.Update(key("w"))
	if m.whereTypingKey(tab, "up") || m.whereTypingKey(tab, "down") {
		t.Fatal("기록이 없으면 ↑↓ 를 먹지 않는다")
	}
	m.Update(tea.KeyMsg{Type: tea.KeyEsc})
	apply(m, tab, "w", "id > 1")
	m.Update(key("w"))
	if !m.whereTypingKey(tab, "up") {
		t.Fatal("기록을 고르면 먹는다")
	}
	if m.whereTypingKey(tab, "up") {
		t.Fatal("더 오래된 기록이 없으면 넘긴다")
	}
	if !m.whereTypingKey(tab, "down") || m.whereTypingKey(tab, "down") {
		t.Fatal("치던 글로 돌아온 뒤 ↓ 는 넘긴다")
	}
}

func TestRecentRecall_초점을_옮겨_입력이_닫히면_기록_자리를_버린다(t *testing.T) {
	m := recentModel(t)
	show(m, channels())
	for _, f := range []string{"1315", "owner_id=null"} {
		filterOnce(m, f)
	}
	m.Update(tea.KeyMsg{Type: tea.KeyEsc})
	m.Update(key("/"))
	m.Update(tea.KeyMsg{Type: tea.KeyUp})
	m.Update(tea.KeyMsg{Type: tea.KeyUp})
	if got := m.cur().grid.Query().Filter; got != "1315" {
		t.Fatalf("둘째 ↑: %q", got)
	}
	// 키가 아닌 길(트리 클릭)로 결과 영역을 떠나 거르기 입력이 닫힌다
	m.Update(press(2, 3))
	if m.focus != fTree || m.cur().grid.FilterTyping() {
		t.Fatalf("트리로 초점: focus=%v typing=%v", m.focus, m.cur().grid.FilterTyping())
	}
	m.setFocus(fResult)
	m.Update(tea.KeyMsg{Type: tea.KeyEsc})
	m.Update(key("/"))
	m.Update(tea.KeyMsg{Type: tea.KeyUp})
	if got := m.cur().grid.Query().Filter; got != "owner_id=null" {
		t.Fatalf("다시 연 입력의 첫 ↑ 는 가장 최근: %q", got)
	}
	m.Update(tea.KeyMsg{Type: tea.KeyDown})
	if got := m.cur().grid.Query().Filter; got != "" {
		t.Fatalf("치던 글은 새 입력의 것(빈 글): %q", got)
	}
}

func TestRecentTree_그룹은_켤_때_접고_접속_아래는_처음_펼칠_때_되살리며_켤_때_접속하지_않는다(t *testing.T) {
	m := recentModel(t)
	group := m.roots[0].label
	m.recent.SetTree([]string{"s:p:public", "s:s:public", "s:gone:x"}, []string{"g:" + group})
	must(t, m.recent.Save())

	r := reopen(m)
	if r.roots[0].expanded {
		t.Fatal("접은 그룹은 켤 때 접는다")
	}
	for i := range r.proj.Conns {
		if cn := r.connNode(i); cn.expanded || cn.loaded || len(cn.children) > 0 {
			t.Fatalf("켤 때 어느 접속도(prod 포함) 펼치거나 읽지 않는다: %s", cn.label)
		}
	}
	if r.boot != nil {
		t.Fatal("켤 때 돌릴 명령을 더하지 않는다")
	}

	// 사용자가 x-stg 를 펼쳐 스키마 목록이 오면 기억한 public 만 다시 펼친다(스키마 읽기는 비동기)
	cn := r.connNode(1)
	r.expand(cn)
	pub := &node{kind: nSchema, label: "public", conn: 1, parent: cn}
	other := &node{kind: nSchema, label: "other", conn: 1, parent: cn}
	_, cmd := r.Update(childrenMsg{n: cn, gen: cn.gen, children: []*node{pub, other}})
	if !pub.expanded || other.expanded || cmd == nil {
		t.Fatalf("기억한 스키마만 펼치고 읽는다: public=%v other=%v cmd=%v", pub.expanded, other.expanded, cmd != nil)
	}
	// 이번 실행에서 접은 뒤 새로고침해도 다시 펼치지 않는다
	pub2 := &node{kind: nSchema, label: "public", conn: 1, parent: cn}
	cn.gen++
	r.Update(childrenMsg{n: cn, gen: cn.gen, children: []*node{pub2}})
	if pub2.expanded {
		t.Fatal("접속마다 한 번만 되살린다")
	}

	// 끝낼 때: 읽은 접속은 지금 펼침, 읽지 않은 x-prod 는 예전 기억을 두고 .idea 에 없는 접속 기억은 버린다
	pub2.expanded, pub2.loaded = true, true
	r.roots[0].expanded = true
	r.saveRecentOnQuit()
	again, err := recent.Load(recentPath(t), r.proj.Dir)
	must(t, err)
	open, closed := again.Tree()
	slices.Sort(open)
	if !slices.Equal(open, []string{"s:p:public", "s:s:public"}) || len(closed) != 0 {
		t.Fatalf("끝낼 때 남긴 펼침: open=%v closed=%v", open, closed)
	}
}

func TestRecentTree_저장소_노드는_기억하지_않아_다시_펼쳐도_요금_조회_목록을_더_부르지_않는다(t *testing.T) {
	noBilledEnv(t)
	t.Setenv("XDG_STATE_HOME", t.TempDir())
	f := newFake()
	registerFake(t, f)
	m := sourceModel(t) // 접속 3 은 DynamoDB(R72 요금 대상)
	m.loadRecent()
	cn := expandConn(t, m, 3)
	drive(m, m.activate(childNamed(cn, "logs-2026"), true))
	m.saveRecentOnQuit()
	if open, _ := m.recent.Tree(); slices.ContainsFunc(open, func(id string) bool { return strings.Contains(id, "s4") }) {
		t.Fatalf("저장소 노드 펼침은 기억하지 않는다: %v", open)
	}

	// 예전·손으로 고친 파일에 저장소 아래 id 가 있어도 되살리며 목록을 부르지 않는다
	r := reopen(m)
	t.Cleanup(func() { r.stop(); r.sources.close() })
	r.recent.SetTree([]string{"f:s4:logs-2026", "s:s4:logs-2026", "q:s4"}, nil)
	f.mu.Lock()
	f.listed = nil
	f.mu.Unlock()
	expandConn(t, r, 3)
	f.mu.Lock()
	defer f.mu.Unlock()
	if !slices.Equal(f.listed, []string{""}) {
		t.Fatalf("사용자가 펼친 접속 목록만 부른다: %q", f.listed)
	}
}

func TestRecent_DB_RECENT_0_이면_읽지도_쓰지도_않는다(t *testing.T) {
	t.Setenv("XDG_STATE_HOME", t.TempDir())
	t.Setenv("DB_RECENT", "0")
	m := paletteModel(t)
	m.loadRecent()
	if m.recent != nil {
		t.Fatal("끈다")
	}
	palette(m, "Ctrl+T · 쿼리 탭")
	openUsers(m, 1)
	m.saveRecentOnQuit()
	if _, err := os.Stat(recentPath(t)); !os.IsNotExist(err) {
		t.Fatalf("파일을 만들지 않는다: %v", err)
	}

	// 이미 있는 기록도 읽지 않는다
	must(t, os.MkdirAll(filepath.Dir(recentPath(t)), 0o700))
	s, _ := recent.Load(recentPath(t), m.proj.Dir)
	s.UseAction(actionKey("쿼리 탭", "ctrl+t"), time.Now())
	must(t, s.Save())
	r := reopen(m)
	if r.recent != nil || strings.Contains(emptyPalette(r)[0], "Ctrl+T") {
		t.Fatal("기록이 있어도 쓰지 않는다")
	}
}

func TestRecent_깨진_파일은_빈_기록으로_시작하고_0600_으로_다시_쓴다(t *testing.T) {
	t.Setenv("XDG_STATE_HOME", t.TempDir())
	path := recentPath(t)
	must(t, os.MkdirAll(filepath.Dir(path), 0o700))
	must(t, os.WriteFile(path, []byte("{broken"), 0o644))
	m := paletteModel(t)
	m.loadRecent()
	if m.recent == nil || !strings.Contains(m.status, "최근 기록을 읽지 못해 새로 시작") {
		t.Fatalf("빈 기록으로 시작하고 알린다: %q", m.status)
	}
	palette(m, "Ctrl+T · 쿼리 탭")
	st, err := os.Stat(path)
	must(t, err)
	if st.Mode().Perm() != 0o600 {
		t.Fatalf("권한: %v", st.Mode().Perm())
	}
	if _, err := recent.Load(path, m.proj.Dir); err != nil {
		t.Fatalf("다시 읽힌다: %v", err)
	}
}

func TestRecent_쓰기에_실패하면_이번_실행에서_끄고_한_번_알린다(t *testing.T) {
	m := recentModel(t)
	must(t, os.MkdirAll(recentPath(t), 0o700)) // 파일 자리에 폴더가 있어 바꿔 끼우기가 실패한다
	palette(m, "Ctrl+T · 쿼리 탭")
	if m.recent != nil || !strings.Contains(m.status, "최근 기록 저장 실패, 이번 실행에서 끔") {
		t.Fatalf("끄고 알린다: %q", m.status)
	}
	palette(m, "Ctrl+T · 쿼리 탭")
	if len(m.tabs) != 3 {
		t.Fatalf("동작은 계속된다: tabs=%d", len(m.tabs))
	}
}

func TestRecentPalette_자주_쓴_동작은_catalog_테이블보다도_먼저다(t *testing.T) {
	m := recentModel(t)
	m.catalogs[0] = &complete.Catalog{Schemas: []complete.Schema{{Name: "public", Tables: []complete.Table{{Name: "tabs"}}}}}
	palette(m, "Ctrl+T · 쿼리 탭")
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlP})
	typeText(m, "탭")
	if got := m.pickerMatches(); !strings.Contains(got[0], "Ctrl+T") {
		t.Fatalf("거를 때도 자주 쓴 동작이 먼저: %v", got[:2])
	}
}
