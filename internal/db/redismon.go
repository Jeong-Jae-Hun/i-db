package db

import (
	"cmp"
	"context"
	"errors"
	"fmt"
	"slices"
	"sort"
	"strconv"
	"strings"
	"time"

	"github.com/Jeong-Jae-Hun/i-db/internal/textutil"
	"github.com/redis/go-redis/v9"
)

// Redis 모니터(INFO 요약·SLOWLOG·큰 키, 명세 R34)는 모두 읽기 명령이다. 트리 조회처럼 풀 클라이언트로 보내
// 편집기 세션의 SELECT 와 섞이지 않는다.

// SLOWLOG 인자에는 값이 그대로 들어 있을 수 있어 앞 80자만 싣는다(명세 R34.1)
const slowLogCmdRunes = 80

type bigKey struct {
	key, typ string
	bytes    int64
	ttl      time.Duration
}

// ─── INFO ───

// InfoSummary 는 INFO 를 항목·값 두 열 표로 줄인다.
func (r *RedisConn) InfoSummary(ctx context.Context) (*Result, error) {
	start := time.Now()
	text, err := r.cl.Info(ctx).Result()
	if err != nil {
		return nil, err
	}
	res := summarizeInfo(text)
	res.Elapsed = time.Since(start)
	return res, nil
}

// summarizeInfo 는 INFO 글을 표로 만든다. ElastiCache·Valkey 는 일부 필드를 숨기거나 이름을 더하므로
// 없는 값은 "-" 로 둔다.
func summarizeInfo(text string) *Result {
	info, dbs := parseInfo(text)
	version := cmp.Or(infoVersion(info), "-")
	uptime := "-"
	if s, err := strconv.ParseInt(info["uptime_in_seconds"], 10, 64); err == nil {
		uptime = formatUptime(s)
	}
	memory := "-"
	if used, err := strconv.ParseInt(info["used_memory"], 10, 64); err == nil {
		memory = humanBytes(used) + " / "
		if maxMem, err := strconv.ParseInt(info["maxmemory"], 10, 64); err != nil {
			memory += "-"
		} else if maxMem == 0 {
			memory += "제한 없음"
		} else {
			memory += humanBytes(maxMem)
		}
	}
	hitRate := "-"
	hits, herr := strconv.ParseInt(info["keyspace_hits"], 10, 64)
	misses, merr := strconv.ParseInt(info["keyspace_misses"], 10, 64)
	if herr == nil && merr == nil && hits+misses > 0 {
		hitRate = fmt.Sprintf("%.1f%%", float64(hits)*100/float64(hits+misses))
	}
	res := &Result{Cols: []string{"항목", "값"}, Rows: [][]string{
		{"버전", version},
		{"가동 시간", uptime},
		{"메모리 사용/최대", memory},
		{"클라이언트 수", cmp.Or(info["connected_clients"], "-")},
		{"초당 명령", cmp.Or(info["instantaneous_ops_per_sec"], "-")},
		{"적중률", hitRate},
	}}
	// db10 이 db2 뒤에 오도록 번호로 정렬한다
	sort.Slice(dbs, func(i, j int) bool {
		a, _ := strconv.Atoi(dbs[i][2:])
		b, _ := strconv.Atoi(dbs[j][2:])
		return a < b
	})
	for _, name := range dbs {
		fields := keyspaceFields(info[name])
		res.Rows = append(res.Rows, []string{name, fmt.Sprintf("키 %s · 만료 있음 %s", digits(fields["keys"]), digits(fields["expires"]))})
	}
	return res
}

// parseInfo 는 INFO 글을 필드 맵과 키 공간 이름(db0·db1…, 나온 순서)으로 가른다.
func parseInfo(text string) (info map[string]string, dbs []string) {
	info = map[string]string{}
	for _, line := range strings.Split(text, "\n") {
		k, v, ok := strings.Cut(strings.TrimSpace(line), ":")
		if !ok || strings.HasPrefix(k, "#") {
			continue
		}
		info[k] = v
		if num, isDB := strings.CutPrefix(k, "db"); isDB && digits(num) != "-" {
			dbs = append(dbs, k)
		}
	}
	return info, dbs
}

