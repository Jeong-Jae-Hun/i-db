package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/charmbracelet/x/ansi"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/statcrypt"
)

const (
	cliPassword = "sekrit-pw-9"
	cliKey      = "0123456789abcdef0123456789abcdef"
	cliIV       = "00112233445566778899aabbccddeeff"
)

type cliSource struct{ name, group, driver, host, port, db, user string }

// writeCLIProject 는 .idea·.env 를 가진 가짜 프로젝트 폴더다. 각 접속 비밀번호는 cliPassword, 그룹 키·IV 는 cliKey·cliIV 다.
func writeCLIProject(t *testing.T, srcs []cliSource) string {
	t.Helper()
	dir := t.TempDir()
	must(t, os.MkdirAll(filepath.Join(dir, ".idea"), 0o755))
	var ds, local, env strings.Builder
	ds.WriteString(`<?xml version="1.0" encoding="UTF-8"?><project version="4"><component name="DataSourceManagerImpl">`)
	local.WriteString(`<?xml version="1.0" encoding="UTF-8"?><project version="4"><component name="dataSourceStorageLocal">`)
	groups := map[string]bool{}
	for _, s := range srcs {
		fmt.Fprintf(&ds, `<data-source name=%q group=%q uuid="u-%s"><driver-ref>%s</driver-ref><jdbc-url>jdbc:%s://%s:%s/%s</jdbc-url></data-source>`,
			s.name, s.group, s.name, s.driver, s.driver, s.host, s.port, s.db)
		fmt.Fprintf(&local, `<data-source name=%q uuid="u-%s"><user-name>%s</user-name></data-source>`, s.name, s.name, s.user)
		prefix := "PG_"
		if s.driver == "redis" {
			prefix = "REDIS_"
		}
		fmt.Fprintf(&env, "%s%s_PASSWORD=%s\n", prefix, db.EnvName(s.name), cliPassword)
		if s.group != "" && !groups[s.group] {
			groups[s.group] = true
			fmt.Fprintf(&env, "%s_AES_KEY=%s\n%s_AES_IV=%s\n", db.EnvName(s.group), cliKey, db.EnvName(s.group), cliIV)
		}
	}
	ds.WriteString(`</component></project>`)
	local.WriteString(`</component></project>`)
	must(t, os.WriteFile(filepath.Join(dir, ".idea", "dataSources.xml"), []byte(ds.String()), 0o644))
	must(t, os.WriteFile(filepath.Join(dir, ".idea", "dataSources.local.xml"), []byte(local.String()), 0o644))
	must(t, os.WriteFile(filepath.Join(dir, ".env"), []byte(env.String()), 0o600))
	return dir
}

// fakeCLIProject 의 접속은 닿지 않는 주소다. 안전장치가 접속 전에 거절하는지와 사용법 오류를 본다.
func fakeCLIProject(t *testing.T) string {
	return writeCLIProject(t, []cliSource{
		{"app-stg", "grp", "postgresql", "127.0.0.1", "1", "app", "u"},
		{"app-prod", "grp", "postgresql", "127.0.0.1", "1", "app", "u"},
		{"app-stg-redis", "grp", "redis", "127.0.0.1", "1", "0", ""},
		{"app-prod-redis", "grp", "redis", "127.0.0.1", "1", "0", ""},
		{"nogroup-stg", "", "postgresql", "127.0.0.1", "1", "app", "u"},
	})
}

// runCLITest 는 터미널이 아닌 stdout 으로 runCLI 를 돌린다. stdin 이 빈 글이면 nil(터미널)로 넘긴다.
func runCLITest(t *testing.T, stdin string, args ...string) (int, string, string) {
	t.Helper()
	var in io.Reader
	if stdin != "" {
		in = strings.NewReader(stdin)
	}
	var out, errw bytes.Buffer
	code := runCLI(context.Background(), args, in, &out, &errw, false)
	return code, out.String(), errw.String()
}

// ─── 인자·입력 ───

func TestCLI_플래그와_위치_인자를_섞어_읽고_대시대시_뒤는_모두_인자다(t *testing.T) {
	o, err := parseCLI("query", []string{"-c", "x", "select 1", "--format", "json", "--param", "a=1", "--param", "b=x=y", "--prod"}, false, io.Discard)
	if err != nil || o.conn != "x" || o.format != "json" || !o.prod || len(o.args) != 1 || o.args[0] != "select 1" {
		t.Fatalf("%+v %v", o, err)
	}
	if o.params["a"] != "1" || o.params["b"] != "x=y" {
		t.Fatalf("--param 은 첫 = 에서 나눈다: %v", o.params)
	}
	o, err = parseCLI("redis", []string{"-c", "r", "--", "INCRBY", "k", "-1"}, false, io.Discard)
	if err != nil || strings.Join(o.args, " ") != "INCRBY k -1" {
		t.Fatalf("-- 뒤: %+v %v", o.args, err)
	}
}

