package awsres

import (
	"context"
	"errors"
	"fmt"
	"reflect"
	"slices"
	"strings"
	"testing"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	lambdatypes "github.com/aws/aws-sdk-go-v2/service/lambda/types"
	snstypes "github.com/aws/aws-sdk-go-v2/service/sns/types"

	"github.com/Jeong-Jae-Hun/i-db/internal/source"
)

var ctx = context.Background()

func names(ns []source.Node) []string {
	out := make([]string, len(ns))
	for i, n := range ns {
		out[i] = n.Name
	}
	return out
}

// ─── 공통 ───

func TestChildren_맨_위는_자격_증명을_확인한_뒤_폴더를_정한_순서로_보인다(t *testing.T) {
	allowBilled(t)
	b, _, _, _ := newBackend()
	ns, err := b.Children(ctx, "")
	if err != nil {
		t.Fatal(err)
	}
	if want := []string{"SNS 토픽", "SQS 큐", "Lambda", "EventBridge", "Scheduler", "Step Functions", "S3"}; !reflect.DeepEqual(names(ns), want) {
		t.Fatalf("폴더: %v", names(ns))
	}
	if !ns[0].HasChildren || ns[0].Openable || ns[1].Path != "sqs" {
		t.Fatalf("폴더 노드: %+v", ns[0])
	}
	b.sts = &fakeSTS{err: denied("ExpiredToken")}
	if _, err := b.Children(ctx, ""); err == nil || err.Error() != "aws sts:GetCallerIdentity: ExpiredToken: not authorized" {
		t.Fatalf("자격 증명 오류는 시험 실패다: %v", err)
	}
}

func TestChildren_권한_없는_폴더는_한_줄로_보이고_다른_폴더는_그대로다(t *testing.T) {
	allowBilled(t)
	b, sn, sq, _ := newBackend()
	sn.topicErr = denied("AuthorizationError")
	sq.urls = []string{"https://sqs.ap-northeast-2.amazonaws.com/111122223333/a"}
	sq.attrs[sq.urls[0]] = map[string]string{attrVisible: "0", attrInFlight: "0", attrDelayed: "0"}
	ns, err := b.Children(ctx, "sns")
	if err != nil || len(ns) != 1 || ns[0].Name != "권한 없음: sns:ListTopics" || ns[0].HasChildren || ns[0].Openable {
		t.Fatalf("sns: %+v %v", ns, err)
	}
	if ns, err := b.Children(ctx, "sqs"); err != nil || !reflect.DeepEqual(names(ns), []string{"a"}) {
		t.Fatalf("sqs: %+v %v", ns, err)
	}
	if _, err := b.Open(ctx, ns[0].Path, 50); err == nil {
		t.Fatal("권한 없음 줄은 열 것이 없다")
	}
	// 권한 거절이 아닌 오류는 그대로 오류다
	sn.topicErr = denied("Throttling")
	if _, err := b.Children(ctx, "sns"); err == nil || !strings.Contains(err.Error(), "sns:ListTopics: Throttling") {
		t.Fatalf("오류: %v", err)
	}
}

func TestDeniedAction_서비스마다_다른_거절_코드를_알아본다(t *testing.T) {
	for code, want := range map[string]bool{
		"AccessDenied": true, "AccessDeniedException": true, "AuthorizationError": true,
		"AWS.SimpleQueueService.AccessDenied": true, "UnauthorizedOperation": true,
		"Throttling": false, "ExpiredToken": false,
	} {
		if got := deniedAction(wrap("x:Y", denied(code))) != ""; got != want {
			t.Errorf("%s: %v", code, got)
		}
	}
	if deniedAction(denied("AccessDenied")) != "" {
		t.Error("작업 이름이 없으면 권한 없음 줄을 만들 수 없다")
	}
}

func TestRun_은_늘_읽기_전용_거절이다(t *testing.T) {
	b, _, _, _ := newBackend()
	if _, err := b.Run(ctx, source.Query{Text: "anything"}); !errors.Is(err, source.ErrWrite) {
		t.Fatalf("Run: %v", err)
	}
}

// ─── SNS ───

