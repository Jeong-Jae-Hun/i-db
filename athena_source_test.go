package main

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/x/ansi"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/history"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
)

// ─── Athena (R68) ───

func TestSource_Athena_는_내장_생성기로_연다(t *testing.T) {
	if sourceOpeners[db.KindAthena] == nil {
		t.Fatal("athena 생성기가 등록돼야 한다")
	}
}

func TestSource_Athena_파티션_테이블_Enter_는_확인_뒤_LIMIT_50_질의를_Force_로_보낸다(t *testing.T) {
	f := newFake()
	f.children[""] = []source.Node{{Name: "sales", Path: "sales", HasChildren: true}}
	f.children["sales"] = []source.Node{{Name: "orders", Path: "sales/orders", Info: "EXTERNAL_TABLE", HasChildren: true, Openable: true}}
	f.open = func(string) (*db.Result, error) { return nil, source.NeedsConfirm("파티션 조건 없음") }
	f.run = func(source.Query) (*db.Result, error) {
		return &db.Result{Cols: []string{"id"}, Rows: [][]string{{"7"}}, Text: "스캔 1.0MB · 엔진 0.3초"}, nil
	}
	registerFake(t, f)
	m := newModel(&db.Project{Dir: t.TempDir(), Mapping: map[string]string{}, Conns: []db.Conn{
		{Name: "athena-stg", UUID: "a1", Kind: db.KindAthena, Region: "ap-northeast-2", Source: db.SourceDB}}})
	m.Update(tea.WindowSizeMsg{Width: 140, Height: 40})
	h, err := history.Open(filepath.Join(t.TempDir(), "history.jsonl"))
	must(t, err)
	m.hist = h
	t.Cleanup(func() { m.stop(); m.sources.close() })
	m.setFocus(fTree)

	cn := expandConn(t, m, 0)
	dbNode := childNamed(cn, "sales")
	m.selectNode(dbNode)
	drive(m, m.activate(dbNode, true))
	m.selectNode(childNamed(dbNode, "orders"))
	_, cmd := m.Update(key("enter"))
	drive(m, cmd)
	if m.overlay != oConfirm || !strings.Contains(ansi.Strip(m.View()), "비싼 질의: 파티션 조건 없음") {
		t.Fatalf("확인 창: overlay=%v status=%q", m.overlay, m.status)
	}
	_, cmd = m.Update(key("y"))
	drive(m, cmd)
	runs, _, _ := f.snapshot()
	if len(runs) != 1 || runs[0].Text != `SELECT * FROM "sales"."orders" LIMIT 50` || !runs[0].Force || runs[0].Limit != 50 {
		t.Fatalf("y 뒤 Force 미리 보기: %+v", runs)
	}
	if res := m.cur().res; res == nil || res.Rows[0][0] != "7" {
		t.Fatal("결과를 표에 붙인다")
	}
	if !strings.Contains(ansi.Strip(m.View()), "스캔 1.0MB · 엔진 0.3초") {
		t.Fatal("스캔 크기·엔진 시간을 표 위 한 줄에 보인다")
	}
}

