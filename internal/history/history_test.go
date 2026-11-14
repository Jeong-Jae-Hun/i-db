package history

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"
)

func openTemp(t *testing.T) (*Store, string) {
	t.Helper()
	path := filepath.Join(t.TempDir(), "state", "db", "history.jsonl")
	s, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	return s, path
}

func joinSQL(es []Entry) string {
	var out []string
	for _, e := range es {
		out = append(out, e.SQL)
	}
	return strings.Join(out, "|")
}

func TestOpen_폴더는_0700_파일은_0600으로_만든다(t *testing.T) {
	_, path := openTemp(t)
	for p, want := range map[string]os.FileMode{filepath.Dir(path): 0o700, path: 0o600} {
		st, err := os.Stat(p)
		if err != nil {
			t.Fatal(err)
		}
		if got := st.Mode().Perm(); got != want {
			t.Errorf("%s: %o, want %o", p, got, want)
		}
	}
}

func TestList_최근_것부터_거르고_다시_열어도_남는다(t *testing.T) {
	s, path := openTemp(t)
	base := time.Date(2026, 11, 14, 9, 0, 0, 0, time.UTC)
	for i, e := range []Entry{
		{Conn: "a", SQL: "select 1"},
		{Conn: "b", SQL: "SELECT name from users", Err: "boom"},
		{Conn: "a", SQL: "update t set x = 1", Rows: 3},
		{Conn: "a", SQL: "select * from Users"},
	} {
		e.Time, e.Elapsed = base.Add(time.Duration(i)*time.Minute), time.Duration(i)*time.Millisecond
		if err := s.Append(e); err != nil {
			t.Fatal(err)
		}
	}
	if got := joinSQL(s.List(Filter{})); got != "select * from Users|update t set x = 1|SELECT name from users|select 1" {
		t.Fatalf("전체: %s", got)
	}
	if got := joinSQL(s.List(Filter{Query: "USERS"})); got != "select * from Users|SELECT name from users" {
		t.Fatalf("검색: %s", got)
	}
	if got := joinSQL(s.List(Filter{Conn: "a", Limit: 2})); got != "select * from Users|update t set x = 1" {
		t.Fatalf("접속·개수: %s", got)
	}
	re, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	got := re.List(Filter{Conn: "b"})
	if len(got) != 1 || got[0].Err != "boom" || !got[0].Time.Equal(base.Add(time.Minute)) || got[0].Elapsed != time.Millisecond {
		t.Fatalf("다시 연 기록: %+v", got)
	}
}

func TestOpen_깨진_줄은_건너뛴다(t *testing.T) {
	path := filepath.Join(t.TempDir(), "history.jsonl")
	data := `{"conn":"a","sql":"select 1"}` + "\n" + `{"conn":"a","sql":` + "\n" + "garbage\n\n" + `{"conn":"b","sql":"select 2"}` + "\n" + `{"conn":"c","sq`
	if err := os.WriteFile(path, []byte(data), 0o600); err != nil {
		t.Fatal(err)
	}
	s, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	if got := s.List(Filter{}); len(got) != 2 || got[0].SQL != "select 2" || got[1].SQL != "select 1" {
		t.Fatalf("got %+v", got)
	}
}

func TestAppend_개수_상한을_넘으면_최근_것만_남긴다(t *testing.T) {
	s, path := openTemp(t)
	for i := range maxEntries + 1 {
		if err := s.Append(Entry{SQL: fmt.Sprint("select ", i)}); err != nil {
			t.Fatal(err)
		}
	}
	got := s.List(Filter{})
	if len(got) != maxEntries*4/5 || got[0].SQL != fmt.Sprint("select ", maxEntries) {
		t.Fatalf("len %d, newest %q", len(got), got[0].SQL)
	}
	re, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	if n := len(re.List(Filter{})); n != len(got) {
		t.Fatalf("파일에 %d개, 메모리 %d개", n, len(got))
	}
}

func TestAppend_크기_상한과_긴_SQL을_자른다(t *testing.T) {
	s, path := openTemp(t)
	big := strings.Repeat("가", maxSQLBytes) // 3바이트 글자라 64KiB 경계가 글자 중간에 걸린다
	for range 2 * maxBytes / maxSQLBytes {
		if err := s.Append(Entry{SQL: big}); err != nil {
			t.Fatal(err)
		}
	}
	st, err := os.Stat(path)
	if err != nil {
		t.Fatal(err)
	}
	if st.Size() > maxBytes {
		t.Fatalf("파일 %d바이트", st.Size())
	}
	got := s.List(Filter{Limit: 1})[0].SQL
	if len(got) > maxSQLBytes || !strings.HasPrefix(big, got) || len(got)%3 != 0 {
		t.Fatalf("SQL %d바이트로 잘려야 한다", len(got))
	}
}