const (
	topicArn = "arn:aws:sns:ap-northeast-2:111122223333:orders"
	subSame  = "arn:aws:sns:ap-northeast-2:111122223333:orders:aaaa"
	subOther = "arn:aws:sns:ap-northeast-2:111122223333:orders:bbbb"
)

func snsFixture(sn *fakeSNS) {
	sn.topics = [][]string{{topicArn}, {"arn:aws:sns:ap-northeast-2:111122223333:alerts"}}
	sn.subs[topicArn] = []snstypes.Subscription{
		{Protocol: aws.String("sqs"), Endpoint: aws.String("arn:aws:sqs:ap-northeast-2:111122223333:orders-worker"), SubscriptionArn: aws.String(subSame)},
		{Protocol: aws.String("sqs"), Endpoint: aws.String("arn:aws:sqs:ap-northeast-2:999988887777:partner"), SubscriptionArn: aws.String(subOther)},
		{Protocol: aws.String("email"), Endpoint: aws.String("ops@example.com"), SubscriptionArn: aws.String("PendingConfirmation")},
	}
	sn.attrs[subSame] = map[string]string{"RawMessageDelivery": "true", "FilterPolicy": `{"type":["paid"]}`}
	sn.attrs[subOther] = map[string]string{"RawMessageDelivery": "false"}
}

func TestSNS_토픽은_페이지를_잇고_구독은_같은_계정_SQS_만_큐_이름으로_줄인다(t *testing.T) {
	allowBilled(t)
	b, sn, _, _ := newBackend()
	snsFixture(sn)
	ns, err := b.Children(ctx, "sns")
	if err != nil || !reflect.DeepEqual(names(ns), []string{"orders", "alerts"}) {
		t.Fatalf("토픽: %v %v", names(ns), err)
	}
	if ns[0].Path != "sns/"+topicArn || !ns[0].HasChildren || !ns[0].Openable {
		t.Fatalf("토픽 노드: %+v", ns[0])
	}
	subs, err := b.Children(ctx, ns[0].Path)
	want := []string{"orders-worker", "arn:aws:sqs:ap-northeast-2:999988887777:partner", "ops@example.com"}
	if err != nil || !reflect.DeepEqual(names(subs), want) || subs[0].Info != "sqs" || subs[0].HasChildren {
		t.Fatalf("구독: %+v %v", subs, err)
	}
	if leaf, err := b.Children(ctx, subs[0].Path); err != nil || leaf != nil {
		t.Fatalf("구독은 잎: %v %v", leaf, err)
	}
}

func TestSNS_토픽_Enter_는_구독_표이고_확인_전_구독은_속성을_묻지_않는다(t *testing.T) {
	allowBilled(t)
	b, sn, _, _ := newBackend()
	snsFixture(sn)
	res, err := b.Open(ctx, "sns/"+topicArn, 50)
	if err != nil {
		t.Fatal(err)
	}
	if want := []string{"protocol", "endpoint", "subscription_arn", "raw", "filter"}; !reflect.DeepEqual(res.Cols, want) {
		t.Fatalf("열: %v", res.Cols)
	}
	want := [][]string{
		{"sqs", "orders-worker", subSame, "true", `{"type":["paid"]}`},
		{"sqs", "arn:aws:sqs:ap-northeast-2:999988887777:partner", subOther, "false", ""},
		{"email", "ops@example.com", "PendingConfirmation", "", ""},
	}
	if !reflect.DeepEqual(res.Rows, want) {
		t.Fatalf("행: %v", res.Rows)
	}
	slices.Sort(sn.attrCalls)
	if !reflect.DeepEqual(sn.attrCalls, []string{subSame, subOther}) {
		t.Fatalf("속성 요청: %v", sn.attrCalls)
	}
}

// ─── SQS ───

const qbase = "https://sqs.ap-northeast-2.amazonaws.com/111122223333/"

func qattrs(arnName, visible, inflight, delayed, dlq string) map[string]string {
	a := map[string]string{attrArn: "arn:aws:sqs:ap-northeast-2:111122223333:" + arnName, attrVisible: visible, attrInFlight: inflight, attrDelayed: delayed}
	if dlq != "" {
		a[attrRedrive] = fmt.Sprintf(`{"deadLetterTargetArn":"%s","maxReceiveCount":5}`, dlq)
	}
	return a
}