func TestCLI_형식_기본값은_터미널이면_table_아니면_tsv_이고_행_상한을_지킨다(t *testing.T) {
	if o, _ := parseCLI("conns", nil, true, io.Discard); o.format != "table" || o.maxRows != cliDefaultRows || o.timeout != 30*time.Second {
		t.Fatalf("터미널: %+v", o)
	}
	if o, _ := parseCLI("conns", nil, false, io.Discard); o.format != "tsv" {
		t.Fatalf("파이프: %+v", o)
	}
	if o, err := parseCLI("conns", []string{"--max-rows", "50000"}, false, io.Discard); err != nil || o.maxRows != cliMaxRows {
		t.Fatalf("상한: %d %v", o.maxRows, err)
	}
	for _, bad := range [][]string{{"--format", "xml"}, {"--max-rows", "0"}, {"--timeout", "0s"}, {"--param", "noeq"}, {"--nope"}} {
		if _, err := parseCLI("conns", bad, false, io.Discard); err == nil {
			t.Errorf("%v 는 사용법 오류여야 한다", bad)
		}
	}
}

func TestCLI_SQL_은_인자_파일_표준입력_순으로_읽는다(t *testing.T) {
	f := filepath.Join(t.TempDir(), "q.sql")
	must(t, os.WriteFile(f, []byte("select 2"), 0o644))
	cases := []struct {
		o     cliOpts
		stdin io.Reader
		want  string
	}{
		{cliOpts{args: []string{"select", "1"}, file: f}, strings.NewReader("select 3"), "select 1"},
		{cliOpts{file: f}, strings.NewReader("select 3"), "select 2"},
		{cliOpts{}, strings.NewReader("select 3"), "select 3"},
	}
	for _, c := range cases {
		if got, err := readInput(context.Background(), c.o, c.stdin); err != nil || got != c.want {
			t.Errorf("%+v: %q %v", c.o, got, err)
		}
	}
	var ce *cliError
	if _, err := readInput(context.Background(), cliOpts{}, nil); !errors.As(err, &ce) || ce.code != exitUsage {
		t.Fatalf("입력이 없으면 사용법 오류: %v", err)
	}
	if _, err := readInput(context.Background(), cliOpts{file: filepath.Join(t.TempDir(), "none.sql")}, nil); err == nil {
		t.Fatal("없는 파일은 오류")
	}
}

func TestCLI_enc_를_그룹_키로_확장하고_파라미터를_바인딩한다(t *testing.T) {
	proj, err := db.LoadProject(fakeCLIProject(t))
	must(t, err)
	c := proj.Conns[0]
	o := cliOpts{args: []string{"select * from u where email = enc('a@b.com') and id = :id and n = :id"}, params: paramFlag{"id": "7"}}
	var note bytes.Buffer
	sql, args, err := pgStatement(context.Background(), o, proj, c, nil, &note)
	must(t, err)
	want, err := statcrypt.Encrypt("a@b.com", cliKey, cliIV)
	must(t, err)
	if sql != "select * from u where email = '"+want+"' and id = $1 and n = $1" || len(args) != 1 || args[0] != "7" {
		t.Fatalf("%q %v", sql, args)
	}
	if !strings.Contains(note.String(), "enc() 1개 암호화") || strings.Contains(note.String(), cliKey) {
		t.Fatalf("안내: %q", note.String())
	}
	// 그룹 키가 없으면 서버 staticEncrypt 로 바꾼다
	sql, _, err = pgStatement(context.Background(), cliOpts{args: []string{"select enc('a')"}}, proj, proj.Conns[4], nil, io.Discard)
	if err != nil || sql != "select staticEncrypt('a')" {
		t.Fatalf("키 없음: %q %v", sql, err)
	}
	var ce *cliError
	for _, bad := range []cliOpts{
		{args: []string{"select :x"}, params: paramFlag{}},
		{args: []string{"select 1"}, params: paramFlag{"x": "1"}},
		{args: []string{"select 1; select 2"}, params: paramFlag{}},
		{args: []string{"select enc(email) from u"}, params: paramFlag{}},
	} {
		if _, _, err := pgStatement(context.Background(), bad, proj, c, nil, io.Discard); !errors.As(err, &ce) || ce.code != exitUsage {
			t.Errorf("%v: 사용법 오류여야 한다: %v", bad.args, err)
		}
	}
}

// ─── 출력 형식 ───

func TestCLI_json_은_명세_모양이고_NULL_은_null_이다(t *testing.T) {
	res := &db.Result{Cols: []string{"id", "name"}, Types: []string{"int8", "text"},
		Rows: [][]string{{"1", "a<b"}, {"2", "NULL"}}, Truncated: true, Elapsed: 9 * time.Millisecond}
	var out bytes.Buffer
	must(t, writeResults(&out, io.Discard, cliOpts{format: "json"}, []cliResult{{conn: db.Conn{Name: "c"}, res: res}}))
	want := `{"conn":"c","cols":["id","name"],"types":["int8","text"],"rows":[["1","a<b"],["2",null]],"truncated":true,"elapsed_ms":9}` + "\n"
	if out.String() != want {
		t.Fatalf("got  %s\nwant %s", out.String(), want)
	}
	out.Reset()
	must(t, writeResults(&out, io.Discard, cliOpts{format: "json"}, []cliResult{{conn: db.Conn{Name: "c"}, res: &db.Result{Text: "UPDATE 3", Elapsed: time.Millisecond}}}))
	if out.String() != `{"conn":"c","text":"UPDATE 3","elapsed_ms":1}`+"\n" {
		t.Fatalf("글 결과: %s", out.String())
	}
	// Types 가 없는 결과(Redis·목록)의 "NULL" 은 글자 그대로다
	out.Reset()
	must(t, writeResults(&out, io.Discard, cliOpts{format: "json"}, []cliResult{{res: &db.Result{Cols: []string{"v"}, Rows: [][]string{{"NULL"}}}}}))
	if !strings.Contains(out.String(), `"types":[],"rows":[["NULL"]]`) {
		t.Fatalf("Redis NULL: %s", out.String())
	}
}

