package awsres

import (
	"reflect"
	"slices"
	"strings"
	"testing"

	"github.com/aws/aws-sdk-go-v2/aws"
	lambdatypes "github.com/aws/aws-sdk-go-v2/service/lambda/types"

	"github.com/Jeong-Jae-Hun/i-db/internal/source"
)

// ─── 이벤트 흐름 (R69) ───

const (
	workerArn = "arn:aws:sqs:ap-northeast-2:111122223333:orders-worker"
	dlqQArn   = "arn:aws:sqs:ap-northeast-2:111122223333:orders-worker-dlq"
	otherArn  = "arn:aws:sqs:ap-northeast-2:999988887777:partner"
)

func flowFixture(la *fakeLambda) {
	la.mappings["worker"] = []lambdatypes.EventSourceMappingConfiguration{
		{EventSourceArn: aws.String(workerArn), FunctionArn: aws.String("arn:aws:lambda:ap-northeast-2:111122223333:function:worker"),
			State: aws.String("Enabled"), LastProcessingResult: aws.String("OK")},
	}
	la.mappings["other"] = []lambdatypes.EventSourceMappingConfiguration{
		{EventSourceArn: aws.String("arn:aws:sqs:ap-northeast-2:111122223333:else"), FunctionArn: aws.String("arn:aws:lambda:x:1:function:other")},
	}
}

func TestQueueFlows_큐_이름은_계정과_리전으로_ARN_을_만들고_Lambda_매핑만_묻는다(t *testing.T) {
	for _, gate := range []string{"", "1"} { // 허용과 상관없이 요금 서비스를 부르지 않는다(R72)
		t.Setenv(source.AllowBilledEnv, gate)
		b, la, trap := freeBackend()
		flowFixture(la)
		got, err := b.QueueFlows(ctx, []QueueRef{{Name: "orders-worker", DLQ: "orders-worker-dlq"}, {Name: otherArn}})
		if err != nil {
			t.Fatal(err)
		}
		want := []Queue{
			{Name: "orders-worker", Arn: workerArn, DLQ: "orders-worker-dlq", DLQArn: dlqQArn, Lambdas: []Mapping{{Function: "worker", State: "Enabled", LastResult: "OK"}}},
			{Name: "partner", Arn: otherArn},
		}
		if !reflect.DeepEqual(got, want) {
			t.Fatalf("gate=%q 큐:\n%+v\nwant\n%+v", gate, got, want)
		}
		slices.Sort(la.sources)
		if !reflect.DeepEqual(la.sources, []string{workerArn, otherArn}) {
			t.Fatalf("매핑은 큐 ARN 으로 고른다: %v", la.sources)
		}
		if n := trap.calls.Load(); n != 0 {
			t.Fatalf("gate=%q 흐름이 요금 서비스를 %d번 부름", gate, n)
		}
	}
}

func TestQueueFlows_DLQ_이름은_큐의_계정을_따르고_ARN_이면_그대로다(t *testing.T) {
	b, _, _ := freeBackend()
	got, err := b.QueueFlows(ctx, []QueueRef{{Name: otherArn, DLQ: "partner-dlq"}, {Name: otherArn, DLQ: "arn:aws:sqs:us-east-1:555:shared"}})
	if err != nil {
		t.Fatal(err)
	}
	if got[0].DLQArn != "arn:aws:sqs:ap-northeast-2:999988887777:partner-dlq" || got[0].DLQ != "partner-dlq" {
		t.Fatalf("다른 계정 큐의 DLQ 이름: %+v", got[0])
	}
	if got[1].DLQArn != "arn:aws:sqs:us-east-1:555:shared" || got[1].DLQ != "shared" {
		t.Fatalf("DLQ ARN: %+v", got[1])
	}
}

func TestQueueFlows_ARN_만이면_계정을_묻지_않고_계정을_못_읽으면_오류다(t *testing.T) {
	b, _, _ := freeBackend()
	st := &fakeSTS{err: denied("ExpiredToken")}
	b.sts = st
	if _, err := b.QueueFlows(ctx, []QueueRef{{Name: otherArn}}); err != nil || st.calls.Load() != 0 {
		t.Fatalf("ARN 만: %v calls=%d", err, st.calls.Load())
	}
	if _, err := b.QueueFlows(ctx, []QueueRef{{Name: "orders-worker"}}); err == nil || err.Error() != "aws sts:GetCallerIdentity: ExpiredToken: not authorized" {
		t.Fatalf("계정 오류: %v", err)
	}
	b.sts, b.region = &fakeSTS{}, ""
	if _, err := b.QueueFlows(ctx, []QueueRef{{Name: "orders-worker"}}); err == nil || !strings.Contains(err.Error(), "no region") {
		t.Fatalf("리전 없음: %v", err)
	}
}

func TestQueueFlows_매핑을_못_읽으면_그_큐만_오류다(t *testing.T) {
	b, la, _ := freeBackend()
	la.mapErr = denied("AccessDeniedException")
	got, err := b.QueueFlows(ctx, []QueueRef{{Name: "orders-worker", DLQ: "orders-worker-dlq"}})
	if err != nil {
		t.Fatal(err)
	}
	if q := got[0]; !strings.Contains(q.Err, "lambda:ListEventSourceMappings") || q.Name != "orders-worker" || q.DLQ != "orders-worker-dlq" {
		t.Fatalf("큐 오류: %+v", q)
	}
}
