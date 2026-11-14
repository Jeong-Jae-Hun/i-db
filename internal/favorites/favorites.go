// Package favorites 는 이름 붙인 SQL 을 프로젝트의 .dbtui/favorites.json 에 둔다(R29).
// 파일은 커밋해 여러 기기가 같이 쓰므로, 이름순으로 정렬해 쓰고 두 칸 들여쓴다. 넣은 순서로 쓰면
// 두 기기가 각자 끝에 붙인 변경이 매번 같은 줄에서 충돌하고, 다시 쓸 때마다 순서가 흔들린다.
package favorites

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"time"
	"unicode"
	"unicode/utf8"

	"github.com/Jeong-Jae-Hun/i-db/internal/atomicfile"
)

const (
	dirName     = ".dbtui"
	fileName    = "favorites.json"
	maxNameLen  = 100 // 목록 한 줄에 들어갈 만큼. 룬 수다
	dirPerm     = 0o755
	filePerm    = 0o644 // 새 파일 권한. 커밋하는 파일이라 비밀이 아니다(R29.3)
	tempPattern = ".favorites-*.json"
)

type Fav struct {
	Name  string    `json:"name"`
	Conn  string    `json:"conn"` // 저장 당시 활성 접속 uuid. 없으면 빈 글
	SQL   string    `json:"sql"`
	Saved time.Time `json:"saved"`
}

// Store 는 마지막으로 읽거나 쓴 즐겨찾기를 이름순으로 들고 있다. 메서드는 동시에 불러도 된다.
// Put·Delete 는 파일을 다시 읽어 그 위에 바꾸므로 다른 프로세스가 그사이 저장한 다른 이름은 지키고,
// 같은 이름은 나중에 쓴 쪽이 이긴다. 다시 읽기와 바꿔 끼우기 사이(밀리초)에 다른 프로세스가 쓴 변경은 잃는다.
// 사람이 키를 눌러 저장하는 빈도라 잠금 파일(커밋 폴더에 남는다)을 두지 않았다. 잃으면 안 되게 되면 history 처럼 flock 을 건다.
type Store struct {
	mu   sync.Mutex
	path string
	favs []Fav
}

// Path 는 프로젝트 폴더의 즐겨찾기 파일 경로다.
func Path(projectDir string) string {
	return filepath.Join(projectDir, dirName, fileName)
}

// Load 는 파일이 없으면 빈 Store 를 돌려주고 폴더를 만들지 않는다(첫 저장 때 만든다). 깨진 파일은 오류다.
func Load(projectDir string) (*Store, error) {
	s := &Store{path: Path(projectDir)}
	favs, err := readFile(s.path)
	if err != nil {
		return nil, err
	}
	s.favs = favs
	return s, nil
}

// NormalizeName 은 앞뒤 공백을 떼고 이름으로 쓸 수 있는지 본다. 부르는 쪽이 덮어쓰기를 묻기 전에 같은 규칙으로 고를 수 있게 연다.
func NormalizeName(name string) (string, error) {
	name = strings.TrimSpace(name)
	switch {
	case name == "":
		return "", errors.New("favorite name is empty")
	case utf8.RuneCountInString(name) > maxNameLen:
		return "", fmt.Errorf("favorite name is longer than %d characters", maxNameLen)
	case strings.ContainsFunc(name, unicode.IsControl):
		return "", errors.New("favorite name contains control characters") // 줄바꿈이 들어가면 목록 한 줄이 깨진다
	}
	return name, nil
}

// List 는 이름순 사본이다.
func (s *Store) List() []Fav {
	s.mu.Lock()
	defer s.mu.Unlock()
	return slices.Clone(s.favs)
}

// Get 은 이름의 앞뒤 공백을 떼고 찾는다. 있는지만 볼 때도 이것을 쓴다.
func (s *Store) Get(name string) (Fav, bool) {
	name = strings.TrimSpace(name)
	s.mu.Lock()
	defer s.mu.Unlock()
	i, ok := slices.BinarySearchFunc(s.favs, name, compareName)
	if !ok {
		return Fav{}, false
	}
	return s.favs[i], true
}