func TestAppend_동시에_불러도_기록을_잃지_않는다(t *testing.T) {
	s, path := openTemp(t)
	var wg sync.WaitGroup
	for g := range 8 {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for i := range 50 {
				if err := s.Append(Entry{Conn: fmt.Sprint(g), SQL: fmt.Sprint(i)}); err != nil {
					t.Error(err)
				}
				s.List(Filter{Limit: 5})
			}
		}()
	}
	wg.Wait()
	re, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	if n := len(re.List(Filter{})); n != 400 {
		t.Fatalf("%d개", n)
	}
}

func TestDefaultPath_XDG_STATE_HOME을_따르고_없으면_홈_아래(t *testing.T) {
	t.Setenv("XDG_STATE_HOME", "/x/state")
	if p, err := DefaultPath(); err != nil || p != "/x/state/db/history.jsonl" {
		t.Fatalf("%q %v", p, err)
	}
	t.Setenv("XDG_STATE_HOME", "relative")
	t.Setenv("HOME", "/home/u")
	if p, err := DefaultPath(); err != nil || p != "/home/u/.local/state/db/history.jsonl" {
		t.Fatalf("%q %v", p, err)
	}
}

func TestOpen_상한보다_큰_파일은_끝부분만_읽어_줄인다(t *testing.T) {
	path := filepath.Join(t.TempDir(), "history.jsonl")
	var b strings.Builder
	for i := 0; b.Len() <= maxBytes+maxBytes/2; i++ {
		fmt.Fprintf(&b, `{"conn":"a","sql":"select %d %s"}`+"\n", i, strings.Repeat("x", 1000))
	}
	if err := os.WriteFile(path, []byte(b.String()), 0o600); err != nil {
		t.Fatal(err)
	}
	s, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	st, err := os.Stat(path)
	if err != nil {
		t.Fatal(err)
	}
	lines := strings.Split(strings.TrimSpace(b.String()), "\n")
	if newest := s.List(Filter{Limit: 1}); st.Size() > maxBytes*4/5 || len(newest) != 1 || !strings.Contains(lines[len(lines)-1], `"`+newest[0].SQL+`"`) {
		t.Fatalf("size %d, newest %+v", st.Size(), newest)
	}
}

func TestAppend_끊긴_마지막_줄_뒤에도_새_기록을_살린다(t *testing.T) {
	path := filepath.Join(t.TempDir(), "history.jsonl")
	if err := os.WriteFile(path, []byte(`{"conn":"a","sql":"select 1"}`+"\n"+`{"conn":"a","sq`), 0o600); err != nil {
		t.Fatal(err)
	}
	s, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	if err := s.Append(Entry{Conn: "a", SQL: "select 2"}); err != nil {
		t.Fatal(err)
	}
	re, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	if got := joinSQL(re.List(Filter{})); got != "select 2|select 1" {
		t.Fatalf("다시 연 기록: %s", got)
	}
}

func TestAppend_두_Store가_붙이고_줄여도_기록을_잃지_않는다(t *testing.T) {
	const perStore = 300
	for round := range 3 {
		path := filepath.Join(t.TempDir(), "history.jsonl")
		// 상한 바로 아래로 채워 두 Store 가 붙이는 동안 줄이기가 일어나게 한다
		var b strings.Builder
		for i := range maxEntries - 10 {
			fmt.Fprintf(&b, `{"conn":"seed","sql":"seed %d"}`+"\n", i)
		}
		if err := os.WriteFile(path, []byte(b.String()), 0o600); err != nil {
			t.Fatal(err)
		}
		stores := make([]*Store, 2)
		for i := range stores {
			s, err := Open(path)
			if err != nil {
				t.Fatal(err)
			}
			stores[i] = s
		}
		var wg sync.WaitGroup
		for g, s := range stores {
			wg.Add(1)
			go func() {
				defer wg.Done()
				for i := range perStore {
					if err := s.Append(Entry{Conn: fmt.Sprint(g), SQL: fmt.Sprint(g, " ", i)}); err != nil {
						t.Error(err)
					}
				}
			}()
		}
		wg.Wait()
		re, err := Open(path)
		if err != nil {
			t.Fatal(err)
		}
		for g := range stores {
			if n := len(re.List(Filter{Conn: fmt.Sprint(g)})); n != perStore {
				t.Fatalf("%d회차 Store %d: 파일에 %d개, want %d", round, g, n, perStore)
			}
		}
	}
}
