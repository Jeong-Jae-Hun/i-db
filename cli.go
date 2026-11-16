package main

import (
	"cmp"
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"slices"
	"strconv"
	"strings"
	"time"

	"github.com/charmbracelet/x/ansi"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/grid"
	"github.com/Jeong-Jae-Hun/i-db/internal/params"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
	"github.com/Jeong-Jae-Hun/i-db/internal/statcrypt"
	"github.com/Jeong-Jae-Hun/i-db/internal/textutil"
)

// ─── CLI 하위 명령 (R58) ───
// 화면 없이 한 번 실행하고 끝난다. 읽기는 늘 서버 READ ONLY 트랜잭션이고, 실행 기록에는 남기지 않는다.

const (
	exitOK      = 0
	exitErr     = 1
	exitUsage   = 2
	exitRefused = 3

	cliDefaultRows = 200
	cliMaxRows     = 10000
	// table 형식에서 칸 하나가 차지하는 최대 폭. 긴 JSON 한 칸이 줄 전체를 밀어내지 않게 자른다
	cliCellWidth = 60
	// 표준 입력으로 받는 SQL·명령의 상한. 실수로 덤프를 흘려 넣어도 메모리를 다 쓰지 않게 한다
	cliMaxStdin = 16 << 20
)

var cliFormats = []string{"table", "tsv", "csv", "json"}

// cliCommand 는 명령 하나다. usage 는 `db help <명령>`·`db <명령> -h` 첫 줄이다.
type cliCommand struct{ name, usage, desc string }

// cliCommands 는 도움말 순서다.
var cliCommands = []cliCommand{
	{"conns", "db conns [-p 경로]", "접속 목록(이름·그룹·종류·환경·호스트·DB, 비밀번호 없음)"},
	{"schemas", "db schemas -c 접속", "스키마 목록"},
	{"tables", "db tables -c 접속 [-s 스키마]", "테이블·뷰 목록(기본 스키마 public). 저장소 접속은 맨 위 목록(인덱스·라벨·지표·테이블)"},
	{"describe", "db describe -c 접속 [-s 스키마] 테이블", "열·제약·인덱스(tsv·csv 에서 제약·인덱스는 stderr). 테이블은 스키마.테이블 로도 쓴다"},
	{"ddl", "db ddl -c 접속 [-s 스키마] 테이블", "CREATE 문"},
	{"query", "db query -c 접속 [SQL | -f 파일 | < 표준입력] [--param 이름=값] [--write [--force]]", "SQL 한 문장 실행(기본 READ ONLY). 저장소 접속은 그 질의(OpenSearch·LogQL·PromQL·PartiQL)를 보내고 비싼 질의는 --force"},
	{"logs", "db logs -c Loki접속 'LogQL' [--since 1h] [--until 0] [--force]", "Loki 로그 질의(query 와 같고 범위 플래그를 받음, Grafana 접속도)"},
	{"metrics", "db metrics -c Prometheus접속 'PromQL' [--since 1h] [--until 0] [--force]", "Prometheus 지표 질의(Grafana 접속도)"},
	{"explain", "db explain -c 접속 [SQL | -f 파일 | < 표준입력]", "실행 계획(ANALYZE 없음)"},
	{"redis", "db redis -c 접속 명령 [인자…] [--write [--force]]", "Redis 명령 하나(기본 읽기 명령만). - 로 시작하는 인자는 -- 뒤에 쓴다"},
	{"user360", "db user360 --prod 이메일|uid|id|닉네임", "사용자 한눈에 보기(R51). prod 를 조회해 --prod 가 필요하다"},
	{"search", "db search -c 접속 [-s 스키마] 찾을글", "스키마의 글자 열 전체 검색(테이블당 5행)"},
	{"linked", "db linked -c 접속 --column 열 --value 값 [--table 스키마.테이블]", "칸 값이 든 다른 테이블을 이름 규칙으로 찾아 셈(R73). 같은 그룹·환경의 PostgreSQL 접속도, 실패가 있으면 1"},
	{"schema-diff", "db schema-diff -c 접속A --to 접속B [-s 스키마] [--to-schema 스키마2]", "두 스키마의 테이블·열·제약·인덱스 비교"},
	{"status", "db status [-c 접속 …] [--format table|tsv|json] [--prod]", "모든 접속(또는 -c 로 고른 접속)을 동시에 확인해 상태·지연·버전·주요 수치를 찍음(R64.3). prod 는 --prod 일 때만, 하나라도 실패하면 1"},
	{"flows", "db flows [-f 흐름 …] [--format table|tsv|json] [--prod]", "이벤트 흐름(.dbtui/flows.json: 아웃박스 → SNS → SQS → DLQ·Lambda)을 한 번 풀어 상자마다 한 줄을 찍음(R69). prod 는 --prod 일 때만, 설정 오류·실패가 있으면 1"},
	{"board", "db board [이름] [--format table|tsv|json] [--prod]", "쿼리 보드(.dbtui/boards/<이름>.json)의 패널을 한 번 돌려 패널마다 한 줄을 찍음(R70). 이름이 없으면 보드 목록, prod 패널은 --prod 일 때만, 실패한 패널이 있으면 1"},
	{"history", "db history [--conn 접속] [--since 24h] [--format table|tsv|json] · db history show <id>", "실행 기록 목록과 결과 사본(R11.5). db help history"},
	{"update", "db update [--check] [--version vX.Y.Z] [--force]", "GitHub 릴리스의 최신 판으로 db 를 바꿈(R65). --check 는 새 판이 있으면 10. db help update"},
	{"conn", "db conn add|edit|rm|test …", "db 전용 접속 추가·고치기·지우기·시험(R59, .dbtui/connections.json). db help conn"},
}

// cliCmdFlags 는 일부 명령만 받는 플래그와 그 명령들이다. 다른 명령에는 플래그를 정의하지 않아
// 주면 사용법 오류(2)이고 도움말에도 나오지 않는다. 여기 없는 플래그(-p·-c·--format·--prod 등)는 모든 명령이 받는다.
var cliCmdFlags = map[string][]string{
	"write":   {"query", "redis"},
	"force":   {"query", "redis", "logs", "metrics"},
	"since":   {"query", "logs", "metrics"}, // --until 도 같다
	"decrypt": {"query", "user360", "search"},
	"param":   {"query", "explain"},
	"f":       {"query", "explain", "redis", "logs", "metrics"},
	"s":       {"tables", "describe", "ddl", "search", "schema-diff"},
	"to":      {"schema-diff"}, // --to-schema 도 같다
	"column":  {"linked"},      // --value·--table 도 같다
}