func TestCLI_user360_json_은_제목별_배열이고_오류도_한_칸이다(t *testing.T) {
	var out bytes.Buffer
	results := []cliResult{
		{title: "계정", conn: db.Conn{Name: "a"}, res: &db.Result{Cols: []string{"id"}, Types: []string{"int8"}, Rows: [][]string{{"1"}}}},
		{title: "채널", conn: db.Conn{Name: "b"}, err: errors.New("boom")},
	}
	must(t, writeResults(&out, io.Discard, cliOpts{cmd: "user360", format: "json"}, results))
	var got []map[string]any
	must(t, json.Unmarshal(out.Bytes(), &got))
	if len(got) != 2 || got[0]["title"] != "계정" || got[1]["error"] != "boom" || got[1]["title"] != "채널" {
		t.Fatalf("%s", out.String())
	}
	out.Reset()
	must(t, writeResults(&out, io.Discard, cliOpts{cmd: "user360", format: "tsv"}, results))
	if out.String() != "== 계정 (a) ==\nid\n1\n\n== 채널 (b) ==\n오류: boom\n" {
		t.Fatalf("tsv: %q", out.String())
	}
}

func TestCLI_tsv_csv_는_내보내기와_같은_이스케이프다(t *testing.T) {
	res := &db.Result{Cols: []string{"a", "b"}, Types: []string{"text", "text"}, Rows: [][]string{{"x\ty\nz", `=1+1`}, {"NULL", `"q",`}}}
	var out, errw bytes.Buffer
	must(t, writeResults(&out, &errw, cliOpts{format: "tsv"}, []cliResult{{res: res}}))
	if out.String() != "a\tb\n"+`x\ty\nz`+"\t=1+1\nNULL\t\"q\",\n" {
		t.Fatalf("tsv: %q", out.String())
	}
	out.Reset()
	must(t, writeResults(&out, &errw, cliOpts{format: "csv"}, []cliResult{{res: res}}))
	if out.String() != "a,b\n\"x\ty\nz\",'=1+1\n,\"\"\"q\"\",\"\n" {
		t.Fatalf("csv: %q", out.String())
	}
	// 표에 딸린 글과 잘림은 stdout 을 깨지 않게 stderr 로 간다
	out.Reset()
	res.Text, res.Truncated = "indexes:\n  i", true
	must(t, writeResults(&out, &errw, cliOpts{format: "tsv"}, []cliResult{{res: res}}))
	if strings.Contains(out.String(), "indexes") || !strings.Contains(errw.String(), "indexes") || !strings.Contains(errw.String(), "결과가 잘림") {
		t.Fatalf("stdout %q stderr %q", out.String(), errw.String())
	}
}

func TestCLI_table_은_넓은_글자로_열을_맞추고_제어문자를_바꾼다(t *testing.T) {
	res := &db.Result{Cols: []string{"id", "name"}, Types: []string{"int8", "text"},
		Rows: [][]string{{"1", "한글이름"}, {"12345", "a\x1b[31mred\x07"}, {"7", strings.Repeat("긴", 100)}}, Truncated: true}
	var out bytes.Buffer
	must(t, writeResults(&out, io.Discard, cliOpts{format: "table"}, []cliResult{{res: res}}))
	s := out.String()
	if strings.ContainsAny(s, "\x1b\x07") || !strings.Contains(s, `a\x1b[31mred\x07`) {
		t.Fatalf("제어 문자가 그대로 나갔다: %q", s)
	}
	lines := strings.Split(strings.TrimSuffix(s, "\n"), "\n")
	if lines[0] != "   id  name" || !strings.HasPrefix(lines[2], "    1  한글이름") || !strings.HasPrefix(lines[3], "12345  a") {
		t.Fatalf("정렬:\n%s", s)
	}
	if w := ansi.StringWidth(lines[4]); w > 5+2+cliCellWidth || w < 5+2+cliCellWidth-1 || !strings.HasSuffix(lines[4], "…") {
		t.Fatalf("긴 칸은 %d칸에서 자른다: %d %q", cliCellWidth, w, lines[4])
	}
	if lines[5] != "(3행, 잘림 — --max-rows 로 늘림)" {
		t.Fatalf("끝 줄: %q", lines[5])
	}
}

// ─── 안전장치·종료 코드 ───

func TestCLI_prod_는_읽기도_prod_플래그가_없으면_3(t *testing.T) {
	dir := fakeCLIProject(t)
	for _, args := range [][]string{
		{"query", "-p", dir, "-c", "app-prod", "select 1"},
		{"schemas", "-p", dir, "-c", "app-prod"},
		{"redis", "-p", dir, "-c", "app-prod-redis", "GET", "k"},
		{"schema-diff", "-p", dir, "-c", "app-stg", "--to", "app-prod"},
	} {
		code, out, errw := runCLITest(t, "", args...)
		if code != exitRefused || errw != "prod 접속은 --prod 가 필요함\n" || out != "" {
			t.Errorf("%v: %d %q %q", args, code, out, errw)
		}
	}
	if code, _, errw := runCLITest(t, "", "user360", "-p", dir, "a@b.com"); code != exitRefused || !strings.Contains(errw, "--prod") {
		t.Fatalf("user360: %d %q", code, errw)
	}
	// --prod 가 있으면 거절하지 않고 접속까지 간다(닿지 않는 주소라 실행 오류 1)
	if code, _, errw := runCLITest(t, "", "query", "-p", dir, "-c", "app-prod", "--prod", "select 1"); code != exitErr {
		t.Fatalf("--prod: %d %q", code, errw)
	}
}

