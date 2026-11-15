package main

import (
	"bytes"
	"context"
	"encoding/base64"
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
	"github.com/Jeong-Jae-Hun/i-db/internal/redisview"
	"github.com/Jeong-Jae-Hun/i-db/internal/sqlfmt"
)

// drive 는 명령을 실행해 나온 메시지를 모델에 다시 넣는다. 시계(tickMsg)는 버려 끝없이 돌지 않게 한다.
func drive(m *model, cmd tea.Cmd) {
	queue := []tea.Cmd{cmd}
	for len(queue) > 0 {
		c := queue[0]
		queue = queue[1:]
		if c == nil {
			continue
		}
		switch msg := c().(type) {
		case nil, tickMsg:
		case tea.BatchMsg:
			queue = append(queue, msg...)
		default:
			_, next := m.Update(msg)
			queue = append(queue, next)
		}
	}
}

// pgModel 은 실제 PostgreSQL 하나를 접속 0 으로 둔 모델이다. 이름에 prod 를 넣으면 prod 확인을 탄다.
func pgModel(t *testing.T, name string) *model {
	t.Helper()
	c := testPGConn(t)
	c.Name, c.UUID = name, name
	m := newModel(&db.Project{Dir: t.TempDir(), Mapping: map[string]string{}, Conns: []db.Conn{c}})
	m.prodLock = false // 잠금(R54) 시험은 다시 켠다
	m.Update(tea.WindowSizeMsg{Width: 140, Height: 44})
	t.Cleanup(func() {
		m.stop()
		m.clients.Close()
	})
	txTick = time.Millisecond
	return m
}

// pgTest 는 화면을 거치지 않고 접속 0 의 별도 클라이언트로 문장을 돌린다(다른 세션에서 보이는지 확인용).
type pgTest struct {
	t *testing.T
	m *model
}

func (p pgTest) sql(q string) *db.Result {
	p.t.Helper()
	cs := db.NewClients()
	defer cs.Close()
	pg, err := cs.PG(context.Background(), p.m.conn(0))
	must(p.t, err)
	var res *db.Result
	for _, st := range db.SplitStatements(q) {
		res, err = pg.Run(context.Background(), st)
		must(p.t, err)
	}
	return res
}

func editable() *db.Result {
	return &db.Result{Cols: []string{"id", "name"}, Rows: [][]string{{"1", "a"}, {"2", "b"}},
		Origin: &db.Origin{Table: db.TableRef{Schema: "public", Table: "t"}, PK: []string{"id"}, Cols: []string{"id", "name"}}}
}

// editCell 은 결과 표 커서 칸을 text 로 고친다.
func editCell(m *model, text string) {
	m.Update(tea.KeyMsg{Type: tea.KeyF2})
	for range 10 {
		m.Update(tea.KeyMsg{Type: tea.KeyBackspace})
	}
	typeText(m, text)
	m.Update(key("enter"))
}

func TestSubmit_미리보기와_prod_확인_뒤_반영하고_다시_읽는다(t *testing.T) {
	m := pgModel(t, "it-prod")
	it := pgTest{t, m}
	it.sql("drop table if exists public.v3_edit; create table public.v3_edit (id int primary key, name text)")
	it.sql("insert into public.v3_edit values (1, 'a'), (2, 'b')")
	m.cur().conn = 0
	drive(m, m.run("select * from public.v3_edit order by id"))
	tab := m.cur()
	if tab.res == nil || tab.res.Origin == nil {
		t.Fatalf("PK 가 있는 SELECT 는 편집할 수 있어야 한다: %+v", tab.res)
	}
	m.setFocus(fResult)
	m.Update(key("l"))
	editCell(m, "z")
	if tab.grid.Pending() != 1 {
		t.Fatalf("pending=%d", tab.grid.Pending())
	}
	m.Update(key("ctrl+s"))
	view := m.View()
	if m.overlay != oConfirm || !strings.Contains(view, "prod(it-prod)에 1건 제출") || !strings.Contains(view, `UPDATE "public"."v3_edit"`) {
		t.Fatalf("prod 제출은 미리 보기와 함께 한 번 묻는다:\n%s", view)
	}
	m.Update(key("j"))
	if m.overlay != oConfirm {
		t.Fatal("미리 보기 넘기기는 취소가 아니다")
	}
	_, cmd := m.Update(key("y"))
	drive(m, cmd)
	if tab.grid.Pending() != 0 || tab.res == nil || tab.res.Rows[0][1] != "z" {
		t.Fatalf("반영 뒤 바꿈을 비우고 다시 읽는다: pending=%d res=%+v status=%q", tab.grid.Pending(), tab.res, m.status)
	}
	if got := it.sql("select name from public.v3_edit where id = 1"); got.Rows[0][0] != "z" {
		t.Fatalf("DB 에 반영: %v", got.Rows)
	}
}

func TestSubmit_보류_중인_편집이_있으면_새_결과로_덮기_전에_묻는다(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 1
	show(m, editable())
	m.setFocus(fResult)
	m.Update(key("l"))
	editCell(m, "q")
	if m.cur().grid.Pending() != 1 {
		t.Fatal("편집 하나")
	}
	if cmd := m.run("select 1"); cmd != nil || m.overlay != oConfirm || !strings.Contains(m.View(), "변경 1건을 버릴까요") {
		t.Fatalf("버릴지 먼저 묻는다: overlay=%v", m.overlay)
	}
	m.Update(key("n"))
	if m.cur().grid.Pending() != 1 || m.cur().running != nil {
		t.Fatal("N 이면 실행하지 않고 편집을 남긴다")
	}
	m.run("select 1")
	if _, cmd := m.Update(key("y")); cmd == nil || m.cur().running == nil {
		t.Fatal("y 면 실행한다(stg 는 더 묻지 않음)")
	}

	// ctrl+s 는 편집이 없으면 파일 저장으로 간다
	show(m, editable())
	m.setFocus(fResult)
	m.Update(key("ctrl+s"))
	if m.overlay != oNone || !strings.Contains(m.status, "연 파일이 없어") {
		t.Fatalf("편집이 없으면 저장: %q", m.status)
	}
}

