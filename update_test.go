package main

import (
	"archive/tar"
	"bytes"
	"compress/gzip"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/Jeong-Jae-Hun/i-db/internal/selfupdate"
)

func tarGzDB(t *testing.T, body string) []byte {
	t.Helper()
	var buf bytes.Buffer
	gz := gzip.NewWriter(&buf)
	tw := tar.NewWriter(gz)
	if err := tw.WriteHeader(&tar.Header{Name: "db", Mode: 0o755, Size: int64(len(body)), Typeflag: tar.TypeReg}); err != nil {
		t.Fatal(err)
	}
	if _, err := tw.Write([]byte(body)); err != nil {
		t.Fatal(err)
	}
	if err := errors.Join(tw.Close(), gz.Close()); err != nil {
		t.Fatal(err)
	}
	return buf.Bytes()
}

// testUpdater 는 v0.11.0·v0.12.0(최신) 두 판이 있는 가짜 GitHub 와 임시 실행 파일로 만든 updater 다.
// badSum 이면 checksums.txt 가 틀린 해시를 준다.
func testUpdater(t *testing.T, current string, badSum bool) (updater, string) {
	t.Helper()
	archives := map[string][]byte{"v0.11.0": tarGzDB(t, "bin-v0.11.0"), "v0.12.0": tarGzDB(t, "bin-v0.12.0")}
	var srv *httptest.Server
	mux := http.NewServeMux()
	release := func(w http.ResponseWriter, tag string) {
		if _, ok := archives[tag]; !ok {
			w.WriteHeader(http.StatusNotFound)
			return
		}
		_ = json.NewEncoder(w).Encode(map[string]any{"tag_name": tag, "assets": []map[string]string{
			{"name": "db_linux_amd64.tar.gz", "browser_download_url": srv.URL + "/dl/" + tag + "/a"},
			{"name": "checksums.txt", "browser_download_url": srv.URL + "/dl/" + tag + "/sums"},
		}})
	}
	mux.HandleFunc("/repos/o/r/releases/latest", func(w http.ResponseWriter, _ *http.Request) { release(w, "v0.12.0") })
	mux.HandleFunc("/repos/o/r/releases/tags/{tag}", func(w http.ResponseWriter, r *http.Request) { release(w, r.PathValue("tag")) })
	mux.HandleFunc("/dl/{tag}/a", func(w http.ResponseWriter, r *http.Request) { _, _ = w.Write(archives[r.PathValue("tag")]) })
	mux.HandleFunc("/dl/{tag}/sums", func(w http.ResponseWriter, r *http.Request) {
		sum := sha256.Sum256(archives[r.PathValue("tag")])
		if badSum {
			sum = sha256.Sum256([]byte("other"))
		}
		_, _ = w.Write([]byte(hex.EncodeToString(sum[:]) + "  ./db_linux_amd64.tar.gz\n"))
	})
	srv = httptest.NewServer(mux)
	t.Cleanup(srv.Close)
	exe := filepath.Join(t.TempDir(), "db")
	if err := os.WriteFile(exe, []byte("bin-"+current), 0o755); err != nil {
		t.Fatal(err)
	}
	return updater{
		src:     selfupdate.Source{HTTP: srv.Client(), API: srv.URL},
		repo:    "o/r",
		current: current,
		goos:    "linux",
		goarch:  "amd64",
		exe:     func() (string, error) { return exe, nil },
	}, exe
}

func runUpdate(t *testing.T, u updater, args ...string) (int, string, string) {
	t.Helper()
	var out, errb bytes.Buffer
	code := runUpdateCLI(context.Background(), u, args, &out, &errb)
	return code, out.String(), errb.String()
}