func TestCLI_prod_쓰기는_prod_플래그가_있어도_거절한다(t *testing.T) {
	dir := fakeCLIProject(t)
	for _, args := range [][]string{
		{"query", "-p", dir, "-c", "app-prod", "--prod", "--write", "--force", "update t set a = 1 where id = 1"},
		{"redis", "-p", dir, "-c", "app-prod-redis", "--prod", "--write", "SET", "k", "v"},
	} {
		if code, _, errw := runCLITest(t, "", args...); code != exitRefused || !strings.Contains(errw, "prod 쓰기") {
			t.Errorf("%v: %d %q", args, code, errw)
		}
	}
}

func TestCLI_쓰기_없는_쓰기_문장과_위험_문장은_force_없이_거절한다(t *testing.T) {
	dir := fakeCLIProject(t)
	cases := []struct {
		args []string
		msg  string
	}{
		{[]string{"query", "-c", "app-stg", "delete from t where id = 1"}, "--write"},
		{[]string{"query", "-c", "app-stg", "select nextval('s')"}, "--write"},
		{[]string{"query", "-c", "app-stg", "--write", "delete from t"}, "위험: WHERE 없는 DELETE (--force)"},
		{[]string{"query", "-c", "app-stg", "--write", "drop table t"}, "위험: DROP (--force)"},
		{[]string{"explain", "-c", "app-stg", "explain analyze select 1"}, "ANALYZE"},
		{[]string{"explain", "-c", "app-stg", "explain (analyze) delete from t"}, "ANALYZE"},
		{[]string{"redis", "-c", "app-stg-redis", "SET", "k", "v"}, "--write"},
		{[]string{"redis", "-c", "app-stg-redis", "--write", "FLUSHALL"}, "위험: FLUSHALL (--force)"},
	}
	for _, c := range cases {
		code, _, errw := runCLITest(t, "", append(c.args, "-p", dir)...)
		if code != exitRefused || !strings.Contains(errw, c.msg) {
			t.Errorf("%v: %d %q", c.args, code, errw)
		}
	}
	// 허락되면 거절하지 않고 접속까지 간다
	for _, args := range [][]string{
		{"query", "-c", "app-stg", "--write", "--force", "delete from t"},
		{"query", "-c", "app-stg", "--write", "delete from t where id = 1"},
		{"explain", "-c", "app-stg", "delete from t"},
		{"redis", "-c", "app-stg-redis", "GET", "k"},
		{"redis", "-c", "app-stg-redis", "--write", "SET", "k", "v"},
	} {
		if code, _, errw := runCLITest(t, "", append(args, "-p", dir)...); code != exitErr {
			t.Errorf("%v: 접속 오류(1)여야 한다: %d %q", args, code, errw)
		}
	}
}

func TestCLI_사용법_오류는_2(t *testing.T) {
	dir := fakeCLIProject(t)
	for _, args := range [][]string{
		{"query", "-p", dir, "select 1"},
		{"query", "-p", dir, "-c", "none", "select 1"},
		{"query", "-p", dir, "-c", "app-stg-redis", "select 1"},
		{"query", "-p", dir, "-c", "app-stg"},
		{"describe", "-p", dir, "-c", "app-stg"},
		{"query", "-p", dir, "--bogus"},
		{"help", "nope"},
	} {
		if code, _, errw := runCLITest(t, "", args...); code != exitUsage {
			t.Errorf("%v: %d %q", args, code, errw)
		}
	}
	if code, out, _ := runCLITest(t, "", "help"); code != exitOK || !strings.Contains(out, "schema-diff") {
		t.Fatalf("help: %d %q", code, out)
	}
	if code, _, errw := runCLITest(t, "", "query", "-h"); code != exitOK || !strings.Contains(errw, "사용법: db query") {
		t.Fatalf("-h: %d %q", code, errw)
	}
	if code, _, _ := runCLITest(t, "", "conns", "-p", t.TempDir()); code != exitErr {
		t.Fatalf("프로젝트가 없으면 1: %d", code)
	}
}

func TestCLI_conns_와_오류에_비밀번호와_키가_나오지_않는다(t *testing.T) {
	dir := fakeCLIProject(t)
	for _, f := range cliFormats {
		code, out, _ := runCLITest(t, "", "conns", "-p", dir, "--format", f)
		if code != exitOK || !strings.Contains(out, "app-prod-redis") || strings.Contains(out, cliPassword) || strings.Contains(out, cliKey) {
			t.Fatalf("%s: %d %q", f, code, out)
		}
	}
	proj, err := db.LoadProject(dir)
	must(t, err)
	msg := cliErrText(context.Background(), proj, cliOpts{}, fmt.Errorf("auth failed for %s key=%s iv=%s url=%s\x1b[2J",
		cliPassword, cliKey, cliIV, url.UserPassword("u", cliPassword).String()), "app-stg")
	if strings.Contains(msg, cliPassword) || strings.Contains(msg, cliKey) || strings.Contains(msg, cliIV) || strings.Contains(msg, "\x1b") {
		t.Fatalf("가려지지 않았다: %q", msg)
	}
}

