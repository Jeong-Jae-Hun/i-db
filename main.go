package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"os"
	"os/signal"
	"path/filepath"
	"runtime"
	"runtime/debug"
	"syscall"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/x/term"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/history"
	"github.com/Jeong-Jae-Hun/i-db/internal/keys"
	"github.com/Jeong-Jae-Hun/i-db/internal/mcpserver"
)

const projectUsage = "DataGrip project directory (.idea 가 있는 폴더). 기본값은 $DBTUI_PROJECT, 없으면 현재 폴더"

// version 은 릴리스 빌드에서 -ldflags "-X main.version=v0.1.0" 으로 넣는다.
// go install …@vX 로 받은 빌드는 ldflags 가 없어 모듈 버전을 대신 쓴다
var version = "dev"

func init() {
	if bi, ok := debug.ReadBuildInfo(); ok && version == "dev" && bi.Main.Version != "" && bi.Main.Version != "(devel)" {
		version = bi.Main.Version
	}
}

type mcpArgs struct {
	dir string
	opt mcpserver.Options
}

func main() {
	if len(os.Args) > 1 && os.Args[1] == "mcp" {
		// MCP 는 stdout 이 프로토콜 통로라 안내·오류는 모두 stderr 로 보낸다
		a, err := parseMCPArgs(os.Args[2:])
		if errors.Is(err, flag.ErrHelp) {
			return
		}
		if err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(2)
		}
		proj, err := loadProject(a.dir)
		if err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
		a.opt.OpenSource = newSource
		ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
		err = mcpserver.Run(ctx, proj, a.opt)
		stop()
		if err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
		return
	}
	if len(os.Args) > 1 && isCLICommand(os.Args[1]) {
		ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
		code := runCLI(ctx, os.Args[1:], os.Stdin, term.IsTerminal(os.Stdin.Fd()), os.Stdout, os.Stderr, term.IsTerminal(os.Stdout.Fd()))
		stop()
		os.Exit(code)
	}

	dir := flag.String("p", defaultProjectDir(), projectUsage)
	showVersion := flag.Bool("version", false, "print version and exit")
	flag.Parse()
	if *showVersion {
		fmt.Println("db", version)
		return
	}

	proj, err := loadProject(*dir)
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	m := newModel(proj)
	// 기록을 못 열어도 조회·실행은 되어야 하므로 상태 줄에만 알린다
	hp, err := history.DefaultPath()
	if err == nil {
		m.hist, err = history.Open(hp)
	}
	if err != nil {
		// 시작 시 건너뛴 접속 경고를 덮지 않게 이어 붙인다
		m.addStatus("실행 기록을 열지 못함: " + err.Error())
	}
	// 결과 사본(R11.4)은 덤이라 열지 못해도 기록은 쓴다
	if m.hist != nil {
		if m.results, err = openResults(hp); err != nil {
			m.addStatus("결과 사본을 열지 못함: " + err.Error())
		} else if m.results != nil {
			m.boot = tea.Batch(m.boot, pruneResultsCmd(m.results))
		}
	}
	m.restoreSession()
	m.tips = loadTips() // 한 번만 보이는 팁(R74.3). 화면에서만 쓰고 CLI·MCP 는 부르지 않는다
	m.boot = tea.Batch(m.boot, updateNoticeCmd(os.Getenv, version, updateCheckPath(), fetchLatestTag))
	// 경계 끌기(R46.1)는 버튼을 누른 채 움직임만 있으면 된다. AllMotion 은 마우스가 지날 때마다 Update·View 를 돌려
	// 경계 위 강조를 빼고 CellMotion 을 쓴다
	opts := []tea.ProgramOption{tea.WithAltScreen(), tea.WithMouseCellMotion()}
	// 맥 터미널은 kitty 키 프로토콜을 켠 앱에만 ⌘ 조합을 넘긴다. 문제가 생기면 DB_KITTY_KEYS=0 으로 끈다.
	// stdin 이 터미널이 아니면 bubbletea 가 /dev/tty 를 따로 열어야 하므로 감싸지 않는다.
	// 감싼 출력은 *os.File 이 아니라서 tea.ExecProcess 를 쓰게 되면 자식 stdout 이 파이프가 된다
	kitty := kittyTerminal(os.Getenv) && term.IsTerminal(os.Stdin.Fd()) && term.IsTerminal(os.Stdout.Fd())
	if kitty {
		opts = append(opts, tea.WithInput(keys.NewTTY(os.Stdin)), tea.WithOutput(keys.NewWriter(os.Stdout)))
	}
	// ⌘ 조합을 받을 수 있으면 키 안내도 ⌘ 로 보인다(R47.1)
	m.mac = keys.MacStyle(kitty)
	p := tea.NewProgram(m, opts...)
	stopHangup := quitOnHangup(p)
	_, err = p.Run()
	stopHangup()
	// SIGTERM·SIGHUP 처럼 키를 거치지 않은 종료도 탭을 저장하고(R41) 진행 중인 작업을 먼저 취소한 뒤 연결을 닫는다
	if serr := m.saveSession(); serr != nil {
		m.saveErr = serr
	}
	m.stop()
	m.clients.Close()
	m.sources.close()
	for _, n := range m.quitNotes {
		fmt.Fprintln(os.Stderr, n)
	}
	if m.saveErr != nil {
		fmt.Fprintln(os.Stderr, "session save failed:", m.saveErr)
	}
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

