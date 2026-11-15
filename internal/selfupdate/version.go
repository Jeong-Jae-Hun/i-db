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

const identChars = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz-"

func digits(s string) bool { return s != "" && strings.Trim(s, "0123456789") == "" }

// validIdents 는 점으로 나눈 식별자가 모두 비지 않은 [0-9A-Za-z-] 인지다. pre-release 의 숫자 식별자는 0 으로 시작하면 안 된다.
func validIdents(s string, pre bool) bool {
	for _, id := range strings.Split(s, ".") {
		if id == "" || strings.Trim(id, identChars) != "" || pre && len(id) > 1 && id[0] == '0' && digits(id) {
			return false
		}
	}
	return true
}

// parse 는 v1.2.3[-pre][+build] 을 semver 대로 읽는다. v 가 없거나 숫자 셋이 아니거나(dev 등) 0 으로 시작하는 숫자·허용하지 않는 글자가 있으면 false 다.
func parse(v string) (semver, bool) {
	rest, ok := strings.CutPrefix(v, "v")
	if !ok {
		return semver{}, false
	}
	rest, build, hasBuild := strings.Cut(rest, "+")
	core, pre, hasPre := strings.Cut(rest, "-")
	if hasBuild && !validIdents(build, false) || hasPre && !validIdents(pre, true) {
		return semver{}, false
	}
	parts := strings.Split(core, ".")
	if len(parts) != 3 {
		return semver{}, false
	}
	var s semver
	for i, p := range parts {
		if !digits(p) || len(p) > 1 && p[0] == '0' {
			return semver{}, false
		}
		n, err := strconv.Atoi(p)
		if err != nil {
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
// 같은 X.Y.Z 면 정식 판이 pre-release(v1.0.0-rc.1, go 의사 판 v0.0.0-2026…)보다 높다. pre-release 는 semver 대로
// 점으로 나눈 식별자를 앞에서부터 비교한다: 숫자끼리는 수로, 숫자는 글자보다 낮고, 앞이 같으면 짧은 쪽이 낮다.
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
	xs, ys := strings.Split(x.pre, "."), strings.Split(y.pre, ".")
	for i := range min(len(xs), len(ys)) {
		xn, yn := digits(xs[i]), digits(ys[i])
		switch {
		case xn && yn:
			// 0 으로 시작하지 않으므로 길이가 길면 큰 수다(int 넘침 없이)
			c = cmp.Or(cmp.Compare(len(xs[i]), len(ys[i])), strings.Compare(xs[i], ys[i]))
		case xn:
			c = -1
		case yn:
			c = 1
		default:
			c = strings.Compare(xs[i], ys[i])
		}
		if c != 0 {
			return c, true
		}
	}
	return cmp.Compare(len(xs), len(ys)), true
}

// newer 는 latest 가 current 보다 높은 판인지다. 비교할 수 없으면 false 다.
func newer(latest, current string) bool {
	c, ok := Compare(latest, current)
	return ok && c > 0
}

// ─── 하루 한 번 확인 (R65.2) ───

// checkInterval 은 TUI 가 최신 판을 다시 물을 간격이다.
const checkInterval = 24 * time.Hour

type checkState struct {
	Checked time.Time `json:"checked"`
	Latest  string    `json:"latest"`
}

// Daily 는 path 에 남긴 마지막 확인이 checkInterval 안이면 그때 받은 최신 판을, 아니면 fetch 로 새로 물어 path 에 남긴 판을 쓴다.
// 그 판이 current 보다 높으면 돌려주고 아니면 "" 다. fetch·파일 오류는 알리지 않고 "" 다(다음 실행에 다시 묻는다).
func Daily(ctx context.Context, path string, now time.Time, current string, fetch func(context.Context) (string, error)) string {
	var st checkState
	data, err := os.ReadFile(path)
	// 앞으로의 시각은 시계가 틀렸던 것이라 믿지 않는다
	fresh := err == nil && json.Unmarshal(data, &st) == nil && Valid(st.Latest) &&
		!st.Checked.After(now) && now.Sub(st.Checked) < checkInterval
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
	if newer(st.Latest, current) {
		return st.Latest
	}
	return ""
}