func cmdTakes(cmd, flagName string) bool {
	cmds, ok := cliCmdFlags[flagName]
	return !ok || slices.Contains(cmds, cmd)
}

// cliWriteVerbs 는 query --write 가 실행하는 문장의 첫 단어다. select·with 는 R25 밖의 숨은 쓰기(select into·CTE 쓰기)를
// --force 로 받으려고 넣는다. COPY(서버 파일·프로그램), DO·CALL(임의 코드), SET·BEGIN 같은 세션 명령은 받지 않는다.
var cliWriteVerbs = []string{
	"insert", "update", "delete", "merge", "create", "alter", "drop", "truncate", "comment", "grant", "revoke",
	"refresh", "vacuum", "analyze", "reindex", "cluster", "select", "with",
}

// findCommand 는 명령 이름의 cliCommands 인덱스다. 없으면 -1 이다.
func findCommand(name string) int {
	return slices.IndexFunc(cliCommands, func(c cliCommand) bool { return c.name == name })
}

func isCLICommand(name string) bool { return name == "help" || findCommand(name) >= 0 }

type cliOpts struct {
	cmd                         string
	dir, conn, format, file     string
	schema, to, toSchema        string
	column, value, table        string // linked(R73)
	maxRows                     int
	maxRowsSet                  bool // --max-rows 를 줬다. 저장소 기본 1,000행(R62.2)과 가른다
	timeout                     time.Duration
	since, until                durFlag // Loki·Prometheus 범위(R62.2)
	prod, write, force, decrypt bool
	params                      paramFlag
	conns                       namesFlag // status 의 -c, flows 의 -f(여러 번)
	args                        []string
}

// namesFlag 는 반복되는 이름이다(status 의 -c, flows 의 -f).
type namesFlag []string

func (n *namesFlag) String() string { return strings.Join(*n, ",") }

func (n *namesFlag) Set(s string) error {
	*n = append(*n, s)
	return nil
}

// paramFlag 는 반복되는 --param 이름=값 이다.
type paramFlag map[string]string

func (p paramFlag) String() string { return "" }

func (p paramFlag) Set(s string) error {
	k, v, ok := strings.Cut(s, "=")
	if !ok || k == "" {
		return fmt.Errorf("--param must be name=value: %q", s)
	}
	p[k] = v
	return nil
}

// durFlag 는 --since·--until 값이다. Go 기간 꼴(1h·15m)에 더해 날짜(7d)를 받는다.
// 날짜는 maxFlagDays(10년)까지다. 더 크면 time.Duration(약 292년)을 넘쳐 음수·작은 범위로 바뀔 수 있다.
type durFlag time.Duration

const maxFlagDays = 3650

func (d *durFlag) String() string { return time.Duration(*d).String() }

func (d *durFlag) Set(s string) error {
	if n, ok := strings.CutSuffix(s, "d"); ok {
		days, err := strconv.Atoi(n)
		if err != nil || days < 0 || days > maxFlagDays {
			return fmt.Errorf("invalid duration %q (days 0..%d)", s, maxFlagDays)
		}
		*d = durFlag(time.Duration(days) * 24 * time.Hour)
		return nil
	}
	v, err := time.ParseDuration(s)
	if err != nil || v < 0 {
		return fmt.Errorf("invalid duration %q", s)
	}
	*d = durFlag(v)
	return nil
}

// cliError 는 종료 코드를 정한 오류다. 그 밖의 오류는 실행 오류(1)다.
type cliError struct {
	code int
	msg  string
}

func (e *cliError) Error() string { return e.msg }

func refused(msg string) error { return &cliError{exitRefused, msg} }

func usageErr(format string, a ...any) error { return &cliError{exitUsage, fmt.Sprintf(format, a...)} }

// cliResult 는 출력할 결과 하나다. user360 처럼 여러 개면 title 로 나눈다. decryptFailed 는 --decrypt 로 풀지 못한 칸 수다.
// failed 는 결과는 찍되 일부가 실패해 1 로 끝낼 때다(linked 의 접속·테이블 실패).
type cliResult struct {
	title         string
	conn          db.Conn
	res           *db.Result
	err           error
	decryptFailed int
	failed        bool
}

