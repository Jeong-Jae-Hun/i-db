package awsres

import (
	"context"
	"strconv"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/lambda"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
)

// ─── Lambda (R67.3) ───

type lambdaAPI interface {
	lambda.ListFunctionsAPIClient
	lambda.ListEventSourceMappingsAPIClient
}

// lambdaTime 은 FunctionConfiguration.LastModified 의 꼴이다(ISO 8601, 예: 2026-11-01T03:04:05.000+0000).
const lambdaTime = "2006-01-02T15:04:05.000-0700"

// functions 는 함수 목록이다. Info 는 런타임과 마지막 수정 시각(KST)이다. 컨테이너 이미지 함수는 런타임이 없다.
func (b *Backend) functions(ctx context.Context) ([]source.Node, error) {
	var out []source.Node
	p := lambda.NewListFunctionsPaginator(b.lambda, &lambda.ListFunctionsInput{})
	for p.HasMorePages() {
		page, err := p.NextPage(ctx)
		if err != nil {
			return nil, wrap("lambda:ListFunctions", err)
		}
		for _, f := range page.Functions {
			name, mod := aws.ToString(f.FunctionName), aws.ToString(f.LastModified)
			if t, err := time.Parse(lambdaTime, mod); err == nil {
				mod = shortTime(t)
			}
			info := string(f.Runtime)
			if info == "" {
				info = string(f.PackageType)
			}
			out = append(out, source.Node{Name: name, Path: "lambda/" + name, Info: info + " · " + mod, Openable: true})
		}
	}
	return out, nil
}

// mappingTable 은 함수 Enter 의 이벤트 소스 매핑 표다(SQS·Kinesis·DynamoDB 스트림 등).
func (b *Backend) mappingTable(ctx context.Context, fn string) (*db.Result, error) {
	res := table("source", "state", "batch", "last_result")
	p := lambda.NewListEventSourceMappingsPaginator(b.lambda, &lambda.ListEventSourceMappingsInput{FunctionName: aws.String(fn)})
	for p.HasMorePages() {
		page, err := p.NextPage(ctx)
		if err != nil {
			return nil, wrap("lambda:ListEventSourceMappings", err)
		}
		for _, m := range page.EventSourceMappings {
			batch := ""
			if m.BatchSize != nil {
				batch = strconv.Itoa(int(*m.BatchSize))
			}
			res.Rows = append(res.Rows, []string{aws.ToString(m.EventSourceArn), aws.ToString(m.State), batch, aws.ToString(m.LastProcessingResult)})
		}
	}
	return res, nil
}
