package complete

import (
	"context"
	"os"
	"reflect"
	"slices"
	"strings"
	"testing"
	"time"

	"github.com/jackc/pgx/v5"
)

var testCatalog = &Catalog{
	Schemas: []Schema{
		{Name: "it", Tables: []Table{
			{Name: "events", Columns: []Column{{"id", "bigint"}, {"payload", "jsonb"}}},
			{Name: "users", Columns: []Column{{"uid", "uuid"}}},
		}},
		{Name: "public", Tables: []Table{
			{Name: "orders", Columns: []Column{{"id", "integer"}, {"user_id", "integer"}, {"total", "numeric(10,2)"}}},
			{Name: "users", Columns: []Column{{"id", "integer"}, {"name", "text"}, {"Email", "text"}, {"order", "integer"}}},
			{Name: "active_users", View: true, Columns: []Column{{"id", "integer"}}},
		}},
	},
	Functions: []string{"my_func"},
}

// suggest 는 텍스트의 '|' 자리를 커서로 본다.
func suggest(text string) []Item {
	off := strings.IndexByte(text, '|')
	return SuggestSQL(testCatalog, Query{Text: text[:off] + text[off+1:], Offset: off})
}

func labels(items []Item) []string {
	out := make([]string, len(items))
	for i, it := range items {
		out[i] = it.Label
	}
	return out
}

func find(items []Item, label string) (Item, bool) {
	for _, it := range items {
		if it.Label == label {
			return it, true
		}
	}
	return Item{}, false
}

func TestSuggestSQL_문맥별_후보(t *testing.T) {
	cases := []struct {
		name string
		text string
		want []string // 앞에서부터 이 순서로 나와야 한다
		not  []string // 나오면 안 된다
	}{
		{"FROM 뒤는 public 테이블이 먼저, 다른 스키마는 한정", "select * from |", []string{"orders", "users", "active_users", "it.events", "it.users", "it", "public"}, []string{"SELECT", "id"}},
		{"FROM 뒤 접두어", "select * from us|", []string{"users", "it.users"}, []string{"orders"}},
		{"접두 일치가 부분 일치보다 앞", "select * from user|", []string{"users", "it.users", "active_users"}, nil},
		{"JOIN 뒤 테이블", "select * from users u left join o|", []string{"orders"}, nil},
		{"UPDATE 뒤 테이블", "update o|", []string{"orders"}, nil},
		{"INSERT INTO 뒤 테이블", "insert into |", []string{"orders", "users"}, nil},
		{"FROM 쉼표 목록", "select * from users u, o|", []string{"orders"}, nil},
		{"스키마. 뒤는 그 스키마 테이블", "select * from it.|", []string{"events", "users"}, []string{"orders"}},
		{"별칭. 뒤는 열", "select u.| from users u", []string{"id", "name", "Email", "order"}, []string{"user_id", "SELECT"}},
		{"AS 별칭", "select o.t| from users join orders as o on true", []string{"total"}, nil},
		{"테이블 이름. 뒤는 열", "select users.na| from users", []string{"name"}, nil},
		{"스키마 한정 테이블의 별칭", "select e.| from it.events e", []string{"id", "payload"}, nil},
		{"스키마.테이블. 뒤는 열", "select it.events.p| from it.events", []string{"payload"}, nil},
		{"SELECT 목록은 참조 테이블 열", "select | from users", []string{"id", "name", "Email", "order"}, []string{"total"}},
		{"WHERE 는 모든 참조 테이블 열", "select * from users u join orders o on o.user_id = u.id where t|", []string{"total"}, nil},
		{"ORDER BY 는 열", "select * from orders order by |", []string{"id", "user_id", "total"}, nil},
		{"SET 은 열", "update users set na|", []string{"name"}, nil},
		{"RETURNING 은 열", "delete from orders returning |", []string{"id"}, nil},
		{"INSERT 열 목록", "insert into public.orders (id, |", []string{"id", "user_id", "total"}, []string{"name", "SELECT"}},
		{"서브쿼리 안의 SELECT", "select * from users where id in (select | from orders)", []string{"id", "user_id", "total", "name"}, nil},
		{"상관 서브쿼리는 바깥 별칭도 본다", "select * from users u where exists (select 1 from orders o where o.user_id = u.|)", []string{"id", "name"}, nil},
		{"함수 인자는 바깥 문맥", "select count(|) from orders", []string{"id"}, nil},
		{"열 문맥에 함수", "select co| from orders", []string{"count", "coalesce", "concat", "concat_ws"}, nil},
		{"카탈로그 함수", "select my| from orders", []string{"my_func"}, nil},
		{"문장 처음은 키워드", "sel|", []string{"SELECT"}, nil},
		{"FROM 테이블 뒤는 키워드", "select * from users |", []string{"SELECT", "FROM", "WHERE"}, []string{"orders"}},
		{"ORDER 뒤는 키워드", "select * from users order |", []string{"BY", "SELECT"}, []string{"id"}},
		{"LEFT 뒤는 JOIN", "select * from users left |", []string{"JOIN", "OUTER"}, nil},
		{"LIMIT 뒤는 열이 아니다", "select * from users limit |", nil, []string{"id"}},
		{"커서 있는 문장만 본다", "select * from orders; select | from users", []string{"id", "name"}, []string{"total"}},
		{"앞 문장 FROM 은 뒤 문장에 안 샌다", "select * from |; select 1", []string{"orders"}, nil},
		{"주석은 건너뛴다", "select * from /* x */ o|", []string{"orders"}, nil},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			got := labels(suggest(c.text))
			if len(got) > maxItems {
				t.Fatalf("후보 %d개", len(got))
			}
			if len(c.want) > 0 && (len(got) < len(c.want) || !reflect.DeepEqual(got[:len(c.want)], c.want)) {
				t.Fatalf("%q → %v, want 앞부분 %v", c.text, got, c.want)
			}
			for _, n := range c.not {
				if slices.Contains(got, n) {
					t.Fatalf("%q → %v 에 %q 가 있으면 안 된다", c.text, got, n)
				}
			}
		})
	}
}

