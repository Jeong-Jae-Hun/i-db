// Package recent 는 자주·최근 쓴 동작·접속·테이블과 테이블 보기 설정, 입력 기록, 트리 펼침을 기억한다(명세 R75).
// WHERE·거르기 글에 개인정보가 들 수 있어 세션(R41)처럼 레포가 아닌 $XDG_STATE_HOME/db/recent.json(0600)에 둔다.
// 동작·거르기 기록은 모든 프로젝트가 같이 쓰고, 접속·테이블·트리는 프로젝트 절대 경로를 키로 따로 둔다.
// 기억은 덤이라 읽지 못하면 빈 기록으로 시작한다.
package recent

import (
	"bytes"
	"cmp"
	"encoding/json"
	"errors"
	"maps"
	"math"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"time"

	"github.com/Jeong-Jae-Hun/i-db/internal/atomicfile"
	"github.com/Jeong-Jae-Hun/i-db/internal/session"
)

const (
	maxActions = 50  // 기억할 동작 수. 넘으면 frecency 가 가장 낮은 것부터 버린다
	maxConns   = 50  // 프로젝트마다 기억할 접속 수
	maxTables  = 500 // 프로젝트마다 기억할 테이블 수. 넘으면 가장 오래 안 연 것부터 버린다(LRU)
	maxFilters = 100 // 결과 거르기 기록(모든 프로젝트 공통)
	maxWheres  = 20  // 테이블마다 WHERE 입력 기록
	maxWidths  = 100 // 테이블마다 기억할 열 폭 수
	maxTree    = 500 // 프로젝트마다 기억할 펼친 노드 수
	maxText    = 4 << 10
	// 사용 횟수는 halfLife 마다 반으로 줄여 센다. 예전에 많이 쓴 것보다 요즘 쓴 것이 앞에 오게 한다
	halfLife = 7 * 24 * time.Hour
)

// Use 는 횟수와 마지막으로 쓴 때다.
type Use struct {
	N  int       `json:"n"`
	At time.Time `json:"at"`
}

// score 는 frecency 점수다. 횟수에 마지막 사용 뒤 지난 시간만큼 줄어드는 가중치를 곱한다.
func (u Use) score(now time.Time) float64 {
	age := max(0, now.Sub(u.At))
	return float64(u.N) * math.Exp2(-float64(age)/float64(halfLife))
}

// Table 은 테이블 하나의 기억이다. Where·Wheres 는 prod 접속이면 비운다.
type Table struct {
	Conn   string         `json:"conn"` // 접속 uuid
	Schema string         `json:"schema"`
	Name   string         `json:"name"`
	At     time.Time      `json:"at"`
	Widths map[string]int `json:"widths,omitempty"`
	Order  string         `json:"order,omitempty"`
	Where  string         `json:"where,omitempty"`
	Wheres []string       `json:"wheres,omitempty"` // WHERE 입력 기록, 최근 것부터
}

type project struct {
	Conns  map[string]Use    `json:"conns,omitempty"`
	Tables map[string]*Table `json:"tables,omitempty"`
	Open   []string          `json:"open,omitempty"`   // 펼친 노드 id
	Closed []string          `json:"closed,omitempty"` // 접은 그룹 id(그룹은 처음에 펼쳐져 있다)
}

type file struct {
	Actions  map[string]Use      `json:"actions,omitempty"`
	Filters  []string            `json:"filters,omitempty"`
	Projects map[string]*project `json:"projects,omitempty"`
}

// Store 는 한 프로젝트에서 본 기억이다. 화면 고루틴에서만 쓴다.
type Store struct {
	path, key string
	f         file
	p         *project
}

// Path 는 기억 파일 경로다. 세션 파일과 같은 폴더의 recent.json 이다.
func Path() (string, error) {
	sp, err := session.Path()
	if err != nil {
		return "", err
	}
	return filepath.Join(filepath.Dir(sp), "recent.json"), nil
}

