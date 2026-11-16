package dynamo

import (
	"context"
	"errors"
	"os"
	"testing"

	"github.com/aws/aws-sdk-go-v2/service/dynamodb"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
)

// TestMain 은 데이터 읽기 시험이 요금 막음(R72) 아래서 돌지 않게 패키지 전체를 허용으로 둔다.
// 막힌 경우는 아래 시험이 t.Setenv 로 따로 본다.
func TestMain(m *testing.M) {
	os.Setenv(source.AllowBilledEnv, "1")
	os.Exit(m.Run())
}

// listOnly 는 요금 없는 목록·DescribeTable 만 답하는 가짜다. 데이터 읽기(ExecuteStatement)를 부르면 센다.
type listOnly struct {
	fakeAPI
	reads int
}

func (f *listOnly) ExecuteStatement(context.Context, *dynamodb.ExecuteStatementInput, ...func(*dynamodb.Options)) (*dynamodb.ExecuteStatementOutput, error) {
	f.reads++
	return &dynamodb.ExecuteStatementOutput{}, nil
}

func TestBilled_막혀_있으면_Enter_와_PartiQL_은_보내지_않고_목록과_확인은_된다(t *testing.T) {
	t.Setenv(source.AllowBilledEnv, "")
	f := &listOnly{fakeAPI: fakeAPI{perPage: 1}}
	b := &Backend{cl: f}
	ctx := context.Background()
	if _, err := b.Open(ctx, "t", 50); !errors.Is(err, source.ErrBilled) {
		t.Fatalf("Enter 는 확인보다 요금 막음이 먼저다: %v", err)
	}
	for _, q := range []source.Query{{Text: `SELECT * FROM "t" WHERE pk = 'a'`}, {Text: `SELECT * FROM "t"`, Force: true}} {
		if _, err := b.Run(ctx, q); !errors.Is(err, source.ErrBilled) {
			t.Fatalf("Run %q: %v", q.Text, err)
		}
	}
	if f.reads != 0 {
		t.Fatalf("막혔는데 데이터를 %d번 읽음", f.reads)
	}
	if _, err := b.Children(ctx, ""); err != nil {
		t.Fatalf("ListTables 는 요금이 없다: %v", err)
	}
	if h := b.Probe(ctx); !h.OK {
		t.Fatalf("대시보드 확인은 그대로다: %+v", h)
	}
	t.Setenv(source.AllowBilledEnv, "1")
	if _, err := b.Run(ctx, source.Query{Text: `SELECT * FROM "t" WHERE pk = 'a'`}); err != nil || f.reads == 0 {
		t.Fatalf("허용하면 보낸다: %v reads=%d", err, f.reads)
	}
}

func TestBilled_이_머신의_DynamoDB_Local_은_막지_않는다(t *testing.T) {
	t.Setenv(source.AllowBilledEnv, "")
	t.Setenv("AWS_ACCESS_KEY_ID", "x")
	t.Setenv("AWS_SECRET_ACCESS_KEY", "x")
	for url, local := range map[string]bool{
		"http://localhost:8000": true, "http://127.0.0.1:8000": true, "http://[::1]:8000": true,
		"": false, "https://dynamodb.ap-northeast-2.amazonaws.com": false, "http://localhost.evil.example": false,
	} {
		sb, err := New(context.Background(), db.Conn{Kind: db.KindDynamoDB, URL: url, Region: "ap-northeast-2"})
		if err != nil {
			t.Fatal(err)
		}
		if got := sb.(*Backend).local; got != local {
			t.Errorf("%q: local=%v", url, got)
		}
	}
}
