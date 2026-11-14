package tableview

import (
	"strings"
	"testing"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
)

func TestCondition_같음_다름_NULL을_감싼_이름과_리터럴로_만든다(t *testing.T) {
	cases := []struct {
		col, value   string
		null, negate bool
		want         string
	}{
		{"status", "ACTIVE", false, false, `"status" = 'ACTIVE'`},
		{"status", "ACTIVE", false, true, `"status" <> 'ACTIVE'`},
		{"status", "", true, false, `"status" IS NULL`},
		{"status", "", true, true, `"status" IS NOT NULL`},
		{`a"b`, `it's \x`, false, false, `"a""b" = E'it''s \\x'`},
	}
	for _, c := range cases {
		if got := Condition(c.col, c.value, c.null, c.negate); got != c.want {
			t.Errorf("Condition(%q, %q, %v, %v) = %s, want %s", c.col, c.value, c.null, c.negate, got, c.want)
		}
	}
}

func TestToggleCondition_빈_WHERE에_더하고_다시_넣으면_비운다(t *testing.T) {
	cond := `"status" = 'ACTIVE'`
	where := toggleCondition("  ", cond)
	if where != cond {
		t.Fatalf("더하기: %q", where)
	}
	if got := toggleCondition(where, cond); got != "" {
		t.Fatalf("빼기: %q", got)
	}
}

func TestToggleCondition_AND로_더하고_뺄_때_다른_항은_깨끗이_남긴다(t *testing.T) {
	cond := `"status" = 'ACTIVE'`
	where := toggleCondition("id > 10", cond)
	if where != `id > 10 AND "status" = 'ACTIVE'` {
		t.Fatalf("더하기: %q", where)
	}
	where = toggleCondition(where, `"kind" IS NULL`)
	if got := toggleCondition(where, cond); got != `id > 10 AND "kind" IS NULL` {
		t.Fatalf("가운데 빼기: %q", got)
	}
	// 공백·대소문자가 다른 and 와 공백 차이는 같은 항으로 본다
	if got := toggleCondition("  \"status\"   =  'ACTIVE'\n and (a and b)  ", cond); got != "(a and b)" {
		t.Fatalf("공백 차이: %q", got)
	}
}

func TestToggleCondition_최상위_OR는_괄호로_감싸고_괄호_안_OR는_그대로_가른다(t *testing.T) {
	cond := `"x" = '1'`
	if got := toggleCondition("a = 1 OR b = 2", cond); got != `(a = 1 OR b = 2) AND "x" = '1'` {
		t.Fatalf("OR: %q", got)
	}
	// 감싼 결과에서 다시 빼면 괄호 항만 남는다
	if got := toggleCondition(`(a = 1 OR b = 2) AND "x" = '1'`, cond); got != "(a = 1 OR b = 2)" {
		t.Fatalf("OR 뒤 빼기: %q", got)
	}
	// OR 가 최상위면 cond 가 AND 항처럼 보여도 빼지 않는다. 빼면 뜻이 바뀐다
	if got := toggleCondition(`a = 1 OR b = 2 AND "x" = '1'`, cond); got != `(a = 1 OR b = 2 AND "x" = '1') AND "x" = '1'` {
		t.Fatalf("OR 안 항: %q", got)
	}
}

func TestToggleCondition_따옴표_안의_AND_OR와_공백과_이름_일부는_가르지_않는다(t *testing.T) {
	cond := `"memo" = 'a  OR  b'`
	where := toggleCondition(`brand = 'X AND Y' AND "order" = 1 AND android = true`, cond)
	if where != `brand = 'X AND Y' AND "order" = 1 AND android = true AND "memo" = 'a  OR  b'` {
		t.Fatalf("더하기: %q", where)
	}
	if got := toggleCondition(where, cond); got != `brand = 'X AND Y' AND "order" = 1 AND android = true` {
		t.Fatalf("빼기: %q", got)
	}
	// 따옴표 안 공백이 다르면 다른 값이다
	if got := toggleCondition(`"memo" = 'a OR b'`, cond); got != `"memo" = 'a OR b' AND `+cond {
		t.Fatalf("값 차이: %q", got)
	}
}

func TestToggleCondition_줄_주석은_줄을_바꿔_감싸고_닫히지_않은_글은_감싼다(t *testing.T) {
	cond := `"x" IS NULL`
	if got := toggleCondition("a = 1 -- 메모", cond); got != "(a = 1 -- 메모\n) AND "+cond {
		t.Fatalf("줄 주석: %q", got)
	}
	if got := toggleCondition("a = (1", cond); got != "(a = (1) AND "+cond {
		t.Fatalf("닫히지 않은 괄호: %q", got)
	}
	if err := CheckWhere(toggleCondition("a = 1 -- 메모", cond)); err != nil {
		t.Fatalf("감싼 WHERE 가 문장 하나여야 한다: %v", err)
	}
}

func TestToggleCondition_달러_인용과_E_문자열_안의_따옴표는_항을_가르지_않는다(t *testing.T) {
	cond := `"x" = '1'`
	for _, where := range []string{
		`note = $$it's AND$$`,
		`note = $tag$a OR 'b$tag$`,
		`note = E'it\' AND y'`,
	} {
		added := toggleCondition(where, cond)
		if added != where+" AND "+cond {
			t.Errorf("%s: 감싸지 않고 AND 로 더한다: %q", where, added)
		}
		if got := toggleCondition(added, cond); got != where {
			t.Errorf("%s: 다시 넣으면 원래대로: %q", where, got)
		}
	}
}

func TestToggleWhere_WHERE를_바꾸고_첫_페이지로_다시_읽는다(t *testing.T) {
	m := New(db.TableRef{Schema: "s", Table: "t"}, Options{PageSize: 10})
	m = m.SetTotal(100, true)
	m, _ = m.Update(key("]"))
	m, a := m.ToggleWhere(`"a" = '1'`)
	if !a.Requery || !strings.Contains(m.SQL(), `WHERE "a" = '1' LIMIT 10 OFFSET 0`) || m.known {
		t.Fatalf("더하기: %+v %s %v", a, m.SQL(), m.known)
	}
	m, _ = m.ToggleWhere(`"a" = '1'`)
	if strings.Contains(m.SQL(), "WHERE") {
		t.Fatalf("빼기: %s", m.SQL())
	}
}
