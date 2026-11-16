package history

import (
	"compress/gzip"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func openResults(t *testing.T) *Results {
	t.Helper()
	r, err := OpenResults(filepath.Join(t.TempDir(), "db", "results"))
	if err != nil {
		t.Fatal(err)
	}
	return r
}

func rowsOf(n, width int) [][]string {
	rows := make([][]string, n)
	for i := range rows {
		rows[i] = []string{strings.Repeat("x", width)}
	}
	return rows
}

func TestResults_사본은_gzip_JSON_0600_으로_쓰고_같은_값으로_읽는다(t *testing.T) {
	r := openResults(t)
	at := time.Date(2026, 11, 16, 10, 0, 0, 0, time.UTC)
	s := Snapshot{ID: NewID(at), Time: at, Conn: "app-stg", SQL: "select 1", Cols: []string{"a"}, Types: []string{"int4"},
		Rows: [][]string{{"1"}}, Elapsed: 120 * time.Millisecond, RowCount: 1}
	if err := r.Write(s); err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(r.dir, s.ID+".json.gz")
	for p, want := range map[string]os.FileMode{r.dir: 0o700, path: 0o600} {
		st, err := os.Stat(p)
		if err != nil {
			t.Fatal(err)
		}
		if st.Mode().Perm() != want {
			t.Errorf("%s: %o, want %o", p, st.Mode().Perm(), want)
		}
	}
	f, err := os.Open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer f.Close()
	zr, err := gzip.NewReader(f)
	if err != nil {
		t.Fatal("gzip 이어야 한다:", err)
	}
	var raw map[string]any
	if err := json.NewDecoder(zr).Decode(&raw); err != nil || raw["conn"] != "app-stg" {
		t.Fatalf("JSON: %v %v", raw, err)
	}
	got, err := r.Read(s.ID)
	if err != nil || got.SQL != "select 1" || got.Rows[0][0] != "1" || got.Types[0] != "int4" || got.Elapsed != s.Elapsed || got.Truncated {
		t.Fatalf("읽기: %+v %v", got, err)
	}
	if !r.Has(s.ID) {
		t.Fatal("쓴 사본은 Has")
	}
	r2, err := OpenResults(r.dir)
	if err != nil || !r2.Has(s.ID) {
		t.Fatal("다시 열면 폴더의 사본을 안다")
	}
	if err := r.Delete(s.ID); err != nil || r.Has(s.ID) {
		t.Fatal("Delete 는 파일과 목록에서 뺀다")
	}
	if _, err := r.Read(s.ID); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("지운 사본: %v", err)
	}
}

func TestResults_사본은_1000행과_1MiB_에서_잘리고_잘림을_표시한다(t *testing.T) {
	s := Snapshot{ID: NewID(time.Now()), Rows: rowsOf(1500, 10), RowCount: 1500}
	s.Cap()
	if len(s.Rows) != SnapshotRows || !s.Truncated || s.RowCount != 1500 {
		t.Fatalf("행 상한: %d %v", len(s.Rows), s.Truncated)
	}
	s = Snapshot{ID: NewID(time.Now()), Rows: rowsOf(800, 4000)} // 약 3.2MB
	s.Cap()
	b, _ := json.Marshal(s)
	if len(b) > SnapshotBytes || !s.Truncated || len(s.Rows) == 0 || len(s.Rows) >= 800 {
		t.Fatalf("크기 상한: %d바이트 %d행 %v", len(b), len(s.Rows), s.Truncated)
	}
	s = Snapshot{ID: NewID(time.Now()), Text: strings.Repeat("가", 1<<20)}
	s.Cap()
	if b, _ := json.Marshal(s); len(b) > SnapshotBytes || !s.Truncated {
		t.Fatalf("글 상한: %d", len(b))
	}
	s = Snapshot{ID: NewID(time.Now()), Rows: rowsOf(10, 10)}
	s.Cap()
	if len(s.Rows) != 10 || s.Truncated {
		t.Fatal("작은 결과는 그대로")
	}
}