// Load 는 projectDir 의 기억을 읽는다. 파일이 없으면 빈 기억이다. 읽지 못하거나 깨졌으면 빈 기억과 그 오류를 같이 돌려준다
// (다음 Save 가 새로 쓴다). 프로젝트 경로를 절대 경로로 바꾸지 못할 때만 nil 이다.
func Load(path, projectDir string) (*Store, error) {
	key, err := filepath.Abs(projectDir)
	if err != nil {
		return nil, err
	}
	s := &Store{path: path, key: key}
	f, err := readFile(path)
	if err == nil {
		s.f = f
	}
	s.attach()
	return s, err
}

func (s *Store) attach() {
	if s.f.Actions == nil {
		s.f.Actions = map[string]Use{}
	}
	if s.f.Projects == nil {
		s.f.Projects = map[string]*project{}
	}
	s.p = s.f.Projects[s.key]
	if s.p == nil {
		s.p = &project{}
		s.f.Projects[s.key] = s.p
	}
	if s.p.Conns == nil {
		s.p.Conns = map[string]Use{}
	}
	if s.p.Tables == nil {
		s.p.Tables = map[string]*Table{}
	}
}

func readFile(path string) (file, error) {
	var f file
	data, err := os.ReadFile(path)
	if errors.Is(err, os.ErrNotExist) {
		return f, nil
	}
	if err != nil {
		return f, err
	}
	if len(bytes.TrimSpace(data)) == 0 {
		return f, nil
	}
	if err := json.Unmarshal(data, &f); err != nil {
		return file{}, err
	}
	return f, nil
}

// LockWait 은 Write 가 recent.json.lock 을 기다리는 시간이다. 다른 db 가 잠금을 쥔 채 멈춰도 저장이 끝없이 서지 않게 한다.
var LockWait = 2 * time.Second

// Snapshot 은 Write 할 기억의 사본이다. 화면 고루틴에서 만들고 다른 고루틴에서 써도 Store 와 나누는 것이 없다.
type Snapshot struct {
	path, key string
	data      []byte // snapData JSON
}

type snapData struct {
	Actions map[string]Use `json:"actions"`
	Filters []string       `json:"filters"`
	Project *project       `json:"project"`
}

// Snapshot 은 지금 기억을 JSON 으로 떠 둔다.
func (s *Store) Snapshot() (*Snapshot, error) {
	data, err := json.Marshal(snapData{Actions: s.f.Actions, Filters: s.f.Filters, Project: s.p})
	if err != nil {
		return nil, err
	}
	return &Snapshot{path: s.path, key: s.key, data: data}, nil
}

// Save 는 Snapshot 을 바로 쓴다(시험·CLI 처럼 화면이 없을 때).
func (s *Store) Save() error {
	snap, err := s.Snapshot()
	if err != nil {
		return err
	}
	return snap.Write()
}

// Write 는 파일을 다시 읽어 이 프로젝트 항목을 사본 것으로 바꾸고 동작·거르기 기록은 사본 것으로 덮어 쓴다.
// 다른 프로젝트 항목은 지킨다. 같은 폴더 임시 파일(0600)에 다 쓴 뒤 바꿔 끼운다. 폴더는 0700 으로 만든다.
// 다시 읽기부터 바꿔 끼우기까지는 옆 .lock 파일의 flock(LockWait 까지 기다림)으로 프로세스끼리 줄 세워 다른 프로젝트 항목을 덮지 않는다.
// 단, 동시에 켠 다른 db 가 그사이 남긴 동작·거르기 기록은 나중에 쓴 쪽이 이긴다.
func (sn *Snapshot) Write() (err error) {
	var d snapData
	if err := json.Unmarshal(sn.data, &d); err != nil {
		return err
	}
	if err := os.MkdirAll(filepath.Dir(sn.path), 0o700); err != nil {
		return err
	}
	lock, err := atomicfile.TryLock(sn.path, LockWait)
	if err != nil {
		return err
	}
	defer func() { err = errors.Join(err, lock.Close()) }()
	disk, err := readFile(sn.path)
	if err != nil {
		disk = file{} // 깨진 파일은 새로 쓴다
	}
	if disk.Projects == nil {
		disk.Projects = map[string]*project{}
	}
	disk.Actions, disk.Filters = d.Actions, d.Filters
	disk.Projects[sn.key] = d.Project
	data, err := json.Marshal(disk)
	if err != nil {
		return err
	}
	return atomicfile.Write(sn.path, ".recent-*.json", data, 0o600)
}

