package db

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestLoadProject는_읽기_전용_저장소를_connections_json_에서만_읽고_종류별_env_키로_비밀값을_채운다(t *testing.T) {
	dir := writeFixture(t, false)
	writeConnsJSON(t, dir, `[
 {"uuid":"s-1","name":"search-stg","kind":"opensearch","url":"https://search.stg.example.internal","auth":"basic","user":"ro"},
 {"uuid":"s-2","name":"logs","kind":"loki","url":"http://loki:3100","auth":"basic","user":"u"},
 {"uuid":"s-3","name":"metrics","kind":"prometheus","url":"http://prom:9090"},
 {"uuid":"s-4","name":"graf","kind":"grafana","url":"https://grafana.example.com","auth":"token"},
 {"uuid":"s-5","name":"ddb","kind":"dynamodb","region":"ap-northeast-2","profile":"stg"},
 {"uuid":"s-6","name":"ddb2","kind":"dynamodb","region":"ap-northeast-2"}]`)
	env := "OS_SEARCH_STG_PASSWORD=p%40ss\nLOKI_LOGS_PASSWORD=lk\nGRAFANA_GRAF_TOKEN=glsa_x\n"
	must(t, os.WriteFile(filepath.Join(dir, ".env"), []byte(env), 0o600))
	p, err := LoadProject(dir)
	must(t, err)
	if len(p.Warnings) != 0 {
		t.Fatalf("경고 없이 읽는다(dynamodb 둘은 비밀값 키가 없어 겹치지 않음): %v", p.Warnings)
	}
	got := map[string]Conn{}
	for _, c := range p.Conns {
		got[c.Name] = c
	}
	if c := got["search-stg"]; c.Kind != KindOpenSearch || c.Password != "p@ss" || c.Auth != "basic" || c.URL != "https://search.stg.example.internal" {
		t.Fatalf("opensearch: %+v pw=%q", c, c.Password)
	}
	if got["logs"].Password != "lk" || got["graf"].Password != "glsa_x" || got["metrics"].Password != "" {
		t.Fatal("loki·grafana 비밀값")
	}
	if c := got["ddb"]; c.Region != "ap-northeast-2" || c.Profile != "stg" || c.Password != "" {
		t.Fatalf("dynamodb: %+v", c)
	}
	if strings.Contains(got["search-stg"].String(), "p@ss") {
		t.Fatal("String 에 비밀번호가 나오면 안 된다")
	}
}

func TestPasswordKey는_종류별_접두와_grafana_토큰_dynamodb_없음(t *testing.T) {
	cases := map[Kind]string{KindPG: "PG_A_B_PASSWORD", KindRedis: "REDIS_A_B_PASSWORD", KindOpenSearch: "OS_A_B_PASSWORD",
		KindLoki: "LOKI_A_B_PASSWORD", KindPrometheus: "PROM_A_B_PASSWORD", KindGrafana: "GRAFANA_A_B_TOKEN", KindDynamoDB: ""}
	for k, want := range cases {
		if got := PasswordKey(k, "a-b"); got != want {
			t.Errorf("%s: %q, want %q", k, got, want)
		}
	}
}

