package history

import (
	"bytes"
	"compress/gzip"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"regexp"
	"slices"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/Jeong-Jae-Hun/i-db/internal/atomicfile"
)

// ─── 결과 사본 (R11.4) ───
// 기록 한 줄이 만든 표·글 결과를 <상태 폴더>/db/results/<id>.json.gz 에 둔다. 사본은 덤이라 쓰지 못하면 끄고 기록은 이어 간다.

const (
	SnapshotRows  = 1000    // 사본에 남기는 앞 행 수
	SnapshotBytes = 1 << 20 // 사본 JSON(압축 전) 상한

	snapshotMaxAge   = 30 * 24 * time.Hour
	snapshotMaxTotal = 200 << 20
	// 쓸 때마다 폴더를 훑지 않게 줄이기는 이 간격에 한 번이다
	pruneEvery = 10 * time.Minute
	snapExt    = ".json.gz"
	// 쓰다 죽은 프로세스가 남긴 임시 파일(atomicfile 의 .snap-*)은 이만큼 지나면 지운다. 쓰는 중인 파일은 건드리지 않게 넉넉히 둔다
	snapTemp       = ".snap-"
	snapTempMaxAge = time.Hour
)

// idRe 는 사본 이름으로 받는 꼴이다(NewID). CLI 가 받은 글로 폴더 밖 경로를 열지 않게 한다
var idRe = regexp.MustCompile(`^[0-9]{8}-[0-9]{6}-[0-9a-f]{6}$`)

// Snapshot 은 결과 하나의 사본이다. RowCount 는 자르기 전 받은 행 수, Truncated 는 사본이 행·크기 상한에서 잘렸다는 뜻,
// ResultTruncated 는 결과 자체가 화면 상한(db.MaxRows)에서 잘렸다는 뜻이다.
type Snapshot struct {
	ID              string        `json:"id"`
	Time            time.Time     `json:"time"`
	Conn            string        `json:"conn"`
	SQL             string        `json:"sql"`
	Cols            []string      `json:"cols,omitempty"`
	Types           []string      `json:"types,omitempty"`
	Rows            [][]string    `json:"rows,omitempty"`
	Text            string        `json:"text,omitempty"`
	Elapsed         time.Duration `json:"elapsed"`
	RowCount        int           `json:"row_count"`
	Truncated       bool          `json:"truncated,omitempty"`
	ResultTruncated bool          `json:"result_truncated,omitempty"`
}

// Cap 은 SQL 을 기록 줄처럼 앞 64KiB 로 자르고, 앞 SnapshotRows 행만 남기고, JSON 이 SnapshotBytes 를 넘지 않게 뒤 행과 글을 버린다.
// 행·글을 버렸으면 Truncated 다. 다 들어가면 그대로 두고, 버려야 하면 `"truncated":true` 를 먼저 달고 그 크기까지 넣어 잰다.
// 행마다 크기를 더해 가며 자르므로 큰 결과도 한 번만 직렬화한다. JSON 크기는 json.Marshal 결과와 바이트까지 같게 센다.
// 단, 열 이름 같은 머리만으로 상한을 넘으면 여기서는 줄이지 못하고 Write 가 ErrTooLarge 로 거절한다.
func (s *Snapshot) Cap() {
	s.SQL = capSQL(s.SQL)
	if len(s.Rows) > SnapshotRows {
		s.Rows, s.Truncated = s.Rows[:SnapshotRows], true
	}
	rows := s.Rows
	s.Rows = nil
	// "rows" 필드는 `,"rows":[` + 행들(쉼표로 이음) + `]` 이라 행마다 (크기+1) 를 더하고 쉼표 하나를 뺀다. 행이 없으면 omitempty 로 빠진다
	sizes, all := make([]int, len(rows)), len(`,"rows":[]`)-1
	for i, r := range rows {
		b, _ := json.Marshal(r)
		sizes[i] = len(b)
		all += len(b) + 1
	}
	head, _ := json.Marshal(s)
	if !s.Truncated && (len(rows) == 0 && len(head) <= SnapshotBytes || len(head)+all <= SnapshotBytes) {
		s.Rows = rows
		return
	}
	s.Truncated = true
	head, _ = json.Marshal(s)
	if budget := SnapshotBytes - len(head); len(s.Text) > budget/2 {
		// 글은 JSON 이스케이프로 늘 수 있어 남은 몫의 반만 둔다
		s.Text = strings.ToValidUTF8(s.Text[:max(budget/2, 0)], "")
		head, _ = json.Marshal(s)
	}
	size, n := len(head)+len(`,"rows":[]`)-1, 0
	for n < len(sizes) && size+sizes[n]+1 <= SnapshotBytes {
		size += sizes[n] + 1
		n++
	}
	s.Rows = rows[:n]
}