func TestSuggestSQL_문자열_주석_숫자_별칭_안에서는_없다(t *testing.T) {
	for _, text := range []string{
		"select 'fr|' from users",
		"select 'it''s |",
		"select E'\\'|",
		"select $$ from |$$",
		"select $t$ x |",
		"select 1 -- from |",
		"select /* from | */ 1",
		"select /* /* */ from |",
		"select 12|",
		"select 'a'|",
		"select id as |",
	} {
		if got := suggest(text); len(got) != 0 {
			t.Fatalf("%q → %v", text, labels(got))
		}
	}
}

func TestSuggestSQL_삽입값(t *testing.T) {
	cases := []struct {
		text, label, kind, detail, insert string
		prefixLen                         int
	}{
		{"select * from |", "it.events", KindTable, "it", "it.events", 0},
		{"select * from act|", "active_users", KindView, "", "active_users", 3},
		{"select u.| from users u", "Email", KindColumn, "text", `"Email"`, 0},
		{"select u.| from users u", "order", KindColumn, "integer", `"order"`, 0},
		{"select | from orders", "total", KindColumn, "numeric(10,2)", "total", 0},
		{"select co| from orders", "count", KindFunction, "", "count(", 2},
		{"sel|", "SELECT", KindKeyword, "", "select", 3},
		{"SEL|", "SELECT", KindKeyword, "", "SELECT", 3},
		{"|", "SELECT", KindKeyword, "", "SELECT", 0},
		{"select * from |", "it", KindSchema, "", "it", 0},
		{"select | from users u", "u", KindTable, "users", "u", 0},
	}
	for _, c := range cases {
		it, ok := find(suggest(c.text), c.label)
		if !ok {
			t.Fatalf("%q 에 %q 없음: %v", c.text, c.label, labels(suggest(c.text)))
		}
		if want := (Item{Label: c.label, Kind: c.kind, Detail: c.detail, Insert: c.insert, PrefixLen: c.prefixLen}); it != want {
			t.Fatalf("%q → %+v, want %+v", c.text, it, want)
		}
	}
}

func TestSuggestSQL_카탈로그_없으면_키워드_함수만(t *testing.T) {
	got := SuggestSQL(nil, Query{Text: "select co from users u where u.", Offset: 9})
	if !slices.Contains(labels(got), "count") || slices.ContainsFunc(got, func(it Item) bool { return it.Kind == KindColumn }) {
		t.Fatalf("%v", labels(got))
	}
	if got := SuggestSQL(nil, Query{Text: "select u.", Offset: 9}); len(got) != 0 {
		t.Fatalf("%v", labels(got))
	}
	if got := SuggestSQL(nil, Query{Text: "select * from ", Offset: 14}); len(got) != 0 {
		t.Fatalf("%v", labels(got))
	}
}