func TestCLI_decrypt_는_플래그일_때만_그룹_키로_푼다(t *testing.T) {
	dir := fakeCLIProject(t)
	proj, err := db.LoadProject(dir)
	must(t, err)
	enc, err := statcrypt.Encrypt("a@b.com", cliKey, cliIV)
	must(t, err)
	bad := cliIV + "^|^" + strings.Repeat("00", 16)
	res := &db.Result{Cols: []string{"e", "x"}, Rows: [][]string{{enc, "plain"}, {bad, "NULL"}}}
	var errw bytes.Buffer
	must(t, decryptResults(proj, []cliResult{{conn: proj.Conns[0], res: res}}, &errw))
	if res.Rows[0][0] != "a@b.com" || res.Rows[0][1] != "plain" || res.Rows[1][0] != bad || errw.String() != "1칸 풀지 못함\n" {
		t.Fatalf("%v %q", res.Rows, errw.String())
	}
	if err := decryptResults(proj, []cliResult{{conn: proj.Conns[4], res: &db.Result{}}}, io.Discard); err == nil || !strings.Contains(err.Error(), "접속 그룹을 몰라") {
		t.Fatalf("그룹 없음: %v", err)
	}
	proj.AESEnv = map[string]string{}
	if err := decryptResults(proj, []cliResult{{conn: proj.Conns[0], res: &db.Result{}}}, io.Discard); err == nil || err.Error() != "GRP_AES_KEY 가 .env 에 없음" {
		t.Fatalf("키 없음: %v", err)
	}
}

// ─── 리뷰 수정 ───

func TestCLI_write_force_decrypt_는_허용된_명령에서만_받는다(t *testing.T) {
	dir := fakeCLIProject(t)
	for _, args := range [][]string{
		{"explain", "-c", "app-stg", "--write", "delete from t"},
		{"explain", "-c", "app-stg", "--force", "select 1"},
		{"schemas", "-c", "app-stg", "--write"},
		{"describe", "-c", "app-stg", "--force", "t"},
		{"ddl", "-c", "app-stg", "--decrypt", "t"},
		{"redis", "-c", "app-stg-redis", "--decrypt", "GET", "k"},
		{"explain", "-c", "app-stg", "--decrypt", "select 1"},
		{"query", "-c", "app-stg", "--force", "select 1"},
	} {
		if code, _, errw := runCLITest(t, "", append(args, "-p", dir)...); code != exitUsage {
			t.Errorf("%v: 사용법 오류(2)여야 한다: %d %q", args, code, errw)
		}
	}
	// --prod 는 어디서나 받고, 허용된 조합은 접속까지 간다(닿지 않는 주소라 1)
	for _, args := range [][]string{
		{"explain", "-c", "app-stg", "--prod", "select 1"},
		{"query", "-c", "app-stg", "--decrypt", "select 1"},
		{"search", "-c", "app-stg", "--decrypt", "x"},
	} {
		if code, _, errw := runCLITest(t, "", append(args, "-p", dir)...); code != exitErr {
			t.Errorf("%v: 접속 오류(1)여야 한다: %d %q", args, code, errw)
		}
	}
	if code, _, errw := runCLITest(t, "", "conns", "-p", dir, "--prod"); code != exitOK {
		t.Fatalf("conns --prod: %d %q", code, errw)
	}
}

func TestCLI_잘린_칸과_복호화_실패를_json_과_stderr_로_알린다(t *testing.T) {
	big := strings.Repeat("a", db.MaxCellBytes) + "…(70000바이트)"
	res := &db.Result{Cols: []string{"v"}, Types: []string{"text"}, Rows: [][]string{{big}, {"x"}, {big}}}
	var out, errw bytes.Buffer
	results := []cliResult{{conn: db.Conn{Name: "c"}, res: res, decryptFailed: 1}}
	must(t, writeResults(&out, &errw, cliOpts{format: "json"}, results))
	var m map[string]any
	must(t, json.Unmarshal(out.Bytes(), &m))
	if m["cells_truncated"] != 2.0 || m["decrypt_failed"] != 1.0 {
		t.Fatalf("json: %v", m)
	}
	if !strings.Contains(errw.String(), "칸 2개가 64KiB 에서 잘림") {
		t.Fatalf("stderr: %q", errw.String())
	}
	// 잘린 칸이 없으면 필드를 빼고 경고도 없다
	out.Reset()
	errw.Reset()
	results = []cliResult{{res: &db.Result{Cols: []string{"v"}, Rows: [][]string{{"x"}}}}}
	must(t, writeResults(&out, &errw, cliOpts{format: "tsv"}, results))
	if errw.String() != "" || strings.Contains(out.String(), "cells_truncated") {
		t.Fatalf("%q %q", out.String(), errw.String())
	}
	// 복호화 실패는 종료 코드 1
	dir := fakeCLIProject(t)
	proj, err := db.LoadProject(dir)
	must(t, err)
	bad := cliIV + "^|^" + strings.Repeat("00", 16)
	results = []cliResult{{conn: proj.Conns[0], res: &db.Result{Cols: []string{"e"}, Rows: [][]string{{bad}}}}}
	errw.Reset()
	must(t, decryptResults(proj, results, &errw))
	if results[0].decryptFailed != 1 || !strings.Contains(errw.String(), "1칸 풀지 못함") {
		t.Fatalf("%+v %q", results[0], errw.String())
	}
}