// Results 는 사본 폴더다. 메서드는 동시에 불러도 된다. have 는 폴더에 있는 사본의 이름과 크기라
// 기록 창이 줄마다 파일을 보지 않고 ◆ 를 그린다.
type Results struct {
	dir      string
	mu       sync.Mutex
	have     map[string]int64
	pruned   time.Time
	off      atomic.Bool
	maxAge   time.Duration
	maxTotal int64
}

// ResultsDir 은 기록 파일 옆 results 폴더다.
func ResultsDir(historyPath string) string {
	return filepath.Join(filepath.Dir(historyPath), "results")
}

// OpenResults 는 폴더가 없으면 0700 으로 만들고(있으면 0700 으로 좁히고) 있는 사본 목록을 읽는다.
func OpenResults(dir string) (*Results, error) {
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return nil, err
	}
	// MkdirAll 은 이미 있는 폴더의 권한을 바꾸지 않는다. 사본은 실제 데이터라 넓게 만들어진 폴더도 좁힌다
	if err := os.Chmod(dir, 0o700); err != nil {
		return nil, err
	}
	return ReadResults(dir)
}

// ReadResults 는 폴더를 만들거나 고치지 않고 사본 목록만 읽는다(db history). 폴더가 없으면 os.ErrNotExist 를 감싼 오류다.
func ReadResults(dir string) (*Results, error) {
	r := &Results{dir: dir, have: map[string]int64{}, maxAge: snapshotMaxAge, maxTotal: snapshotMaxTotal}
	ents, err := os.ReadDir(dir)
	if err != nil {
		return nil, err
	}
	for _, e := range ents {
		id, ok := strings.CutSuffix(e.Name(), snapExt)
		if !ok || !idRe.MatchString(id) {
			continue
		}
		if info, err := e.Info(); err == nil {
			r.have[id] = info.Size()
		}
	}
	return r, nil
}

func (r *Results) path(id string) string { return filepath.Join(r.dir, id+snapExt) }

// Off 는 쓰기 실패로 사본을 껐는지다. Disable 은 처음 끈 때만 참이라 알림을 한 번만 낸다.
func (r *Results) Off() bool     { return r.off.Load() }
func (r *Results) Disable() bool { return r.off.CompareAndSwap(false, true) }

// Has 는 id 의 사본이 폴더에 있는지다.
func (r *Results) Has(id string) bool {
	r.mu.Lock()
	defer r.mu.Unlock()
	_, ok := r.have[id]
	return ok
}

// ErrTooLarge 는 Cap 을 거쳐도 사본 JSON 이 SnapshotBytes 를 넘어 남기지 않았다는 뜻이다. 쓰기 실패가 아니라 사본을 끄지 않는다.
var ErrTooLarge = errors.New("snapshot too large")

// Write 는 Cap 을 거친 사본을 gzip JSON 으로 바꿔 끼워 쓴다(파일 0600). 그래도 상한을 넘으면 쓰지 않고 ErrTooLarge 다.
func (r *Results) Write(s Snapshot) error {
	if !idRe.MatchString(s.ID) {
		return fmt.Errorf("bad snapshot id %q", s.ID)
	}
	s.Cap()
	data, err := json.Marshal(s)
	if err != nil {
		return err
	}
	if len(data) > SnapshotBytes {
		return ErrTooLarge
	}
	var buf bytes.Buffer
	zw := gzip.NewWriter(&buf)
	_, werr := zw.Write(data)
	if err := errors.Join(werr, zw.Close()); err != nil {
		return err
	}
	if err := atomicfile.Write(r.path(s.ID), ".snap-*", buf.Bytes(), 0o600); err != nil {
		return err
	}
	r.mu.Lock()
	r.have[s.ID] = int64(buf.Len())
	r.mu.Unlock()
	return nil
}

