package db

import (
	"strings"
	"testing"
)

func TestLoadProject는_Athena_작업_그룹과_결과_위치를_읽고_비밀값_키가_없다(t *testing.T) {
	dir := writeFixture(t, false)
	writeConnsJSON(t, dir, `[
 {"uuid":"a-1","name":"athena-prod","group":"data","kind":"athena","region":"ap-northeast-2","profile":"team","workgroup":"primary","output":"s3://bucket/prefix/"},
 {"uuid":"a-2","name":"athena_prod","kind":"athena","region":"ap-northeast-2"}]`)
	p, err := LoadProject(dir)
	must(t, err)
	if len(p.Warnings) != 0 {
		t.Fatalf("비밀값 키가 없어 a-b·a_b 도 겹치지 않는다: %v", p.Warnings)
	}
	var c Conn
	for _, x := range p.Conns {
		if x.Name == "athena-prod" {
			c = x
		}
	}
	if c.Kind != KindAthena || c.Workgroup != "primary" || c.Output != "s3://bucket/prefix/" || c.Profile != "team" || c.Env() != "prod" {
		t.Fatalf("athena: %+v", c)
	}
	if PasswordKey(KindAthena, "athena-prod") != "" || KindAthena.IsHTTP() || !KindAthena.IsSource() {
		t.Fatal("Athena 는 url·비밀값이 없는 저장소다")
	}
	// 고쳐 써도 작업 그룹·결과 위치가 남는다
	c.Workgroup = "analysts"
	if _, err := UpdateConn(dir, "athena-prod", c); err != nil {
		t.Fatal(err)
	}
	p, err = LoadProject(dir)
	must(t, err)
	for _, x := range p.Conns {
		if x.Name == "athena-prod" && (x.Workgroup != "analysts" || x.Output != "s3://bucket/prefix/") {
			t.Fatalf("고친 뒤: %+v", x)
		}
	}
}

func TestValidateConn은_Athena_리전_결과_위치_url_을_본다(t *testing.T) {
	if err := ValidateConn(Conn{Name: "a", Kind: KindAthena, Region: "r", Output: "s3://b/p/"}); err != nil {
		t.Fatal(err)
	}
	bad := map[string]Conn{
		"region is":      {Name: "a", Kind: KindAthena},
		"s3://":          {Name: "a", Kind: KindAthena, Region: "r", Output: "bucket/p"},
		"does not use":   {Name: "a", Kind: KindAthena, Region: "r", URL: "https://athena.internal"},
		"athena auth mu": {Name: "a", Kind: KindAthena, Region: "r", Auth: "sigv4"},
	}
	for want, c := range bad {
		if err := ValidateConn(c); err == nil || !strings.Contains(err.Error(), want) {
			t.Errorf("%+v: %v, want %q", c, err, want)
		}
	}
}
