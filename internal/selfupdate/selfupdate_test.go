package selfupdate

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
	"sync"
	"testing"
	"time"
)

type tarEntry struct {
	name, body, link string
	typ              byte
}

func makeTarGz(t *testing.T, entries ...tarEntry) []byte {
	t.Helper()
	var buf bytes.Buffer
	gz := gzip.NewWriter(&buf)
	tw := tar.NewWriter(gz)
	for _, e := range entries {
		typ := e.typ
		if typ == 0 {
			typ = tar.TypeReg
		}
		h := &tar.Header{Name: e.name, Typeflag: typ, Mode: 0o755, Size: int64(len(e.body)), Linkname: e.link}
		if typ != tar.TypeReg {
			h.Size = 0
		}
		if err := tw.WriteHeader(h); err != nil {
			t.Fatal(err)
		}
		if typ == tar.TypeReg {
			if _, err := tw.Write([]byte(e.body)); err != nil {
				t.Fatal(err)
			}
		}
	}
	if err := tw.Close(); err != nil {
		t.Fatal(err)
	}
	if err := gz.Close(); err != nil {
		t.Fatal(err)
	}
	return buf.Bytes()
}

// fakeGitHub 는 releases API 와 내려받기를 흉내 낸다. tags 는 판 → 묶음 바이트, sums 가 비면 바른 checksums.txt 를 만든다.
type fakeGitHub struct {
	*httptest.Server
	latest string
	tags   map[string][]byte
	sums   string
	mu     sync.Mutex
	auth   []string // API 요청의 Authorization
	dlAuth []string // 내려받기 요청의 Authorization
}

const testArchive = "db_linux_amd64.tar.gz"

func newFakeGitHub(t *testing.T, latest string, tags map[string][]byte) *fakeGitHub {
	t.Helper()
	f := &fakeGitHub{latest: latest, tags: tags}
	mux := http.NewServeMux()
	release := func(w http.ResponseWriter, r *http.Request, tag string) {
		f.mu.Lock()
		f.auth = append(f.auth, r.Header.Get("Authorization"))
		f.mu.Unlock()
		if r.Header.Get("Accept") != "application/vnd.github+json" || r.Header.Get("User-Agent") != "db-selfupdate" {
			http.Error(w, "bad headers", http.StatusBadRequest)
			return
		}
		if _, ok := f.tags[tag]; !ok {
			http.NotFound(w, r)
			return
		}
		base := f.URL + "/dl/" + tag + "/"
		_ = json.NewEncoder(w).Encode(map[string]any{
			"tag_name": tag,
			"assets": []map[string]string{
				{"name": testArchive, "browser_download_url": base + testArchive},
				{"name": "checksums.txt", "browser_download_url": base + "checksums.txt"},
			},
		})
	}
	mux.HandleFunc("/repos/o/r/releases/latest", func(w http.ResponseWriter, r *http.Request) { release(w, r, f.latest) })
	mux.HandleFunc("/repos/o/r/releases/tags/{tag}", func(w http.ResponseWriter, r *http.Request) { release(w, r, r.PathValue("tag")) })
	mux.HandleFunc("/dl/{tag}/{name}", func(w http.ResponseWriter, r *http.Request) {
		f.mu.Lock()
		f.dlAuth = append(f.dlAuth, r.Header.Get("Authorization"))
		f.mu.Unlock()
		data := f.tags[r.PathValue("tag")]
		switch r.PathValue("name") {
		case testArchive:
			_, _ = w.Write(data)
		case "checksums.txt":
			if f.sums != "" {
				_, _ = w.Write([]byte(f.sums))
				return
			}
			sum := sha256.Sum256(data)
			// release.yml 의 `sha256sum ./*.tar.gz` 꼴
			_, _ = w.Write([]byte("0000000000000000000000000000000000000000000000000000000000000000  ./db_darwin_arm64.tar.gz\n" +
				hex.EncodeToString(sum[:]) + "  ./" + testArchive + "\n"))
		default:
			http.NotFound(w, r)
		}
	})
	f.Server = httptest.NewServer(mux)
	t.Cleanup(f.Close)
	return f
}

