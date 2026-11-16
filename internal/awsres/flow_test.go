package awsres

import (
	"reflect"
	"strings"
	"testing"

	"github.com/aws/aws-sdk-go-v2/aws"
	lambdatypes "github.com/aws/aws-sdk-go-v2/service/lambda/types"
)

// ─── 이벤트 흐름 (R69) ───

const (
	workerURL = "https://sqs.ap-northeast-2.amazonaws.com/111122223333/orders-worker"
	dlqURL    = "https://sqs.ap-northeast-2.amazonaws.com/111122223333/orders-worker-dlq"
	workerArn = "arn:aws:sqs:ap-northeast-2:111122223333:orders-worker"
	dlqQArn   = "arn:aws:sqs:ap-northeast-2:111122223333:orders-worker-dlq"
)

func flowFixture(sn *fakeSNS, sq *fakeSQS, la *fakeLambda) {
	snsFixture(sn)
	sq.byName = map[string]string{"orders-worker": workerURL, "orders-worker-dlq": dlqURL}
	sq.attrs[workerURL] = map[string]string{attrVisible: "0", attrInFlight: "2", attrDelayed: "0",
		attrRedrive: `{"deadLetterTargetArn":"` + dlqQArn + `","maxReceiveCount":5}`}
	sq.attrs[dlqURL] = map[string]string{attrVisible: "4"}
	la.mappings["worker"] = []lambdatypes.EventSourceMappingConfiguration{
		{EventSourceArn: aws.String(workerArn), FunctionArn: aws.String("arn:aws:lambda:ap-northeast-2:111122223333:function:worker"),
			State: aws.String("Enabled"), LastProcessingResult: aws.String("OK")},
	}
	la.mappings["other"] = []lambdatypes.EventSourceMappingConfiguration{
		{EventSourceArn: aws.String("arn:aws:sqs:ap-northeast-2:111122223333:else"), FunctionArn: aws.String("arn:aws:lambda:x:1:function:other")},
	}
}

func TestTopicFlow_토픽_이름에서_큐_DLQ_Lambda_까지_따라간다(t *testing.T) {
	b, sn, sq, la := newBackend()
	flowFixture(sn, sq, la)
	got, err := b.TopicFlow(ctx, "orders")
	if err != nil {
		t.Fatal(err)
	}
	if got.Arn != topicArn || got.Name != "orders" || len(got.Subs) != 3 {
		t.Fatalf("토픽: %+v", got)
	}
	want := &Queue{Name: "orders-worker", Arn: workerArn, Visible: "0", InFlight: "2", Delayed: "0", DLQ: "orders-worker-dlq", DLQArn: dlqQArn, DLQCount: "4",
		Lambdas: []Mapping{{Function: "worker", State: "Enabled", LastResult: "OK"}}}
	if s := got.Subs[0]; s.Protocol != "sqs" || s.Endpoint != "orders-worker" || !reflect.DeepEqual(s.Queue, want) {
		t.Fatalf("같은 계정 큐: %+v %+v", s, s.Queue)
	}
	// 다른 계정 큐는 속성을 묻지 않고 ARN 잎이다. email 은 받는 사람을 가린다
	if s := got.Subs[1]; s.Queue != nil || s.Endpoint != "arn:aws:sqs:ap-northeast-2:999988887777:partner" {
		t.Fatalf("다른 계정 큐: %+v", s)
	}
	if s := got.Subs[2]; s.Queue != nil || s.Protocol != "email" || s.Endpoint != "o***@example.com" {
		t.Fatalf("email: %+v", s)
	}
	for _, c := range sq.urlCalls {
		if strings.Contains(c, "partner") {
			t.Fatalf("다른 계정 큐를 물음: %v", sq.urlCalls)
		}
	}
}

func TestTopicFlow_ARN_은_목록을_묻지_않고_없는_이름은_오류다(t *testing.T) {
	b, sn, sq, la := newBackend()
	flowFixture(sn, sq, la)
	sn.topicErr = denied("Throttling") // ARN 이면 ListTopics 를 부르지 않는다
	if got, err := b.TopicFlow(ctx, topicArn); err != nil || got.Name != "orders" {
		t.Fatalf("ARN: %+v %v", got, err)
	}
	sn.topicErr = nil
	if _, err := b.TopicFlow(ctx, "nope"); err == nil || err.Error() != "topic not found: nope" {
		t.Fatalf("없는 토픽: %v", err)
	}
}

func TestTopicFlow_큐_속성을_못_읽으면_그_큐만_오류다(t *testing.T) {
	b, sn, sq, la := newBackend()
	flowFixture(sn, sq, la)
	sq.attrErr[workerURL] = denied("AccessDenied")
	got, err := b.TopicFlow(ctx, "orders")
	if err != nil {
		t.Fatal(err)
	}
	if q := got.Subs[0].Queue; q == nil || !strings.Contains(q.Err, "sqs:GetQueueAttributes") || q.Visible != "" {
		t.Fatalf("큐 오류: %+v", q)
	}
	// DLQ 를 못 읽으면 수는 ? 다
	delete(sq.attrErr, workerURL)
	delete(sq.byName, "orders-worker-dlq")
	got, _ = b.TopicFlow(ctx, "orders")
	if q := got.Subs[0].Queue; q.DLQ != "orders-worker-dlq" || q.DLQCount != "?" {
		t.Fatalf("DLQ 못 읽음: %+v", q)
	}
}

func TestMaskEndpoint_email_만_가리고_다른_프로토콜은_그대로다(t *testing.T) {
	for _, c := range [][3]string{
		{"email", "ops@example.com", "o***@example.com"},
		{"email-json", "한글@example.com", "한***@example.com"},
		{"email", "broken", "***"},
		{"https", "https://hook.example.com/x", "https://hook.example.com/x"},
	} {
		if got := maskEndpoint(c[0], c[1]); got != c[2] {
			t.Errorf("%v: %q", c, got)
		}
	}
}
