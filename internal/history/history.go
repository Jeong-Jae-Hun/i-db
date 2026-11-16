// Package history 는 실행한 쿼리를 JSONL 파일 하나에 쌓고 최근 것부터 찾아 준다.
package history

import (
	"bytes"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/Jeong-Jae-Hun/i-db/internal/atomicfile"
)

// 상한을 넘으면 4/5 까지 줄인다. 상한에 딱 맞추면 그다음 Append 마다 파일 전체를 다시 쓰게 된다(E63).
const (
	maxEntries  = 5000
	maxBytes    = 5 << 20
	maxSQLBytes = 64 << 10 // 큰 스크립트 하나가 기록을 다 밀어내지 않게 문장 하나는 앞 64KiB 만 남긴다
)

type Entry struct {
	ID      string        `json:"id,omitempty"` // 결과 사본 파일 이름(R11.4). 옛 줄은 비어 있다
	Time    time.Time     `json:"time"`
	Conn    string        `json:"conn"`
	SQL     string        `json:"sql"`
	Elapsed time.Duration `json:"elapsed"`
	Rows    int           `json:"rows"`
	Err     string        `json:"err,omitempty"`
	Result  bool          `json:"result,omitempty"` // 결과 사본을 썼다. 그 뒤 보존 기간·d 로 지워졌을 수 있다
}

// Filter 의 Query 는 SQL 이나 접속 이름에서 대소문자 없이 찾을 부분 문자열, Conn 은 정확히 같은 접속 이름이다.
// From·To 는 [From, To) 시각 범위이고 0 이면 그쪽 끝이 없다. Limit 0 이하는 전부다.
type Filter struct {
	Query, Conn string
	From, To    time.Time
	Limit       int
}

// 기록 창 거르기 글의 날짜 낱말. 값은 오늘에서 거스를 날 수다
var dayWords = map[string]int{"오늘": 0, "today": 0, "어제": 1, "yesterday": 1}

// ParseQuery 는 기록 창 거르기 글을 Filter 로 바꾼다. 낱말 `오늘`·`today`·`어제`·`yesterday` 는 now 의 지역 날짜로
// 거르고(여럿이면 마지막 것), 나머지 낱말은 빈칸 하나로 이어 SQL·접속 이름에서 찾는다.
//   - 예: "어제 orders" → 어제 0시~오늘 0시 기록 가운데 orders 가 든 것
func ParseQuery(s string, now time.Time) Filter {
	var f Filter
	var rest []string
	for _, w := range strings.Fields(s) {
		back, ok := dayWords[strings.ToLower(w)]
		if !ok {
			rest = append(rest, w)
			continue
		}
		y, mo, d := now.Date()
		f.From = time.Date(y, mo, d-back, 0, 0, 0, 0, now.Location())
		f.To = f.From.AddDate(0, 0, 1)
	}
	f.Query = strings.Join(rest, " ")
	return f
}

// NewID 는 기록 하나의 이름이다. 시각(UTC, 정렬되게)에 무작위 6자를 붙여 같은 초의 기록도 겹치지 않는다.
//   - 예: 20261116-011530-a3f9c1
func NewID(t time.Time) string {
	var b [3]byte
	_, _ = rand.Read(b[:]) // 실패해도 0 이 남을 뿐 시각이 이름을 가른다
	return t.UTC().Format("20060102-150405") + "-" + hex.EncodeToString(b[:])
}

// Store 는 기록을 메모리(오래된 것부터)와 파일에 같이 둔다. 메서드는 동시에 불러도 된다.
// 다른 프로세스(MCP 서버 등)가 같은 파일에 붙여 쓴 기록은 다시 Open 하거나 줄일 때 읽힌다.
// 붙여 쓰기와 줄이기는 옆 .lock 파일의 flock 으로 프로세스끼리도 순서를 맞춘다.
type Store struct {
	mu      sync.Mutex
	path    string
	entries []Entry
	size    int64 // 파일 크기
}