func (f *fakeGitHub) source(token string) Source {
	return Source{HTTP: f.Client(), API: f.URL, Token: token}
}

func writeExe(t *testing.T, dir string, mode os.FileMode) string {
	t.Helper()
	p := filepath.Join(dir, "db")
	if err := os.WriteFile(p, []byte("old"), mode); err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(p, mode); err != nil { // umask 를 거치지 않게
		t.Fatal(err)
	}
	return p
}

func TestRelease_최신과_태그를_읽고_토큰은_API_에만_붙인다(t *testing.T) {
	gh := newFakeGitHub(t, "v0.12.0", map[string][]byte{
		"v0.12.0": makeTarGz(t, tarEntry{name: "db", body: "new"}),
		"v0.11.0": makeTarGz(t, tarEntry{name: "db", body: "older"}),
	})
	ctx := context.Background()
	rel, err := gh.source("tok").Release(ctx, "o/r", "")
	if err != nil || rel.Tag != "v0.12.0" || rel.Assets[testArchive] == "" || rel.Assets["checksums.txt"] == "" {
		t.Fatalf("최신: %+v %v", rel, err)
	}
	rel, err = gh.source("").Release(ctx, "o/r", "v0.11.0")
	if err != nil || rel.Tag != "v0.11.0" {
		t.Fatalf("태그: %+v %v", rel, err)
	}
	if _, err := gh.source("").Release(ctx, "o/r", "v9.9.9"); err == nil || !strings.Contains(err.Error(), "릴리스가 없음: v9.9.9") {
		t.Fatalf("없는 태그: %v", err)
	}
	if gh.auth[0] != "Bearer tok" || gh.auth[1] != "" {
		t.Fatalf("Authorization: %q", gh.auth)
	}
	dir := t.TempDir()
	exe := writeExe(t, dir, 0o755)
	rel, _ = gh.source("tok").Release(ctx, "o/r", "")
	if err := gh.source("tok").Apply(ctx, rel, "linux", "amd64", exe); err != nil {
		t.Fatal(err)
	}
	for _, a := range gh.dlAuth {
		if a != "" {
			t.Fatalf("내려받기에 토큰이 붙음: %q", gh.dlAuth)
		}
	}
}

func TestApply_검증한_db_로_바꾸고_권한과_링크를_지킨다(t *testing.T) {
	gh := newFakeGitHub(t, "v0.12.0", map[string][]byte{
		"v0.12.0": makeTarGz(t, tarEntry{name: "README", body: "x"}, tarEntry{name: "./db", body: "new"}),
	})
	ctx := context.Background()
	rel, err := gh.source("").Release(ctx, "o/r", "")
	if err != nil {
		t.Fatal(err)
	}
	real := t.TempDir()
	target := writeExe(t, real, 0o750)
	link := filepath.Join(t.TempDir(), "db")
	if err := os.Symlink(target, link); err != nil {
		t.Fatal(err)
	}
	if err := gh.source("").Apply(ctx, rel, "linux", "amd64", link); err != nil {
		t.Fatal(err)
	}
	if fi, err := os.Lstat(link); err != nil || fi.Mode()&os.ModeSymlink == 0 {
		t.Fatalf("링크가 그대로여야 한다: %v", err)
	}
	st, _ := os.Stat(target)
	if b, _ := os.ReadFile(target); string(b) != "new" || st.Mode().Perm() != 0o750 {
		t.Fatalf("내용 %q 권한 %o", b, st.Mode().Perm())
	}
	if left, _ := filepath.Glob(filepath.Join(real, ".db-update-*")); len(left) != 0 || len(mustReadDir(t, real)) != 1 {
		t.Fatalf("다른 파일이 남음: %v", mustReadDir(t, real))
	}
	// 이 OS·CPU 묶음이 없는 판
	if err := gh.source("").Apply(ctx, rel, "darwin", "amd64", link); err == nil || !strings.Contains(err.Error(), "db_darwin_amd64.tar.gz") {
		t.Fatalf("없는 묶음: %v", err)
	}
}

