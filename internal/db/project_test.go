package db

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

const fixtureDataSources = `<?xml version="1.0" encoding="UTF-8"?>
<project version="4">
  <component name="DataSourceManagerImpl" format="xml" multifile-model="true">
    <data-source source="LOCAL" name="app-stg" group="app" uuid="u-pg">
      <driver-ref>postgresql</driver-ref>
      <jdbc-url>jdbc:postgresql://db.stg.example.internal:5432/app_stg</jdbc-url>
    </data-source>
    <data-source source="LOCAL" name="app-stg-redis" uuid="u-redis">
      <driver-ref>redis</driver-ref>
      <jdbc-url>jdbc:redis://redis.stg.example.internal:6379/0</jdbc-url>
    </data-source>
    <data-source source="LOCAL" name="legacy-mysql" uuid="u-mysql">
      <driver-ref>mysql.8</driver-ref>
      <jdbc-url>jdbc:mysql://localhost:3306/x</jdbc-url>
    </data-source>
  </component>
</project>`

const fixtureLocal = `<?xml version="1.0" encoding="UTF-8"?>
<project version="4">
  <component name="dataSourceStorageLocal">
    <data-source name="app-stg" uuid="u-pg">
      <user-name>app_stg_admin</user-name>
    </data-source>
  </component>
</project>`

const fixtureMapping = `<?xml version="1.0" encoding="UTF-8"?>
<project version="4">
  <component name="DataSourcePerFileMappings">
    <file url="file://$PROJECT_DIR$/queries/app/Query.sql" value="u-pg" />
  </component>
</project>`

func writeFixture(t *testing.T, withEnv bool) string {
	t.Helper()
	dir := t.TempDir()
	idea := filepath.Join(dir, ".idea")
	must(t, os.MkdirAll(idea, 0o755))
	must(t, os.WriteFile(filepath.Join(idea, "dataSources.xml"), []byte(fixtureDataSources), 0o644))
	must(t, os.WriteFile(filepath.Join(idea, "dataSources.local.xml"), []byte(fixtureLocal), 0o644))
	must(t, os.WriteFile(filepath.Join(idea, "data_source_mapping.xml"), []byte(fixtureMapping), 0o644))
	if withEnv {
		env := "# 주석\nPG_APP_STG_PASSWORD=p%40ss%2Fword\nREDIS_APP_STG_REDIS_PASSWORD=r-secret\nAPP_AES_KEY=k123\nAPP_AES_IV=00ff\n"
		must(t, os.WriteFile(filepath.Join(dir, ".env"), []byte(env), 0o600))
	}
	return dir
}

func must(t *testing.T, err error) {
	t.Helper()
	if err != nil {
		t.Fatal(err)
	}
}

func TestLoadProject는_DataGrip_파일에서_접속을_만든다(t *testing.T) {
	p, err := LoadProject(writeFixture(t, true))
	must(t, err)
	if len(p.Conns) != 2 {
		t.Fatalf("postgresql·redis 만 남아야 한다: %d개", len(p.Conns))
	}
	pg := p.Conns[0]
	want := Conn{Name: "app-stg", UUID: "u-pg", Group: "app", Kind: KindPG, Host: "db.stg.example.internal", Port: 5432, DB: "app_stg", User: "app_stg_admin", Password: "p@ss/word"}
	if pg != want {
		t.Fatalf("pg 접속\n got %+v\nwant %+v", pg, want)
	}
	rd := p.Conns[1]
	if rd.Kind != KindRedis || rd.DB != "0" || rd.Password != "r-secret" || rd.User != "" {
		t.Fatalf("redis 접속이 다르다: %+v", rd)
	}
	if p.Mapping["queries/app/Query.sql"] != "u-pg" {
		t.Fatalf("파일 매핑이 없다: %v", p.Mapping)
	}
	if got := p.Groups(); len(got) != 2 || got[0] != "app" || got[1] != "기타" {
		t.Fatalf("그룹: %v", got)
	}
	if len(p.AESEnv) != 2 || p.AESEnv["APP_AES_KEY"] != "k123" || p.AESEnv["APP_AES_IV"] != "00ff" {
		t.Fatalf("AES 키·IV 만 모은다: %d개", len(p.AESEnv))
	}
}

func TestLoadProject는_env가_없어도_목록은_만든다(t *testing.T) {
	p, err := LoadProject(writeFixture(t, false))
	must(t, err)
	if len(p.Conns) != 2 || p.Conns[0].Password != "" {
		t.Fatalf("비밀번호 없이 목록만 있어야 한다: %+v", p.Conns)
	}
}

