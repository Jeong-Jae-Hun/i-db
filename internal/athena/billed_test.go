package athena

import (
	"context"
	"errors"
	"os"
	"testing"

	"github.com/aws/aws-sdk-go-v2/service/athena/types"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
)

// TestMain 은 질의·카탈로그 시험이 요금 막음(R72) 아래서 돌지 않게 패키지 전체를 허용으로 둔다.
// 막힌 경우는 아래 시험이 t.Setenv 로 따로 본다.
func TestMain(m *testing.M) {
	os.Setenv(source.AllowBilledEnv, "1")
	os.Exit(m.Run())
}

func TestBilled_막혀_있으면_트리_미리_보기_질의를_보내지_않고_작업_그룹_확인만_된다(t *testing.T) {
	t.Setenv(source.AllowBilledEnv, "")
	// GetWorkGroup 말고는 구현이 없는 가짜다. 다른 작업을 부르면 내장한 nil api 로 패닉해 시험이 실패한다
	f := &fakeAPI{wg: &types.WorkGroup{State: types.WorkGroupStateEnabled}}
	b := testBackend(f, db.Conn{Name: "athena", Kind: db.KindAthena, Region: "ap-northeast-2"})
	ctx := context.Background()
	for _, p := range []string{"", "sales", "sales/orders"} {
		if _, err := b.Children(ctx, p); !errors.Is(err, source.ErrBilled) {
			t.Errorf("Children %q: %v", p, err)
		}
	}
	if _, err := b.Open(ctx, "sales/orders", 50); !errors.Is(err, source.ErrBilled) {
		t.Errorf("Open: %v", err)
	}
	if _, err := b.Run(ctx, source.Query{Text: "SELECT 1 LIMIT 1", Force: true}); !errors.Is(err, source.ErrBilled) {
		t.Errorf("Run: %v", err)
	}
	if len(f.started) != 0 {
		t.Fatalf("막혔는데 질의를 보냄: %d", len(f.started))
	}
	if h := b.Probe(ctx); !h.OK || h.Summary() != "ENABLED" {
		t.Fatalf("GetWorkGroup 은 요금이 없어 그대로다: %+v", h)
	}
}
