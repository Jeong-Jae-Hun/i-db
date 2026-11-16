package mcpserver

import (
	"context"
	"path/filepath"
	"testing"

	"github.com/Jeong-Jae-Hun/i-db/internal/athena"
	"github.com/Jeong-Jae-Hun/i-db/internal/awsres"
	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/dynamo"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
)

// ─── 요금 막기 (R72) ───

// 실제 Backend 를 열어도 요금이 붙는 도구 호출은 네트워크에 나가기 전에 같은 안내로 거절한다.
// 자격 증명 파일은 비워 SDK 가 이 머신 설정을 읽지 않게 한다(막힘 판정은 요청을 만들기 전이다).
func TestBilled_막혀_있으면_query_와_요금_aws_경로는_보내지_않고_같은_안내로_거절한다(t *testing.T) {
	t.Setenv(source.AllowBilledEnv, "")
	t.Setenv("AWS_ACCESS_KEY_ID", "x")
	t.Setenv("AWS_SECRET_ACCESS_KEY", "x")
	t.Setenv("AWS_PROFILE", "")
	t.Setenv("AWS_CONFIG_FILE", filepath.Join(t.TempDir(), "none"))
	t.Setenv("AWS_SHARED_CREDENTIALS_FILE", filepath.Join(t.TempDir(), "none"))
	t.Setenv("AWS_EC2_METADATA_DISABLED", "true")
	proj := &db.Project{Conns: []db.Conn{
		{Name: "athena-x", UUID: "a", Kind: db.KindAthena, Region: "ap-northeast-2"},
		{Name: "ddb-x", UUID: "d", Kind: db.KindDynamoDB, Region: "ap-northeast-2"},
		{Name: "aws-x", UUID: "w", Kind: db.KindAWS, Region: "ap-northeast-2"},
	}}
	opens := map[db.Kind]func(context.Context, db.Conn) (source.Backend, error){
		db.KindAthena: athena.New, db.KindDynamoDB: dynamo.New, db.KindAWS: awsres.New,
	}
	h := newHandler(proj, Options{OpenSource: func(ctx context.Context, c db.Conn) (source.Backend, error) { return opens[c.Kind](ctx, c) }})
	cs := connect(t, h)
	want := "요금이 붙는 조회라 막음 (DB_ALLOW_BILLED=1 로 허용)"
	for _, c := range []call{
		{"query", map[string]any{"connection": "athena-x", "sql": "SELECT 1 LIMIT 1", "force": true}},
		{"query", map[string]any{"connection": "ddb-x", "sql": `SELECT * FROM "t" WHERE pk = 'a'`}},
		{"aws_tree", map[string]any{"connection": "aws-x", "path": "sqs"}},
		{"aws_tree", map[string]any{"connection": "aws-x", "path": "s3/logs"}},
		{"aws_open", map[string]any{"connection": "aws-x", "path": "sns/arn:aws:sns:ap-northeast-2:1:t"}},
	} {
		if text, isErr := cs.callTool(c); !isErr || text != want {
			t.Errorf("%s %v: %v %q", c.name, c.args, isErr, text)
		}
	}
}
