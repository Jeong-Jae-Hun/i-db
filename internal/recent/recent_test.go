package recent

import (
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"
	"time"
)

var t0 = time.Date(2026, 11, 16, 9, 0, 0, 0, time.UTC)

func newStore(t *testing.T) (*Store, string) {
	t.Helper()
	path := filepath.Join(t.TempDir(), "db", "recent.json")
	s, err := Load(path, t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	return s, path
}

func TestFrecency_자주_쓴_것이_먼저고_오래되면_요즘_쓴_것에_밀린다(t *testing.T) {
	s, _ := newStore(t)
	for range 3 {
		s.UseAction("a", t0)
	}
	s.UseAction("b", t0)
	s.UseAction("c", t0.Add(time.Minute))
	if got := s.TopActions(t0.Add(time.Minute), 3); !slices.Equal(got, []string{"a", "c", "b"}) {
		t.Fatalf("횟수 먼저, 같으면 최근 것: %v", got)
	}
	// 4주 뒤에는 3번 쓴 a(가중치 1/16)보다 어제 한 번 쓴 d 가 앞이다
	later := t0.Add(28 * 24 * time.Hour)
	s.UseAction("d", later.Add(-24*time.Hour))
	if got := s.TopActions(later, 1); !slices.Equal(got, []string{"d"}) {
		t.Fatalf("오래된 사용은 줄어든다: %v", got)
	}
}

func TestFrecency_동작은_50개까지_점수_낮은_것부터_버린다(t *testing.T) {
	s, _ := newStore(t)
	s.UseAction("keep", t0)
	s.UseAction("keep", t0)
	for i := range 60 {
		s.UseAction(fmt.Sprintf("x%02d", i), t0.Add(time.Duration(i)*time.Second))
	}
	if n := len(s.f.Actions); n != maxActions {
		t.Fatalf("50개까지: %d", n)
	}
	if _, ok := s.f.Actions["keep"]; !ok {
		t.Fatal("두 번 쓴 동작은 남는다")
	}
	if _, ok := s.f.Actions["x00"]; ok {
		t.Fatal("가장 점수 낮은 것부터 버린다")
	}
}

func TestTables_500개를_넘으면_가장_오래_안_연_테이블부터_버린다(t *testing.T) {
	s, _ := newStore(t)
	for i := range maxTables + 10 {
		s.TouchTable("u", "public", fmt.Sprintf("t%03d", i), t0.Add(time.Duration(i)*time.Second))
	}
	if n := len(s.p.Tables); n != maxTables {
		t.Fatalf("500개까지: %d", n)
	}
	if _, ok := s.Table("u", "public", "t009"); ok {
		t.Fatal("가장 오래된 것부터 버린다")
	}
	if got := s.RecentTables(2); got[0].Name != "t509" || got[1].Name != "t508" {
		t.Fatalf("최근 연 순: %+v", got)
	}
	s.TouchTable("u", "public", "t010", t0.Add(time.Hour))
	if got := s.RecentTables(1); got[0].Name != "t010" {
		t.Fatalf("다시 열면 맨 앞: %+v", got)
	}
}

func TestTables_prod_는_WHERE_와_그_기록을_남기지_않고_정렬과_폭만_남긴다(t *testing.T) {
	s, path := newStore(t)
	s.AddWhere("p", "public", "users", "email = 'a@b.com'", true, t0)
	s.SetView("p", "public", "users", "id DESC", "email = 'a@b.com'", true, t0)
	s.SetWidth("p", "public", "users", "email", 30, t0)
	must(t, s.Save())
	data, err := os.ReadFile(path)
	must(t, err)
	if strings.Contains(string(data), "a@b.com") {
		t.Fatalf("prod WHERE 는 파일에 없다: %s", data)
	}
	tb, _ := s.Table("p", "public", "users")
	if tb.Order != "id DESC" || tb.Where != "" || tb.Widths["email"] != 30 || len(s.Wheres("p", "public", "users", true)) != 0 {
		t.Fatalf("정렬·폭만: %+v", tb)
	}

	s.SetView("s", "public", "users", "", "status = 'A'", false, t0)
	s.AddWhere("s", "public", "users", "status = 'A'", false, t0)
	if tb, _ := s.Table("s", "public", "users"); tb.Where != "status = 'A'" || !slices.Equal(tb.Wheres, []string{"status = 'A'"}) {
		t.Fatalf("prod 가 아니면 WHERE 도 남긴다: %+v", tb)
	}
	// 나중에 prod 로 보이게 된 접속이면 남은 WHERE 기록도 지운다
	s.SetView("s", "public", "users", "", "status = 'A'", true, t0)
	if tb, _ := s.Table("s", "public", "users"); tb.Where != "" || tb.Wheres != nil {
		t.Fatalf("prod 면 지운다: %+v", tb)
	}
}

func TestInputs_거르기는_100개_WHERE_는_테이블마다_20개까지_최근_것부터_겹치지_않게(t *testing.T) {
	s, _ := newStore(t)
	for i := range 120 {
		s.AddFilter(fmt.Sprintf("f%d", i))
	}
	s.AddFilter("f50")
	got := s.Filters()
	if len(got) != maxFilters || got[0] != "f50" || got[1] != "f119" || slices.Index(got[1:], "f50") >= 0 {
		t.Fatalf("거르기 기록: n=%d %v", len(got), got[:3])
	}
	for i := range 25 {
		s.AddWhere("s", "public", "t", fmt.Sprintf("id = %d", i), false, t0)
	}
	if w := s.Wheres("s", "public", "t", false); len(w) != maxWheres || w[0] != "id = 24" {
		t.Fatalf("WHERE 기록: %v", w)
	}
}

func TestSave_0600_으로_쓰고_다른_프로젝트를_지키며_다시_읽힌다(t *testing.T) {
	path := filepath.Join(t.TempDir(), "db", "recent.json")
	a, err := Load(path, "/proj/a")
	must(t, err)
	a.UseConn("ua", t0)
	must(t, a.Save())
	b, err := Load(path, "/proj/b")
	must(t, err)
	b.UseConn("ub", t0)
	b.SetTree([]string{"c:ub"}, []string{"g:etc"})
	must(t, b.Save())
	st, err := os.Stat(path)
	must(t, err)
	if st.Mode().Perm() != 0o600 {
		t.Fatalf("권한: %v", st.Mode().Perm())
	}
	a2, err := Load(path, "/proj/a")
	must(t, err)
	if got := a2.TopConns(t0, 5); !slices.Equal(got, []string{"ua"}) {
		t.Fatalf("다른 프로젝트 기록을 지킨다: %v", got)
	}
	b2, _ := Load(path, "/proj/b")
	if open, closed := b2.Tree(); !slices.Equal(open, []string{"c:ub"}) || !slices.Equal(closed, []string{"g:etc"}) {
		t.Fatalf("트리: %v %v", open, closed)
	}
}

func TestLoad_깨진_파일은_빈_기억과_오류이고_저장하면_새로_쓴다(t *testing.T) {
	path := filepath.Join(t.TempDir(), "recent.json")
	must(t, os.WriteFile(path, []byte("{not json"), 0o600))
	s, err := Load(path, t.TempDir())
	if err == nil || s == nil || len(s.Filters()) != 0 || len(s.TopActions(t0, 5)) != 0 {
		t.Fatalf("빈 기억과 오류: s=%v err=%v", s, err)
	}
	s.AddFilter("x")
	must(t, s.Save())
	s2, err := Load(path, t.TempDir())
	if err != nil || !slices.Equal(s2.Filters(), []string{"x"}) {
		t.Fatalf("새로 쓴 파일: %v %v", s2.Filters(), err)
	}
}

func must(t *testing.T, err error) {
	t.Helper()
	if err != nil {
		t.Fatal(err)
	}
}
