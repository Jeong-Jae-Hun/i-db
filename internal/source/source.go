// Package source 는 PostgreSQL·Redis 밖의 읽기 전용 저장소(OpenSearch·Loki·Prometheus·Grafana·DynamoDB)를
// 같은 트리·편집기·결과 표로 다루는 공통 틀이다(명세 R60~R63). 종류마다 하위 패키지가 Backend 를 구현하고
// 통합(ui·cli)은 이 인터페이스만 본다.
package source

import (
	"context"
	"errors"
	"time"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
)

// Node 는 트리 한 줄이다. Path 는 Children·Open 에 그대로 돌려줄 종류별 경로다(예: "index/field").
type Node struct {
	Name        string
	Path        string
	Info        string // 타입·개수 같은 오른쪽 흐린 글
	HasChildren bool
	Openable    bool // Enter 로 Open 할 수 있다(인덱스·테이블·라벨 값)
}

// Query 는 편집기 실행 한 번이다. Since·Until 은 Loki·Prometheus 범위이고 0 이면 종류 기본값(1시간)을 쓴다.
type Query struct {
	Text  string
	Since time.Duration
	Until time.Duration
	Limit int  // 행 상한. 0 이면 1,000
	Force bool // ErrNeedsConfirm 을 확인받은 뒤 다시 보낼 때
}

// Backend 는 한 접속이다. 모두 읽기 전용이고, 쓰기로 보이는 질의는 Run 이 보내기 전에 ErrWrite 로 거절한다.
type Backend interface {
	Children(ctx context.Context, path string) ([]Node, error) // path "" 는 맨 위
	Run(ctx context.Context, q Query) (*db.Result, error)
	Open(ctx context.Context, path string, limit int) (*db.Result, error) // 트리 Enter 미리 보기
	Close() error
}

var (
	// ErrWrite 는 읽기 전용 저장소에 쓰기로 보이는 질의를 보냈을 때다
	ErrWrite = errors.New("read-only source: write requests are not sent")
	// ErrNeedsConfirm 은 전체 스캔·큰 범위처럼 비싼 질의다. 통합이 확인받은 뒤 Query.Force 로 다시 보낸다
	ErrNeedsConfirm = errors.New("expensive query needs confirmation")
)

// Secret 은 .env 에서 접속 비밀값을 고른다. 비밀번호(basic)·토큰(grafana)을 같은 자리에서 읽는다(R59.1 키 규칙).
type Secret func(c db.Conn) string
