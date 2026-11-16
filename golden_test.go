package main

import (
	"encoding/json"
	"flag"
	"net"
	"net/url"
	"os"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"
	"testing"
)

// ─── 골든 CLI 출력 ───
//
// testdata/golden/cases.json 의 명령을 돌려 종료 코드·stdout·stderr 를 out/<name>.txt 와 바이트로 비교한다.
// Rust 구현도 같은 cases.json·out/ 을 읽어 같은 결과를 내야 한다. 그래서 형식은 언어와 무관한 글이다.
//
//   - project "offline" 은 닿지 않는 주소만 든 프로젝트다(설정 읽기·사용법·안전장치 거절).
//   - project "live" 는 ${PG_HOST} 같은 자리를 DBTUI_TEST_PG_URL·DBTUI_TEST_REDIS_ADDR 로 채운다. 순서대로 돈다.
//   - DBTUI_PROJECT 는 /golden/project, XDG_STATE_HOME·HOME 은 빈 임시 폴더다.
//   - 인자의 {P} 는 프로젝트를 복사한 임시 폴더다. 출력에서는 다시 {P} 로 바꾼다.
//   - "elapsed_ms":N 은 0 으로 바꾼다. 나머지는 그대로 비교한다.
//
// 기대 출력 갱신: go test -run TestGolden_ -update-golden (live 도 갱신하려면 두 환경 변수를 준다)

var updateGolden = flag.Bool("update-golden", false, "rewrite testdata/golden/out")

type goldenCase struct {
	Name    string   `json:"name"`
	Project string   `json:"project"`
	Args    []string `json:"args"`
	Stdin   string   `json:"stdin,omitempty"`
}

var (
	elapsedRe     = regexp.MustCompile(`"elapsed_ms":\d+`)
	placeholderRe = regexp.MustCompile(`\$\{[A-Z_]+\}`)
)

func TestGolden_골든_케이스의_CLI_출력이_기대_파일과_같다(t *testing.T) {
	raw, err := os.ReadFile(filepath.Join("testdata", "golden", "cases.json"))
	must(t, err)
	var cases []goldenCase
	must(t, json.Unmarshal(raw, &cases))

	vars, live := goldenLiveVars()
	dirs := map[string]string{}
	state := t.TempDir()
	t.Setenv("XDG_STATE_HOME", state)
	t.Setenv("HOME", state)
	// 도움말이 -p 기본값으로 찍는 값이다. 머신마다 달라지지 않게 고정한다
	t.Setenv("DBTUI_PROJECT", "/golden/project")
	seen := map[string]bool{}
	parent := t
	for _, c := range cases {
		if seen[c.Name] {
			t.Fatalf("duplicate case name: %s", c.Name)
		}
		seen[c.Name] = true
		t.Run(c.Name, func(t *testing.T) {
			if c.Project == "live" && !live {
				t.Skip("DBTUI_TEST_PG_URL / DBTUI_TEST_REDIS_ADDR not set")
			}
			dir, ok := dirs[c.Project]
			if !ok {
				// 하위 테스트의 TempDir 은 그 하위 테스트가 끝나면 지워져 바깥 t 로 만든다
				dir = copyGoldenProject(parent, c.Project, vars)
				dirs[c.Project] = dir
			}
			args := make([]string, len(c.Args))
			for i, a := range c.Args {
				args[i] = strings.ReplaceAll(a, "{P}", dir)
			}
			code, out, errw := runCLITest(t, c.Stdin, args...)
			got := goldenText(c, code, out, errw, dir)
			path := filepath.Join("testdata", "golden", "out", c.Name+".txt")
			if *updateGolden {
				must(t, os.WriteFile(path, []byte(got), 0o644))
				return
			}
			want, err := os.ReadFile(path)
			must(t, err)
			if got != string(want) {
				t.Fatalf("golden mismatch (%s)\n--- want\n%s\n--- got\n%s", path, want, got)
			}
		})
	}
}

func goldenText(c goldenCase, code int, out, errw, dir string) string {
	norm := func(s string) string {
		return elapsedRe.ReplaceAllString(strings.ReplaceAll(s, dir, "{P}"), `"elapsed_ms":0`)
	}
	args, _ := json.Marshal(c.Args)
	var b strings.Builder
	b.WriteString("args: " + string(args) + "\n")
	b.WriteString("exit: " + strconv.Itoa(code) + "\n")
	b.WriteString("--- stdout\n" + norm(out))
	b.WriteString("--- stderr\n" + norm(errw))
	return b.String()
}

// goldenLiveVars 는 live 프로젝트의 ${…} 자리 값이다. 두 환경 변수가 다 있어야 live 를 돈다.
func goldenLiveVars() (map[string]string, bool) {
	raw, addr := os.Getenv("DBTUI_TEST_PG_URL"), os.Getenv("DBTUI_TEST_REDIS_ADDR")
	if raw == "" || addr == "" {
		return nil, false
	}
	u, err := url.Parse(raw)
	if err != nil {
		return nil, false
	}
	rhost, rport, err := net.SplitHostPort(addr)
	if err != nil {
		return nil, false
	}
	pw, _ := u.User.Password()
	return map[string]string{
		"PG_HOST": u.Hostname(), "PG_PORT": u.Port(), "PG_DB": strings.TrimPrefix(u.Path, "/"),
		"PG_USER": u.User.Username(), "PG_PASSWORD": url.QueryEscape(pw),
		"REDIS_HOST": rhost, "REDIS_PORT": rport,
	}, true
}

// copyGoldenProject 는 testdata/golden/<name> 을 임시 폴더로 복사하며 ${…} 자리를 vars 로 채운다.
// 명령이 프로젝트에 쓰더라도(즐겨찾기 등) testdata 는 바뀌지 않는다.
func copyGoldenProject(t *testing.T, name string, vars map[string]string) string {
	t.Helper()
	src := filepath.Join("testdata", "golden", name)
	dst := t.TempDir()
	must(t, filepath.WalkDir(src, func(p string, d os.DirEntry, err error) error {
		if err != nil {
			return err
		}
		rel, _ := filepath.Rel(src, p)
		target := filepath.Join(dst, rel)
		if d.IsDir() {
			return os.MkdirAll(target, 0o755)
		}
		b, err := os.ReadFile(p)
		if err != nil {
			return err
		}
		// ${NAME} 꼴만 채운다. DataGrip 의 $PROJECT_DIR$ 는 그대로 둔다
		text := placeholderRe.ReplaceAllStringFunc(string(b), func(m string) string { return vars[m[2:len(m)-1]] })
		return os.WriteFile(target, []byte(text), 0o600)
	}))
	return dst
}
