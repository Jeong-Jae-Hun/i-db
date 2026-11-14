package db

import (
	"reflect"
	"strings"
	"testing"
	"unicode"
	"unicode/utf8"
)

func TestStatementAt_커서가_있는_문장만_고른다(t *testing.T) {
	text := "select 1; select 2;"
	if got := StatementAt(text, strings.Index(text, "2")); got != "select 2" {
		t.Fatalf("got %q", got)
	}
	if got := StatementAt(text, 0); got != "select 1" {
		t.Fatalf("got %q", got)
	}
}

func TestStatementAt_따옴표_달러태그_주석_안의_세미콜론은_경계가_아니다(t *testing.T) {
	text := "select ';', \"a;b\" -- x;y\n from t;\n" +
		"create function f() returns int as $$ begin; return 1; end $$ language plpgsql;\n" +
		"select $fmt$ a;b $fmt$ /* c;d */;"
	stmts := []string{}
	for _, probe := range []string{"from t", "return 1", "$fmt$ a"} {
		stmts = append(stmts, StatementAt(text, strings.Index(text, probe)))
	}
	if !strings.HasSuffix(stmts[0], "from t") || !strings.HasPrefix(stmts[0], "select ';'") {
		t.Fatalf("첫 문장: %q", stmts[0])
	}
	if !strings.HasPrefix(stmts[1], "create function") || !strings.HasSuffix(stmts[1], "language plpgsql") {
		t.Fatalf("함수 문장: %q", stmts[1])
	}
	if stmts[2] != "select $fmt$ a;b $fmt$ /* c;d */" {
		t.Fatalf("셋째 문장: %q", stmts[2])
	}
}

func TestStatementAt_마지막_세미콜론_뒤_커서는_뒤_조각(t *testing.T) {
	text := "select 1;\nselect 2"
	if got := StatementAt(text, len(text)); got != "select 2" {
		t.Fatalf("got %q", got)
	}
}

func TestLineAt_커서가_있는_줄을_고른다(t *testing.T) {
	text := "GET a\nHGETALL b\n"
	if got := LineAt(text, strings.Index(text, "HGET")+2); got != "HGETALL b" {
		t.Fatalf("got %q", got)
	}
}

func TestSplitArgs_따옴표_안_공백을_유지한다(t *testing.T) {
	got := SplitArgs(`SET "a b" 'c d' "x\"y"`)
	want := []string{"SET", "a b", "c d", `x"y`}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("got %q", got)
	}
	if got := SplitArgs("GET 세션:키 값"); !reflect.DeepEqual(got, []string{"GET", "세션:키", "값"}) {
		t.Fatalf("한글 인자: %q", got)
	}
	if got := SplitArgs(`GET ""`); !reflect.DeepEqual(got, []string{"GET", ""}) {
		t.Fatalf("빈 인자: %q", got)
	}
}

func TestIsReadOnlySQL_첫_단어로_읽기_전용을_가른다(t *testing.T) {
	read := []string{"select 1", "  WITH x as (select 1) select * from x", "-- 주석\nselect 1", "/* a */ explain select 1", "(select 1)"}
	write := []string{"update t set a=1", "delete from t", "insert into t values (1)", "create table t()", "drop table t",
		"explain analyze delete from t", "EXPLAIN (ANALYZE, BUFFERS) update t set a=1"}
	for _, s := range read {
		if !IsReadOnlySQL(s) {
			t.Errorf("읽기여야 한다: %q", s)
		}
	}
	for _, s := range write {
		if IsReadOnlySQL(s) {
			t.Errorf("쓰기여야 한다: %q", s)
		}
	}
}

func TestIsReadOnlyRedis_읽기_명령만_통과한다(t *testing.T) {
	if !IsReadOnlyRedis([]string{"hgetall", "k"}) || IsReadOnlyRedis([]string{"DEL", "k"}) || IsReadOnlyRedis(nil) {
		t.Fatal("판정이 다르다")
	}
}

func FuzzStatementAt_패닉없이_원문의_일부를_돌려준다(f *testing.F) {
	for _, s := range []string{"select 1; select 2;", "$$ a;b $$;", "'it''s';--x\n;", "/* ; */ select $t$ ; $t$;", "\"a;\"", "$1"} {
		f.Add(s, 3)
	}
	f.Fuzz(func(t *testing.T, text string, offset int) {
		if offset < 0 || offset > len(text) {
			return
		}
		if got := StatementAt(text, offset); !strings.Contains(text, got) {
			t.Fatalf("원문에 없는 문장: %q / %q", text, got)
		}
	})
}

func FuzzSplitArgs_패닉없이_따옴표_없는_입력은_공백으로만_나눈다(f *testing.F) {
	for _, s := range []string{`GET "a b"`, `SET 'x' "y\"z"`, `"unterminated`, `\`, "", "GET 세션 값\r"} {
		f.Add(s)
	}
	f.Fuzz(func(t *testing.T, line string) {
		args := SplitArgs(line)
		if strings.ContainsAny(line, `"'\`) || !utf8.ValidString(line) {
			return
		}
		if want := strings.Fields(line); len(args)+len(want) > 0 && !reflect.DeepEqual(args, want) {
			t.Fatalf("%q → %q, want %q", line, args, want)
		}
	})
}