func DefaultPath() (string, error) {
	if dir := os.Getenv("XDG_STATE_HOME"); filepath.IsAbs(dir) { // XDG 명세는 상대 경로면 무시하라고 한다
		return filepath.Join(dir, "db", "history.jsonl"), nil
	}
	home, err := os.UserHomeDir()
	if err != nil {
		return "", err
	}
	return filepath.Join(home, ".local", "state", "db", "history.jsonl"), nil
}

// Open 은 파일이 없으면 만든다(폴더 0700, 파일 0600). 읽지 못하는 줄은 건너뛴다.
func Open(path string) (*Store, error) {
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		return nil, err
	}
	f, err := os.OpenFile(path, os.O_CREATE|os.O_RDONLY, 0o600)
	if err != nil {
		return nil, err
	}
	if err := f.Close(); err != nil {
		return nil, err
	}
	entries, size, err := readTail(path)
	if err != nil {
		return nil, err
	}
	s := &Store{path: path, entries: entries, size: size}
	if !s.over() {
		return s, nil
	}
	lock, err := lockFile(path)
	if err != nil {
		return nil, err
	}
	if err := errors.Join(s.compactLocked(), lock.Close()); err != nil {
		return nil, err
	}
	return s, nil
}

func (s *Store) Append(e Entry) (err error) {
	if len(e.SQL) > maxSQLBytes {
		e.SQL = strings.ToValidUTF8(e.SQL[:maxSQLBytes], "") // 바이트로 자른 끝의 깨진 UTF-8 조각을 버린다
	}
	line, err := json.Marshal(e)
	if err != nil {
		return err
	}
	line = append(line, '\n')
	s.mu.Lock()
	defer s.mu.Unlock()
	lock, err := lockFile(s.path)
	if err != nil {
		return err
	}
	defer func() { err = errors.Join(err, lock.Close()) }()
	// 잠금을 잡은 뒤 열어야 다른 프로세스가 줄이며 바꿔 끼운 새 파일에 쓴다
	f, err := os.OpenFile(s.path, os.O_APPEND|os.O_CREATE|os.O_RDWR, 0o600)
	if err != nil {
		return err
	}
	st, err := f.Stat()
	if err != nil {
		return errors.Join(err, f.Close())
	}
	// 쓰다 끊긴 줄(줄바꿈 없는 끝)에 이어 쓰면 새 기록까지 깨지므로 줄을 먼저 끝낸다
	if st.Size() > 0 {
		last := make([]byte, 1)
		if _, err := f.ReadAt(last, st.Size()-1); err != nil {
			return errors.Join(fmt.Errorf("read history: %w", err), f.Close())
		}
		if last[0] != '\n' {
			line = append([]byte{'\n'}, line...)
		}
	}
	n, werr := f.Write(line)
	if err := errors.Join(werr, f.Close()); err != nil {
		return err
	}
	s.size = st.Size() + int64(n)
	s.entries = append(s.entries, e)
	if s.over() {
		return s.compactLocked()
	}
	return nil
}

// List 는 최근 것부터 돌려준다.
func (s *Store) List(f Filter) []Entry {
	q := strings.ToLower(f.Query)
	s.mu.Lock()
	defer s.mu.Unlock()
	var out []Entry
	for i := len(s.entries) - 1; i >= 0 && (f.Limit <= 0 || len(out) < f.Limit); i-- {
		e := s.entries[i]
		if (f.Conn != "" && e.Conn != f.Conn) || (!f.From.IsZero() && e.Time.Before(f.From)) || (!f.To.IsZero() && !e.Time.Before(f.To)) {
			continue
		}
		if strings.Contains(strings.ToLower(e.SQL), q) || strings.Contains(strings.ToLower(e.Conn), q) {
			out = append(out, e)
		}
	}
	return out
}

