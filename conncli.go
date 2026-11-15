package main

import (
	"bufio"
	"context"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"slices"
	"strconv"
	"strings"
	"time"

	"github.com/charmbracelet/x/term"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/textutil"
)

// ─── db conn (R59.4) ───
// db 전용 접속을 화면 없이 더하고 고치고 지우고 시험한다. 비밀번호는 --url·--password-stdin 으로만 받고 찍지 않는다.

const connUsage = `사용법:
  db conn add <이름> --url URL [--group 그룹] [--kind 종류] [--user 사용자] [--auth A] [--region R] [--profile P] [--password-stdin] [--force]
  db conn edit <이름> [--url URL] [--host 호스트] [--port 포트] [--db DB] [--user 사용자] [--group 그룹] [--auth A] [--region R] [--profile P] [--password-stdin] [--force]
  db conn rm <이름> [--yes]
  db conn test <이름> | --url URL

  접속은 .dbtui/connections.json, 비밀번호는 .env 의 PG_<이름>_PASSWORD·REDIS_<이름>_PASSWORD(URL 인코딩)에 둔다.
  읽기 전용 저장소(R60)는 --kind opensearch|loki|prometheus|grafana|dynamodb 와 http(s) --url 로 더한다(DynamoDB 의 --url 은 선택 엔드포인트).
  --auth 는 basic·sigv4(OpenSearch)·token(Grafana), sigv4·DynamoDB 는 --region 이 필요하다. 비밀값 키는 OS_·LOKI_·PROM_·GRAFANA_<이름>_TOKEN 이다.
  비밀번호는 --url 또는 --password-stdin 으로만 받고 찍지 않는다. edit 에서 주지 않으면 그대로 둔다.
  add·edit 는 저장 전에 접속을 시험하고, 실패하면 --force 없이는 저장하지 않는다(종료 코드 1).
  rm 은 묻고 지운다. 표준 입력이 터미널이 아니면 --yes 가 필요하다. DataGrip 접속은 DataGrip 에서 고친다.
  prod 접속(R6.2)에 붙는 add·edit·test 는 --prod 가 필요하다.

공통 플래그: -p 경로 --timeout 30s
종료 코드: 0 성공 · 1 실행 오류(시험 실패 포함) · 2 사용법 · 3 안전장치 거절
`

// connMaxPassword 는 --password-stdin 으로 받는 비밀번호 상한이다.
const connMaxPassword = 64 << 10

type connCLIOpts struct {
	dir, url, group, kind, host, port, db, user string
	auth, region, profile                       string
	pwStdin, force, yes, prod                   bool
	timeout                                     time.Duration
	given                                       map[string]bool // 명령줄에 준 플래그
}

