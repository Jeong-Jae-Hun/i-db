package grid

import (
	"errors"
	"reflect"
	"strconv"
	"strings"
	"testing"

	"github.com/charmbracelet/x/ansi"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
)

// cipherResult 는 sub 열에 암호문 꼴(enc:…) 값이 든 테이블 결과다. enc:bad 는 풀지 못한다.
func cipherResult() *db.Result {
	return &db.Result{
		Cols:   []string{"id", "sub"},
		Types:  []string{"int4", "text"},
		Rows:   [][]string{{"1", "enc:alice"}, {"2", "plain"}, {"3", "enc:bad"}, {"4", "NULL"}},
		Origin: &db.Origin{Table: peopleTable, PK: []string{"id"}, Cols: []string{"id", "sub"}},
	}
}

func looksEnc(v string) bool { return strings.HasPrefix(v, "enc:") }

func decryptEnc(v string) (string, error) {
	if v == "enc:bad" {
		return "", errors.New("bad cipher")
	}
	return "평문-" + strings.TrimPrefix(v, "enc:"), nil
}

func TestInList_숫자_열은_그대로_같은_값은_한_번_NULL은_뺀다(t *testing.T) {
	r := &db.Result{Cols: []string{"user_id"}, Types: []string{"int8"}, Rows: [][]string{{"1315"}, {"1318"}, {"NULL"}, {"1315"}}}
	_, a := press(newGrid(r, [2]int{40, 10}), "C")
	if got := copied(t, a); got != "(1315, 1318)" || a.Status != "IN 목록 2개 복사" {
		t.Fatalf("IN 목록: %q %q", got, a.Status)
	}
}

func TestInList_글자_열은_따옴표로_감싸고_보이는_순서를_따른다(t *testing.T) {
	r := &db.Result{Cols: []string{"sub"}, Types: []string{"text"}, Rows: [][]string{{"b2"}, {"it's"}, {"a1"}, {`c\d`}}}
	m := newGrid(r, [2]int{40, 10})
	_, a := press(m, "C")
	if got := copied(t, a); got != `('b2', 'it''s', 'a1', E'c\\d')` {
		t.Fatalf("IN 목록: %q", got)
	}
	_, a = press(m, "s", "C")
	if got := copied(t, a); got != `('a1', 'b2', E'c\\d', 'it''s')` {
		t.Fatalf("정렬 뒤: %q", got)
	}
	_, a = press(m, "/", "2", "enter", "C")
	if got := copied(t, a); got != "('b2')" {
		t.Fatalf("거른 뒤: %q", got)
	}
}

func TestInList_숫자_열의_NaN_Infinity는_리터럴로_감싼다(t *testing.T) {
	r := &db.Result{Cols: []string{"f"}, Types: []string{"float8"}, Rows: [][]string{{"1.5"}, {"NaN"}, {"-Infinity"}}}
	_, a := press(newGrid(r, [2]int{40, 10}), "C")
	if got := copied(t, a); got != "(1.5, 'NaN', '-Infinity')" {
		t.Fatalf("IN 목록: %q", got)
	}
}

func TestInList_타입_없는_결과는_표준_숫자_꼴만_그대로_두고_나머지는_감싼다(t *testing.T) {
	r := &db.Result{Cols: []string{"value"}, Rows: [][]string{{"12"}, {"007"}, {"1e3"}, {"1.50"}, {"-3"}, {"2.5"}, {"+4"}, {"-0"}}}
	_, a := press(newGrid(r, [2]int{40, 10}), "C")
	if got := copied(t, a); got != "(12, '007', '1e3', '1.50', -3, 2.5, '+4', '-0')" {
		t.Fatalf("IN 목록: %q", got)
	}
}

