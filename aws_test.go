package main

import (
	"context"
	"strings"
	"testing"

	"github.com/charmbracelet/x/ansi"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/history"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
)

// ─── aws 접속 (R67.1) ───

func TestAWS_편집기_실행은_보내지_않고_트리로_보라고_한다(t *testing.T) {
	f := newFake()
	registerFake(t, f)
	m := sourceModel(t)
	i := len(m.proj.Conns)
	m.proj.Conns = append(m.proj.Conns, db.Conn{Name: "aws-stg", UUID: "a1", Kind: db.KindAWS, Region: "ap-northeast-2", Source: db.SourceDB})
	m.setConn(m.cur(), i)
	editorRun(m, "anything")
	if runs, _, _ := f.snapshot(); len(runs) != 0 || m.status != msgTreeOnly || m.cur().running != nil {
		t.Fatalf("runs=%v status=%q", runs, m.status)
	}
	if es := m.hist.List(history.Filter{}); len(es) != 0 {
		t.Fatalf("보내지 않은 실행은 기록하지 않는다: %+v", es)
	}
}

func TestAWS_트리_경고_노드는_warn_을_이어받는다(t *testing.T) {
	f := newFake()
	f.children[""] = []source.Node{
		{Name: "orders", Path: "sqs/orders", Info: "대기 1 · 처리 중 0 · 지연 0 · DLQ orders-dlq 3", Warn: true, Openable: true},
		{Name: "calm", Path: "sqs/calm", Info: "대기 0 · 처리 중 0 · 지연 0"},
	}
	registerFake(t, f)
	m := sourceModel(t)
	cn := expandConn(t, m, 0)
	if n := childNamed(cn, "orders"); n == nil || !n.warn {
		t.Fatalf("Warn 노드: %+v", n)
	}
	if n := childNamed(cn, "calm"); n == nil || n.warn {
		t.Fatalf("보통 노드: %+v", n)
	}
	if !strings.Contains(ansi.Strip(m.View()), "orders 대기 1") {
		t.Fatal("경고 노드도 Info 를 보인다")
	}
}

func TestAWS_CLI_query_는_사용법_오류이고_tables_는_폴더다(t *testing.T) {
	f := newFake()
	f.children[""] = []source.Node{{Name: "SNS 토픽", Path: "sns", HasChildren: true}}
	registerFake(t, f)
	dir := fakeCLIProject(t)
	if _, err := db.AddConn(dir, db.Conn{Name: "infra", Kind: db.KindAWS, Region: "ap-northeast-2"}); err != nil {
		t.Fatal(err)
	}
	if code, _, errw := runCLITest(t, "", "query", "-p", dir, "-c", "infra", "x"); code != exitUsage || !strings.Contains(errw, msgTreeOnly) {
		t.Fatalf("query: %d %q", code, errw)
	}
	if runs, _, _ := f.snapshot(); len(runs) != 0 {
		t.Fatalf("Run 은 부르지 않는다: %v", runs)
	}
	if code, out, errw := runCLITest(t, "", "tables", "-p", dir, "-c", "infra"); code != exitOK || out != "name\tinfo\nSNS 토픽\t\n" {
		t.Fatalf("tables: %d %q %q", code, out, errw)
	}
}

func TestAWS_conn_add_는_url_없이_리전으로_더하고_url_은_받지_않는다(t *testing.T) {
	f := newFake()
	registerFake(t, f)
	dir := fakeCLIProject(t)
	var out, errw strings.Builder
	code := runConnCLI(context.Background(), []string{"add", "infra", "-p", dir, "--kind", "aws", "--region", "ap-northeast-2", "--profile", "stg"},
		nil, false, &out, &errw)
	if code != exitOK || !strings.Contains(out.String(), "AWS ap-northeast-2 stg") || f.conn.Kind != db.KindAWS {
		t.Fatalf("add: %d %q %q", code, out.String(), errw.String())
	}
	errw.Reset()
	code = runConnCLI(context.Background(), []string{"add", "infra2", "-p", dir, "--kind", "aws", "--region", "ap-northeast-2", "--url", "https://sqs.amazonaws.com"},
		nil, false, &out, &errw)
	if code == exitOK || !strings.Contains(errw.String(), "aws connection has no url") {
		t.Fatalf("url: %d %q", code, errw.String())
	}
}

func TestAWS_접속_창은_URL_칸이_없고_숨은_URL_값을_저장하지_않는다(t *testing.T) {
	m := connTestModel(t)
	m.openConnForm(nil, false)
	f := m.connForm
	f.setKind(db.KindDynamoDB)
	f.inputs[cfName].SetValue("infra")
	f.inputs[cfURL].SetValue("http://localhost:8000")
	f.inputs[cfRegion].SetValue("ap-northeast-2")
	f.setKind(db.KindAWS)
	c, err := f.conn()
	if err != nil || c.URL != "" || c.Region != "ap-northeast-2" || c.Kind != db.KindAWS {
		t.Fatalf("conn = %+v, %v", c, err)
	}
	if err := db.ValidateConn(c); err != nil {
		t.Fatal(err)
	}
	c.Region = ""
	if err := db.ValidateConn(c); err == nil || !strings.Contains(err.Error(), "region is required") {
		t.Fatalf("리전은 필수다: %v", err)
	}
	if db.PasswordKey(db.KindAWS, "infra") != "" {
		t.Fatal("aws 는 .env 비밀값 키가 없다")
	}
}

func TestAWS_트리에서_같은_환경의_DynamoDB_뒤에_온다(t *testing.T) {
	conns := []db.Conn{
		{Name: "a-aws", Group: "g", Kind: db.KindAWS, Region: "ap-northeast-2"},
		{Name: "z-ddb", Group: "g", Kind: db.KindDynamoDB, Region: "ap-northeast-2"},
		{Name: "z-pg", Group: "g", Kind: db.KindPG, Host: "localhost"},
	}
	var names []string
	for _, c := range connRoots(conns, nil, map[string]bool{})[0].children {
		names = append(names, c.label)
	}
	if got := strings.Join(names, ","); got != "z-pg,z-ddb,a-aws" {
		t.Fatalf("순서: %s", got)
	}
}