// runCLI 는 `db <명령> …` 을 실행하고 종료 코드를 돌려준다. stdin 이 nil 이면(터미널) 표준 입력에서 SQL 을 읽지 않는다.
// stdinTTY 면 표준 입력이 터미널이다. conn 은 rm 확인·--password-stdin 에 그대로 읽고(R59.4),
// 나머지 명령은 SQL 을 기다리며 멈추지 않게 읽지 않는다. tty 는 표준 출력이 터미널인지다(기본 출력 형식).
func runCLI(ctx context.Context, args []string, stdin io.Reader, stdinTTY bool, stdout, stderr io.Writer, tty bool) (code int) {
	// 패닉 값과 스택에는 SQL·서버 응답이 섞여 비밀번호가 나올 수 있어 찍지 않는다
	defer func() {
		if recover() != nil {
			fmt.Fprintln(stderr, "internal error")
			code = exitErr
		}
	}()
	cmd := args[0]
	switch cmd {
	case "help":
		return cliHelp(args[1:], stdout, stderr)
	case "conn":
		return runConnCLI(ctx, args[1:], stdin, stdinTTY, stdout, stderr)
	case "update":
		return runUpdateCLI(ctx, defaultUpdater(), args[1:], stdout, stderr)
	case "history":
		return runHistoryCLI(args[1:], stdout, stderr, tty)
	}
	if stdinTTY {
		stdin = nil
	}
	o, err := parseCLI(cmd, args[1:], tty, stderr)
	switch {
	case errors.Is(err, flag.ErrHelp):
		return exitOK
	case err != nil:
		// flag 가 이미 오류와 사용법을 찍은 경우가 아니면 여기서 찍는다
		var ce *cliError
		if errors.As(err, &ce) {
			fmt.Fprintf(stderr, "db %s: %s\n", cmd, ce.msg)
		}
		return exitUsage
	}
	proj, err := loadProject(o.dir)
	if err != nil {
		fmt.Fprintln(stderr, err)
		return exitErr
	}
	// 저장소 결과 한도는 따로 주지 않으면 1,000행이다(R62.2)
	if i := connByName(proj, o.conn); i >= 0 && proj.Conns[i].Kind.IsSource() && !o.maxRowsSet {
		o.maxRows = sourceDefaultRows
	}
	ctx, cancel := context.WithTimeout(ctx, o.timeout)
	defer cancel()
	clients := db.NewClients()
	defer clients.Close()
	switch o.cmd {
	case "status":
		return runStatus(ctx, o, proj, clients, stdout, stderr)
	case "flows":
		return runFlows(ctx, o, proj, clients, stdout, stderr)
	case "board":
		return runBoard(ctx, o, proj, clients, stdout, stderr)
	}

	start := time.Now()
	results, err := runCommand(ctx, o, proj, clients, stdin, stderr)
	elapsed := time.Since(start)
	if err == nil && o.decrypt {
		err = decryptResults(proj, results, stderr)
	}
	if err != nil {
		fmt.Fprintln(stderr, cliErrText(ctx, proj, o, err, o.conn, o.to))
		var ce *cliError
		if errors.As(err, &ce) {
			return ce.code
		}
		return exitErr
	}
	failed := false
	for i := range results {
		r := &results[i]
		// --decrypt 로 풀지 못한 칸이 있으면 결과는 찍되 실패로 끝낸다
		failed = failed || r.decryptFailed > 0 || r.failed
		if r.err != nil {
			failed = true
			r.err = errors.New(cliErrText(ctx, proj, o, r.err, r.conn.Name))
			fmt.Fprintf(stderr, "%s: %s\n", r.title, r.err)
			continue
		}
		if r.res.Elapsed == 0 {
			r.res.Elapsed = elapsed
		}
		if len(r.res.Rows) > o.maxRows {
			r.res.Rows, r.res.Truncated = r.res.Rows[:o.maxRows], true
		}
	}
	if err := writeResults(stdout, stderr, o, results); err != nil {
		fmt.Fprintln(stderr, err)
		return exitErr
	}
	if failed {
		return exitErr
	}
	return exitOK
}

// cliErrText 는 오류를 찍을 글로 만든다. 서버가 비밀번호·키를 되돌려줘도 나오지 않게 conns(접속 이름)의 비밀번호와
// 모든 AES 키·IV 를 지운다. 비밀번호를 모든 접속 것으로 넓히지 않는 까닭: test 같은 짧은 로컬 비밀번호가 접속 이름까지 가린다.
func cliErrText(ctx context.Context, proj *db.Project, o cliOpts, err error, conns ...string) string {
	msg := err.Error()
	if errors.Is(err, context.DeadlineExceeded) || errors.Is(ctx.Err(), context.DeadlineExceeded) {
		msg = fmt.Sprintf("시간 초과(--timeout %s): %s", o.timeout, msg)
	}
	for _, c := range proj.Conns {
		if slices.Contains(conns, c.Name) {
			msg = db.Redact(msg, c.Password)
		}
	}
	for _, v := range proj.AESEnv {
		msg = db.Redact(msg, v)
	}
	return textutil.Printable(msg, true)
}

func cliHelp(args []string, stdout, stderr io.Writer) int {
	if len(args) > 0 && args[0] == "conn" {
		fmt.Fprint(stdout, connUsage)
		return exitOK
	}
	if len(args) > 0 && args[0] == "update" {
		fmt.Fprint(stdout, updateUsage)
		return exitOK
	}
	if len(args) > 0 && args[0] == "history" {
		fmt.Fprint(stdout, historyUsage)
		return exitOK
	}
	if len(args) > 0 {
		if findCommand(args[0]) < 0 {
			fmt.Fprintf(stderr, "알 수 없는 명령: %s\n", args[0])
			return exitUsage
		}
		fs, _ := newCLIFlags(args[0], stdout)
		fs.Usage()
		return exitOK
	}
	fmt.Fprintln(stdout, "사용법: db                 화면(TUI) 실행")
	fmt.Fprintln(stdout, "        db mcp             읽기 전용 MCP 서버")
	fmt.Fprintln(stdout, "        db <명령> [플래그]  화면 없이 실행하고 끝냄")
	fmt.Fprintln(stdout, "\n명령:")
	for _, c := range cliCommands {
		fmt.Fprintf(stdout, "  %-12s %s\n", c.name, c.desc)
	}
	fmt.Fprintln(stdout, "\n공통 플래그: -p 경로 -c 접속 --format table|tsv|csv|json --max-rows N --timeout 30s --prod --decrypt")
	fmt.Fprintln(stdout, "종료 코드: 0 성공 · 1 실행 오류 · 2 사용법 · 3 안전장치 거절")
	fmt.Fprintln(stdout, "명령별 사용법: db help <명령> 또는 db <명령> -h")
	return exitOK
}

