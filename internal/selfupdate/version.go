package selfupdate

import (
	"cmp"
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"time"

	"github.com/Jeong-Jae-Hun/i-db/internal/atomicfile"
)

// ─── 판 비교 ───

type semver struct {
	core [3]int
	pre  string
}

// parse 는 v1.2.3[-pre][+build] 을 읽는다. v 가 없거나 숫자 셋이 아니면(dev 등) false 다.
func parse(v string) (semver, bool) {
	rest, ok := strings.CutPrefix(v, "v")
	if !ok {
		return semver{}, false
	}
	rest, _, _ = strings.Cut(rest, "+")
	core, pre, _ := strings.Cut(rest, "-")
	parts := strings.Split(core, ".")
	if len(parts) != 3 {
		return semver{}, false
	}
	var s semver
	for i, p := range parts {
		n, err := strconv.Atoi(p)
		if err != nil || n < 0 {
			return semver{}, false
		}
		s.core[i] = n
	}
	s.pre = pre
	return s, true
}

// Valid 는 v 가 비교할 수 있는 판(vX.Y.Z)인지다. 소스 빌드(dev)는 false 다.
func Valid(v string) bool {
	_, ok := parse(v)
	return ok
}

// Compare 는 a 와 b 를 비교한다(-1·0·1). 둘 중 하나라도 판이 아니면 ok 가 false 다.
// 같은 X.Y.Z 면 정식 판이 pre-release(v1.0.0-rc1, go 의사 판 v0.0.0-2026…)보다 높고, pre-release 끼리는 글자로 비교한다.
func Compare(a, b string) (c int, ok bool) {
	x, okx := parse(a)
	y, oky := parse(b)
	if !okx || !oky {
		return 0, false
	}
	for i := range x.core {
		if c := cmp.Compare(x.core[i], y.core[i]); c != 0 {
			return c, true
		}
	}
	switch {
	case x.pre == y.pre:
		return 0, true
	case x.pre == "":
		return 1, true
	case y.pre == "":
		return -1, true
	}
	return cmp.Compare(x.pre, y.pre), true
}

// Newer 는 latest 가 current 보다 높은 판인지다. 비교할 수 없으면 false 다.
func Newer(latest, current string) bool {
	c, ok := Compare(latest, current)
	return ok && c > 0
}

// ─── 하루 한 번 확인 (R65.2) ───

// CheckInterval 은 TUI 가 최신 판을 다시 물을 간격이다.
const CheckInterval = 24 * time.Hour

type checkState struct {
	Checked time.Time `json:"checked"`
	Latest  string    `json:"latest"`
}

// Daily 는 path 에 남긴 마지막 확인이 CheckInterval 안이면 그때 받은 최신 판을, 아니면 fetch 로 새로 물어 path 에 남긴 판을 쓴다.
// 그 판이 current 보다 높으면 돌려주고 아니면 "" 다. fetch·파일 오류는 알리지 않고 "" 다(다음 실행에 다시 묻는다).
func Daily(ctx context.Context, path string, now time.Time, current string, fetch func(context.Context) (string, error)) string {
	var st checkState
	data, err := os.ReadFile(path)
	// 앞으로의 시각은 시계가 틀렸던 것이라 믿지 않는다
	fresh := err == nil && json.Unmarshal(data, &st) == nil && Valid(st.Latest) &&
		!st.Checked.After(now) && now.Sub(st.Checked) < CheckInterval
	if !fresh {
		latest, err := fetch(ctx)
		if err != nil || !Valid(latest) {
			return ""
		}
		st = checkState{Checked: now.UTC().Truncate(time.Second), Latest: latest}
		if b, err := json.Marshal(st); err == nil && os.MkdirAll(filepath.Dir(path), 0o700) == nil {
			_ = atomicfile.Write(path, ".update-check-*", b, 0o600)
		}
	}
	if Newer(st.Latest, current) {
		return st.Latest
	}
	return ""
}