// runConnCLI 는 `db conn <add|edit|rm|test> …` 를 실행하고 종료 코드를 돌려준다. stdinTTY 면 rm 은 묻고 비밀번호는 가려 읽는다.
func runConnCLI(ctx context.Context, args []string, stdin io.Reader, stdinTTY bool, stdout, stderr io.Writer) (code int) {
	defer func() {
		if recover() != nil {
			fmt.Fprintln(stderr, "internal error")
			code = exitErr
		}
	}()
	if len(args) == 0 {
		fmt.Fprint(stderr, connUsage)
		return exitUsage
	}
	sub := args[0]
	if sub == "-h" || sub == "--help" || sub == "help" {
		fmt.Fprint(stdout, connUsage)
		return exitOK
	}
	fs := flag.NewFlagSet("db conn "+sub, flag.ContinueOnError)
	fs.SetOutput(stderr)
	fs.Usage = func() { fmt.Fprint(stderr, connUsage) }
	o := connCLIOpts{given: map[string]bool{}}
	fs.StringVar(&o.dir, "p", defaultProjectDir(), projectUsage)
	fs.DurationVar(&o.timeout, "timeout", 30*time.Second, "전체 시간 제한")
	switch sub {
	case "add", "edit":
		fs.StringVar(&o.url, "url", "", "접속 URL (postgres://user:pw@host:5432/db, redis://:pw@host:6379/0)")
		fs.StringVar(&o.group, "group", "", "그룹")
		fs.BoolVar(&o.pwStdin, "password-stdin", false, "비밀번호를 표준 입력에서 읽기")
		fs.BoolVar(&o.force, "force", false, "접속 시험이 실패해도 저장")
		fs.BoolVar(&o.prod, "prod", false, "prod 접속 허용")
		fs.StringVar(&o.user, "user", "", "사용자")
		fs.StringVar(&o.auth, "auth", "", "저장소 인증 basic|sigv4|token (없으면 비움)")
		fs.StringVar(&o.region, "region", "", "AWS 리전 (sigv4·DynamoDB)")
		fs.StringVar(&o.profile, "profile", "", "AWS 프로필 (비우면 표준 체인)")
		if sub == "add" {
			fs.StringVar(&o.kind, "kind", "", "postgresql|redis (URL 과 같아야 함)|opensearch|loki|prometheus|grafana|dynamodb")
		} else {
			fs.StringVar(&o.host, "host", "", "호스트")
			fs.StringVar(&o.port, "port", "", "포트")
			fs.StringVar(&o.db, "db", "", "DB (Redis 는 번호)")
		}
	case "rm":
		fs.BoolVar(&o.yes, "yes", false, "묻지 않고 지우기")
	case "test":
		fs.StringVar(&o.url, "url", "", "저장하지 않고 시험할 URL")
		fs.BoolVar(&o.prod, "prod", false, "prod 접속 허용")
	default:
		// 이름 자리에 URL(비밀번호)을 잘못 넣었을 수 있어 이름 꼴이 아니면 되풀이하지 않는다
		if !db.ValidConnName(sub) {
			sub = "(가림)"
		}
		fmt.Fprintf(stderr, "db conn: 알 수 없는 하위 명령: %s\n\n%s", sub, connUsage)
		return exitUsage
	}
	pos, err := parseInterleaved(fs, args[1:])
	switch {
	case errors.Is(err, flag.ErrHelp):
		return exitOK
	case err != nil:
		return exitUsage
	case o.timeout <= 0:
		fmt.Fprintln(stderr, "db conn: --timeout must be positive")
		return exitUsage
	}
	fs.Visit(func(f *flag.Flag) { o.given[f.Name] = true })
	ctx, cancel := context.WithTimeout(ctx, o.timeout)
	defer cancel()
	if err := connCommand(ctx, sub, o, pos, stdin, stdinTTY, stdout, stderr); err != nil {
		fmt.Fprintf(stderr, "db conn %s: %s\n", sub, textutil.Printable(err.Error(), true))
		var ce *cliError
		if errors.As(err, &ce) {
			return ce.code
		}
		return exitErr
	}
	return exitOK
}

func connCommand(ctx context.Context, sub string, o connCLIOpts, pos []string, stdin io.Reader, stdinTTY bool, stdout, stderr io.Writer) error {
	if sub == "test" && o.url != "" {
		if len(pos) > 0 {
			return usageErr("<이름> 과 --url 중 하나만 씀")
		}
		c, pw, err := db.ParseConnURL(o.url)
		if err != nil {
			return usageErr("%v", err)
		}
		c.Password = pw
		return connTest(ctx, o, c, stdout, stderr)
	}
	if len(pos) != 1 {
		return usageErr("접속 이름 하나가 필요함 (db help conn)")
	}
	name := pos[0]
	// 이름 자리에 URL 을 넣으면 비밀번호가 오류에 찍힐 수 있어 찾거나 되풀이하기 전에 거른다
	if !db.ValidConnName(name) {
		return usageErr("접속 이름은 영숫자·-·_ 만 받음")
	}
	proj, err := loadProject(o.dir)
	if err != nil {
		return err
	}
	i := connByName(proj, name)
	switch {
	case sub == "add":
		if i >= 0 {
			return usageErr("접속이 이미 있음: %s", name)
		}
		return connSave(ctx, o, proj, -1, name, stdin, stdinTTY, stdout, stderr)
	case i < 0:
		return usageErr("접속 없음: %s (db conns)", name)
	case sub == "test":
		return connTest(ctx, o, proj.Conns[i], stdout, stderr)
	case editRefusal(proj.Conns[i]) != "":
		return usageErr("%s", editRefusal(proj.Conns[i]))
	case sub == "edit":
		return connSave(ctx, o, proj, i, name, stdin, stdinTTY, stdout, stderr)
	}
	// rm
	if !o.yes {
		if !stdinTTY || stdin == nil {
			return usageErr("--yes 가 필요함 (표준 입력이 터미널이 아님)")
		}
		fmt.Fprintf(stderr, "%s 접속을 지울까요(.env 비밀번호도) y/N: ", name)
		ans, _ := bufio.NewReader(stdin).ReadString('\n')
		if strings.TrimSpace(ans) != "y" {
			return errors.New("취소함")
		}
	}
	if err := db.RemoveConn(proj.Dir, name); err != nil {
		return err
	}
	fmt.Fprintf(stdout, "지움: %s (db)\n", name)
	return nil
}

