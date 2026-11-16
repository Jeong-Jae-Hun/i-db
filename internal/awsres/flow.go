package awsres

import (
	"context"
	"fmt"
	"strings"
	"unicode/utf8"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/lambda"
	"github.com/aws/aws-sdk-go-v2/service/sns"
	"github.com/aws/aws-sdk-go-v2/service/sqs"
	"github.com/aws/aws-sdk-go-v2/service/sqs/types"
)

// ─── 이벤트 흐름 (R69) ───
// 토픽 하나에서 구독 → 같은 계정 SQS 큐(대기·처리 중·지연·DLQ) → 그 큐를 읽는 Lambda 매핑까지 따라간다.

// Topic 은 토픽 하나를 풀어 낸 흐름이다.
type Topic struct {
	Arn, Name string
	Subs      []Sub
}

// Sub 는 구독 하나다. 같은 계정 SQS 큐면 Queue 가 있고, 그 밖(lambda·https·email·다른 계정 큐)은 Endpoint 만 보인다.
type Sub struct {
	Protocol, Endpoint string
	Queue              *Queue
}

// Queue 는 구독한 큐의 수다. 속성을 못 읽었으면 Err 다. DLQ 가 없으면 DLQ 가 비고, 수를 못 읽었으면 DLQCount 가 `?` 다.
// DLQArn 은 DLQ 의 ARN 이다. 다른 계정 DLQ 도 상세(Open `sqs/<ARN>`)가 같은 이름의 지금 계정 큐로 가지 않게 남긴다.
type Queue struct {
	Name, Arn                  string
	Visible, InFlight, Delayed string
	DLQ, DLQArn, DLQCount      string
	Lambdas                    []Mapping
	Err                        string
}

// Mapping 은 큐를 읽는 Lambda 이벤트 소스 매핑이다.
type Mapping struct {
	Function, State, LastResult string
}

// TopicFlow 는 topic(이름 또는 ARN)을 풀어 낸다. 이름이면 ListTopics 에서 같은 이름을 찾는다.
// 큐 하나가 실패해도 그 큐의 Err 로 남기고 나머지는 그대로 보인다.
func (b *Backend) TopicFlow(ctx context.Context, topic string) (*Topic, error) {
	arn, err := b.topicArn(ctx, topic)
	if err != nil {
		return nil, err
	}
	subs, err := b.subscriptions(ctx, arn)
	if err != nil {
		return nil, err
	}
	out := &Topic{Arn: arn, Name: arnPart(arn, 5), Subs: make([]Sub, len(subs))}
	each(len(subs), func(i int) {
		s := subs[i]
		proto, ep := aws.ToString(s.Protocol), aws.ToString(s.Endpoint)
		out.Subs[i] = Sub{Protocol: proto, Endpoint: maskEndpoint(proto, ep)}
		if proto == "sqs" && arnPart(ep, 4) != "" && arnPart(ep, 4) == arnPart(arn, 4) {
			out.Subs[i].Endpoint = arnPart(ep, 5)
			out.Subs[i].Queue = b.flowQueue(ctx, ep)
		}
	})
	return out, nil
}

// topicArn 은 ARN 이면 그대로, 이름이면 ListTopics 에서 끝 마디가 같은 토픽이다.
func (b *Backend) topicArn(ctx context.Context, topic string) (string, error) {
	if strings.HasPrefix(topic, "arn:") {
		return topic, nil
	}
	p := sns.NewListTopicsPaginator(b.sns, &sns.ListTopicsInput{})
	for p.HasMorePages() {
		page, err := p.NextPage(ctx)
		if err != nil {
			return "", wrap("sns:ListTopics", err)
		}
		for _, t := range page.Topics {
			if arn := aws.ToString(t.TopicArn); arnPart(arn, 5) == topic {
				return arn, nil
			}
		}
	}
	return "", fmt.Errorf("topic not found: %s", topic)
}

// flowQueue 는 큐 ARN 의 수·DLQ·Lambda 매핑이다.
func (b *Backend) flowQueue(ctx context.Context, arn string) *Queue {
	q := &Queue{Name: arnPart(arn, 5), Arn: arn}
	u, err := b.sqs.GetQueueUrl(ctx, &sqs.GetQueueUrlInput{QueueName: aws.String(q.Name)})
	if err != nil {
		q.Err = wrap("sqs:GetQueueUrl", err).Error()
		return q
	}
	a, err := b.queueAttrs(ctx, aws.ToString(u.QueueUrl), types.QueueAttributeNameApproximateNumberOfMessages,
		types.QueueAttributeNameApproximateNumberOfMessagesNotVisible, types.QueueAttributeNameApproximateNumberOfMessagesDelayed,
		types.QueueAttributeNameRedrivePolicy)
	if err != nil {
		q.Err = err.Error()
		return q
	}
	q.Visible, q.InFlight, q.Delayed = a[attrVisible], a[attrInFlight], a[attrDelayed]
	if dlq := dlqArn(a); dlq != "" {
		q.DLQ, q.DLQArn, q.DLQCount = arnPart(dlq, 5), dlq, "?"
		if d, err := b.dlqAttrs(ctx, dlq); err == nil && d[attrVisible] != "" {
			q.DLQCount = d[attrVisible]
		}
	}
	q.Lambdas, err = b.queueMappings(ctx, arn)
	if err != nil {
		q.Err = err.Error()
	}
	return q
}

// queueMappings 는 EventSourceArn 이 큐인 Lambda 매핑이다. 함수 이름은 FunctionArn 끝 마디다.
func (b *Backend) queueMappings(ctx context.Context, queueArn string) ([]Mapping, error) {
	var out []Mapping
	p := lambda.NewListEventSourceMappingsPaginator(b.lambda, &lambda.ListEventSourceMappingsInput{EventSourceArn: aws.String(queueArn)})
	for p.HasMorePages() {
		page, err := p.NextPage(ctx)
		if err != nil {
			return out, wrap("lambda:ListEventSourceMappings", err)
		}
		for _, m := range page.EventSourceMappings {
			fn := aws.ToString(m.FunctionArn)
			if i := strings.LastIndex(fn, ":function:"); i >= 0 {
				fn = fn[i+len(":function:"):]
			}
			out = append(out, Mapping{Function: fn, State: aws.ToString(m.State), LastResult: aws.ToString(m.LastProcessingResult)})
		}
	}
	return out, nil
}

// maskEndpoint 는 email 구독의 받는 사람을 `a***@domain` 으로 가린다. 개인 메일 주소가 화면·CLI 출력에 그대로 남지 않게 한다.
func maskEndpoint(proto, ep string) string {
	if proto != "email" && proto != "email-json" {
		return ep
	}
	local, domain, ok := strings.Cut(ep, "@")
	if !ok || local == "" {
		return "***"
	}
	r, _ := utf8.DecodeRuneInString(local)
	return string(r) + "***@" + domain
}
