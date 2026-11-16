package awsres

import (
	"context"
	"fmt"
	"strconv"
	"time"

	"github.com/aws/aws-sdk-go-v2/service/sqs/types"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
)

var _ source.Prober = (*Backend)(nil)

// probeQueues 는 대시보드 확인이 속성을 읽는 큐 수 상한이다. 5초(db.ProbeTimeout) 안에 parallel 개씩 왕복하려면
// 수백 개가 한계라 넘으면 앞쪽만 세고 `(앞 N개)` 를 붙인다.
const probeQueues = 200

// MetricDLQ 는 DLQ 에 메시지가 쌓인 큐 수의 수치 이름이다. 대시보드 요약 카드(R64.1)가 더한다
const MetricDLQ = "DLQ 쌓임"

// Probe 는 대시보드 확인이다(R64.2·R67.1): 큐 수와 DLQ 에 메시지가 쌓인 큐 수다. 지연은 ListQueues(모든 페이지) 시간이다.
// DLQ 는 같이 읽은 큐 속성에서 찾는다. 목록에 없는 DLQ(다른 계정)는 확인을 싸게 두려고 세지 않는다.
func (b *Backend) Probe(ctx context.Context) db.Health {
	start := time.Now()
	urls, err := b.listQueueURLs(ctx)
	latency := time.Since(start)
	if err != nil {
		return db.Failed(latency, err, "")
	}
	probed := urls[:min(len(urls), probeQueues)]
	attrs, errs := b.allQueueAttrs(ctx, probed, types.QueueAttributeNameQueueArn, types.QueueAttributeNameRedrivePolicy,
		types.QueueAttributeNameApproximateNumberOfMessages)
	byArn := map[string]map[string]string{}
	failed := 0
	for i, a := range attrs {
		if errs[i] != nil {
			failed++
			continue
		}
		byArn[a[attrArn]] = a
	}
	piled := 0
	for _, a := range byArn {
		if d, ok := byArn[dlqArn(a)]; ok {
			if n, _ := strconv.Atoi(d[attrVisible]); n > 0 {
				piled++
			}
		}
	}
	dlq := strconv.Itoa(piled)
	if len(probed) < len(urls) {
		dlq += fmt.Sprintf(" (앞 %d개)", len(probed))
	}
	ms := []db.Metric{{Name: "큐", Value: strconv.Itoa(len(urls))}, {Name: MetricDLQ, Value: dlq, Warn: piled > 0}}
	if failed > 0 {
		ms = append(ms, db.Metric{Name: "읽기 실패", Value: strconv.Itoa(failed), Warn: true})
	}
	return db.Health{OK: true, Latency: latency, Metrics: ms}
}