func TestCopyAs_i는_INSERT_문을_OSC52로_복사한다(t *testing.T) {
	var buf bytes.Buffer
	old := clipboardOut
	clipboardOut = &buf
	defer func() { clipboardOut = old }()
	m := testModel(t)
	show(m, editable())
	m.setFocus(fResult)
	_, cmd := m.Update(key("i"))
	must(t, nilMsg(cmd))
	raw := buf.String()
	enc := raw[strings.LastIndex(raw, ";")+1 : strings.Index(raw, "\a")]
	dec, err := base64.StdEncoding.DecodeString(enc)
	must(t, err)
	if !strings.HasPrefix(string(dec), `INSERT INTO "public"."t"`) || m.status != "insert 문 복사함" {
		t.Fatalf("복사: %q status=%q", dec, m.status)
	}
}

func TestTableView_페이지_개수_머리_클릭_정렬(t *testing.T) {
	m := pgModel(t, "it-local")
	it := pgTest{t, m}
	it.sql("drop table if exists public.v3_page; create table public.v3_page (id int primary key, name text)")
	it.sql("insert into public.v3_page select g, 'n' || (1000 - g) from generate_series(1, 450) g")
	cn := m.connNode(0)
	schema := &node{kind: nSchema, label: "public", conn: 0, parent: cn, loaded: true, expanded: true}
	schema.children = []*node{{kind: nTable, label: "v3_page", schema: "public", conn: 0, parent: schema}}
	cn.children, cn.loaded, cn.expanded = []*node{schema}, true, true
	m.rebuild()
	m.selectNode(schema.children[0])
	_, cmd := m.Update(key("enter"))
	drive(m, cmd)
	tab := m.cur()
	if tab.tv == nil || tab.res == nil || len(tab.res.Rows) != 200 || tab.res.Origin == nil {
		t.Fatalf("첫 페이지 200행, 편집 가능: tv=%v res=%v status=%q", tab.tv != nil, tab.res != nil, m.status)
	}
	m.setFocus(fResult)
	for range 2 {
		_, cmd = m.Update(key("]"))
		drive(m, cmd)
	}
	if len(tab.res.Rows) != 50 || !strings.Contains(tab.tv.View(200), "450") {
		t.Fatalf("짧은 페이지로 끝을 안다: rows=%d view=%q", len(tab.res.Rows), tab.tv.View(200))
	}
	_, cmd = m.Update(key("#"))
	drive(m, cmd)
	if m.status != "전체 450행" {
		t.Fatalf("정확한 개수: %q", m.status)
	}

	l := m.layout()
	lines := m.resultLines(tab, l.rightW)
	x := -1
	for i := 0; i < l.rightW && x < 0; i++ {
		if col, ok := tab.grid.HeaderAt(i, 0); ok && col == "name" {
			x = i
		}
	}
	if x < 0 {
		t.Fatal("name 머리를 찾지 못함")
	}
	_, cmd = m.Update(press(l.treeW+2+1+x, l.edH+5+len(lines)))
	drive(m, cmd)
	if !strings.Contains(tab.tv.SQL(), `ORDER BY "name" ASC`) || !strings.HasSuffix(tab.tv.SQL(), "OFFSET 0") || tab.res.Rows[0][1] != "n550" {
		t.Fatalf("머리 클릭은 서버 정렬, 첫 페이지로: %q first=%v", tab.tv.SQL(), tab.res.Rows[0])
	}

	// 편집 중 페이지를 넘기려다 거절하면 페이지가 그대로다
	m.Update(key("l"))
	editCell(m, "x")
	m.Update(key("]"))
	m.Update(key("n"))
	if !strings.HasSuffix(tab.tv.SQL(), "OFFSET 0") || tab.grid.Pending() != 1 {
		t.Fatalf("거절하면 되돌린다: %q", tab.tv.SQL())
	}
}

func TestTx_Manual은_BEGIN을_먼저_보내고_F10_F11과_종료_확인으로_끝낸다(t *testing.T) {
	m := pgModel(t, "it-local")
	it := pgTest{t, m}
	it.sql("drop table if exists public.v3_tx; create table public.v3_tx (a int)")
	m.cur().conn = 0
	m.Update(tea.KeyMsg{Type: tea.KeyF9})
	if !m.manual[0] || !strings.Contains(m.View(), "Manual") {
		t.Fatal("F9 는 Manual")
	}
	drive(m, m.run("insert into public.v3_tx values (1)"))
	if !m.inTx[0] || !strings.Contains(m.View(), "Manual · tx 00:0") {
		t.Fatalf("Manual 은 BEGIN 을 먼저 보낸다: inTx=%v status=%q", m.inTx[0], m.status)
	}
	if n := it.sql("select count(*) from public.v3_tx").Rows[0][0]; n != "0" {
		t.Fatalf("커밋 전에는 다른 세션에 안 보인다: %s", n)
	}
	_, cmd := m.Update(tea.KeyMsg{Type: tea.KeyF11})
	drive(m, cmd)
	if m.inTx[0] || it.sql("select count(*) from public.v3_tx").Rows[0][0] != "0" {
		t.Fatal("F11 은 ROLLBACK")
	}
	drive(m, m.run("insert into public.v3_tx values (2)"))
	_, cmd = m.Update(tea.KeyMsg{Type: tea.KeyF10})
	drive(m, cmd)
	if m.inTx[0] || it.sql("select count(*) from public.v3_tx").Rows[0][0] != "1" {
		t.Fatalf("F10 은 COMMIT: %q", m.status)
	}

	drive(m, m.run("insert into public.v3_tx values (3)"))
	m.setFocus(fTree)
	if _, cmd := m.Update(key("q")); isQuit(cmd) || m.overlay != oTx || !strings.Contains(m.View(), "열린 트랜잭션") {
		t.Fatal("열린 트랜잭션이 있으면 종료 전에 묻는다")
	}
	m.Update(tea.KeyMsg{Type: tea.KeyEsc})
	if m.overlay != oNone || !m.inTx[0] {
		t.Fatal("esc 는 취소")
	}
	m.Update(key("q"))
	_, cmd = m.Update(key("r"))
	msg := cmd()
	if done, ok := msg.(txDoneMsg); !ok || done.err != nil {
		t.Fatalf("ROLLBACK 결과: %#v", msg)
	}
	_, cmd = m.Update(msg)
	if !isQuit(cmd) || m.inTx[0] || it.sql("select count(*) from public.v3_tx").Rows[0][0] != "1" {
		t.Fatal("ROLLBACK 뒤 종료")
	}
}

