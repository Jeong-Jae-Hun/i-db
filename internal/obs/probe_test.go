package obs

import (
	"context"
	"net/url"
	"os"
	"strings"
	"testing"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
)

func TestLokiProbe_ready와_최근_1시간_라벨_수와_버전이다(t *testing.T) {
	f := newFake(t, map[string]func(url.Values) (int, string){
		"/ready":                        ok("ready\n"),
		"/loki/api/v1/labels":           success(`["app","job","level"]`),
		"/loki/api/v1/status/buildinfo": ok(`{"version":"3.0.0","revision":"x"}`),
	})
	h := newLokiAt(t, f.srv.URL, db.Conn{}).Probe(context.Background())
	if !h.OK || h.Summary() != "ready · 라벨 3" || h.Version != "Loki 3.0.0" || h.Latency <= 0 {
		t.Fatalf("Probe = %+v (%s)", h, h.Summary())
	}
	f.mu.Lock()
	labels := f.reqs[1]
	f.mu.Unlock()
	eq(t, "labels start", labels.query.Get("start"), ns(fixedNow.Add(-source.DefaultRange)))
}

func TestLokiProbe_준비되지_않으면_실패이고_비밀번호를_지운다(t *testing.T) {
	f := newFake(t, map[string]func(url.Values) (int, string){
		"/ready": func(url.Values) (int, string) { return 503, "Ingester not ready: pw=s3cret" },
	})
	h := newLokiAt(t, f.srv.URL, db.Conn{Auth: "basic", User: "u", Password: "s3cret"}).Probe(context.Background())
	if h.OK || !strings.Contains(h.Err, "loki 503") || strings.Contains(h.Err, "s3cret") {
		t.Fatalf("Probe = %+v", h)
	}
}

func TestPrometheusProbe_up_대상_수이고_down_이_있으면_경고다(t *testing.T) {
	vector := func(v string) string {
		return `{"resultType":"vector","result":[{"metric":{},"value":[1763175600,"` + v + `"]}]}`
	}
	cases := []struct {
		down, want string
		warn       bool
	}{
		{"", "up 380/380", false}, // count(up == 0) 은 down 이 없으면 빈 vector 다
		{"8", "up 372/380", true},
	}
	for _, c := range cases {
		f := newFake(t, map[string]func(url.Values) (int, string){
			"/api/v1/query": func(q url.Values) (int, string) {
				switch q.Get("query") {
				case "count(up)":
					return 200, `{"status":"success","data":` + vector("380") + `}`
				case "count(up == 0)":
					if c.down == "" {
						return 200, `{"status":"success","data":{"resultType":"vector","result":[]}}`
					}
					return 200, `{"status":"success","data":` + vector(c.down) + `}`
				}
				return 400, `{"status":"error","error":"unexpected query"}`
			},
			"/api/v1/status/buildinfo": success(`{"version":"2.53.0","revision":"x"}`),
		})
		h := newPromAt(t, f.srv.URL, db.Conn{}).Probe(context.Background())
		if !h.OK || h.Summary() != c.want || h.Metrics[0].Warn != c.warn || h.Version != "Prometheus 2.53.0" {
			t.Errorf("down %q: Probe = %+v", c.down, h)
		}
	}
}

func TestPrometheusProbe_buildinfo_가_없어도_OK이고_질의_오류는_토큰을_지운다(t *testing.T) {
	f := newFake(t, map[string]func(url.Values) (int, string){
		"/api/v1/query": success(`{"resultType":"vector","result":[]}`),
	})
	h := newPromAt(t, f.srv.URL, db.Conn{}).Probe(context.Background())
	if !h.OK || h.Summary() != "up 0/0" || h.Version != "" {
		t.Fatalf("Probe = %+v", h)
	}
	f = newFake(t, map[string]func(url.Values) (int, string){
		"/api/v1/query": func(url.Values) (int, string) { return 401, `{"status":"error","error":"bad token tok-123"}` },
	})
	h = newPromAt(t, f.srv.URL, db.Conn{Auth: "token", Password: "tok-123"}).Probe(context.Background())
	if h.OK || !strings.Contains(h.Err, "401") || strings.Contains(h.Err, "tok-123") {
		t.Fatalf("Probe = %+v", h)
	}
}

func grafanaProbe(t *testing.T, health func(url.Values) (int, string)) db.Health {
	t.Helper()
	f := newFake(t, map[string]func(url.Values) (int, string){"/grafana/api/health": health})
	b, err := NewGrafana(db.Conn{URL: f.srv.URL + "/grafana/", Auth: "token", Password: "glsa_x"})
	if err != nil {
		t.Fatal(err)
	}
	return b.(source.Prober).Probe(context.Background())
}

func TestGrafanaProbe_database_상태와_버전이고_ok_가_아니면_경고다(t *testing.T) {
	h := grafanaProbe(t, ok(`{"commit":"abc","database":"ok","version":"11.4.0"}`))
	if !h.OK || h.Summary() != "ok" || h.Version != "Grafana 11.4.0" || h.Metrics[0].Warn {
		t.Fatalf("Probe = %+v", h)
	}
	if h := grafanaProbe(t, ok(`{"database":"failing","version":"11.4.0"}`)); !h.OK || !h.Metrics[0].Warn {
		t.Errorf("failing: Probe = %+v", h)
	}
	h = grafanaProbe(t, func(url.Values) (int, string) { return 503, `{"message":"db down glsa_x"}` })
	if h.OK || !strings.Contains(h.Err, "grafana 503") || strings.Contains(h.Err, "glsa_x") {
		t.Errorf("503: Probe = %+v", h)
	}
}

// ─── 통합(로컬 컨테이너) ───

func TestIntegration_Loki_Prometheus_를_확인한다(t *testing.T) {
	for _, c := range []struct {
		env  string
		make func(db.Conn) (source.Backend, error)
		pre  string
	}{
		{"DBTUI_TEST_LOKI_URL", NewLoki, "Loki "},
		{"DBTUI_TEST_PROM_URL", NewPrometheus, "Prometheus "},
	} {
		t.Run(c.env, func(t *testing.T) {
			base := os.Getenv(c.env)
			if base == "" {
				t.Skip(c.env + " not set")
			}
			b, err := c.make(db.Conn{URL: base})
			if err != nil {
				t.Fatal(err)
			}
			defer b.Close()
			ctx, cancel := context.WithTimeout(context.Background(), db.ProbeTimeout)
			defer cancel()
			h := b.(source.Prober).Probe(ctx)
			if !h.OK || !strings.HasPrefix(h.Version, c.pre) || len(h.Metrics) == 0 {
				t.Fatalf("Probe = %+v", h)
			}
		})
	}
}
