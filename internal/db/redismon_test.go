package db

import (
	"context"
	"errors"
	"fmt"
	"slices"
	"strconv"
	"strings"
	"testing"
	"time"
	"unicode/utf8"

	"github.com/redis/go-redis/v9"
)

// ─── 단위 ───

func infoValue(t *testing.T, res *Result, name string) string {
	t.Helper()
	for _, row := range res.Rows {
		if row[0] == name {
			return row[1]
		}
	}
	t.Fatalf("%q 행이 없다: %v", name, res.Rows)
	return ""
}

func TestSummarizeInfo_Redis7_INFO_를_항목별로_줄이고_DB는_번호순이다(t *testing.T) {
	text := strings.Join([]string{
		"# Server", "redis_version:7.2.4", "uptime_in_seconds:273600", "",
		"# Clients", "connected_clients:12", "",
		"# Memory", "used_memory:1572864", "used_memory_human:1.50M", "maxmemory:0", "",
		"# Stats", "instantaneous_ops_per_sec:345", "keyspace_hits:932", "keyspace_misses:68", "",
		"# Keyspace", "db0:keys=1234,expires=12,avg_ttl=0", "db10:keys=1,expires=0,avg_ttl=0", "db2:keys=5,expires=5,avg_ttl=10,subexpiry=0", "",
	}, "\r\n")
	res := summarizeInfo(text)
	want := map[string]string{
		"버전": "Redis 7.2.4", "가동 시간": "3일 4시간", "메모리 사용/최대": "1.5MB / 제한 없음",
		"클라이언트 수": "12", "초당 명령": "345", "적중률": "93.2%", "db0": "키 1,234 · 만료 있음 12",
	}
	for k, v := range want {
		if got := infoValue(t, res, k); got != v {
			t.Errorf("%s: got %q, want %q", k, got, v)
		}
	}
	var order []string
	for _, row := range res.Rows[6:] {
		order = append(order, row[0])
	}
	if strings.Join(order, ",") != "db0,db2,db10" {
		t.Fatalf("DB 순서: %v", order)
	}
}

func TestSummarizeInfo_관리형_서버가_숨긴_필드는_대시로_둔다(t *testing.T) {
	// ElastiCache·Valkey 처럼 uptime·적중 통계·maxmemory 가 빠지고 valkey_version 이 더해진 INFO
	text := "# Server\r\nredis_version:7.2.4\r\nvalkey_version:8.0.1\r\n# Memory\r\nused_memory:2048\r\n# Keyspace\r\n"
	res := summarizeInfo(text)
	want := map[string]string{
		"버전": "Valkey 8.0.1", "가동 시간": "-", "메모리 사용/최대": "2.0KB / -",
		"클라이언트 수": "-", "초당 명령": "-", "적중률": "-",
	}
	for k, v := range want {
		if got := infoValue(t, res, k); got != v {
			t.Errorf("%s: got %q, want %q", k, got, v)
		}
	}
	if len(res.Rows) != 6 {
		t.Fatalf("키가 없으면 DB 행도 없다: %v", res.Rows)
	}
	if got := infoValue(t, summarizeInfo("used_memory:1024\nmaxmemory:1073741824\nkeyspace_hits:0\nkeyspace_misses:0"), "메모리 사용/최대"); got != "1.0KB / 1.0GB" {
		t.Fatalf("최대 메모리: %q", got)
	}
	if got := infoValue(t, summarizeInfo(""), "버전"); got != "-" {
		t.Fatalf("빈 INFO: %q", got)
	}
}

func TestFormatUptime_과_humanBytes(t *testing.T) {
	for secs, want := range map[int64]string{0: "0초", 59: "59초", 61: "1분 1초", 3600: "1시간", 90061: "1일 1시간", 86400 * 400: "400일"} {
		if got := formatUptime(secs); got != want {
			t.Errorf("formatUptime(%d) = %q, want %q", secs, got, want)
		}
	}
	for n, want := range map[int64]string{0: "0B", 1023: "1023B", 1536: "1.5KB", 5 << 20: "5.0MB", 3 << 40: "3.0TB", 5000 << 40: "5000.0TB"} {
		if got := HumanBytes(n); got != want {
			t.Errorf("humanBytes(%d) = %q, want %q", n, got, want)
		}
	}
}

