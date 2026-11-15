package opensearch

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"strconv"
	"strings"
	"time"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
	"github.com/Jeong-Jae-Hun/i-db/internal/textutil"
)

var _ source.Prober = (*backend)(nil)

// Probe 는 대시보드 확인이다(R64.2): `_cluster/health` 상태·`_cat/indices` 인덱스 수와 문서 합·`GET /` 버전.
// 편집기 허용 경로(R61.2)를 거치지 않는 내부 읽기다. 지연은 `_cluster/health` 왕복이고, `green` 이 아니면 경고다.
// 인덱스·문서 수는 트리처럼 `.` 으로 시작하는 시스템 인덱스를 뺀다. 버전은 못 읽어도(권한) 실패로 보지 않는다.
func (b *backend) Probe(ctx context.Context) db.Health {
	start := time.Now()
	data, err := b.do(ctx, http.MethodGet, "_cluster/health", nil)
	latency := time.Since(start)
	if err != nil {
		return db.Failed(latency, err, b.password)
	}
	var health struct {
		Status string `json:"status"`
	}
	if err := json.Unmarshal(data, &health); err != nil || health.Status == "" {
		return db.Failed(latency, fmt.Errorf("opensearch: unexpected _cluster/health response"), b.password)
	}
	if data, err = b.do(ctx, http.MethodGet, "_cat/indices?format=json&h=index,docs.count", nil); err != nil {
		return db.Failed(latency, err, b.password)
	}
	var idx []struct {
		Index string `json:"index"`
		Docs  string `json:"docs.count"`
	}
	if err := json.Unmarshal(data, &idx); err != nil {
		return db.Failed(latency, fmt.Errorf("opensearch: decode _cat/indices: %w", err), b.password)
	}
	var indices, docs int64
	for _, i := range idx {
		if strings.HasPrefix(i.Index, ".") {
			continue
		}
		indices++
		n, _ := strconv.ParseInt(i.Docs, 10, 64) // 닫힌 인덱스는 docs.count 가 비어 있다
		docs += n
	}
	return db.Health{OK: true, Latency: latency, Version: b.version(ctx), Metrics: []db.Metric{
		{Value: health.Status, Warn: health.Status != "green"},
		{Name: "문서", Value: textutil.KoreanCount(docs)},
		{Name: "인덱스", Value: textutil.KoreanCount(indices)},
	}}
}

// version 은 `GET /` 의 배포판·버전이다(`OpenSearch 2.17.0`, distribution 이 없으면 Elasticsearch). 못 읽으면 빈 글이다.
func (b *backend) version(ctx context.Context) string {
	data, err := b.do(ctx, http.MethodGet, "", nil)
	if err != nil {
		return ""
	}
	var root struct {
		Version struct {
			Distribution string `json:"distribution"`
			Number       string `json:"number"`
		} `json:"version"`
	}
	if json.Unmarshal(data, &root) != nil || root.Version.Number == "" {
		return ""
	}
	if root.Version.Distribution == "opensearch" {
		return "OpenSearch " + root.Version.Number
	}
	return "Elasticsearch " + root.Version.Number
}
