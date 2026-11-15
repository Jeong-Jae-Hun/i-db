package db

import "time"

// Health 는 대시보드(R64)가 접속 하나를 가볍게 살펴본 결과다. 읽기 전용 메타데이터만 묻는다.
type Health struct {
	OK      bool
	Latency time.Duration
	Version string
	Metrics []Metric // 종류별 주요 수치(활성 세션·클라이언트·메모리·문서 수 등)
	Err     string   // 비밀값을 지운 한 줄
}

// Metric 은 대시보드 한 칸이다. Warn 이면 눈에 띄게 그린다(임계치를 넘음).
type Metric struct {
	Name  string
	Value string
	Warn  bool
}