func TestSlowLogResult_명령은_제어문자를_보이게_바꾸고_80자에서_자른다(t *testing.T) {
	at := time.Date(2026, 11, 14, 12, 0, 0, 0, time.UTC)
	logs := []redis.SlowLog{
		{Time: at, Duration: 12345 * time.Microsecond, Args: []string{"SET", "세션:" + strings.Repeat("가", 100), "\x1b]52;c;x\a"}, ClientAddr: "10.0.0.1:5000", ClientName: "api\x1b"},
		{Time: at, Duration: 850 * time.Microsecond, Args: []string{"GET", "a\x1b[31m"}, ClientAddr: "10.0.0.2:5000"},
	}
	res := slowLogResult(logs)
	if len(res.Rows) != 2 || strings.Join(res.Cols, ",") != "시각,걸린 시간,명령,클라이언트" {
		t.Fatalf("표: %+v", res)
	}
	long, short := res.Rows[0], res.Rows[1]
	if long[0] != at.Local().Format("2006-01-02 15:04:05") {
		t.Errorf("시각은 지역 시간: %q", long[0])
	}
	if long[1] != "12.3ms" || short[1] != "850µs" {
		t.Errorf("걸린 시간: %q %q", long[1], short[1])
	}
	if utf8.RuneCountInString(long[2]) != 81 || !strings.HasSuffix(long[2], "…") || !strings.HasPrefix(long[2], "SET 세션:가") {
		t.Errorf("긴 명령은 80자 + …: %q", long[2])
	}
	if short[2] != `GET a\x1b[31m` {
		t.Errorf("짧은 명령은 자르지 않고 ESC 를 보이게 바꾼다: %q", short[2])
	}
	if long[3] != `10.0.0.1:5000 (api\x1b)` || short[3] != "10.0.0.2:5000" {
		t.Errorf("클라이언트: %q %q", long[3], short[3])
	}
	for _, row := range res.Rows {
		for _, cell := range row {
			if strings.ContainsAny(cell, "\x1b\a") {
				t.Errorf("제어 문자가 남았다: %q", cell)
			}
		}
	}
	if empty := slowLogResult(nil); empty.Rows == nil || len(empty.Cols) != 4 {
		t.Errorf("기록이 없어도 빈 표: %+v", empty)
	}
}

// ─── 통합 ───

// 다른 테스트가 DB 0 을 FLUSHDB 하므로 모니터 테스트는 DB 7 에서 v4redis: 접두 키만 쓰고 지운다.
func testMonitorRedis(t *testing.T) *RedisConn {
	t.Helper()
	c := testRedisConn(t)
	c.UUID, c.DB = "it-redis-monitor", "7"
	cs := NewClients()
	t.Cleanup(cs.Close)
	rc, err := cs.Redis(ctx(t), c)
	must(t, err)
	return rc
}

func cleanupKeys(t *testing.T, rc *RedisConn, keys ...string) {
	t.Helper()
	must(t, rc.cl.Del(ctx(t), keys...).Err())
	t.Cleanup(func() {
		if err := rc.cl.Del(context.Background(), keys...).Err(); err != nil {
			t.Errorf("키 정리: %v", err)
		}
	})
}

func TestRedis_INFO_요약은_버전과_현재_DB_키_수를_보인다(t *testing.T) {
	rc := testMonitorRedis(t)
	cleanupKeys(t, rc, "v4redis:info")
	must(t, rc.cl.Set(ctx(t), "v4redis:info", "1", time.Hour).Err())
	res, err := rc.InfoSummary(ctx(t))
	must(t, err)
	if v := infoValue(t, res, "버전"); !strings.HasPrefix(v, "Redis ") && !strings.HasPrefix(v, "Valkey ") {
		t.Fatalf("버전: %q", v)
	}
	if v := infoValue(t, res, "db7"); !strings.HasPrefix(v, "키 ") {
		t.Fatalf("db7: %q", v)
	}
}

