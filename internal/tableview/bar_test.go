package tableview

import (
	"strings"
	"testing"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
)

// ─── WHERE·ORDER BY 줄 (R16.4) ───

func barModel() Model {
	return New(db.TableRef{Schema: "s", Table: "t"}, Options{}).
		SetColumns([]string{"id", "name", "status", "email", "Note"}, []string{"int8", "text", "text", "text", "text"})
}

// enter 는 열린 입력 줄을 비우고 text 를 친 뒤 Enter 다.
func enter(m Model, text string) (Model, Action) {
	for m.input != "" {
		m, _ = m.Update(key("backspace"))
	}
	if text != "" {
		m, _ = m.Update(key(text))
	}
	return m.Update(key("enter"))
}

func TestBar_WHERE_와_ORDER_BY_칸을_늘_보인다(t *testing.T) {
	m := barModel()
	if got := m.View(200); !strings.HasPrefix(got, "WHERE (없음)   ORDER BY (없음) · ") {
		t.Fatalf("빈 칸: %q", got)
	}
	m, _ = typeWhere(t, m, "a = 1")
	m, _ = m.HeaderClickAdd("Note")
	m, _ = m.HeaderClickAdd("Note")
	m, _ = m.HeaderClickAdd("name")
	if got := m.View(200); !strings.HasPrefix(got, `WHERE a = 1   ORDER BY "Note" DESC, name · `) {
		t.Fatalf("머리 클릭 정렬이 ORDER BY 칸에 보인다: %q", got)
	}
}

func TestBarClick_칸마다_지금_값으로_채운_입력을_연다(t *testing.T) {
	m := barModel()
	m, _ = typeWhere(t, m, "a = 1")
	m, _ = m.HeaderClick("id")
	m, _ = m.HeaderClick("id")
	whereSeg, orderSeg := m.bar(120)
	cases := []struct {
		x     int
		field field
		input string
	}{
		{0, fieldWhere, "a = 1"},
		{len(whereSeg), fieldWhere, "a = 1"},
		{len(whereSeg) + len(barGap), fieldOrder, "id DESC"},
		{len(whereSeg) + len(barGap) + len(orderSeg) - 1, fieldOrder, "id DESC"},
	}
	for _, c := range cases {
		got, a := m.BarClick(c.x, 120)
		if !a.Handled || a.Requery || got.typing != c.field || got.input != c.input {
			t.Errorf("x=%d: %+v typing=%v input=%q", c.x, a, got.typing, got.input)
		}
	}
	if _, a := m.BarClick(len(whereSeg)+len(barGap)+len(orderSeg)+5, 120); a.Handled {
		t.Error("칸 밖은 넘긴다")
	}
	typing, _ := m.Update(key("w"))
	if _, a := typing.BarClick(0, 120); a.Handled {
		t.Error("입력 중에는 클릭으로 다시 열지 않는다")
	}
}

func TestKey_슬래시는_WHERE_대문자_O는_ORDER_BY_입력이다(t *testing.T) {
	m := barModel()
	m, _ = m.HeaderClick("name")
	if w, a := m.Update(key("/")); !a.Handled || w.typing != fieldWhere {
		t.Fatalf("/ 는 WHERE 입력: %+v", a)
	}
	o, a := m.Update(key("O"))
	if !a.Handled || o.typing != fieldOrder || o.input != "name" {
		t.Fatalf("O 는 ORDER BY 입력: %+v %q", a, o.input)
	}
	if !strings.HasPrefix(o.View(80), "ORDER BY name█") {
		t.Fatalf("입력 줄: %q", o.View(80))
	}
}

func TestComplete_Tab_은_열_이름을_대소문자_무시로_채우고_돈다(t *testing.T) {
	m := barModel()
	m, _ = m.Update(key("w"))
	m, _ = m.Update(key("x = 1 and N"))
	tab := tea.KeyMsg{Type: tea.KeyTab}
	want := []string{"x = 1 and name", `x = 1 and "Note"`, "x = 1 and name"}
	for _, w := range want {
		m, _ = m.Update(tab)
		if m.input != w {
			t.Fatalf("got %q want %q", m.input, w)
		}
	}
	m, _ = m.Update(key(" "))
	m, _ = m.Update(key("zz"))
	if m, _ = m.Update(tab); m.input != "x = 1 and name zz" {
		t.Fatalf("후보가 없으면 그대로: %q", m.input)
	}

	o, _ := barModel().Update(key("O"))
	o, _ = o.Update(key("st"))
	o, _ = o.Update(tab)
	o, _ = o.Update(key(" d"))
	o, _ = o.Update(tab)
	if o.input != "status desc" {
		t.Fatalf("ORDER BY 는 asc·desc 도 채운다: %q", o.input)
	}
}

