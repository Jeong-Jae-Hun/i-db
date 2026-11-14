package tableview

import (
	"context"
	"net"
	"os"
	"strconv"
	"strings"
	"testing"
	"time"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/x/ansi"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
)

var weird = db.TableRef{Schema: `my"sch.ema`, Table: `표 "a.b"`}

func key(s string) tea.KeyMsg {
	switch s {
	case "enter":
		return tea.KeyMsg{Type: tea.KeyEnter}
	case "esc":
		return tea.KeyMsg{Type: tea.KeyEsc}
	case "backspace":
		return tea.KeyMsg{Type: tea.KeyBackspace}
	case " ":
		return tea.KeyMsg{Type: tea.KeySpace}
	}
	return tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune(s)}
}

func typeWhere(t *testing.T, m Model, where string) (Model, Action) {
	t.Helper()
	m, a := m.Update(key("w"))
	if !m.Typing() || !a.Handled {
		t.Fatalf("w should open WHERE input: %+v", a)
	}
	for m.input != "" {
		m, _ = m.Update(key("backspace"))
	}
	if where != "" {
		m, _ = m.Update(key(where))
	}
	return m.Update(key("enter"))
}

func TestSQL_기본은_200행_첫_페이지(t *testing.T) {
	m := New(db.TableRef{Schema: "public", Table: "users"}, Options{})
	if got, want := m.SQL(), `SELECT * FROM "public"."users" LIMIT 200 OFFSET 0`; got != want {
		t.Fatalf("got %q want %q", got, want)
	}
}

func TestSQL_따옴표_점_한글_이름을_감싼다(t *testing.T) {
	m := New(weird, Options{PageSize: 50})
	want := `SELECT * FROM "my""sch.ema"."표 ""a.b""" LIMIT 50 OFFSET 0`
	if got := m.SQL(); got != want {
		t.Fatalf("got %q want %q", got, want)
	}
	if got, want := m.CountSQL(), `SELECT count(*) FROM "my""sch.ema"."표 ""a.b"""`; got != want {
		t.Fatalf("got %q want %q", got, want)
	}
}

func TestSQL_페이지_크기는_MaxRows_까지(t *testing.T) {
	m := New(weird, Options{PageSize: 5000})
	if !strings.Contains(m.SQL(), " LIMIT 1000 ") {
		t.Fatalf("page size should be capped: %s", m.SQL())
	}
}

func TestSQL_WHERE_와_여러_열_정렬(t *testing.T) {
	m := New(db.TableRef{Schema: "s", Table: "t"}, Options{PageSize: 10})
	m, a := typeWhere(t, m, "age > 3 AND name <> 'x;y'")
	if !a.Requery || m.Typing() {
		t.Fatalf("enter should requery: %+v", a)
	}
	m, _ = m.HeaderClickAdd("a")
	m, _ = m.HeaderClickAdd(`b"c`)
	m, _ = m.HeaderClickAdd(`b"c`)
	want := `SELECT * FROM "s"."t" WHERE age > 3 AND name <> 'x;y' ORDER BY "a" ASC, "b""c" DESC LIMIT 10 OFFSET 0`
	if got := m.SQL(); got != want {
		t.Fatalf("got %q want %q", got, want)
	}
	if got, want := m.CountSQL(), `SELECT count(*) FROM "s"."t" WHERE age > 3 AND name <> 'x;y'`; got != want {
		t.Fatalf("got %q want %q", got, want)
	}
}

func TestSQL_줄_주석이_있는_WHERE_는_줄을_바꿔_LIMIT_을_지킨다(t *testing.T) {
	m := New(db.TableRef{Schema: "s", Table: "t"}, Options{})
	m, _ = typeWhere(t, m, "id = 1 -- 메모")
	if got, want := m.SQL(), "SELECT * FROM \"s\".\"t\" WHERE id = 1 -- 메모\n LIMIT 200 OFFSET 0"; got != want {
		t.Fatalf("got %q want %q", got, want)
	}
}

func TestEstimateSQL_작은따옴표_백슬래시를_이스케이프한다(t *testing.T) {
	m := New(db.TableRef{Schema: "public", Table: "a'b"}, Options{})
	if got, want := m.EstimateSQL(), `SELECT reltuples::bigint FROM pg_class WHERE oid = '"public"."a''b"'::regclass`; got != want {
		t.Fatalf("got %q want %q", got, want)
	}
	m = New(db.TableRef{Schema: "public", Table: `a\b`}, Options{})
	if got, want := m.EstimateSQL(), `SELECT reltuples::bigint FROM pg_class WHERE oid = E'"public"."a\\b"'::regclass`; got != want {
		t.Fatalf("got %q want %q", got, want)
	}
}