func TestResults_줄이기는_30일_넘은_것을_지우고_합이_상한을_넘으면_오래된_것부터_지운다(t *testing.T) {
	r := openResults(t)
	now := time.Now()
	var ids []string
	for i := range 4 {
		s := Snapshot{ID: NewID(now.Add(time.Duration(i) * time.Second)), Rows: rowsOf(50, 200)}
		if err := r.Write(s); err != nil {
			t.Fatal(err)
		}
		ids = append(ids, s.ID)
	}
	// 0 은 31일 전, 1·2·3 은 3·2·1시간 전
	ages := []time.Duration{31 * 24 * time.Hour, 3 * time.Hour, 2 * time.Hour, time.Hour}
	for i, id := range ids {
		at := now.Add(-ages[i])
		if err := os.Chtimes(r.path(id), at, at); err != nil {
			t.Fatal(err)
		}
	}
	st, _ := os.Stat(r.path(ids[1]))
	r.maxTotal = 2*st.Size() + 10
	if err := r.Prune(now); err != nil {
		t.Fatal(err)
	}
	for i, want := range []bool{false, false, true, true} {
		if r.Has(ids[i]) != want {
			t.Errorf("%d: Has=%v, want %v", i, r.Has(ids[i]), want)
		}
		if _, err := os.Stat(r.path(ids[i])); (err == nil) != want {
			t.Errorf("%d: 파일 %v", i, err)
		}
	}
	// 줄이기는 간격 안에 다시 하지 않는다
	r.maxTotal = 0
	if err := r.MaybePrune(now.Add(time.Minute)); err != nil || !r.Has(ids[3]) {
		t.Fatal("간격 안의 MaybePrune 은 아무것도 하지 않는다")
	}
	if err := r.MaybePrune(now.Add(pruneEvery)); err != nil || r.Has(ids[3]) {
		t.Fatal("간격이 지나면 줄인다")
	}
}

func TestResults_이름_꼴이_아니면_폴더_밖을_읽거나_지우지_않는다(t *testing.T) {
	r := openResults(t)
	for _, id := range []string{"../history", "", "20261116-101530-a3f9c1/../../x"} {
		if _, err := r.Read(id); err == nil || errors.Is(err, os.ErrNotExist) {
			t.Errorf("%q: %v", id, err)
		}
		if err := r.Delete(id); err == nil {
			t.Errorf("Delete %q", id)
		}
	}
	if !idRe.MatchString(NewID(time.Now())) {
		t.Fatal("NewID 는 받는 꼴이어야 한다")
	}
}

func TestParseQuery_오늘_어제_낱말은_날짜로_거르고_나머지는_찾을_글이다(t *testing.T) {
	loc := time.FixedZone("KST", 9*3600)
	now := time.Date(2026, 11, 16, 0, 30, 0, 0, loc)
	f := ParseQuery("어제  orders 2", now)
	if f.Query != "orders 2" || !f.From.Equal(time.Date(2026, 11, 15, 0, 0, 0, 0, loc)) || !f.To.Equal(time.Date(2026, 11, 16, 0, 0, 0, 0, loc)) {
		t.Fatalf("어제: %+v", f)
	}
	if f := ParseQuery("Today", now); f.Query != "" || f.From.Day() != 16 {
		t.Fatalf("today: %+v", f)
	}
	if f := ParseQuery("select 오늘의", now); f.Query != "select 오늘의" || !f.From.IsZero() {
		t.Fatal("낱말 통째로 같을 때만 날짜다")
	}
}

func TestList_거르기는_접속_이름과_날짜_범위도_본다(t *testing.T) {
	s, _ := openTemp(t)
	day := time.Date(2026, 11, 15, 12, 0, 0, 0, time.Local)
	for _, e := range []Entry{
		{Time: day, Conn: "app-stg", SQL: "select 1"},
		{Time: day.AddDate(0, 0, 1), Conn: "log-prod", SQL: "select 2"},
	} {
		if err := s.Append(e); err != nil {
			t.Fatal(err)
		}
	}
	if got := joinSQL(s.List(Filter{Query: "PROD"})); got != "select 2" {
		t.Fatalf("접속 이름으로: %q", got)
	}
	f := ParseQuery("어제", day.AddDate(0, 0, 1))
	if got := joinSQL(s.List(f)); got != "select 1" {
		t.Fatalf("어제: %q", got)
	}
	if got := joinSQL(s.List(Filter{From: day.Add(time.Hour)})); got != "select 2" {
		t.Fatalf("From: %q", got)
	}
}
