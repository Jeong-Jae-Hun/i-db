package sqlfmt

import (
	"strings"
	"testing"
)

func TestFormat_절마다_줄을_바꾸고_키워드를_대문자로_쓴다(t *testing.T) {
	cases := []struct{ in, want string }{
		{
			"select id, name from users u left join orders o on o.user_id = u.id where u.id = 1 and o.total > 10 or u.vip order by u.id limit 5",
			"SELECT id,\n    name\nFROM users u\nLEFT JOIN orders o ON o.user_id = u.id\nWHERE u.id = 1\n    AND o.total > 10\n    OR u.vip\nORDER BY u.id\nLIMIT 5",
		},
		{
			"select * from t where id in (select id from u where x between 1 and 2)",
			"SELECT *\nFROM t\nWHERE id IN (\n    SELECT id\n    FROM u\n    WHERE x BETWEEN 1 AND 2\n)",
		},
		{
			"insert into t (a, b) values (1, 2), (3, 4) returning id",
			"INSERT INTO t (a, b)\nVALUES (1, 2),\n    (3, 4)\nRETURNING id",
		},
		{
			"update t set a = 1, b = 2 where id = $1",
			"UPDATE t\nSET a = 1,\n    b = 2\nWHERE id = $1",
		},
		{
			"delete from t where a is distinct from b",
			"DELETE FROM t\nWHERE a IS DISTINCT FROM b",
		},
		{
			"select count(*), extract(year from d) from t group by 2 having count(*) > 1 union all select 1, 2",
			"SELECT count(*),\n    extract(year FROM d)\nFROM t\nGROUP BY 2\nHAVING count(*) > 1\nUNION ALL\nSELECT 1,\n    2",
		},
		{
			"with a as (select 1) select * from a",
			"WITH a AS (\n    SELECT 1\n)\nSELECT *\nFROM a",
		},
		{
			"insert into t values (1) on conflict (id) do update set a = 1",
			"INSERT INTO t\nVALUES (1) ON CONFLICT (id) DO UPDATE\nSET a = 1",
		},
	}
	for _, c := range cases {
		if got := Format(c.in); got != c.want {
			t.Errorf("Format(%q)\n got: %q\nwant: %q", c.in, got, c.want)
		}
	}
}

func TestFormat_문자열_따옴표_이름_주석은_그대로_둔다(t *testing.T) {
	in := "select 'from x', E'a\\'b', $$ select $$, $f$ where $f$, \"Select\", t.from /* a /* b */ where */ from t -- where\nwhere x=1"
	want := "SELECT 'from x',\n    E'a\\'b',\n    $$ select $$,\n    $f$ where $f$,\n    \"Select\",\n    t.from /* a /* b */ where */\nFROM t -- where\nWHERE x=1"
	if got := Format(in); got != want {
		t.Fatalf("\n got: %q\nwant: %q", got, want)
	}
}

func TestFormat_줄_주석_뒤에는_항상_줄을_바꾼다(t *testing.T) {
	got := Format("select 1 -- c\n;select a, -- n\n b")
	want := "SELECT 1 -- c\n;\n\nSELECT a,\n    -- n\n    b"
	if got != want {
		t.Fatalf("\n got: %q\nwant: %q", got, want)
	}
}

func TestFormat_여러_문장은_빈_줄로_나누고_끝_세미콜론은_지킨다(t *testing.T) {
	if got := Format("select 1;select 2;  \n"); got != "SELECT 1;\n\nSELECT 2;" {
		t.Fatalf("got %q", got)
	}
	if got := Format("  select 1 ; select 2"); got != "SELECT 1;\n\nSELECT 2" {
		t.Fatalf("got %q", got)
	}
}

func TestFormat_결과를_다시_넣어도_같다(t *testing.T) {
	for _, in := range fuzzSeeds {
		once := Format(in)
		if twice := Format(once); twice != once {
			t.Errorf("not idempotent for %q\n once: %q\ntwice: %q", in, once, twice)
		}
	}
}

func FuzzFormat_멱등이고_토큰을_바꾸지_않는다(f *testing.F) {
	for _, s := range fuzzSeeds {
		f.Add(s)
	}
	f.Fuzz(func(t *testing.T, in string) {
		once := Format(in)
		if twice := Format(once); twice != once {
			t.Fatalf("not idempotent for %q\n once: %q\ntwice: %q", in, once, twice)
		}
		a, b := Tokenize(in), Tokenize(once)
		if len(a) != len(b) {
			t.Fatalf("token count %d -> %d for %q -> %q", len(a), len(b), in, once)
		}
		for i := range a {
			if a[i].Kind != b[i].Kind || !sameText(a[i], b[i]) {
				t.Fatalf("token %d %q -> %q for %q", i, a[i].Text, b[i].Text, in)
			}
		}
	})
}

var fuzzSeeds = []string{
	"",
	"select 1",
	"select a,b from t where a=1 and (b=2 or c=3) order by 1",
	"select case when a and b then 1 else 2 end, x from t where a between 1 and 2 and b",
	"select * from (select 1) s join (values (1),(2)) v(x) on true",
	"select 'x''y', e'\\n', b'01', $a$ $ $a$, $$, \"q\"\"q\" from t",
	"/* unterminated",
	"select 'unterminated",
	"-- only comment",
	"select 1 -- c",
	"select 1;;select 2",
	"select (((select 1)))",
	")) select (",
	"select a::int, x[1:2], $1, :id from t",
	"select 한글, 1from t",
	"update t set a=1 from u where t.id=u.id returning *",
	"select 1 union select 2 intersect select 3 except select 4",
	"natural left join x cross join y full outer join z",
}

func sameText(a, b Token) bool {
	if a.Kind == Word && keywords[strings.ToLower(a.Text)] {
		return strings.EqualFold(a.Text, b.Text)
	}
	return a.Text == b.Text
}