func TestInList_선택_중에는_선택한_칸을_행_순서로_넣는다(t *testing.T) {
	m := newGrid(peopleResult(), [2]int{60, 12})
	_, a := press(m, "V", "j", "j", "l", "C")
	if got := copied(t, a); got != "(1, 'Alice', 2, 'bob', 3, 'carol')" || a.Status != "IN 목록 6개 복사" {
		t.Fatalf("선택 IN 목록: %q %q", got, a.Status)
	}
}

func TestInList_1000개가_넘으면_앞_1000개만_넣는다(t *testing.T) {
	r := &db.Result{Cols: []string{"id"}, Types: []string{"int4"}}
	for i := range 1500 {
		r.Rows = append(r.Rows, []string{strconv.Itoa(i)})
	}
	_, a := press(newGrid(r, [2]int{40, 10}), "C")
	got := copied(t, a)
	if n := strings.Count(got, ",") + 1; n != 1000 || !strings.HasSuffix(got, ", 999)") || a.Status != "앞 1,000개만 복사" {
		t.Fatalf("상한: %d개 %q %q", n, got[len(got)-10:], a.Status)
	}
	// 같은 값이 겹쳐 1,000개 안이면 상한을 알리지 않는다
	r.Rows = r.Rows[:1000]
	r.Rows = append(r.Rows, []string{"0"}, []string{"999"})
	if _, a = press(newGrid(r, [2]int{40, 10}), "C"); a.Status != "IN 목록 1000개 복사" {
		t.Fatalf("중복만 넘친 경우: %q", a.Status)
	}
}

func TestInList_값이_없으면_알리고_추가_행과_보류_중인_바꿈은_넣지_않는다(t *testing.T) {
	r := &db.Result{Cols: []string{"x"}, Types: []string{"text"}, Rows: [][]string{{"NULL"}}}
	if _, a := press(newGrid(r, [2]int{40, 10}), "C"); a.Copy != nil || a.Status != "복사할 값이 없음" {
		t.Fatalf("빈 목록: %+v", a)
	}
	// 첫 행 name 을 AliceZ 로 고치고 행을 하나 더해도 DB 값만 들어간다
	m := editedGrid(t, [2]int{80, 10})
	m, _ = press(m, "a", "l", "enter", "new", "enter")
	_, a := press(m, "C")
	if got := copied(t, a); got != "('Alice', 'bob', 'carol')" {
		t.Fatalf("IN 목록: %q", got)
	}
}

func TestFollow_F는_커서_열과_행의_DB_값을_R은_반대_방향을_청한다(t *testing.T) {
	m := editedGrid(t, [2]int{80, 10})
	m, a := press(m, "l", "l", "F")
	want := Follow{Col: "city", Row: map[string]string{"id": "1", "name": "Alice", "city": "seoul"}}
	if a.Follow == nil || !reflect.DeepEqual(*a.Follow, want) {
		t.Fatalf("F: %+v", a.Follow)
	}
	_, a = press(m, "R")
	want.Reverse = true
	if a.Follow == nil || !reflect.DeepEqual(*a.Follow, want) {
		t.Fatalf("R: %+v", a.Follow)
	}
}

func TestFollow_따라갈_수_없으면_이유만_알린다(t *testing.T) {
	cases := map[string]struct {
		r    *db.Result
		keys []string
		want string
	}{
		"원본 없음": {peopleResult(), []string{"F"}, "원본 테이블을 모르는 결과"},
		"식 열":   {editableResult(), []string{"$", "F"}, "외래 키 열이 아님"},
		"NULL":  {editableResult(), []string{"G", "l", "l", "F"}, "참조 값이 NULL"},
		"추가 행":  {editableResult(), []string{"a", "R"}, "추가한 행은 아직 DB 에 없음"},
		"빈 결과":  {&db.Result{Cols: []string{"id"}, Types: []string{"int4"}}, []string{"R"}, "따라갈 행 없음"},
		"텍스트":   {&db.Result{Text: "INSERT 0 1"}, []string{"F"}, "표 결과가 아님"},
	}
	for name, c := range cases {
		if _, a := press(newGrid(c.r, [2]int{80, 10}), c.keys...); a.Follow != nil || a.Status != c.want {
			t.Errorf("%s: %+v", name, a)
		}
	}
	// 식 열에서도 R 은 행 값으로 청한다
	if _, a := press(newGrid(editableResult(), [2]int{80, 10}), "$", "R"); a.Follow == nil || a.Follow.Col != "" || a.Follow.Row["id"] != "1" {
		t.Fatalf("식 열 R: %+v", a)
	}
}

