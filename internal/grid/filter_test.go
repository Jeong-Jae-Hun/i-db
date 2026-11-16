package grid

import (
	"strings"
	"testing"

	"github.com/charmbracelet/x/ansi"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
)

func TestParseFilter_꼴마다_열_연산자_값으로_읽는다(t *testing.T) {
	cols := []string{"id", "Status", "email"}
	cases := []struct {
		expr string
		want Filter
	}{
		{"abc", Filter{Col: -1, Op: OpContains, Value: "abc"}},
		{"!abc", Filter{Col: -1, Op: OpNotContains, Value: "abc"}},
		{"status:act", Filter{Col: 1, Name: "Status", Op: OpContains, Value: "act"}},
		{"STATUS : act ", Filter{Col: 1, Name: "Status", Op: OpContains, Value: "act"}},
		{"status=ACTIVE", Filter{Col: 1, Name: "Status", Op: OpEq, Value: "ACTIVE"}},
		{"status!=ACTIVE", Filter{Col: 1, Name: "Status", Op: OpNe, Value: "ACTIVE"}},
		{"id>100", Filter{Col: 0, Name: "id", Op: OpGt, Value: "100"}},
		{"id>=100", Filter{Col: 0, Name: "id", Op: OpGe, Value: "100"}},
		{"id<5", Filter{Col: 0, Name: "id", Op: OpLt, Value: "5"}},
		{"id <= 5", Filter{Col: 0, Name: "id", Op: OpLe, Value: "5"}},
		{"email=null", Filter{Col: 2, Name: "email", Op: OpNull, Value: "null"}},
		{"email!=NULL", Filter{Col: 2, Name: "email", Op: OpNotNull, Value: "NULL"}},
		{"nocol=1", Filter{Col: -1, Op: OpContains, Value: "nocol=1"}},
		{"http://x", Filter{Col: -1, Op: OpContains, Value: "http://x"}},
		{"=x", Filter{Col: -1, Op: OpContains, Value: "=x"}},
		{"!status=x", Filter{Col: -1, Op: OpNotContains, Value: "status=x"}},
		{"", Filter{Col: -1, Op: OpContains, Value: ""}},
	}
	for _, c := range cases {
		if got := ParseFilter(c.expr, cols); got != c.want {
			t.Errorf("%q: got %+v want %+v", c.expr, got, c.want)
		}
	}
}

func TestFilterSQL_조건을_WHERE_식으로_바꾸고_값은_감싼다(t *testing.T) {
	cols := []string{"id", "name", "로그"}
	cases := []struct {
		expr    string
		numeric bool
		want    string
	}{
		{"name:a_b%c", false, `"name"::text ILIKE E'%a\\_b\\%c%'`},
		{`name:a\b`, false, `"name"::text ILIKE E'%a\\\\b%'`},
		{"name=O'Neil", false, `"name"::text = 'O''Neil'`},
		{"name!=x", false, `"name"::text <> 'x'`},
		{"id=7", true, `"id" = 7`},
		{"id!=7", true, `"id" <> 7`},
		{"id=x", true, `"id"::text = 'x'`},
		{"id>100", true, `"id" > 100`},
		{"id<=-1.5e3", true, `"id" <= -1.5e3`},
		{"id>abc", true, `"id"::text > 'abc'`},
		{"id>NaN", true, `"id"::text > 'NaN'`},
		{"name>m", false, `"name"::text > 'm'`},
		{"로그=null", false, `"로그" IS NULL`},
		{"로그!=null", false, `"로그" IS NOT NULL`},
	}
	for _, c := range cases {
		got, ok := ParseFilter(c.expr, cols).SQL(c.numeric)
		if !ok || got != c.want {
			t.Errorf("%q: got %q %v want %q", c.expr, got, ok, c.want)
		}
	}
	if _, ok := ParseFilter("abc", cols).SQL(false); ok {
		t.Error("모든 열 찾기는 SQL 로 바꾸지 않는다")
	}
}

func filterResult() *db.Result {
	return &db.Result{
		Cols:  []string{"id", "name", "n", "t"},
		Types: []string{"int4", "text", "int4", "text"},
		Rows: [][]string{
			{"1", "Bob", "9", "9"},
			{"2", "bob", "10", "10"},
			{"3", "carol", "100", "100"},
			{"4", "NULL", "NULL", "NULL"},
		},
	}
}