func TestSQS_큐_Info_는_대기_처리_중_지연이고_DLQ_에_쌓이면_경고다(t *testing.T) {
	allowBilled(t)
	b, _, sq, _ := newBackend()
	sq.urls = []string{qbase + "orders", qbase + "orders-dlq", qbase + "calm", qbase + "x-acct", qbase + "broken"}
	sq.attrs[qbase+"orders"] = qattrs("orders", "12", "3", "0", "arn:aws:sqs:ap-northeast-2:111122223333:orders-dlq")
	sq.attrs[qbase+"orders-dlq"] = qattrs("orders-dlq", "2", "0", "0", "")
	sq.attrs[qbase+"calm"] = qattrs("calm", "0", "0", "1", "arn:aws:sqs:ap-northeast-2:111122223333:calm-dlq")
	sq.attrs[qbase+"x-acct"] = qattrs("x-acct", "0", "0", "0", "arn:aws:sqs:ap-northeast-2:999988887777:shared-dlq")
	sq.attrErr[qbase+"broken"] = denied("AccessDenied")
	// calm-dlq 는 목록에 없어 GetQueueUrl 로 따로 읽는다. shared-dlq 는 못 찾는다
	sq.byName["calm-dlq"] = "https://sqs.ap-northeast-2.amazonaws.com/111122223333/calm-dlq"
	sq.attrs[sq.byName["calm-dlq"]] = map[string]string{attrVisible: "0"}
	ns, err := b.Children(ctx, "sqs")
	if err != nil {
		t.Fatal(err)
	}
	want := []struct {
		info string
		warn bool
	}{
		{"대기 12 · 처리 중 3 · 지연 0 · DLQ orders-dlq 2", true},
		{"대기 2 · 처리 중 0 · 지연 0", false},
		{"대기 0 · 처리 중 0 · 지연 1 · DLQ calm-dlq 0", false},
		{"대기 0 · 처리 중 0 · 지연 0 · DLQ shared-dlq ?", false},
		{"권한 없음: sqs:GetQueueAttributes", false},
	}
	for i, w := range want {
		if ns[i].Info != w.info || ns[i].Warn != w.warn || ns[i].Path != "sqs/"+ns[i].Name || !ns[i].Openable {
			t.Errorf("%d: %+v, want %+v", i, ns[i], w)
		}
	}
	slices.Sort(sq.urlCalls)
	if !reflect.DeepEqual(sq.urlCalls, []string{"calm-dlq@111122223333", "shared-dlq@999988887777"}) {
		t.Fatalf("목록에 없는 DLQ 만 GetQueueUrl: %v", sq.urlCalls)
	}
	if !slices.Equal(sq.maxResult, []int32{1000}) {
		t.Fatalf("MaxResults 가 없으면 1,000개에서 끊긴다: %v", sq.maxResult)
	}
	if sq.attrNames[0][0] != "All" {
		t.Fatalf("목록은 All 로 읽는다: %v", sq.attrNames[0])
	}
}

func TestSQS_큐_Enter_는_QueueUrl_먼저_속성_이름순이고_JSON_은_원문이다(t *testing.T) {
	allowBilled(t)
	b, _, sq, _ := newBackend()
	u := qbase + "orders"
	sq.byName["orders"] = u
	redrive := `{"deadLetterTargetArn":"arn:aws:sqs:ap-northeast-2:111122223333:orders-dlq","maxReceiveCount":5}`
	sq.attrs[u] = map[string]string{"VisibilityTimeout": "30", attrRedrive: redrive, attrVisible: "12"}
	res, err := b.Open(ctx, "sqs/orders", 50)
	if err != nil {
		t.Fatal(err)
	}
	want := [][]string{{"QueueUrl", u}, {attrVisible, "12"}, {attrRedrive, redrive}, {"VisibilityTimeout", "30"}}
	if !reflect.DeepEqual(res.Cols, []string{"attribute", "value"}) || !reflect.DeepEqual(res.Rows, want) {
		t.Fatalf("%v %v", res.Cols, res.Rows)
	}
}