// connSave 는 add(i < 0)·edit 를 한다. 플래그로 접속 값을 만들고 시험한 뒤 저장한다.
func connSave(ctx context.Context, o connCLIOpts, proj *db.Project, i int, name string, stdin io.Reader, stdinTTY bool, stdout, stderr io.Writer) error {
	var c db.Conn
	keep := ""
	if i >= 0 {
		c, keep = proj.Conns[i], proj.Conns[i].Password
		c.Password = "" // 비우면 UpdateConn 이 .env 값을 그대로 둔다
		changed := false
		for _, f := range []string{"url", "host", "port", "db", "user", "group", "password-stdin", "auth", "region", "profile"} {
			changed = changed || o.given[f]
		}
		if !changed {
			return usageErr("고칠 값이 없음 (--url --host --port --db --user --group --auth --region --profile --password-stdin)")
		}
	} else {
		c.Kind = db.Kind(o.kind)
		if o.url == "" && c.Kind != db.KindDynamoDB {
			return usageErr("--url 이 필요함")
		}
	}
	if c.Kind.IsSource() {
		// 저장소는 http(s) URL 이고 사용자 정보는 user·.env 로 나눈다(R60.1)
		if o.url != "" {
			u, user, pw, err := db.ParseHTTPURL(o.url)
			if err != nil {
				return usageErr("%v", err)
			}
			c.URL = u
			if user != "" || pw != "" {
				c.User, c.Password = user, pw
			}
		}
		if i < 0 && !o.given["auth"] {
			c.Auth = db.AuthOptions(c.Kind)[0]
			if (c.User != "" || c.Password != "") && slices.Contains(db.AuthOptions(c.Kind), "basic") {
				c.Auth = "basic"
			}
		}
		for f, dst := range map[string]*string{"auth": &c.Auth, "region": &c.Region, "profile": &c.Profile} {
			if o.given[f] {
				*dst = map[string]string{"auth": o.auth, "region": o.region, "profile": o.profile}[f]
			}
		}
	} else if o.url != "" {
		u, pw, err := db.ParseConnURL(o.url)
		if err != nil {
			return usageErr("%v", err)
		}
		c.Kind, c.Host, c.Port, c.DB, c.User, c.Password = u.Kind, u.Host, u.Port, u.DB, u.User, pw
	}
	if o.kind != "" && db.Kind(o.kind) != c.Kind && !db.Kind(o.kind).IsSource() {
		return usageErr("--kind %s 가 URL 종류(%s)와 다름", o.kind, c.Kind)
	}
	c.Name = name
	if o.given["group"] {
		c.Group = o.group
	}
	if o.given["host"] {
		c.Host = o.host
	}
	if o.given["db"] {
		c.DB = o.db
	}
	if o.given["user"] {
		c.User = o.user
	}
	if o.given["port"] {
		n, err := strconv.Atoi(o.port)
		if err != nil || n < 1 || n > 65535 {
			return usageErr("--port 는 1~65535 숫자")
		}
		c.Port = n
	}
	if o.pwStdin {
		pw, err := readPasswordStdin(stdin, stdinTTY, stderr)
		if err != nil {
			return err
		}
		c.Password = pw
	}
	if err := db.ValidateConn(c); err != nil {
		return usageErr("%v", err)
	}
	if c.Env() == "prod" && !o.prod {
		return refused("prod 접속은 --prod 가 필요함")
	}
	test := c
	if test.Password == "" {
		test.Password = keep
	}
	if w := connWarning(c); w != "" {
		fmt.Fprintln(stderr, "경고: "+w)
	}
	note := ""
	if v, err := testConn(ctx, test); err != nil {
		if !o.force {
			return fmt.Errorf("접속 시험 실패 (저장하지 않음, --force 로 저장): %w", err)
		}
		fmt.Fprintf(stderr, "접속 시험 실패 (--force 로 저장): %s\n", textutil.Printable(err.Error(), true))
	} else {
		note = " · 접속됨 (" + v + ")"
	}
	verb := "추가함"
	var err error
	if i >= 0 {
		verb = "고침"
		c, err = db.UpdateConn(proj.Dir, name, c)
	} else {
		c, err = db.AddConn(proj.Dir, c)
	}
	// ErrEnvNotUpdated 면 접속은 저장됐다. 저장을 알리고 손볼 .env 키는 오류로 내 종료 코드 1이다
	if err != nil && !errors.Is(err, db.ErrEnvNotUpdated) {
		return err
	}
	fmt.Fprintf(stdout, "%s: %s · %s%s\n", verb, connLabel(c), connSummary(c), note)
	return err
}