// infoVersion 은 서버 이름과 버전(`Valkey 8.0.1`·`Redis 7.2.4`)이다. Valkey 는 호환용 redis_version 도 내므로
// valkey_version 을 먼저 본다. 모르면 빈 글이다.
func infoVersion(info map[string]string) string {
	if v := info["valkey_version"]; v != "" {
		return "Valkey " + v
	}
	if v := info["redis_version"]; v != "" {
		return "Redis " + v
	}
	return ""
}

// keyspaceFields 는 키 공간 한 줄(keys=1,expires=0,avg_ttl=0[,subexpiry=0])을 맵으로 편다.
func keyspaceFields(line string) map[string]string {
	fields := map[string]string{}
	for _, kv := range strings.Split(line, ",") {
		k, v, _ := strings.Cut(kv, "=")
		fields[k] = v
	}
	return fields
}

// digits 는 숫자 글에 쉼표를 넣고, 숫자가 아니면 "-" 를 돌려준다.
func digits(s string) string {
	n, err := strconv.ParseInt(s, 10, 64)
	if err != nil || n < 0 {
		return "-"
	}
	return textutil.GroupDigits(n)
}

// formatUptime 은 큰 단위 둘만 보인다(예: 3일 4시간).
func formatUptime(secs int64) string {
	units := []struct {
		name string
		secs int64
	}{{"일", 86400}, {"시간", 3600}, {"분", 60}, {"초", 1}}
	for i, u := range units {
		if secs < u.secs && u.name != "초" {
			continue
		}
		out := fmt.Sprintf("%d%s", secs/u.secs, u.name)
		if i+1 < len(units) {
			if rest := secs % u.secs / units[i+1].secs; rest > 0 {
				out += fmt.Sprintf(" %d%s", rest, units[i+1].name)
			}
		}
		return out
	}
	return "0초"
}

// humanBytes 는 1024 단위로 줄인다(예: 1.5KB).
func humanBytes(n int64) string {
	if n < 1024 {
		return fmt.Sprintf("%dB", n)
	}
	f := float64(n)
	for _, u := range []string{"KB", "MB", "GB", "TB"} {
		f /= 1024
		if f < 1024 || u == "TB" {
			return fmt.Sprintf("%.1f%s", f, u)
		}
	}
	return ""
}

// ─── SLOWLOG ───

// SlowLog 는 SLOWLOG GET n 이다. 최근 것이 먼저 온다.
func (r *RedisConn) SlowLog(ctx context.Context, n int) (*Result, error) {
	start := time.Now()
	logs, err := r.cl.SlowLogGet(ctx, int64(n)).Result()
	if err != nil {
		return nil, err
	}
	res := slowLogResult(logs)
	res.Elapsed = time.Since(start)
	return res, nil
}

func slowLogResult(logs []redis.SlowLog) *Result {
	res := &Result{Cols: []string{"시각", "걸린 시간", "명령", "클라이언트"}, Rows: [][]string{}}
	for _, l := range logs {
		took := fmt.Sprintf("%dµs", l.Duration.Microseconds())
		if l.Duration >= time.Millisecond {
			took = fmt.Sprintf("%.1fms", float64(l.Duration.Microseconds())/1000)
		}
		// 제어 문자를 먼저 보이는 글자로 바꾼 뒤 자른다. 화면에 실리는 글이 80자를 넘지 않게 하려는 것이다
		cmdText := []rune(textutil.Printable(strings.Join(l.Args, " "), false))
		if len(cmdText) > slowLogCmdRunes {
			cmdText = append(cmdText[:slowLogCmdRunes], '…')
		}
		client := l.ClientAddr
		if l.ClientName != "" {
			client += " (" + l.ClientName + ")"
		}
		res.Rows = append(res.Rows, []string{
			l.Time.Local().Format("2006-01-02 15:04:05"), took, string(cmdText), textutil.Printable(client, false),
		})
	}
	return res
}

// ─── 큰 키 ───

// ErrNoMemoryUsage 는 서버가 MEMORY USAGE 를 모르거나 ACL 이 막아 큰 키를 셀 수 없을 때다.
var ErrNoMemoryUsage = errors.New("MEMORY USAGE is not available on this server")

