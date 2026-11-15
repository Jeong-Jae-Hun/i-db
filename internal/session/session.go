// Package session 은 끝낼 때의 탭(제목·편집기 글·커서·연 파일·접속)을 개인 상태 폴더에 두고 다음에 되살린다(R41).
// 편집기 글에 개인정보가 들 수 있어 레포가 아닌 $XDG_STATE_HOME/db/session.json 한 파일에 두고,
// 프로젝트마다 절대 경로를 키로 따로 둔다.
package session

import (
	"bytes"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"time"

	"github.com/Jeong-Jae-Hun/i-db/internal/atomicfile"
)

// 프로젝트 하나의 편집기 글 상한. 큰 덤프를 붙여 둔 탭 몇 개로 끝낼 때마다 수십 MB 를 다시 쓰지 않게 한다.
// 넘으면 활성 탭부터 차례로 넣고 들어가지 않는 탭은 글을 비우고 Dropped 로 표시한다. 되살릴 때 그 수를 알린다.
// 연 파일이 있으면 파일에서 다시 읽고, 없으면 빈 탭이다. 앞부분만 잘라 두면 실행할 수 있는 반쪽 SQL 이 남아 자르지 않는다.
const maxProjectText = 1 << 20

// ErrCorrupt 는 세션 파일을 JSON 으로 읽을 수 없다는 뜻이다. Load·Save 는 그 파일을 session.json.bad 로 옮긴다.
var ErrCorrupt = errors.New("session file is corrupt")

type Tab struct {
	Title string `json:"title"`
	// 편집기 글. 연 파일이 있으면 저장 안 한 고침이 있을 때만(디스크 내용과 다를 때만) 둔다
	Text string `json:"text,omitempty"`
	File string `json:"file,omitempty"`
	// Text 를 둔 파일 탭의, 저장할 때 디스크 내용 해시(DiskHash). 되살릴 때 파일이 그사이 바뀌었는지 가른다
	Disk    string `json:"disk,omitempty"`
	Conn    string `json:"conn,omitempty"` // 접속 uuid
	Cursor  int    `json:"cursor,omitempty"`
	Dropped bool   `json:"dropped,omitempty"` // 상한에 걸려 글을 비웠다
	// 사용자가 바꾼 탭 이름과 고정(R57). 이름이 비면 Title·파일 이름을 보인다
	Name   string `json:"name,omitempty"`
	Pinned bool   `json:"pinned,omitempty"`
}

// DiskHash 는 Tab.Disk 에 둘 파일 내용 해시다.
func DiskHash(data []byte) string {
	sum := sha256.Sum256(data)
	return hex.EncodeToString(sum[:])
}

// BadPath 는 깨진 세션 파일을 옮겨 둘 자리다. 이전 것은 덮는다.
func BadPath(path string) string { return path + ".bad" }

// maxRecent 는 접속마다 남길 최근 파일 수다. 자동 열기(R44)는 앞에서부터 지운 파일을 건너뛰며 고른다
const maxRecent = 20

type State struct {
	Active int   `json:"active"`
	Tabs   []Tab `json:"tabs"`
	// 접속 uuid → 그 접속에서 연·실행한·저장한 파일, 최근 것부터(R44)
	Recent map[string][]Recent `json:"recent,omitempty"`
	Panes  Panes               `json:"panes"`
	// 최근 연 테이블 보기, 최근 것부터(R40.3). 명령 팔레트가 맨 위에 보인다
	RecentTables []RecentTable `json:"recentTables,omitempty"`
}

// maxRecentTables 는 남길 최근 테이블 수다(R40.3)
const maxRecentTables = 50

type RecentTable struct {
	Conn   string    `json:"conn"` // 접속 uuid
	Schema string    `json:"schema"`
	Table  string    `json:"table"`
	At     time.Time `json:"at"`
}

type Recent struct {
	File string    `json:"file"`
	At   time.Time `json:"at"`
}

// Panes 는 마우스로 바꾼 창 크기다(R46). 0 이면 기본 크기다
type Panes struct {
	Tree   int `json:"tree,omitempty"`
	Editor int `json:"editor,omitempty"`
}

// Touch 는 conn 의 최근 파일 맨 앞에 file 을 둔다. 같은 파일의 이전 기록은 지우고 maxRecent 개만 남긴다.
// 접속이나 파일이 비면 남기지 않는다.
func (s *State) Touch(conn, file string, at time.Time) {
	if conn == "" || file == "" {
		return
	}
	if s.Recent == nil {
		s.Recent = map[string][]Recent{}
	}
	list := slices.DeleteFunc(slices.Clone(s.Recent[conn]), func(r Recent) bool { return r.File == file })
	list = slices.Insert(list, 0, Recent{File: file, At: at})
	s.Recent[conn] = list[:min(len(list), maxRecent)]
}