func TestTx_prod_COMMIT은_묻고_탭_닫기는_열린_트랜잭션을_먼저_묻는다(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 0
	m.inTx[0] = true
	m.Update(tea.KeyMsg{Type: tea.KeyF10})
	if m.overlay != oConfirm || !strings.Contains(m.View(), "prod(x-prod)에 쓰기 실행") {
		t.Fatal("prod COMMIT 은 확인")
	}
	m.Update(key("n"))
	if _, cmd := m.Update(tea.KeyMsg{Type: tea.KeyF11}); cmd == nil || m.overlay != oNone {
		t.Fatal("ROLLBACK 은 묻지 않는다")
	}
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlW})
	if m.overlay != oTx || !strings.Contains(m.View(), "열린 트랜잭션(prod 포함)") {
		t.Fatal("탭 닫기 전에 묻는다")
	}
	m.Update(tea.KeyMsg{Type: tea.KeyEsc})
	m.newTab() // 같은 접속을 쓰는 탭이 남으면 그 탭에서 이어 끝낼 수 있어 묻지 않는다
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlW})
	if m.overlay != oNone || len(m.tabs) != 1 {
		t.Fatalf("overlay=%v tabs=%d", m.overlay, len(m.tabs))
	}
	m.inTx[0] = false
	m.Update(tea.KeyMsg{Type: tea.KeyF10})
	if m.status != "열린 트랜잭션 없음" {
		t.Fatalf("status=%q", m.status)
	}
}

func TestPin_결과를_고정하고_여러_문장_결과는_문장별_탭이다(t *testing.T) {
	m := testModel(t)
	m.setFocus(fResult)
	show(m, &db.Result{Cols: []string{"a"}, Rows: [][]string{{"first"}}})
	m.Update(key("p"))
	show(m, &db.Result{Cols: []string{"a"}, Rows: [][]string{{"second"}}})
	tab := m.cur()
	if len(tab.subs) != 1 || !tab.subs[0].pinned || gridCell(t, m) != "second" {
		t.Fatalf("고정은 다음 실행에도 남는다: %+v", tab.subs)
	}
	m.Update(key("}"))
	if tab.view != 0 || gridCell(t, m) != "first" || !strings.Contains(m.View(), "● 고정 1") {
		t.Fatalf("} 는 고정 결과로: view=%d", tab.view)
	}
	m.Update(key("}"))
	if tab.view != -1 || gridCell(t, m) != "second" {
		t.Fatal("한 바퀴 돌면 마지막 결과")
	}
	for range maxPins {
		m.Update(key("p"))
	}
	if len(tab.subs) != maxPins || !strings.Contains(m.status, "5개까지") {
		t.Fatalf("고정은 5개까지: %d %q", len(tab.subs), m.status)
	}
	m.Update(key("{"))
	m.Update(key("P"))
	if len(tab.subs) != maxPins-1 || tab.view != -1 {
		t.Fatal("P 는 고른 고정을 푼다")
	}

	all := []*db.Result{{Cols: []string{"a"}, Rows: [][]string{{"s1"}}}, {Text: "UPDATE 2"}, {Cols: []string{"a"}, Rows: [][]string{{"s3"}}}}
	m.Update(resultMsg{tab: tab.id, seq: tab.seq, conn: 1, stmts: 3, res: all[2], all: all})
	if len(tab.subs) != maxPins-1+3 || tab.subs[len(tab.subs)-3].title != "문장 1" || tab.subs[len(tab.subs)-1].pinned {
		t.Fatalf("문장별 결과 탭: %+v", tab.subs)
	}
	show(m, &db.Result{Cols: []string{"a"}, Rows: [][]string{{"next"}}})
	if len(tab.subs) != maxPins-1 {
		t.Fatal("다음 실행은 고정하지 않은 문장 탭을 지운다")
	}
}

func TestImport_CSV_머리를_열_이름에_맞추고_빈_칸은_NULL이다(t *testing.T) {
	p := filepath.Join(t.TempDir(), "a.csv")
	must(t, os.WriteFile(p, []byte("\ufeffID,extra,name\n1,x,\"a,b\"\n2,y,\n3,z,c\n4,z,d\n5,z,e\n6,z,f\n"), 0o600))
	plan, err := readCSVPlan(p, []string{"id", "name", "created"})
	must(t, err)
	if strings.Join(plan.columns, ",") != "id,name" || strings.Join(plan.dropped, ",") != "extra" || len(plan.rows) != 6 {
		t.Fatalf("매핑: %+v", plan)
	}
	if *plan.rows[0][1] != "a,b" || plan.rows[1][1] != nil {
		t.Fatal("따옴표 값과 빈 칸 NULL")
	}
	text := strings.Join(plan.preview, "\n")
	if !strings.Contains(text, "extra → (버림)") || !strings.Contains(text, "ID → id") || !strings.Contains(text, "5 | e") || strings.Contains(text, "6 | f") {
		t.Fatalf("미리 보기는 매핑과 앞 5행:\n%s", text)
	}
	must(t, os.WriteFile(p, []byte("nope\n1\n"), 0o600))
	if _, err := readCSVPlan(p, []string{"id"}); err == nil {
		t.Fatal("맞는 열이 없으면 오류")
	}
}