func TestCheckWhere_최상위_세미콜론만_거부한다(t *testing.T) {
	ok := []string{"", "  ", "a = 1", "s = 'a;b'", `"x;y" = 1`, "a = 1 -- ; 주석", "a = 1 /* ; */", "a = $$;$$"}
	for _, w := range ok {
		if err := CheckWhere(w); err != nil {
			t.Errorf("%q should pass: %v", w, err)
		}
	}
	bad := []string{"a = 1;", "a = 1; drop table t", "1=1; select 1", "-- 주석뿐", ";"}
	for _, w := range bad {
		if err := CheckWhere(w); err == nil {
			t.Errorf("%q should be rejected", w)
		}
	}
}

func TestWHERE_세미콜론이면_재조회하지_않고_입력을_유지한다(t *testing.T) {
	m := New(weird, Options{})
	before := m.SQL()
	m, a := typeWhere(t, m, "a = 1; drop table x")
	if a.Requery || a.Status == "" || !m.Typing() {
		t.Fatalf("should reject: %+v typing=%v", a, m.Typing())
	}
	if m.SQL() != before {
		t.Fatalf("SQL changed on rejected WHERE: %s", m.SQL())
	}
}

func TestWHERE_Esc_는_취소하고_빈_값은_해제한다(t *testing.T) {
	m := New(db.TableRef{Schema: "s", Table: "t"}, Options{})
	m, _ = typeWhere(t, m, "a = 1")
	m, _ = m.Update(key("w"))
	if m.input != "a = 1" {
		t.Fatalf("input should start with current WHERE: %q", m.input)
	}
	m, _ = m.Update(key(" "))
	m, _ = m.Update(key("x"))
	m, a := m.Update(key("esc"))
	if m.Typing() || a.Requery || !strings.Contains(m.SQL(), "WHERE a = 1 LIMIT") {
		t.Fatalf("esc should keep old WHERE: %s %+v", m.SQL(), a)
	}
	m, a = typeWhere(t, m, "")
	if !a.Requery || strings.Contains(m.SQL(), "WHERE") {
		t.Fatalf("empty WHERE should clear: %s %+v", m.SQL(), a)
	}
}

func TestWHERE_붙여넣은_줄바꿈은_공백이다(t *testing.T) {
	m := New(db.TableRef{Schema: "s", Table: "t"}, Options{})
	m, _ = m.Update(key("w"))
	m, _ = m.Update(tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("a = 1\nand\tb = 2"), Paste: true})
	m, _ = m.Update(key("enter"))
	if !strings.Contains(m.SQL(), "WHERE a = 1 and b = 2 LIMIT") {
		t.Fatalf("got %s", m.SQL())
	}
}

func TestWHERE_적용하면_첫_페이지와_정확한_개수를_버린다(t *testing.T) {
	m := New(db.TableRef{Schema: "s", Table: "t"}, Options{PageSize: 10})
	m = m.SetTotal(100, true)
	m, _ = m.Update(key("]"))
	m, _ = typeWhere(t, m, "a = 1")
	if !strings.HasSuffix(m.SQL(), "OFFSET 0") {
		t.Fatalf("page should reset: %s", m.SQL())
	}
	if m.known {
		t.Fatal("exact total should be dropped after WHERE change")
	}
}

func TestPage_정확한_개수_안에서만_넘긴다(t *testing.T) {
	m := New(db.TableRef{Schema: "s", Table: "t"}, Options{PageSize: 200})
	m, a := m.Update(key("["))
	if a.Requery || a.Status != "첫 페이지" {
		t.Fatalf("prev on first page: %+v", a)
	}
	m = m.SetTotal(401, true)
	for _, off := range []string{"OFFSET 200", "OFFSET 400"} {
		m, a = m.Update(key("]"))
		if !a.Requery || !strings.HasSuffix(m.SQL(), off) {
			t.Fatalf("want %s: %s %+v", off, m.SQL(), a)
		}
	}
	m, a = m.Update(key("]"))
	if a.Requery || a.Status != "마지막 페이지" || !strings.HasSuffix(m.SQL(), "OFFSET 400") {
		t.Fatalf("next past end: %s %+v", m.SQL(), a)
	}
	m, a = m.Update(key("["))
	if !a.Requery || !strings.HasSuffix(m.SQL(), "OFFSET 200") {
		t.Fatalf("prev: %s %+v", m.SQL(), a)
	}
	if off, size := m.Page(); off != 200 || size != 200 {
		t.Fatalf("Page: %d %d", off, size)
	}
}