func TestFilter_더하기_빼기는_커서_칸의_DB_값으로_거르기를_청한다(t *testing.T) {
	m := editedGrid(t, [2]int{80, 10})
	if _, a := press(m, "l", "+"); a.Filter == nil || *a.Filter != (CellFilter{Col: "name", Value: "Alice"}) {
		t.Fatalf("+: %+v", a.Filter)
	}
	if _, a := press(m, "G", "l", "l", "-"); a.Filter == nil || *a.Filter != (CellFilter{Col: "city", Value: "NULL", Null: true, Negate: true}) {
		t.Fatalf("-: %+v", a.Filter)
	}
	if _, a := press(m, "a", "+"); a.Filter != nil || a.Status != "추가한 행은 아직 DB 에 없음" {
		t.Fatalf("추가 행: %+v", a)
	}
	if _, a := press(newGrid(&db.Result{Cols: []string{"x"}}, [2]int{40, 10}), "-"); a.Filter != nil || a.Status != "거를 행 없음" {
		t.Fatalf("빈 결과: %+v", a)
	}
}

func TestDecrypt_D는_요청만_하고_SetDecrypted는_푼_값을_보이며_풀지_못한_칸을_센다(t *testing.T) {
	m := newGrid(cipherResult(), [2]int{60, 10})
	if _, a := press(m, "D"); !a.Decrypt || m.Decrypted() {
		t.Fatalf("D 는 요청만 한다: %+v %v", a, m.Decrypted())
	}
	if failed := m.SetDecrypted(looksEnc, decryptEnc); failed != 1 || !m.Decrypted() {
		t.Fatalf("풀지 못한 칸 %d, 켜짐 %v", failed, m.Decrypted())
	}
	plain := ansi.Strip(m.View(true))
	if !strings.Contains(plain, "평문-alice") || strings.Contains(plain, "enc:alice") || !strings.Contains(plain, "enc:bad") || !strings.Contains(plain, "복호화 보기 · D 원래 값") {
		t.Fatalf("화면:\n%s", plain)
	}
	checkBounds(t, m)
	m.ClearDecrypted()
	if m.Decrypted() || !strings.Contains(ansi.Strip(m.View(true)), "enc:alice") {
		t.Fatal("ClearDecrypted 뒤에는 원래 값이다")
	}
	m.SetDecrypted(looksEnc, decryptEnc)
	m.SetResult(cipherResult())
	if m.Decrypted() || !strings.Contains(ansi.Strip(m.View(true)), "enc:alice") {
		t.Fatal("새 결과는 복호화 보기를 끈다")
	}
}

