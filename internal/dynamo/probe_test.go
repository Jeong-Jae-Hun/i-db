package dynamo

import (
	"context"
	"strings"
	"testing"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/dynamodb"
	"github.com/aws/smithy-go"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
)

// listAPI 는 ListTables 만 쓴다. pages 를 차례로 주고 err 가 있으면 그 페이지에서 실패한다.
type listAPI struct {
	api
	pages [][]string
	err   error
	calls int
}

func (f *listAPI) ListTables(context.Context, *dynamodb.ListTablesInput, ...func(*dynamodb.Options)) (*dynamodb.ListTablesOutput, error) {
	f.calls++
	if f.calls > len(f.pages) {
		return nil, f.err
	}
	out := &dynamodb.ListTablesOutput{TableNames: f.pages[f.calls-1]}
	if f.calls < len(f.pages) || f.err != nil {
		out.LastEvaluatedTableName = aws.String("next")
	}
	return out, nil
}

func TestProbe_ListTables_페이지를_이어_테이블_수를_센다(t *testing.T) {
	f := &listAPI{pages: [][]string{{"a", "b"}, {"c"}}}
	h := (&Backend{cl: f}).Probe(context.Background())
	if !h.OK || h.Summary() != "테이블 3" || h.Version != "" || f.calls != 2 {
		t.Fatalf("Probe = %+v, calls %d", h, f.calls)
	}
}

func TestProbe_AWS_오류는_작업_이름과_코드로_실패다(t *testing.T) {
	f := &listAPI{pages: [][]string{{"a"}}, err: &smithy.GenericAPIError{Code: "AccessDeniedException", Message: "not authorized"}}
	h := (&Backend{cl: f}).Probe(context.Background())
	if h.OK || h.Err != "dynamodb ListTables: AccessDeniedException: not authorized" {
		t.Fatalf("Probe = %+v", h)
	}
}

func TestIntegration_DynamoDB_Local_을_확인한다(t *testing.T) {
	b, table := newLocal(t, 1)
	ctx, cancel := context.WithTimeout(context.Background(), db.ProbeTimeout)
	defer cancel()
	var p source.Prober = b
	h := p.Probe(ctx)
	if !h.OK || len(h.Metrics) != 1 || h.Metrics[0].Value == "0" || h.Latency <= 0 {
		t.Fatalf("Probe = %+v (만든 테이블 %s)", h, table)
	}
	if !strings.HasPrefix(h.Summary(), "테이블 ") {
		t.Errorf("Summary = %q", h.Summary())
	}
}