func TestRedis_SLOWLOG_은_느린_명령을_80자에서_잘라_보인다(t *testing.T) {
	rc := testMonitorRedis(t)
	limit, err := rc.cl.ConfigGet(ctx(t), "slowlog-log-slower-than").Result()
	must(t, err)
	us, err := strconv.Atoi(limit["slowlog-log-slower-than"])
	if err != nil || us < 0 || us > 20000 {
		t.Skipf("slowlog-log-slower-than=%q 이라 느린 명령을 만들 수 없다", limit["slowlog-log-slower-than"])
	}
	marker := "v4redis:slow:" + strconv.FormatInt(time.Now().UnixNano(), 10)
	// Lua 바쁜 루프로 기준(기본 10ms)을 넘긴다. 키를 만들지 않는다
	script := "local i = 0 while i < 20000000 do i = i + 1 end return i -- " + strings.Repeat("x", 100)
	must(t, rc.cl.Eval(ctx(t), script, nil, marker).Err())
	res, err := rc.SlowLog(ctx(t), 50)
	must(t, err)
	for _, row := range res.Rows {
		if !strings.HasPrefix(strings.ToLower(row[2]), "eval local i = 0") {
			continue
		}
		if utf8.RuneCountInString(row[2]) != 81 || !strings.HasSuffix(row[2], "…") || row[3] == "" {
			t.Fatalf("잘린 명령·클라이언트: %q", row)
		}
		return
	}
	t.Fatalf("느린 EVAL 이 SLOWLOG 에 없다: %v", res.Rows)
}

func TestRedis_큰_키는_표본에서_바이트순_상위만_타입_TTL과_보인다(t *testing.T) {
	rc := testMonitorRedis(t)
	keys := []string{"v4redis:big", "v4redis:mid", "v4redis:small"}
	cleanupKeys(t, rc, keys...)
	must(t, rc.cl.Set(ctx(t), "v4redis:big", strings.Repeat("b", 50000), 0).Err())
	must(t, rc.cl.HSet(ctx(t), "v4redis:mid", "f", strings.Repeat("m", 5000)).Err())
	must(t, rc.cl.Expire(ctx(t), "v4redis:mid", time.Hour).Err())
	must(t, rc.cl.Set(ctx(t), "v4redis:small", "s", 0).Err())

	res, err := rc.BigKeys(ctx(t), 1000, 2)
	must(t, err)
	if len(res.Rows) != 2 || res.Rows[0][0] != "v4redis:big" || res.Rows[1][0] != "v4redis:mid" {
		t.Fatalf("상위 2개: %v", res.Rows)
	}
	big, mid := res.Rows[0], res.Rows[1]
	if big[1] != "string" || big[4] != "∞" || mid[1] != "hash" || mid[4] == "∞" {
		t.Fatalf("타입·TTL: %v", res.Rows)
	}
	if n, err := strconv.Atoi(big[3]); err != nil || n < 50000 || !strings.HasSuffix(big[2], "KB") {
		t.Fatalf("크기: %v", big)
	}
	if !strings.HasPrefix(res.Text, "표본 ") || !strings.HasSuffix(res.Text, "개") {
		t.Fatalf("표본 수: %q", res.Text)
	}
	res, err = rc.BigKeys(ctx(t), 1, 50)
	must(t, err)
	if res.Text != "표본 1개" || len(res.Rows) > 1 {
		t.Fatalf("표본 상한: %q %v", res.Text, res.Rows)
	}
	if _, err := rc.BigKeys(ctx(t), 0, 50); err == nil {
		t.Fatal("표본 0 은 오류")
	}
}

