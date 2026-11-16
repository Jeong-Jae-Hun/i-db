// Package awsres 는 AWS 메시징·인프라를 트리로 보는 읽기 전용 저장소다(명세 R67). 편집기 질의 언어는 없고
// 트리 펼치기(Children)와 Enter(Open)만 있다. 부르는 API 는 List*·Get*·Describe*·Head* 뿐이다(readonly_test 가 지킨다).
package awsres

import (
	"context"
	"errors"
	"fmt"
	"strings"
	"sync"
	"time"

	"github.com/aws/aws-sdk-go-v2/config"
	"github.com/aws/aws-sdk-go-v2/service/eventbridge"
	"github.com/aws/aws-sdk-go-v2/service/lambda"
	"github.com/aws/aws-sdk-go-v2/service/s3"
	"github.com/aws/aws-sdk-go-v2/service/scheduler"
	"github.com/aws/aws-sdk-go-v2/service/sfn"
	"github.com/aws/aws-sdk-go-v2/service/sns"
	"github.com/aws/aws-sdk-go-v2/service/sqs"
	"github.com/aws/aws-sdk-go-v2/service/sts"
	"github.com/aws/smithy-go"
	"golang.org/x/sync/errgroup"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
)

// Backend 는 aws 접속 하나다. 서비스마다 클라이언트를 들고, 시험은 가짜로 바꿔 끼운다.
type Backend struct {
	sts       stsAPI
	sns       snsAPI
	sqs       sqsAPI
	lambda    lambdaAPI
	events    eventsAPI
	scheduler schedulerAPI
	sfn       sfnAPI
	s3        s3API
	regions   sync.Map // S3 버킷 이름 → 리전
}

type stsAPI interface {
	GetCallerIdentity(context.Context, *sts.GetCallerIdentityInput, ...func(*sts.Options)) (*sts.GetCallerIdentityOutput, error)
}

// ErrNoQuery 는 편집기 실행 거절이다. 통합(ui·cli)은 보내기 전에 종류로 막고, 이것은 그래도 온 실행의 안전망이다.
var ErrNoQuery = fmt.Errorf("%w: aws connection has no query language", source.ErrWrite)

// 목록 한 단계에서 동시에 부르는 Get* 수다. 큐·구독이 수백 개여도 API 한도(초당 수십~수백)를 넘지 않게 작게 둔다.
const parallel = 8

// New 는 접속을 만든다. 자격 증명은 저장하지 않고 Profile(없으면 표준 체인)로 받는다(R60.1). 실제 연결은 첫 호출 때 한다.
func New(ctx context.Context, c db.Conn) (source.Backend, error) {
	var opts []func(*config.LoadOptions) error
	if c.Region != "" {
		opts = append(opts, config.WithRegion(c.Region))
	}
	if c.Profile != "" {
		opts = append(opts, config.WithSharedConfigProfile(c.Profile))
	}
	cfg, err := config.LoadDefaultConfig(ctx, opts...)
	if err != nil {
		return nil, fmt.Errorf("load aws config: %w", err)
	}
	cfg.HTTPClient = source.HTTPClient() // 리다이렉트를 따라가지 않는다(3xx 는 SDK 가 오류로 낸다)
	return &Backend{
		sts:       sts.NewFromConfig(cfg),
		sns:       sns.NewFromConfig(cfg),
		sqs:       sqs.NewFromConfig(cfg),
		lambda:    lambda.NewFromConfig(cfg),
		events:    eventbridge.NewFromConfig(cfg),
		scheduler: scheduler.NewFromConfig(cfg),
		sfn:       sfn.NewFromConfig(cfg),
		s3:        s3.NewFromConfig(cfg),
	}, nil
}

func (b *Backend) Close() error { return nil }

// Run 은 늘 거절한다. aws 접속은 트리로 본다(R67.1).
func (b *Backend) Run(context.Context, source.Query) (*db.Result, error) { return nil, ErrNoQuery }

// ─── 트리 ───

// folders 는 맨 위 폴더다. key 가 경로의 첫 마디다(R67.1).
var folders = []struct{ key, name string }{
	{"sns", "SNS 토픽"},
	{"sqs", "SQS 큐"},
	{"lambda", "Lambda"},
	{"events", "EventBridge"},
	{"scheduler", "Scheduler"},
	{"sfn", "Step Functions"},
	{"s3", "S3"},
}