func newCLIFlags(cmd string, out io.Writer) (*flag.FlagSet, *cliOpts) {
	fs := flag.NewFlagSet("db "+cmd, flag.ContinueOnError)
	fs.SetOutput(out)
	o := &cliOpts{cmd: cmd, params: paramFlag{}}
	fs.StringVar(&o.dir, "p", defaultProjectDir(), projectUsage)
	if cmd == "status" {
		fs.Var(&o.conns, "c", "확인할 접속 이름(여러 번, 없으면 모두)")
	} else {
		fs.StringVar(&o.conn, "c", "", "접속 이름(db conns)")
	}
	fs.StringVar(&o.format, "format", "", "출력 형식 table|tsv|csv|json (기본: 터미널이면 table, 아니면 tsv)")
	fs.IntVar(&o.maxRows, "max-rows", cliDefaultRows, fmt.Sprintf("최대 행 수(상한 %d)", cliMaxRows))
	fs.DurationVar(&o.timeout, "timeout", 30*time.Second, "전체 시간 제한")
	fs.BoolVar(&o.prod, "prod", false, "prod 접속 허용(읽기만)")
	if cmdTakes(cmd, "write") {
		fs.BoolVar(&o.write, "write", false, "쓰기 허용(prod 는 거절)")
	}
	if cmdTakes(cmd, "force") {
		fs.BoolVar(&o.force, "force", false, "--write 에서 위험 문장(R25)·숨은 쓰기 허용, 저장소는 비싼 질의 허용(R60.2)")
	}
	if cmdTakes(cmd, "since") {
		fs.Var(&o.since, "since", "Loki·Prometheus 범위 시작(지금부터 거슬러, 1h·15m·7d, 기본 1h)")
		fs.Var(&o.until, "until", "범위 끝(지금부터 거슬러, 기본 0)")
	}
	if cmdTakes(cmd, "decrypt") {
		fs.BoolVar(&o.decrypt, "decrypt", false, "staticEncrypt 칸을 접속 그룹 키로 풀어 찍기")
	}
	if cmdTakes(cmd, "param") {
		fs.Var(o.params, "param", "이름=값, SQL 의 :이름 에 바인딩(여러 번)")
	}
	if cmdTakes(cmd, "f") {
		fs.StringVar(&o.file, "f", "", "SQL·명령을 읽을 파일")
	}
	if cmd == "flows" {
		fs.Var(&o.conns, "f", "풀 흐름 이름(여러 번, 없으면 모두)")
	}
	if cmdTakes(cmd, "s") {
		fs.StringVar(&o.schema, "s", "", "스키마")
	}
	if cmdTakes(cmd, "to") {
		fs.StringVar(&o.to, "to", "", "비교할 접속")
		fs.StringVar(&o.toSchema, "to-schema", "", "비교할 스키마(기본: -s 와 같음)")
	}
	if cmdTakes(cmd, "column") {
		fs.StringVar(&o.column, "column", "", "칸의 열 이름(user_id 면 *_user_id·users.id 도 찾음)")
		fs.StringVar(&o.value, "value", "", "찾을 값(DB 값 그대로)")
		fs.StringVar(&o.table, "table", "", "칸의 원본 테이블(열이 id 일 때 이름 뿌리를 뽑음)")
	}
	i := findCommand(cmd)
	fs.Usage = func() {
		fmt.Fprintf(out, "사용법: %s\n  %s\n\n플래그:\n", cliCommands[i].usage, cliCommands[i].desc)
		fs.PrintDefaults()
	}
	return fs, o
}

// parseInterleaved 는 플래그와 위치 인자를 섞어 쓸 수 있게 읽고 위치 인자를 돌려준다(`db query -c x "select 1" --format json`).
// `--` 뒤는 모두 위치 인자다(Redis 의 -1 같은 인자).
func parseInterleaved(fs *flag.FlagSet, args []string) ([]string, error) {
	var pos []string
	for {
		if err := fs.Parse(args); err != nil {
			return pos, err
		}
		rest := fs.Args()
		if len(rest) == 0 {
			return pos, nil
		}
		if len(args) > len(rest) && args[len(args)-len(rest)-1] == "--" {
			return append(pos, rest...), nil
		}
		pos, args = append(pos, rest[0]), rest[1:]
	}
}

// parseCLI 는 명령 플래그를 읽고 기본값·범위를 맞춘다.
func parseCLI(cmd string, args []string, tty bool, stderr io.Writer) (cliOpts, error) {
	fs, o := newCLIFlags(cmd, stderr)
	var err error
	if o.args, err = parseInterleaved(fs, args); err != nil {
		return *o, err
	}
	fs.Visit(func(f *flag.Flag) { o.maxRowsSet = o.maxRowsSet || f.Name == "max-rows" })
	switch {
	case o.format == "" && tty:
		o.format = "table"
	case o.format == "":
		o.format = "tsv"
	case !slices.Contains(cliFormats, o.format):
		return *o, usageErr("--format must be one of %s", strings.Join(cliFormats, "|"))
	}
	if o.maxRows < 1 {
		return *o, usageErr("--max-rows must be at least 1")
	}
	o.maxRows = min(o.maxRows, cliMaxRows)
	if o.timeout <= 0 {
		return *o, usageErr("--timeout must be positive")
	}
	return *o, nil
}

// pickConn 은 -c(또는 --to) 이름의 접속을 찾고 prod 허용을 확인한다. 접속을 열기 전에 거절해야 prod 에 연결 자체를 하지 않는다.
func pickConn(proj *db.Project, o cliOpts, flagName, name string, kind db.Kind) (db.Conn, error) {
	if name == "" {
		return db.Conn{}, usageErr("%s <접속 이름> 이 필요함 (db conns)", flagName)
	}
	i := connByName(proj, name)
	switch {
	case i < 0:
		return db.Conn{}, usageErr("접속 없음: %s (db conns)", name)
	case proj.Conns[i].Kind != kind && proj.Conns[i].Kind.IsSource():
		return db.Conn{}, usageErr("%s(%s): %s", name, proj.Conns[i].Kind, msgUnsupported)
	case proj.Conns[i].Kind != kind:
		return db.Conn{}, usageErr("%s 는 %s 접속이 아님", name, kind)
	case proj.Conns[i].Env() == "prod" && !o.prod:
		return db.Conn{}, refused("prod 접속은 --prod 가 필요함")
	}
	return proj.Conns[i], nil
}

// readInput 은 위치 인자, -f 파일, 표준 입력 순으로 SQL·명령 글을 읽는다.
// 표준 입력은 닫히지 않는 파이프에서도 --timeout 에 끝나게 따로 읽고, cliMaxStdin 을 넘으면 오류다.
func readInput(ctx context.Context, o cliOpts, stdin io.Reader) (string, error) {
	var text string
	switch {
	case len(o.args) > 0:
		text = strings.Join(o.args, " ")
	case o.file != "":
		b, err := os.ReadFile(o.file)
		if err != nil {
			return "", err
		}
		text = string(b)
	case stdin != nil:
		type read struct {
			b   []byte
			err error
		}
		// 시간 초과로 돌아가도 읽는 고루틴은 남는다. CLI 는 곧 끝나므로 두고, 버퍼 1칸이라 보낼 때 막히지 않는다
		ch := make(chan read, 1)
		go func() {
			b, err := io.ReadAll(io.LimitReader(stdin, cliMaxStdin+1))
			ch <- read{b, err}
		}()
		select {
		case <-ctx.Done():
			return "", fmt.Errorf("표준 입력 읽기: %w", ctx.Err())
		case r := <-ch:
			switch {
			case r.err != nil:
				return "", r.err
			case len(r.b) > cliMaxStdin:
				return "", errors.New("표준 입력이 16MiB 를 넘음 (-f 파일로도 같은 문장 하나만 실행함)")
			}
			text = string(r.b)
		}
	}
	if strings.TrimSpace(text) == "" {
		return "", usageErr("실행할 글이 없음 (인자, -f 파일, 표준 입력)")
	}
	return text, nil
}

