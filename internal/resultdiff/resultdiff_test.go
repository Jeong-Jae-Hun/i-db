package resultdiff

import (
	"fmt"
	"slices"
	"strings"
	"testing"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
)

func table(pk string, cols []string, rows ...[]string) *db.Result {
	r := &db.Result{Cols: cols, Rows: rows}
	if pk != "" {
		r.Origin = &db.Origin{Table: db.TableRef{Schema: "public", Table: "t"}, PK: []string{pk}, Cols: cols}
	}
	return r
}

// render 는 Rows 를 `+id,name` `-…` `~id,name[1]` 꼴로 한 줄씩 적는다.
func render(r Report) string {
	marks := map[Op]string{Added: "+", Removed: "-", Changed: "~"}
	var out []string
	for _, row := range r.Rows {
		s := marks[row.Op] + strings.Join(row.Row, ",")
		if row.Op == Changed {
			s += fmt.Sprint(row.ChangedCols)
		}
		out = append(out, s)
	}
	return r.Summary() + " " + strings.Join(out, " ")
}

func TestDiff_PK가_같으면_PK로_짝지어_바뀐_칸을_찾는다(t *testing.T) {
	cols := []string{"id", "name", "n"}
	a := table("id", cols, []string{"1", "a", "10"}, []string{"2", "b", "20"}, []string{"3", "c", "NULL"})
	b := table("id", cols, []string{"3", "c", "30"}, []string{"4", "d", "40"}, []string{"1", "a", "10"})
	r, err := Diff(a, b)
	if err != nil {
		t.Fatal(err)
	}
	if got := render(r); got != "+1 −1 ~1 ~3,c,30[2] +4,d,40 -2,b,20" {
		t.Fatal(got)
	}
}

func TestDiff_PK_열을_결과_열_별칭이_아니라_출처_열로_찾는다(t *testing.T) {
	a := table("id", []string{"k", "v"}, []string{"1", "x"})
	a.Origin.Cols = []string{"id", "v"} // select id as k, v
	b := table("id", []string{"k", "v"}, []string{"1", "y"})
	b.Origin.Cols = []string{"id", "v"}
	r, err := Diff(a, b)
	if err != nil {
		t.Fatal(err)
	}
	if got := render(r); got != "+0 −0 ~1 ~1,y[1]" {
		t.Fatal(got)
	}
}

func TestDiff_PK가_없거나_다르면_행_전체를_개수까지_맞춘다(t *testing.T) {
	cols := []string{"a", "b"}
	x, y, z := []string{"1", "x"}, []string{"2", "y"}, []string{"1", "z"}
	cases := []struct {
		name string
		a, b *db.Result
		want string
	}{
		{"PK 없음 중복", table("", cols, x, x, y), table("", cols, x, z, x, x), "+2 −1 ~0 +1,z +1,x -2,y"},
		{"한쪽만 PK", table("a", cols, x), table("", cols, z), "+1 −1 ~0 +1,z -1,x"},
		{"다른 테이블", table("a", cols, x), func() *db.Result {
			r := table("a", cols, z)
			r.Origin.Table.Table = "other"
			return r
		}(), "+1 −1 ~0 +1,z -1,x"},
		{"같은 결과", table("", cols, x, y), table("", cols, y, x), "+0 −0 ~0 "},
		{"둘 다 빈 결과", table("", cols), table("", cols), "+0 −0 ~0 "},
		{"빈 결과로", table("a", cols, x, y), table("a", cols), "+0 −2 ~0 -1,x -2,y"},
	}
	for _, c := range cases {
		r, err := Diff(c.a, c.b)
		if err != nil {
			t.Fatalf("%s: %v", c.name, err)
		}
		if got := render(r); got != c.want {
			t.Errorf("%s: %s", c.name, got)
		}
	}
}

func TestDiff_값_안의_구분자로_다른_행이_같아지지_않는다(t *testing.T) {
	cols := []string{"a", "b"}
	r, err := Diff(table("", cols, []string{"1:x", ""}), table("", cols, []string{"1", ":x"}))
	if err != nil {
		t.Fatal(err)
	}
	if r.Added != 1 || r.Removed != 1 {
		t.Fatal(render(r))
	}
}