// Children 은 "" 이면 폴더, 그 밖은 서비스별 목록이다. 맨 위는 GetCallerIdentity 로 자격 증명부터 확인한다:
// 그러지 않으면 접속 시험(testConn)이 자격 증명 없이도 폴더 수만 보고 성공한다.
// 권한이 없는 목록은 오류 대신 `권한 없음: <작업>` 한 줄이라 다른 폴더는 그대로 쓴다.
func (b *Backend) Children(ctx context.Context, path string) ([]source.Node, error) {
	if path == "" {
		if _, err := b.sts.GetCallerIdentity(ctx, &sts.GetCallerIdentityInput{}); err != nil {
			return nil, wrap("sts:GetCallerIdentity", err)
		}
		out := make([]source.Node, len(folders))
		for i, f := range folders {
			out[i] = source.Node{Name: f.name, Path: f.key, HasChildren: true}
		}
		return out, nil
	}
	svc, rest, _ := strings.Cut(path, "/")
	var (
		out []source.Node
		err error
	)
	switch svc {
	case "sns":
		out, err = b.snsChildren(ctx, rest)
	case "sqs":
		if rest == "" {
			out, err = b.queues(ctx)
		}
	case "lambda":
		if rest == "" {
			out, err = b.functions(ctx)
		}
	case "events": // 규칙은 잎이라 rest 는 "" 또는 버스 이름이다
		out, err = b.eventsChildren(ctx, rest)
	case "scheduler":
		out, err = b.schedulerChildren(ctx, rest)
	case "sfn":
		if rest == "" {
			out, err = b.machines(ctx)
		}
	case "s3":
		out, err = b.s3Children(ctx, rest)
	}
	if action := deniedAction(err); action != "" {
		return []source.Node{{Name: "권한 없음: " + action, Path: "#denied:" + path}}, nil
	}
	return out, err
}

// Open 은 노드 Enter 의 표다. 경로 첫 마디로 서비스를 고른다. `권한 없음`·`… 앞 N개만` 줄(#로 시작)은 열 것이 없다.
func (b *Backend) Open(ctx context.Context, path string, _ int) (*db.Result, error) {
	svc, rest, _ := strings.Cut(path, "/")
	switch {
	case rest == "":
	case svc == "sns":
		return b.subscriptionTable(ctx, rest)
	case svc == "sqs":
		return b.queueTable(ctx, rest)
	case svc == "lambda":
		return b.mappingTable(ctx, rest)
	case svc == "events":
		return b.targetTable(ctx, rest)
	case svc == "scheduler":
		return b.scheduleTable(ctx, rest)
	case svc == "sfn":
		return b.executionTable(ctx, rest)
	case svc == "s3" && strings.Contains(rest, "/") && !strings.HasSuffix(rest, "/"):
		return b.objectTable(ctx, rest)
	}
	return nil, fmt.Errorf("nothing to open: %s", path)
}

// ─── 오류 ───

// opError 는 실패한 AWS 작업(IAM 이름, 예: sqs:ListQueues)과 원래 오류다.
type opError struct {
	action string
	err    error
}

func (e *opError) Error() string {
	var ae smithy.APIError
	if errors.As(e.err, &ae) {
		return fmt.Sprintf("aws %s: %s: %s", e.action, ae.ErrorCode(), ae.ErrorMessage())
	}
	return fmt.Sprintf("aws %s: %v", e.action, e.err)
}

func (e *opError) Unwrap() error { return e.err }

func wrap(action string, err error) error {
	if err == nil {
		return nil
	}
	var oe *opError
	if errors.As(err, &oe) {
		return err
	}
	return &opError{action: action, err: err}
}

// deniedAction 은 err 가 권한 거절이면 그 작업 이름이다. 서비스마다 코드가 달라(SNS AuthorizationError,
// SQS·S3 AccessDenied, Lambda·Scheduler AccessDeniedException) 이름에 AccessDenied 가 든 것과 둘을 더 본다.
func deniedAction(err error) string {
	var (
		oe *opError
		ae smithy.APIError
	)
	if !errors.As(err, &oe) || !errors.As(err, &ae) {
		return ""
	}
	code := ae.ErrorCode()
	if strings.Contains(code, "AccessDenied") || code == "AuthorizationError" || code == "UnauthorizedOperation" {
		return oe.action
	}
	return ""
}

// ─── 공통 ───

// each 는 fn(i) 를 0..n-1 에 parallel 개씩 동시에 부른다. fn 은 결과를 제 칸에 쓰고 오류는 스스로 담는다.
func each(n int, fn func(i int)) {
	var g errgroup.Group
	g.SetLimit(parallel)
	for i := range n {
		g.Go(func() error { fn(i); return nil })
	}
	_ = g.Wait()
}

var kst = time.FixedZone("KST", 9*3600)

// shortTime 은 KST `2006-01-02 15:04` 다. 영 값은 빈 글이다.
func shortTime(t time.Time) string {
	if t.IsZero() {
		return ""
	}
	return t.In(kst).Format("2006-01-02 15:04")
}

// longTime 은 KST 초까지다. 영 값은 빈 글이다.
func longTime(t time.Time) string {
	if t.IsZero() {
		return ""
	}
	return t.In(kst).Format("2006-01-02 15:04:05")
}

// arnPart 는 ARN 의 i 번째 마디다(arn:partition:service:region:account:resource). 없으면 빈 글이다.
// resource(5) 는 뒤에 ':' 가 더 있어도 통째다.
func arnPart(arn string, i int) string {
	parts := strings.SplitN(arn, ":", 6)
	if !strings.HasPrefix(arn, "arn:") || len(parts) <= i {
		return ""
	}
	return parts[i]
}

func table(cols ...string) *db.Result { return &db.Result{Cols: cols, Rows: [][]string{}} }