// Put 은 같은 이름이 있으면 덮어쓴다. 덮어쓸지 묻는 것은 부르는 쪽 몫이다(R29.1).
// Saved 가 비면 지금 시각을 넣고, 초 아래는 버린다(형식 예시 2026-11-14T20:30:00+09:00 와 맞춘다).
func (s *Store) Put(f Fav) error {
	name, err := NormalizeName(f.Name)
	if err != nil {
		return err
	}
	if strings.TrimSpace(f.SQL) == "" {
		return errors.New("favorite sql is empty")
	}
	f.Name = name
	if f.Saved.IsZero() {
		f.Saved = time.Now()
	}
	f.Saved = f.Saved.Truncate(time.Second)
	s.mu.Lock()
	defer s.mu.Unlock()
	favs, err := readFile(s.path)
	if err != nil {
		return err
	}
	i, ok := slices.BinarySearchFunc(favs, f.Name, compareName)
	if ok {
		favs[i] = f
	} else {
		favs = slices.Insert(favs, i, f)
	}
	return s.writeLocked(favs)
}

// Delete 는 없는 이름이면 파일을 건드리지 않고 nil 이다(다른 프로세스가 먼저 지웠을 수 있다).
func (s *Store) Delete(name string) error {
	name = strings.TrimSpace(name)
	s.mu.Lock()
	defer s.mu.Unlock()
	favs, err := readFile(s.path)
	if err != nil {
		return err
	}
	i, ok := slices.BinarySearchFunc(favs, name, compareName)
	if !ok {
		s.favs = favs
		return nil
	}
	return s.writeLocked(slices.Delete(favs, i, i+1))
}

// writeLocked 는 같은 폴더 임시 파일에 다 쓴 뒤 바꿔 끼운다(R5.2). 실패하면 원본이 그대로 남는다.
func (s *Store) writeLocked(favs []Fav) error {
	var buf bytes.Buffer
	enc := json.NewEncoder(&buf)
	enc.SetEscapeHTML(false) // SQL 의 < > & 가 < 로 바뀌면 git diff 를 읽을 수 없다
	enc.SetIndent("", "  ")
	if favs == nil {
		favs = []Fav{} // 다 지워도 null 이 아니라 [] 로 남긴다
	}
	if err := enc.Encode(favs); err != nil {
		return err
	}
	dir := filepath.Dir(s.path)
	if err := os.MkdirAll(dir, dirPerm); err != nil {
		return err
	}
	// 있던 파일은 그 권한을 따르고(R5.2) 새 파일만 0644 다
	perm := os.FileMode(filePerm)
	if st, err := os.Stat(s.path); err == nil {
		perm = st.Mode().Perm()
	}
	if err := atomicfile.Write(s.path, tempPattern, buf.Bytes(), perm); err != nil {
		return err
	}
	s.favs = favs
	return nil
}

func compareName(f Fav, name string) int {
	return strings.Compare(f.Name, name)
}

// readFile 은 이름순으로 돌려준다. 손으로 고치거나 git 병합으로 같은 이름이 둘 생기면 뒤의 것을 남긴다.
// 없는 파일·빈 파일은 빈 목록이다.
func readFile(path string) ([]Fav, error) {
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
	var raw []Fav
	if err := json.Unmarshal(data, &raw); err != nil {
		// 깨진 파일 위에 새로 쓰면 남은 즐겨찾기를 다 잃으므로 읽기·쓰기 모두 멈춘다
		return nil, fmt.Errorf("read %s: %w", path, err)
	}
	byName := make(map[string]Fav, len(raw))
	for _, f := range raw {
		byName[f.Name] = f
	}
	favs := make([]Fav, 0, len(byName))
	for _, f := range byName {
		favs = append(favs, f)
	}
	slices.SortFunc(favs, func(a, b Fav) int { return strings.Compare(a.Name, b.Name) })
	return favs, nil
}