// quitOnHangup 은 터미널을 닫아 온 SIGHUP 을 SIGTERM 처럼 프로그램 끝내기로 바꾼다. bubbletea 는 SIGINT·SIGTERM 만 받아
// SIGHUP 은 기본 동작(바로 종료)이라 Run 뒤의 세션 저장(R41)을 거치지 못했다. 돌려준 stop 으로 받기를 그만둔다.
// kittyTerminal 은 kitty 키 프로토콜로 입력을 감쌀지다. DB_KITTY_KEYS=1·0 이 먼저다.
// 그 밖에는 맥(⌘ 를 받으려면 필요)과 프로토콜을 아는 터미널만 켠다. Windows Terminal(WT_SESSION)·ConPTY 는
// 마우스 보고를 잘게 나눠 보내 감싼 입력에서 글자로 새어 나온 적이 있어 켜지 않는다
func kittyTerminal(getenv func(string) string) bool {
	switch getenv("DB_KITTY_KEYS") {
	case "1":
		return true
	case "0":
		return false
	}
	if getenv("WT_SESSION") != "" {
		return false
	}
	switch getenv("TERM_PROGRAM") {
	case "ghostty", "WezTerm", "iTerm.app", "kitty":
		return true
	}
	return runtime.GOOS == "darwin" || getenv("TERM") == "xterm-kitty" || getenv("TERM") == "xterm-ghostty"
}

func quitOnHangup(p *tea.Program) (stop func()) {
	// nohup 처럼 SIGHUP 을 무시하고 띄웠으면 Notify 가 무시를 풀어 버리므로 건드리지 않는다
	if signal.Ignored(syscall.SIGHUP) {
		return func() {}
	}
	sig := make(chan os.Signal, 1)
	signal.Notify(sig, syscall.SIGHUP)
	done := make(chan struct{})
	go func() {
		select {
		case <-sig:
			p.Quit()
		case <-done:
		}
	}()
	return func() {
		signal.Stop(sig)
		close(done)
	}
}

// parseMCPArgs 는 `db mcp` 뒤 인자를 읽는다. 사용법·오류 출력은 stderr 로 간다.
func parseMCPArgs(args []string) (mcpArgs, error) {
	fs := flag.NewFlagSet("db mcp", flag.ContinueOnError)
	fs.SetOutput(os.Stderr)
	dir := fs.String("p", defaultProjectDir(), projectUsage)
	allowProd := fs.Bool("allow-prod", false, "allow tools on prod connections (read-only)")
	maxRows := fs.Int("max-rows", 0, "default and maximum rows per result (0 = server default)")
	if err := fs.Parse(args); err != nil {
		return mcpArgs{}, err
	}
	if fs.NArg() > 0 {
		return mcpArgs{}, fmt.Errorf("unexpected arguments: %v", fs.Args())
	}
	if *maxRows < 0 {
		return mcpArgs{}, fmt.Errorf("max-rows must not be negative: %d", *maxRows)
	}
	return mcpArgs{dir: *dir, opt: mcpserver.Options{AllowProd: *allowProd, MaxRows: *maxRows}}, nil
}

func defaultProjectDir() string {
	if d := os.Getenv("DBTUI_PROJECT"); d != "" {
		return d
	}
	return "."
}

func loadProject(dir string) (*db.Project, error) {
	abs, err := filepath.Abs(dir)
	if err != nil {
		return nil, err
	}
	return db.LoadProject(abs)
}
