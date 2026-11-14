package paramstore

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

func TestSave_Load_프로젝트마다_합쳐_기억하고_0600_이다(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "state", "db", "params.json")
	a, b := filepath.Join(dir, "a"), filepath.Join(dir, "b")

	if got, err := Load(path, a); err != nil || len(got) != 0 {
		t.Fatalf("없는 파일: %v %v", got, err)
	}
	if err := Save(path, a, map[string]string{"user_id": "1315", "email": "user@example.com"}); err != nil {
		t.Fatal(err)
	}
	if err := Save(path, b, map[string]string{"user_id": "7"}); err != nil {
		t.Fatal(err)
	}
	// 덮어쓰기·빈 값 지우기·다른 이름 지키기
	if err := Save(path, a, map[string]string{"user_id": "42", "email": "", "slug": "nino"}); err != nil {
		t.Fatal(err)
	}
	if got, err := Load(path, a); err != nil || !reflect.DeepEqual(got, map[string]string{"user_id": "42", "slug": "nino"}) {
		t.Fatalf("a: %v %v", got, err)
	}
	if got, err := Load(path, b+string(filepath.Separator)+"."); err != nil || !reflect.DeepEqual(got, map[string]string{"user_id": "7"}) {
		t.Fatalf("b(같은 절대 경로): %v %v", got, err)
	}
	st, err := os.Stat(path)
	if err != nil || st.Mode().Perm() != 0o600 {
		t.Fatalf("권한: %v %v", st.Mode(), err)
	}
	if st, err := os.Stat(filepath.Dir(path)); err != nil || st.Mode().Perm() != 0o700 {
		t.Fatalf("폴더 권한: %v %v", st.Mode(), err)
	}
}

func TestSave_긴_값과_빈_이름은_건너뛰고_이름_수를_자른다(t *testing.T) {
	path := filepath.Join(t.TempDir(), "params.json")
	proj := t.TempDir()
	big := strings.Repeat("x", maxValue+1)
	if err := Save(path, proj, map[string]string{"ok": strings.Repeat("y", maxValue), "big": big, "": "v"}); err != nil {
		t.Fatal(err)
	}
	got, _ := Load(path, proj)
	if len(got) != 1 || len(got["ok"]) != maxValue {
		t.Fatalf("상한: %d개 %v", len(got), got["big"] != "")
	}
	// 긴 값으로 덮으려 하면 예전 값을 지킨다
	if err := Save(path, proj, map[string]string{"ok": big}); err != nil {
		t.Fatal(err)
	}
	if got, _ := Load(path, proj); len(got["ok"]) != maxValue {
		t.Fatal("긴 값이 예전 값을 지웠다")
	}

	old := map[string]string{}
	for i := range maxNames {
		old[fmt.Sprintf("old%03d", i)] = "v"
	}
	if err := Save(path, proj, old); err != nil {
		t.Fatal(err)
	}
	if err := Save(path, proj, map[string]string{"zz_new": "n"}); err != nil {
		t.Fatal(err)
	}
	got, _ = Load(path, proj)
	if len(got) != maxNames || got["zz_new"] != "n" {
		t.Fatalf("이름 수 %d, 새 값 %q", len(got), got["zz_new"])
	}
	// 이름순으로 채워 맨 뒤 기존 이름이 밀려난다
	if _, ok := got["old199"]; ok {
		t.Fatal("old199 가 남았다")
	}
}

func TestLoad_Save_깨진_파일은_bad_로_옮기고_새로_시작한다(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "params.json")
	if err := os.WriteFile(path, []byte("{broken"), 0o600); err != nil {
		t.Fatal(err)
	}
	if _, err := Load(path, dir); !errors.Is(err, errCorrupt) {
		t.Fatalf("Load: %v", err)
	}
	if b, err := os.ReadFile(badPath(path)); err != nil || string(b) != "{broken" {
		t.Fatalf("bad: %q %v", b, err)
	}
	if got, err := Load(path, dir); err != nil || len(got) != 0 {
		t.Fatalf("옮긴 뒤: %v %v", got, err)
	}

	if err := os.WriteFile(path, []byte(`{"x": 1}`), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := Save(path, dir, map[string]string{"id": "1"}); err != nil {
		t.Fatal(err)
	}
	if got, err := Load(path, dir); err != nil || got["id"] != "1" {
		t.Fatalf("Save 뒤: %v %v", got, err)
	}
	if b, _ := os.ReadFile(badPath(path)); string(b) != `{"x": 1}` {
		t.Fatalf("Save 가 옮긴 bad: %q", b)
	}
}

func TestPath_XDG_STATE_HOME_은_절대_경로일_때만_따른다(t *testing.T) {
	t.Setenv("XDG_STATE_HOME", "/xdg")
	if p, _ := Path(); p != filepath.Join("/xdg", "db", "params.json") {
		t.Errorf("절대: %s", p)
	}
	t.Setenv("XDG_STATE_HOME", "rel")
	t.Setenv("HOME", "/home/u")
	if p, _ := Path(); p != filepath.Join("/home/u", ".local", "state", "db", "params.json") {
		t.Errorf("상대: %s", p)
	}
}

func TestSave_Load_비밀_같은_이름의_값은_기억하지_않는다(t *testing.T) {
	path := filepath.Join(t.TempDir(), "params.json")
	dir := t.TempDir()
	if err := Save(path, dir, map[string]string{"user_id": "1", "password": "p", "api_TOKEN": "t", "pwd": "x", "secret_key": "k"}); err != nil {
		t.Fatal(err)
	}
	if got, err := Load(path, dir); err != nil || !reflect.DeepEqual(got, map[string]string{"user_id": "1"}) {
		t.Fatalf("got %v %v", got, err)
	}
	if b, _ := os.ReadFile(path); strings.Contains(string(b), `"p"`) {
		t.Fatal("파일에 비밀 값이 남으면 안 된다")
	}
}