func TestValidateConn은_종류마다_필요한_칸을_본다(t *testing.T) {
	ok := []Conn{
		{Name: "a", Kind: KindOpenSearch, URL: "https://h", Auth: "sigv4", Region: "r"},
		{Name: "a", Kind: KindLoki, URL: "http://h:3100"},
		{Name: "a", Kind: KindGrafana, URL: "https://g", Auth: "token"},
		{Name: "a", Kind: KindDynamoDB, Region: "r"},
		{Name: "a", Kind: KindDynamoDB, Region: "r", URL: "http://localhost:8000"},
	}
	for _, c := range ok {
		if err := ValidateConn(c); err != nil {
			t.Errorf("%+v: %v", c, err)
		}
	}
	bad := map[string]Conn{
		"url is required":   {Name: "a", Kind: KindPrometheus},
		"http(s)":           {Name: "a", Kind: KindLoki, URL: "ftp://h"},
		"user or password":  {Name: "a", Kind: KindLoki, URL: "http://u:pw@h"},
		"region":            {Name: "a", Kind: KindOpenSearch, URL: "https://h", Auth: "sigv4"},
		"auth must be":      {Name: "a", Kind: KindLoki, URL: "http://h", Auth: "sigv4"},
		"region is":         {Name: "a", Kind: KindDynamoDB},
		"grafana auth must": {Name: "a", Kind: KindGrafana, URL: "https://g"},
	}
	for want, c := range bad {
		err := ValidateConn(c)
		if err == nil || !strings.Contains(err.Error(), want) {
			t.Errorf("%+v: %v, want %q", c, err, want)
		}
		if err != nil && strings.Contains(err.Error(), "pw@") {
			t.Error("오류에 URL 원문(비밀번호)을 싣지 않는다")
		}
	}
}

func TestEnv는_저장소_URL_호스트의_prod_마디도_본다(t *testing.T) {
	if e := (Conn{Name: "logs", Kind: KindLoki, URL: "https://loki.prod.example.internal"}).Env(); e != "prod" {
		t.Fatalf("URL 호스트 prod: %s", e)
	}
	if e := (Conn{Name: "logs", Kind: KindLoki, URL: "https://producer.example"}).Env(); e != "local" {
		t.Fatalf("마디가 통째로 같아야 한다: %s", e)
	}
}

func TestAddConn_UpdateConn_RemoveConn은_저장소_비밀값을_종류별_키로_쓰고_옮긴다(t *testing.T) {
	dir := writeFixture(t, false)
	envPath := filepath.Join(dir, ".env")
	must(t, os.WriteFile(envPath, []byte("# 주석\n\nOTHER=1\n"), 0o600))
	c, err := AddConn(dir, Conn{Name: "graf", Kind: KindGrafana, URL: "https://g", Auth: "token", Password: "glsa/1"})
	must(t, err)
	b, _ := os.ReadFile(envPath)
	if !strings.Contains(string(b), "GRAFANA_GRAF_TOKEN=glsa%2F1") {
		t.Fatalf(".env: %s", b)
	}
	raw, _ := os.ReadFile(connsPath(dir))
	if !strings.Contains(string(raw), `"url": "https://g"`) || !strings.Contains(string(raw), `"auth": "token"`) || strings.Contains(string(raw), "region") {
		t.Fatalf("connections.json: %s", raw)
	}
	// dynamodb 로 바꾸면 비밀값 줄을 지우고 주석·빈 줄은 그대로 둔다
	c.Kind, c.Auth, c.URL, c.Region, c.Password = KindDynamoDB, "", "", "r", ""
	_, err = UpdateConn(dir, "graf", c)
	must(t, err)
	b, _ = os.ReadFile(envPath)
	if strings.Contains(string(b), "GRAFANA") || !strings.Contains(string(b), "# 주석\n\nOTHER=1") {
		t.Fatalf("옮긴 뒤 .env: %q", b)
	}
	must(t, RemoveConn(dir, "graf"))
	b, _ = os.ReadFile(envPath)
	if string(b) != "# 주석\n\nOTHER=1\n" {
		t.Fatalf("키 없는 종류를 지워도 .env 는 그대로: %q", b)
	}
}

func TestParseHTTPURL은_사용자_정보를_떼어_낸다(t *testing.T) {
	u, user, pw, err := ParseHTTPURL(" https://ro:p%40ss@search.internal:9200/ ")
	if err != nil || u != "https://search.internal:9200" || user != "ro" || pw != "p@ss" {
		t.Fatalf("%q %q %q %v", u, user, pw, err)
	}
	if _, _, _, err := ParseHTTPURL("postgres://h"); err == nil {
		t.Fatal("http(s) 만 받는다")
	}
}
