package db

import (
	"encoding/json"
	"errors"
	"fmt"
	"maps"
	"os"
	"path/filepath"
	"strings"
	"unicode"

	"github.com/Jeong-Jae-Hun/i-db/internal/atomicfile"
)

// ─── 트리 폴더 (R66.2) ───
// 폴더는 보이는 묶음일 뿐이고 Group 은 그대로 둔다. Group 은 .env 의 <GROUP>_AES_KEY 조회(AESKeyIV)에도 쓰여,
// 폴더를 옮길 때 Group 을 바꾸면 enc()·복호화가 조용히 다른 키를 찾게 된다. 그래서 .dbtui/folders.json 에 접속 → 폴더만 둔다.

func foldersPath(dir string) string { return filepath.Join(dir, ".dbtui", "folders.json") }

// folderKey 는 folders.json 의 키다. 이름을 바꿔도 따라가게 uuid 를 쓰고, uuid 가 없을 때만 이름을 쓴다.
func folderKey(c Conn) string {
	if c.UUID != "" {
		return c.UUID
	}
	return "name:" + c.Name
}

// FolderOf 는 트리에 보일 폴더다. 옮긴 폴더가 없으면 그룹(없으면 기타)이다.
func FolderOf(c Conn) string {
	if c.Folder != "" {
		return c.Folder
	}
	return GroupOf(c)
}

func readFolders(dir string) (map[string]string, error) {
	b, err := os.ReadFile(foldersPath(dir))
	if os.IsNotExist(err) {
		return map[string]string{}, nil
	}
	if err != nil {
		return nil, err
	}
	m := map[string]string{}
	if err := json.Unmarshal(b, &m); err != nil {
		return nil, err
	}
	return m, nil
}

// loadFolders 는 접속에 옮긴 폴더를 붙인다. 파일이 깨졌으면 경고만 하고 그룹대로 보인다(R66.3).
func (p *Project) loadFolders() {
	m, err := readFolders(p.Dir)
	if err != nil {
		p.Warnings = append(p.Warnings, fmt.Sprintf("folders.json: %v", err))
		return
	}
	for i, c := range p.Conns {
		f, ok := m[folderKey(c)]
		if !ok {
			continue
		}
		if err := validFolder(f); err != nil {
			p.Warnings = append(p.Warnings, fmt.Sprintf("folders.json: %s: %v", c.Name, err))
			continue
		}
		p.Conns[i].Folder = f
	}
}

func validFolder(f string) error {
	switch {
	case f == "":
		return errors.New("folder name is empty")
	case len([]rune(f)) > 64:
		return errors.New("folder name is longer than 64")
	// 줄 순서를 뒤집어 보이게 하는 방향 문자(Cf)·줄 구분 문자도 막는다
	case strings.IndexFunc(f, func(r rune) bool { return unicode.IsControl(r) || unicode.In(r, unicode.Cf, unicode.Zl, unicode.Zp) }) >= 0:
		return errors.New("folder name has control characters")
	}
	return nil
}

// SetFolder 는 접속들을 folder 로 옮긴다. folder 가 비거나 접속의 원래 그룹과 같으면 옮긴 기록을 지워 그룹으로 되돌린다.
// 지운 접속의 옛 기록은 건드리지 않는다(남아도 맞는 접속이 없어 무시된다).
func SetFolder(dir string, conns []Conn, folder string) error {
	folder = strings.TrimSpace(folder)
	if folder != "" {
		if err := validFolder(folder); err != nil {
			return err
		}
	}
	m, err := readFolders(dir)
	if err != nil {
		return fmt.Errorf("read %s: %w", foldersPath(dir), err) // 깨진 파일을 덮어 다른 기록을 잃지 않게 쓰지 않는다
	}
	old := maps.Clone(m)
	for _, c := range conns {
		if folder == "" || folder == GroupOf(c) {
			delete(m, folderKey(c))
		} else {
			m[folderKey(c)] = folder
		}
	}
	if maps.Equal(old, m) {
		return nil
	}
	b, err := json.MarshalIndent(m, "", "  ")
	if err != nil {
		return err
	}
	if err := os.MkdirAll(filepath.Dir(foldersPath(dir)), 0o755); err != nil {
		return err
	}
	path, perm, err := writeTarget(foldersPath(dir), 0o644)
	if err != nil {
		return err
	}
	return atomicfile.Write(path, ".folders-*.json", append(b, '\n'), perm)
}