// Read 는 id 의 사본을 읽는다. 없으면 os.ErrNotExist 를 감싼 오류다.
func (r *Results) Read(id string) (Snapshot, error) {
	var s Snapshot
	if !idRe.MatchString(id) {
		return s, fmt.Errorf("bad snapshot id %q", id)
	}
	f, err := os.Open(r.path(id))
	if err != nil {
		return s, err
	}
	defer f.Close() // 읽기 전용이라 닫기 오류로 잃을 데이터가 없다
	zr, err := gzip.NewReader(f)
	if err != nil {
		return s, fmt.Errorf("read snapshot %s: %w", id, err)
	}
	// Cap 이 1MiB 로 자르므로 손으로 넣은 큰 파일만 이 상한에 걸린다
	if err := json.NewDecoder(io.LimitReader(zr, 4*SnapshotBytes)).Decode(&s); err != nil {
		return s, fmt.Errorf("read snapshot %s: %w", id, err)
	}
	return s, nil
}

// Delete 는 id 의 사본을 지운다. 없던 사본은 오류가 아니다.
func (r *Results) Delete(id string) error {
	if !idRe.MatchString(id) {
		return fmt.Errorf("bad snapshot id %q", id)
	}
	if err := os.Remove(r.path(id)); err != nil && !errors.Is(err, os.ErrNotExist) {
		return err
	}
	r.mu.Lock()
	delete(r.have, id)
	r.mu.Unlock()
	return nil
}

// MaybePrune 은 마지막 줄이기에서 pruneEvery 가 지났을 때만 Prune 한다. 쓰기 뒤에 부른다.
func (r *Results) MaybePrune(now time.Time) error {
	r.mu.Lock()
	due := now.Sub(r.pruned) >= pruneEvery
	r.mu.Unlock()
	if !due {
		return nil
	}
	return r.Prune(now)
}

// Prune 은 maxAge(30일)보다 오래된 사본과 한 시간 넘은 임시 파일을 지우고, 남은 합이 maxTotal(200MiB)을 넘으면 오래된 것부터 지운다.
// 나이는 파일 수정 시각으로 잰다. 다른 프로세스가 같은 폴더를 줄여도 없는 파일은 건너뛴다.
func (r *Results) Prune(now time.Time) error {
	r.mu.Lock()
	r.pruned = now
	r.mu.Unlock()
	ents, err := os.ReadDir(r.dir)
	if err != nil {
		return err
	}
	type snap struct {
		id   string
		at   time.Time
		size int64
	}
	var keep []snap
	var errs []error
	var total int64
	for _, e := range ents {
		if strings.HasPrefix(e.Name(), snapTemp) {
			if info, err := e.Info(); err == nil && now.Sub(info.ModTime()) > snapTempMaxAge {
				if err := os.Remove(filepath.Join(r.dir, e.Name())); err != nil && !errors.Is(err, os.ErrNotExist) {
					errs = append(errs, err)
				}
			}
			continue
		}
		id, ok := strings.CutSuffix(e.Name(), snapExt)
		if !ok || !idRe.MatchString(id) {
			continue
		}
		info, err := e.Info()
		if err != nil {
			continue
		}
		if now.Sub(info.ModTime()) > r.maxAge {
			errs = append(errs, r.Delete(id))
			continue
		}
		keep = append(keep, snap{id, info.ModTime(), info.Size()})
		total += info.Size()
	}
	slices.SortFunc(keep, func(a, b snap) int { return a.at.Compare(b.at) })
	for _, s := range keep {
		if total <= r.maxTotal {
			break
		}
		errs = append(errs, r.Delete(s.id))
		total -= s.size
	}
	return errors.Join(errs...)
}