func TestWHERE_짧은_꼴은_SQL_로_바꾸고_SQL_은_그대로_둔다(t *testing.T) {
	cases := []struct{ in, where string }{
		{"status=ACTIVE", `"status"::text = 'ACTIVE'`},
		{"status != ACTIVE", `"status"::text <> 'ACTIVE'`},
		{"id>100", `"id" > 100`},
		{"id = 7", `"id" = 7`},
		{"name>m", `"name"::text > 'm'`},
		{"email=null", `"email" IS NULL`},
		{"name:abc def", `"name"::text ILIKE '%abc def%'`},
		{"status = 'ACTIVE'", "status = 'ACTIVE'"},
		{"id = 1 AND name = 'x'", "id = 1 AND name = 'x'"},
		{"id<>2", "id<>2"},
		{"id = name", "id = name"},
		{"name::text = 'x'", "name::text = 'x'"},
		{"created > now()", "created > now()"},
		{"nocol=1", "nocol=1"},
		{"abc", "abc"},
	}
	for _, c := range cases {
		m, _ := barModel().Update(key("/"))
		m, a := enter(m, c.in)
		if !a.Requery || m.where != c.where {
			t.Errorf("%q: where=%q %+v", c.in, m.where, a)
		}
	}
}

func TestEsc_짧은_꼴로_건_WHERE_만_지운다(t *testing.T) {
	m, _ := barModel().Update(key("/"))
	m, _ = enter(m, "status=ACTIVE")
	cleared, a := m.Update(key("esc"))
	if !a.Requery || cleared.where != "" {
		t.Fatalf("esc 는 짧은 꼴 조건을 지운다: %q %+v", cleared.where, a)
	}
	if _, a = cleared.Update(key("esc")); a.Handled {
		t.Fatal("지울 조건이 없으면 결과 표로 넘긴다")
	}
	raw, _ := typeWhere(t, barModel(), "id = 1 or id = 2")
	if _, a = raw.Update(key("esc")); a.Handled {
		t.Fatal("SQL 로 쓴 WHERE 는 esc 로 지우지 않는다")
	}
	toggled, _ := m.ToggleWhere(`"id" = 1`)
	if _, a = toggled.Update(key("esc")); a.Handled {
		t.Fatal("다른 조건을 더한 WHERE 는 esc 로 지우지 않는다")
	}
}

func TestORDER_BY_입력은_열_목록으로_읽고_식은_그대로_쓴다(t *testing.T) {
	m, _ := barModel().Update(key("O"))
	m, a := enter(m, `NAME desc, id, "Note" asc`)
	if !a.Requery || !strings.Contains(m.SQL(), `ORDER BY "name" DESC, "id" ASC, "Note" ASC LIMIT`) {
		t.Fatalf("열 목록: %s %+v", m.SQL(), a)
	}
	if m, _ = m.HeaderClickAdd("id"); !strings.Contains(m.SQL(), `ORDER BY "name" DESC, "id" DESC, "Note" ASC LIMIT`) {
		t.Fatalf("읽은 정렬에서 머리 클릭이 이어 돈다: %s", m.SQL())
	}

	m, _ = m.Update(key("O"))
	m, _ = enter(m, "lower(name) desc -- 끝")
	if !strings.Contains(m.SQL(), "ORDER BY lower(name) desc -- 끝\n LIMIT") || !strings.HasPrefix(m.View(200), "WHERE (없음)   ORDER BY lower(name)") {
		t.Fatalf("식은 그대로, 줄 주석은 줄을 바꾼다: %q / %q", m.SQL(), m.View(200))
	}
	if m, _ = m.HeaderClick("id"); !strings.Contains(m.SQL(), `ORDER BY "id" ASC LIMIT`) {
		t.Fatalf("머리 클릭은 식 정렬을 바꾼다: %s", m.SQL())
	}

	m, _ = m.Update(key("O"))
	if m, a = enter(m, "id; drop table x"); a.Requery || a.Status == "" || !m.Typing() {
		t.Fatalf("최상위 ; 는 거부: %+v", a)
	}
	m, _ = m.Update(key("esc"))
	m, _ = m.Update(key("O"))
	if m, _ = enter(m, ""); strings.Contains(m.SQL(), "ORDER BY") {
		t.Fatalf("빈 값은 정렬 해제: %s", m.SQL())
	}
}

func TestIdentText_소문자_이름만_따옴표_없이_쓴다(t *testing.T) {
	cases := map[string]string{"name": "name", "a_1": "a_1", "Note": `"Note"`, "1a": `"1a"`, "로그": `"로그"`, "": `""`}
	for in, want := range cases {
		if got := identText(in); got != want {
			t.Errorf("%q: got %q want %q", in, got, want)
		}
	}
}