// pgStatement 는 입력에서 문장 하나를 골라 enc() 를 확장하고(R52) :이름 을 $n 으로 바꾼 글과 인자를 돌려준다.
func pgStatement(ctx context.Context, o cliOpts, proj *db.Project, c db.Conn, stdin io.Reader, stderr io.Writer) (string, []any, error) {
	text, err := readInput(ctx, o, stdin)
	if err != nil {
		return "", nil, err
	}
	stmts := db.SplitStatements(text)
	if len(stmts) != 1 {
		return "", nil, usageErr("문장 하나만 실행함 (%d개)", len(stmts))
	}
	sent, note, err := expandEnc(proj, c, stmts[0])
	if err != nil {
		return "", nil, usageErr("%s", err)
	}
	if note != "" {
		fmt.Fprintln(stderr, note)
	}
	sql, names := params.Bind(sent)
	args := make([]any, len(names))
	for i, n := range names {
		v, ok := o.params[n]
		if !ok {
			return "", nil, usageErr("파라미터 값이 없음: %s (--param %s=값)", n, n)
		}
		args[i] = v
	}
	for k := range o.params {
		if !slices.Contains(names, k) {
			return "", nil, usageErr("문장에 없는 파라미터: %s", k)
		}
	}
	return sql, args, nil
}

// tableRef 는 describe·ddl 의 대상이다. -s 가 없으면 스키마.테이블 꼴을 나누고, 그것도 없으면 public 이다.
func tableRef(o cliOpts) (db.TableRef, error) {
	if len(o.args) != 1 {
		return db.TableRef{}, usageErr("테이블 이름 하나가 필요함")
	}
	if o.schema != "" {
		return db.TableRef{Schema: o.schema, Table: o.args[0]}, nil
	}
	if s, t, ok := strings.Cut(o.args[0], "."); ok {
		return db.TableRef{Schema: s, Table: t}, nil
	}
	return db.TableRef{Schema: "public", Table: o.args[0]}, nil
}

func namesResult(col string, names []string) *db.Result {
	res := &db.Result{Cols: []string{col}}
	for _, n := range names {
		res.Rows = append(res.Rows, []string{n})
	}
	return res
}

