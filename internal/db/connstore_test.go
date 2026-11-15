package db

import (
	"context"
	"encoding/json"
	"errors"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func writeConnsJSON(t *testing.T, dir, body string) {
	t.Helper()
	must(t, os.MkdirAll(filepath.Join(dir, ".dbtui"), 0o755))
	must(t, os.WriteFile(connsPath(dir), []byte(body), 0o644))
}

func connNames(p *Project) []string {
	out := make([]string, len(p.Conns))
	for i, c := range p.Conns {
		out[i] = c.Name
	}
	return out
}

func TestLoadProject는_connections_json_접속을_idea_뒤에_붙이고_db_표시를_단다(t *testing.T) {
	dir := writeFixture(t, false)
	writeConnsJSON(t, dir, `[{"uuid":"d-1","name":"app-stg2","group":"app","kind":"postgresql","host":"db2.internal","port":5433,"db":"app","user":"ro"}]`)
	must(t, os.WriteFile(filepath.Join(dir, ".env"), []byte("PG_APP_STG2_PASSWORD=a%40b\n"), 0o600))
	p, err := LoadProject(dir)
	must(t, err)
	if got := strings.Join(connNames(p), ","); got != "app-stg,app-stg-redis,app-stg2" {
		t.Fatalf("순서: %s", got)
	}
	c := p.Conns[2]
	want := Conn{Name: "app-stg2", UUID: "d-1", Group: "app", Kind: KindPG, Host: "db2.internal", Port: 5433, DB: "app", User: "ro", Password: "a@b", Source: SourceDB}
	if c != want {
		t.Fatalf("\n got %+v\nwant %+v", c, want)
	}
	if p.Conns[0].Source != "" {
		t.Fatal(".idea 접속은 Source 가 비어야 한다")
	}
}

func TestLoadProject는_idea_와_이름이_겹치는_db_접속을_빼고_경고한다(t *testing.T) {
	dir := writeFixture(t, false)
	writeConnsJSON(t, dir, `[{"uuid":"d-1","name":"app-stg","kind":"postgresql","host":"h"},{"uuid":"d-2","name":"ok","kind":"redis","host":"h"}]`)
	p, err := LoadProject(dir)
	must(t, err)
	if got := strings.Join(connNames(p), ","); got != "app-stg,app-stg-redis,ok" {
		t.Fatalf("겹친 접속을 빼야 한다: %s", got)
	}
	if len(p.Warnings) != 1 || !strings.HasPrefix(p.Warnings[0], "app-stg:") {
		t.Fatalf("경고: %v", p.Warnings)
	}
}

func TestLoadProject는_깨진_connections_json_이면_오류(t *testing.T) {
	dir := writeFixture(t, false)
	writeConnsJSON(t, dir, `[{`)
	if _, err := LoadProject(dir); err == nil || !strings.Contains(err.Error(), "connections.json") {
		t.Fatalf("오류여야 한다: %v", err)
	}
}

func TestAddConn은_json_끝에_더하고_env_를_0600으로_만든다(t *testing.T) {
	dir := writeFixture(t, false)
	c, err := AddConn(dir, Conn{Name: "a1", Group: "g", Kind: KindPG, Host: "h1", Port: 5432, DB: "d", User: "u", Password: "p w&/"})
	must(t, err)
	if c.UUID == "" || c.Source != SourceDB {
		t.Fatalf("uuid·source: %+v", c)
	}
	_, err = AddConn(dir, Conn{Name: "b2", Kind: KindRedis, Host: "h2", DB: "0"})
	must(t, err)
	var es []connEntry
	must(t, json.Unmarshal([]byte(readFile(t, connsPath(dir))), &es))
	if len(es) != 2 || es[0].Name != "a1" || es[1].Name != "b2" {
		t.Fatalf("순서: %+v", es)
	}
	if !strings.Contains(readFile(t, connsPath(dir)), "\n  {\n    \"uuid\"") {
		t.Fatalf("2칸 들여쓰기: %s", readFile(t, connsPath(dir)))
	}
	if strings.Contains(readFile(t, connsPath(dir)), "p w") {
		t.Fatal("json 에 비밀번호가 들어갔다")
	}
	env := readFile(t, filepath.Join(dir, ".env"))
	if env != "PG_A1_PASSWORD="+url.QueryEscape("p w&/")+"\n" {
		t.Fatalf(".env: %q", env)
	}
	st, err := os.Stat(filepath.Join(dir, ".env"))
	must(t, err)
	if st.Mode().Perm() != 0o600 {
		t.Fatalf("새 .env 권한: %v", st.Mode().Perm())
	}
	p, err := LoadProject(dir)
	must(t, err)
	if p.Conns[2].Password != "p w&/" {
		t.Fatalf("다시 읽은 비밀번호가 다르다")
	}
}

func TestAddConn은_이름이_겹치거나_env_키가_같으면_거절한다(t *testing.T) {
	dir := writeFixture(t, false)
	if _, err := AddConn(dir, Conn{Name: "app-stg", Kind: KindPG, Host: "h"}); err == nil {
		t.Fatal(".idea 이름과 겹치면 거절")
	}
	_, err := AddConn(dir, Conn{Name: "a-b", Kind: KindPG, Host: "h"})
	must(t, err)
	if _, err := AddConn(dir, Conn{Name: "a_b", Kind: KindPG, Host: "h"}); err == nil || !strings.Contains(err.Error(), ".env key") {
		t.Fatalf("같은 .env 키는 거절: %v", err)
	}
	if _, err := AddConn(dir, Conn{Name: "bad name", Kind: KindPG, Host: "h"}); err == nil {
		t.Fatal("이름 검사")
	}
}

func TestUpdateConn은_이름을_바꾸면_env_키를_옮기고_다른_줄과_권한을_지킨다(t *testing.T) {
	dir := writeFixture(t, false)
	_, err := AddConn(dir, Conn{Name: "first", Kind: KindPG, Host: "h0"})
	must(t, err)
	_, err = AddConn(dir, Conn{Name: "x", Kind: KindPG, Host: "h", Password: "s3"})
	must(t, err)
	_, err = AddConn(dir, Conn{Name: "last", Kind: KindRedis, Host: "h9"})
	must(t, err)
	envPath := filepath.Join(dir, ".env")
	must(t, os.WriteFile(envPath, []byte("# 머리 주석\nOTHER=1\nPG_X_PASSWORD=s3\n\nAPP_AES_KEY=k\n"), 0o640))
	must(t, os.Chmod(envPath, 0o640))

	c, err := UpdateConn(dir, "x", Conn{Name: "y", Kind: KindPG, Host: "h2", Port: 6000})
	must(t, err)
	if got := readFile(t, envPath); got != "# 머리 주석\nOTHER=1\nPG_Y_PASSWORD=s3\n\nAPP_AES_KEY=k\n" {
		t.Fatalf("비밀번호를 옮기고 다른 줄은 그대로: %q", got)
	}
	st, _ := os.Stat(envPath)
	if st.Mode().Perm() != 0o640 {
		t.Fatalf("권한 유지: %v", st.Mode().Perm())
	}
	p, err := LoadProject(dir)
	must(t, err)
	if got := strings.Join(connNames(p), ","); got != "app-stg,app-stg-redis,first,y,last" {
		t.Fatalf("자리 유지: %s", got)
	}
	if p.Conns[3].UUID != c.UUID || p.Conns[3].Host != "h2" || p.Conns[3].Password != "s3" {
		t.Fatalf("고친 값: %+v", p.Conns[3])
	}

	_, err = UpdateConn(dir, "y", Conn{Name: "y", Kind: KindPG, Host: "h2", Password: "new"})
	must(t, err)
	if got := readFile(t, envPath); !strings.Contains(got, "PG_Y_PASSWORD=new\n") || strings.Count(got, "PG_Y") != 1 {
		t.Fatalf("새 비밀번호: %q", got)
	}
}

func TestUpdateConn_RemoveConn은_db_전용_접속만_고친다(t *testing.T) {
	dir := writeFixture(t, false)
	if _, err := UpdateConn(dir, "app-stg", Conn{Name: "app-stg", Kind: KindPG, Host: "h"}); err == nil {
		t.Fatal("DataGrip 접속 고치기는 오류")
	}
	if err := RemoveConn(dir, "app-stg"); err == nil {
		t.Fatal("DataGrip 접속 지우기는 오류")
	}
}

func TestLoadProject는_uuid_없는_줄과_env_키가_겹치는_뒤_줄을_빼고_경고한다(t *testing.T) {
	dir := writeFixture(t, false)
	writeConnsJSON(t, dir, `[{"name":"no-uuid","kind":"postgresql","host":"h"},`+
		`{"uuid":"d-1","name":"a-b","kind":"postgresql","host":"h"},{"uuid":"d-2","name":"a_b","kind":"postgresql","host":"h"},`+
		`{"uuid":"d-3","name":"a_b","kind":"redis","host":"h"}]`)
	p, err := LoadProject(dir)
	must(t, err)
	if got := strings.Join(connNames(p), ","); got != "app-stg,app-stg-redis,a-b,a_b" {
		t.Fatalf("남는 접속: %s", got)
	}
	if len(p.Warnings) != 2 || !strings.HasPrefix(p.Warnings[0], "no-uuid: missing uuid") || !strings.HasPrefix(p.Warnings[1], "a_b: same .env password key") {
		t.Fatalf("경고: %v", p.Warnings)
	}
}

func TestUpdateConn은_env_쓰기가_실패하면_옛_키를_남기고_ErrEnvNotUpdated_를_돌려준다(t *testing.T) {
	if os.Geteuid() == 0 {
		t.Skip("root 는 읽기 전용 폴더에도 쓴다")
	}
	dir := writeFixture(t, false)
	_, err := AddConn(dir, Conn{Name: "x", Kind: KindPG, Host: "h", Password: "keep-me"})
	must(t, err)
	must(t, os.Chmod(dir, 0o555)) // .env 임시 파일을 못 만든다. .dbtui 폴더는 그대로 쓸 수 있다
	t.Cleanup(func() { _ = os.Chmod(dir, 0o755) })
	_, err = UpdateConn(dir, "x", Conn{Name: "y", Kind: KindPG, Host: "h"})
	if !errors.Is(err, ErrEnvNotUpdated) || !strings.Contains(err.Error(), "PG_X_PASSWORD to PG_Y_PASSWORD") {
		t.Fatalf("오류: %v", err)
	}
	if env := readFile(t, filepath.Join(dir, ".env")); env != "PG_X_PASSWORD=keep-me\n" {
		t.Fatalf("옛 키가 남아야 한다: %q", env)
	}
	if !strings.Contains(readFile(t, connsPath(dir)), `"name": "y"`) {
		t.Fatal("접속은 저장된다")
	}
}

func TestSetEnv는_심볼릭_링크면_가리키는_파일에_쓰고_폴더면_거절하며_CRLF_를_따른다(t *testing.T) {
	dir, real := t.TempDir(), filepath.Join(t.TempDir(), "real.env")
	must(t, os.WriteFile(real, []byte("A=1\r\n"), 0o600))
	must(t, os.Symlink(real, filepath.Join(dir, ".env")))
	must(t, setEnv(dir, []string{"B"}, "B=2"))
	if st, err := os.Lstat(filepath.Join(dir, ".env")); err != nil || st.Mode()&os.ModeSymlink == 0 {
		t.Fatal("링크는 그대로")
	}
	if got := readFile(t, real); got != "A=1\r\nB=2\r\n" {
		t.Fatalf("가리키는 파일에 CRLF 로: %q", got)
	}

	dir2 := t.TempDir()
	must(t, os.Mkdir(filepath.Join(dir2, "envdir"), 0o755))
	must(t, os.Symlink(filepath.Join(dir2, "envdir"), filepath.Join(dir2, ".env")))
	if err := setEnv(dir2, []string{"B"}, "B=2"); err == nil || !strings.Contains(err.Error(), "is a directory") {
		t.Fatalf("폴더는 거절: %v", err)
	}
}

func TestRemoveConn은_json_줄과_env_비밀번호를_지우고_나머지는_둔다(t *testing.T) {
	dir := writeFixture(t, true)
	for _, n := range []string{"a", "b", "c"} {
		_, err := AddConn(dir, Conn{Name: n, Kind: KindRedis, Host: "h", Password: "pw-" + n})
		must(t, err)
	}
	must(t, RemoveConn(dir, "b"))
	p, err := LoadProject(dir)
	must(t, err)
	if got := strings.Join(connNames(p), ","); got != "app-stg,app-stg-redis,a,c" {
		t.Fatalf("남은 접속: %s", got)
	}
	env := readFile(t, filepath.Join(dir, ".env"))
	if strings.Contains(env, "REDIS_B_PASSWORD") || !strings.HasPrefix(env, "# 주석\nPG_APP_STG_PASSWORD=") || !strings.Contains(env, "REDIS_C_PASSWORD=pw-c") {
		t.Fatalf(".env: %q", env)
	}
	must(t, RemoveConn(dir, "a"))
	must(t, RemoveConn(dir, "c"))
	if got := strings.TrimSpace(readFile(t, connsPath(dir))); got != "[]" {
		t.Fatalf("다 지우면 빈 배열: %s", got)
	}
}

func TestParseConnURL은_접속_값과_비밀번호로_나눈다(t *testing.T) {
	cases := []struct {
		raw, pw string
		want    Conn
	}{
		{"postgres://app_ro:pw@db.stg.example.internal:5432/app", "pw", Conn{Kind: KindPG, Host: "db.stg.example.internal", Port: 5432, DB: "app", User: "app_ro"}},
		{"postgresql://u@h/d", "", Conn{Kind: KindPG, Host: "h", Port: 5432, DB: "d", User: "u"}},
		{"postgres://u:p%40ss%2Fw%3Ard@h:6543/d?sslmode=require", "p@ss/w:rd", Conn{Kind: KindPG, Host: "h", Port: 6543, DB: "d", User: "u"}},
		{"redis://:pw@cache:6380/2", "pw", Conn{Kind: KindRedis, Host: "cache", Port: 6380, DB: "2"}},
		{"rediss://default:x@cache.amazonaws.com", "x", Conn{Kind: KindRedis, Host: "cache.amazonaws.com", Port: 6379, User: "default"}},
		{"redis://[::1]:7000/0", "", Conn{Kind: KindRedis, Host: "::1", Port: 7000, DB: "0"}},
	}
	for _, c := range cases {
		got, pw, err := ParseConnURL(c.raw)
		if err != nil || got != c.want || pw != c.pw {
			t.Errorf("%s\n got %+v %q %v\nwant %+v %q", c.raw, got, pw, err, c.want, c.pw)
		}
	}
	for _, bad := range []string{"mysql://u:pw@h/d", "postgres://", "postgres://u:sekrit@h:99999/d", "redis://h/abc", "h:5432", "", "postgres://u:sekrit@:5432/d"} {
		_, _, err := ParseConnURL(bad)
		if err == nil {
			t.Errorf("%q 는 오류여야 한다", bad)
		} else if strings.Contains(err.Error(), "sekrit") {
			t.Errorf("오류에 비밀번호: %v", err)
		}
	}
}

func TestValidateConn은_이름을_영숫자_하이픈_밑줄만_받는다(t *testing.T) {
	for name, ok := range map[string]bool{"app-stg_2": true, "A": true, "": false, "a b": false, "a.b": false, "한글": false, "a/b": false} {
		err := ValidateConn(Conn{Name: name, Kind: KindPG, Host: "h"})
		if (err == nil) != ok {
			t.Errorf("%q: %v", name, err)
		}
	}
	if ValidateConn(Conn{Name: "r", Kind: KindRedis, Host: "h", DB: "x"}) == nil {
		t.Error("Redis DB 는 숫자")
	}
}

func TestRedisVersion은_Valkey_를_먼저_본다(t *testing.T) {
	if got := redisVersion("# Server\r\nredis_version:7.2.4\r\nvalkey_version:8.0.1\r\n"); got != "Valkey 8.0.1" {
		t.Fatal(got)
	}
	if got := redisVersion("redis_version:7.2.4\r\n"); got != "Redis 7.2.4" {
		t.Fatal(got)
	}
}

func TestRedisVersion은_버전_필드가_없으면_이름만_보인다(t *testing.T) {
	if got := redisVersion("# Server\r\nconnected_clients:1\r\n"); got != "Redis" {
		t.Fatalf("%q", got)
	}
}

func TestTestConn은_오류에_비밀번호를_싣지_않는다(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	// 닿지 않는 로컬 포트. 거절 오류에 비밀번호가 섞여도 지워야 한다
	_, err := TestConn(ctx, Conn{Name: "x", Kind: KindPG, Host: "127.0.0.1", Port: 1, User: "u", Password: "Sup3r-Sekrit"})
	if err == nil || strings.Contains(err.Error(), "Sup3r-Sekrit") {
		t.Fatalf("오류: %v", err)
	}
}

func TestTestConn은_로컬_컨테이너에서_서버_버전을_읽는다(t *testing.T) {
	raw, addr := os.Getenv("DBTUI_TEST_PG_URL"), os.Getenv("DBTUI_TEST_REDIS_ADDR")
	if raw == "" || addr == "" {
		t.Skip("DBTUI_TEST_PG_URL / DBTUI_TEST_REDIS_ADDR not set")
	}
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	c, pw, err := ParseConnURL(raw)
	must(t, err)
	c.Password = pw
	v, err := TestConn(ctx, c)
	if err != nil || !strings.HasPrefix(v, "PostgreSQL ") || len(v) <= len("PostgreSQL ") {
		t.Fatalf("pg: %q %v", v, err)
	}
	c.Password = "wrong-Pw-123"
	if _, err := TestConn(ctx, c); err == nil || strings.Contains(err.Error(), "wrong-Pw-123") {
		t.Fatalf("틀린 비밀번호: %v", err)
	}
	rc, _, err := ParseConnURL("redis://" + addr + "/0")
	must(t, err)
	v, err = TestConn(ctx, rc)
	if err != nil || !(strings.HasPrefix(v, "Redis ") || strings.HasPrefix(v, "Valkey ")) {
		t.Fatalf("redis: %q %v", v, err)
	}
}

func TestForget은_Redis_연결도_닫아_다음에_고친_접속으로_새로_연다(t *testing.T) {
	addr := os.Getenv("DBTUI_TEST_REDIS_ADDR")
	if addr == "" {
		t.Skip("DBTUI_TEST_REDIS_ADDR not set")
	}
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	c, _, err := ParseConnURL("redis://" + addr + "/0")
	must(t, err)
	c.UUID, c.Name = "r", "r"
	cs := NewClients()
	defer cs.Close()
	r1, err := cs.Redis(ctx, c)
	must(t, err)
	must(t, cs.Forget(ctx, "r"))
	if r1.cl.Ping(ctx).Err() == nil {
		t.Fatal("옛 클라이언트는 닫힌다")
	}
	c.DB = "1"
	r2, err := cs.Redis(ctx, c)
	must(t, err)
	if r2 == r1 || r2.opts.DB != 1 {
		t.Fatalf("새 설정으로 연다: db=%d", r2.opts.DB)
	}
}