func TestDecrypt_복사와_값_보기는_푼_값을_내보내기_IN_SQL_비교는_원래_값을_쓴다(t *testing.T) {
	m := newGrid(cipherResult(), [2]int{60, 10})
	m.SetDecrypted(looksEnc, decryptEnc)
	m, _ = press(m, "l")
	if _, a := press(m, "y"); copied(t, a) != "평문-alice" {
		t.Fatalf("y: %q", *a.Copy)
	}
	if _, a := press(m, "Y"); copied(t, a) != "1\t평문-alice" {
		t.Fatalf("Y: %q", *a.Copy)
	}
	if _, a := press(m, "v", "y"); copied(t, a) != "평문-alice" {
		t.Fatalf("레코드 y: %q", *a.Copy)
	}
	if _, a := press(m, "V", "j", "y"); copied(t, a) != "평문-alice\nplain" {
		t.Fatalf("선택 y: %q", *a.Copy)
	}
	g, _ := press(m, "o")
	if g.mode != modeCell || g.cell.raw != "평문-alice" || !strings.Contains(ansi.Strip(g.View(true)), "평문-alice") {
		t.Fatalf("값 보기: %q", g.cell.raw)
	}
	if _, a := press(m, "e"); a.Export == nil || !strings.Contains(a.Export.Content, "enc:alice") || strings.Contains(a.Export.Content, "평문") {
		t.Fatalf("CSV 내보내기는 원래 값: %+v", a.Export)
	}
	if _, a := press(m, "E"); a.Export == nil || strings.Contains(a.Export.Content, "평문") {
		t.Fatalf("JSON 내보내기는 원래 값: %+v", a.Export)
	}
	if _, a := press(m, "C"); copied(t, a) != "('enc:alice', 'plain', 'enc:bad')" {
		t.Fatalf("IN 목록은 원래 값: %q", *a.Copy)
	}
	if _, a := press(m, "+"); a.Filter == nil || a.Filter.Value != "enc:alice" {
		t.Fatalf("거르기는 원래 값: %+v", a.Filter)
	}
	if _, a := press(m, "F"); a.Follow == nil || a.Follow.Row["sub"] != "enc:alice" {
		t.Fatalf("따라가기는 원래 값: %+v", a.Follow)
	}
	if _, a := press(m, "i"); a.CopyAs == nil || !reflect.DeepEqual(a.CopyAs.Rows, []int{0}) {
		t.Fatalf("SQL 복사는 행 인덱스로 원래 값: %+v", a.CopyAs)
	}
	if err := m.ShowDiff(cipherResult()); err != nil || len(m.diff.rep.Rows) != 0 {
		t.Fatalf("비교는 원래 값끼리라 차이가 없다: %v %+v", err, m.diff.rep)
	}
}

func TestDecrypt_보기_중에는_칸을_고치지_않고_enter는_푼_값을_연다(t *testing.T) {
	m := newGrid(cipherResult(), [2]int{60, 10})
	m.SetDecrypted(looksEnc, decryptEnc)
	if g, a := press(m, "l", "f2"); g.editing || a.Status != "편집할 수 없음: 복호화 보기 중 (D 로 원래 값)" {
		t.Fatalf("f2: %v %q", g.editing, a.Status)
	}
	if g, _ := press(m, "l", "enter"); g.editing || g.mode != modeCell || g.cell.raw != "평문-alice" {
		t.Fatalf("enter: %v %v %q", g.editing, g.mode, g.cell.raw)
	}
}

func TestDecrypt_고친_칸은_푼_값_대신_고친_값을_보이고_제출은_고친_값이다(t *testing.T) {
	m := newGrid(cipherResult(), [2]int{60, 10})
	m, _ = press(m, "l", "enter", "backspace", "x", "enter")
	m.SetDecrypted(looksEnc, decryptEnc)
	out := ansi.Strip(m.View(true))
	if !strings.Contains(out, "enc:alicx") || strings.Contains(out, "평문-alice") {
		t.Fatalf("고친 칸은 고친 값:\n%s", out)
	}
	cs := submit(t, m)
	if v := cs.Changes[0].Values["sub"]; v == nil || *v != "enc:alicx" || *cs.Changes[0].Old["sub"] != "enc:alice" {
		t.Fatalf("제출: %+v", cs.Changes[0])
	}
	// 제출이 끝나 원래 값이 바뀐 칸은 예전에 푼 값을 보이지 않는다
	m.ClearSubmitted(m.Revision())
	if out := ansi.Strip(m.View(true)); !strings.Contains(out, "enc:alicx") || strings.Contains(out, "평문-alice") {
		t.Fatalf("제출 뒤:\n%s", out)
	}
}
