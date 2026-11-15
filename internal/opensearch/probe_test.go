package opensearch

import (
	"context"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"strings"
	"testing"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
)

func probe(t *testing.T, routes map[string]string, c db.Conn) (db.Health, func() []seen) {
	t.Helper()
	srv, got := fakeServer(t, routes)
	c.URL = srv.URL
	return newBackend(t, c).(source.Prober).Probe(context.Background()), got
}

func TestProbe_상태_문서_합_인덱스_수와_버전이고_시스템_인덱스는_뺀다(t *testing.T) {
	h, got := probe(t, map[string]string{
		"GET /_cluster/health": `{"cluster_name":"c","status":"green","number_of_nodes":3}`,
		"GET /_cat/indices": `[{"index":"logs-1","docs.count":"1000000"},{"index":"logs-2","docs.count":"200000"},
			{"index":"closed","docs.count":null},{"index":".kibana_1","docs.count":"99999999"}]`,
		"GET /": `{"version":{"distribution":"opensearch","number":"2.17.0"}}`,
	}, db.Conn{Auth: "basic", User: "u", Password: "pw"})
	if !h.OK || h.Summary() != "green · 문서 120만 · 인덱스 3" || h.Version != "OpenSearch 2.17.0" || h.Metrics[0].Warn {
		t.Fatalf("Probe = %+v (%s)", h, h.Summary())
	}
	for _, r := range got() {
		if r.method != "GET" || r.auth == "" {
			t.Errorf("요청 %+v: GET·basic 인증이어야 함", r)
		}
	}
}

func TestProbe_green이_아니면_경고이고_버전을_못_읽어도_OK다(t *testing.T) {
	for _, status := range []string{"yellow", "red"} {
		h, _ := probe(t, map[string]string{
			"GET /_cluster/health": `{"status":"` + status + `"}`,
			"GET /_cat/indices":    `[]`,
		}, db.Conn{})
		if !h.OK || !h.Metrics[0].Warn || h.Metrics[0].Value != status || h.Version != "" {
			t.Errorf("%s: Probe = %+v", status, h)
		}
	}
}

func TestProbe_오류는_실패이고_비밀번호를_지운다(t *testing.T) {
	// 서버가 비밀번호를 되돌려주는 꼴을 흉내 낸다
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		w.WriteHeader(http.StatusUnauthorized)
		_, _ = io.WriteString(w, `{"error":{"type":"security_exception","reason":"bad password hunter2"}}`)
	}))
	t.Cleanup(srv.Close)
	h := newBackend(t, db.Conn{URL: srv.URL, Auth: "basic", User: "u", Password: "hunter2"}).(source.Prober).Probe(context.Background())
	if h.OK || !strings.Contains(h.Err, "opensearch 401: security_exception") || strings.Contains(h.Err, "hunter2") {
		t.Fatalf("Probe = %+v", h)
	}
	h, _ = probe(t, map[string]string{"GET /_cluster/health": `<html>`}, db.Conn{})
	if h.OK || !strings.Contains(h.Err, "_cluster/health") {
		t.Fatalf("깨진 응답 Probe = %+v", h)
	}
}

func TestIntegration_실제_OpenSearch_를_확인한다(t *testing.T) {
	base := os.Getenv("DBTUI_TEST_OPENSEARCH_URL")
	if base == "" {
		t.Skip("DBTUI_TEST_OPENSEARCH_URL not set")
	}
	ctx, cancel := context.WithTimeout(context.Background(), db.ProbeTimeout)
	defer cancel()
	h := newBackend(t, db.Conn{URL: base}).(source.Prober).Probe(ctx)
	if !h.OK || !strings.HasPrefix(h.Version, "OpenSearch ") || len(h.Metrics) != 3 || h.Latency <= 0 {
		t.Fatalf("Probe = %+v", h)
	}
}
