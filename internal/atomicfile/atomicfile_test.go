package atomicfile

import (
	"os"
	"path/filepath"
	"testing"
)

func TestWrite_권한대로_바꿔_끼우고_실패하면_원래_것과_임시_파일을_남기지_않는다(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "a.json")
	if err := os.WriteFile(path, []byte("old"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := Write(path, ".a-*", []byte("new"), 0o600); err != nil {
		t.Fatal(err)
	}
	st, err := os.Stat(path)
	if err != nil {
		t.Fatal(err)
	}
	if data, _ := os.ReadFile(path); string(data) != "new" || st.Mode().Perm() != 0o600 {
		t.Fatalf("내용 %q 권한 %o", data, st.Mode().Perm())
	}
	// 폴더 자리에는 바꿔 끼울 수 없다
	sub := filepath.Join(dir, "sub")
	if err := os.Mkdir(sub, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := Write(sub, ".a-*", []byte("x"), 0o600); err == nil {
		t.Fatal("폴더 자리 쓰기는 실패해야 한다")
	}
	if st, err := os.Stat(sub); err != nil || !st.IsDir() {
		t.Fatalf("원래 것이 남아야 한다: %v", err)
	}
	if left, _ := filepath.Glob(filepath.Join(dir, ".a-*")); len(left) != 0 {
		t.Fatalf("임시 파일이 남았다: %v", left)
	}
}