func TestSuggestSQL_오프셋_범위밖은_끝으로_맞춘다(t *testing.T) {
	q := Query{Text: "select * from us"}
	want := labels(SuggestSQL(testCatalog, Query{Text: q.Text, Offset: len(q.Text)}))
	q.Offset = 999
	if got := labels(SuggestSQL(testCatalog, q)); !reflect.DeepEqual(got, want) {
		t.Fatalf("%v != %v", got, want)
	}
	q.Offset = -5
	SuggestSQL(testCatalog, q)
}

func TestSuggestSQL_후보는_50개까지_중복없이(t *testing.T) {
	cat := &Catalog{Schemas: []Schema{{Name: "public"}}}
	for i := 0; i < 200; i++ {
		cat.Schemas[0].Tables = append(cat.Schemas[0].Tables, Table{Name: "t" + strings.Repeat("x", i)})
	}
	cat.Schemas[0].Tables = append(cat.Schemas[0].Tables, Table{Name: "t"}, Table{Name: "T"})
	got := SuggestSQL(cat, Query{Text: "select * from t", Offset: 15})
	if len(got) != maxItems {
		t.Fatalf("후보 %d개", len(got))
	}
	seen := map[string]bool{}
	for _, it := range got {
		if seen[it.Label] {
			t.Fatalf("중복 %q", it.Label)
		}
		seen[it.Label] = true
	}
}

func TestTableRefs_별칭과_스키마를_모은다(t *testing.T) {
	text := `select * from public.users u, orders as "O" join it.events e on true left join x where 1
		; update accounts set a = 1; insert into logs (a) values (1)`
	got := tableRefs(text, tokenize(text))
	want := []tableRef{
		{"public", "users", "u"}, {"", "orders", "O"}, {"it", "events", "e"}, {"", "x", ""},
		{"", "accounts", ""}, {"", "logs", ""},
	}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("%+v", got)
	}
}

func TestTokenize_문자열_주석_식별자_경계(t *testing.T) {
	text := `select "a""b", E'x\'y', 'it''s', $t$ ; $t$, $1, 1.5 -- c` + "\n" + `/* /* */ */ a$b;`
	var got []string
	for _, tk := range tokenize(text) {
		got = append(got, text[tk.start:tk.end])
	}
	want := []string{"select", `"a""b"`, ",", `E'x\'y'`, ",", `'it''s'`, ",", "$t$ ; $t$", ",", "$1", ",", "1.5", "-- c", "/* /* */ */", "a$b", ";"}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("%q", got)
	}
	if name := identName(text, tokenize(text)[1]); name != `a"b` {
		t.Fatalf("identName %q", name)
	}
}

func TestTokenize_닫히지_않은_토큰은_open(t *testing.T) {
	for _, text := range []string{`'abc`, `"abc`, `/* abc`, `$x$ abc`, `-- abc`} {
		toks := tokenize(text)
		if len(toks) != 1 || !toks[0].open || toks[0].end != len(text) {
			t.Fatalf("%q → %+v", text, toks)
		}
	}
}

func TestQuoteIdent_필요할_때만_따옴표(t *testing.T) {
	cases := map[string]string{
		"users": "users", "user_id2": "user_id2", "Email": `"Email"`, "order": `"order"`, "user": `"user"`,
		"1a": `"1a"`, "a b": `"a b"`, `q"t`: `"q""t"`, "": `""`, "한글": `"한글"`, "a$": "a$",
	}
	for in, want := range cases {
		if got := quoteIdent(in); got != want {
			t.Fatalf("%q → %q, want %q", in, got, want)
		}
	}
}

func TestRank_접두일치_먼저_입력순서_유지(t *testing.T) {
	items := []Item{
		{Label: "abc"}, {Label: "xab"}, {Label: "s.ab", Kind: KindTable, Detail: "s"}, {Label: "k.ab", Kind: KindKey},
		{Label: "AB"}, {Label: "ab"}, {Label: "ab"}, {Label: "zzz"},
	}
	got := rank(items, typed{name: "Ab", size: 2})
	if want := []string{"abc", "s.ab", "AB", "ab", "xab", "k.ab"}; !reflect.DeepEqual(labels(got), want) {
		t.Fatalf("%v, want %v", labels(got), want)
	}
	if slices.ContainsFunc(got, func(it Item) bool { return it.PrefixLen != 2 }) {
		t.Fatalf("PrefixLen: %+v", got)
	}
	if items[0].PrefixLen != 0 {
		t.Fatalf("입력을 바꾸면 안 된다: %+v", items[0])
	}
	if got := rank(nil, typed{name: "a"}); got != nil {
		t.Fatalf("%v", got)
	}
}