func mustReadDir(t *testing.T, dir string) []string {
	t.Helper()
	ents, err := os.ReadDir(dir)
	if err != nil {
		t.Fatal(err)
	}
	var names []string
	for _, e := range ents {
		names = append(names, e.Name())
	}
	return names
}

func TestApply_체크섬이_다르거나_db_가_이상하면_아무것도_바꾸지_않는다(t *testing.T) {
	cases := []struct {
		name    string
		archive []byte
		sums    string
		want    string
	}{
		{"체크섬 다름", makeTarGz(t, tarEntry{name: "db", body: "evil"}), strings.Repeat("ab", 32) + "  ./" + testArchive + "\n", "checksum mismatch"},
		{"체크섬 줄 없음", makeTarGz(t, tarEntry{name: "db", body: "new"}), strings.Repeat("ab", 32) + "  ./other.tar.gz\n", "checksums.txt 에 " + testArchive + " 가 없음"},
		{"db 없음", makeTarGz(t, tarEntry{name: "README", body: "x"}), "", "묶음에 db 가 없음"},
		{"다른 경로의 db", makeTarGz(t, tarEntry{name: "../db", body: "x"}, tarEntry{name: "sub/db", body: "x"}), "", "묶음에 db 가 없음"},
		{"심볼릭 링크 db", makeTarGz(t, tarEntry{name: "db", link: "/bin/sh", typ: tar.TypeSymlink}), "", "db 가 일반 파일이 아님"},
		{"db 둘", makeTarGz(t, tarEntry{name: "db", body: "a"}, tarEntry{name: "./db", body: "b"}), "", "db 가 둘 이상"},
		{"빈 db", makeTarGz(t, tarEntry{name: "db"}), "", "db 가 비었음"},
		{"gzip 아님", []byte("this is definitely not a gzip stream"), "", "gzip: invalid header"},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			gh := newFakeGitHub(t, "v0.12.0", map[string][]byte{"v0.12.0": c.archive})
			gh.sums = c.sums
			ctx := context.Background()
			rel, err := gh.source("").Release(ctx, "o/r", "")
			if err != nil {
				t.Fatal(err)
			}
			dir := t.TempDir()
			exe := writeExe(t, dir, 0o755)
			err = gh.source("").Apply(ctx, rel, "linux", "amd64", exe)
			if err == nil || !strings.Contains(err.Error(), c.want) {
				t.Fatalf("오류 %v, %q 를 기대", err, c.want)
			}
			if c.name == "체크섬 다름" && !errors.Is(err, ErrChecksum) {
				t.Fatalf("ErrChecksum 이어야 한다: %v", err)
			}
			if b, _ := os.ReadFile(exe); string(b) != "old" || len(mustReadDir(t, dir)) != 1 {
				t.Fatalf("바뀌면 안 된다: %q %v", b, mustReadDir(t, dir))
			}
		})
	}
}

func TestApply_쓸_권한이_없으면_경로와_sudo_를_알린다(t *testing.T) {
	if os.Geteuid() == 0 {
		t.Skip("root 는 권한 검사를 받지 않는다")
	}
	gh := newFakeGitHub(t, "v0.12.0", map[string][]byte{"v0.12.0": makeTarGz(t, tarEntry{name: "db", body: "new"})})
	ctx := context.Background()
	rel, _ := gh.source("").Release(ctx, "o/r", "")
	dir := t.TempDir()
	exe := writeExe(t, dir, 0o755)
	if err := os.Chmod(dir, 0o555); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = os.Chmod(dir, 0o755) })
	err := gh.source("").Apply(ctx, rel, "linux", "amd64", exe)
	var nw *NotWritableError
	if !errors.As(err, &nw) || err.Error() != "쓸 수 없음: "+exe+" (sudo 로 다시)" {
		t.Fatalf("오류: %v", err)
	}
	if b, _ := os.ReadFile(exe); string(b) != "old" {
		t.Fatalf("바뀌면 안 된다: %q", b)
	}
}

