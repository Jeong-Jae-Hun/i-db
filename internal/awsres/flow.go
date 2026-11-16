package awsres

import (
	"context"
	"errors"
	"fmt"
	"slices"
	"strings"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/lambda"
	"github.com/aws/aws-sdk-go-v2/service/sts"
)

// ─── 이벤트 흐름 (R69) ───
// 설정에 적은 큐마다 그 큐를 읽는 Lambda 매핑을 찾는다. 토픽·큐·DLQ 이름은 설정의 글 그대로이고, 큐 수·DLQ 수는
// SQS 요청마다 요금이 붙어 묻지 않는다(R72). 그래서 부르는 API 는 sts:GetCallerIdentity·lambda:ListEventSourceMappings 뿐이다.

// QueueRef 는 흐름 설정의 큐 하나다. Name·DLQ 는 이름 또는 ARN 이다.
type QueueRef struct{ Name, DLQ string }

// Queue 는 풀어 낸 큐다. Arn 은 매핑을 찾은 ARN 이고 DLQArn 은 상세(Open `sqs/<ARN>`)로 열 DLQ 다.
// 매핑을 못 읽었으면 Err 다.
type Queue struct {
	Name, Arn   string
	DLQ, DLQArn string
	Lambdas     []Mapping
	Err         string
}

// Mapping 은 큐를 읽는 Lambda 이벤트 소스 매핑이다.
type Mapping struct {
	Function, State, LastResult string
}

// QueueFlows 는 refs 마다 Lambda 매핑을 찾는다. 이름은 GetCallerIdentity 의 계정(이름이 하나라도 있을 때 한 번)과 접속 리전으로
// ARN 을 만들고, DLQ 이름은 그 큐와 같은 계정·리전으로 본다. 계정을 못 읽으면 오류이고, 큐 하나의 실패는 그 큐의 Err 다.
func (b *Backend) QueueFlows(ctx context.Context, refs []QueueRef) ([]Queue, error) {
	partition, account := "aws", ""
	if slices.ContainsFunc(refs, func(r QueueRef) bool { return !strings.HasPrefix(r.Name, "arn:") }) {
		if b.region == "" {
			return nil, errors.New("aws connection has no region")
		}
		id, err := b.sts.GetCallerIdentity(ctx, &sts.GetCallerIdentityInput{})
		if err != nil {
			return nil, wrap("sts:GetCallerIdentity", err)
		}
		account = aws.ToString(id.Account)
		if p := arnPart(aws.ToString(id.Arn), 1); p != "" {
			partition = p
		}
	}
	out := make([]Queue, len(refs))
	each(len(refs), func(i int) {
		r := refs[i]
		arn := r.Name
		if !strings.HasPrefix(arn, "arn:") {
			arn = fmt.Sprintf("arn:%s:sqs:%s:%s:%s", partition, b.region, account, r.Name)
		}
		q := Queue{Name: arnPart(arn, 5), Arn: arn, DLQArn: r.DLQ}
		if r.DLQ != "" && !strings.HasPrefix(r.DLQ, "arn:") {
			q.DLQArn = fmt.Sprintf("arn:%s:sqs:%s:%s:%s", arnPart(arn, 1), arnPart(arn, 3), arnPart(arn, 4), r.DLQ)
		}
		q.DLQ = arnPart(q.DLQArn, 5)
		var err error
		if q.Lambdas, err = b.queueMappings(ctx, arn); err != nil {
			q.Err = err.Error()
		}
		out[i] = q
	})
	return out, nil
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