func TestSuggestSQL_대소문자만_다른_열은_둘_다_남는다(t *testing.T) {
	cat := &Catalog{Schemas: []Schema{{Name: "public", Tables: []Table{{Name: "t", Columns: []Column{{"ID", "int"}, {"id", "int"}}}}}}}
	got := SuggestSQL(cat, Query{Text: "select i from t", Offset: 8})
	a, okA := find(got, "ID")
	b, okB := find(got, "id")
	if !okA || !okB || a.Insert != `"ID"` || b.Insert != "id" {
		t.Fatalf("%+v", got)
	}
}

func TestQuoteIdent_예약어는_따옴표를_씌운다(t *testing.T) {
	for _, w := range []string{"to", "verbose", "authorization", "binary", "collation", "concurrently", "freeze", "isnull",
		"notnull", "overlaps", "tablesample", "current_schema", "current_catalog", "current_role", "system_user",
		"deferrable", "initially", "placing", "left", "join"} {
		if got := quoteIdent(w); got != `"`+w+`"` {
			t.Errorf("%q → %q", w, got)
		}
	}
	// 예약어가 아닌 절 키워드는 이름으로 써도 된다
	for _, w := range []string{"comment", "update", "set", "values", "name"} {
		if got := quoteIdent(w); got != w {
			t.Errorf("%q → %q", w, got)
		}
	}
	it, ok := find(SuggestSQL(&Catalog{Schemas: []Schema{{Name: "public", Tables: []Table{{Name: "t", Columns: []Column{{"to", "int"}}}}}}},
		Query{Text: "select t from t", Offset: 8}), "to")
	if !ok || it.Insert != `"to"` {
		t.Fatalf("%+v", it)
	}
}

func TestReservedWords_서버의_예약어를_다_담는다(t *testing.T) {
	url := os.Getenv("DBTUI_TEST_PG_URL")
	if url == "" {
		t.Skip("DBTUI_TEST_PG_URL not set")
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	conn, err := pgx.Connect(ctx, url)
	if err != nil {
		t.Fatal(err)
	}
	defer conn.Close(ctx)
	rows, err := conn.Query(ctx, "select word from pg_get_keywords() where catcode in ('R', 'T')")
	if err != nil {
		t.Fatal(err)
	}
	words, err := pgx.CollectRows(rows, pgx.RowTo[string])
	if err != nil {
		t.Fatal(err)
	}
	if len(words) == 0 {
		t.Fatal("pg_get_keywords() 가 비었다")
	}
	for _, w := range words {
		if !slices.Contains(reservedWords, w) {
			t.Errorf("reservedWords 에 %q 가 없다", w)
		}
	}
}

func TestSuggestSQL_PrefixLen_은_바꿀_원문_바이트_수다(t *testing.T) {
	cat := &Catalog{Schemas: []Schema{{Name: "public", Tables: []Table{{Name: "My Table"}}}}}
	cases := []struct {
		cat         *Catalog
		text, label string
		want        int
	}{
		{cat, `select * from "My Ta|`, "My Table", 6},
		{cat, `select * from "My Ta| where`, "My Table", 6},
		{nil, "sel|", "SELECT", 3},
		{nil, "|", "SELECT", 0},
		{testCatalog, "select u.na| from users u", "name", 2},
		{testCatalog, "select * from it.ev|", "events", 2},
	}
	for _, c := range cases {
		off := strings.IndexByte(c.text, '|')
		got := SuggestSQL(c.cat, Query{Text: c.text[:off] + c.text[off+1:], Offset: off})
		if it, ok := find(got, c.label); !ok || it.PrefixLen != c.want {
			t.Fatalf("%q → %+v, want PrefixLen %d", c.text, it, c.want)
		}
	}
}

func FuzzSuggestSQL_패닉없이_50개_이하(f *testing.F) {
	for _, s := range []string{"select * from u", "select u. from users u", "insert into t (", "'", "$a$", "/*", `"x`, "a.b.c.", "((", "))"} {
		f.Add(s, len(s))
	}
	f.Fuzz(func(t *testing.T, text string, offset int) {
		if got := SuggestSQL(testCatalog, Query{Text: text, Offset: offset}); len(got) > maxItems {
			t.Fatalf("후보 %d개", len(got))
		}
		toks := tokenize(text)
		for i, tk := range toks {
			if tk.start >= tk.end || tk.end > len(text) || i > 0 && toks[i-1].end > tk.start {
				t.Fatalf("토큰 범위 %+v", toks)
			}
		}
	})
}