func TestImport_확인_뒤_COPY_로_넣고_실패하면_한_행도_남지_않는다(t *testing.T) {
	m := pgModel(t, "it-local")
	it := pgTest{t, m}
	it.sql("drop table if exists public.v3_imp; create table public.v3_imp (id int primary key, name text not null)")
	cn := m.connNode(0)
	schema := &node{kind: nSchema, label: "public", conn: 0, parent: cn, loaded: true, expanded: true}
	tn := &node{kind: nTable, label: "v3_imp", schema: "public", conn: 0, parent: schema}
	schema.children = []*node{tn}
	cn.children, cn.loaded, cn.expanded = []*node{schema}, true, true
	m.catalogs[0] = &complete.Catalog{Schemas: []complete.Schema{{Name: "public", Tables: []complete.Table{{Name: "v3_imp",
		Columns: []complete.Column{{Name: "id"}, {Name: "name"}}}}}}}
	m.rebuild()
	m.selectNode(tn)
	bad := filepath.Join(m.proj.Dir, "bad.csv")
	must(t, os.WriteFile(bad, []byte("id,name\n1,a\n2,\n"), 0o600)) // 두 번째 행 name NULL 은 NOT NULL 위반
	good := filepath.Join(m.proj.Dir, "good.csv")
	must(t, os.WriteFile(good, []byte("id,name\n1,a\n2,b\n"), 0o600))

	for _, f := range []string{bad, good} {
		m.Update(key("I"))
		if m.overlay != oImport || !strings.HasPrefix(m.input.Value(), m.proj.Dir) {
			t.Fatalf("I 는 프로젝트 폴더에서 경로를 묻는다: %q", m.input.Value())
		}
		m.input.SetValue(f)
		m.Update(key("enter"))
		if m.overlay != oConfirm || !strings.Contains(m.View(), `"public"."v3_imp" 에 2행 가져오기`) || !strings.Contains(m.View(), "name → name") {
			t.Fatalf("매핑 미리 보기와 확인:\n%s", m.View())
		}
		_, cmd := m.Update(key("y"))
		drive(m, cmd)
		m.setFocus(fTree)
		if f == bad {
			if !strings.Contains(m.status, "실패") || it.sql("select count(*) from public.v3_imp").Rows[0][0] != "0" {
				t.Fatalf("실패하면 한 행도 남지 않는다: %q", m.status)
			}
		}
	}
	if m.status != "2행 가져옴" || it.sql("select count(*) from public.v3_imp").Rows[0][0] != "2" {
		t.Fatalf("가져온 개수: %q", m.status)
	}
}

func TestRedis_키를_이름공간_트리로_묶는다(t *testing.T) {
	parent := &node{kind: nConn, conn: 3}
	parent.children = redisNodes(redisview.BuildTree([]string{"user:1", "user:2", "solo", ""}), parent)
	labels := []string{}
	for _, n := range parent.children {
		labels = append(labels, n.label)
	}
	if strings.Join(labels, "|") != `user (2)|""|solo` || parent.children[0].kind != nFolder || !parent.children[0].loaded {
		t.Fatalf("폴더 먼저, 빈 키는 \"\": %v", labels)
	}
	if got := strings.Join(redisKeys(parent), ","); got != "user:1,user:2,,solo" {
		t.Fatalf("읽힌 키: %q", got)
	}
	if k := findKey(parent, "user:2"); k == nil || k.parent != parent.children[0] || k.conn != 3 {
		t.Fatal("폴더 안 키 찾기")
	}
}

func TestRedis_값_편집기가_만든_명령을_실행하고_RENAMENX_0은_알린다(t *testing.T) {
	addr := os.Getenv("DBTUI_TEST_REDIS_ADDR")
	if addr == "" {
		t.Skip("DBTUI_TEST_REDIS_ADDR not set")
	}
	cl := redis.NewClient(&redis.Options{Addr: addr})
	defer cl.Close()
	ctx := context.Background()
	must(t, cl.Del(ctx, "it:v3ui:a", "it:v3ui:b").Err())
	defer cl.Del(ctx, "it:v3ui:a", "it:v3ui:b")
	must(t, cl.Set(ctx, "it:v3ui:a", strings.Repeat("v", 70_000), 0).Err())
	must(t, cl.Set(ctx, "it:v3ui:b", "other", 0).Err())

	m := testModel(t)
	host, port, _ := strings.Cut(addr, ":")
	p, err := strconv.Atoi(port)
	must(t, err)
	m.proj.Conns = append(m.proj.Conns, db.Conn{Name: "r-local", UUID: "rl", Kind: db.KindRedis, Host: host, Port: p, DB: "0"})
	t.Cleanup(func() { m.stop(); m.clients.Close() })
	rn := &node{kind: nConn, label: "r-local", conn: 2, parent: m.roots[0]}
	m.roots[0].children = append(m.roots[0].children, rn)
	m.patterns[2] = "it:v3ui:*"
	m.rebuild()
	m.selectNode(rn)
	drive(m, m.activate(rn, false))
	a := findKey(rn, "it:v3ui:a")
	if a == nil || rn.children[0].kind != nFolder {
		t.Fatalf("이름공간 트리: %+v", rn.children)
	}
	drive(m, m.activate(a, false))
	tab := m.cur()
	if tab.redis == nil || tab.redisKey != "it:v3ui:a" {
		t.Fatalf("키를 고르면 값 편집기: status=%q", m.status)
	}
	m.setFocus(fResult)
	if !strings.Contains(m.View(), "string") {
		t.Fatal("값 편집기를 결과 영역에 그린다")
	}

	m.Update(key("r"))
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlU})
	typeText(m, "it:v3ui:b")
	_, cmd := m.Update(key("enter"))
	drive(m, cmd)
	if m.status != "같은 이름의 키가 있어 바꾸지 않음" || tab.redisKey != "it:v3ui:a" || cl.Exists(ctx, "it:v3ui:a").Val() != 1 {
		t.Fatalf("RENAMENX 0: %q key=%q", m.status, tab.redisKey)
	}

	m.Update(key("e"))
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlU})
	typeText(m, "new")
	_, cmd = m.Update(key("enter"))
	drive(m, cmd)
	if v := cl.Get(ctx, "it:v3ui:a").Val(); v != "new" || m.status != "반영함" {
		t.Fatalf("SET 실행: %q status=%q", v, m.status)
	}
	m.Update(key("D"))
	_, cmd = m.Update(key("y"))
	drive(m, cmd)
	if tab.redis != nil || cl.Exists(ctx, "it:v3ui:a").Val() != 0 || findKey(rn, "it:v3ui:a") != nil {
		t.Fatalf("키를 지우면 편집기를 닫고 트리를 다시 읽는다: %q", m.status)
	}
}

