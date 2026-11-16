package athena

import (
	"context"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/athena"
	"github.com/aws/aws-sdk-go-v2/service/athena/types"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
)

var _ source.Prober = (*Backend)(nil)

// Probe 는 대시보드 확인이다(R64.2): GetWorkGroup 한 번으로 작업 그룹 상태(`ENABLED`)와 질의당 스캔 상한을 보인다.
// 버전 열은 작업 그룹의 엔진 판(`Athena engine version 3`)이다. ENABLED 가 아니면 경고 색이다.
func (b *Backend) Probe(ctx context.Context) db.Health {
	start := time.Now()
	out, err := b.cl.GetWorkGroup(ctx, &athena.GetWorkGroupInput{WorkGroup: aws.String(b.workgroup)})
	latency := time.Since(start)
	if err != nil {
		return db.Failed(latency, wrap("GetWorkGroup", err), "")
	}
	h := db.Health{OK: true, Latency: latency}
	wg := out.WorkGroup
	if wg == nil {
		return h
	}
	h.Metrics = append(h.Metrics, db.Metric{Value: string(wg.State), Warn: wg.State != types.WorkGroupStateEnabled})
	if cfg := wg.Configuration; cfg != nil {
		if cfg.BytesScannedCutoffPerQuery != nil {
			h.Metrics = append(h.Metrics, db.Metric{Name: "스캔 상한", Value: humanBytes(*cfg.BytesScannedCutoffPerQuery)})
		}
		if cfg.EngineVersion != nil {
			h.Version = aws.ToString(cfg.EngineVersion.EffectiveEngineVersion)
		}
	}
	return h
}