func runCommand(ctx context.Context, o cliOpts, proj *db.Project, clients *db.Clients, stdin io.Reader, stderr io.Writer) ([]cliResult, error) {
	if i := connByName(proj, o.conn); i >= 0 && proj.Conns[i].Kind.IsSource() && o.cmd != "conns" && o.cmd != "user360" {
		return runSourceCommand(ctx, o, proj, proj.Conns[i].Kind, stdin)
	}
	// --force 는 PostgreSQL·Redis 에서는 --write 와 함께만 쓴다(R58.1). 저장소는 비싼 질의 허용이라 따로 받는다
	if o.force && !o.write {
		return nil, usageErr("--force 는 --write 와 함께 씀")
	}
	if o.cmd == "logs" || o.cmd == "metrics" {
		return nil, usageErr("%s 는 %s 접속에서만 됨 (db conns)", o.cmd, map[string]string{"logs": "Loki·Grafana", "metrics": "Prometheus·Grafana"}[o.cmd])
	}
	switch o.cmd {
	case "conns":
		// 사람이 읽는 표는 db 전용 접속 이름에 (db) 를 붙인다(R59.1). 기계가 읽는 형식은 -c 에 그대로 넣게 이름만 두고 source 열로 가른다
		res := &db.Result{Cols: []string{"name", "group", "kind", "env", "host", "db", "source", "folder"}}
		for _, c := range proj.Conns {
			name, source := c.Name, "datagrip"
			if c.Source == db.SourceDB {
				source = db.SourceDB
			}
			if o.format == "table" {
				name = connLabel(c)
			}
			res.Rows = append(res.Rows, []string{name, c.Group, string(c.Kind), c.Env(), cmp.Or(c.Host, c.URL), c.DB, source, db.FolderOf(c)})
		}
		return []cliResult{{res: res}}, nil

	case "user360":
		if !o.prod {
			return nil, refused("user360 은 prod 를 조회해 --prod 가 필요함")
		}
		if len(o.args) == 0 {
			return nil, usageErr("찾을 이메일·uid·id·닉네임이 필요함")
		}
		b, err := planUserLookup(proj, strings.Join(o.args, " "))
		if err != nil {
			return nil, err
		}
		subs, errs := runAll(ctx, clients, b.conns, b.sqls, b.errs)
		out := make([]cliResult, len(subs))
		for i := range subs {
			out[i] = cliResult{title: b.titles[i], conn: b.conns[i], res: subs[i], err: errs[i]}
		}
		return out, nil

	case "redis":
		c, err := pickConn(proj, o, "-c", o.conn, db.KindRedis)
		if err != nil {
			return nil, err
		}
		args := o.args
		if len(args) <= 1 {
			text, err := readInput(ctx, o, stdin)
			if err != nil {
				return nil, err
			}
			args = db.SplitArgs(strings.TrimSpace(text))
		}
		switch {
		case len(args) > 0 && strings.EqualFold(args[0], "KEYS"):
			// KEYS 는 키 전체를 한 번에 훑어 그동안 서버가 다른 요청을 받지 못한다
			pat := "…"
			if len(args) > 1 {
				pat = args[1]
			}
			return nil, refused(fmt.Sprintf("KEYS 는 CLI 에서 하지 않음 (서버를 멈춤): SCAN 0 MATCH %s COUNT 1000 을 쓰세요", pat))
		case !o.write && !db.IsReadOnlyRedis(args):
			return nil, refused("읽기 명령이 아님 (쓰기는 --write)")
		case o.write && c.Env() == "prod":
			return nil, refused("prod 쓰기는 CLI 에서 하지 않음 (화면에서 R6·R54 를 거침)")
		case o.write && !o.force && db.DangerOfRedis(args) != "":
			return nil, refused("위험: " + db.DangerOfRedis(args) + " (--force)")
		}
		r, err := clients.Redis(ctx, c)
		if err != nil {
			return nil, err
		}
		res, err := r.Run(ctx, args)
		return []cliResult{{conn: c, res: res}}, err

	case "linked":
		// 접속을 열기 전에 prod(--prod)·사용법을 거른다. 다른 접속은 환경이 같을 때만 들어 prod 는 --prod 아래에서만 닿는다
		c, err := pickConn(proj, o, "-c", o.conn, db.KindPG)
		if err != nil {
			return nil, err
		}
		if o.column == "" || o.value == "" {
			return nil, usageErr("--column 과 --value 가 필요함")
		}
		_, table, _ := strings.Cut(o.table, ".")
		base := db.LinkedBase(o.column, cmp.Or(table, o.table))
		if strings.EqualFold(o.column, "id") && base == "" {
			return nil, usageErr("id 열은 --table 이 있어야 찾음")
		}
		idx := linkedConns(proj, connByName(proj, c.Name))
		conns := make([]db.Conn, len(idx))
		for i, ci := range idx {
			conns[i] = proj.Conns[ci]
		}
		found := findLinked(ctx, clients, conns, o.column, base, o.value)
		res, _, summary, failed := linkedTable(conns, found, func(i int, err error) string { return cliErrText(ctx, proj, o, err, conns[i].Name) })
		res.Text = summary
		return []cliResult{{conn: c, res: res, failed: failed}}, nil

	case "schema-diff":
		a, err := pickConn(proj, o, "-c", o.conn, db.KindPG)
		if err != nil {
			return nil, err
		}
		b, err := pickConn(proj, o, "--to", o.to, db.KindPG)
		if err != nil {
			return nil, err
		}
		schema := cmp.Or(o.schema, "public")
		sa, err := schemaSnap(ctx, clients, a, schema)
		if err != nil {
			return nil, fmt.Errorf("%s: %w", a.Name, err)
		}
		sb, err := schemaSnap(ctx, clients, b, cmp.Or(o.toSchema, schema))
		if err != nil {
			return nil, fmt.Errorf("%s: %w", b.Name, err)
		}
		return []cliResult{{conn: a, res: db.DiffSnapshots(sa, sb)}}, nil
	}

	// 나머지는 PostgreSQL 접속 하나에서 돈다
	c, err := pickConn(proj, o, "-c", o.conn, db.KindPG)
	if err != nil {
		return nil, err
	}
	var sql string
	var args []any
	var table db.TableRef
	switch o.cmd {
	case "describe", "ddl":
		if table, err = tableRef(o); err != nil {
			return nil, err
		}
	case "search":
		if len(o.args) == 0 {
			return nil, usageErr("찾을 글이 필요함")
		}
	case "query":
		if sql, args, err = pgStatement(ctx, o, proj, c, stdin, stderr); err != nil {
			return nil, err
		}
		if o.write {
			verb := db.FirstSQLWord(sql)
			danger := strings.Join(slices.DeleteFunc([]string{db.DangerOf(sql), db.HiddenWriteOf(sql)}, func(s string) bool { return s == "" }), ", ")
			switch {
			case c.Env() == "prod":
				return nil, refused("prod 쓰기는 CLI 에서 하지 않음 (화면에서 R6·R54 를 거침)")
			case !slices.Contains(cliWriteVerbs, verb):
				return nil, refused("--write 로 실행하지 않는 문장: " + strings.ToUpper(verb))
			case !o.force && danger != "":
				return nil, refused("위험: " + danger + " (--force)")
			}
		} else if err := db.CheckReadOnlySQL(sql); err != nil {
			return nil, refused("읽기 문장이 아님 (쓰기는 --write): " + err.Error())
		}
	case "explain":
		if sql, args, err = pgStatement(ctx, o, proj, c, stdin, stderr); err != nil {
			return nil, err
		}
		// 직접 쓴 EXPLAIN 은 감싸지 않으므로 ANALYZE 는 여기서 거절한다(문장이 실제로 실행된다)
		sql = db.ExplainSQL(sql, db.ExplainOpt{})
		if err := db.CheckReadOnlySQL(sql); err != nil {
			return nil, refused("EXPLAIN ANALYZE·쓰기 문장 EXPLAIN 은 실행하지 않음: " + err.Error())
		}
	}

	pg, err := clients.PG(ctx, c)
	if err != nil {
		return nil, err
	}
	var res *db.Result
	switch o.cmd {
	case "schemas":
		var names []string
		names, err = pg.FindSchemas(ctx)
		res = namesResult("schema", names)
	case "tables":
		var names []string
		names, err = pg.FindTables(ctx, cmp.Or(o.schema, "public"))
		res = namesResult("table", names)
	case "describe":
		res, err = pg.DescribeTable(ctx, table)
	case "ddl":
		var ddl string
		ddl, err = pg.TableDDL(ctx, table)
		res = &db.Result{Text: ddl}
	case "search":
		var skipped []string
		res, skipped, err = pg.SearchData(ctx, o.schema, strings.Join(o.args, " "))
		if len(skipped) > 0 {
			// 테이블 이름과 함께 오는 오류 글도 서버 응답이라 가리고 제어 문자를 바꾼다
			fmt.Fprintln(stderr, cliErrText(ctx, proj, o, errors.New("건너뛴 테이블: "+strings.Join(skipped, ", ")), o.conn))
		}
	case "query", "explain":
		// explain 은 --write 를 받지 않지만 실행 경로도 명령으로 갈라 늘 READ ONLY 로 돈다
		if o.cmd == "query" && o.write {
			res, err = pg.RunArgs(ctx, db.Query{SQL: sql, Args: args})
		} else {
			// RunReadOnly 는 바인딩을 받지 않아 값을 리터럴로 넣는다(R51 과 같은 방식)
			res, err = pg.RunReadOnlyRows(ctx, inlineArgs(sql, args), o.maxRows)
		}
	}
	if err != nil {
		return nil, err
	}
	return []cliResult{{conn: c, res: res}}, nil
}