func TestOps_pid_결과에서_K_X는_늘_묻고_prod는_이름을_보인다(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 0
	m.Update(resultMsg{tab: m.cur().id, seq: m.cur().seq, conn: 0, res: &db.Result{Cols: []string{"pid", "state"}, Rows: [][]string{{"123", "active"}}}})
	m.setFocus(fResult)
	m.Update(key("K"))
	if m.overlay != oConfirm || !strings.Contains(m.View(), "prod(x-prod)에 pid 123 쿼리 취소: y/N") {
		t.Fatalf("prod 취소 확인:\n%s", m.View())
	}
	m.Update(key("n"))
	m.cur().conn = 1
	show(m, &db.Result{Cols: []string{"pid", "state"}, Rows: [][]string{{"123", "active"}}})
	m.Update(key("X"))
	if m.overlay != oConfirm || !strings.Contains(m.View(), " pid 123 연결 종료: y/N") {
		t.Fatal("stg 도 종료는 묻는다")
	}
	if _, cmd := m.Update(key("y")); cmd == nil || m.cur().running == nil {
		t.Fatal("y 면 신호를 보낸다")
	}

	show(m, &db.Result{Cols: []string{"a"}, Rows: [][]string{{"1"}}})
	if m.Update(key("K")); m.overlay != oNone || !strings.Contains(m.status, "pid 열") {
		t.Fatal("pid 열이 없으면 안내만")
	}
	m.setFocus(fTree)
	m.selectNode(m.connNode(1))
	if _, cmd := m.Update(key("A")); cmd == nil || m.cur().running == nil {
		t.Fatal("접속 줄의 A 는 활동을 읽는다")
	}
}

func TestPlan_실행_계획은_나무로_열고_t는_JSON_esc는_닫는다(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 1
	plan := `[{"Plan": {"Node Type": "Seq Scan", "Relation Name": "users", "Startup Cost": 0, "Total Cost": 1.5, "Plan Rows": 10, "Plan Width": 4}}]`
	tab := m.cur()
	m.Update(resultMsg{tab: tab.id, seq: tab.seq, conn: 1, kind: rkPlan, res: &db.Result{Cols: []string{"QUERY PLAN"}, Rows: [][]string{{plan}}}})
	if tab.plan == nil || !tab.planOn || !strings.Contains(m.View(), "Seq Scan") {
		t.Fatal("계획 나무를 연다")
	}
	m.setFocus(fResult)
	m.Update(key("t"))
	if tab.planOn || !strings.Contains(m.View(), `"Node Type": "Seq Scan"`) {
		t.Fatal("t 는 JSON 원문")
	}
	m.Update(key("t"))
	if !tab.planOn {
		t.Fatal("t 를 다시 누르면 나무")
	}
	m.Update(tea.KeyMsg{Type: tea.KeyEsc})
	if tab.plan != nil || tab.planOn {
		t.Fatal("esc 는 닫는다")
	}
	m.Update(resultMsg{tab: tab.id, seq: tab.seq, conn: 1, kind: rkPlan, res: &db.Result{Cols: []string{"QUERY PLAN"}, Rows: [][]string{{"Seq Scan on t"}}}})
	if tab.plan != nil || gridCell(t, m) != "Seq Scan on t" {
		t.Fatal("JSON 이 아니면(직접 쓴 EXPLAIN) 표로 보인다")
	}
}

func TestFinder_고르면_트리에서_찾아_테이블_보기를_연다(t *testing.T) {
	m := testModel(t)
	tn := addTable(m, 1)
	schema := tn.parent
	m.connNode(1).expanded = false
	m.rebuild()
	m.catalogs[1] = &complete.Catalog{Schemas: []complete.Schema{{Name: "public", Tables: []complete.Table{{Name: "users"}, {Name: "orders"}}}}}
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlN})
	if m.overlay != oFinder || !strings.Contains(m.View(), "users") {
		t.Fatal("Ctrl+N 은 찾기")
	}
	typeText(m, "users")
	_, cmd := m.Update(key("enter"))
	if m.overlay != oNone || m.current() != tn || cmd == nil || m.cur().tv == nil || m.focus != fTree {
		t.Fatalf("찾은 테이블을 고르고 연다: cur=%v", m.current())
	}

	// 부모를 아직 안 읽었으면 읽은 뒤 이어서 찾는다
	schema.loaded, schema.children = false, nil
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlN})
	typeText(m, "orders")
	if _, cmd := m.Update(key("enter")); cmd == nil || m.jump == nil {
		t.Fatal("스키마를 읽는다")
	}
	orders := &node{kind: nTable, label: "orders", schema: "public", conn: 1, parent: schema}
	m.Update(childrenMsg{n: schema, gen: schema.gen, children: []*node{orders}})
	if m.jump != nil || m.current() != orders || m.cur().title != "orders" {
		t.Fatalf("읽은 뒤 고른다: cur=%v jump=%v title=%q status=%q", m.current(), m.jump, m.cur().title, m.status)
	}
}