func TestCLI_표준입력이_닫히지_않으면_timeout_에_1로_끝난다(t *testing.T) {
	dir := fakeCLIProject(t)
	pr, pw := io.Pipe()
	t.Cleanup(func() { pw.Close() })
	var out, errw bytes.Buffer
	start := time.Now()
	code := runCLI(context.Background(), []string{"query", "-p", dir, "-c", "app-stg", "--timeout", "200ms"}, pr, &out, &errw, false)
	if code != exitErr || time.Since(start) > 2*time.Second || !strings.Contains(errw.String(), "시간 초과") {
		t.Fatalf("%d %v %q", code, time.Since(start), errw.String())
	}
}

func TestCLI_표준입력이_16MiB_를_넘으면_오류다(t *testing.T) {
	big := io.MultiReader(strings.NewReader("select '"), io.LimitReader(zeroReader{}, cliMaxStdin), strings.NewReader("'"))
	if _, err := readInput(context.Background(), cliOpts{}, big); err == nil || !strings.Contains(err.Error(), "16MiB") {
		t.Fatalf("%v", err)
	}
	if got, err := readInput(context.Background(), cliOpts{}, strings.NewReader("select 1")); err != nil || got != "select 1" {
		t.Fatalf("%q %v", got, err)
	}
}

type zeroReader struct{}

func (zeroReader) Read(p []byte) (int, error) {
	for i := range p {
		p[i] = 'x'
	}
	return len(p), nil
}

func TestCLI_redis_KEYS_는_SCAN_을_권하며_거절한다(t *testing.T) {
	dir := fakeCLIProject(t)
	for _, args := range [][]string{
		{"redis", "-c", "app-stg-redis", "KEYS", "user:*"},
		{"redis", "-c", "app-stg-redis", "--write", "keys", "*"},
	} {
		code, _, errw := runCLITest(t, "", append(args, "-p", dir)...)
		if code != exitRefused || !strings.Contains(errw, "SCAN 0 MATCH") || !strings.Contains(errw, "COUNT 1000") {
			t.Errorf("%v: %d %q", args, code, errw)
		}
	}
	if code, _, errw := runCLITest(t, "", "redis", "-p", dir, "-c", "app-stg-redis", "SCAN", "0"); code != exitErr {
		t.Fatalf("SCAN 은 접속까지 간다: %d %q", code, errw)
	}
}

func TestCLI_write_는_쓰기_동사만_받고_숨은_쓰기는_force_가_필요하다(t *testing.T) {
	dir := fakeCLIProject(t)
	cases := []struct{ sql, msg string }{
		{"do $$ begin perform 1; end $$", "--write 로 실행하지 않는 문장: DO"},
		{"call p()", "CALL"},
		{"copy t to program 'rm -rf /'", "COPY"},
		{"copy t from '/etc/passwd'", "COPY"},
		{"set role postgres", "SET"},
		{"with x as (delete from t returning 1) select * from x", "위험: CTE 안 쓰기 (--force)"},
		{"select * into t2 from t", "위험: SELECT INTO (--force)"},
		{"update t set a = pg_terminate_backend(1) where id = 1", "위험: 부수효과 함수 pg_terminate_backend (--force)"},
	}
	for _, c := range cases {
		code, _, errw := runCLITest(t, "", "query", "-p", dir, "-c", "app-stg", "--write", c.sql)
		if code != exitRefused || !strings.Contains(errw, c.msg) {
			t.Errorf("%s: %d %q", c.sql, code, errw)
		}
	}
	// COPY 는 --force 여도 거절하고, 숨은 쓰기는 --force 면 접속까지 간다
	if code, _, errw := runCLITest(t, "", "query", "-p", dir, "-c", "app-stg", "--write", "--force", "copy t to program 'x'"); code != exitRefused {
		t.Fatalf("COPY --force: %d %q", code, errw)
	}
	for _, args := range [][]string{
		{"--force", "with x as (delete from t returning 1) select * from x"},
		{"insert into t values (1)"}, {"create index i on t (a)"}, {"vacuum t"}, {"refresh materialized view v"},
	} {
		if code, _, errw := runCLITest(t, "", append([]string{"query", "-p", dir, "-c", "app-stg", "--write"}, args...)...); code != exitErr {
			t.Errorf("%v: 접속 오류(1)여야 한다: %d %q", args, code, errw)
		}
	}
}

func TestCLI_write_의_잘림_안내는_max_rows_를_권하지_않는다(t *testing.T) {
	res := &db.Result{Cols: []string{"a"}, Rows: [][]string{{"1"}}, Truncated: true}
	var out, errw bytes.Buffer
	must(t, writeResults(&out, &errw, cliOpts{format: "tsv", write: true}, []cliResult{{res: res}}))
	if strings.Contains(errw.String(), "--max-rows") || !strings.Contains(errw.String(), "1,000행 상한") {
		t.Fatalf("tsv: %q", errw.String())
	}
	out.Reset()
	must(t, writeResults(&out, &errw, cliOpts{format: "table", write: true}, []cliResult{{res: res}}))
	if strings.Contains(out.String(), "--max-rows") || !strings.Contains(out.String(), "1,000행 상한") {
		t.Fatalf("table: %q", out.String())
	}
}

type panicWriter struct{}

func (panicWriter) Write([]byte) (int, error) { panic("boom " + cliPassword) }

