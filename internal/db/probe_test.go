package db

import (
	"errors"
	"strings"
	"testing"
	"time"
)

func TestPgMetrics_세션이_max_connections의_80퍼센트를_넘을_때만_경고한다(t *testing.T) {
	cases := []struct {
		sessions, max int64
		warn          bool
	}{
		{160, 200, false}, // 딱 80% 는 넘지 않음
		{161, 200, true},
		{12, 200, false},
		{5, 0, false}, // 한도를 모르면 경고하지 않음
	}
	for _, c := range cases {
		m := pgMetrics(c.sessions, 3, c.max, 4_509_715_660)
		if m[0].Warn != c.warn {
			t.Errorf("세션 %d/%d Warn = %v, want %v", c.sessions, c.max, m[0].Warn, c.warn)
		}
	}
	got := Health{Metrics: pgMetrics(12, 3, 200, 4_509_715_660)}.Summary()
	if got != "세션 12/200 · 활성 3 · DB 4.2GB" {
		t.Errorf("Summary = %q", got)
	}
}

func TestPgVersion_server_version_첫_낱말에_이름을_붙인다(t *testing.T) {
	for in, want := range map[string]string{
		"16.4 (Debian 16.4-1.pgdg120+2)": "PostgreSQL 16.4",
		"15.8":                           "PostgreSQL 15.8",
		"":                               "",
	} {
		if got := pgVersion(in); got != want {
			t.Errorf("pgVersion(%q) = %q, want %q", in, got, want)
		}
	}
}

const redisInfoSample = "# Server\r\nredis_version:7.2.4\r\nvalkey_version:8.0.1\r\n# Clients\r\nconnected_clients:35\r\n" +
	"# Memory\r\nused_memory:1181116006\r\nmaxmemory:4294967296\r\n# Keyspace\r\ndb0:keys=10000,expires=3,avg_ttl=0\r\ndb2:keys=2000,expires=0,avg_ttl=0\r\n"

func TestRedisMetrics_키를_합치고_메모리는_maxmemory_80퍼센트_초과면_경고한다(t *testing.T) {
	info, dbs := parseInfo(redisInfoSample)
	h := Health{Version: infoVersion(info), Metrics: redisMetrics(info, dbs)}
	if got := h.Summary(); got != "클라이언트 35 · 메모리 1.1/4.0GB · 키 1.2만" {
		t.Errorf("Summary = %q", got)
	}
	if h.Version != "Valkey 8.0.1" {
		t.Errorf("Version = %q", h.Version)
	}
	if h.Metrics[1].Warn {
		t.Error("27% 사용인데 경고함")
	}

	info["used_memory"] = "3500000000" // 81%
	if m := redisMetrics(info, dbs); !m[1].Warn || m[1].Value != "3.3/4.0GB" {
		t.Errorf("81%% 메모리 = %+v, want 경고 3.3/4.0GB", m[1])
	}
	info["used_memory"] = "536870912"
	if m := redisMetrics(info, dbs); m[1].Value != "512.0MB/4.0GB" {
		t.Errorf("단위가 다르면 둘 다 붙임: %q", m[1].Value)
	}
}

func TestRedisMetrics_maxmemory_0이면_제한_없음이고_숨긴_필드는_대시다(t *testing.T) {
	info, dbs := parseInfo("used_memory:1181116006\r\nmaxmemory:0\r\n")
	m := redisMetrics(info, dbs)
	if m[1].Value != "1.1GB/제한 없음" || m[1].Warn {
		t.Errorf("메모리 = %+v", m[1])
	}
	if m[0].Value != "-" || m[2].Value != "0" {
		t.Errorf("클라이언트·키 = %q %q", m[0].Value, m[2].Value)
	}
	info, dbs = parseInfo("connected_clients:1\r\n")
	if m := redisMetrics(info, dbs); m[1].Value != "-" {
		t.Errorf("used_memory 없음 = %q", m[1].Value)
	}
}

func TestFailed_오류의_비밀번호를_지우고_한_줄로_만든다(t *testing.T) {
	h := Failed(3*time.Millisecond, errors.New("dial postgres://u:s3cr%2Ft@h\n  password s3cr/t rejected"), "s3cr/t")
	if h.OK || strings.Contains(h.Err, "s3cr") || strings.Contains(h.Err, "\n") {
		t.Errorf("Err = %q", h.Err)
	}
	if h.Err != "dial postgres://u:***@h password *** rejected" || h.Latency != 3*time.Millisecond {
		t.Errorf("Failed = %+v", h)
	}
}

// ─── 통합(로컬 컨테이너) ───

func TestProbePG_세션_활성_크기_버전을_읽는다(t *testing.T) {
	c := testPGConn(t)
	cs := NewClients()
	defer cs.Close()
	h := cs.ProbePG(ctx(t), c)
	if !h.OK || h.Err != "" {
		t.Fatalf("ProbePG = %+v", h)
	}
	if !strings.HasPrefix(h.Version, "PostgreSQL ") || len(h.Metrics) != 3 || h.Latency <= 0 {
		t.Fatalf("ProbePG = %+v", h)
	}
	// 확인하는 연결 자신이 세션이자 활성이다
	if n, _, ok := strings.Cut(h.Metrics[0].Value, "/"); !ok || n == "0" || h.Metrics[1].Value == "0" || !strings.HasSuffix(h.Metrics[2].Value, "B") {
		t.Errorf("Metrics = %+v", h.Metrics)
	}
	// 풀에 돌려준 연결이 READ ONLY 로 남지 않는다
	p, err := cs.PG(ctx(t), c)
	must(t, err)
	res, err := p.Run(ctx(t), "show transaction_read_only")
	must(t, err)
	if res.Rows[0][0] != "off" {
		t.Errorf("transaction_read_only = %q", res.Rows[0][0])
	}
}

func TestProbePG_비밀번호가_틀리면_실패이고_오류에_비밀번호가_없다(t *testing.T) {
	c := testPGConn(t)
	c.UUID, c.Password = "it-pg-badpw", "wrong-pw-xyz"
	cs := NewClients()
	defer cs.Close()
	h := cs.ProbePG(ctx(t), c)
	if h.OK || h.Err == "" || strings.Contains(h.Err, c.Password) {
		t.Fatalf("ProbePG = %+v", h)
	}
}

func TestProbeRedis_클라이언트_메모리_키_버전을_읽는다(t *testing.T) {
	c := testRedisConn(t)
	cs := NewClients()
	defer cs.Close()
	h := cs.ProbeRedis(ctx(t), c)
	if !h.OK || h.Err != "" || len(h.Metrics) != 3 || h.Latency <= 0 {
		t.Fatalf("ProbeRedis = %+v", h)
	}
	if !strings.HasPrefix(h.Version, "Redis ") && !strings.HasPrefix(h.Version, "Valkey ") {
		t.Errorf("Version = %q", h.Version)
	}
	if h.Metrics[0].Value == "-" || h.Metrics[0].Value == "0" || h.Metrics[1].Value == "-" {
		t.Errorf("Metrics = %+v", h.Metrics)
	}
}

func TestProbeRedis_연결이_안_되면_실패다(t *testing.T) {
	c := testRedisConn(t)
	c.UUID, c.Port, c.Password = "it-redis-down", 1, "pw-should-hide"
	cs := NewClients()
	defer cs.Close()
	h := cs.ProbeRedis(ctx(t), c)
	if h.OK || h.Err == "" || strings.Contains(h.Err, c.Password) {
		t.Fatalf("ProbeRedis = %+v", h)
	}
}
