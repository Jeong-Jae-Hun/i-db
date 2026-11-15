package obs

import (
	"bytes"
	"context"
	"fmt"
	"net/http"
	"os"
	"slices"
	"testing"
	"time"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
)

// DBTUI_TEST_LOKI_URL(예: http://localhost:53100) 이 있으면 줄 몇 개를 넣고 트리·질의로 다시 읽는다.
func TestIntegration_Loki_넣은_줄을_라벨_트리와_LogQL로_읽는다(t *testing.T) {
	base := os.Getenv("DBTUI_TEST_LOKI_URL")
	if base == "" {
		t.Skip("DBTUI_TEST_LOKI_URL not set")
	}
	run := fmt.Sprintf("r%d", time.Now().UnixNano())
	now := time.Now()
	body := fmt.Sprintf(`{"streams":[{"stream":{"job":"dbtui_obs_test","run":%q},"values":[["%d","첫 줄"],["%d","둘째 줄 error"],["%d","셋째 줄"]]}]}`,
		run, now.Add(-3*time.Second).UnixNano(), now.Add(-2*time.Second).UnixNano(), now.Add(-time.Second).UnixNano())
	resp, err := http.Post(base+"/loki/api/v1/push", "application/json", bytes.NewBufferString(body))
	if err != nil {
		t.Fatal(err)
	}
	resp.Body.Close()
	if resp.StatusCode/100 != 2 {
		t.Fatalf("push: %s", resp.Status)
	}

	b, err := NewLoki(db.Conn{Kind: db.KindLoki, URL: base})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	ctx := context.Background()
	sel := fmt.Sprintf(`{job="dbtui_obs_test",run=%q}`, run)

	// 수집이 늦을 수 있어 잠시 다시 묻는다
	var res *db.Result
	for deadline := time.Now().Add(15 * time.Second); time.Now().Before(deadline); time.Sleep(500 * time.Millisecond) {
		if res, err = b.Run(ctx, source.Query{Text: sel}); err != nil {
			t.Fatal(err)
		}
		if len(res.Rows) == 3 {
			break
		}
	}
	if len(res.Rows) != 3 {
		t.Fatalf("rows = %d, want 3", len(res.Rows))
	}
	line := slices.Index(res.Cols, "line")
	if res.Cols[0] != "time" || line < 0 || res.Rows[0][line] != "셋째 줄" {
		t.Fatalf("result = %v %v", res.Cols, res.Rows)
	}

	filtered, err := b.Run(ctx, source.Query{Text: sel + ` |= "error"`, Since: 15 * time.Minute})
	if err != nil {
		t.Fatal(err)
	}
	if len(filtered.Rows) != 1 {
		t.Errorf("filtered rows = %d", len(filtered.Rows))
	}

	metric, err := b.Run(ctx, source.Query{Text: fmt.Sprintf(`sum(count_over_time(%s[5m]))`, sel)})
	if err != nil {
		t.Fatal(err)
	}
	if metric.Cols[len(metric.Cols)-1] != "value" || len(metric.Rows) == 0 {
		t.Errorf("metric = %v %d rows", metric.Cols, len(metric.Rows))
	}

	labels, err := b.Children(ctx, "")
	if err != nil {
		t.Fatal(err)
	}
	if !slices.ContainsFunc(labels, func(n source.Node) bool { return n.Path == "run" }) {
		t.Errorf("labels = %+v", labels)
	}
	values, err := b.Children(ctx, "run")
	if err != nil {
		t.Fatal(err)
	}
	if !slices.ContainsFunc(values, func(n source.Node) bool { return n.Path == "run/"+run }) {
		t.Errorf("run values missing %s", run)
	}
	opened, err := b.Open(ctx, "run/"+run, 0)
	if err != nil {
		t.Fatal(err)
	}
	if len(opened.Rows) != 3 {
		t.Errorf("open rows = %d", len(opened.Rows))
	}
}

// DBTUI_TEST_PROM_URL(예: http://localhost:59090) 이 있으면 up 을 즉시값·범위로 읽는다.
func TestIntegration_Prometheus_up을_즉시값과_범위로_읽는다(t *testing.T) {
	base := os.Getenv("DBTUI_TEST_PROM_URL")
	if base == "" {
		t.Skip("DBTUI_TEST_PROM_URL not set")
	}
	b, err := NewPrometheus(db.Conn{Kind: db.KindPrometheus, URL: base})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	ctx := context.Background()

	names, err := b.Children(ctx, "")
	if err != nil {
		t.Fatal(err)
	}
	if !slices.ContainsFunc(names, func(n source.Node) bool { return n.Path == "up" }) {
		t.Fatalf("metric names missing up (%d names)", len(names))
	}
	inst, err := b.Open(ctx, "up", 0)
	if err != nil {
		t.Fatal(err)
	}
	if len(inst.Rows) == 0 || inst.Cols[0] != "time" || inst.Cols[len(inst.Cols)-1] != "value" {
		t.Fatalf("instant = %v %v", inst.Cols, inst.Rows)
	}
	rng, err := b.Run(ctx, source.Query{Text: "up", Since: 15 * time.Minute})
	if err != nil {
		t.Fatal(err)
	}
	if len(rng.Rows) == 0 {
		t.Errorf("range rows = 0")
	}
	if _, err := b.Run(ctx, source.Query{Text: "up{"}); err == nil {
		t.Error("bad PromQL must fail")
	}
}