func TestCompare_판_비교표(t *testing.T) {
	cases := []struct {
		a, b string
		want int
		ok   bool
	}{
		{"v0.12.0", "v0.11.0", 1, true},
		{"v0.9.0", "v0.12.0", -1, true},
		{"v1.0.0", "v0.99.99", 1, true},
		{"v0.12.0", "v0.12.0", 0, true},
		{"v0.12.0", "v0.12.0-rc1", 1, true},
		{"v0.12.0-rc1", "v0.12.0-rc2", -1, true},
		{"v0.12.0+meta", "v0.12.0", 0, true},
		{"v0.0.0-20261115000000-abcdef", "v0.1.0", -1, true},
		{"dev", "v0.1.0", 0, false},
		{"0.12.0", "v0.1.0", 0, false},
		{"v1.2", "v1.2.0", 0, false},
		{"v1.x.0", "v1.2.0", 0, false},
	}
	for _, c := range cases {
		got, ok := Compare(c.a, c.b)
		if got != c.want || ok != c.ok {
			t.Errorf("Compare(%q, %q) = %d,%v 기대 %d,%v", c.a, c.b, got, ok, c.want, c.ok)
		}
	}
}

func TestDaily_하루_안이면_남긴_판을_쓰고_지나면_새로_묻는다(t *testing.T) {
	now := time.Date(2026, 11, 15, 12, 0, 0, 0, time.UTC)
	write := func(t *testing.T, path string, checked time.Time, latest string) {
		b, _ := json.Marshal(checkState{Checked: checked, Latest: latest})
		if err := os.WriteFile(path, b, 0o600); err != nil {
			t.Fatal(err)
		}
	}
	cases := []struct {
		name      string
		prepare   func(t *testing.T, path string)
		fetched   string
		fetchErr  error
		current   string
		want      string
		wantCalls int
	}{
		{"파일 없음 → 묻고 새 판", nil, "v0.13.0", nil, "v0.12.0", "v0.13.0", 1},
		{"하루 안 → 남긴 새 판을 묻지 않고 보임", func(t *testing.T, p string) { write(t, p, now.Add(-time.Hour), "v0.13.0") }, "v9.0.0", nil, "v0.12.0", "v0.13.0", 0},
		{"하루 안 → 남긴 판이 같으면 없음", func(t *testing.T, p string) { write(t, p, now.Add(-time.Hour), "v0.12.0") }, "v9.0.0", nil, "v0.12.0", "", 0},
		{"하루 지남 → 다시 묻는다", func(t *testing.T, p string) { write(t, p, now.Add(-25*time.Hour), "v0.12.0") }, "v0.14.0", nil, "v0.12.0", "v0.14.0", 1},
		{"앞으로의 시각 → 다시 묻는다", func(t *testing.T, p string) { write(t, p, now.Add(time.Hour), "v0.13.0") }, "v0.12.0", nil, "v0.12.0", "", 1},
		{"깨진 파일 → 다시 묻는다", func(t *testing.T, p string) { _ = os.WriteFile(p, []byte("{"), 0o600) }, "v0.13.0", nil, "v0.12.0", "v0.13.0", 1},
		{"네트워크 오류 → 조용히 없음", nil, "", errors.New("offline"), "v0.12.0", "", 1},
		{"지금 판이 더 높음", nil, "v0.12.0", nil, "v0.13.0", "", 1},
		{"상태 파일의 판에 제어 문자 → 다시 묻는다", func(t *testing.T, p string) { write(t, p, now.Add(-time.Hour), "v0.13.0-\x1b[2J") }, "v0.13.0", nil, "v0.12.0", "v0.13.0", 1},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			path := filepath.Join(t.TempDir(), "state", "update-check")
			if c.prepare != nil {
				if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
					t.Fatal(err)
				}
				c.prepare(t, path)
			}
			calls := 0
			fetch := func(context.Context) (string, error) { calls++; return c.fetched, c.fetchErr }
			if got := Daily(context.Background(), path, now, c.current, fetch); got != c.want || calls != c.wantCalls {
				t.Fatalf("Daily = %q (묻기 %d번), 기대 %q (%d번)", got, calls, c.want, c.wantCalls)
			}
			if c.wantCalls == 1 && c.fetchErr == nil {
				st, err := os.Stat(path)
				if err != nil || st.Mode().Perm() != 0o600 {
					t.Fatalf("상태 파일 0600: %v", err)
				}
				var s checkState
				b, _ := os.ReadFile(path)
				if json.Unmarshal(b, &s) != nil || s.Latest != c.fetched || !s.Checked.Equal(now) {
					t.Fatalf("남긴 상태: %s", b)
				}
			}
		})
	}
}