// ─── 동작·접속 (frecency) ───

// UseAction 은 동작 key 를 한 번 쓴 것으로 센다.
func (s *Store) UseAction(key string, now time.Time) {
	use(s.f.Actions, key, now, maxActions)
}

// TopActions 는 frecency 높은 순으로 n 개 동작 key 다.
func (s *Store) TopActions(now time.Time, n int) []string { return top(s.f.Actions, now, n) }

// UseConn 은 접속 uuid 를 한 번 쓴 것으로 센다.
func (s *Store) UseConn(uuid string, now time.Time) {
	use(s.p.Conns, uuid, now, maxConns)
}

// TopConns 는 frecency 높은 순으로 n 개 접속 uuid 다.
func (s *Store) TopConns(now time.Time, n int) []string { return top(s.p.Conns, now, n) }

func use(m map[string]Use, key string, now time.Time, limit int) {
	if key == "" {
		return
	}
	u := m[key]
	m[key] = Use{N: u.N + 1, At: now}
	if len(m) > limit {
		keep := top(m, now, limit)
		maps.DeleteFunc(m, func(k string, _ Use) bool { return !slices.Contains(keep, k) })
	}
}

// top 은 점수 높은 순, 같으면 최근 순, 그것도 같으면 key 순으로 n 개다.
func top(m map[string]Use, now time.Time, n int) []string {
	keys := slices.Collect(maps.Keys(m))
	slices.SortFunc(keys, func(a, b string) int {
		ua, ub := m[a], m[b]
		return cmp.Or(cmp.Compare(ub.score(now), ua.score(now)), ub.At.Compare(ua.At), strings.Compare(a, b))
	})
	return keys[:min(n, len(keys))]
}

// ─── 테이블 ───

func tableKey(conn, schema, name string) string { return conn + "\x00" + schema + "\x00" + name }

// table 은 기억한 테이블이다. 없으면 만들지 않고 nil 이다.
func (s *Store) table(conn, schema, name string) *Table {
	return s.p.Tables[tableKey(conn, schema, name)]
}

// ensure 는 기억한 테이블을 돌려주고 없으면 만든다. 넘치면 가장 오래 안 연 것부터 버린다.
func (s *Store) ensure(conn, schema, name string, now time.Time) *Table {
	k := tableKey(conn, schema, name)
	if t := s.p.Tables[k]; t != nil {
		return t
	}
	t := &Table{Conn: conn, Schema: schema, Name: name, At: now}
	s.p.Tables[k] = t
	for len(s.p.Tables) > maxTables {
		oldest := ""
		for k, v := range s.p.Tables {
			if o := s.p.Tables[oldest]; oldest == "" || v.At.Before(o.At) || (v.At.Equal(o.At) && k < oldest) {
				oldest = k
			}
		}
		delete(s.p.Tables, oldest)
	}
	return t
}

// TouchTable 은 테이블을 연 때를 남긴다. 접속이나 테이블이 비면 남기지 않는다.
func (s *Store) TouchTable(conn, schema, name string, now time.Time) {
	if conn == "" || name == "" {
		return
	}
	t := s.ensure(conn, schema, name, now)
	t.At = now
}

// Table 은 기억한 테이블의 사본이다.
func (s *Store) Table(conn, schema, name string) (Table, bool) {
	t := s.table(conn, schema, name)
	if t == nil {
		return Table{}, false
	}
	c := *t
	c.Widths, c.Wheres = maps.Clone(t.Widths), slices.Clone(t.Wheres)
	return c, true
}