func TestFilter_연산자마다_받은_행을_거른다(t *testing.T) {
	m := newGrid(filterResult(), [2]int{80, 12})
	cases := []struct{ q, want string }{
		{"BOB", "1,2"},
		{"!bob", "3,4"},
		{"name:OB", "1,2"},
		{"name=Bob", "1"},    // 같음은 대소문자 구분
		{"name!=Bob", "2,3"}, // NULL 은 다름에도 맞지 않는다(SQL 과 같음)
		{"n>9", "2,3"},       // 숫자 열은 숫자로
		{"n=10.0", "2"},      // 같음도 숫자 열에 십진수면 숫자로
		{"t>9", ""},          // 글자 열은 글자로: "10"·"100" < "9"
		{"t<9", "2,3"},       //
		{"n>=10", "2,3"},     //
		{"n<10", "1"},        //
		{"n<=abc", "1,2,3"},  // 숫자 열이라도 값이 숫자가 아니면 글자로
		{"n=null", "4"},      //
		{"n!=null", "1,2,3"}, //
		{"nocol>1", ""},      // 모르는 열이면 글 전체 찾기
		{"name=", ""},        //
		{"name:", "1,2,3"},   // 빈 열 찾기는 SQL ILIKE '%%' 처럼 NULL 을 뺀다
	}
	for _, c := range cases {
		g, _ := press(m, "/", c.q, "enter")
		if got := strings.Join(viewed(g, 0), ","); got != c.want {
			t.Errorf("%q: got %q want %q", c.q, got, c.want)
		}
	}
}

func TestCycleSort_오름_내림_없음으로_돌고_위_줄에_보인다(t *testing.T) {
	m := newGrid(filterResult(), [2]int{80, 12})
	m, _ = press(m, "l", "l") // n 열
	steps := []struct{ order, summary string }{
		{"1,2,3,4", "정렬 n ↑"},
		{"3,2,1,4", "정렬 n ↓"},
		{"1,2,3,4", ""},
	}
	for _, s := range steps {
		m.CycleSort()
		if got := strings.Join(viewed(m, 0), ","); got != s.order || m.Summary() != s.summary {
			t.Fatalf("got %q %q want %q %q", got, m.Summary(), s.order, s.summary)
		}
	}
	m, _ = press(m, "/", "n>9", "enter")
	m.CycleSort()
	if got := m.Summary(); got != "정렬 n ↑ · 거르기 n>9 · 2/4행 · esc 해제" {
		t.Fatalf("정렬·거르기 줄: %q", got)
	}
	m, _ = press(m, "/", "\x1b[31m", "enter")
	if strings.ContainsRune(m.Summary(), '\x1b') || ansi.StringWidth(m.Summary()) == 0 {
		t.Fatalf("제어 문자는 치환한다: %q", m.Summary())
	}
}

func TestCurrentCol_FocusCol_은_이름으로_커서_열을_지킨다(t *testing.T) {
	m := newGrid(filterResult(), [2]int{80, 12})
	m, _ = press(m, "l", "l")
	if m.CurrentCol() != "n" {
		t.Fatalf("커서 열: %q", m.CurrentCol())
	}
	m.SetResult(filterResult())
	m.FocusCol("n")
	if m.CurrentCol() != "n" {
		t.Fatalf("새 결과에서 같은 이름 열: %q", m.CurrentCol())
	}
	m.FocusCol("없는열")
	if m.CurrentCol() != "n" {
		t.Fatal("없는 이름이면 그대로")
	}
}

func TestParseFilter_대소문자까지_같은_열을_먼저_고른다(t *testing.T) {
	if f := ParseFilter("Note=x", []string{"note", "Note"}); f.Col != 1 || f.Name != "Note" {
		t.Fatalf("같은 열: %+v", f)
	}
	if f := ParseFilter("NOTE=x", []string{"note", "Note"}); f.Col != 0 {
		t.Fatalf("없으면 대소문자 무시로 앞 열: %+v", f)
	}
}

// ─── R14.7 더 읽기 ───

func TestPageSQL_정렬_거르기가_없어도_감싸고_OFFSET_을_붙인다(t *testing.T) {
	m := newGrid(filterResult(), [2]int{80, 12})
	if got := m.PageSQL("select 1 -- c", 1000); got != "SELECT * FROM (\nselect 1 -- c\n) AS _q OFFSET 1000" {
		t.Fatalf("감싸기: %q", got)
	}
	m.FocusCol("n")
	m.CycleSort()
	m, _ = press(m, "/", "n>1", "enter")
	want := "SELECT * FROM (\nb\n) AS _q WHERE \"n\" > 1 ORDER BY \"n\" ASC NULLS LAST OFFSET 2000"
	if got := m.PageSQL("b", 2000); got != want {
		t.Fatalf("정렬·거르기와 같이: %q", got)
	}
}

func TestExtend_행을_덧붙여도_정렬_거르기_커서를_둔다(t *testing.T) {
	r := filterResult()
	m := newGrid(r, [2]int{80, 12})
	m.FocusCol("n")
	m.SetQuery(Query{SortCol: "n", Desc: true}, true)
	m, _ = press(m, "j")
	before := m.CurrentRow()
	more := *r
	more.Rows = append(append([][]string{}, r.Rows...), r.Rows[0])
	m.Extend(&more)
	if m.CurrentCol() != "n" || strings.Join(m.CurrentRow(), ",") != strings.Join(before, ",") || m.Summary() != "정렬 n ↓" {
		t.Fatalf("커서·정렬을 둔다: %q %v %q", m.CurrentCol(), m.CurrentRow(), m.Summary())
	}
	if len(viewed(m, 0)) != len(more.Rows) {
		t.Fatalf("덧붙인 행이 보인다: %d", len(viewed(m, 0)))
	}
}