func TestUpdate_check_는_새_판이면_10_최신이면_0(t *testing.T) {
	cases := []struct {
		current string
		code    int
		out     string
	}{
		{"v0.11.0", exitNewer, "새 판 v0.11.0 → v0.12.0\n"},
		{"v0.12.0", exitOK, "이미 최신 (v0.12.0)\n"},
		{"v0.13.0", exitOK, "이미 최신 (v0.13.0)\n"},
		{"dev", exitNewer, "새 판 dev → v0.12.0\n"},
	}
	for _, c := range cases {
		u, exe := testUpdater(t, c.current, false)
		code, out, errOut := runUpdate(t, u, "--check")
		if code != c.code || out != c.out {
			t.Errorf("%s: 코드 %d 출력 %q 오류 %q", c.current, code, out, errOut)
		}
		if b, _ := os.ReadFile(exe); string(b) != "bin-"+c.current {
			t.Errorf("%s: --check 는 바꾸지 않는다: %q", c.current, b)
		}
	}
}

func TestUpdate_최신_판으로_바꾸고_전후_판을_보인다(t *testing.T) {
	u, exe := testUpdater(t, "v0.11.0", false)
	code, out, errOut := runUpdate(t, u)
	if code != exitOK || out != "v0.11.0 → v0.12.0\n" {
		t.Fatalf("코드 %d 출력 %q 오류 %q", code, out, errOut)
	}
	if b, _ := os.ReadFile(exe); string(b) != "bin-v0.12.0" {
		t.Fatalf("바뀐 내용: %q", b)
	}
	u, _ = testUpdater(t, "v0.12.0", false)
	if code, out, _ := runUpdate(t, u); code != exitOK || out != "이미 최신 (v0.12.0)\n" {
		t.Fatalf("이미 최신: %d %q", code, out)
	}
}

func TestUpdate_내리기와_소스_빌드는_force_가_있어야_바꾼다(t *testing.T) {
	cases := []struct {
		name, current string
		args          []string
		wantErr       string
		after         string
	}{
		{"내리기", "v0.12.0", []string{"--version", "v0.11.0"}, "낮은 판으로 내리기(v0.12.0 → v0.11.0)는 --force", "bin-v0.11.0"},
		{"소스 빌드", "dev", nil, "소스 빌드(dev)는 --force 로 바꿈", "bin-v0.12.0"},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			u, exe := testUpdater(t, c.current, false)
			code, _, errOut := runUpdate(t, u, c.args...)
			if code != exitRefused || !strings.Contains(errOut, c.wantErr) {
				t.Fatalf("코드 %d 오류 %q", code, errOut)
			}
			if b, _ := os.ReadFile(exe); string(b) != "bin-"+c.current {
				t.Fatalf("바뀌면 안 된다: %q", b)
			}
			if code, _, errOut := runUpdate(t, u, append(c.args, "--force")...); code != exitOK {
				t.Fatalf("--force: %d %q", code, errOut)
			}
			if b, _ := os.ReadFile(exe); string(b) != c.after {
				t.Fatalf("--force 뒤 내용: %q", b)
			}
		})
	}
}

func TestUpdate_실패하면_바꾸지_않고_종료_코드를_가른다(t *testing.T) {
	u, exe := testUpdater(t, "v0.11.0", true)
	if code, _, errOut := runUpdate(t, u); code != exitErr || !strings.Contains(errOut, "checksum mismatch") {
		t.Fatalf("체크섬: %d %q", code, errOut)
	}
	if b, _ := os.ReadFile(exe); string(b) != "bin-v0.11.0" {
		t.Fatalf("바뀌면 안 된다: %q", b)
	}
	u, _ = testUpdater(t, "v0.11.0", false)
	if code, _, errOut := runUpdate(t, u, "--version", "v9.9.9"); code != exitErr || !strings.Contains(errOut, "릴리스가 없음: v9.9.9") {
		t.Fatalf("없는 판: %d %q", code, errOut)
	}
	if code, _, _ := runUpdate(t, u, "--version", "latest"); code != exitUsage {
		t.Fatalf("판 꼴: %d", code)
	}
	if code, _, _ := runUpdate(t, u, "extra"); code != exitUsage {
		t.Fatalf("남는 인자: %d", code)
	}
	u.goos = "windows"
	if code, _, errOut := runUpdate(t, u); code != exitErr || !strings.Contains(errOut, "이 OS 는 릴리스가 없음: windows") {
		t.Fatalf("windows: %d %q", code, errOut)
	}
}