func TestF8_선택_또는_전체를_정렬하고_Redis는_하지_않는다(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 1
	src := "select a,b from t where x=1"
	m.cur().editor.SetValue(src)
	m.setFocus(fEditor)
	m.Update(tea.KeyMsg{Type: tea.KeyF8})
	if got := m.cur().editor.Value(); got != sqlfmt.Format(src) || got == src {
		t.Fatalf("전체 정렬: %q", got)
	}
	m.Update(tea.KeyMsg{Type: tea.KeyF8})
	if m.status != "이미 정렬됨" {
		t.Fatalf("멱등: %q", m.status)
	}
	m.cur().editor.SetValue(src)
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlA})
	m.Update(tea.KeyMsg{Type: tea.KeyF8})
	if m.cur().editor.Value() != sqlfmt.Format(src) {
		t.Fatal("선택 정렬")
	}
	addRedis(m)
	m.cur().conn = 2
	m.cur().editor.SetValue("get   a")
	m.Update(tea.KeyMsg{Type: tea.KeyF8})
	if m.cur().editor.Value() != "get   a" || !strings.Contains(m.status, "Redis") {
		t.Fatal("Redis 는 그대로")
	}
}

func TestParams_이름마다_값을_묻고_바인딩해_실행한다(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 1
	if cmd := m.run("select :a, :b, :a"); cmd != nil || m.overlay != oParams || !strings.Contains(m.input.Prompt, ":a (1/2)") {
		t.Fatalf("값을 묻는다: overlay=%v prompt=%q", m.overlay, m.input.Prompt)
	}
	typeText(m, "x")
	m.Update(key("enter"))
	if m.overlay != oParams || !strings.Contains(m.input.Prompt, ":b (2/2)") {
		t.Fatal("다음 이름")
	}
	_, cmd := m.Update(key("enter"))
	if cmd == nil || m.overlay != oNone || m.cur().running == nil {
		t.Fatal("마지막이면 실행")
	}
	if msg := cmd().(resultMsg); msg.sql != "select :a, :b, :a\n-- 파라미터: a, b" {
		t.Fatalf("기록은 원래 문장과 이름: %q", msg.sql)
	}
	m.run("select :a")
	m.Update(tea.KeyMsg{Type: tea.KeyEsc})
	if m.overlay != oNone || m.status != "취소함" {
		t.Fatal("esc 는 취소")
	}
	if cmd := m.run("select :a; select 2"); cmd != nil || !strings.Contains(m.status, "하나씩") {
		t.Fatalf("여러 문장의 파라미터는 막는다: %q", m.status)
	}
	m.cur().conn = 0
	m.run("delete from t where id = :id")
	typeText(m, "1")
	m.Update(key("enter"))
	if m.overlay != oConfirm {
		t.Fatal("prod 확인은 그대로")
	}
}

func TestParams_실제_서버에서_문자열_값을_자리_타입으로_읽는다(t *testing.T) {
	m := pgModel(t, "it-local")
	m.cur().conn = 0
	m.run("select :v::int + 1 as n")
	typeText(m, "41")
	_, cmd := m.Update(key("enter"))
	drive(m, cmd)
	if res := m.cur().res; res == nil || res.Rows[0][0] != "42" {
		t.Fatalf("바인딩 실행: %+v status=%q", res, m.status)
	}
}

// ─── v3 최종 리뷰 회귀 ───

func TestTx_풀에서_읽은_결과는_열린_트랜잭션_표시를_지우지_않는다(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 1
	m.inTx[1] = true
	m.Update(resultMsg{tab: m.cur().id, seq: m.cur().seq, conn: 1, res: &db.Result{Cols: []string{"pid"}, Rows: [][]string{{"1"}}}})
	if !m.inTx[1] {
		t.Fatal("운영 화면·구조·신호 결과(InTx=false)가 Manual 트랜잭션 표시를 지우면 종료 확인이 빠진다")
	}
	m.Update(resultMsg{tab: m.cur().id, seq: m.cur().seq, conn: 1, session: true, res: &db.Result{Text: "COMMIT"}})
	if m.inTx[1] {
		t.Fatal("편집기 세션 결과는 트랜잭션 표시를 갱신한다")
	}
}

func TestExecScript_접속_종류와_다른_글은_패닉_없이_오류다(t *testing.T) {
	ctx := context.Background()
	rc := db.Conn{Name: "r", Kind: db.KindRedis, Host: "127.0.0.1", Port: 1}
	if _, err := execScript(nil, script{conn: rc, stmts: []string{"select 1"}})(ctx); err == nil {
		t.Fatal("Redis 접속에 SQL 문장은 오류")
	}
	pc := db.Conn{Name: "p", Kind: db.KindPG, Host: "127.0.0.1", Port: 1}
	if _, err := execScript(nil, script{conn: pc, cmds: [][]string{{"GET", "k"}}})(ctx); err == nil {
		t.Fatal("PostgreSQL 접속에 Redis 명령은 오류")
	}
}