// decryptResults 는 --decrypt 일 때 staticEncrypt 꼴 칸을 그 접속 그룹 키로 푼다(R37). 풀지 못한 칸은 그대로 둔다.
func decryptResults(proj *db.Project, results []cliResult, stderr io.Writer) error {
	for i := range results {
		r := &results[i]
		if r.err != nil || r.res == nil {
			continue
		}
		if r.conn.Group == "" {
			return errors.New("접속 그룹을 몰라 복호화 키를 고를 수 없음")
		}
		key, _ := proj.AESKeyIV(r.conn)
		if key == "" {
			return errors.New(db.EnvName(r.conn.Group) + "_AES_KEY 가 .env 에 없음")
		}
		failed := 0
		for _, row := range r.res.Rows {
			for j, v := range row {
				if !statcrypt.LooksEncrypted(v) {
					continue
				}
				if plain, err := statcrypt.Decrypt(v, key); err == nil {
					row[j] = plain
				} else {
					failed++
				}
			}
		}
		r.decryptFailed = failed
		if failed > 0 {
			fmt.Fprintf(stderr, "%d칸 풀지 못함\n", failed)
		}
	}
	return nil
}

// ─── 출력 ───

type jsonTable struct {
	Title     string      `json:"title,omitempty"`
	Conn      string      `json:"conn"`
	Cols      []string    `json:"cols"`
	Types     []string    `json:"types"`
	Rows      [][]*string `json:"rows"`
	Text      string      `json:"text,omitempty"` // describe 의 제약·인덱스처럼 표에 딸린 글
	Truncated bool        `json:"truncated"`
	// CellsTruncated 는 MaxCellBytes 에서 잘린 칸 수, DecryptFailed 는 --decrypt 로 풀지 못한 칸 수다
	CellsTruncated int   `json:"cells_truncated,omitempty"`
	DecryptFailed  int   `json:"decrypt_failed,omitempty"`
	ElapsedMS      int64 `json:"elapsed_ms"`
}

type jsonText struct {
	Title     string `json:"title,omitempty"`
	Conn      string `json:"conn"`
	Text      string `json:"text"`
	ElapsedMS int64  `json:"elapsed_ms"`
}

type jsonError struct {
	Title string `json:"title,omitempty"`
	Conn  string `json:"conn"`
	Error string `json:"error"`
}

// jsonOf 는 결과 하나의 JSON 값이다. PostgreSQL 결과(Types 가 있음)의 "NULL" 칸은 null 이다(grid.ToJSON 과 같은 한계).
func jsonOf(r cliResult) any {
	if r.err != nil {
		return jsonError{Title: r.title, Conn: r.conn.Name, Error: r.err.Error()}
	}
	res := r.res
	ms := res.Elapsed.Milliseconds()
	if len(res.Cols) == 0 {
		return jsonText{Title: r.title, Conn: r.conn.Name, Text: res.Text, ElapsedMS: ms}
	}
	out := jsonTable{Title: r.title, Conn: r.conn.Name, Cols: res.Cols, Types: res.Types, Rows: [][]*string{},
		Text: res.Text, Truncated: res.Truncated, CellsTruncated: cappedCells(res), DecryptFailed: r.decryptFailed, ElapsedMS: ms}
	if out.Types == nil {
		out.Types = []string{}
	}
	for _, row := range res.Rows {
		cells := make([]*string, len(row))
		for i := range row {
			if res.Types == nil || row[i] != "NULL" {
				cells[i] = &row[i]
			}
		}
		out.Rows = append(out.Rows, cells)
	}
	return out
}

// cappedCells 는 결과에서 MaxCellBytes 에서 잘린 칸 수다.
func cappedCells(res *db.Result) int {
	n := 0
	for _, row := range res.Rows {
		for _, v := range row {
			if db.IsCappedCell(v) {
				n++
			}
		}
	}
	return n
}

func writeResults(w, stderr io.Writer, o cliOpts, results []cliResult) error {
	multi := o.cmd == "user360"
	// 잘린 칸은 형식과 상관없이 stderr 로 알린다. 칸 끝의 …(N바이트) 를 값으로 오해하지 않게 한다
	for _, r := range results {
		if r.err != nil || r.res == nil {
			continue
		}
		if n := cappedCells(r.res); n > 0 {
			prefix := ""
			if r.title != "" {
				prefix = textutil.Printable(r.title, false) + ": "
			}
			fmt.Fprintf(stderr, "%s칸 %d개가 64KiB 에서 잘림\n", prefix, n)
		}
	}
	if o.format == "json" {
		enc := json.NewEncoder(w)
		enc.SetEscapeHTML(false)
		if !multi {
			return enc.Encode(jsonOf(results[0]))
		}
		all := make([]any, len(results))
		for i, r := range results {
			all[i] = jsonOf(r)
		}
		return enc.Encode(all)
	}
	var b strings.Builder
	for i, r := range results {
		if multi {
			if i > 0 {
				b.WriteByte('\n')
			}
			fmt.Fprintf(&b, "== %s (%s) ==\n", textutil.Printable(r.title, false), r.conn.Name)
		}
		if r.err != nil {
			b.WriteString("오류: " + r.err.Error() + "\n")
			continue
		}
		res := r.res
		if len(res.Cols) == 0 {
			text := res.Text
			if o.format == "table" {
				text = textutil.Printable(text, true)
			}
			b.WriteString(strings.TrimRight(text, "\n") + "\n")
			continue
		}
		switch o.format {
		case "table":
			writeTable(&b, res, o.write)
		case "csv":
			b.WriteString(grid.ToCSV(res))
		case "tsv":
			b.WriteString(grid.RowTSV(res.Cols) + "\n")
			for _, row := range res.Rows {
				b.WriteString(grid.RowTSV(row) + "\n")
			}
		}
		if o.format != "table" {
			// 표에 딸린 글은 tsv·csv 를 깨지 않게 stderr 로 보낸다
			if res.Text != "" {
				fmt.Fprintln(stderr, res.Text)
			}
			if res.Truncated {
				fmt.Fprintf(stderr, "결과가 잘림: %d행까지 (%s)\n", len(res.Rows), truncHint(o.write))
			}
		}
	}
	_, err := io.WriteString(w, b.String())
	return err
}