func TestDiff_열이_다르면_차이를_적은_오류다(t *testing.T) {
	cases := []struct {
		a, b []string
		want string
	}{
		{[]string{"id", "name"}, []string{"id", "email"}, "열이 달라 비교할 수 없음: -name +email"},
		{[]string{"id", "name"}, []string{"name", "id"}, "열이 달라 비교할 수 없음: 열 순서가 다름"},
		{[]string{"id"}, []string{"id", "id"}, "열이 달라 비교할 수 없음: +id"},
	}
	for _, c := range cases {
		_, err := Diff(table("", c.a), table("", c.b))
		if err == nil || err.Error() != c.want {
			t.Errorf("%v → %v: %v", c.a, c.b, err)
		}
	}
	if _, err := Diff(&db.Result{Text: "UPDATE 1"}, table("", []string{"id"})); err == nil {
		t.Error("표가 아닌 결과를 비교했다")
	}
	if _, err := Diff(nil, table("", []string{"id"})); err == nil {
		t.Error("nil 을 비교했다")
	}
}

func TestAggregate_숫자_칸만_정확히_센다(t *testing.T) {
	cases := []struct {
		name    string
		cells   []string
		numeric []bool
		want    string
	}{
		{"기본", []string{"1", "2", "NULL", "abc", "4"}, []bool{true, true, true, false, true},
			"칸 5 · 합 7 · 평균 2.3333333333 · 최소 1 · 최대 4"},
		{"숫자 칸 없음", []string{"a", "NULL", "3"}, []bool{false, true, false}, "칸 3"},
		{"빈 선택", nil, nil, "칸 0"},
		{"float 정밀도를 넘는 bigint", []string{"9223372036854775807", "9223372036854775807"}, []bool{true, true},
			"칸 2 · 합 18446744073709551614 · 평균 9223372036854775807 · 최소 9223372036854775807 · 최대 9223372036854775807"},
		{"numeric 소수", []string{"0.1", "0.2", "-0.30"}, []bool{true, true, true}, "칸 3 · 합 0 · 평균 0 · 최소 -0.3 · 최대 0.2"},
		{"긴 소수는 잘리지 않는다", []string{"1.000000000000000000001", "1"}, []bool{true, true},
			"칸 2 · 합 2.000000000000000000001 · 평균 1 · 최소 1 · 최대 1.000000000000000000001"},
		{"지수 표기 float", []string{"1.5e+20", "1e-05"}, []bool{true, true},
			"칸 2 · 합 150000000000000000000.00001 · 평균 75000000000000000000.000005 · 최소 0.00001 · 최대 150000000000000000000"},
		{"NaN·Infinity·분수는 숫자가 아니다", []string{"NaN", "Infinity", "-Infinity", "1/3", "5"}, []bool{true, true, true, true, true},
			"칸 5 · 합 5 · 평균 5 · 최소 5 · 최대 5"},
		{"numeric 이 짧으면 나머지는 숫자가 아니다", []string{"1", "2"}, []bool{true}, "칸 2 · 합 1 · 평균 1 · 최소 1 · 최대 1"},
	}
	for _, c := range cases {
		if got := Aggregate(c.cells, c.numeric).String(); got != c.want {
			t.Errorf("%s: %s", c.name, got)
		}
	}
}

func TestAggregate_평균은_끝나는_소수면_정확히_아니면_10자리에서_반올림한다(t *testing.T) {
	s := Aggregate([]string{"2", "-1", "-1", "0", "0", "0"}, slices.Repeat([]bool{true}, 6))
	if got := s.String(); got != "칸 6 · 합 0 · 평균 0 · 최소 -1 · 최대 2" {
		t.Fatal(got)
	}
	s = Aggregate([]string{"2", "0", "0"}, []bool{true, true, true})
	if got := s.String(); got != "칸 3 · 합 2 · 평균 0.6666666667 · 최소 0 · 최대 2" {
		t.Fatal(got)
	}
	s = Aggregate([]string{"1", "0", "0", "0", "0", "0", "0", "0"}, slices.Repeat([]bool{true}, 8))
	if got := s.String(); got != "칸 8 · 합 1 · 평균 0.125 · 최소 0 · 최대 1" {
		t.Fatal(got)
	}
}

func TestAggregate_0이_아닌_아주_작은_평균은_0으로_적지_않고_유효_숫자_10자리를_남긴다(t *testing.T) {
	cases := []struct {
		cells []string
		avg   string
	}{
		{[]string{"1e-12", "1e-12"}, "0.000000000001"},
		{[]string{"1e-12", "0", "0"}, "0.0000000000003333333333"},
		{[]string{"-3e-15", "0"}, "-0.0000000000000015"},
		{[]string{"0.05", "0.1", "0"}, "0.05"},
		{[]string{"0.1", "0", "0"}, "0.03333333333"},
	}
	for _, c := range cases {
		got := Aggregate(c.cells, slices.Repeat([]bool{true}, len(c.cells))).String()
		if !strings.Contains(got, " · 평균 "+c.avg+" · ") {
			t.Errorf("%v: %s, want 평균 %s", c.cells, got, c.avg)
		}
	}
}