func TestConnForm_Athena_는_작업_그룹과_결과_위치를_저장하고_숨은_URL_은_버린다(t *testing.T) {
	registerFake(t, newFake())
	m := connTestModel(t)
	m.openConnForm(nil, false)
	f := m.connForm
	f.inputs[cfName].SetValue("athena-stg")
	f.setKind(db.KindAthena)
	if view := ansi.Strip(m.View()); !strings.Contains(view, "작업 그룹") || !strings.Contains(view, "결과 위치") || strings.Contains(view, "엔드포인트") {
		t.Fatalf("Athena 칸:\n%s", view)
	}
	f.inputs[cfURL].SetValue("https://숨은")
	f.inputs[cfRegion].SetValue("ap-northeast-2")
	f.inputs[cfWorkgroup].SetValue("analysts")
	f.inputs[cfOutput].SetValue("s3://bucket/prefix/")
	_, cmd := m.Update(key("enter"))
	runTest(t, m, cmd)
	if m.connForm != nil {
		t.Fatalf("시험 성공 뒤 저장하고 닫는다: %q", f.status)
	}
	raw, _ := os.ReadFile(filepath.Join(m.proj.Dir, ".dbtui", "connections.json"))
	for _, want := range []string{`"kind": "athena"`, `"workgroup": "analysts"`, `"output": "s3://bucket/prefix/"`} {
		if !strings.Contains(string(raw), want) {
			t.Fatalf("connections.json 에 %s 가 없다: %s", want, raw)
		}
	}
	if strings.Contains(string(raw), "숨은") {
		t.Fatalf("숨은 URL 칸은 저장하지 않는다: %s", raw)
	}
	i := connByName(m.proj, "athena-stg")
	m.rebuild()
	m.selectNode(m.connNode(i))
	m.Update(key("E"))
	if f := m.connForm; f == nil || f.inputs[cfWorkgroup].Value() != "analysts" || f.inputs[cfOutput].Value() != "s3://bucket/prefix/" {
		t.Fatal("고치기 창은 작업 그룹·결과 위치를 채운다")
	}
}

func TestConnCLI_Athena_접속을_url_없이_더한다(t *testing.T) {
	f := newFake()
	registerFake(t, f)
	dir := fakeCLIProject(t)
	var out, errw strings.Builder
	code := runConnCLI(context.Background(), []string{"add", "athena-stg", "-p", dir, "--kind", "athena", "--region", "ap-northeast-2",
		"--profile", "stg", "--workgroup", "analysts", "--output", "s3://bucket/prefix/"}, nil, false, &out, &errw)
	if code != exitOK || !strings.Contains(out.String(), "Athena ap-northeast-2 stg analysts s3://bucket/prefix/") {
		t.Fatalf("add: %d %q %q", code, out.String(), errw.String())
	}
	if f.conn.Workgroup != "analysts" || f.conn.Output != "s3://bucket/prefix/" || f.conn.URL != "" {
		t.Fatalf("시험한 접속: %+v", f.conn)
	}
	out.Reset()
	errw.Reset()
	code = runConnCLI(context.Background(), []string{"add", "athena-bad", "-p", dir, "--kind", "athena", "--region", "r", "--output", "bucket/prefix"}, nil, false, &out, &errw)
	if code == exitOK || !strings.Contains(errw.String(), "s3://") {
		t.Fatalf("s3:// 가 아닌 결과 위치는 저장하지 않는다: %d %q", code, errw.String())
	}
}

func TestCLI_Athena_query_는_저장소_길로_보내고_스캔_줄은_stderr_다(t *testing.T) {
	f := newFake()
	f.run = func(source.Query) (*db.Result, error) {
		return &db.Result{Cols: []string{"n"}, Rows: [][]string{{"1"}}, Text: "스캔 10B · 엔진 0.1초"}, nil
	}
	registerFake(t, f)
	dir := fakeCLIProject(t)
	if _, err := db.AddConn(dir, db.Conn{Name: "athena-stg", Kind: db.KindAthena, Region: "ap-northeast-2"}); err != nil {
		t.Fatal(err)
	}
	code, out, errw := runCLITest(t, "", "query", "-p", dir, "-c", "athena-stg", "--format", "tsv", "SELECT 1 AS n")
	if code != exitOK || out != "n\n1\n" || !strings.Contains(errw, "스캔 10B · 엔진 0.1초") {
		t.Fatalf("query: %d %q %q", code, out, errw)
	}
	if runs, _, _ := f.snapshot(); runs[0].Text != "SELECT 1 AS n" || runs[0].Limit != sourceDefaultRows {
		t.Fatalf("Run: %+v", runs[0])
	}
}

func TestConnRank_Athena_는_DynamoDB_다음이다(t *testing.T) {
	if !(connRank(db.Conn{Kind: db.KindDynamoDB}) < connRank(db.Conn{Kind: db.KindAthena})) {
		t.Fatal("R66.1: DynamoDB → Athena")
	}
}