// TouchTable 은 최근 테이블 맨 앞에 conn 의 schema.table 을 둔다. 같은 테이블의 이전 기록은 지우고 maxRecentTables 개만 남긴다.
// 접속이나 테이블이 비면 남기지 않는다.
func (s *State) TouchTable(conn, schema, table string, at time.Time) {
	if conn == "" || table == "" {
		return
	}
	list := slices.DeleteFunc(slices.Clone(s.RecentTables), func(r RecentTable) bool {
		return r.Conn == conn && r.Schema == schema && r.Table == table
	})
	list = slices.Insert(list, 0, RecentTable{Conn: conn, Schema: schema, Table: table, At: at})
	s.RecentTables = list[:min(len(list), maxRecentTables)]
}

// Path 는 세션 파일 경로다. history 와 같이 XDG_STATE_HOME 이 절대 경로일 때만 따른다.
func Path() (string, error) {
	if dir := os.Getenv("XDG_STATE_HOME"); filepath.IsAbs(dir) { // XDG 명세는 상대 경로면 무시하라고 한다
		return filepath.Join(dir, "db", "session.json"), nil
	}
	home, err := os.UserHomeDir()
	if err != nil {
		return "", err
	}
	return filepath.Join(home, ".local", "state", "db", "session.json"), nil
}

// Load 는 projectDir 의 세션을 읽는다. 파일이나 그 프로젝트 항목이 없으면 false 다.
// 깨진 파일은 BadPath 로 옮기고 ErrCorrupt 를 싼 오류를 돌려준다. 다음 Save 는 새 파일에 쓴다.
// 활성 탭 번호가 탭 범위 밖이면 0 으로 맞춘다.
func Load(path, projectDir string) (State, bool, error) {
	key, err := filepath.Abs(projectDir)
	if err != nil {
		return State{}, false, err
	}
	all, err := readAll(path)
	if errors.Is(err, ErrCorrupt) {
		err = errors.Join(err, os.Rename(path, BadPath(path)))
	}
	if err != nil {
		return State{}, false, err
	}
	st, ok := all[key]
	if !ok {
		return State{}, false, nil
	}
	if st.Active < 0 || st.Active >= len(st.Tabs) {
		st.Active = 0
	}
	return st, true, nil
}

// Save 는 파일을 다시 읽어 projectDir 항목만 바꿔 쓴다. 다른 프로젝트 항목은 지킨다.
// 같은 폴더 임시 파일(0600)에 다 쓴 뒤 바꿔 끼우므로 실패하면 원래 파일이 남는다. 폴더는 0700 으로 만든다.
// 깨진 파일은 BadPath 로 옮겨 두고(다른 프로젝트 세션을 손으로 살릴 수 있게) 이 프로젝트 항목만 든 새 파일을 쓴다.
// 단, 다시 읽기와 바꿔 끼우기 사이(밀리초)에 다른 db 프로세스가 저장한 다른 프로젝트 항목은 잃는다.
// 끝낼 때 한 번 쓰는 파일이라 잠금을 두지 않았다.
func Save(path, projectDir string, st State) error {
	key, err := filepath.Abs(projectDir)
	if err != nil {
		return err
	}
	all, err := readAll(path)
	if errors.Is(err, ErrCorrupt) {
		all, err = nil, os.Rename(path, BadPath(path))
	}
	if err != nil {
		return err
	}
	if all == nil {
		all = map[string]State{}
	}
	st.Tabs = slices.Clone(st.Tabs)
	order := make([]int, 0, len(st.Tabs))
	if st.Active >= 0 && st.Active < len(st.Tabs) {
		order = append(order, st.Active)
	}
	for i := range st.Tabs {
		if i != st.Active {
			order = append(order, i)
		}
	}
	budget := maxProjectText
	for _, i := range order {
		if n := len(st.Tabs[i].Text); n <= budget {
			budget -= n
			continue
		}
		st.Tabs[i].Text, st.Tabs[i].Disk, st.Tabs[i].Cursor, st.Tabs[i].Dropped = "", "", 0, true
	}
	all[key] = st
	data, err := json.Marshal(all)
	if err != nil {
		return err
	}
	dir := filepath.Dir(path)
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return err
	}
	return atomicfile.Write(path, ".session-*.json", data, 0o600)
}

// readAll 은 프로젝트 절대 경로 → 세션이다. 없는 파일·빈 파일은 nil 이다.
func readAll(path string) (map[string]State, error) {
	data, err := os.ReadFile(path)
	if errors.Is(err, os.ErrNotExist) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	if len(bytes.TrimSpace(data)) == 0 {
		return nil, nil
	}
	var all map[string]State
	if err := json.Unmarshal(data, &all); err != nil {
		return nil, fmt.Errorf("%w: %s: %w", ErrCorrupt, path, err)
	}
	return all, nil
}