func TestLoadProject는_dataSources_없으면_오류(t *testing.T) {
	_, err := LoadProject(t.TempDir())
	if err == nil || !strings.HasPrefix(err.Error(), "dataSources.xml not found") {
		t.Fatalf("오류가 달라야 한다: %v", err)
	}
}

func TestConnEnv_이름으로_환경을_가른다(t *testing.T) {
	cases := map[string]string{"shop-prod": "prod", "Shop-PROD": "prod", "shop-stg-read": "stg", "X-STG": "stg", "app-local": "local", "shop-prd": "prod", "App-PRD-redis": "prod"}
	for name, want := range cases {
		if got := (Conn{Name: name}).Env(); got != want {
			t.Errorf("%s: got %s want %s", name, got, want)
		}
	}
}

func TestConnEnv_호스트에_prod_토큰이_있으면_이름과_상관없이_prod(t *testing.T) {
	cases := []struct{ name, host, want string }{
		{"session-redis", "redis01.prod.example.internal", "prod"},
		{"chat-main", "prod-app-main.cluster-x.example.com", "prod"},
		{"x-stg", "db.PRD.local", "prod"},
		{"x-stg", "write.db.stg.example.internal", "stg"},
		{"x-local", "producer.local", "local"},
		{"x-local", "reprod-1.local", "local"},
		{"x-local", "localhost", "local"},
	}
	for _, c := range cases {
		if got := (Conn{Name: c.name, Host: c.host}).Env(); got != c.want {
			t.Errorf("%s %s: got %s want %s", c.name, c.host, got, c.want)
		}
	}
}

func TestEnvName_영숫자_외는_밑줄_대문자(t *testing.T) {
	if got := EnvName("app-stg-redis"); got != "APP_STG_REDIS" {
		t.Fatal(got)
	}
}

func TestLoadProject는_JDBC_를_못_읽은_접속은_빼고_알린다(t *testing.T) {
	dir := writeFixture(t, false)
	broken := strings.Replace(fixtureDataSources, "jdbc:redis://redis.stg.example.internal:6379/0", "redis-without-jdbc", 1)
	must(t, os.WriteFile(filepath.Join(dir, ".idea", "dataSources.xml"), []byte(broken), 0o644))
	p, err := LoadProject(dir)
	must(t, err)
	if len(p.Conns) != 1 || len(p.Warnings) != 1 || !strings.HasPrefix(p.Warnings[0], "app-stg-redis") {
		t.Fatalf("conns=%d warnings=%v", len(p.Conns), p.Warnings)
	}
}

func TestLoadProject는_선택_파일이_깨졌으면_오류(t *testing.T) {
	dir := writeFixture(t, false)
	must(t, os.WriteFile(filepath.Join(dir, ".idea", "dataSources.local.xml"), []byte("<project><broken"), 0o644))
	if _, err := LoadProject(dir); err == nil || !strings.Contains(err.Error(), "dataSources.local.xml") {
		t.Fatalf("깨진 local.xml 은 오류여야 한다: %v", err)
	}
}

func FuzzLoadProject는_JDBC_URL_이_깨져도_패닉없이_비밀번호를_싣지_않는다(f *testing.F) {
	for _, s := range []string{"jdbc:postgresql://h:5432/db", "jdbc:postgresql://u:pw@h/db", "jdbc:redis://[::1]:6379/0", "jdbc:postgresql://h:99999999999999999999/db", "", "jdbc:postgresql://h:5432/db?ssl=true"} {
		f.Add(s)
	}
	f.Fuzz(func(t *testing.T, jdbc string) {
		if strings.ContainsAny(jdbc, "<>&\"'") {
			return
		}
		dir := writeFixture(t, true)
		ds := strings.Replace(fixtureDataSources, "jdbc:postgresql://db.stg.example.internal:5432/app_stg", jdbc, 1)
		must(t, os.WriteFile(filepath.Join(dir, ".idea", "dataSources.xml"), []byte(ds), 0o644))
		p, err := LoadProject(dir)
		if err != nil {
			return
		}
		for _, w := range p.Warnings {
			if strings.Contains(w, "p@ss/word") {
				t.Fatalf("경고에 비밀번호: %q", w)
			}
		}
		for _, c := range p.Conns {
			if c.Port < 0 || c.Port > 65535 || strings.Contains(c.Host, "@") {
				t.Fatalf("잘못된 접속이 목록에 들어갔다: %v", c)
			}
		}
	})
}