func TestTabs_탭_접속이_바뀌면_다른_접속의_표에서_쓰기와_다시_읽기를_막는다(t *testing.T) {
	m := testModel(t)
	rn := addRedis(m)
	tab := m.cur()
	drive(m, m.activate(addTable(m, 1), false))
	if tab.tv == nil || tab.viewConn != 1 {
		t.Fatalf("stg 테이블 보기: viewConn=%d", tab.viewConn)
	}
	// 같은 탭에서 Redis 접속 줄을 펼치면 탭 접속만 바뀐다
	m.activate(rn, false)
	if tab.conn != rn.conn || tab.tv == nil {
		t.Fatalf("conn=%d", tab.conn)
	}
	m.setFocus(fResult)
	if _, cmd := m.Update(key("]")); cmd != nil || tab.running != nil || !strings.Contains(m.status, "x-stg 접속의 것이라 막음") {
		t.Fatalf("다음 페이지는 막는다: %q", m.status)
	}

	show(m, editable())
	editCell(m, "q")
	tab.conn = 0
	if m.Update(key("ctrl+s")); m.overlay != oNone || !strings.Contains(m.status, "막음") {
		t.Fatal("다른 접속의 편집을 prod 에 제출하지 않는다")
	}
	show(m, &db.Result{Cols: []string{"pid"}, Rows: [][]string{{"9"}}})
	if m.Update(key("K")); m.overlay != oNone || !strings.Contains(m.status, "막음") {
		t.Fatal("다른 서버의 pid 에 신호를 보내지 않는다")
	}
}

func TestTabs_다른_접속의_화면을_든_탭은_다른_접속의_객체를_새_탭에_연다(t *testing.T) {
	m := testModel(t)
	tab := m.cur()
	m.activate(addTable(m, 1), false)
	m.activate(addTable(m, 0), false)
	if len(m.tabs) != 2 || tab.conn != 1 || tab.viewConn != 1 || m.cur().conn != 0 || m.cur().tv == nil {
		t.Fatalf("tabs=%d conn=%d", len(m.tabs), tab.conn)
	}
	m.switchTab(0)
	m.setFocus(fTree)
	m.selectNode(m.connNode(0))
	m.Update(key("A"))
	if len(m.tabs) != 3 || tab.conn != 1 || m.cur().conn != 0 {
		t.Fatalf("A 도 새 탭: tabs=%d conn=%d", len(m.tabs), tab.conn)
	}
}

func TestTree_편집_버리기를_거절하면_탭의_테이블_보기와_이름이_그대로다(t *testing.T) {
	m := testModel(t)
	tab := m.cur()
	tab.conn = 1
	show(m, editable())
	m.setFocus(fResult)
	editCell(m, "q")
	m.activate(addTable(m, 1), false)
	if m.overlay != oConfirm {
		t.Fatal("보류 중인 편집이 있으면 묻는다")
	}
	m.Update(key("n"))
	if tab.tv != nil || tab.title != "Query 1" || tab.grid.Pending() != 1 {
		t.Fatalf("거절하면 바꾸지 않는다: tv=%v title=%q", tab.tv != nil, tab.title)
	}
}

func TestTx_트리의_A와_파일_매핑도_이_탭만_쓰던_트랜잭션을_버리지_않는다(t *testing.T) {
	m := testModel(t)
	tab := m.cur()
	tab.conn = 0
	m.inTx[0] = true
	m.setFocus(fTree)
	m.selectNode(m.connNode(1))
	m.Update(key("A"))
	if m.overlay != oTx || tab.conn != 0 || m.txAsk.after != afterRun {
		t.Fatalf("접속을 바꾸기 전에 묻는다: overlay=%v conn=%d", m.overlay, tab.conn)
	}
	m.Update(tea.KeyMsg{Type: tea.KeyEsc})

	must(t, os.MkdirAll(filepath.Join(m.proj.Dir, "queries"), 0o755))
	must(t, os.WriteFile(filepath.Join(m.proj.Dir, "queries", "q.sql"), []byte("select 1;"), 0o600))
	m.proj.Mapping["queries/q.sql"] = "s"
	openQuery(m, "q.sql")
	if len(m.tabs) != 2 || tab.conn != 0 || m.cur().conn != 1 {
		t.Fatalf("매핑이 다른 접속이면 새 탭: tabs=%d conn=%d", len(m.tabs), tab.conn)
	}
}

func TestTx_끝나기를_기다리는_사이_탭을_옮겨도_확인을_띄운_탭을_닫는다(t *testing.T) {
	m := testModel(t)
	a := m.cur()
	a.conn = 0
	m.inTx[0] = true
	b := m.newTab()
	m.setConn(b, 1)
	m.switchTab(0)
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlW})
	if m.overlay != oTx {
		t.Fatal("열린 트랜잭션을 묻는다")
	}
	p := *m.txAsk
	m.Update(key("r"))
	m.switchTab(1)
	m.Update(txDoneMsg{prompt: p, done: []int{0}})
	if len(m.tabs) != 1 || m.cur() != b {
		t.Fatalf("확인을 띄운 탭(a)을 닫는다: tabs=%d", len(m.tabs))
	}
}

func TestTx_오류_상태에서_COMMIT은_ROLLBACK으로_끝나_종료를_멈춘다(t *testing.T) {
	m := pgModel(t, "it-local")
	m.cur().conn = 0
	m.Update(tea.KeyMsg{Type: tea.KeyF9})
	drive(m, m.run("select 1/0"))
	if !m.inTx[0] {
		t.Fatal("Manual 에서 실패한 문장 뒤에도 트랜잭션은 열려 있다")
	}
	_, cmd := m.Update(tea.KeyMsg{Type: tea.KeyF10})
	drive(m, cmd)
	if m.status != commitRolledBack || m.inTx[0] {
		t.Fatalf("F10: %q", m.status)
	}

	drive(m, m.run("select 1/0"))
	m.setFocus(fTree)
	m.Update(key("q"))
	_, cmd = m.Update(key("c"))
	msg := cmd()
	_, next := m.Update(msg)
	if isQuit(next) || !strings.Contains(m.status, commitRolledBack) || m.inTx[0] {
		t.Fatalf("종료하지 않고 알린다: %q", m.status)
	}
}

