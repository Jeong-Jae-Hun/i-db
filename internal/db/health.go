package db

import (
	"strings"
	"time"
)

// 확인 제한 시간(R64.2). ProbeTimeout 은 질의 한 번, ProbeOpenTimeout 은 아직 열지 않은 접속을 여는 데 쓴다.
// 여는 쪽이 긴 까닭은 Redis 가 평문 3초 뒤 TLS 로 다시 붙고(CNAME 조회 3초 포함) PostgreSQL connect_timeout 이 8초라서다.
// 확인 함수가 ctx 에 스스로 건다. 테스트가 줄인다
var (
	ProbeTimeout     = 5 * time.Second
	ProbeOpenTimeout = 15 * time.Second
)

// Health 는 대시보드(R64)가 접속 하나를 가볍게 살펴본 결과다. 읽기 전용 메타데이터만 묻는다.
type Health struct {
	OK      bool
	Latency time.Duration
	Version string
	Metrics []Metric // 종류별 주요 수치(활성 세션·클라이언트·메모리·문서 수 등)
	Err     string   // 비밀값을 지운 한 줄
}

// Metric 은 대시보드 한 칸이다. Warn 이면 눈에 띄게 그린다(임계치를 넘음).
// Name 이 비면 값만 보인다(OpenSearch `green`·Loki `ready` 같은 상태 낱말).
type Metric struct {
	Name  string
	Value string
	Warn  bool
}

// String 은 칸 글자다. 예: `세션 12/200`, `green`.
func (m Metric) String() string { return strings.TrimSpace(m.Name + " " + m.Value) }

// Summary 는 주요 수치를 ` · ` 로 이은 한 줄이다. 예: `green · 문서 120만 · 인덱스 37`.
func (h Health) Summary() string {
	parts := make([]string, len(h.Metrics))
	for i, m := range h.Metrics {
		parts[i] = m.String()
	}
	return strings.Join(parts, " · ")
}

// Failed 는 확인 실패 결과다. 오류에서 secret 을 지우고 여러 줄이면 한 줄로 붙인다(R64.2).
func Failed(latency time.Duration, err error, secret string) Health {
	return Health{Latency: latency, Err: strings.Join(strings.Fields(Redact(err.Error(), secret)), " ")}
}