func TestUpdate_쓸_권한이_없으면_sudo_를_권한다(t *testing.T) {
	if os.Geteuid() == 0 {
		t.Skip("root 는 권한 검사를 받지 않는다")
	}
	u, exe := testUpdater(t, "v0.11.0", false)
	dir := filepath.Dir(exe)
	if err := os.Chmod(dir, 0o555); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = os.Chmod(dir, 0o755) })
	code, _, errOut := runUpdate(t, u)
	if code != exitErr || errOut != "쓸 수 없음: "+exe+" (sudo 로 다시)\n" {
		t.Fatalf("코드 %d 오류 %q", code, errOut)
	}
}

func TestUpdate_도움말(t *testing.T) {
	var out, errb bytes.Buffer
	if code := runCLI(context.Background(), []string{"help", "update"}, nil, true, &out, &errb, false); code != exitOK || !strings.Contains(out.String(), "10 새 판 있음") {
		t.Fatalf("help update: %d %q", code, out.String())
	}
	out.Reset()
	if code := runCLI(context.Background(), []string{"help"}, nil, true, &out, &errb, false); code != exitOK || !strings.Contains(out.String(), "update") {
		t.Fatalf("help: %q", out.String())
	}
	u, _ := testUpdater(t, "v0.11.0", false)
	errb.Reset()
	if code := runUpdateCLI(context.Background(), u, []string{"-h"}, &out, &errb); code != exitOK || !strings.Contains(errb.String(), "db update [--check]") {
		t.Fatalf("-h: %d %q", code, errb.String())
	}
}

func TestUpdateNotice_끄기_소스_빌드_새_판(t *testing.T) {
	path := filepath.Join(t.TempDir(), "update-check")
	env := func(v string) func(string) string {
		return func(k string) string {
			if k == "DB_UPDATE_CHECK" {
				return v
			}
			return ""
		}
	}
	calls := 0
	fetch := func(context.Context) (string, error) { calls++; return "v0.13.0", nil }
	if updateNoticeCmd(env("0"), "v0.12.0", path, fetch) != nil {
		t.Fatal("DB_UPDATE_CHECK=0 이면 묻지 않는다")
	}
	if updateNoticeCmd(env(""), "dev", path, fetch) != nil {
		t.Fatal("소스 빌드는 묻지 않는다")
	}
	cmd := updateNoticeCmd(env(""), "v0.12.0", path, fetch)
	if cmd == nil {
		t.Fatal("켜져 있으면 묻는다")
	}
	msg, ok := cmd().(updateNoticeMsg)
	if !ok || msg.tag != "v0.13.0" || calls != 1 {
		t.Fatalf("알림: %#v (묻기 %d번)", msg, calls)
	}
	// 하루 안에 다시 켜면 묻지 않고 남긴 판으로 알린다
	if msg, ok := updateNoticeCmd(env(""), "v0.12.0", path, fetch)().(updateNoticeMsg); !ok || msg.tag != "v0.13.0" || calls != 1 {
		t.Fatalf("다시 켬: %#v (묻기 %d번)", msg, calls)
	}
	// 이미 그 판이면 알리지 않는다
	if got := updateNoticeCmd(env(""), "v0.13.0", path, fetch)(); got != nil {
		t.Fatalf("최신이면 알림 없음: %#v", got)
	}

	m := testModel(t)
	m.status = "탭 1개 되살림"
	m.Update(updateNoticeMsg{tag: "v0.13.0"})
	if m.status != "탭 1개 되살림 · 새 판 v0.13.0 — db update" {
		t.Fatalf("상태 줄: %q", m.status)
	}
}
