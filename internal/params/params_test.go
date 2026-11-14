package params

import (
	"slices"
	"testing"
)

func TestBind_이름_자리를_번호로_바꾸고_나머지는_건드리지_않는다(t *testing.T) {
	sql, names := Bind(`select ':x', "a:b", x::int, :id, :id, arr[1:2], a[lo:hi], $$ :no $$, :name -- :c` + "\n/* :d */")
	want := `select ':x', "a:b", x::int, $1, $1, arr[1:2], a[lo:hi], $$ :no $$, $2 -- :c` + "\n/* :d */"
	if sql != want || !slices.Equal(names, []string{"id", "name"}) {
		t.Fatalf("got %q %v", sql, names)
	}
}

func TestBind_아래쪽을_생략한_배열_조각은_자리로_보지_않는다(t *testing.T) {
	sql, names := Bind("select a[:n], b[ :m], :id")
	if sql != "select a[:n], b[ $1], $2" || !slices.Equal(names, []string{"m", "id"}) {
		t.Fatalf("got %q %v", sql, names)
	}
}

func TestBind_번호_자리는_그대로_두고_이름은_가장_큰_번호_다음을_받는다(t *testing.T) {
	sql, names := Bind("select $2, :a, $1, :b, :a")
	if sql != "select $2, $3, $1, $4, $3" || !slices.Equal(names, []string{"1", "2", "a", "b"}) {
		t.Fatalf("got %q %v", sql, names)
	}
	sql, names = Bind("select $1 where x = $1")
	if sql != "select $1 where x = $1" || !slices.Equal(names, []string{"1"}) {
		t.Fatalf("got %q %v", sql, names)
	}
}

func TestBind_자리가_없으면_그대로_돌려준다(t *testing.T) {
	sql, names := Bind("select 1 where a = ':b' and $9999999999999999999 is null")
	if sql != "select 1 where a = ':b' and $9999999999999999999 is null" || names != nil {
		t.Fatalf("got %q %v", sql, names)
	}
}

func TestBind_붙여_쓴_비교와_한글_이름도_찾는다(t *testing.T) {
	sql, names := Bind("select * from t where id=:id and 이름 = :이름")
	if sql != "select * from t where id=$1 and 이름 = $2" || !slices.Equal(names, []string{"id", "이름"}) {
		t.Fatalf("got %q %v", sql, names)
	}
}

func FuzzBind_패닉없이_번호는_이름_수를_넘지_않는다(f *testing.F) {
	f.Add("select :a, $2, x::int, ':b'")
	f.Add("::a:b:::c$1:")
	f.Fuzz(func(t *testing.T, in string) {
		sql, names := Bind(in)
		if len(names) == 0 && sql != in {
			t.Fatalf("changed without names: %q -> %q", in, sql)
		}
		// 바꾼 결과에는 이름 자리가 남지 않아 다시 묶으면 그대로이고 이름은 "1".."N" 이다
		again, numbered := Bind(sql)
		if again != sql || len(numbered) != len(names) {
			t.Fatalf("rebind %q -> %q, names %v -> %v", sql, again, names, numbered)
		}
	})
}
