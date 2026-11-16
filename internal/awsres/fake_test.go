package awsres

import (
	"context"
	"errors"
	"sync"
	"sync/atomic"
	"testing"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/lambda"
	lambdatypes "github.com/aws/aws-sdk-go-v2/service/lambda/types"
	"github.com/aws/aws-sdk-go-v2/service/s3"
	"github.com/aws/aws-sdk-go-v2/service/sns"
	snstypes "github.com/aws/aws-sdk-go-v2/service/sns/types"
	"github.com/aws/aws-sdk-go-v2/service/sqs"
	"github.com/aws/aws-sdk-go-v2/service/sts"
	"github.com/aws/smithy-go"

	"github.com/Jeong-Jae-Hun/i-db/internal/source"
)

// allowBilled 는 요금이 붙는 조회(R72)를 허용하고 시험이 끝나면 되돌린다. SNS·SQS·S3 시험은 이 허용 아래서 돈다.
func allowBilled(t *testing.T) { t.Setenv(source.AllowBilledEnv, "1") }

// ─── 가짜 AWS ───
// 서비스마다 정한 값을 돌려주고 받은 요청을 센다. 목록 Get* 을 동시에 부르므로 잠근다.

func denied(code string) error { return &smithy.GenericAPIError{Code: code, Message: "not authorized"} }

type fakeSTS struct {
	err   error
	calls atomic.Int32
}

func (f *fakeSTS) GetCallerIdentity(context.Context, *sts.GetCallerIdentityInput, ...func(*sts.Options)) (*sts.GetCallerIdentityOutput, error) {
	f.calls.Add(1)
	if f.err != nil {
		return nil, f.err
	}
	return &sts.GetCallerIdentityOutput{Account: aws.String("111122223333"), Arn: aws.String("arn:aws:iam::111122223333:user/dev")}, nil
}

type fakeSNS struct {
	mu        sync.Mutex
	topics    [][]string // 페이지마다 토픽 ARN
	topicErr  error
	subs      map[string][]snstypes.Subscription
	attrs     map[string]map[string]string // 구독 ARN → 속성
	attrCalls []string
}

func (f *fakeSNS) ListTopics(_ context.Context, in *sns.ListTopicsInput, _ ...func(*sns.Options)) (*sns.ListTopicsOutput, error) {
	if f.topicErr != nil {
		return nil, f.topicErr
	}
	i := 0
	if in.NextToken != nil {
		i = int((*in.NextToken)[0] - '0')
	}
	out := &sns.ListTopicsOutput{}
	if i < len(f.topics) {
		for _, a := range f.topics[i] {
			out.Topics = append(out.Topics, snstypes.Topic{TopicArn: aws.String(a)})
		}
	}
	if i+1 < len(f.topics) {
		out.NextToken = aws.String(string(rune('0' + i + 1)))
	}
	return out, nil
}

func (f *fakeSNS) ListSubscriptionsByTopic(_ context.Context, in *sns.ListSubscriptionsByTopicInput, _ ...func(*sns.Options)) (*sns.ListSubscriptionsByTopicOutput, error) {
	return &sns.ListSubscriptionsByTopicOutput{Subscriptions: f.subs[aws.ToString(in.TopicArn)]}, nil
}

func (f *fakeSNS) GetSubscriptionAttributes(_ context.Context, in *sns.GetSubscriptionAttributesInput, _ ...func(*sns.Options)) (*sns.GetSubscriptionAttributesOutput, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.attrCalls = append(f.attrCalls, aws.ToString(in.SubscriptionArn))
	return &sns.GetSubscriptionAttributesOutput{Attributes: f.attrs[aws.ToString(in.SubscriptionArn)]}, nil
}

type fakeSQS struct {
	mu        sync.Mutex
	urls      []string
	listErr   error
	attrs     map[string]map[string]string // URL → 속성
	attrErr   map[string]error
	byName    map[string]string // 이름 → URL (GetQueueUrl)
	maxResult []int32
	attrNames [][]string
	urlCalls  []string
}

