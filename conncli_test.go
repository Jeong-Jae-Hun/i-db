package main

import (
	"bytes"
	"context"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

const connCLIPw = "Cli-Sekrit-42"

func connCLIProject(t *testing.T) string {
	return writeCLIProject(t, []cliSource{{"dg-stg", "grp", "postgresql", "127.0.0.1", "1", "app", "u"}})
}

// noSecret 은 출력 어디에도 비밀번호(원문·인코딩)가 없는지 본다.
func noSecret(t *testing.T, secret string, outs ...string) {
	t.Helper()
	for _, s := range outs {
		if strings.Contains(s, secret) || strings.Contains(s, url.QueryEscape(secret)) {
			t.Fatalf("출력에 비밀번호: %q", s)
		}
	}
}

func readEnvFile(t *testing.T, dir string) string {
	t.Helper()
	b, err := os.ReadFile(filepath.Join(dir, ".env"))
	must(t, err)
	return string(b)
}

func TestConnCLI_add_는_시험이_실패하면_force_없이_저장하지_않는다(t *testing.T) {
	dir := connCLIProject(t)
	u := "postgres://ro:" + connCLIPw + "@127.0.0.1:1/app"
	code, out, errw := runCLITest(t, "", "conn", "add", "app-stg2", "--url", u, "--group", "app", "-p", dir)
	if code != exitErr || !strings.Contains(errw, "접속 시험 실패") {
		t.Fatalf("code=%d err=%q", code, errw)
	}
	noSecret(t, connCLIPw, out, errw)
	if _, err := os.Stat(filepath.Join(dir, ".dbtui", "connections.json")); !os.IsNotExist(err) {
		t.Fatal("저장하지 않는다")
	}

	code, out, errw = runCLITest(t, "", "conn", "add", "app-stg2", "--url", u, "--group", "app", "-p", dir, "--force")
	if code != exitOK || !strings.Contains(out, "추가함: app-stg2 (db) · PostgreSQL ro@127.0.0.1:1/app") {
		t.Fatalf("code=%d out=%q err=%q", code, out, errw)
	}
	noSecret(t, connCLIPw, out, errw)
	if env := readEnvFile(t, dir); !strings.Contains(env, "PG_APP_STG2_PASSWORD="+connCLIPw+"\n") {
		t.Fatalf(".env: %q", env)
	}
	code, out, _ = runCLITest(t, "", "conns", "-p", dir, "--format", "table")
	if code != exitOK || !strings.Contains(out, "app-stg2 (db)") {
		t.Fatalf("conns 표는 (db) 를 붙인다: %q", out)
	}
	_, out, _ = runCLITest(t, "", "conns", "-p", dir, "--format", "tsv")
	if !strings.Contains(out, "app-stg2\tapp\tpostgresql\tstg\t127.0.0.1\tapp\tdb\tapp\n") || !strings.Contains(out, "dg-stg\tgrp\tpostgresql\tstg\t127.0.0.1\tapp\tdatagrip\tgrp\n") {
		t.Fatalf("tsv 는 이름 그대로 source 열: %q", out)
	}
	if code, _, errw := runCLITest(t, "", "conn", "add", "app-stg2", "--url", u, "-p", dir, "--force"); code != exitUsage || !strings.Contains(errw, "이미 있음") {
		t.Fatalf("같은 이름: %d %q", code, errw)
	}
}

func TestConnCLI_사용법_오류는_종료_코드_2(t *testing.T) {
	dir := connCLIProject(t)
	cases := [][]string{
		{"conn"},
		{"conn", "bogus"},
		{"conn", "add", "x", "-p", dir},
		{"conn", "add", "bad name", "--url", "postgres://h/d", "-p", dir},
		{"conn", "add", "x", "--url", "mysql://u:" + connCLIPw + "@h/d", "-p", dir},
		{"conn", "add", "x", "--url", "redis://h:1/0", "--kind", "postgresql", "-p", dir},
		{"conn", "add", "x", "--url", "postgres://h/d", "--password", "pw", "-p", dir},
		{"conn", "edit", "nope", "--host", "h", "-p", dir},
		{"conn", "edit", "dg-stg", "--host", "h", "-p", dir},
		{"conn", "rm", "dg-stg", "--yes", "-p", dir},
		{"conn", "test", "-p", dir},
		{"conn", "test", "dg-stg", "--url", "postgres://h/d", "-p", dir},
	}
	for _, args := range cases {
		code, out, errw := runCLITest(t, "", args...)
		if code != exitUsage {
			t.Errorf("%v: code=%d err=%q", args, code, errw)
		}
		noSecret(t, connCLIPw, out, errw)
	}
	if _, _, errw := runCLITest(t, "", "conn", "edit", "dg-stg", "--host", "h", "-p", dir); !strings.Contains(errw, dataGripRefusal) {
		t.Fatalf("DataGrip 안내: %q", errw)
	}
}

func TestConnCLI_edit_는_password_stdin_으로_비밀번호를_바꾸고_주지_않으면_그대로_둔다(t *testing.T) {
	dir := connCLIProject(t)
	if code, _, errw := runCLITest(t, "", "conn", "add", "mine", "--url", "redis://:"+connCLIPw+"@127.0.0.1:1/0", "-p", dir, "--force"); code != exitOK {
		t.Fatalf("add: %q", errw)
	}
	code, out, errw := runCLITest(t, "", "conn", "edit", "mine", "--db", "3", "-p", dir, "--force")
	if code != exitOK || !strings.Contains(out, "고침: mine (db) · Redis 127.0.0.1:1/3") {
		t.Fatalf("code=%d out=%q err=%q", code, out, errw)
	}
	if env := readEnvFile(t, dir); !strings.Contains(env, "REDIS_MINE_PASSWORD="+connCLIPw+"\n") {
		t.Fatalf("비밀번호를 그대로 둔다: %q", env)
	}
	code, out, errw = runCLITest(t, "new-Pw-9\n", "conn", "edit", "mine", "--password-stdin", "-p", dir, "--force")
	if code != exitOK || strings.Contains(out+errw, "new-Pw-9") {
		t.Fatalf("code=%d out=%q err=%q", code, out, errw)
	}
	if env := readEnvFile(t, dir); !strings.Contains(env, "REDIS_MINE_PASSWORD=new-Pw-9\n") || strings.Contains(env, connCLIPw) {
		t.Fatalf("끝 줄바꿈을 떼고 바꾼다: %q", env)
	}
	if code, _, errw := runCLITest(t, "", "conn", "edit", "mine", "-p", dir); code != exitUsage || !strings.Contains(errw, "고칠 값이 없음") {
		t.Fatalf("고칠 값 없음: %d %q", code, errw)
	}
	if code, _, _ := runCLITest(t, "", "conn", "edit", "mine", "--password-stdin", "-p", dir, "--force"); code != exitUsage {
		t.Fatal("표준 입력이 없으면 2")
	}
	if code, _, errw := runCLITest(t, "", "conn", "edit", "mine", "--host", "db.prod.internal", "-p", dir, "--force"); code != exitRefused {
		t.Fatalf("prod 호스트는 --prod 가 필요: %d %q", code, errw)
	}
}

func TestConnCLI_rm_은_터미널이_아니면_yes_가_필요하고_터미널이면_묻는다(t *testing.T) {
	dir := connCLIProject(t)
	if code, _, errw := runCLITest(t, "", "conn", "add", "mine", "--url", "postgres://u:"+connCLIPw+"@127.0.0.1:1/d", "-p", dir, "--force"); code != exitOK {
		t.Fatalf("add: %q", errw)
	}
	if code, _, errw := runCLITest(t, "y\n", "conn", "rm", "mine", "-p", dir); code != exitUsage || !strings.Contains(errw, "--yes") {
		t.Fatalf("파이프 stdin 은 --yes 필요: %d %q", code, errw)
	}
	var out, errw bytes.Buffer
	if code := runConnCLI(context.Background(), []string{"rm", "mine", "-p", dir}, strings.NewReader("n\n"), true, &out, &errw); code != exitErr || !strings.Contains(errw.String(), "mine 접속을 지울까요(.env 비밀번호도) y/N") {
		t.Fatalf("n 이면 그대로: %d %q", code, errw.String())
	}
	if code := runConnCLI(context.Background(), []string{"rm", "mine", "-p", dir}, strings.NewReader("y\n"), true, &out, &errw); code != exitOK || !strings.Contains(out.String(), "지움: mine (db)") {
		t.Fatalf("y 면 지움: %d %q", code, errw.String())
	}
	if env := readEnvFile(t, dir); strings.Contains(env, "PG_MINE_PASSWORD") {
		t.Fatalf(".env: %q", env)
	}
	if code, _, _ := runCLITest(t, "", "conn", "add", "m2", "--url", "postgres://127.0.0.1:1/d", "-p", dir, "--force"); code != exitOK {
		t.Fatal("add")
	}
	if code, _, _ := runCLITest(t, "", "conn", "rm", "m2", "--yes", "-p", dir); code != exitOK {
		t.Fatal("--yes 면 묻지 않고 지움")
	}
}

func TestConnCLI_test_는_실패하면_1_이고_비밀번호를_찍지_않는다(t *testing.T) {
	dir := connCLIProject(t)
	code, out, errw := runCLITest(t, "", "conn", "test", "--url", "postgres://u:"+connCLIPw+"@127.0.0.1:1/d", "-p", dir)
	if code != exitErr || !strings.Contains(errw, "접속 실패") {
		t.Fatalf("code=%d err=%q", code, errw)
	}
	noSecret(t, connCLIPw, out, errw)
	if code, _, errw := runCLITest(t, "", "conn", "test", "dg-stg", "-p", dir); code != exitErr {
		t.Fatalf("DataGrip 접속도 시험한다: %d %q", code, errw)
	}
	if code, _, _ := runCLITest(t, "", "conn", "test", "--url", "postgres://db.prod.internal/d", "-p", dir); code != exitRefused {
		t.Fatal("prod 는 --prod 가 필요")
	}
}

func TestConnCLI_도움말(t *testing.T) {
	for _, args := range [][]string{{"help", "conn"}, {"conn", "--help"}} {
		code, out, _ := runCLITest(t, "", args...)
		if code != exitOK || !strings.Contains(out, "db conn add <이름> --url URL") || !strings.Contains(out, "--password-stdin") {
			t.Fatalf("%v: %d %q", args, code, out)
		}
	}
	if _, out, _ := runCLITest(t, "", "help"); !strings.Contains(out, "conn") {
		t.Fatal("db help 목록에 conn")
	}
}

func TestConnCLI_로컬_컨테이너에_추가하고_시험한다(t *testing.T) {
	raw, addr := os.Getenv("DBTUI_TEST_PG_URL"), os.Getenv("DBTUI_TEST_REDIS_ADDR")
	if raw == "" || addr == "" {
		t.Skip("DBTUI_TEST_PG_URL / DBTUI_TEST_REDIS_ADDR not set")
	}
	dir := connCLIProject(t)
	pu, err := url.Parse(raw)
	must(t, err)
	pw, _ := pu.User.Password()
	code, out, errw := runCLITest(t, "", "conn", "add", "local-pg", "--url", raw, "-p", dir)
	if code != exitOK || !strings.Contains(out, "접속됨 (PostgreSQL ") {
		t.Fatalf("code=%d out=%q err=%q", code, out, errw)
	}
	noSecret(t, pw, out, errw)
	if code, out, errw := runCLITest(t, "", "conn", "add", "local-redis", "--url", "redis://"+addr+"/0", "-p", dir); code != exitOK || !strings.Contains(out, "접속됨 (") {
		t.Fatalf("redis: %d %q %q", code, out, errw)
	}
	if code, out, _ := runCLITest(t, "", "conn", "test", "local-pg", "-p", dir); code != exitOK || !strings.HasPrefix(out, "접속됨 (PostgreSQL ") {
		t.Fatalf("test: %d %q", code, out)
	}
	if code, out, _ := runCLITest(t, "", "query", "-c", "local-pg", "-p", dir, "select 1 as one"); code != exitOK || !strings.Contains(out, "one") {
		t.Fatalf("저장한 접속으로 query 가 돈다: %d %q", code, out)
	}
	// 틀린 비밀번호로 바꾸면 시험이 실패해 저장하지 않는다
	if code, _, errw := runCLITest(t, "wrong-pw\n", "conn", "edit", "local-pg", "--password-stdin", "-p", dir); code != exitErr || strings.Contains(errw, "wrong-pw") {
		t.Fatalf("edit: %d %q", code, errw)
	}
	if env := readEnvFile(t, dir); !strings.Contains(env, "PG_LOCAL_PG_PASSWORD="+url.QueryEscape(pw)) {
		t.Fatalf("그대로: %q", env)
	}
}

func TestConnCLI_이름_자리의_URL_은_찾거나_되풀이하기_전에_거절한다(t *testing.T) {
	dir := connCLIProject(t)
	u := "postgres://u:" + connCLIPw + "@127.0.0.1:1/d"
	for _, sub := range []string{"test", "edit", "rm", "add"} {
		code, out, errw := runCLITest(t, "", "conn", sub, u, "-p", dir)
		if code != exitUsage || !strings.Contains(errw, "접속 이름은 영숫자·-·_ 만 받음") {
			t.Errorf("%s: code=%d err=%q", sub, code, errw)
		}
		noSecret(t, connCLIPw, out, errw)
		if strings.Contains(out+errw, "127.0.0.1") {
			t.Errorf("%s: URL 을 되풀이한다: %q", sub, errw)
		}
	}
	if code, out, errw := runCLITest(t, "", "conn", u); code != exitUsage || strings.Contains(out+errw, connCLIPw) {
		t.Fatalf("하위 명령 자리: %d %q", code, errw)
	}
}

func TestConnCLI_add_는_URL_의_sslmode_를_접속_파일에_남긴다(t *testing.T) {
	dir := connCLIProject(t)
	u := "postgres://ro:" + connCLIPw + "@10.0.0.9:1/app?sslmode=prefer"
	if code, _, errw := runCLITest(t, "", "conn", "add", "app-stg3", "--url", u, "-p", dir, "--force"); code != exitOK {
		t.Fatalf("code=%d err=%q", code, errw)
	}
	raw, err := os.ReadFile(filepath.Join(dir, ".dbtui", "connections.json"))
	if err != nil || !strings.Contains(string(raw), `"sslmode": "prefer"`) {
		t.Fatalf("sslmode 를 버리면 TLS 를 받지 않는 서버에 다시 못 붙는다: %s %v", raw, err)
	}
}
