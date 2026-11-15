// Package source 는 PostgreSQL·Redis 밖의 읽기 전용 저장소(OpenSearch·Loki·Prometheus·Grafana·DynamoDB)를
// 같은 트리·편집기·결과 표로 다루는 공통 틀이다(명세 R60~R63). 종류마다 하위 패키지가 Backend 를 구현하고
// 통합(ui·cli)은 이 인터페이스만 본다.
package source

import (
	"context"
	"errors"
	"fmt"
	"net/http"
	"strings"
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

// Prober 는 대시보드(R64.2)용 가벼운 확인을 하는 Backend 다. 읽기 전용 메타데이터만 묻고, 오류는 비밀값을 지운
// db.Health{OK: false} 로 돌려준다. 제한 시간은 호출부가 ctx 에 건다(db.ProbeTimeout).
type Prober interface {
	Probe(ctx context.Context) db.Health
}

var (
	// ErrWrite 는 읽기 전용 저장소에 쓰기로 보이는 질의를 보냈을 때다
	ErrWrite = errors.New("read-only source: write requests are not sent")
	// ErrNeedsConfirm 은 전체 스캔·큰 범위처럼 비싼 질의다. 통합이 확인받은 뒤 Query.Force 로 다시 보낸다.
	// 까닭은 NeedsConfirm 으로 감싸 싣고 ConfirmReason 으로 꺼낸다(확인 창·CLI 거절 글, R60.2)
	ErrNeedsConfirm = errors.New("expensive query needs confirmation")
)

// ─── 종류 공통 값 (R60~R63) ───

const (
	// MaxBody 는 HTTP 응답 본문 상한이다. 결과 표는 MaxRows 행이라 이보다 큰 응답은 질의가 잘못된 것으로 본다
	MaxBody = 16 << 20
	// OpenLimit 은 트리 Enter 미리 보기 행 수다(R61.1·R63.1). Loki 만 LokiOpenLimit 줄이다(R62.1)
	OpenLimit     = 50
	LokiOpenLimit = 100
	// DefaultRange 는 Loki·Prometheus 범위·라벨 값 기본이다(R62)
	DefaultRange = time.Hour
	// ConfirmRange 를 넘는 Loki·Prometheus 범위는 확인을 받는다(R60.2)
	ConfirmRange = 7 * 24 * time.Hour
)

// NeedsConfirm 은 까닭(한국어, 확인 창에 그대로 보임)을 실은 ErrNeedsConfirm 이다.
func NeedsConfirm(reason string) error { return fmt.Errorf("%w: %s", ErrNeedsConfirm, reason) }

// ConfirmReason 은 err 에 실린 확인 까닭이다. ErrNeedsConfirm 이 아니거나 까닭이 없으면 빈 글이다.
func ConfirmReason(err error) string {
	if !errors.Is(err, ErrNeedsConfirm) {
		return ""
	}
	_, reason, _ := strings.Cut(err.Error(), ErrNeedsConfirm.Error()+": ")
	return reason
}

// HTTPClient 는 저장소 HTTP 클라이언트다. 리다이렉트를 따라가지 않는다: 따라가면 basic·토큰 인증 헤더를
// 다른 주소로 다시 보내거나(같은 호스트의 다른 경로) 허용 경로 검사(R61.2)를 우회할 수 있다. 3xx 는 CheckStatus 가 오류로 만든다.
func HTTPClient() *http.Client {
	return &http.Client{
		Timeout:       2 * time.Minute,
		CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse },
	}
}

// RedirectError 는 3xx 응답이면 `redirect not followed: <상태>` 오류다. 그 밖은 nil 이다.
func RedirectError(resp *http.Response) error {
	if resp.StatusCode/100 != 3 {
		return nil
	}
	return fmt.Errorf("redirect not followed: %s", resp.Status)
}

// Secret 은 .env 에서 접속 비밀값을 고른다. 비밀번호(basic)·토큰(grafana)을 같은 자리에서 읽는다(R59.1 키 규칙).
type Secret func(c db.Conn) string