func (f *fakeSQS) ListQueues(_ context.Context, in *sqs.ListQueuesInput, _ ...func(*sqs.Options)) (*sqs.ListQueuesOutput, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.maxResult = append(f.maxResult, aws.ToInt32(in.MaxResults))
	if f.listErr != nil {
		return nil, f.listErr
	}
	return &sqs.ListQueuesOutput{QueueUrls: f.urls}, nil
}

func (f *fakeSQS) GetQueueAttributes(_ context.Context, in *sqs.GetQueueAttributesInput, _ ...func(*sqs.Options)) (*sqs.GetQueueAttributesOutput, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	var names []string
	for _, n := range in.AttributeNames {
		names = append(names, string(n))
	}
	f.attrNames = append(f.attrNames, names)
	u := aws.ToString(in.QueueUrl)
	if err := f.attrErr[u]; err != nil {
		return nil, err
	}
	return &sqs.GetQueueAttributesOutput{Attributes: f.attrs[u]}, nil
}

func (f *fakeSQS) GetQueueUrl(_ context.Context, in *sqs.GetQueueUrlInput, _ ...func(*sqs.Options)) (*sqs.GetQueueUrlOutput, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.urlCalls = append(f.urlCalls, aws.ToString(in.QueueName)+"@"+aws.ToString(in.QueueOwnerAWSAccountId))
	u, ok := f.byName[aws.ToString(in.QueueName)]
	if !ok {
		return nil, &smithy.GenericAPIError{Code: "AWS.SimpleQueueService.NonExistentQueue", Message: "no queue"}
	}
	return &sqs.GetQueueUrlOutput{QueueUrl: aws.String(u)}, nil
}

type fakeLambda struct {
	mu            sync.Mutex
	fns           []lambdatypes.FunctionConfiguration
	mappings      map[string][]lambdatypes.EventSourceMappingConfiguration
	fnErr, mapErr error
	sources       []string // EventSourceArn 으로 고른 요청
}

func (f *fakeLambda) ListFunctions(context.Context, *lambda.ListFunctionsInput, ...func(*lambda.Options)) (*lambda.ListFunctionsOutput, error) {
	if f.fnErr != nil {
		return nil, f.fnErr
	}
	return &lambda.ListFunctionsOutput{Functions: f.fns}, nil
}

// ListEventSourceMappings 는 FunctionName 으로 고르고, EventSourceArn 을 주면 모든 함수에서 그 소스의 매핑을 모은다.
// 둘 다 없으면 모든 매핑이다(대시보드 확인).
func (f *fakeLambda) ListEventSourceMappings(_ context.Context, in *lambda.ListEventSourceMappingsInput, _ ...func(*lambda.Options)) (*lambda.ListEventSourceMappingsOutput, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	if f.mapErr != nil {
		return nil, f.mapErr
	}
	if in.EventSourceArn == nil && in.FunctionName == nil {
		out := &lambda.ListEventSourceMappingsOutput{}
		for _, ms := range f.mappings {
			out.EventSourceMappings = append(out.EventSourceMappings, ms...)
		}
		return out, nil
	}
	if src := aws.ToString(in.EventSourceArn); src != "" {
		f.sources = append(f.sources, src)
		out := &lambda.ListEventSourceMappingsOutput{}
		for _, ms := range f.mappings {
			for _, m := range ms {
				if aws.ToString(m.EventSourceArn) == src {
					out.EventSourceMappings = append(out.EventSourceMappings, m)
				}
			}
		}
		return out, nil
	}
	return &lambda.ListEventSourceMappingsOutput{EventSourceMappings: f.mappings[aws.ToString(in.FunctionName)]}, nil
}

func newBackend() (*Backend, *fakeSNS, *fakeSQS, *fakeLambda) {
	sn, sq, la := &fakeSNS{subs: map[string][]snstypes.Subscription{}, attrs: map[string]map[string]string{}},
		&fakeSQS{attrs: map[string]map[string]string{}, attrErr: map[string]error{}, byName: map[string]string{}},
		&fakeLambda{mappings: map[string][]lambdatypes.EventSourceMappingConfiguration{}}
	return &Backend{sts: &fakeSTS{}, sns: sn, sqs: sq, lambda: la}, sn, sq, la
}

