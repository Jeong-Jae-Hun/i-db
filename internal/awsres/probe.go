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

var _ source.Prober = (*Backend)(nil)

// Probe 는 대시보드 확인이다(R64.2·R67.1): `Lambda 86 · 매핑 꺼짐 2 · 실패 1`. 대시보드는 30초마다 다시 보므로
// 요금이 붙지 않는 lambda:ListFunctions·ListEventSourceMappings 만 부르고, 요금 허용(R72)과 상관없이 SQS 는 묻지 않는다.
// 매핑 꺼짐은 상태가 Enabled 가 아닌 매핑, 실패는 마지막 처리 결과가 문제(MappingOK 가 아님)인 매핑 수다. 지연은 함수 목록 시간이다.
func (b *Backend) Probe(ctx context.Context) db.Health {
	start := time.Now()
	fns := 0
	p := lambda.NewListFunctionsPaginator(b.lambda, &lambda.ListFunctionsInput{})
	for p.HasMorePages() {
		page, err := p.NextPage(ctx)
		if err != nil {
			return db.Failed(time.Since(start), wrap("lambda:ListFunctions", err), "")
		}
		fns += len(page.Functions)
	}
	latency := time.Since(start)
	off, failed := 0, 0
	mp := lambda.NewListEventSourceMappingsPaginator(b.lambda, &lambda.ListEventSourceMappingsInput{})
	for mp.HasMorePages() {
		page, err := mp.NextPage(ctx)
		if err != nil {
			return db.Failed(latency, wrap("lambda:ListEventSourceMappings", err), "")
		}
		for _, m := range page.EventSourceMappings {
			if aws.ToString(m.State) != "Enabled" {
				off++
			}
			if !MappingOK(aws.ToString(m.LastProcessingResult)) {
				failed++
			}
		}
	}
	return db.Health{OK: true, Latency: latency, Metrics: []db.Metric{
		{Name: "Lambda", Value: strconv.Itoa(fns)},
		{Name: "매핑 꺼짐", Value: strconv.Itoa(off), Warn: off > 0},
		{Name: "실패", Value: strconv.Itoa(failed), Warn: failed > 0},
	}}
}

// MappingOK 는 매핑의 마지막 처리 결과가 문제 없음인지다. `No records processed` 는 메시지가 없던 매핑의 보통 값이라 경고하지 않는다.
func MappingOK(r string) bool { return r == "OK" || r == "" || r == "No records processed" }
