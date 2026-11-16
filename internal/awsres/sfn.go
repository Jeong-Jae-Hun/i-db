package awsres

import (
	"context"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/sfn"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
)

// ─── Step Functions (R67.5) ───

type sfnAPI interface {
	sfn.ListStateMachinesAPIClient
	ListExecutions(context.Context, *sfn.ListExecutionsInput, ...func(*sfn.Options)) (*sfn.ListExecutionsOutput, error)
}

// recentExecutions 는 상태 기계 Enter 가 보이는 최근 실행 수다. ListExecutions 한 페이지(최신순)만 받는다.
const recentExecutions = 50

// machines 는 상태 기계 목록이다(Info 는 STANDARD·EXPRESS 와 만든 시각). 경로는 sfn/<ARN> 이다.
func (b *Backend) machines(ctx context.Context) ([]source.Node, error) {
	var out []source.Node
	p := sfn.NewListStateMachinesPaginator(b.sfn, &sfn.ListStateMachinesInput{})
	for p.HasMorePages() {
		page, err := p.NextPage(ctx)
		if err != nil {
			return nil, wrap("states:ListStateMachines", err)
		}
		for _, m := range page.StateMachines {
			out = append(out, source.Node{Name: aws.ToString(m.Name), Path: "sfn/" + aws.ToString(m.StateMachineArn),
				Info: string(m.Type) + " · " + shortTime(aws.ToTime(m.CreationDate)), Openable: true})
		}
	}
	return out, nil
}

// executionTable 은 최근 실행 50개 표다. 시각은 KST 초까지이고 실행 중이면 stop 이 빈 칸이다.
func (b *Backend) executionTable(ctx context.Context, arn string) (*db.Result, error) {
	out, err := b.sfn.ListExecutions(ctx, &sfn.ListExecutionsInput{StateMachineArn: aws.String(arn), MaxResults: recentExecutions})
	if err != nil {
		return nil, wrap("states:ListExecutions", err)
	}
	res := table("name", "status", "start", "stop")
	for _, e := range out.Executions {
		res.Rows = append(res.Rows, []string{aws.ToString(e.Name), string(e.Status), longTime(aws.ToTime(e.StartDate)), longTime(aws.ToTime(e.StopDate))})
	}
	res.Truncated = aws.ToString(out.NextToken) != ""
	return res, nil
}