func TestPage_추정치나_모르는_개수는_막지_않는다(t *testing.T) {
	m := New(db.TableRef{Schema: "s", Table: "t"}, Options{PageSize: 10}).SetTotal(5, false)
	m, a := m.Update(key("]"))
	if !a.Requery || !strings.HasSuffix(m.SQL(), "OFFSET 10") {
		t.Fatalf("estimate should not bound: %+v", a)
	}
	m = m.SetTotal(-1, false)
	if m.known {
		t.Fatal("negative reltuples means unknown")
	}
}

func TestKey_샵은_개수_요청이고_모르는_키는_넘긴다(t *testing.T) {
	m := New(weird, Options{})
	if _, a := m.Update(key("#")); !a.Count || !a.Handled || a.Requery {
		t.Fatalf("# : %+v", a)
	}
	for _, k := range []string{"j", "s", "ww"} {
		if _, a := m.Update(key(k)); a.Handled {
			t.Fatalf("%q should not be handled", k)
		}
	}
}

func TestHeaderClick_오름_내림_없음_순환하고_다른_열을_지운다(t *testing.T) {
	m := New(db.TableRef{Schema: "s", Table: "t"}, Options{PageSize: 10}).SetTotal(100, true)
	m, _ = m.Update(key("]"))
	m, _ = m.HeaderClickAdd("x")
	steps := []string{`ORDER BY "a" ASC LIMIT`, `ORDER BY "a" DESC LIMIT`, `"t" LIMIT`}
	for _, want := range steps {
		var a Action
		m, a = m.HeaderClick("a")
		if !a.Requery || !strings.Contains(m.SQL(), want) || !strings.HasSuffix(m.SQL(), "OFFSET 0") {
			t.Fatalf("want %q: %s", want, m.SQL())
		}
	}
}

func TestHeaderClickAdd_이전_Model_의_정렬을_바꾸지_않는다(t *testing.T) {
	m := New(db.TableRef{Schema: "s", Table: "t"}, Options{})
	m1, _ := m.HeaderClickAdd("a")
	m1, _ = m1.HeaderClickAdd("b")
	m2, _ := m1.HeaderClickAdd("a")
	m3, _ := m2.HeaderClickAdd("a")
	if !strings.Contains(m1.SQL(), `"a" ASC, "b" ASC`) {
		t.Fatalf("m1 mutated: %s", m1.SQL())
	}
	if !strings.Contains(m2.SQL(), `"a" DESC, "b" ASC`) || !strings.Contains(m3.SQL(), `ORDER BY "b" ASC LIMIT`) {
		t.Fatalf("m2 %s / m3 %s", m2.SQL(), m3.SQL())
	}
}

func TestView_행_범위와_개수_표기(t *testing.T) {
	m := New(db.TableRef{Schema: "public", Table: "users"}, Options{})
	m, _ = m.Update(key("]"))
	cases := []struct {
		m    Model
		want string
	}{
		{m, "rows 201–400 ·"},
		{m.SetTotal(12345, false), "rows 201–400 of ~12,345 (추정)"},
		{m.SetTotal(1234567, true), "rows 201–400 of 1,234,567"},
		{m.SetTotal(250, true), "rows 201–250 of 250"},
		{m.SetTotal(0, true), "rows 0 of 0"},
	}
	for _, c := range cases {
		if got := c.m.View(300); !strings.Contains(got, c.want) {
			t.Errorf("want %q in %q", c.want, got)
		}
	}
	got := m.View(300)
	for _, want := range []string{"public.users", "WHERE 없음", "w 거르기"} {
		if !strings.Contains(got, want) {
			t.Errorf("want %q in %q", want, got)
		}
	}
}

func TestView_정렬_요약과_제어_문자_치환(t *testing.T) {
	m := New(db.TableRef{Schema: "s", Table: "t\x1b[31m"}, Options{})
	m, _ = m.HeaderClickAdd("a")
	m, _ = m.HeaderClickAdd("b")
	m, _ = m.HeaderClickAdd("b")
	got := m.View(300)
	if strings.ContainsRune(got, '\x1b') || !strings.Contains(got, `t\x1b[31m`) {
		t.Fatalf("control chars not sanitized: %q", got)
	}
	if !strings.Contains(got, "ORDER BY a↑, b↓") {
		t.Fatalf("order summary: %q", got)
	}
}