func TestExtractDB_압축_풀린_전체_크기를_넘으면_다른_항목을_건너뛰다가도_멈춘다(t *testing.T) {
	var buf bytes.Buffer
	gz, _ := gzip.NewWriterLevel(&buf, gzip.BestSpeed)
	tw := tar.NewWriter(gz)
	const big = 2*maxArchive + 1<<20
	if err := tw.WriteHeader(&tar.Header{Name: "pad", Typeflag: tar.TypeReg, Mode: 0o644, Size: big}); err != nil {
		t.Fatal(err)
	}
	zeros := make([]byte, 1<<20)
	for range big >> 20 {
		if _, err := tw.Write(zeros); err != nil {
			t.Fatal(err)
		}
	}
	if err := tw.WriteHeader(&tar.Header{Name: "db", Typeflag: tar.TypeReg, Mode: 0o755, Size: 3}); err != nil {
		t.Fatal(err)
	}
	_, _ = tw.Write([]byte("new"))
	if err := errors.Join(tw.Close(), gz.Close()); err != nil {
		t.Fatal(err)
	}
	if _, err := extractDB(buf.Bytes()); err == nil || !strings.Contains(err.Error(), "uncompressed") {
		t.Fatalf("압축 폭탄은 거절해야 한다: %v", err)
	}
}

func TestClient_기본은_https_와_GitHub_호스트만_따라가고_토큰은_API_에만_남긴다(t *testing.T) {
	cases := []struct {
		url      string
		ok       bool
		keepAuth bool
	}{
		{"https://api.github.com/repos/o/r", true, true},
		{"https://github.com/o/r/releases/download/v1.0.0/x", true, false},
		{"https://objects.githubusercontent.com/x", true, false},
		{"http://github.com/x", false, false},
		{"https://evil.example/x", false, false},
		{"https://githubusercontent.com.evil.example/x", false, false},
	}
	for _, c := range cases {
		req := httptest.NewRequest(http.MethodGet, c.url, nil)
		req.Header.Set("Authorization", "Bearer tok")
		err := Source{}.client().CheckRedirect(req, []*http.Request{httptest.NewRequest(http.MethodGet, "https://api.github.com/", nil)})
		if (err == nil) != c.ok {
			t.Errorf("%s: 오류 %v", c.url, err)
		}
		if c.ok && (req.Header.Get("Authorization") != "") != c.keepAuth {
			t.Errorf("%s: Authorization %q", c.url, req.Header.Get("Authorization"))
		}
	}
}

func TestRelease_다른_호스트로_넘어가면_토큰을_보내지_않는다(t *testing.T) {
	var got []string
	other := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		got = append(got, r.Header.Get("Authorization"))
		_, _ = w.Write([]byte(`{"tag_name":"v1.0.0"}`))
	}))
	t.Cleanup(other.Close)
	api := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		http.Redirect(w, r, other.URL+"/latest", http.StatusFound)
	}))
	t.Cleanup(api.Close)
	if _, err := (Source{HTTP: api.Client(), API: api.URL, Token: "tok"}).Release(context.Background(), "o/r", ""); err != nil {
		t.Fatal(err)
	}
	if len(got) != 1 || got[0] != "" {
		t.Fatalf("넘어간 호스트의 Authorization: %q", got)
	}
}

