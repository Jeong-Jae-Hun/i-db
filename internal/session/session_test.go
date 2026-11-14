package session

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
	"time"
)

func tempPath(t *testing.T) string {
	t.Helper()
	return filepath.Join(t.TempDir(), "state", "db", "session.json")
}

func TestPath_XDG_STATE_HOME을_따르고_상대_경로면_홈_아래(t *testing.T) {
	t.Setenv("XDG_STATE_HOME", "/x/state")
	if p, err := Path(); err != nil || p != "/x/state/db/session.json" {
		t.Fatalf("XDG: %q %v", p, err)
	}
	t.Setenv("XDG_STATE_HOME", "relative")
	t.Setenv("HOME", "/home/u")
	if p, err := Path(); err != nil || p != "/home/u/.local/state/db/session.json" {
		t.Fatalf("홈: %q %v", p, err)
	}
}

func TestLoad_파일이나_프로젝트가_없으면_false다(t *testing.T) {
	path := tempPath(t)
	if _, ok, err := Load(path, "/p"); ok || err != nil {
		t.Fatalf("없는 파일: %v %v", ok, err)
	}
	if err := Save(path, "/a", State{Tabs: []Tab{{Title: "Query 1"}}}); err != nil {
		t.Fatal(err)
	}
	if _, ok, err := Load(path, "/b"); ok || err != nil {
		t.Fatalf("없는 프로젝트: %v %v", ok, err)
	}
}

func TestLoad_깨진_파일은_bad로_옮기고_오류를_한_번_알린_뒤_새로_시작한다(t *testing.T) {
	path := tempPath(t)
	must(t, os.MkdirAll(filepath.Dir(path), 0o700))
	must(t, os.WriteFile(BadPath(path), []byte("older"), 0o600))
	must(t, os.WriteFile(path, []byte("{broken"), 0o600))
	if _, ok, err := Load(path, "/a"); ok || !errors.Is(err, ErrCorrupt) {
		t.Fatalf("깨진 파일은 ErrCorrupt: %v %v", ok, err)
	}
	if data, _ := os.ReadFile(BadPath(path)); string(data) != "{broken" {
		t.Fatalf("깨진 파일을 .bad 로 옮기고 이전 .bad 는 덮는다: %q", data)
	}
	if _, ok, err := Load(path, "/a"); ok || err != nil {
		t.Fatalf("옮긴 뒤에는 새로 시작한다: %v %v", ok, err)
	}
}

func TestSave_깨진_파일은_bad로_옮기고_새_파일에_쓴다(t *testing.T) {
	path := tempPath(t)
	must(t, os.MkdirAll(filepath.Dir(path), 0o700))
	must(t, os.WriteFile(path, []byte("{broken"), 0o600))
	if err := Save(path, "/a", State{Tabs: []Tab{{Title: "a"}}}); err != nil {
		t.Fatal(err)
	}
	if data, _ := os.ReadFile(BadPath(path)); string(data) != "{broken" {
		t.Fatalf(".bad: %q", data)
	}
	if got, ok, err := Load(path, "/a"); !ok || err != nil || got.Tabs[0].Title != "a" {
		t.Fatalf("새 파일: %+v %v %v", got, ok, err)
	}
}

func must(t *testing.T, err error) {
	t.Helper()
	if err != nil {
		t.Fatal(err)
	}
}

func TestSave_되살리고_다른_프로젝트는_지키며_권한은_0600_0700이다(t *testing.T) {
	path := tempPath(t)
	a := State{Active: 1, Tabs: []Tab{
		{Title: "Query 1", Text: "select 1;\n-- 메모", Conn: "uuid-1", Cursor: 9},
		{Title: "users.sql", Text: "select * from users", File: "queries/users.sql"},
	}}
	b := State{Tabs: []Tab{{Title: "Query 1", Text: "select 2"}}}
	for dir, st := range map[string]State{"/proj/a": a, "/proj/b": b} {
		if err := Save(path, dir, st); err != nil {
			t.Fatal(err)
		}
	}
	a.Tabs[0].Text = "바뀐 글"
	if err := Save(path, "/proj/a", a); err != nil {
		t.Fatal(err)
	}
	for dir, want := range map[string]State{"/proj/a": a, "/proj/b": b} {
		got, ok, err := Load(path, dir)
		if err != nil || !ok || !reflect.DeepEqual(got, want) {
			t.Fatalf("%s: %+v %v %v", dir, got, ok, err)
		}
	}
	for p, want := range map[string]os.FileMode{filepath.Dir(path): 0o700, path: 0o600} {
		st, err := os.Stat(p)
		if err != nil {
			t.Fatal(err)
		}
		if got := st.Mode().Perm(); got != want {
			t.Errorf("%s: %o, want %o", p, got, want)
		}
	}
	if left, _ := filepath.Glob(filepath.Join(filepath.Dir(path), ".session-*")); len(left) != 0 {
		t.Fatalf("임시 파일이 남았다: %v", left)
	}
}

func TestSave_프로젝트_경로는_정리한_절대_경로로_같은_항목이다(t *testing.T) {
	path := tempPath(t)
	if err := Save(path, "/proj/x/../a/", State{Tabs: []Tab{{Title: "x"}}}); err != nil {
		t.Fatal(err)
	}
	if _, ok, err := Load(path, "/proj/a"); !ok || err != nil {
		t.Fatalf("정리한 경로로 읽기: %v %v", ok, err)
	}
}

