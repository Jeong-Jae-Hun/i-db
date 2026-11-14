// Package paramstore 는 파라미터 창(:name)에 마지막으로 넣은 값을 프로젝트마다 기억한다(명세 R55).
// 값에 이메일 같은 개인정보가 들 수 있어 세션(R41)처럼 레포가 아닌 $XDG_STATE_HOME/db/params.json(0600)에 두고,
// 프로젝트 절대 경로를 키로 따로 둔다.
package paramstore

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"maps"
	"os"
	"path/filepath"
	"slices"

	"github.com/Jeong-Jae-Hun/i-db/internal/atomicfile"
	"github.com/Jeong-Jae-Hun/i-db/internal/textutil"
)

const (
	// 값 하나의 상한. 넘는 값은 잘라 두면 다른 값이 되어 기억하지 않는다
	maxValue = 4 << 10
	// 프로젝트 하나에 기억할 이름 수
	maxNames = 200
)

// errCorrupt 는 파일을 JSON 으로 읽을 수 없다는 뜻이다. Load·Save 는 그 파일을 params.json.bad 로 옮긴다.
var errCorrupt = errors.New("params file is corrupt")

// Path 는 기억 파일 경로다. session 과 같이 XDG_STATE_HOME 이 절대 경로일 때만 따른다.
func Path() (string, error) {
	if dir := os.Getenv("XDG_STATE_HOME"); filepath.IsAbs(dir) { // XDG 명세는 상대 경로면 무시하라고 한다
		return filepath.Join(dir, "db", "params.json"), nil
	}
	home, err := os.UserHomeDir()
	if err != nil {
		return "", err
	}
	return filepath.Join(home, ".local", "state", "db", "params.json"), nil
}

// badPath 는 깨진 파일을 옮겨 둘 자리다. 이전 것은 덮는다.
func badPath(path string) string { return path + ".bad" }

// Load 는 projectDir 의 이름 → 마지막 값이다. 파일·항목이 없으면 빈 map 이다. 비밀 같은 이름(textutil.LooksSecret)은 예전 파일에 있어도 돌려주지 않는다.
// 깨진 파일은 badPath 로 옮기고 errCorrupt 를 싼 오류를 돌려준다. 다음 Save 는 새 파일에 쓴다.
func Load(path, projectDir string) (map[string]string, error) {
	key, err := filepath.Abs(projectDir)
	if err != nil {
		return nil, err
	}
	all, err := readAll(path)
	if errors.Is(err, errCorrupt) {
		err = errors.Join(err, os.Rename(path, badPath(path)))
	}
	if err != nil {
		return nil, err
	}
	out := all[key]
	if out == nil {
		out = map[string]string{}
	}
	maps.DeleteFunc(out, func(name, _ string) bool { return textutil.LooksSecret(name) })
	return out, nil
}

// Save 는 파일을 다시 읽어 projectDir 항목에 values 를 덮어 합친다. 다른 이름·프로젝트는 지킨다.
// 빈 값은 그 이름을 지우고, 이름이 비었거나 비밀 같은 이름(textutil.LooksSecret)이거나 maxValue 를 넘는 값은 건너뛴다. 합친 이름이 maxNames 를 넘으면
// 이번에 저장한 이름을 먼저 남기고 나머지 기존 이름은 이름순으로 채운다.
// 단, 이름순 채우기는 최근에 쓴 이름이라도 밀려날 수 있다. 자주 넘치면 값마다 쓴 시각을 두고 오래된 것부터 지운다.
// 같은 폴더 임시 파일(0600)에 다 쓴 뒤 바꿔 끼우므로 실패하면 원래 파일이 남는다. 폴더는 0700 으로 만든다.
// 깨진 파일은 badPath 로 옮기고 이 프로젝트 항목만 든 새 파일을 쓴다.
func Save(path, projectDir string, values map[string]string) error {
	key, err := filepath.Abs(projectDir)
	if err != nil {
		return err
	}
	all, err := readAll(path)
	if errors.Is(err, errCorrupt) {
		all, err = nil, os.Rename(path, badPath(path))
	}
	if err != nil {
		return err
	}
	if all == nil {
		all = map[string]map[string]string{}
	}
	cur := maps.Clone(all[key])
	if cur == nil {
		cur = map[string]string{}
	}
	var fresh []string
	for name, v := range values {
		switch {
		case name == "" || len(v) > maxValue || textutil.LooksSecret(name):
		case v == "":
			delete(cur, name)
		default:
			cur[name] = v
			fresh = append(fresh, name)
		}
	}
	if len(cur) > maxNames {
		slices.Sort(fresh)
		keep := append(fresh[:min(len(fresh), maxNames):min(len(fresh), maxNames)], slices.Sorted(maps.Keys(cur))...)
		next := map[string]string{}
		for _, name := range keep {
			if len(next) == maxNames {
				break
			}
			next[name] = cur[name]
		}
		cur = next
	}
	if len(cur) == 0 {
		delete(all, key)
	} else {
		all[key] = cur
	}
	data, err := json.Marshal(all)
	if err != nil {
		return err
	}
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		return err
	}
	return atomicfile.Write(path, ".params-*.json", data, 0o600)
}

// readAll 은 프로젝트 절대 경로 → 이름 → 값이다. 없는 파일·빈 파일은 nil 이다.
func readAll(path string) (map[string]map[string]string, error) {
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
	var all map[string]map[string]string
	if err := json.Unmarshal(data, &all); err != nil {
		return nil, fmt.Errorf("%w: %s: %w", errCorrupt, path, err)
	}
	return all, nil
}