func TestView_좁은_폭에서도_넘치지_않는다(t *testing.T) {
	m := New(weird, Options{})
	m, _ = typeWhere(t, m, "이름 = '아주 긴 값' AND 나이 > 30")
	typing, _ := m.Update(key("w"))
	for _, w := range []int{0, 1, 5, 20, 40, 80} {
		for _, v := range []Model{m, typing} {
			got := v.View(w)
			if ansi.StringWidth(got) > w || strings.Contains(got, "\n") {
				t.Errorf("width %d: %q (%d)", w, got, ansi.StringWidth(got))
			}
		}
	}
}

func TestView_입력_중에는_긴_입력의_끝을_보인다(t *testing.T) {
	m := New(weird, Options{})
	m, _ = m.Update(key("w"))
	m, _ = m.Update(key(strings.Repeat("x", 100) + "END"))
	got := m.View(60)
	if !strings.HasPrefix(got, "WHERE …") || !strings.Contains(got, "END█") {
		t.Fatalf("got %q", got)
	}
}

// ─── 통합 (DBTUI_TEST_PG_URL 이 있을 때만) ───

func TestPG_이상한_이름_테이블에서_SQL_세_가지가_돈다(t *testing.T) {
	raw := os.Getenv("DBTUI_TEST_PG_URL")
	if raw == "" {
		t.Skip("DBTUI_TEST_PG_URL not set")
	}
	cred, hostdb, _ := strings.Cut(strings.TrimPrefix(raw, "postgres://"), "@")
	user, pass, _ := strings.Cut(cred, ":")
	hp, dbname, _ := strings.Cut(hostdb, "/")
	host, port, _ := net.SplitHostPort(hp)
	p, _ := strconv.Atoi(port)
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	cs := db.NewClients()
	defer cs.Close()
	pg, err := cs.PG(ctx, db.Conn{Name: "it-local", UUID: "it-tableview", Kind: db.KindPG, Host: host, Port: p, DB: dbname, User: user, Password: pass})
	if err != nil {
		t.Fatal(err)
	}
	tbl := db.TableRef{Schema: `it tv"s.x`, Table: `a'b."표"\`}
	name := db.QuoteIdent(tbl.Schema) + "." + db.QuoteIdent(tbl.Table)
	setup := []string{
		"drop schema if exists " + db.QuoteIdent(tbl.Schema) + " cascade",
		"create schema " + db.QuoteIdent(tbl.Schema),
		"create table " + name + ` (id int, "그룹" text)`,
		"insert into " + name + ` select g, case when g % 2 = 0 then '짝' else '홀' end from generate_series(1, 450) g`,
		"analyze " + name,
	}
	// cs.Close 보다 먼저 돌도록 t.Cleanup 대신 defer 로 지운다. 정리 실패는 다음 실행의 첫 drop 이 다시 지우므로 오류를 보지 않는다
	defer pg.Run(ctx, "drop schema if exists "+db.QuoteIdent(tbl.Schema)+" cascade")
	for _, q := range setup {
		if _, err := pg.Run(ctx, q); err != nil {
			t.Fatalf("%s: %v", q, err)
		}
	}

	m := New(tbl, Options{PageSize: 100})
	est, err := pg.Run(ctx, m.EstimateSQL())
	if err != nil {
		t.Fatalf("%s: %v", m.EstimateSQL(), err)
	}
	if len(est.Rows) != 1 || est.Rows[0][0] != "450" {
		t.Fatalf("estimate: %+v", est.Rows)
	}

	m, _ = typeWhere(t, m, `"그룹" = '짝' -- 짝수만`)
	m, _ = m.HeaderClick("id")
	m, _ = m.HeaderClick("id")
	m, _ = m.Update(key("]"))
	res, err := pg.Run(ctx, m.SQL())
	if err != nil {
		t.Fatalf("%s: %v", m.SQL(), err)
	}
	// 짝수 225개를 내림차순으로 100개씩 보면 두 번째 페이지는 250 부터 52 까지다
	if len(res.Rows) != 100 || res.Rows[0][0] != "250" || res.Rows[99][0] != "52" {
		t.Fatalf("page rows: %d first=%v last=%v", len(res.Rows), res.Rows[0], res.Rows[len(res.Rows)-1])
	}
	cnt, err := pg.Run(ctx, m.CountSQL())
	if err != nil {
		t.Fatalf("%s: %v", m.CountSQL(), err)
	}
	if cnt.Rows[0][0] != "225" {
		t.Fatalf("count: %+v", cnt.Rows)
	}
}