func TestSave_글이_상한을_넘으면_활성_탭부터_넣고_나머지_글은_비운다(t *testing.T) {
	path := tempPath(t)
	big := strings.Repeat("x", maxProjectText/2+1)
	st := State{Active: 2, Tabs: []Tab{
		{Title: "a", Text: big, Cursor: 5},
		{Title: "b", Text: "small", Cursor: 3},
		{Title: "c", Text: big, File: "c.sql", Cursor: 7},
	}}
	if err := Save(path, "/p", st); err != nil {
		t.Fatal(err)
	}
	got, _, _ := Load(path, "/p")
	if got.Tabs[2].Text != big || got.Tabs[2].Cursor != 7 {
		t.Fatal("활성 탭 글은 남아야 한다")
	}
	if got.Tabs[0].Text != "" || got.Tabs[0].Cursor != 0 || got.Tabs[0].Title != "a" || !got.Tabs[0].Dropped {
		t.Fatalf("넘친 탭은 글만 비우고 비웠다고 표시한다: %+v", got.Tabs[0])
	}
	if got.Tabs[1].Text != "small" || got.Tabs[1].Dropped || got.Tabs[2].Dropped {
		t.Fatalf("들어가는 탭은 남는다: %+v", got.Tabs[1])
	}
	if st.Tabs[0].Text != big {
		t.Fatal("넘긴 State 를 바꾸지 않아야 한다")
	}
}

func TestLoad_활성_탭이_범위_밖이면_0이다(t *testing.T) {
	path := tempPath(t)
	if err := Save(path, "/p", State{Active: 5, Tabs: []Tab{{Title: "a"}}}); err != nil {
		t.Fatal(err)
	}
	if got, _, _ := Load(path, "/p"); got.Active != 0 {
		t.Fatalf("Active %d", got.Active)
	}
}

func TestTouch_최근_것부터_같은_파일은_하나로_접속마다_20개까지(t *testing.T) {
	var st State
	t0 := time.Date(2026, 11, 15, 9, 0, 0, 0, time.UTC)
	st.Touch("c1", "queries/a.sql", t0)
	st.Touch("c1", "queries/b.sql", t0.Add(time.Minute))
	st.Touch("c1", "queries/a.sql", t0.Add(2*time.Minute))
	st.Touch("c2", "queries/z.sql", t0)
	st.Touch("", "queries/x.sql", t0)
	st.Touch("c1", "", t0)
	want := []Recent{{"queries/a.sql", t0.Add(2 * time.Minute)}, {"queries/b.sql", t0.Add(time.Minute)}}
	if !reflect.DeepEqual(st.Recent["c1"], want) {
		t.Fatalf("c1 = %v, want %v", st.Recent["c1"], want)
	}
	if len(st.Recent) != 2 {
		t.Fatalf("빈 접속·파일은 남기지 않는다: %v", st.Recent)
	}
	if rs := st.Recent["c2"]; len(rs) == 0 || rs[0].File != "queries/z.sql" {
		t.Fatalf("c2 의 맨 앞: %+v", rs)
	}
	for i := range 25 {
		st.Touch("c3", fmt.Sprintf("f%d.sql", i), t0.Add(time.Duration(i)*time.Second))
	}
	if n, first, last := len(st.Recent["c3"]), st.Recent["c3"][0].File, st.Recent["c3"][19].File; n != 20 || first != "f24.sql" || last != "f5.sql" {
		t.Fatalf("20개까지 최근 것부터: n=%d first=%s last=%s", n, first, last)
	}
}

func TestTouch_복사한_State_의_목록을_건드리지_않는다(t *testing.T) {
	var a State
	a.Touch("c", "x.sql", time.Time{})
	a.Touch("c", "y.sql", time.Time{})
	b := a
	b.Recent = map[string][]Recent{"c": a.Recent["c"]}
	b.Touch("c", "x.sql", time.Time{})
	if a.Recent["c"][0].File != "y.sql" || a.Recent["c"][1].File != "x.sql" {
		t.Fatalf("원래 목록이 바뀜: %v", a.Recent["c"])
	}
}

func TestSave_최근_파일과_창_크기를_되살리고_예전_파일도_읽는다(t *testing.T) {
	path := tempPath(t)
	at := time.Date(2026, 11, 15, 9, 0, 0, 0, time.FixedZone("KST", 9*3600))
	st := State{Tabs: []Tab{{Title: "Query 1"}}, Panes: Panes{Tree: 40, Editor: 12}}
	st.Touch("c1", "queries/a.sql", at)
	must(t, Save(path, "/p", st))
	got, ok, err := Load(path, "/p")
	if err != nil || !ok {
		t.Fatalf("load: %v %v", ok, err)
	}
	if got.Panes != (Panes{Tree: 40, Editor: 12}) || len(got.Recent["c1"]) != 1 || !got.Recent["c1"][0].At.Equal(at) {
		t.Fatalf("되살림: %+v", got)
	}

	must(t, os.WriteFile(path, []byte(`{"/old":{"active":0,"tabs":[{"title":"Q"}]}}`), 0o600))
	old, ok, err := Load(path, "/old")
	if err != nil || !ok || old.Recent != nil || old.Panes != (Panes{}) || old.Tabs[0].Title != "Q" {
		t.Fatalf("v5 파일: %+v %v %v", old, ok, err)
	}
	if len(old.Recent) != 0 {
		t.Fatal("v5 파일에는 최근 기록이 없다")
	}
}