// ─── 요금 서비스 덫 (R72) ───

// billedTrap 은 요청마다 요금이 붙는 SNS·SQS·S3 의 가짜다. 어떤 메서드든 부르면 calls 를 세고 오류를 돌려준다.
// 대시보드·흐름·막힌 트리가 이것을 한 번도 부르지 않는지 본다.
type billedTrap struct{ calls atomic.Int32 }

var errBilledCall = errors.New("billed call")

func (b *billedTrap) ListTopics(context.Context, *sns.ListTopicsInput, ...func(*sns.Options)) (*sns.ListTopicsOutput, error) {
	b.calls.Add(1)
	return nil, errBilledCall
}

func (b *billedTrap) ListSubscriptionsByTopic(context.Context, *sns.ListSubscriptionsByTopicInput, ...func(*sns.Options)) (*sns.ListSubscriptionsByTopicOutput, error) {
	b.calls.Add(1)
	return nil, errBilledCall
}

func (b *billedTrap) GetSubscriptionAttributes(context.Context, *sns.GetSubscriptionAttributesInput, ...func(*sns.Options)) (*sns.GetSubscriptionAttributesOutput, error) {
	b.calls.Add(1)
	return nil, errBilledCall
}

func (b *billedTrap) ListQueues(context.Context, *sqs.ListQueuesInput, ...func(*sqs.Options)) (*sqs.ListQueuesOutput, error) {
	b.calls.Add(1)
	return nil, errBilledCall
}

func (b *billedTrap) GetQueueAttributes(context.Context, *sqs.GetQueueAttributesInput, ...func(*sqs.Options)) (*sqs.GetQueueAttributesOutput, error) {
	b.calls.Add(1)
	return nil, errBilledCall
}

func (b *billedTrap) GetQueueUrl(context.Context, *sqs.GetQueueUrlInput, ...func(*sqs.Options)) (*sqs.GetQueueUrlOutput, error) {
	b.calls.Add(1)
	return nil, errBilledCall
}

func (b *billedTrap) ListBuckets(context.Context, *s3.ListBucketsInput, ...func(*s3.Options)) (*s3.ListBucketsOutput, error) {
	b.calls.Add(1)
	return nil, errBilledCall
}

func (b *billedTrap) ListObjectsV2(context.Context, *s3.ListObjectsV2Input, ...func(*s3.Options)) (*s3.ListObjectsV2Output, error) {
	b.calls.Add(1)
	return nil, errBilledCall
}

func (b *billedTrap) GetBucketLocation(context.Context, *s3.GetBucketLocationInput, ...func(*s3.Options)) (*s3.GetBucketLocationOutput, error) {
	b.calls.Add(1)
	return nil, errBilledCall
}

func (b *billedTrap) HeadObject(context.Context, *s3.HeadObjectInput, ...func(*s3.Options)) (*s3.HeadObjectOutput, error) {
	b.calls.Add(1)
	return nil, errBilledCall
}

func (b *billedTrap) GetObject(context.Context, *s3.GetObjectInput, ...func(*s3.Options)) (*s3.GetObjectOutput, error) {
	b.calls.Add(1)
	return nil, errBilledCall
}

// freeBackend 는 SNS·SQS·S3 자리에 덫을 끼운 Backend 다. 요금 없는 sts·lambda 는 보통 가짜다.
func freeBackend() (*Backend, *fakeLambda, *billedTrap) {
	trap := &billedTrap{}
	la := &fakeLambda{mappings: map[string][]lambdatypes.EventSourceMappingConfiguration{}}
	return &Backend{sts: &fakeSTS{}, sns: trap, sqs: trap, s3: trap, lambda: la, region: "ap-northeast-2"}, la, trap
}