func TestPin_보류_중인_편집이_있으면_P로_풀지_않는다(t *testing.T) {
	m := testModel(t)
	show(m, editable())
	m.setFocus(fResult)
	m.Update(key("p"))
	m.Update(key("}"))
	editCell(m, "q")
	if m.Update(key("P")); m.cur().grid.Pending() != 1 || len(m.cur().subs) != 1 {
		t.Fatalf("편집을 지키고 막는다: %q", m.status)
	}
}

func TestResult_실행_중에_고친_편집은_결과로_덮지_않는다(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 1
	show(m, editable())
	cmd := m.run("select 1")
	m.setFocus(fResult)
	editCell(m, "q")
	m.Update(cmd())
	if m.cur().grid.Pending() != 1 || !strings.Contains(m.status, "결과를 붙이지 않음") {
		t.Fatalf("pending=%d status=%q", m.cur().grid.Pending(), m.status)
	}
}

// editTable 은 (1,a),(2,b) 가 든 테이블을 만들어 읽고 결과 표에 초점을 둔다.
func editTable(t *testing.T, table string) (*model, pgTest) {
	t.Helper()
	m := pgModel(t, "it-local")
	it := pgTest{t, m}
	it.sql("drop table if exists public." + table + "; create table public." + table + " (id int primary key, name text)")
	it.sql("insert into public." + table + " values (1, 'a'), (2, 'b')")
	m.cur().conn = 0
	drive(m, m.run("select * from public."+table+" order by id"))
	m.setFocus(fResult)
	return m, it
}

func TestSubmit_제출_중에_고친_편집만_남기고_다시_내도_추가_행이_두_번_가지_않는다(t *testing.T) {
	m, it := editTable(t, "v3_edit2")
	m.Update(key("a"))
	editCell(m, "3")
	m.Update(key("l"))
	editCell(m, "c")
	m.Update(key("ctrl+s"))
	_, cmd := m.Update(key("y"))
	m.Update(key("g"))
	editCell(m, "z")
	drive(m, cmd)
	if m.cur().grid.Pending() != 1 || !strings.Contains(m.status, "1건은 남김") {
		t.Fatalf("제출 중 고친 1건만 남는다: pending=%d status=%q", m.cur().grid.Pending(), m.status)
	}
	m.Update(key("ctrl+s"))
	_, cmd = m.Update(key("y"))
	drive(m, cmd)
	if m.cur().grid.Pending() != 0 || strings.Contains(m.status, "실패") {
		t.Fatalf("두 번째 제출: pending=%d status=%q", m.cur().grid.Pending(), m.status)
	}
	got := it.sql("select id, name from public.v3_edit2 order by id")
	if want := [][]string{{"1", "z"}, {"2", "b"}, {"3", "c"}}; fmt.Sprint(got.Rows) != fmt.Sprint(want) {
		t.Fatalf("DB: %v", got.Rows)
	}
}

func TestSubmit_제출_중에_다시_고친_칸은_낸_값을_옛값으로_다시_낸다(t *testing.T) {
	m, it := editTable(t, "v3_edit3")
	m.Update(key("l"))
	editCell(m, "z")
	m.Update(key("ctrl+s"))
	_, cmd := m.Update(key("y"))
	editCell(m, "w")
	drive(m, cmd)
	if m.cur().grid.Pending() != 1 {
		t.Fatalf("다시 고친 칸은 남는다: pending=%d status=%q", m.cur().grid.Pending(), m.status)
	}
	m.Update(key("ctrl+s"))
	_, cmd = m.Update(key("y"))
	drive(m, cmd)
	if m.cur().grid.Pending() != 0 || strings.Contains(m.status, "실패") {
		t.Fatalf("옛값 확인이 DB 와 맞아야 한다: %q", m.status)
	}
	if got := it.sql("select name from public.v3_edit3 where id = 1"); got.Rows[0][0] != "w" {
		t.Fatalf("DB: %v", got.Rows)
	}
}

func TestSubmit_제출_중에_고친_추가_행은_버리고_다시_읽으라고_알린다(t *testing.T) {
	m, it := editTable(t, "v3_edit4")
	m.Update(key("a"))
	editCell(m, "3")
	m.Update(key("ctrl+s"))
	_, cmd := m.Update(key("y"))
	m.Update(key("l"))
	editCell(m, "c")
	drive(m, cmd)
	if m.cur().grid.Pending() != 0 || !strings.Contains(m.status, "1개는 버림: 다시 읽은 뒤 고치세요") {
		t.Fatalf("pending=%d status=%q", m.cur().grid.Pending(), m.status)
	}
	if got := it.sql("select count(*) from public.v3_edit4 where id = 3"); got.Rows[0][0] != "1" {
		t.Fatalf("낸 추가 행은 반영됐다: %v", got.Rows)
	}
}

func TestHistory_제출과_Redis_편집은_값_없이_요약만_기록한다(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 1
	show(m, editable())
	m.setFocus(fResult)
	m.Update(key("l"))
	editCell(m, "hunter2")
	m.Update(key("ctrl+s"))
	if want := `-- 제출 1건 "public"."t" (UPDATE 1 · INSERT 0 · DELETE 0)`; m.pending == nil || m.pending.sql != want {
		t.Fatalf("제출 기록: %+v", m.pending)
	}
	m.Update(key("n"))

	rn := addRedis(m)
	tab := m.newTab()
	m.setConn(tab, rn.conn)
	m.Update(redisValueMsg{tab: tab.id, conn: rn.conn, v: &db.RedisValue{Key: "k", Type: "string", String: "old", TTL: -1}})
	m.setFocus(fResult)
	m.Update(key("e"))
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlU})
	typeText(m, "hunter2")
	_, cmd := m.Update(key("enter"))
	msg, ok := cmd().(resultMsg)
	if !ok || msg.sql != "-- Redis 편집: SET k" {
		t.Fatalf("Redis 편집 기록: %q", msg.sql)
	}
}
