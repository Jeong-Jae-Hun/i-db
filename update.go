package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"runtime"
	"time"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/Jeong-Jae-Hun/i-db/internal/history"
	"github.com/Jeong-Jae-Hun/i-db/internal/selfupdate"
)

// ─── db update (R65) ───

const exitNewer = 10 // update --check 가 새 판을 찾음

const updateUsage = `사용법:
  db update [--check] [--version vX.Y.Z] [--force] [--timeout 2m]

  GitHub 릴리스(` + selfupdate.DefaultRepo + `)의 최신 판(또는 --version 판)을 받아 checksums.txt 로 검증하고
  지금 실행 중인 db 를 바꿔 끼운다. 심볼릭 링크면 가리키는 파일을 바꾼다. GITHUB_TOKEN 이 있으면 API 요청에 붙인다.
  --check 는 바꾸지 않고 새 판 여부만 보인다. 낮은 판으로 내리거나 소스 빌드(dev)를 바꿀 때는 --force 가 필요하다.

종료 코드: 0 성공·이미 최신 · 1 실행 오류 · 2 사용법 · 3 --force 가 필요함 · 10 새 판 있음(--check)
`

// updater 는 db update 가 쓰는 바깥 것이다. 테스트는 가짜 서버·임시 파일로 바꾼다.
type updater struct {
	src           selfupdate.Source
	repo, current string
	goos, goarch  string
	exe           func() (string, error)
}

func defaultUpdater() updater {
	return updater{
		src:     selfupdate.Source{Token: os.Getenv("GITHUB_TOKEN")},
		repo:    selfupdate.DefaultRepo,
		current: version,
		goos:    runtime.GOOS,
		goarch:  runtime.GOARCH,
		exe:     os.Executable,
	}
}

func runUpdateCLI(ctx context.Context, u updater, args []string, stdout, stderr io.Writer) int {
	fs := flag.NewFlagSet("db update", flag.ContinueOnError)
	fs.SetOutput(stderr)
	fs.Usage = func() { fmt.Fprint(stderr, updateUsage) }
	check := fs.Bool("check", false, "바꾸지 않고 새 판 여부만 보임")
	want := fs.String("version", "", "바꿀 판(기본 최신)")
	force := fs.Bool("force", false, "낮은 판·소스 빌드도 바꿈")
	timeout := fs.Duration("timeout", 2*time.Minute, "전체 시간 제한")
	if err := fs.Parse(args); err != nil {
		if errors.Is(err, flag.ErrHelp) {
			return exitOK
		}
		return exitUsage
	}
	if fs.NArg() > 0 {
		fmt.Fprintf(stderr, "db update: 모르는 인자 %q\n", fs.Arg(0))
		return exitUsage
	}
	if *want != "" && !selfupdate.Valid(*want) {
		fmt.Fprintf(stderr, "db update: --version 은 vX.Y.Z 꼴: %q\n", *want)
		return exitUsage
	}
	if u.goos != "linux" && u.goos != "darwin" {
		fmt.Fprintf(stderr, "db update: 이 OS 는 릴리스가 없음: %s (linux·darwin 만)\n", u.goos)
		return exitErr
	}
	ctx, cancel := context.WithTimeout(ctx, *timeout)
	defer cancel()
	rel, err := u.src.Release(ctx, u.repo, *want)
	if err != nil {
		fmt.Fprintln(stderr, "db update:", err)
		return exitErr
	}
	// rel.Tag 는 Release 가 판 꼴만 돌려주므로 비교할 수 없으면 지금 판이 소스 빌드(dev)다
	c, comparable := selfupdate.Compare(rel.Tag, u.current)
	if *check {
		if !comparable {
			fmt.Fprintf(stderr, "db update: 소스 빌드(%s)는 판을 비교할 수 없음\n", u.current)
			return exitErr
		}
		if c > 0 {
			fmt.Fprintf(stdout, "새 판 %s → %s\n", u.current, rel.Tag)
			return exitNewer
		}
		fmt.Fprintf(stdout, "이미 최신 (%s)\n", u.current)
		return exitOK
	}
	switch {
	case *force:
	case !comparable:
		fmt.Fprintf(stderr, "db update: 소스 빌드(%s)는 --force 로 바꿈\n", u.current)
		return exitRefused
	case c == 0 && *want != "":
		fmt.Fprintf(stdout, "이미 %s\n", u.current)
		return exitOK
	case c <= 0 && *want == "":
		fmt.Fprintf(stdout, "이미 최신 (%s)\n", u.current)
		return exitOK
	case c < 0:
		fmt.Fprintf(stderr, "db update: 낮은 판으로 내리기(%s → %s)는 --force\n", u.current, rel.Tag)
		return exitRefused
	}
	exe, err := u.exe()
	if err != nil {
		fmt.Fprintln(stderr, "db update:", err)
		return exitErr
	}
	if err := u.src.Apply(ctx, rel, u.goos, u.goarch, exe); err != nil {
		var nw *selfupdate.NotWritableError
		if errors.As(err, &nw) {
			fmt.Fprintln(stderr, err)
		} else {
			fmt.Fprintln(stderr, "db update:", err)
		}
		return exitErr
	}
	fmt.Fprintf(stdout, "%s → %s\n", u.current, rel.Tag)
	return exitOK
}

// ─── 켤 때 새 판 알림 (R65.2) ───

type updateNoticeMsg struct{ tag string }

// updateCheckPath 는 마지막 확인을 남길 파일이다. 실행 기록과 같은 개인 상태 폴더에 둔다.
func updateCheckPath() string {
	hp, err := history.DefaultPath()
	if err != nil {
		return ""
	}
	return filepath.Join(filepath.Dir(hp), "update-check")
}

func fetchLatestTag(ctx context.Context) (string, error) {
	rel, err := selfupdate.Source{Token: os.Getenv("GITHUB_TOKEN")}.Release(ctx, selfupdate.DefaultRepo, "")
	return rel.Tag, err
}

// updateNoticeCmd 는 하루 한 번 최신 판을 조용히 묻는 명령이다. 끄였거나(DB_UPDATE_CHECK=0) 소스 빌드면 nil 이다.
func updateNoticeCmd(getenv func(string) string, current, path string, fetch func(context.Context) (string, error)) tea.Cmd {
	if getenv("DB_UPDATE_CHECK") == "0" || path == "" || !selfupdate.Valid(current) {
		return nil
	}
	return func() tea.Msg {
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		if tag := selfupdate.Daily(ctx, path, time.Now(), current, fetch); tag != "" {
			return updateNoticeMsg{tag}
		}
		return nil
	}
}
