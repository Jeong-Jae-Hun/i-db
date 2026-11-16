package main

import (
	"context"
	"path/filepath"
	"strings"
	"testing"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/history"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
)

// ─── 요금 막기 (R72) ───

const msgBilledWant = "요금이 붙는 조회라 막음 (DB_ALLOW_BILLED=1 로 허용)"

// noBilledEnv 는 요금을 막고 AWS SDK 가 이 머신의 자격 증명·설정을 읽지 않게 한다. 실제 내장 생성기(dynamo·athena·awsres)를 쓰는 시험이
// 막힘 판정 전에 네트워크에 나가면 가짜 자격 증명으로 실패해 드러난다.
func noBilledEnv(t *testing.T) {
	t.Helper()
	t.Setenv(source.AllowBilledEnv, "")
	t.Setenv("AWS_ACCESS_KEY_ID", "x")
	t.Setenv("AWS_SECRET_ACCESS_KEY", "x")
	t.Setenv("AWS_SESSION_TOKEN", "")
	t.Setenv("AWS_PROFILE", "")
	t.Setenv("AWS_CONFIG_FILE", filepath.Join(t.TempDir(), "none"))
	t.Setenv("AWS_SHARED_CREDENTIALS_FILE", filepath.Join(t.TempDir(), "none"))
	t.Setenv("AWS_EC2_METADATA_DISABLED", "true")
}

func TestBilled_편집기_DynamoDB_실행은_보내지_않고_상태_줄에_알리며_확인을_묻지_않는다(t *testing.T) {
	noBilledEnv(t)
	m := sourceModel(t) // ddb-stg 는 실제 dynamo 생성기로 연다
	m.setConn(m.cur(), 3)
	editorRun(m, `SELECT * FROM "orders"`)
	if m.status != msgBilledWant || m.overlay != oNone || m.cur().running != nil {
		t.Fatalf("status=%q overlay=%v", m.status, m.overlay)
	}
	if es := m.hist.List(history.Filter{}); len(es) != 0 {
		t.Fatalf("보내지 않은 질의는 기록하지 않는다: %+v", es)
	}
}

func TestBilled_트리_Enter_가_막히면_확인_창_없이_알린다(t *testing.T) {
	noBilledEnv(t)
	f := newFake()
	f.children[""] = []source.Node{{Name: "orders", Path: "orders", HasChildren: true, Openable: true}}
	f.open = func(string) (*db.Result, error) { return nil, source.ErrBilled }
	registerFake(t, f)
	m := sourceModel(t)
	cn := expandConn(t, m, 3)
	m.selectNode(childNamed(cn, "orders"))
	_, cmd := m.Update(key("enter"))
	drive(m, cmd)
	if m.status != msgBilledWant || m.overlay != oNone {
		t.Fatalf("status=%q overlay=%v", m.status, m.overlay)
	}
	if runs, _, _ := f.snapshot(); len(runs) != 0 {
		t.Fatalf("막힌 Enter 뒤 Force 스캔을 보냄: %+v", runs)
	}
}

func TestBilled_CLI_요금_조회는_보내지_않고_종료_코드_3이다(t *testing.T) {
	noBilledEnv(t)
	dir := sourceCLIProject(t)
	_, err := db.AddConn(dir, db.Conn{Name: "athena-stg", Kind: db.KindAthena, Region: "ap-northeast-2"})
	must(t, err)
	for _, args := range [][]string{
		{"query", "-p", dir, "-c", "ddb", "--force", `SELECT * FROM "t"`},
		{"query", "-p", dir, "-c", "athena-stg", "--force", "SELECT 1"},
		{"tables", "-p", dir, "-c", "athena-stg"},
	} {
		if code, out, errw := runCLITest(t, "", args...); code != exitRefused || !strings.Contains(errw, msgBilledWant) || out != "" {
			t.Errorf("%v: %d %q %q", args, code, out, errw)
		}
	}
}

func TestBilled_보드_패널은_요금_조회를_보내지_않고_안내를_보인다(t *testing.T) {
	noBilledEnv(t)
	p := boardProject(t)
	p.Conns = append(p.Conns, db.Conn{Name: "ddb-stg", UUID: "dd", Kind: db.KindDynamoDB, Region: "ap-northeast-2", Source: db.SourceDB})
	sources := newSourcePool()
	defer sources.close()
	r := runPanel(context.Background(), nil, sources, p, boardPanel{Title: "주문", Conn: "ddb-stg", Query: `SELECT * FROM "t" WHERE pk = 'a'`, View: "table"}, true)
	if r.err != msgBilledWant || r.res != nil {
		t.Fatalf("%+v", r)
	}
}

// proberFake 는 맨 위 목록이 요금으로 막히고 요금 없는 대시보드 확인만 되는 저장소다(Athena 꼴).
type proberFake struct {
	*fakeSource
	health db.Health
	probes int
}

func (p *proberFake) Probe(context.Context) db.Health {
	p.probes++
	return p.health
}

func TestBilled_접속_시험은_막힌_목록_대신_요금_없는_확인으로_자격_증명을_본다(t *testing.T) {
	noBilledEnv(t)
	f := newFake()
	f.childErr = source.ErrBilled
	pf := &proberFake{fakeSource: f, health: db.Health{OK: true}}
	saved := sourceOpeners[db.KindAthena]
	sourceOpeners[db.KindAthena] = func(context.Context, db.Conn) (source.Backend, error) { return pf, nil }
	t.Cleanup(func() { sourceOpeners[db.KindAthena] = saved })
	c := db.Conn{Name: "athena-stg", Kind: db.KindAthena, Region: "ap-northeast-2"}
	if v, err := testConn(context.Background(), c); err != nil || v != "Athena · 목록은 요금이 붙어 시험 안 함" || pf.probes != 1 {
		t.Fatalf("%q %v probes=%d", v, err, pf.probes)
	}
	pf.health = db.Health{Err: "aws GetWorkGroup: ExpiredToken"}
	if _, err := testConn(context.Background(), c); err == nil || err.Error() != "aws GetWorkGroup: ExpiredToken" {
		t.Fatalf("확인 실패는 시험 실패다: %v", err)
	}
	// 확인이 없는 종류는 건너뛰었다고만 알린다
	saved2 := sourceOpeners[db.KindDynamoDB]
	sourceOpeners[db.KindDynamoDB] = func(context.Context, db.Conn) (source.Backend, error) { return f, nil }
	t.Cleanup(func() { sourceOpeners[db.KindDynamoDB] = saved2 })
	if v, err := testConn(context.Background(), db.Conn{Name: "d", Kind: db.KindDynamoDB}); err != nil || !strings.Contains(v, "시험 건너뜀 ("+msgBilledWant+")") {
		t.Fatalf("%q %v", v, err)
	}
}