func TestSQS_ARN_으로_열면_그_계정의_큐를_열고_다른_리전은_안내만_둔다(t *testing.T) {
	allowBilled(t)
	b, _, sq, _ := newBackend()
	b.region = "ap-northeast-2"
	u := "https://sqs.ap-northeast-2.amazonaws.com/999988887777/shared-dlq"
	sq.byName["shared-dlq"] = u
	sq.attrs[u] = map[string]string{attrVisible: "3"}
	res, err := b.Open(ctx, "sqs/arn:aws:sqs:ap-northeast-2:999988887777:shared-dlq", 50)
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(sq.urlCalls, []string{"shared-dlq@999988887777"}) || res.Rows[0][1] != u {
		t.Fatalf("ARN 의 계정으로 GetQueueUrl: %v %v", sq.urlCalls, res.Rows)
	}
	res, err = b.Open(ctx, "sqs/arn:aws:sqs:us-east-1:999988887777:shared-dlq", 50)
	if err != nil {
		t.Fatal(err)
	}
	if len(sq.urlCalls) != 1 || !reflect.DeepEqual(res.Rows[1], []string{"note", "다른 리전 큐는 수만 보임"}) {
		t.Fatalf("다른 리전은 부르지 않고 안내: %v %v", sq.urlCalls, res.Rows)
	}
}

// ─── Lambda ───

func TestLambda_함수_Info_는_런타임과_KST_수정_시각이고_Enter_는_이벤트_소스_매핑이다(t *testing.T) {
	b, _, _, la := newBackend()
	la.fns = []lambdatypes.FunctionConfiguration{
		{FunctionName: aws.String("worker"), Runtime: lambdatypes.RuntimeNodejs20x, LastModified: aws.String("2026-11-01T03:04:05.000+0000")},
		{FunctionName: aws.String("img"), PackageType: lambdatypes.PackageTypeImage, LastModified: aws.String("bad")},
	}
	la.mappings["worker"] = []lambdatypes.EventSourceMappingConfiguration{
		{EventSourceArn: aws.String("arn:aws:sqs:ap-northeast-2:111122223333:orders"), State: aws.String("Enabled"), BatchSize: aws.Int32(10), LastProcessingResult: aws.String("OK")},
		{EventSourceArn: aws.String("arn:aws:kinesis:ap-northeast-2:111122223333:stream/s"), State: aws.String("Disabled")},
	}
	ns, err := b.Children(ctx, "lambda")
	if err != nil {
		t.Fatal(err)
	}
	if ns[0].Info != "nodejs20.x · 2026-11-01 12:04" || ns[1].Info != "Image · bad" || ns[0].Path != "lambda/worker" || !ns[0].Openable || ns[0].HasChildren {
		t.Fatalf("함수: %+v", ns)
	}
	res, err := b.Open(ctx, "lambda/worker", 50)
	if err != nil {
		t.Fatal(err)
	}
	want := [][]string{
		{"arn:aws:sqs:ap-northeast-2:111122223333:orders", "Enabled", "10", "OK"},
		{"arn:aws:kinesis:ap-northeast-2:111122223333:stream/s", "Disabled", "", ""},
	}
	if !reflect.DeepEqual(res.Cols, []string{"source", "state", "batch", "last_result"}) || !reflect.DeepEqual(res.Rows, want) {
		t.Fatalf("%v %v", res.Cols, res.Rows)
	}
}

// ─── 대시보드 ───

