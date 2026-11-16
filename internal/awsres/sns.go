package awsres

import (
	"context"
	"strings"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/sns"
	"github.com/aws/aws-sdk-go-v2/service/sns/types"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
)

// ─── SNS (R67.2) ───

type snsAPI interface {
	sns.ListTopicsAPIClient
	sns.ListSubscriptionsByTopicAPIClient
	GetSubscriptionAttributes(context.Context, *sns.GetSubscriptionAttributesInput, ...func(*sns.Options)) (*sns.GetSubscriptionAttributesOutput, error)
}

// snsChildren 은 rest "" 면 토픽, "<토픽 ARN>" 이면 구독이다. 토픽 경로는 sns/<ARN> 이고 ARN 에는 / 가 없다.
func (b *Backend) snsChildren(ctx context.Context, rest string) ([]source.Node, error) {
	if rest == "" {
		var out []source.Node
		p := sns.NewListTopicsPaginator(b.sns, &sns.ListTopicsInput{})
		for p.HasMorePages() {
			page, err := p.NextPage(ctx)
			if err != nil {
				return nil, wrap("sns:ListTopics", err)
			}
			for _, t := range page.Topics {
				arn := aws.ToString(t.TopicArn)
				out = append(out, source.Node{Name: arnPart(arn, 5), Path: "sns/" + arn, HasChildren: true, Openable: true})
			}
		}
		return out, nil
	}
	if strings.Contains(rest, "/") { // 구독 줄은 잎이다
		return nil, nil
	}
	subs, err := b.subscriptions(ctx, rest)
	if err != nil {
		return nil, err
	}
	out := make([]source.Node, len(subs))
	for i, s := range subs {
		out[i] = source.Node{Name: subEndpoint(rest, s), Info: aws.ToString(s.Protocol), Path: "sns/" + rest + "/" + aws.ToString(s.SubscriptionArn)}
	}
	return out, nil
}

func (b *Backend) subscriptions(ctx context.Context, topic string) ([]types.Subscription, error) {
	var out []types.Subscription
	p := sns.NewListSubscriptionsByTopicPaginator(b.sns, &sns.ListSubscriptionsByTopicInput{TopicArn: aws.String(topic)})
	for p.HasMorePages() {
		page, err := p.NextPage(ctx)
		if err != nil {
			return nil, wrap("sns:ListSubscriptionsByTopic", err)
		}
		out = append(out, page.Subscriptions...)
	}
	return out, nil
}

// subEndpoint 는 구독 대상의 보일 글이다. 토픽과 같은 계정의 SQS 큐는 큐 이름으로 줄인다(R67.2).
func subEndpoint(topic string, s types.Subscription) string {
	ep := aws.ToString(s.Endpoint)
	if aws.ToString(s.Protocol) == "sqs" && arnPart(ep, 4) != "" && arnPart(ep, 4) == arnPart(topic, 4) {
		return arnPart(ep, 5)
	}
	return ep
}

// subscriptionTable 은 토픽 Enter 의 구독 표다. raw·filter 는 확인된 구독마다 GetSubscriptionAttributes 로 읽는다.
// 확인 전 구독(ARN 이 PendingConfirmation)은 속성을 물을 수 없어 빈 칸이다.
func (b *Backend) subscriptionTable(ctx context.Context, topic string) (*db.Result, error) {
	subs, err := b.subscriptions(ctx, topic)
	if err != nil {
		return nil, err
	}
	attrs := make([]map[string]string, len(subs))
	errs := make([]error, len(subs))
	each(len(subs), func(i int) {
		arn := aws.ToString(subs[i].SubscriptionArn)
		if !strings.HasPrefix(arn, "arn:") {
			return
		}
		out, err := b.sns.GetSubscriptionAttributes(ctx, &sns.GetSubscriptionAttributesInput{SubscriptionArn: aws.String(arn)})
		if err != nil {
			errs[i] = wrap("sns:GetSubscriptionAttributes", err)
			return
		}
		attrs[i] = out.Attributes
	})
	res := table("protocol", "endpoint", "subscription_arn", "raw", "filter")
	for i, s := range subs {
		raw, filter := attrs[i]["RawMessageDelivery"], attrs[i]["FilterPolicy"]
		if errs[i] != nil {
			raw = errs[i].Error()
		}
		res.Rows = append(res.Rows, []string{aws.ToString(s.Protocol), subEndpoint(topic, s), aws.ToString(s.SubscriptionArn), raw, filter})
	}
	return res, nil
}