// writeTable 은 열을 맞춘 글 표를 쓴다. 칸의 제어 문자는 보이는 글자로 바꾸고(터미널 제어 차단),
// 폭은 한글 같은 넓은 글자를 2칸으로 센다. 숫자 열은 오른쪽에 붙인다.
func writeTable(b *strings.Builder, res *db.Result, write bool) {
	widths := make([]int, len(res.Cols))
	head := make([]string, len(res.Cols))
	for i, c := range res.Cols {
		head[i] = tableCell(c)
		widths[i] = ansi.StringWidth(head[i])
	}
	rows := make([][]string, len(res.Rows))
	for r, row := range res.Rows {
		rows[r] = make([]string, len(res.Cols))
		for i := range res.Cols {
			if i < len(row) {
				rows[r][i] = tableCell(row[i])
			}
			widths[i] = max(widths[i], ansi.StringWidth(rows[r][i]))
		}
	}
	writeTableLine(b, head, widths, res.Types)
	sep := make([]string, len(widths))
	for i, w := range widths {
		sep[i] = strings.Repeat("-", w)
	}
	writeTableLine(b, sep, widths, res.Types)
	for _, row := range rows {
		writeTableLine(b, row, widths, res.Types)
	}
	fmt.Fprintf(b, "(%d행", len(res.Rows))
	if res.Truncated {
		b.WriteString(", 잘림 — " + truncHint(write))
	}
	b.WriteString(")\n")
	if res.Text != "" {
		b.WriteString(textutil.Printable(res.Text, true) + "\n")
	}
}

// truncHint 는 결과가 잘린 까닭이다. --write 는 RunArgs 가 db.MaxRows(1,000)행에서 자르고 --max-rows 로 늘지 않는다.
func truncHint(write bool) string {
	if write {
		return "--write 는 1,000행 상한"
	}
	return "--max-rows 로 늘림"
}

// tableCell 은 표 칸 하나의 보일 글이다. 제어 문자를 바꾼 뒤 cliCellWidth 칸에서 자른다.
func tableCell(s string) string {
	return ansi.Truncate(textutil.Printable(s, false), cliCellWidth, "…")
}

// writeTableLine 은 칸들을 widths 에 맞춰 한 줄로 쓴다. 숫자 타입 열은 오른쪽에 붙이고 마지막 칸 뒤에는 공백을 두지 않는다.
func writeTableLine(b *strings.Builder, cells []string, widths []int, types []string) {
	for i, v := range cells {
		if i > 0 {
			b.WriteString("  ")
		}
		pad := strings.Repeat(" ", widths[i]-ansi.StringWidth(v))
		switch {
		case i < len(types) && db.IsNumericType(types[i]):
			b.WriteString(pad + v)
		case i == len(cells)-1:
			b.WriteString(v)
		default:
			b.WriteString(v + pad)
		}
	}
	b.WriteByte('\n')
}

// ─── 읽기 전용 저장소 (R60~R63) ───

// sourceDefaultRows 는 저장소 질의의 기본 한도다(R62.2). --max-rows 를 주면 그 값이다.
const sourceDefaultRows = 1000

// sourceCmdKinds 는 logs·metrics 가 받는 접속 종류다.
var sourceCmdKinds = map[string][]db.Kind{
	"logs":    {db.KindLoki, db.KindGrafana},
	"metrics": {db.KindPrometheus, db.KindGrafana},
}

// runSourceCommand 는 저장소 접속의 query·logs·metrics·tables 를 Backend 로 돌린다. 다른 명령은 사용법 오류다.
// prod 는 읽기도 --prod 가 필요하고(R58.2), 쓰기로 보이는 질의(ErrWrite)·--force 없는 비싼 질의(R60.2)·
// DB_ALLOW_BILLED 없는 요금 조회(ErrBilled, R72)는 3이다.
func runSourceCommand(ctx context.Context, o cliOpts, proj *db.Project, kind db.Kind, stdin io.Reader) ([]cliResult, error) {
	switch o.cmd {
	case "query", "tables":
		if o.cmd == "query" && kind == db.KindAWS {
			return nil, usageErr("%s (db tables -c %s)", msgTreeOnly, o.conn)
		}
	case "logs", "metrics":
		if !slices.Contains(sourceCmdKinds[o.cmd], kind) {
			return nil, usageErr("%s 는 %s 접속이 아님 (%s)", o.conn, o.cmd, kind)
		}
	default:
		return nil, usageErr("%s(%s): %s", o.cmd, kind, msgUnsupported)
	}
	switch {
	case o.write:
		return nil, usageErr("읽기 전용 저장소라 --write 를 받지 않음")
	case len(o.params) > 0:
		return nil, usageErr("--param 은 %s", msgUnsupported)
	}
	c, err := pickConn(proj, o, "-c", o.conn, kind)
	if err != nil {
		return nil, err
	}
	var q source.Query
	if o.cmd != "tables" {
		text, err := readInput(ctx, o, stdin)
		if err != nil {
			return nil, err
		}
		q = source.Query{Text: strings.TrimSpace(text), Since: time.Duration(o.since), Until: time.Duration(o.until), Limit: o.maxRows, Force: o.force}
	}
	b, err := newSource(ctx, c)
	if err != nil {
		return nil, err
	}
	defer b.Close()
	var res *db.Result
	if o.cmd == "tables" {
		nodes, err := b.Children(ctx, "")
		if errors.Is(err, source.ErrBilled) {
			return nil, refused(err.Error())
		}
		if err != nil {
			return nil, err
		}
		res = &db.Result{Cols: []string{"name", "info"}}
		for _, n := range nodes {
			res.Rows = append(res.Rows, []string{n.Name, n.Info})
		}
	} else if res, err = b.Run(ctx, q); err != nil {
		switch {
		case errors.Is(err, source.ErrWrite):
			return nil, refused(msgReadOnlySource)
		case errors.Is(err, source.ErrBilled):
			return nil, refused(err.Error())
		case errors.Is(err, source.ErrNeedsConfirm):
			return nil, refused(sourceErrText(err) + " (--force)")
		}
		return nil, err
	}
	return []cliResult{{conn: c, res: res}}, nil
}
