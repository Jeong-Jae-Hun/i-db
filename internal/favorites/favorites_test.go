package favorites

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func names(favs []Fav) string {
	var out []string
	for _, f := range favs {
		out = append(out, f.Name)
	}
	return strings.Join(out, "|")
}

func mustLoad(t *testing.T, dir string) *Store {
	t.Helper()
	s, err := Load(dir)
	if err != nil {
		t.Fatal(err)
	}
	return s
}

func TestLoad_파일이_없으면_빈_목록이고_폴더를_만들지_않는다(t *testing.T) {
	dir := t.TempDir()
	s := mustLoad(t, dir)
	if got := s.List(); len(got) != 0 {
		t.Fatalf("목록: %+v", got)
	}
	if _, err := os.Stat(filepath.Join(dir, ".dbtui")); !os.IsNotExist(err) {
		t.Fatalf(".dbtui 를 만들었다: %v", err)
	}
}

func TestPut_이름순으로_두_칸_들여써_저장하고_권한은_0755_0644다(t *testing.T) {
	dir := t.TempDir()
	s := mustLoad(t, dir)
	kst := time.FixedZone("KST", 9*3600)
	saved := time.Date(2026, 11, 14, 20, 30, 0, 123, kst)
	for _, f := range []Fav{
		{Name: "  오늘 가입자 ", Conn: "u1", SQL: "select count(*) from users where created_at > now() - '1 day'::interval", Saved: saved},
		{Name: "a<b", Conn: "u2", SQL: "select 1 where 1 < 2 && true", Saved: saved},
	} {
		if err := s.Put(f); err != nil {
			t.Fatal(err)
		}
	}
	data, err := os.ReadFile(Path(dir))
	if err != nil {
		t.Fatal(err)
	}
	want := `[
  {
    "name": "a<b",
    "conn": "u2",
    "sql": "select 1 where 1 < 2 && true",
    "saved": "2026-11-14T20:30:00+09:00"
  },
  {
    "name": "오늘 가입자",
    "conn": "u1",
    "sql": "select count(*) from users where created_at > now() - '1 day'::interval",
    "saved": "2026-11-14T20:30:00+09:00"
  }
]
`
	if string(data) != want {
		t.Fatalf("파일:\n%s", data)
	}
	for p, perm := range map[string]os.FileMode{filepath.Dir(Path(dir)): 0o755, Path(dir): 0o644} {
		st, err := os.Stat(p)
		if err != nil {
			t.Fatal(err)
		}
		if st.Mode().Perm() != perm {
			t.Errorf("%s: %o, want %o", p, st.Mode().Perm(), perm)
		}
	}
	if got := names(mustLoad(t, dir).List()); got != "a<b|오늘 가입자" {
		t.Fatalf("다시 읽은 목록: %s", got)
	}
}

func TestPut_같은_이름은_덮어쓰고_Saved가_비면_지금_시각이다(t *testing.T) {
	s := mustLoad(t, t.TempDir())
	if err := s.Put(Fav{Name: "q", Conn: "old", SQL: "select 1"}); err != nil {
		t.Fatal(err)
	}
	before := time.Now().Add(-time.Second)
	if err := s.Put(Fav{Name: " q", Conn: "new", SQL: "select 2"}); err != nil {
		t.Fatal(err)
	}
	if got := s.List(); len(got) != 1 {
		t.Fatalf("목록: %+v", got)
	}
	f, ok := s.Get("q ")
	if !ok || f.Conn != "new" || f.SQL != "select 2" {
		t.Fatalf("Get: %+v %v", f, ok)
	}
	if f.Saved.Before(before) || f.Saved.After(time.Now()) || f.Saved.Nanosecond() != 0 {
		t.Fatalf("Saved: %v", f.Saved)
	}
}

func TestPut_잘못된_이름과_빈_SQL은_거부하고_파일을_만들지_않는다(t *testing.T) {
	dir := t.TempDir()
	s := mustLoad(t, dir)
	for _, f := range []Fav{
		{Name: "", SQL: "select 1"},
		{Name: " \t\n", SQL: "select 1"},
		{Name: strings.Repeat("가", 101), SQL: "select 1"},
		{Name: "줄\n바꿈", SQL: "select 1"},
		{Name: "빈 SQL", SQL: "  \n"},
	} {
		if err := s.Put(f); err == nil {
			t.Errorf("%q 를 받아들였다", f.Name)
		}
	}
	if _, err := os.Stat(Path(dir)); !os.IsNotExist(err) {
		t.Fatalf("파일을 만들었다: %v", err)
	}
	if err := s.Put(Fav{Name: strings.Repeat("가", 100), SQL: "select 1"}); err != nil {
		t.Fatalf("100자 이름: %v", err)
	}
}

func TestDelete_지우고_없는_이름은_오류_없이_넘어간다(t *testing.T) {
	dir := t.TempDir()
	s := mustLoad(t, dir)
	for _, n := range []string{"a", "b"} {
		if err := s.Put(Fav{Name: n, SQL: "select 1"}); err != nil {
			t.Fatal(err)
		}
	}
	if err := s.Delete(" a "); err != nil {
		t.Fatal(err)
	}
	if err := s.Delete("없음"); err != nil {
		t.Fatal(err)
	}
	if got := names(mustLoad(t, dir).List()); got != "b" {
		t.Fatalf("목록: %s", got)
	}
	if err := s.Delete("b"); err != nil {
		t.Fatal(err)
	}
	if data, _ := os.ReadFile(Path(dir)); string(data) != "[]\n" {
		t.Fatalf("다 지운 파일: %q", data)
	}
}