func TestRedis_큰_키_표본은_SCAN_순서의_앞_N개다(t *testing.T) {
	c := testRedisConn(t)
	c.UUID, c.DB = "it-redis-v4fix", "9"
	cs := NewClients()
	t.Cleanup(cs.Close)
	rc, err := cs.Redis(ctx(t), c)
	must(t, err)
	keys := make([]string, 60)
	for i := range keys {
		keys[i] = fmt.Sprintf("v4fix:k%02d", i)
	}
	cleanupKeys(t, rc, keys...)
	for _, k := range keys {
		must(t, rc.cl.Set(ctx(t), k, k, 0).Err())
	}
	const sample = 7
	// BigKeys 와 같은 COUNT 로 SCAN 해 처음 나온 순서대로 sample 개를 고른다
	var want []string
	var cursor uint64
	for len(want) < sample {
		batch, next, err := rc.cl.Scan(ctx(t), cursor, "*", sample).Result()
		must(t, err)
		for _, k := range batch {
			if len(want) < sample && !slices.Contains(want, k) {
				want = append(want, k)
			}
		}
		if cursor = next; cursor == 0 {
			break
		}
	}
	res, err := rc.BigKeys(ctx(t), sample, sample)
	must(t, err)
	var got []string
	for _, row := range res.Rows {
		got = append(got, row[0])
	}
	slices.Sort(got)
	slices.Sort(want)
	if !slices.Equal(got, want) || res.Text != "표본 7개" {
		t.Fatalf("표본 %q, want %q (%s)", got, want, res.Text)
	}
}

func TestRedis_MEMORY_USAGE_를_못_쓰면_false_로_빼고_값_보기에는_바이트를_싣는다(t *testing.T) {
	rc := testMonitorRedis(t)
	cleanupKeys(t, rc, "v4redis:mem")
	must(t, rc.cl.Set(ctx(t), "v4redis:mem", strings.Repeat("v", 2000), 0).Err())

	n, ok, err := rc.memoryUsage(ctx(t), "v4redis:mem")
	if err != nil || !ok || n < 2000 {
		t.Fatalf("있는 키: %d %v %v", n, ok, err)
	}
	if _, ok, err := rc.memoryUsage(ctx(t), "v4redis:none"); err != nil || ok {
		t.Fatalf("없는 키: %v %v", ok, err)
	}
	v, err := rc.GetValue(ctx(t), "v4redis:mem")
	must(t, err)
	if first, _, _ := strings.Cut(v.Text, "\n"); !strings.Contains(first, " · 메모리 ") || !strings.HasSuffix(first, "KB") {
		t.Fatalf("정보 줄: %q", first)
	}

	// MEMORY 를 막은 ACL 사용자는 관리형 서버에서 명령을 못 쓰는 경우와 같다
	user := "v4redis-nomem"
	must(t, rc.cl.Do(ctx(t), "ACL", "SETUSER", user, "reset", "on", ">v4pass", "~v4redis:*", "+@all", "-memory").Err())
	t.Cleanup(func() { rc.cl.Do(context.Background(), "ACL", "DELUSER", user) })
	denied := &RedisConn{cl: redis.NewClient(&redis.Options{Addr: rc.opts.Addr, Username: user, Password: "v4pass", DB: 7})}
	t.Cleanup(func() { _ = denied.cl.Close() })
	if _, ok, err := denied.memoryUsage(ctx(t), "v4redis:mem"); err != nil || ok {
		t.Fatalf("ACL 로 막힌 MEMORY USAGE: %v %v", ok, err)
	}
	if _, err := denied.BigKeys(ctx(t), 1000, 50); !errors.Is(err, ErrNoMemoryUsage) {
		t.Fatalf("큰 키는 ErrNoMemoryUsage 여야 한다: %v", err)
	}
	v, err = denied.GetValue(ctx(t), "v4redis:mem")
	must(t, err)
	if strings.Contains(v.Text, "메모리") {
		t.Fatalf("못 쓰면 뺀다: %q", v.Text)
	}
}
