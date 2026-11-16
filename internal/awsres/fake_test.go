package awsres

import (
	"context"
	"sync"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/lambda"
	lambdatypes "github.com/aws/aws-sdk-go-v2/service/lambda/types"
	"github.com/aws/aws-sdk-go-v2/service/sns"
	snstypes "github.com/aws/aws-sdk-go-v2/service/sns/types"
	"github.com/aws/aws-sdk-go-v2/service/sqs"
	"github.com/aws/aws-sdk-go-v2/service/sts"
	"github.com/aws/smithy-go"
)

// ─── 가짜 AWS ───
// 서비스마다 정한 값을 돌려주고 받은 요청을 센다. 목록 Get* 을 동시에 부르므로 잠근다.

func denied(code string) error { return &smithy.GenericAPIError{Code: code, Message: "not authorized"} }

type fakeSTS struct{ err error }

func (f *fakeSTS) GetCallerIdentity(context.Context, *sts.GetCallerIdentityInput, ...func(*sts.Options)) (*sts.GetCallerIdentityOutput, error) {
	if f.err != nil {
		return nil, f.err
	}
	return &sts.GetCallerIdentityOutput{Account: aws.String("111122223333")}, nil
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
	fns      []lambdatypes.FunctionConfiguration
	mappings map[string][]lambdatypes.EventSourceMappingConfiguration
}

func (f *fakeLambda) ListFunctions(context.Context, *lambda.ListFunctionsInput, ...func(*lambda.Options)) (*lambda.ListFunctionsOutput, error) {
	return &lambda.ListFunctionsOutput{Functions: f.fns}, nil
}

func (f *fakeLambda) ListEventSourceMappings(_ context.Context, in *lambda.ListEventSourceMappingsInput, _ ...func(*lambda.Options)) (*lambda.ListEventSourceMappingsOutput, error) {
	return &lambda.ListEventSourceMappingsOutput{EventSourceMappings: f.mappings[aws.ToString(in.FunctionName)]}, nil
}

func newBackend() (*Backend, *fakeSNS, *fakeSQS, *fakeLambda) {
	sn, sq, la := &fakeSNS{subs: map[string][]snstypes.Subscription{}, attrs: map[string]map[string]string{}},
		&fakeSQS{attrs: map[string]map[string]string{}, attrErr: map[string]error{}, byName: map[string]string{}},
		&fakeLambda{mappings: map[string][]lambdatypes.EventSourceMappingConfiguration{}}
	return &Backend{sts: &fakeSTS{}, sns: sn, sqs: sq, lambda: la}, sn, sq, la
}
