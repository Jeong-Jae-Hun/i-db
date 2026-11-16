package db

import (
	"cmp"
	"context"
	"errors"
	"fmt"
	"strconv"
	"strings"
	"time"

	"github.com/jackc/pgx/v5"
	"github.com/jackc/pgx/v5/pgxpool"
	"github.com/redis/go-redis/v9"

	"github.com/Jeong-Jae-Hun/i-db/internal/textutil"
)

// 대시보드 확인(R64.2)은 읽기 전용 메타데이터만 묻는다. 아직 열지 않은 접속은 여기서 연다(풀 연결 하나).
// 지연은 연결을 연 뒤 질의 한 번의 왕복이다. 처음 여는 비용은 한 번뿐이라 30초마다 보는 값에 섞지 않는다.
// 여는 데는 ProbeOpenTimeout, 질의에는 ProbeTimeout 을 건다. 호출부 ctx 는 취소만 넘긴다.

// warnRatio 를 넘으면 경고다(세션/max_connections, 메모리/maxmemory, R64.1)
const warnRatio = 0.8

// 대시보드 요약 카드(R64.1)가 더하는 수치 이름이다. 값은 앞자리가 정수다
const (
	MetricPGActive     = "활성"
	MetricRedisClients = "클라이언트"
)

// pgProbeSQL 은 클라이언트 세션 수·활성 수·max_connections·DB 크기·버전을 한 번에 묻는다.
// 슈퍼유저·pg_read_all_stats 가 아니면 남의 세션 state 가 NULL 이라 활성 수가 적게 나온다(행은 보여 세션 수는 맞다).
// 확인하는 연결 자신은 늘 active 라 활성 수에서 뺀다(세션 수에는 남는다).
const pgProbeSQL = `select count(*), count(*) filter (where state = 'active' and pid <> pg_backend_pid()),
	current_setting('max_connections')::int, pg_database_size(current_database()), current_setting('server_version')
	from pg_stat_activity where backend_type = 'client backend'`

// pgVersionSQL 은 위 질의가 막힐 때(권한·옛 서버) 버전만이라도 보이려는 것이다.
const pgVersionSQL = `select current_setting('server_version')`

// ProbePG 는 PostgreSQL 접속을 READ ONLY 트랜잭션 한 번으로 살펴본다.
// 수치: `세션 N/M`(M 의 80% 초과면 경고)·`활성 K`·`DB 4.2GB`. 집계가 막히면 버전만 보이고 OK 다.
func (cs *Clients) ProbePG(ctx context.Context, c Conn) Health {
	start := time.Now()
	octx, cancel := context.WithTimeout(ctx, ProbeOpenTimeout)
	p, err := cs.PG(octx, c)
	cancel()
	if err != nil {
		return Failed(time.Since(start), err, c.Password)
	}
	ctx, cancel = context.WithTimeout(ctx, ProbeTimeout)
	defer cancel()
	start = time.Now()
	var (
		sessions, active, maxConns, size int64
		version                          string
	)
	err = pgReadOnlyRow(ctx, p.pool, pgProbeSQL, &sessions, &active, &maxConns, &size, &version)
	if err == nil {
		return Health{OK: true, Latency: time.Since(start), Version: pgVersion(version), Metrics: pgMetrics(sessions, active, maxConns, size)}
	}
	if ctx.Err() == nil && pgReadOnlyRow(ctx, p.pool, pgVersionSQL, &version) == nil {
		return Health{OK: true, Latency: time.Since(start), Version: pgVersion(version)}
	}
	return Failed(time.Since(start), err, c.Password)
}

// pgReadOnlyRow 는 풀 연결에서 고정 문장 하나를 READ ONLY 트랜잭션으로 읽고 되돌린다.
// 사용자 문장이 아니라 세션 상태를 남기지 않으므로 RunReadOnly 처럼 연결을 버리지 않고 풀에 돌려준다.
func pgReadOnlyRow(ctx context.Context, pool *pgxpool.Pool, sql string, dest ...any) error {
	tx, err := pool.BeginTx(ctx, pgx.TxOptions{AccessMode: pgx.ReadOnly})
	if err != nil {
		return err
	}
	defer func() { _ = tx.Rollback(context.WithoutCancel(ctx)) }() // 읽기만 했으니 되돌림 오류로 할 일이 없다
	return tx.QueryRow(ctx, sql).Scan(dest...)
}

