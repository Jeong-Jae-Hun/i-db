package dynamo

import (
	"context"
	"time"

	"github.com/aws/aws-sdk-go-v2/service/dynamodb"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
	"github.com/Jeong-Jae-Hun/i-db/internal/textutil"
)

var _ source.Prober = (*Backend)(nil)

// Probe 는 대시보드 확인이다(R64.2): ListTables 로 테이블 수만 센다. 지연은 첫 페이지 왕복이다.
// DynamoDB 는 서버 버전이 없어 Version 은 비어 있다.
func (b *Backend) Probe(ctx context.Context) db.Health {
	start := time.Now()
	var (
		tables  int64
		latency time.Duration
	)
	p := dynamodb.NewListTablesPaginator(b.cl, &dynamodb.ListTablesInput{})
	for p.HasMorePages() {
		page, err := p.NextPage(ctx)
		if latency == 0 {
			latency = time.Since(start)
		}
		if err != nil {
			return db.Failed(latency, wrap("ListTables", err), "")
		}
		tables += int64(len(page.TableNames))
	}
	return db.Health{OK: true, Latency: latency, Metrics: []db.Metric{{Name: "테이블", Value: textutil.KoreanCount(tables)}}}
}