func TestApply_https_가_아니거나_주입한_주소_밖이면_받지_않는다(t *testing.T) {
	gh := newFakeGitHub(t, "v0.12.0", map[string][]byte{"v0.12.0": makeTarGz(t, tarEntry{name: "db", body: "new"})})
	exe := writeExe(t, t.TempDir(), 0o755)
	for _, src := range []Source{gh.source(""), {}} {
		rel := Release{Tag: "v0.12.0", Assets: map[string]string{
			testArchive:     "http://127.0.0.1:1/" + testArchive,
			"checksums.txt": "http://127.0.0.1:1/checksums.txt",
		}}
		if err := src.Apply(context.Background(), rel, "linux", "amd64", exe); err == nil || !strings.Contains(err.Error(), "refuse download") {
			t.Fatalf("API %q: %v", src.API, err)
		}
	}
}

func TestRelease_판_꼴이_아니거나_요청과_다른_태그는_거절한다(t *testing.T) {
	tag := "v1.0.0"
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_ = json.NewEncoder(w).Encode(map[string]string{"tag_name": tag})
	}))
	t.Cleanup(srv.Close)
	src := Source{HTTP: srv.Client(), API: srv.URL}
	ctx := context.Background()
	if _, err := src.Release(ctx, "o/r", "v1.1.0"); err == nil || !strings.Contains(err.Error(), "does not match") {
		t.Fatalf("다른 태그: %v", err)
	}
	tag = "v1.0.0-\x1b]0;x\x07"
	if _, err := src.Release(ctx, "o/r", ""); err == nil || !strings.Contains(err.Error(), "not a version") || strings.Contains(err.Error(), "\x1b") {
		t.Fatalf("판 꼴 아님: %q", err)
	}
	if _, err := src.Release(ctx, "o/r", "v1.0.0/../x"); err == nil {
		t.Fatal("판 꼴이 아닌 요청은 주소를 만들기 전에 거절한다")
	}
}

func TestCompare_pre_release_는_semver_순서를_따른다(t *testing.T) {
	cases := []struct {
		a, b string
		want int
		ok   bool
	}{
		{"v1.0.0-rc2", "v1.0.0-rc10", 1, true}, // rc2 는 한 식별자라 글자로 비교한다(semver). 숫자로 비교하려면 rc.2
		{"v1.0.0-rc.2", "v1.0.0-rc.10", -1, true},
		{"v1.0.0-1", "v1.0.0-alpha", -1, true},
		{"v1.0.0-alpha", "v1.0.0-alpha.1", -1, true},
		{"v1.0.0-alpha.1", "v1.0.0-alpha.beta", -1, true},
		{"v1.0.0-beta.11", "v1.0.0-rc.1", -1, true},
		{"v1.0.0-2", "v1.0.0-10", -1, true},
		{"v1.0.0-01", "v1.0.0", 0, false},
		{"v01.0.0", "v1.0.0", 0, false},
		{"v1.0.0-", "v1.0.0", 0, false},
		{"v1.0.0-a..b", "v1.0.0", 0, false},
		{"v1.0.0-a\x1b", "v1.0.0", 0, false},
		{"v1.0.0+b\x1b", "v1.0.0", 0, false},
		{"v1.0.0+001", "v1.0.0", 0, true},
		{"v+1.0.0", "v1.0.0", 0, false},
	}
	for _, c := range cases {
		got, ok := Compare(c.a, c.b)
		if got != c.want || ok != c.ok {
			t.Errorf("Compare(%q, %q) = %d,%v 기대 %d,%v", c.a, c.b, got, ok, c.want, c.ok)
		}
	}
}
