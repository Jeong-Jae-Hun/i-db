// Package selfupdate 는 GitHub 릴리스에서 이 OS·CPU 의 db 를 받아 실행 중인 바이너리를 바꿔 끼운다(R65).
// 받은 묶음은 checksums.txt 의 sha256 으로 검증하고, 묶음 안의 `db` 한 파일만 꺼낸다.
package selfupdate

import (
	"archive/tar"
	"bufio"
	"bytes"
	"compress/gzip"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"syscall"

	"github.com/Jeong-Jae-Hun/i-db/internal/atomicfile"
)

const (
	DefaultAPI  = "https://api.github.com"
	DefaultRepo = "Jeong-Jae-Hun/i-db"

	// maxArchive 는 받는 묶음과 꺼낸 바이너리의 상한이다. 지금 묶음은 15MiB 안팎이다
	maxArchive  = 100 << 20
	maxMetadata = 4 << 20
)

// ErrChecksum 은 받은 묶음의 sha256 이 checksums.txt 와 다르다는 뜻이다.
var ErrChecksum = errors.New("checksum mismatch")

// NotWritableError 는 바꿀 실행 파일 자리에 쓸 권한이 없다는 뜻이다.
type NotWritableError struct{ Path string }

func (e *NotWritableError) Error() string { return "쓸 수 없음: " + e.Path + " (sudo 로 다시)" }

// Release 는 릴리스 하나다. Assets 는 파일 이름 → 내려받기 주소다.
type Release struct {
	Tag    string
	Assets map[string]string
}

// Source 는 릴리스를 읽어 올 곳이다. API 가 비면 DefaultAPI, HTTP 가 nil 이면 GitHub https 로만 넘어가는 기본 클라이언트다.
// Token 은 API 요청에만 붙인다(묶음 주소는 다른 호스트로 넘어갈 수 있다).
// API 를 주입하면(테스트) 그 주소 아래의 내려받기는 https·GitHub 호스트가 아니어도 받는다.
type Source struct {
	HTTP  *http.Client
	API   string
	Token string
}

// client 는 넘어갈 때 처음과 다른 호스트(포트 포함)면 Authorization 을 지우는 클라이언트다.
// 표준 클라이언트는 포트만 다른 같은 호스트에는 그대로 넘긴다.
func (s Source) client() *http.Client {
	c := http.Client{CheckRedirect: checkRedirect}
	if s.HTTP != nil {
		c = *s.HTTP
	}
	next := c.CheckRedirect
	c.CheckRedirect = func(req *http.Request, via []*http.Request) error {
		if req.URL.Host != via[0].URL.Host {
			req.Header.Del("Authorization")
		}
		if next == nil {
			return redirectLimit(via)
		}
		return next(req, via)
	}
	return &c
}

// redirectLimit 는 표준 클라이언트와 같은 10번 상한이다.
func redirectLimit(via []*http.Request) error {
	if len(via) >= 10 {
		return errors.New("stopped after 10 redirects")
	}
	return nil
}

// checkRedirect 는 기본 클라이언트가 https 의 GitHub 호스트로만 넘어가게 한다.
func checkRedirect(req *http.Request, via []*http.Request) error {
	if !githubURL(req.URL) {
		return fmt.Errorf("refuse redirect to %s", req.URL.Redacted())
	}
	return redirectLimit(via)
}

// githubURL 은 릴리스 API·내려받기(github.com → *.githubusercontent.com)가 쓰는 https 주소인지다.
func githubURL(u *url.URL) bool {
	h := u.Hostname()
	return u.Scheme == "https" && (h == "api.github.com" || h == "github.com" || strings.HasSuffix(h, ".githubusercontent.com"))
}