func connTest(ctx context.Context, o connCLIOpts, c db.Conn, stdout, stderr io.Writer) error {
	if c.Env() == "prod" && !o.prod {
		return refused("prod 접속은 --prod 가 필요함")
	}
	if w := connWarning(c); w != "" {
		fmt.Fprintln(stderr, "경고: "+w)
	}
	v, err := testConn(ctx, c)
	if err != nil {
		return fmt.Errorf("접속 실패: %w", err)
	}
	fmt.Fprintf(stdout, "접속됨 (%s) · %s\n", v, connSummary(c))
	return nil
}

// connSummary 는 비밀번호 없는 접속 요약이다(PostgreSQL ro@host:5432/app, OpenSearch https://h sigv4 ap-northeast-2).
func connSummary(c db.Conn) string {
	if c.Kind.IsSource() {
		return strings.Join(slices.DeleteFunc([]string{connKinds[c.Kind], c.URL, c.Auth, c.Region, c.Profile},
			func(s string) bool { return s == "" }), " ")
	}
	user := ""
	if c.User != "" {
		user = c.User + "@"
	}
	return fmt.Sprintf("%s %s%s:%d/%s", connKinds[c.Kind], user, c.Host, c.Port, c.DB)
}

// readPasswordStdin 은 --password-stdin 의 비밀번호다. 터미널이면 가려 읽고, 아니면 끝 줄바꿈 하나를 뗀 표준 입력 전체다.
func readPasswordStdin(stdin io.Reader, tty bool, stderr io.Writer) (string, error) {
	if stdin == nil {
		return "", usageErr("--password-stdin 에 줄 표준 입력이 없음")
	}
	if f, ok := stdin.(*os.File); ok && tty {
		fmt.Fprint(stderr, "비밀번호: ")
		b, err := term.ReadPassword(f.Fd())
		fmt.Fprintln(stderr)
		if err != nil {
			return "", err
		}
		stdin = strings.NewReader(string(b))
	}
	b, err := io.ReadAll(io.LimitReader(stdin, connMaxPassword+1))
	if err != nil {
		return "", err
	}
	if len(b) > connMaxPassword {
		return "", usageErr("비밀번호가 64KiB 를 넘음")
	}
	pw := strings.TrimSuffix(strings.TrimSuffix(string(b), "\n"), "\r")
	if pw == "" {
		return "", usageErr("비밀번호가 비었음")
	}
	return pw, nil
}