func (s *Store) over() bool { return len(s.entries) > maxEntries || s.size > maxBytes }

// compactLocked 는 파일을 다시 읽어(다른 프로세스가 붙인 기록까지) 최근 것만 남기고, 임시 파일에 다 쓴 뒤 바꿔 끼운다.
// 파일 잠금을 잡은 채로 불러야 다시 읽은 뒤 바꿔 끼우기 전에 남이 붙인 줄을 잃지 않는다.
func (s *Store) compactLocked() error {
	entries, _, err := readTail(s.path)
	if err != nil {
		return err
	}
	var lines [][]byte
	total := 0
	for i := len(entries) - 1; i >= 0 && len(lines) < maxEntries*4/5; i-- {
		line, err := json.Marshal(entries[i])
		if err != nil {
			return err
		}
		if total+len(line)+1 > maxBytes*4/5 {
			break
		}
		total += len(line) + 1
		lines = append(lines, line)
	}
	var buf bytes.Buffer
	for i := len(lines) - 1; i >= 0; i-- {
		buf.Write(lines[i])
		buf.WriteByte('\n')
	}
	if err := atomicfile.Write(s.path, ".history-*.jsonl", buf.Bytes(), 0o600); err != nil {
		return err
	}
	s.entries = entries[len(entries)-len(lines):]
	s.size = int64(buf.Len())
	return nil
}

// lockFile 은 path 옆 .lock 파일에 배타 flock 을 건다. 돌려준 파일을 닫으면 풀린다.
// 바꿔 끼우는 기록 파일 자체에 걸면 잠금이 옛 inode 에 남으므로 따로 둔다. 배포 대상(linux·darwin)은 flock 이 있다.
func lockFile(path string) (*os.File, error) {
	f, err := os.OpenFile(path+".lock", os.O_CREATE|os.O_RDWR, 0o600)
	if err != nil {
		return nil, err
	}
	if err := syscall.Flock(int(f.Fd()), syscall.LOCK_EX); err != nil {
		return nil, errors.Join(fmt.Errorf("lock history: %w", err), f.Close())
	}
	return f, nil
}

// readTail 은 파일 끝 maxBytes 만 읽어 기록으로 바꾼다. 파일이 아무리 커져도 읽는 양이 정해져 있다(E63).
// 오래된 것부터 최대 maxEntries 개와 파일 전체 크기를 돌려준다.
func readTail(path string) ([]Entry, int64, error) {
	f, err := os.Open(path)
	if err != nil {
		return nil, 0, err
	}
	defer f.Close() // 읽기 전용이라 닫기 오류로 잃을 데이터가 없다
	st, err := f.Stat()
	if err != nil {
		return nil, 0, err
	}
	from := max(st.Size()-maxBytes, 0)
	buf := make([]byte, st.Size()-from)
	// 다른 프로세스가 그사이 파일을 줄였으면 EOF 와 함께 덜 읽힌다. 읽힌 만큼만 쓴다
	n, err := f.ReadAt(buf, from)
	if err != nil && !errors.Is(err, io.EOF) {
		return nil, 0, fmt.Errorf("read history: %w", err)
	}
	buf = buf[:n]
	if from > 0 {
		// 중간부터 읽었으면 첫 줄은 잘린 조각이다
		_, buf, _ = bytes.Cut(buf, []byte{'\n'})
	}
	var entries []Entry
	for _, line := range bytes.Split(buf, []byte{'\n'}) {
		var e Entry
		// 깨진 줄(쓰다 끊긴 줄, 손으로 고친 줄)은 기록 하나만 잃고 나머지를 살리려고 건너뛴다
		if len(bytes.TrimSpace(line)) == 0 || json.Unmarshal(line, &e) != nil {
			continue
		}
		entries = append(entries, e)
	}
	if len(entries) > maxEntries {
		entries = entries[len(entries)-maxEntries:]
	}
	return entries, st.Size(), nil
}