func TestCLI_패닉은_internal_error_와_1로_끝난다(t *testing.T) {
	dir := fakeCLIProject(t)
	var errw bytes.Buffer
	code := runCLI(context.Background(), []string{"conns", "-p", dir}, nil, panicWriter{}, &errw, false)
	if code != exitErr || errw.String() != "internal error\n" {
		t.Fatalf("%d %q", code, errw.String())
	}
}

// ─── 통합 (DBTUI_TEST_PG_URL·DBTUI_TEST_REDIS_ADDR 가 있을 때만) ───

func integrationCLIProject(t *testing.T) string {
	t.Helper()
	raw, addr := os.Getenv("DBTUI_TEST_PG_URL"), os.Getenv("DBTUI_TEST_REDIS_ADDR")
	if raw == "" || addr == "" {
		t.Skip("DBTUI_TEST_PG_URL / DBTUI_TEST_REDIS_ADDR not set")
	}
	u, err := url.Parse(raw)
	must(t, err)
	rhost, rport, err := net.SplitHostPort(addr)
	must(t, err)
	dir := writeCLIProject(t, []cliSource{
		{"clitest-local", "clitest", "postgresql", u.Hostname(), u.Port(), strings.TrimPrefix(u.Path, "/"), u.User.Username()},
		{"clitest-local-redis", "clitest", "redis", rhost, rport, "0", ""},
	})
	// 시험 PG 비밀번호는 접속 URL 의 것이다. 파일의 cliPassword 를 바꿔 쓴다
	pw, _ := u.User.Password()
	envPath := filepath.Join(dir, ".env")
	b, err := os.ReadFile(envPath)
	must(t, err)
	must(t, os.WriteFile(envPath, []byte(strings.Replace(string(b), "PG_CLITEST_LOCAL_PASSWORD="+cliPassword, "PG_CLITEST_LOCAL_PASSWORD="+url.QueryEscape(pw), 1)), 0o600))
	return dir
}

func cliJSON(t *testing.T, args ...string) map[string]any {
	t.Helper()
	code, out, errw := runCLITest(t, "", append(args, "--format", "json")...)
	if code != exitOK {
		t.Fatalf("%v: %d %q", args, code, errw)
	}
	var m map[string]any
	must(t, json.Unmarshal([]byte(out), &m))
	return m
}