// Release 는 repo 의 tag 릴리스다. tag 가 비면 최신 릴리스다.
func (s Source) Release(ctx context.Context, repo, tag string) (Release, error) {
	api := s.API
	if api == "" {
		api = DefaultAPI
	}
	if tag != "" && !Valid(tag) {
		return Release{}, fmt.Errorf("tag %q is not a version", tag)
	}
	u := strings.TrimSuffix(api, "/") + "/repos/" + repo + "/releases/latest"
	if tag != "" {
		u = strings.TrimSuffix(api, "/") + "/repos/" + repo + "/releases/tags/" + url.PathEscape(tag)
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, u, nil)
	if err != nil {
		return Release{}, err
	}
	req.Header.Set("Accept", "application/vnd.github+json")
	req.Header.Set("User-Agent", "db-selfupdate")
	if s.Token != "" {
		req.Header.Set("Authorization", "Bearer "+s.Token)
	}
	resp, err := s.client().Do(req)
	if err != nil {
		return Release{}, err
	}
	defer resp.Body.Close()
	switch {
	case resp.StatusCode == http.StatusNotFound && tag != "":
		return Release{}, fmt.Errorf("릴리스가 없음: %s", tag)
	case resp.StatusCode == http.StatusForbidden || resp.StatusCode == http.StatusTooManyRequests:
		return Release{}, fmt.Errorf("GitHub API %s (요청 한도면 GITHUB_TOKEN 을 두고 다시)", resp.Status)
	case resp.StatusCode != http.StatusOK:
		return Release{}, fmt.Errorf("GitHub API %s", resp.Status)
	}
	var body struct {
		Tag    string `json:"tag_name"`
		Assets []struct {
			Name string `json:"name"`
			URL  string `json:"browser_download_url"`
		} `json:"assets"`
	}
	if err := json.NewDecoder(io.LimitReader(resp.Body, maxMetadata)).Decode(&body); err != nil {
		return Release{}, fmt.Errorf("read release: %w", err)
	}
	// 태그는 화면·상태 파일에 그대로 나가므로 판 꼴만 받는다(제어 문자 차단)
	if !Valid(body.Tag) {
		return Release{}, fmt.Errorf("release tag %q is not a version", body.Tag)
	}
	if tag != "" && body.Tag != tag {
		return Release{}, fmt.Errorf("release tag %q does not match requested %s", body.Tag, tag)
	}
	rel := Release{Tag: body.Tag, Assets: map[string]string{}}
	for _, a := range body.Assets {
		rel.Assets[a.Name] = a.URL
	}
	return rel, nil
}

// archiveName 은 release.yml 이 올리는 묶음 이름이다.
func archiveName(goos, goarch string) string { return "db_" + goos + "_" + goarch + ".tar.gz" }

// Apply 는 rel 의 goos/goarch 묶음을 받아 검증하고 exePath 를 바꿔 끼운다. exePath 가 심볼릭 링크면 가리키는 파일을 바꾼다.
// 어느 단계에서 실패해도 원래 파일은 그대로다.
func (s Source) Apply(ctx context.Context, rel Release, goos, goarch, exePath string) error {
	name := archiveName(goos, goarch)
	archiveURL, ok := rel.Assets[name]
	if !ok {
		return fmt.Errorf("%s 에 %s 가 없음", rel.Tag, name)
	}
	sumsURL, ok := rel.Assets["checksums.txt"]
	if !ok {
		return fmt.Errorf("%s 에 checksums.txt 가 없음", rel.Tag)
	}
	target, err := filepath.EvalSymlinks(exePath)
	if err != nil {
		return err
	}
	perm := os.FileMode(0o755)
	if st, err := os.Stat(target); err == nil && st.Mode().Perm() != 0 {
		perm = st.Mode().Perm()
	}

	sums, err := s.download(ctx, sumsURL, maxMetadata)
	if err != nil {
		return fmt.Errorf("checksums.txt: %w", err)
	}
	want, err := checksumOf(sums, name)
	if err != nil {
		return err
	}
	archive, err := s.download(ctx, archiveURL, maxArchive)
	if err != nil {
		return fmt.Errorf("%s: %w", name, err)
	}
	got := sha256.Sum256(archive)
	if hex.EncodeToString(got[:]) != want {
		return fmt.Errorf("%w: %s", ErrChecksum, name)
	}
	bin, err := extractDB(archive)
	if err != nil {
		return fmt.Errorf("%s: %w", name, err)
	}
	if err := atomicfile.Write(target, ".db-update-*", bin, perm); err != nil {
		if errors.Is(err, fs.ErrPermission) || errors.Is(err, syscall.EROFS) {
			return &NotWritableError{Path: target}
		}
		return err
	}
	return nil
}

