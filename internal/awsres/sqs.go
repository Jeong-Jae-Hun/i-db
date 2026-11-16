package awsres

import (
	"context"
	"encoding/json"
	"fmt"
	"path"
	"slices"
	"strconv"
	"strings"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/sqs"
	"github.com/aws/aws-sdk-go-v2/service/sqs/types"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
)

// ─── SQS (R67.2) ───

type sqsAPI interface {
	sqs.ListQueuesAPIClient
	GetQueueAttributes(context.Context, *sqs.GetQueueAttributesInput, ...func(*sqs.Options)) (*sqs.GetQueueAttributesOutput, error)
	GetQueueUrl(context.Context, *sqs.GetQueueUrlInput, ...func(*sqs.Options)) (*sqs.GetQueueUrlOutput, error)
}

const (
	attrArn      = "QueueArn"
	attrRedrive  = "RedrivePolicy"
	attrVisible  = "ApproximateNumberOfMessages"
	attrInFlight = "ApproximateNumberOfMessagesNotVisible"
	attrDelayed  = "ApproximateNumberOfMessagesDelayed"
)

// listQueueURLs 는 큐 URL 전부다. MaxResults 를 주지 않으면 ListQueues 는 1,000개에서 다음 토큰 없이 끊는다.
func (b *Backend) listQueueURLs(ctx context.Context) ([]string, error) {
	var out []string
	p := sqs.NewListQueuesPaginator(b.sqs, &sqs.ListQueuesInput{MaxResults: aws.Int32(1000)})
	for p.HasMorePages() {
		page, err := p.NextPage(ctx)
		if err != nil {
			return nil, wrap("sqs:ListQueues", err)
		}
		out = append(out, page.QueueUrls...)
	}
	return out, nil
}

func (b *Backend) queueAttrs(ctx context.Context, url string, names ...types.QueueAttributeName) (map[string]string, error) {
	out, err := b.sqs.GetQueueAttributes(ctx, &sqs.GetQueueAttributesInput{QueueUrl: aws.String(url), AttributeNames: names})
	if err != nil {
		return nil, wrap("sqs:GetQueueAttributes", err)
	}
	return out.Attributes, nil
}

// allQueueAttrs 는 urls 마다 names 속성을 parallel 개씩 동시에 읽는다. 실패한 큐는 errs 칸에 남는다.
func (b *Backend) allQueueAttrs(ctx context.Context, urls []string, names ...types.QueueAttributeName) ([]map[string]string, []error) {
	attrs, errs := make([]map[string]string, len(urls)), make([]error, len(urls))
	each(len(urls), func(i int) { attrs[i], errs[i] = b.queueAttrs(ctx, urls[i], names...) })
	return attrs, errs
}

// dlqArn 은 RedrivePolicy 의 deadLetterTargetArn 이다. 정책이 없거나 읽을 수 없으면 빈 글이다.
func dlqArn(attrs map[string]string) string {
	var rp struct {
		Target string `json:"deadLetterTargetArn"`
	}
	if json.Unmarshal([]byte(attrs[attrRedrive]), &rp) != nil {
		return ""
	}
	return rp.Target
}

// queues 는 큐 목록이다. Info 는 `대기 · 처리 중 · 지연` 이고 DLQ 가 있으면 그 큐의 대기 수를 붙인다(R67.2).
// DLQ 는 보통 같은 목록에 있어 그 속성을 쓰고, 없으면(다른 계정 등) GetQueueUrl 로 따로 읽는다. DLQ 에 메시지가 있으면 경고다.
func (b *Backend) queues(ctx context.Context) ([]source.Node, error) {
	urls, err := b.listQueueURLs(ctx)
	if err != nil {
		return nil, err
	}
	attrs, errs := b.allQueueAttrs(ctx, urls, types.QueueAttributeNameAll)
	byArn := map[string]map[string]string{}
	for i, a := range attrs {
		if errs[i] == nil {
			byArn[a[attrArn]] = a
		}
	}
	out := make([]source.Node, len(urls))
	each(len(urls), func(i int) {
		name := path.Base(urls[i])
		n := source.Node{Name: name, Path: "sqs/" + name, Openable: true}
		if errs[i] != nil {
			n.Info = "속성 읽기 실패"
			if action := deniedAction(errs[i]); action != "" {
				n.Info = "권한 없음: " + action
			}
			out[i] = n
			return
		}
		a := attrs[i]
		n.Info = fmt.Sprintf("대기 %s · 처리 중 %s · 지연 %s", a[attrVisible], a[attrInFlight], a[attrDelayed])
		if arn := dlqArn(a); arn != "" {
			d, ok := byArn[arn]
			if !ok {
				d, _ = b.dlqAttrs(ctx, arn) // 못 읽으면 수는 ? 로 둔다
			}
			cnt, ok := d[attrVisible]
			if !ok {
				cnt = "?"
			}
			n.Info += " · DLQ " + arnPart(arn, 5) + " " + cnt
			v, _ := strconv.Atoi(cnt)
			n.Warn = v > 0
		}
		out[i] = n
	})
	return out, nil
}

// dlqAttrs 는 목록에 없는 DLQ 의 대기 수를 ARN 으로 읽는다.
func (b *Backend) dlqAttrs(ctx context.Context, arn string) (map[string]string, error) {
	u, err := b.sqs.GetQueueUrl(ctx, &sqs.GetQueueUrlInput{QueueName: aws.String(arnPart(arn, 5)), QueueOwnerAWSAccountId: aws.String(arnPart(arn, 4))})
	if err != nil {
		return nil, wrap("sqs:GetQueueUrl", err)
	}
	return b.queueAttrs(ctx, aws.ToString(u.QueueUrl), types.QueueAttributeNameApproximateNumberOfMessages)
}

// queueTable 은 큐 Enter 의 `attribute, value` 표다. 첫 줄은 QueueUrl 이고 나머지는 속성 이름순이다.
// RedrivePolicy·Policy 같은 JSON 은 원문 그대로다(값 보기 R32 로 연다). name 이 ARN 이면 그 계정의 큐를 연다(흐름의 DLQ).
// 접속과 다른 리전 큐는 이 리전 클라이언트로 열 수 없어 안내 한 줄만 둔다.
func (b *Backend) queueTable(ctx context.Context, name string) (*db.Result, error) {
	in := &sqs.GetQueueUrlInput{QueueName: aws.String(name)}
	if strings.HasPrefix(name, "arn:") {
		if r := arnPart(name, 3); b.region != "" && r != b.region {
			res := table("attribute", "value")
			res.Rows = append(res.Rows, []string{"QueueArn", name}, []string{"note", "다른 리전 큐는 수만 보임"})
			return res, nil
		}
		in = &sqs.GetQueueUrlInput{QueueName: aws.String(arnPart(name, 5)), QueueOwnerAWSAccountId: aws.String(arnPart(name, 4))}
	}
	u, err := b.sqs.GetQueueUrl(ctx, in)
	if err != nil {
		return nil, wrap("sqs:GetQueueUrl", err)
	}
	url := aws.ToString(u.QueueUrl)
	attrs, err := b.queueAttrs(ctx, url, types.QueueAttributeNameAll)
	if err != nil {
		return nil, err
	}
	res := table("attribute", "value")
	res.Rows = append(res.Rows, []string{"QueueUrl", url})
	keys := make([]string, 0, len(attrs))
	for k := range attrs {
		keys = append(keys, k)
	}
	slices.Sort(keys)
	for _, k := range keys {
		res.Rows = append(res.Rows, []string{k, attrs[k]})
	}
	return res, nil
}