func TestCLIIntegration_PG_읽기_쓰기_enc_decrypt_행_상한(t *testing.T) {
	dir := integrationCLIProject(t)
	pc := []string{"-p", dir, "-c", "clitest-local"}
	must(t, cliRun(t, cliQuery(pc, "drop schema if exists clitest cascade", "--write", "--force")))
	must(t, cliRun(t, cliQuery(pc, "create schema clitest", "--write")))
	must(t, cliRun(t, cliQuery(pc, "create table clitest.t (id int primary key, email text)", "--write")))
	// 읽기 경로는 서버가 막는다. 판정이 놓치는 CTE 안 쓰기·select into 도 실행 오류(1)이고 행이 생기지 않는다
	for _, sql := range []string{"with x as (insert into clitest.t values (99, 'x') returning 1) select * from x", "select 1 into clitest.t2"} {
		if code, _, errw := runCLITest(t, "", cliQuery(pc, sql)...); code != exitErr || !strings.Contains(errw, "SQLSTATE") {
			t.Fatalf("%s: %d %q", sql, code, errw)
		}
	}
	m := cliJSON(t, cliQuery(pc, "insert into clitest.t values (:id, enc('a@b.com'))", "--write", "--param", "id=1")...)
	if m["text"] != "INSERT 0 1" {
		t.Fatalf("insert: %v", m)
	}
	enc, err := statcrypt.Encrypt("a@b.com", cliKey, cliIV)
	must(t, err)
	m = cliJSON(t, cliQuery(pc, "select id, email from clitest.t where email = enc('a@b.com')")...)
	if fmt.Sprint(m["rows"]) != "[[1 "+enc+"]]" || fmt.Sprint(m["types"]) != "[int4 text]" || m["conn"] != "clitest-local" {
		t.Fatalf("enc 확장·암호문 그대로: %v", m)
	}
	m = cliJSON(t, cliQuery(pc, "select email from clitest.t", "--decrypt")...)
	if fmt.Sprint(m["rows"]) != "[[a@b.com]]" {
		t.Fatalf("--decrypt: %v", m)
	}

	// 풀지 못한 칸은 decrypt_failed 로 세고 종료 코드 1, 64KiB 를 넘는 칸은 cells_truncated 로 센다
	bad := cliIV + "^|^" + strings.Repeat("00", 16)
	code, out, errw := runCLITest(t, "", cliQuery(pc, "select :v as e", "--param", "v="+bad, "--decrypt", "--format", "json")...)
	if code != exitErr || !strings.Contains(out, `"decrypt_failed":1`) || !strings.Contains(errw, "1칸 풀지 못함") {
		t.Fatalf("decrypt 실패: %d %q %q", code, out, errw)
	}
	code, out, errw = runCLITest(t, "", cliQuery(pc, "select repeat('a', 70000) as v", "--format", "json")...)
	if code != exitOK || !strings.Contains(out, `"cells_truncated":1`) || !strings.Contains(errw, "칸 1개가 64KiB 에서 잘림") {
		t.Fatalf("잘린 칸: %d %q", code, errw)
	}

	m = cliJSON(t, cliQuery(pc, "select g from generate_series(1, 2000) g", "--max-rows", "1500")...)
	if rows := m["rows"].([]any); len(rows) != 1500 || m["truncated"] != true {
		t.Fatalf("1500행에서 잘림: %d %v", len(rows), m["truncated"])
	}
	m = cliJSON(t, cliQuery(pc, "select g from generate_series(1, 3) g", "--max-rows", "3")...)
	if rows := m["rows"].([]any); len(rows) != 3 || m["truncated"] != false {
		t.Fatalf("딱 3행은 잘림 아님: %v", m)
	}
	if code, _, errw := runCLITest(t, "", cliQuery(pc, "select * from clitest.none")...); code != exitErr || strings.Contains(errw, cliPassword) {
		t.Fatalf("없는 테이블: %d %q", code, errw)
	}
	// 표준 입력·explain·구조
	if code, out, _ := runCLITest(t, "select count(*) as n from clitest.t;\n", append([]string{"query"}, pc...)...); code != exitOK || out != "n\n1\n" {
		t.Fatalf("표준 입력: %d %q", code, out)
	}
	if code, out, _ := runCLITest(t, "", append(append([]string{"explain"}, pc...), "delete from clitest.t")...); code != exitOK || !strings.Contains(out, "Delete on t") {
		t.Fatalf("explain: %d %q", code, out)
	}
	if fmt.Sprint(cliJSON(t, cliQuery(pc, "select count(*) from clitest.t")...)["rows"]) != "[[1]]" {
		t.Fatal("explain 이 delete 를 실행했다")
	}
	if code, out, _ := runCLITest(t, "", append(append([]string{"tables"}, pc...), "-s", "clitest")...); code != exitOK || out != "table\nt\n" {
		t.Fatalf("tables: %d %q", code, out)
	}
	if m := cliJSON(t, append(append([]string{"describe"}, pc...), "clitest.t")...); !strings.Contains(fmt.Sprint(m["rows"]), "email text") || !strings.Contains(m["text"].(string), "PRIMARY KEY") {
		t.Fatalf("describe: %v", m)
	}
	if m := cliJSON(t, append(append([]string{"ddl"}, pc...), "-s", "clitest", "t")...); !strings.Contains(m["text"].(string), "CREATE TABLE") {
		t.Fatalf("ddl: %v", m)
	}
	if m := cliJSON(t, append(append([]string{"search"}, pc...), "-s", "clitest", enc[:10])...); len(m["rows"].([]any)) != 1 {
		t.Fatalf("search: %v", m)
	}
	if m := cliJSON(t, append(append([]string{"schema-diff"}, pc...), "--to", "clitest-local", "-s", "clitest", "--to-schema", "public")...); !strings.Contains(fmt.Sprint(m["rows"]), "t") {
		t.Fatalf("schema-diff: %v", m)
	}

	// user360: 조회마다 한 칸, 그룹 키로 암호화한 :email_enc, 실패한 조회는 오류 칸이고 종료 코드 1
	must(t, os.MkdirAll(filepath.Join(dir, ".dbtui"), 0o755))
	must(t, os.WriteFile(filepath.Join(dir, ".dbtui", "user360.json"), []byte(`[
		{"title": "사용자", "conn": "clitest-local", "by": ["email", "id"], "sql": "select id from clitest.t where email = :email_enc or id = :id::int"},
		{"title": "없는 접속", "conn": "clitest-none", "by": ["email"], "sql": "select :email"}]`), 0o644))
	code, out, errw = runCLITest(t, "", "user360", "-p", dir, "--prod", "a@b.com", "--format", "json")
	var secs []map[string]any
	must(t, json.Unmarshal([]byte(out), &secs))
	if code != exitErr || len(secs) != 2 || secs[0]["title"] != "사용자" || fmt.Sprint(secs[0]["rows"]) != "[[1]]" ||
		secs[1]["error"] != "접속 없음: clitest-none" || !strings.Contains(errw, "없는 접속") {
		t.Fatalf("user360: %d %s %q", code, out, errw)
	}
	must(t, cliRun(t, cliQuery(pc, "drop schema clitest cascade", "--write", "--force")))
}

// cliQuery 는 pc(-p·-c) 접속에서 query 명령 인자다.
func cliQuery(pc []string, sql string, extra ...string) []string {
	return append(append(append([]string{"query"}, pc...), sql), extra...)
}

// cliRun 은 성공해야 하는 명령이다. 실패하면 stderr 를 오류로 돌려준다.
func cliRun(t *testing.T, args []string) error {
	t.Helper()
	if code, _, errw := runCLITest(t, "", args...); code != exitOK {
		return fmt.Errorf("%v: %d %s", args, code, errw)
	}
	return nil
}

func TestCLIIntegration_Redis_읽기는_바로_쓰기는_write_로(t *testing.T) {
	dir := integrationCLIProject(t)
	rc := []string{"redis", "-p", dir, "-c", "clitest-local-redis"}
	must(t, cliRun(t, append(rc, "--write", "SET", "clitest:k", "값 1")))
	if m := cliJSON(t, append(rc, "GET", "clitest:k")...); m["text"] != "값 1" || m["conn"] != "clitest-local-redis" {
		t.Fatalf("GET: %v", m)
	}
	if code, out, _ := runCLITest(t, "", append(rc, "EXISTS clitest:k")...); code != exitOK || out != "1\n" {
		t.Fatalf("한 인자 명령 줄: %d %q", code, out)
	}
	must(t, cliRun(t, append(rc, "--write", "DEL", "clitest:k")))
}