// RecentTables 는 최근 연 순으로 n 개 테이블이다.
func (s *Store) RecentTables(n int) []Table {
	out := make([]Table, 0, len(s.p.Tables))
	for _, t := range s.p.Tables {
		out = append(out, Table{Conn: t.Conn, Schema: t.Schema, Name: t.Name, At: t.At})
	}
	slices.SortFunc(out, func(a, b Table) int {
		return cmp.Or(b.At.Compare(a.At), strings.Compare(tableKey(a.Conn, a.Schema, a.Name), tableKey(b.Conn, b.Schema, b.Name)))
	})
	return out[:min(n, len(out))]
}

// SetView 는 마지막 정렬·WHERE 를 남기고 바뀌었는지 돌려준다. prod 면 정렬·WHERE·WHERE 기록을 남기지 않고 지운다
// (정렬 글도 식에 값이 들 수 있어 prod 는 열 폭만 남긴다).
func (s *Store) SetView(conn, schema, name, order, where string, prod bool, now time.Time) bool {
	if conn == "" || name == "" || len(order) > maxText || len(where) > maxText {
		return false
	}
	if prod {
		order, where = "", ""
	}
	t := s.table(conn, schema, name)
	if t == nil && order == "" && where == "" {
		return false
	}
	if t == nil {
		t = s.ensure(conn, schema, name, now)
	}
	changed := t.Order != order || t.Where != where || (prod && t.Wheres != nil)
	t.Order, t.Where = order, where
	if prod {
		t.Wheres = nil
	}
	return changed
}

// SetWidth 는 열 폭을 남긴다.
func (s *Store) SetWidth(conn, schema, name, col string, w int, now time.Time) {
	if conn == "" || name == "" || col == "" {
		return
	}
	t := s.ensure(conn, schema, name, now)
	if t.Widths == nil {
		t.Widths = map[string]int{}
	}
	if _, ok := t.Widths[col]; !ok && len(t.Widths) >= maxWidths {
		return
	}
	t.Widths[col] = w
}

// AddWhere 는 WHERE 입력 기록 맨 앞에 where 를 둔다. prod 면 남기지 않는다.
func (s *Store) AddWhere(conn, schema, name, where string, prod bool, now time.Time) {
	where = strings.TrimSpace(where)
	if prod || conn == "" || name == "" || where == "" || len(where) > maxText {
		return
	}
	t := s.ensure(conn, schema, name, now)
	t.Wheres = pushFront(t.Wheres, where, maxWheres)
}

// Wheres 는 테이블의 WHERE 입력 기록, 최근 것부터다. prod 면 비어 있다.
func (s *Store) Wheres(conn, schema, name string, prod bool) []string {
	t := s.table(conn, schema, name)
	if prod || t == nil {
		return nil
	}
	return slices.Clone(t.Wheres)
}

// ─── 거르기 기록 ───

// AddFilter 는 결과 거르기 기록 맨 앞에 f 를 둔다.
func (s *Store) AddFilter(f string) {
	if f == "" || len(f) > maxText {
		return
	}
	s.f.Filters = pushFront(s.f.Filters, f, maxFilters)
}

// Filters 는 결과 거르기 기록, 최근 것부터다.
func (s *Store) Filters() []string { return slices.Clone(s.f.Filters) }

func pushFront(list []string, v string, limit int) []string {
	list = slices.DeleteFunc(slices.Clone(list), func(x string) bool { return x == v })
	list = slices.Insert(list, 0, v)
	return list[:min(len(list), limit)]
}

// ─── 트리 펼침 ───

// Tree 는 펼친 노드 id 와 접은 그룹 id 다.
func (s *Store) Tree() (open, closed []string) {
	return slices.Clone(s.p.Open), slices.Clone(s.p.Closed)
}

// SetTree 는 펼친 노드 id 와 접은 그룹 id 를 바꾸고 바뀌었는지 돌려준다.
func (s *Store) SetTree(open, closed []string) bool {
	open, closed = open[:min(len(open), maxTree)], closed[:min(len(closed), maxTree)]
	if slices.Equal(open, s.p.Open) && slices.Equal(closed, s.p.Closed) {
		return false
	}
	s.p.Open, s.p.Closed = slices.Clone(open), slices.Clone(closed)
	return true
}
