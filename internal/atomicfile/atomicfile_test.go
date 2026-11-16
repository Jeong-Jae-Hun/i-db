package atomicfile

import (
	"errors"
	"os"
	"path/filepath"
	"testing"
	"time"
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

func TestTryLock_다른_쪽이_쥐고_있으면_기다린_시간_뒤_포기하고_풀리면_얻는다(t *testing.T) {
	path := filepath.Join(t.TempDir(), "a.json")
	held, err := Lock(path)
	if err != nil {
		t.Fatal(err)
	}
	start := time.Now()
	if _, err := TryLock(path, 50*time.Millisecond); !errors.Is(err, ErrLockTimeout) {
		t.Fatalf("잠금 기다림 초과: %v", err)
	}
	if d := time.Since(start); d > time.Second {
		t.Fatalf("정해진 시간만 기다린다: %v", d)
	}
	if err := held.Close(); err != nil {
		t.Fatal(err)
	}
	f, err := TryLock(path, 50*time.Millisecond)
	if err != nil {
		t.Fatalf("풀리면 얻는다: %v", err)
	}
	f.Close()
}