func TestProbe_Lambda_수와_꺼진_매핑_실패한_매핑을_세고_요금_서비스는_부르지_않는다(t *testing.T) {
	allowBilled(t) // 허용해도 대시보드는 요금 서비스를 부르지 않는다(R72)
	b, la, trap := freeBackend()
	la.fns = []lambdatypes.FunctionConfiguration{{FunctionName: aws.String("a")}, {FunctionName: aws.String("b")}, {FunctionName: aws.String("c")}}
	la.mappings["a"] = []lambdatypes.EventSourceMappingConfiguration{
		{State: aws.String("Enabled"), LastProcessingResult: aws.String("OK")},
		{State: aws.String("Enabled"), LastProcessingResult: aws.String("No records processed")},
		{State: aws.String("Disabled"), LastProcessingResult: aws.String("OK")},
	}
	la.mappings["b"] = []lambdatypes.EventSourceMappingConfiguration{{State: aws.String("Enabled"), LastProcessingResult: aws.String("PROBLEM: Function call failed")}}
	h := b.Probe(ctx)
	if !h.OK || h.Summary() != "Lambda 3 · 매핑 꺼짐 1 · 실패 1" || h.Metrics[0].Warn || !h.Metrics[1].Warn || !h.Metrics[2].Warn {
		t.Fatalf("Probe = %+v", h)
	}
	if n := trap.calls.Load(); n != 0 {
		t.Fatalf("요금 서비스 호출 %d번", n)
	}
	la.mappings = map[string][]lambdatypes.EventSourceMappingConfiguration{"a": {{State: aws.String("Enabled"), LastProcessingResult: aws.String("OK")}}}
	if h := b.Probe(ctx); h.Summary() != "Lambda 3 · 매핑 꺼짐 0 · 실패 0" || h.Metrics[1].Warn || h.Metrics[2].Warn {
		t.Fatalf("문제가 없으면 경고가 아니다: %+v", h)
	}
}

func TestProbe_목록_실패는_실패다(t *testing.T) {
	b, la, _ := freeBackend()
	la.fnErr = denied("AccessDeniedException")
	if h := b.Probe(ctx); h.OK || h.Err != "aws lambda:ListFunctions: AccessDeniedException: not authorized" {
		t.Fatalf("Probe = %+v", h)
	}
	la.fnErr, la.mapErr = nil, denied("AccessDeniedException")
	if h := b.Probe(ctx); h.OK || !strings.Contains(h.Err, "lambda:ListEventSourceMappings") {
		t.Fatalf("Probe = %+v", h)
	}
}

// ─── 요금 막기 (R72) ───

func TestBilled_막혀_있으면_SNS_SQS_S3_폴더가_없고_경로로_와도_보내지_않는다(t *testing.T) {
	t.Setenv(source.AllowBilledEnv, "")
	b, _, trap := freeBackend()
	ns, err := b.Children(ctx, "")
	if err != nil || !reflect.DeepEqual(names(ns), []string{"Lambda", "EventBridge", "Scheduler", "Step Functions"}) {
		t.Fatalf("폴더: %v %v", names(ns), err)
	}
	for _, p := range []string{"sns", "sns/" + topicArn, "sqs", "s3", "s3/logs", "s3/logs/app/"} {
		if _, err := b.Children(ctx, p); !errors.Is(err, source.ErrBilled) || err.Error() != "요금이 붙는 조회라 막음 (DB_ALLOW_BILLED=1 로 허용)" {
			t.Errorf("Children %s: %v", p, err)
		}
	}
	for _, p := range []string{"sns/" + topicArn, "sqs/orders", "sqs/arn:aws:sqs:ap-northeast-2:111122223333:orders-dlq", "s3/logs/a.txt"} {
		if _, err := b.Open(ctx, p, 50); !errors.Is(err, source.ErrBilled) {
			t.Errorf("Open %s: %v", p, err)
		}
	}
	if n := trap.calls.Load(); n != 0 {
		t.Fatalf("막혔는데 요금 서비스를 %d번 부름", n)
	}
	// 1 일 때만 연다
	t.Setenv(source.AllowBilledEnv, "true")
	if ns, _ := b.Children(ctx, ""); len(ns) != 4 {
		t.Fatalf("1 이 아니면 막는다: %v", names(ns))
	}
	t.Setenv(source.AllowBilledEnv, "1")
	if ns, _ := b.Children(ctx, ""); len(ns) != 7 {
		t.Fatalf("허용하면 모든 폴더: %v", names(ns))
	}
}

func TestShortTime_은_KST_분까지이고_영_값은_빈_글이다(t *testing.T) {
	if got := shortTime(time.Date(2026, 11, 16, 15, 30, 59, 0, time.UTC)); got != "2026-11-17 00:30" {
		t.Fatal(got)
	}
	if shortTime(time.Time{}) != "" {
		t.Fatal("영 값")
	}
}