// BigKeys 는 SCAN 이 준 순서대로 앞 sample 개 키를 모아 MEMORY USAGE 가 큰 top 개를 보인다.
// 표본이라 전체 순위가 아니어서 Text 에 표본 수를 적는다(명세 R34.1). MEMORY USAGE 를 못 쓰면 ErrNoMemoryUsage 다.
func (r *RedisConn) BigKeys(ctx context.Context, sample, top int) (*Result, error) {
	if sample <= 0 || top <= 0 {
		return nil, errors.New("sample and top must be positive")
	}
	start := time.Now()
	var keys []string
	// SCAN 은 리해시 중 같은 키를 두 번 줄 수 있다
	seen := map[string]bool{}
	var cursor uint64
	for len(keys) < sample {
		batch, next, err := r.cl.Scan(ctx, cursor, "*", int64(min(sample, 500))).Result()
		if err != nil {
			return nil, err
		}
		for _, k := range batch {
			if len(keys) < sample && !seen[k] {
				seen[k] = true
				keys = append(keys, k)
			}
		}
		if cursor = next; cursor == 0 {
			break
		}
	}

	pipe := r.cl.Pipeline()
	mems := make([]*redis.IntCmd, len(keys))
	types := make([]*redis.StatusCmd, len(keys))
	ttls := make([]*redis.DurationCmd, len(keys))
	for i, k := range keys {
		mems[i] = pipe.MemoryUsage(ctx, k)
		types[i] = pipe.Type(ctx, k)
		ttls[i] = pipe.TTL(ctx, k)
	}
	// Exec 의 오류는 첫 실패 명령의 것이라 버리고 명령마다 본다. SCAN 뒤 지워진 키는 redis.Nil 이라 건너뛴다
	_, _ = pipe.Exec(ctx)
	found := make([]bigKey, 0, len(keys))
	for i, k := range keys {
		n, err := mems[i].Result()
		if errors.Is(err, redis.Nil) {
			continue
		}
		if commandUnavailable(err) {
			return nil, ErrNoMemoryUsage
		}
		if err != nil {
			return nil, err
		}
		found = append(found, bigKey{key: k, typ: types[i].Val(), bytes: n, ttl: ttls[i].Val()})
	}
	slices.SortFunc(found, func(a, b bigKey) int {
		return cmp.Or(cmp.Compare(b.bytes, a.bytes), strings.Compare(a.key, b.key))
	})
	found = found[:min(len(found), top)]

	res := &Result{
		Cols: []string{"키", "타입", "크기", "바이트", "TTL"},
		Rows: make([][]string, 0, len(found)),
		Text: fmt.Sprintf("표본 %s개", textutil.GroupDigits(int64(len(keys)))),
	}
	for _, b := range found {
		ttl := "∞"
		if b.ttl >= 0 {
			ttl = b.ttl.String()
		}
		res.Rows = append(res.Rows, []string{CapCell(b.key), b.typ, humanBytes(b.bytes), strconv.FormatInt(b.bytes, 10), ttl})
	}
	res.Elapsed = time.Since(start)
	return res, nil
}

// memoryUsage 는 키의 MEMORY USAGE 바이트다. 서버가 명령을 모르거나(관리형 서버가 이름을 바꾼 경우 등)
// ACL 이 막거나 키가 없으면 false 를 돌려줘, 화면은 그 값을 빼고 보인다(명세 R34.2).
func (r *RedisConn) memoryUsage(ctx context.Context, key string) (int64, bool, error) {
	n, err := r.cl.MemoryUsage(ctx, key).Result()
	if errors.Is(err, redis.Nil) {
		return 0, false, nil
	}
	if commandUnavailable(err) {
		return 0, false, nil
	}
	if err != nil {
		return 0, false, err
	}
	return n, true, nil
}

// commandUnavailable 은 서버가 명령을 모르거나(관리형 서버가 이름을 바꾼 경우 등) ACL 이 막은 오류인지다.
func commandUnavailable(err error) bool {
	var re redis.Error
	if !errors.As(err, &re) {
		return false
	}
	msg := re.Error()
	return strings.Contains(msg, "unknown command") || strings.Contains(msg, "unknown subcommand") || strings.HasPrefix(msg, "NOPERM")
}