func TestStatementAt_E문자열_중첩주석_식별자_달러(t *testing.T) {
	cases := []struct{ text, probe, want string }{
		{`SELECT E'it\'s; x'; DELETE FROM t;`, "DELETE", "DELETE FROM t"},
		{`SELECT E'it\'s; x'; DELETE FROM t;`, "SELECT", `SELECT E'it\'s; x'`},
		{"select /* a /* b; */ c; */ 1; select 2;", "select 2", "select 2"},
		{"select a$b; select 2;", "select 2", "select 2"},
		{"select 'e\\'; select 2;", "select 2", "select 2"}, // E 접두가 없으면 백슬래시는 이스케이프가 아니다
	}
	for _, c := range cases {
		if got := StatementAt(c.text, strings.Index(c.text, c.probe)); got != c.want {
			t.Errorf("%q @%q: got %q want %q", c.text, c.probe, got, c.want)
		}
	}
}

func TestIsReadOnly_EXPLAIN_옵션_ANALYZE와_Redis_관리_하위명령(t *testing.T) {
	for _, s := range []string{"EXPLAIN (ANALYZE) DELETE FROM t", "explain (format json, analyze true) update t set a=1", "EXPLAIN ANALYSE DELETE FROM t", "explain (analyse) delete from t"} {
		if IsReadOnlySQL(s) {
			t.Errorf("쓰기여야 한다: %q", s)
		}
	}
	for _, args := range [][]string{{"SLOWLOG", "RESET"}, {"client", "kill", "x"}, {"CLIENT", "PAUSE", "1"}, {"MEMORY", "PURGE"}, {"CLIENT"}} {
		if IsReadOnlyRedis(args) {
			t.Errorf("쓰기여야 한다: %v", args)
		}
	}
	// Redis 모니터(R34)가 보내는 명령도 읽기다
	for _, args := range [][]string{{"SLOWLOG", "GET"}, {"client", "list"}, {"MEMORY", "USAGE", "k"}, {"SLOWLOG", "GET", "50"}, {"info"}, {"SCAN", "0", "COUNT", "500"}, {"TYPE", "k"}, {"TTL", "k"}} {
		if !IsReadOnlyRedis(args) {
			t.Errorf("읽기여야 한다: %v", args)
		}
	}
}

func TestIsReadOnlySQL_중첩_주석과_여러_줄_주석을_건너뛴다(t *testing.T) {
	if IsReadOnlySQL("/* /* */ with */ delete from t") {
		t.Error("중첩 주석 뒤 delete 는 쓰기여야 한다")
	}
	if FirstSQLWord("/*\n설명\n*/\n-- 한 줄\nselect * from big") != "select" {
		t.Error("여러 줄 주석 뒤 select 를 찾아야 한다")
	}
	if FirstSQLWord("/* 안 닫힘 select") != "" {
		t.Error("닫히지 않은 주석은 첫 단어가 없다")
	}
}

func TestSplitStatements_StatementAt과_같은_경계로_나누고_빈_조각은_뺀다(t *testing.T) {
	text := "select ';';\n-- 주석만\n;\n" +
		"create function f() returns int as $$ begin; return 1; end $$ language plpgsql;\n" +
		"select E'a\\';b', /* x; /* y; */ z; */ 1 ;;  \n select 3 -- 끝"
	want := []string{
		"select ';'",
		"create function f() returns int as $$ begin; return 1; end $$ language plpgsql",
		"select E'a\\';b', /* x; /* y; */ z; */ 1",
		"select 3 -- 끝",
	}
	if got := SplitStatements(text); !reflect.DeepEqual(got, want) {
		t.Fatalf("got %q", got)
	}
	if got := SplitStatements(" \n-- x\n/* y */ ; "); got != nil {
		t.Fatalf("주석·공백뿐이면 비어야 한다: %q", got)
	}
}

func FuzzSplitStatements_문장마다_원문의_일부이고_그_안의_커서는_그_문장을_고른다(f *testing.F) {
	for _, s := range []string{"select 1; select 2;", "$$ a;b $$;", "'it''s';--x\n;", "/* ; */ select $t$ ; $t$;", "E'\\';' ; x", "a$b$; c", "; ;\u0085;"} {
		f.Add(s)
	}
	f.Fuzz(func(t *testing.T, text string) {
		got := SplitStatements(text)
		i := 0
		for _, sp := range scanStatements(text) {
			seg := text[sp.start:sp.end]
			stmt := strings.TrimSpace(seg)
			if !sp.code || stmt == "" {
				continue
			}
			if i >= len(got) || got[i] != stmt {
				t.Fatalf("%q: %d번째 문장이 다르다: %q", text, i, got)
			}
			from := sp.start + len(seg) - len(strings.TrimLeftFunc(seg, unicode.IsSpace))
			for off := from; off <= from+len(stmt); off++ {
				if at := StatementAt(text, off); at != stmt {
					t.Fatalf("%q @%d: StatementAt %q, want %q", text, off, at, stmt)
				}
			}
			i++
		}
		if i != len(got) {
			t.Fatalf("%q: 문장 수가 다르다: %q", text, got)
		}
	})
}

func TestFirstSQLWord_식별자_글자가_아닌_곳에서_끊는다(t *testing.T) {
	cases := map[string]string{
		"select*from t":                    "select",
		"select'x'":                        "select",
		`select"a"from t`:                  "select",
		"(select 1)":                       "select",
		"( /* c */ (select 1))":            "select",
		"with/*c*/x as(select 1) select 1": "with",
		"copy(select 1) to stdout":         "copy",
	}
	for in, want := range cases {
		if got := FirstSQLWord(in); got != want {
			t.Errorf("%q → %q, want %q", in, got, want)
		}
		if want != "copy" && !IsReadOnlySQL(in) {
			t.Errorf("읽기여야 한다: %q", in)
		}
	}
}