func pgMetrics(sessions, active, maxConns, size int64) []Metric {
	return []Metric{
		{Name: "세션", Value: fmt.Sprintf("%d/%d", sessions, maxConns), Warn: over(sessions, maxConns)},
		{Name: MetricPGActive, Value: strconv.FormatInt(active, 10)},
		{Name: "DB", Value: HumanBytes(size)},
	}
}

// pgVersion 은 server_version(`16.4 (Debian 16.4-1.pgdg120+2)`)의 첫 낱말에 이름을 붙인다.
func pgVersion(v string) string {
	if f := strings.Fields(v); len(f) > 0 {
		return "PostgreSQL " + f[0]
	}
	return ""
}

// ProbeRedis 는 Redis 접속에 쓰는 네 구역만 INFO 로 묻는다(편집기 세션이 아니라 풀 클라이언트).
// 여러 구역 INFO 는 Redis 7 부터라, 옛 서버가 오류로 답하면 인자 없는 INFO 로 다시 묻는다.
// 수치: `클라이언트 N`·`메모리 1.1/4.0GB`(maxmemory 의 80% 초과면 경고, 0 이면 `제한 없음`)·`키 1.2만`(키 공간 합).
func (cs *Clients) ProbeRedis(ctx context.Context, c Conn) Health {
	start := time.Now()
	octx, cancel := context.WithTimeout(ctx, ProbeOpenTimeout)
	r, err := cs.Redis(octx, c)
	cancel()
	if err != nil {
		return Failed(time.Since(start), err, c.Password)
	}
	ctx, cancel = context.WithTimeout(ctx, ProbeTimeout)
	defer cancel()
	start = time.Now()
	text, err := r.cl.Info(ctx, "server", "clients", "memory", "keyspace").Result()
	var replyErr redis.Error
	if errors.As(err, &replyErr) {
		text, err = r.cl.Info(ctx).Result()
	}
	if err != nil {
		return Failed(time.Since(start), err, c.Password)
	}
	info, dbs := parseInfo(text)
	return Health{OK: true, Latency: time.Since(start), Version: infoVersion(info), Metrics: redisMetrics(info, dbs)}
}

// redisMetrics 는 INFO 필드에서 수치를 만든다. ElastiCache 처럼 필드를 숨긴 서버는 그 칸을 `-` 로 둔다.
// maxmemory 만 숨겼으면 사용량만 보인다(`제한 없음`은 maxmemory:0 일 때뿐이다).
func redisMetrics(info map[string]string, dbs []string) []Metric {
	var keys int64
	for _, name := range dbs {
		n, _ := strconv.ParseInt(keyspaceFields(info[name])["keys"], 10, 64)
		keys += n
	}
	mem := Metric{Name: "메모리", Value: "-"}
	if used, err := strconv.ParseInt(info["used_memory"], 10, 64); err == nil {
		maxMem, err := strconv.ParseInt(info["maxmemory"], 10, 64)
		switch {
		case err != nil:
			mem.Value = HumanBytes(used)
		case maxMem > 0:
			mem.Value, mem.Warn = bytesPair(used, maxMem), over(used, maxMem)
		default:
			mem.Value = HumanBytes(used) + "/제한 없음"
		}
	}
	return []Metric{
		{Name: MetricRedisClients, Value: cmp.Or(info["connected_clients"], "-")},
		mem,
		{Name: "키", Value: textutil.KoreanCount(keys)},
	}
}

// bytesPair 는 `1.1/4.0GB` 다. 단위가 다르면 둘 다 붙인다(`512.0MB/4.0GB`).
func bytesPair(used, total int64) string {
	u, t := HumanBytes(used), HumanBytes(total)
	if unit := strings.TrimLeft(t, "0123456789."); strings.TrimLeft(u, "0123456789.") == unit {
		u = strings.TrimSuffix(u, unit)
	}
	return u + "/" + t
}

// over 는 n 이 limit 의 80% 를 넘는지다. limit 이 0 이하면 한도가 없어 경고하지 않는다.
func over(n, limit int64) bool { return limit > 0 && float64(n) > float64(limit)*warnRatio }