func (s Source) download(ctx context.Context, rawURL string, limit int64) ([]byte, error) {
	u, err := url.Parse(rawURL)
	if err != nil {
		return nil, err
	}
	injected := s.API != "" && strings.HasPrefix(rawURL, strings.TrimSuffix(s.API, "/")+"/")
	if !githubURL(u) && !injected {
		return nil, fmt.Errorf("refuse download from %s", u.Redacted())
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, rawURL, nil)
	if err != nil {
		return nil, err
	}
	req.Header.Set("User-Agent", "db-selfupdate")
	resp, err := s.client().Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("download %s", resp.Status)
	}
	data, err := io.ReadAll(io.LimitReader(resp.Body, limit+1))
	if err != nil {
		return nil, err
	}
	if int64(len(data)) > limit {
		return nil, fmt.Errorf("download exceeds %d bytes", limit)
	}
	return data, nil
}

// checksumOf 는 sha256sum 출력에서 name 의 해시다. release.yml 은 `sha256sum ./*.tar.gz` 로 만들어 이름 앞에 ./ 가 붙는다.
func checksumOf(sums []byte, name string) (string, error) {
	sc := bufio.NewScanner(bytes.NewReader(sums))
	for sc.Scan() {
		sum, file, ok := strings.Cut(strings.TrimSpace(sc.Text()), " ")
		if !ok {
			continue
		}
		// 두 칸 띄움과 바이너리 표시(*)를 걷어 낸다
		file = strings.TrimPrefix(strings.TrimPrefix(strings.TrimSpace(file), "*"), "./")
		if file == name && len(sum) == sha256.Size*2 {
			return strings.ToLower(sum), nil
		}
	}
	return "", fmt.Errorf("checksums.txt 에 %s 가 없음", name)
}

// extractDB 는 tar.gz 맨 위의 일반 파일 `db` 만 꺼낸다. 다른 항목은 디스크에 쓰지 않고 넘긴다.
// `db` 가 링크·폴더이거나 둘 이상이면 거절한다. 넘기는 항목도 풀어야 하므로 풀린 전체 크기를 2*maxArchive 로 막는다.
func extractDB(archive []byte) ([]byte, error) {
	gz, err := gzip.NewReader(bytes.NewReader(archive))
	if err != nil {
		return nil, err
	}
	lr := &io.LimitedReader{R: gz, N: 2 * maxArchive}
	tr := tar.NewReader(lr)
	var bin []byte
	for {
		h, err := tr.Next()
		// 상한에 닿으면 tar 는 EOF 로 보므로 남은 양으로 가른다(정확히 상한에서 끝나는 묶음도 거절한다)
		if lr.N <= 0 {
			return nil, fmt.Errorf("archive exceeds %d bytes uncompressed", 2*maxArchive)
		}
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			return nil, err
		}
		if strings.TrimPrefix(h.Name, "./") != "db" {
			continue
		}
		if h.Typeflag != tar.TypeReg {
			return nil, errors.New("db 가 일반 파일이 아님")
		}
		if bin != nil {
			return nil, errors.New("db 가 둘 이상")
		}
		data, err := io.ReadAll(io.LimitReader(tr, maxArchive+1))
		if err != nil {
			return nil, err
		}
		if len(data) > maxArchive {
			return nil, fmt.Errorf("db exceeds %d bytes", maxArchive)
		}
		if len(data) == 0 {
			return nil, errors.New("db 가 비었음")
		}
		bin = data
	}
	if bin == nil {
		return nil, errors.New("묶음에 db 가 없음")
	}
	return bin, nil
}