func TestPut_다른_프로세스가_그사이_저장한_이름을_지킨다(t *testing.T) {
	dir := t.TempDir()
	mine := mustLoad(t, dir)
	other := mustLoad(t, dir)
	if err := other.Put(Fav{Name: "남의 것", SQL: "select 1"}); err != nil {
		t.Fatal(err)
	}
	if err := mine.Put(Fav{Name: "내 것", SQL: "select 2"}); err != nil {
		t.Fatal(err)
	}
	if got := names(mustLoad(t, dir).List()); got != "남의 것|내 것" {
		t.Fatalf("목록: %s", got)
	}
	if got := names(mine.List()); got != "남의 것|내 것" {
		t.Fatalf("쓴 뒤 메모리 목록: %s", got)
	}
}

func TestLoad_같은_이름이_둘이면_뒤의_것을_남기고_빈_파일은_빈_목록이다(t *testing.T) {
	dir := t.TempDir()
	if err := os.MkdirAll(filepath.Dir(Path(dir)), 0o755); err != nil {
		t.Fatal(err)
	}
	data := `[{"name":"b","sql":"1"},{"name":"a","sql":"old"},{"name":"a","sql":"new"}]`
	if err := os.WriteFile(Path(dir), []byte(data), 0o644); err != nil {
		t.Fatal(err)
	}
	s := mustLoad(t, dir)
	if f, _ := s.Get("a"); names(s.List()) != "a|b" || f.SQL != "new" {
		t.Fatalf("목록: %+v", s.List())
	}
	if err := os.WriteFile(Path(dir), []byte(" \n"), 0o644); err != nil {
		t.Fatal(err)
	}
	if got := mustLoad(t, dir).List(); len(got) != 0 {
		t.Fatalf("빈 파일: %+v", got)
	}
}

func TestPut_깨진_파일이면_오류이고_파일을_덮어쓰지_않는다(t *testing.T) {
	dir := t.TempDir()
	if err := os.MkdirAll(filepath.Dir(Path(dir)), 0o755); err != nil {
		t.Fatal(err)
	}
	broken := `[{"name":"a","sql":`
	if err := os.WriteFile(Path(dir), []byte(broken), 0o644); err != nil {
		t.Fatal(err)
	}
	if _, err := Load(dir); err == nil {
		t.Fatal("Load 가 깨진 파일을 받아들였다")
	}
	s := &Store{path: Path(dir)}
	if err := s.Put(Fav{Name: "b", SQL: "select 1"}); err == nil {
		t.Fatal("Put 이 깨진 파일 위에 썼다")
	}
	if err := s.Delete("a"); err == nil {
		t.Fatal("Delete 가 깨진 파일 위에 썼다")
	}
	if data, _ := os.ReadFile(Path(dir)); string(data) != broken {
		t.Fatalf("원본이 바뀌었다: %q", data)
	}
}

func TestPut_임시_파일을_남기지_않는다(t *testing.T) {
	dir := t.TempDir()
	s := mustLoad(t, dir)
	for i := range 3 {
		if err := s.Put(Fav{Name: string(rune('a' + i)), SQL: "select 1"}); err != nil {
			t.Fatal(err)
		}
	}
	if err := s.Delete("a"); err != nil {
		t.Fatal(err)
	}
	entries, err := os.ReadDir(filepath.Dir(Path(dir)))
	if err != nil {
		t.Fatal(err)
	}
	if len(entries) != 1 || entries[0].Name() != "favorites.json" {
		t.Fatalf("폴더: %v", entries)
	}
}

func TestPut_쓰기에_실패하면_원본이_남고_임시_파일도_없다(t *testing.T) {
	if os.Getuid() == 0 {
		t.Skip("root 는 권한 없는 폴더에도 쓴다")
	}
	dir := t.TempDir()
	s := mustLoad(t, dir)
	if err := s.Put(Fav{Name: "a", SQL: "select 1"}); err != nil {
		t.Fatal(err)
	}
	sub := filepath.Dir(Path(dir))
	before, _ := os.ReadFile(Path(dir))
	if err := os.Chmod(sub, 0o555); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = os.Chmod(sub, 0o755) })
	if err := s.Put(Fav{Name: "b", SQL: "select 2"}); err == nil {
		t.Fatal("읽기 전용 폴더에 썼다")
	}
	if after, _ := os.ReadFile(Path(dir)); string(after) != string(before) {
		t.Fatalf("원본이 바뀌었다: %q", after)
	}
	if got := names(s.List()); got != "a" {
		t.Fatalf("실패한 쓰기가 메모리에 남았다: %s", got)
	}
	entries, _ := os.ReadDir(sub)
	if len(entries) != 1 {
		t.Fatalf("폴더: %v", entries)
	}
}

func TestPut_Delete_는_있던_파일의_권한을_지킨다(t *testing.T) {
	dir := t.TempDir()
	s := mustLoad(t, dir)
	if err := s.Put(Fav{Name: "a", SQL: "select 1"}); err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(Path(dir), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := s.Put(Fav{Name: "b", SQL: "select 2"}); err != nil {
		t.Fatal(err)
	}
	if err := s.Delete("a"); err != nil {
		t.Fatal(err)
	}
	st, err := os.Stat(Path(dir))
	if err != nil {
		t.Fatal(err)
	}
	if st.Mode().Perm() != 0o600 {
		t.Fatalf("권한 %o, want 600", st.Mode().Perm())
	}
}
