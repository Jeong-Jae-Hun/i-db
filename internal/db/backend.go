package db

import (
	"bytes"
	"context"
	"crypto/tls"
	"encoding/json"
	"errors"
	"fmt"
	"net"
	"net/url"
	"regexp"
	"slices"
	"sort"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/jackc/pgx/v5"
	"github.com/jackc/pgx/v5/pgconn"
	"github.com/jackc/pgx/v5/pgxpool"
	"github.com/redis/go-redis/v9"
)

const (
	MaxRows        = 1000
	maxStringBytes = 1 << 20  // Redis string 값은 앞 1MiB 만 보여 준다(E63)
	MaxCellBytes   = 64 << 10 // 표 칸 하나에 담는 최대 바이트(E63)
)

var (
	errSessionReset = errors.New("redis session connection was re-established")
	errClosed       = errors.New("connections are closed")
)

type Result struct {
	Cols      []string
	Types     []string // 열 타입 이름(PostgreSQL 만). 모르는 OID 는 숫자 그대로
	Rows      [][]string
	Text      string // 표가 아닌 결과(명령 태그, string 값 등)
	Truncated bool
	InTx      bool     // 편집기 세션이 트랜잭션 안에 있다
	Origin    *Origin  // 결과가 한 테이블에서 왔고 PK 를 알 때만 채운다. nil 이면 표는 읽기 전용이다
	Applied   *Applied // ApplyChanges 만 채운다. UndoChanges 가 되돌리는 묶음을 만든다
	Elapsed   time.Duration
}

// Clients 는 접속을 처음 쓸 때 연다. 접속마다 여는 순서를 지키는 칸(slot)을 두고,
// 기다리는 동안에도 컨텍스트 취소를 따른다(E10).
type Clients struct {
	mu     sync.Mutex
	slots  map[string]*slot
	closed bool
}

type slot struct {
	sem   chan struct{}
	pg    *PGConn
	redis *RedisConn
}

// PGConn 의 pool 은 트리 조회용, sess 는 편집기 실행용 전용 연결이다.
// 편집기는 BEGIN·SET 이 다음 문장까지 이어져야 해서 풀이 아니라 연결 하나를 계속 쓴다.
type PGConn struct {
	pool *pgxpool.Pool
	sess *pgSession
}

// pgSession 의 sem 은 한 번에 하나만 실행하게 하는 잠금이다. 채널이라 기다리다 취소할 수 있다(E10).
// closed 는 Clients.Close 뒤라는 표시다. 잠금 밖에서도 읽으려고 atomic 으로 둔다.
type pgSession struct {
	sem    chan struct{}
	closed atomic.Bool
	conn   *pgxpool.Conn
	inTx   bool
	lost   bool // 연결이 끊겨 트랜잭션·SET 같은 세션 상태를 잃었고 아직 알리지 않았다
	// searchPath 는 SetSearchPath 로 고른 스키마다(string). 새 연결·트랜잭션 종료 때 다시 보낸다.
	// 화면이 실행 중에도 읽어서 atomic 으로 둔다. 쓰기는 sem 안에서만 한다
	searchPath atomic.Value
}

// RedisConn 도 SELECT·MULTI 가 이어지도록 편집기 명령은 연결 하나짜리 전용 클라이언트로 보낸다.
// 전용 클라이언트를 닫으면 진행 중인 읽기의 소켓이 끊겨 취소가 실제로 걸린다.
type RedisConn struct {
	cl   *redis.Client
	opts redis.Options // NewClient 가 채우기 전의 접속 설정. 세션 클라이언트를 새로 만들 때 쓴다
	sess *redisSession
}

type redisSession struct {
	sem    chan struct{}
	closed atomic.Bool
	cl     *redis.Client
	lost   bool // 세션 연결이 끊겨 SELECT 한 DB 를 잃었고 아직 알리지 않았다
}

type redisWriter struct{ strings.Builder }

type redisReply struct {
	v   any
	err error
}

func NewClients() *Clients {
	return &Clients{slots: map[string]*slot{}}
}

// acquire 는 크기 1 채널 잠금을 잡는다. 기다리다 컨텍스트가 끝나면 포기한다.
func acquire(ctx context.Context, sem chan struct{}) error {
	select {
	case sem <- struct{}{}:
		if err := ctx.Err(); err != nil {
			<-sem
			return err
		}
		return nil
	case <-ctx.Done():
		return ctx.Err()
	}
}

// lock 은 접속 uuid 의 칸을 잡는다. 돌려받은 함수로 놓는다.
func (cs *Clients) lock(ctx context.Context, uuid string) (*slot, func(), error) {
	cs.mu.Lock()
	if cs.closed {
		cs.mu.Unlock()
		return nil, nil, errClosed
	}
	s, ok := cs.slots[uuid]
	if !ok {
		s = &slot{sem: make(chan struct{}, 1)}
		cs.slots[uuid] = s
	}
	cs.mu.Unlock()
	if err := acquire(ctx, s.sem); err != nil {
		return nil, nil, err
	}
	// 기다리는 사이 Close 가 칸을 닫고 놓았을 수 있다
	cs.mu.Lock()
	closed := cs.closed
	cs.mu.Unlock()
	if closed {
		<-s.sem
		return nil, nil, errClosed
	}
	return s, func() { <-s.sem }, nil
}

func (cs *Clients) PG(ctx context.Context, c Conn) (*PGConn, error) {
	s, unlock, err := cs.lock(ctx, c.UUID)
	if err != nil {
		return nil, err
	}
	defer unlock()
	if s.pg != nil {
		return s.pg, nil
	}
	port := c.Port
	if port == 0 {
		port = 5432
	}
	u := url.URL{Scheme: "postgres", User: url.UserPassword(c.User, c.Password), Host: net.JoinHostPort(c.Host, strconv.Itoa(port)), Path: "/" + c.DB}
	// 원격은 TLS 를 강제해 평문으로 떨어지지 않게 한다. 로컬 컨테이너는 보통 TLS 가 없어 prefer 로 둔다
	sslmode := "require"
	if ip := net.ParseIP(c.Host); c.Host == "localhost" || (ip != nil && ip.IsLoopback()) {
		sslmode = "prefer"
	}
	u.RawQuery = "sslmode=" + sslmode + "&connect_timeout=8&application_name=db"
	cfg, err := pgxpool.ParseConfig(u.String())
	if err != nil {
		return nil, fmt.Errorf("invalid connection config for %s", c.Name) // URL 에 비밀번호가 있어 원문 오류를 싣지 않는다
	}
	cfg.MaxConns = 4
	pool, err := pgxpool.NewWithConfig(ctx, cfg)
	if err != nil {
		return nil, err
	}
	if err := pool.Ping(ctx); err != nil {
		pool.Close()
		return nil, err
	}
	s.pg = &PGConn{pool: pool, sess: &pgSession{sem: make(chan struct{}, 1)}}
	return s.pg, nil
}

// Redis 는 평문 PING 을 먼저 하고, 3초 안에 답이 없으면 TLS 로 다시 붙는다.
// ElastiCache 인증서는 *.cache.amazonaws.com 이라 .local 별칭과 맞지 않으므로 서버 이름은 CNAME 대상을 쓴다(명세 R4.3).
func (cs *Clients) Redis(ctx context.Context, c Conn) (*RedisConn, error) {
	s, unlock, err := cs.lock(ctx, c.UUID)
	if err != nil {
		return nil, err
	}
	defer unlock()
	if s.redis != nil {
		return s.redis, nil
	}
	port := c.Port
	if port == 0 {
		port = 6379
	}
	db := 0
	if c.DB != "" {
		if db, err = strconv.Atoi(c.DB); err != nil {
			return nil, fmt.Errorf("invalid redis db %q for %s", c.DB, c.Name)
		}
	}
	opts := &redis.Options{
		Addr: net.JoinHostPort(c.Host, strconv.Itoa(port)), Username: c.User, Password: c.Password, DB: db,
		DialTimeout: 3 * time.Second, ReadTimeout: 15 * time.Second, MaxRetries: -1, // 재시도 없음(E11)
		// 기본값은 컨텍스트 마감을 무시하고 ReadTimeout 만 따른다. 켜야 평문 3초 시도가 실제로 3초에 끝난다(E10).
		ContextTimeoutEnabled: true,
	}
	// NewClient 가 넘긴 옵션에 Dialer 를 채워 넣으므로 template 은 NewClient 에 넘기지 않고 사본만 넘긴다
	template := *opts
	// 평문 시도는 비밀번호 없이 한다. TLS 전용 서버에 비밀번호를 평문으로 흘리지 않기 위해서다
	probe := template
	probe.Username, probe.Password = "", ""
	pcl, plainErr := pingRedis(ctx, &probe)
	if pcl != nil {
		_ = pcl.Close() // 평문 응답 여부만 보려던 연결이다
	}
	var replyErr redis.Error
	var cl *redis.Client
	used := template
	if plainErr == nil || errors.As(plainErr, &replyErr) {
		// 평문으로 답했다(인증 불필요 또는 NOAUTH). 이제 비밀번호를 넣어 평문으로 붙는다(명세 R4.3 예외)
		plain := template
		if cl, err = pingRedis(ctx, &plain); err != nil {
			return nil, err
		}
	} else {
		lctx, cancel := context.WithTimeout(ctx, 3*time.Second)
		cname, err := net.DefaultResolver.LookupCNAME(lctx, c.Host)
		cancel()
		if err != nil {
			cname = c.Host // CNAME 이 없으면 호스트 이름 그대로 검증한다
		}
		used.TLSConfig = &tls.Config{ServerName: strings.TrimSuffix(cname, "."), MinVersion: tls.VersionTLS12}
		tlsOpts := used
		if cl, err = pingRedis(ctx, &tlsOpts); err != nil {
			return nil, fmt.Errorf("plain: %v / tls: %w", plainErr, err)
		}
	}
	s.redis = &RedisConn{cl: cl, opts: used, sess: &redisSession{sem: make(chan struct{}, 1)}}
	return s.redis, nil
}

func pingRedis(ctx context.Context, opts *redis.Options) (*redis.Client, error) {
	cl := redis.NewClient(opts)
	pctx, cancel := context.WithTimeout(ctx, 3*time.Second)
	defer cancel()
	if err := cl.Ping(pctx).Err(); err != nil {
		_ = cl.Close() // 실패한 시도를 정리할 뿐이라 닫기 오류로 할 일이 없다
		return nil, err
	}
	return cl, nil
}

// Close 는 새 작업을 막고, 각 칸·세션 잠금을 잡은 뒤 연결을 닫고 잠금을 놓는다.
// 세션 잠금보다 닫힌 표시를 먼저 세워, 뒤에 오거나 잠금을 기다리던 실행은 막히지 않고 errClosed 로 끝난다.
// 진행 중인 실행은 호출 전에 취소해야 한다. 그래도 잠금이 3초 안에 안 풀리면 그 칸은 건너뛴다
// (프로세스가 끝나며 소켓이 닫힌다). 종료 시점이라 닫기 오류로 할 수 있는 일이 없어 버린다.
func (cs *Clients) Close() {
	cs.mu.Lock()
	cs.closed = true
	slots := make([]*slot, 0, len(cs.slots))
	for _, s := range cs.slots {
		slots = append(slots, s)
	}
	cs.mu.Unlock()
	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
	defer cancel()
	for _, s := range slots {
		if acquire(ctx, s.sem) != nil {
			continue
		}
		// 칸 잠금 안에서만 s.pg·s.redis 를 읽는다(PG·Redis 가 그 잠금 안에서 채운다)
		if s.pg != nil {
			s.pg.sess.closed.Store(true)
		}
		if s.redis != nil {
			s.redis.sess.closed.Store(true)
		}
		if s.pg != nil && acquire(ctx, s.pg.sess.sem) == nil {
			s.pg.shutdown()
			<-s.pg.sess.sem
		}
		if s.redis != nil && acquire(ctx, s.redis.sess.sem) == nil {
			if s.redis.sess.cl != nil {
				_ = s.redis.sess.cl.Close()
				s.redis.sess.cl = nil
			}
			_ = s.redis.cl.Close()
			<-s.redis.sess.sem
		}
		<-s.sem
	}
}

// Forget 은 접속 uuid 의 풀과 편집기 세션을 닫고 칸을 비운다. 다음 PG·Redis 는 넘긴 Conn(바꾼 DB, 고친 접속 등)으로 새로 연다(명세 R27.1·R59.3).
// 칸 자체는 map 에 남긴다. lock 을 기다리던 호출이 지운 칸을 잡고 그 칸에 연결을 채우면 아무도 닫지 못해서다.
// 실행 중인 문장이 ctx 안에 끝나지 않으면 아무것도 닫지 않고 오류를 돌려준다. 호출부는 실행을 먼저 취소한다.
// 옛 PGConn 은 이후 편집기 실행이 errClosed, 풀 조회가 closed pool 오류로 끝난다. 고른 search_path 도 같이 버린다.
func (cs *Clients) Forget(ctx context.Context, uuid string) error {
	s, unlock, err := cs.lock(ctx, uuid)
	if err != nil {
		return err
	}
	defer unlock()
	if s.redis != nil {
		if err := acquire(ctx, s.redis.sess.sem); err != nil {
			return fmt.Errorf("editor session is busy: %w", err)
		}
		s.redis.sess.closed.Store(true)
		if s.redis.sess.cl != nil {
			_ = s.redis.sess.cl.Close() // 버리는 연결이라 닫기 오류로 할 일이 없다
		}
		_ = s.redis.cl.Close()
		<-s.redis.sess.sem
		s.redis = nil
	}
	if s.pg == nil {
		return nil
	}
	if err := acquire(ctx, s.pg.sess.sem); err != nil {
		return fmt.Errorf("editor session is busy: %w", err)
	}
	// 세션 잠금을 기다리던 실행은 잠금을 잡은 뒤 closed 를 보고 errClosed 로 끝난다
	s.pg.sess.closed.Store(true)
	s.pg.shutdown()
	<-s.pg.sess.sem
	s.pg = nil
	return nil
}

// shutdown 은 세션 연결을 돌려주고 풀을 닫는다. 세션 잠금을 잡은 채로 부른다.
// 풀의 Close 는 빌려 간 연결이 돌아올 때까지 기다리므로 세션 연결을 먼저 놓는다.
func (p *PGConn) shutdown() {
	if p.sess.conn != nil {
		p.sess.conn.Release()
		p.sess.conn = nil
	}
	p.pool.Close()
}

// ─── PostgreSQL ───

// Databases 는 같은 서버에서 접속할 수 있는 데이터베이스 이름이다. 템플릿·접속 금지 DB·CONNECT 권한이 없는 DB 는 뺀다.
func (p *PGConn) Databases(ctx context.Context) ([]string, error) {
	rows, err := p.pool.Query(ctx, `select datname::text from pg_database
		where datallowconn and not datistemplate and has_database_privilege(datname, 'CONNECT') order by 1`)
	if err != nil {
		return nil, err
	}
	return pgx.CollectRows(rows, pgx.RowTo[string])
}

// SetSearchPath 는 편집기 세션에 SET search_path TO "<schema>", public 을 보내고, 성공하면 값을 기억해
// 세션이 다시 열릴 때와 트랜잭션이 끝날 때 다시 보낸다(명세 R27.2).
// 트랜잭션 안에서 보낸 SET 은 ROLLBACK 하면 되돌아가서 끝날 때도 다시 보낸다.
func (p *PGConn) SetSearchPath(ctx context.Context, schema string) (err error) {
	c, err := p.openSession(ctx)
	if err != nil {
		return err
	}
	defer func() { _, err = p.sess.release(&Result{}, err) }()
	if _, err = c.Exec(ctx, searchPathSQL(schema)); err != nil {
		return err
	}
	p.sess.searchPath.Store(schema)
	return nil
}

// SearchPath 는 SetSearchPath 로 고른 스키마다. 고른 적 없으면 빈 문자열이다.
func (p *PGConn) SearchPath() string {
	v, _ := p.sess.searchPath.Load().(string)
	return v
}

func searchPathSQL(schema string) string {
	return "SET search_path TO " + QuoteIdent(schema) + ", public"
}

func (p *PGConn) FindSchemas(ctx context.Context) ([]string, error) {
	rows, err := p.pool.Query(ctx, `select nspname from pg_namespace
		where nspname not like 'pg\_%' and nspname <> 'information_schema' order by 1`)
	if err != nil {
		return nil, err
	}
	return pgx.CollectRows(rows, pgx.RowTo[string])
}

func (p *PGConn) FindTables(ctx context.Context, schema string) ([]string, error) {
	rows, err := p.pool.Query(ctx, `select c.relname from pg_class c join pg_namespace n on n.oid = c.relnamespace
		where n.nspname = $1 and c.relkind in ('r','p','v','m','f') order by 1`, schema)
	if err != nil {
		return nil, err
	}
	return pgx.CollectRows(rows, pgx.RowTo[string])
}

// Run 은 편집기 세션 연결에서 문장 하나를 실행한다.
func (p *PGConn) Run(ctx context.Context, sql string) (*Result, error) {
	return p.RunArgs(ctx, Query{SQL: sql})
}

// RunArgs 는 Run 에 바인딩 인자를 더한 것이다.
// extended protocol 은 여러 문장을 거부하므로 문장 나누기가 틀려도 뒤 문장이 몰래 실행되지 않는다(명세 R3.7).
// 값은 텍스트 형식으로 받아 서버 표기 그대로 보여 준다.
func (p *PGConn) RunArgs(ctx context.Context, q Query) (res *Result, err error) {
	start := time.Now()
	c, err := p.openSession(ctx)
	if err != nil {
		return nil, err
	}
	defer func() { res, err = p.sess.release(res, err) }()
	if q.ReadOnly {
		// 트랜잭션 밖이면 READ ONLY 트랜잭션을 열고 닫는다. 안이면(연 사이 시작한 트랜잭션) SAVEPOINT 안에서만 읽기 전용으로 두고
		// 그 SAVEPOINT 로 되돌려 사용자 트랜잭션을 원래 모드로 돌린다. 읽기만 했으므로 되돌려도 잃는 것이 없다
		begin, end, abort := "BEGIN READ ONLY", "COMMIT", "ROLLBACK"
		if txOpen(c) {
			begin = "SAVEPOINT dbtui_ro; SET LOCAL transaction_read_only = on"
			end = "ROLLBACK TO SAVEPOINT dbtui_ro; RELEASE SAVEPOINT dbtui_ro"
			abort = end
		}
		if _, err = c.Exec(ctx, begin); err != nil {
			return nil, err
		}
		defer func() {
			if c.Conn().IsClosed() {
				return
			}
			if err == nil {
				if _, err = c.Exec(ctx, end); err != nil {
					res = nil
				}
				return
			}
			rctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
			defer cancel()
			if _, rerr := c.Exec(rctx, abort); rerr != nil {
				err = errors.Join(err, fmt.Errorf("rollback: %w", rerr))
			}
		}()
	}
	// pgx 는 인자가 없어도 DescribeExec 면 extended protocol 로 보낸다. string 인자는 텍스트로 보내 서버가 타입을 정한다
	opts := append([]any{pgx.QueryExecModeDescribeExec, pgx.QueryResultFormats{pgx.TextFormatCode}}, q.Args...)
	w := FirstSQLWord(q.SQL)

	// SELECT·VALUES·TABLE 은 커서로 1,001행만 받아 1,000행에서 끊는다. 실행을 취소하면 연결이 닫혀
	// 세션 상태가 사라지므로 취소로 끊지 않는다(명세 R3.8). 그 밖의 문장(쓰기·RETURNING·SHOW·EXPLAIN)은 끝까지 받는다.
	var fields []pgconn.FieldDescription
	if (w != "select" && w != "values" && w != "table") || !IsReadOnlySQL(q.SQL) {
		rows, err := c.Query(ctx, q.SQL, opts...)
		if err != nil {
			return nil, err
		}
		res = collectRows(rows, MaxRows, nil)
		if err := rows.Err(); err != nil {
			return nil, err
		}
		// 필드 정보는 연결이 다음 문장에 다시 쓰는 버퍼라 복사해 둔다
		fields = slices.Clone(rows.FieldDescriptions())
		if len(res.Cols) == 0 {
			res.Text = rows.CommandTag().String()
		}
	} else {
		// 트랜잭션 밖이면 커서용 트랜잭션을 직접 열고, 실패하면 직접 닫는다
		began := !txOpen(c)
		defer func() {
			if err != nil && began && !c.Conn().IsClosed() {
				rctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
				defer cancel()
				_, _ = c.Exec(rctx, "ROLLBACK") // 실패하면 연결이 닫히고 다음 실행이 세션 유실로 알린다
			}
		}()
		if began {
			if _, err = c.Exec(ctx, "BEGIN"); err != nil {
				return nil, err
			}
		}
		// pgx 의 Exec 는 인자가 없으면 simple protocol 로 바꿔 여러 문장을 실행하므로 Query(extended)로 보낸다(R3.7)
		decl, err := c.Query(ctx, "DECLARE dbtui_cursor NO SCROLL CURSOR FOR "+trimStatement(q.SQL), opts...)
		if err != nil {
			return nil, err
		}
		decl.Close()
		if err = decl.Err(); err != nil {
			return nil, err
		}
		// FETCH 문자열은 매번 같아 기본 모드(문장 캐시)면 첫 커서의 열 정보가 남는다. 매번 describe 한다.
		rows, err := c.Query(ctx, fmt.Sprintf("FETCH FORWARD %d FROM dbtui_cursor", MaxRows+1),
			pgx.QueryExecModeDescribeExec, pgx.QueryResultFormats{pgx.TextFormatCode})
		if err != nil {
			return nil, err
		}
		res = collectRows(rows, MaxRows, nil)
		if err = rows.Err(); err != nil {
			return nil, err
		}
		fields = slices.Clone(rows.FieldDescriptions())
		if _, err = c.Exec(ctx, "CLOSE dbtui_cursor"); err != nil {
			return nil, err
		}
		if began {
			if _, err = c.Exec(ctx, "COMMIT"); err != nil {
				return nil, err
			}
		}
	}

	if oid := singleTableOID(fields); oid != 0 {
		// 문장은 이미 성공(쓰기면 커밋)했다. 출처를 못 읽어도 오류로 바꾸지 않고 읽기 전용 결과로 돌려준다.
		// 사용자가 취소해도 조회가 끊겨 열린 트랜잭션이 깨지지 않게 실행 ctx 의 취소와 떼고 짧은 마감만 둔다.
		// 한계: 열린 트랜잭션 안에서 이 카탈로그 조회 자체가 서버 오류를 내면 그 트랜잭션은 깨진다. 막으려면 SAVEPOINT 로 감싸야 한다
		lctx, cancel := context.WithTimeout(context.WithoutCancel(ctx), 5*time.Second)
		if rel, lerr := p.lookupRel(lctx, oid); lerr == nil {
			res.Origin = buildOrigin(rel, fields)
		}
		cancel()
	}
	res.Elapsed = time.Since(start)
	return res, nil
}

// openSession 은 세션 잠금을 잡고 편집기 연결을 돌려준다. 성공하면 release 로 놓아야 하고, 실패하면 잠금을 이미 놓았다.
func (p *PGConn) openSession(ctx context.Context) (*pgxpool.Conn, error) {
	s := p.sess
	if s.closed.Load() {
		return nil, errClosed
	}
	if err := acquire(ctx, s.sem); err != nil {
		return nil, err
	}
	if s.closed.Load() {
		<-s.sem
		return nil, errClosed
	}
	if s.conn != nil && s.conn.Conn().IsClosed() {
		s.conn.Release()
		s.conn, s.lost = nil, true
	}
	if s.lost {
		// 끊긴 세션의 상태(트랜잭션·SET)를 모르고 이어 쓰지 않도록 한 번은 실행하지 않고 알린다(명세 R3.6)
		s.lost = false
		<-s.sem
		return nil, errors.New("이전 세션 연결이 끊겨 트랜잭션·SET 같은 세션 상태가 사라졌다. 다시 실행하면 새 연결에서 돈다")
	}
	if s.conn == nil {
		c, err := p.pool.Acquire(ctx)
		if err != nil {
			<-s.sem
			return nil, err
		}
		if sp := p.SearchPath(); sp != "" {
			if _, err := c.Exec(ctx, searchPathSQL(sp)); err != nil {
				c.Release()
				<-s.sem
				return nil, fmt.Errorf("restore search_path: %w", err)
			}
		}
		s.conn, s.inTx = c, false
	}
	return s.conn, nil
}

// release 는 세션 상태를 갱신하고 잠금을 놓는다. 실패해도 트랜잭션이 열려 있으면 화면이 알 수 있게 결과에 InTx 를 싣는다(R3.6).
func (s *pgSession) release(res *Result, err error) (*Result, error) {
	defer func() { <-s.sem }()
	if s.conn.Conn().IsClosed() {
		s.lost = true // 실행 중 취소 등으로 연결이 닫혔다. 다음 실행에서 알린다
	}
	wasInTx := s.inTx
	s.inTx = txOpen(s.conn)
	if sp, _ := s.searchPath.Load().(string); sp != "" && wasInTx && !s.inTx && !s.conn.Conn().IsClosed() {
		// 트랜잭션 안에서 고른 search_path 는 ROLLBACK 이 되돌린다. 실패하면 연결이 닫히고 다음 실행이 세션 유실로 알린다
		rctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		_, _ = s.conn.Exec(rctx, searchPathSQL(sp))
		cancel()
	}
	if err != nil {
		return &Result{InTx: s.inTx}, err
	}
	res.InTx = s.inTx
	return res, nil
}

// collectRows 는 행을 끝까지 읽되 표에는 limit 행까지만 싣는다. raw 가 있으면 행마다 자르기 전 원값을 넘긴다(NULL 은 nil).
// raw 에 넘긴 조각은 다음 행을 읽으면 바뀌므로 붙잡아 둘 때는 복사한다.
func collectRows(rows pgx.Rows, limit int, raw func([][]byte)) *Result {
	defer rows.Close()
	res := &Result{}
	tm := rows.Conn().TypeMap()
	for _, f := range rows.FieldDescriptions() {
		res.Cols = append(res.Cols, f.Name)
		// 사용자 정의 타입(enum 등)은 기본 타입 표에 없다. 카탈로그를 한 번 더 조회하지 않고 OID 로 보여 준다
		typ := strconv.FormatUint(uint64(f.DataTypeOID), 10)
		if t, ok := tm.TypeForOID(f.DataTypeOID); ok {
			typ = t.Name
		}
		res.Types = append(res.Types, typ)
	}
	for rows.Next() {
		vals := rows.RawValues()
		if raw != nil {
			raw(vals)
		}
		if len(res.Rows) >= limit {
			res.Truncated = true
			continue
		}
		row := make([]string, len(vals))
		for i, v := range vals {
			if v == nil {
				row[i] = "NULL"
			} else {
				row[i] = CapCell(v) // []byte 를 통째로 string 으로 복사하지 않게 자른 뒤 바꾼다
			}
		}
		res.Rows = append(res.Rows, row)
	}
	return res
}

func txOpen(c *pgxpool.Conn) bool {
	st := c.Conn().PgConn().TxStatus()
	return st == 'T' || st == 'E'
}

// ─── Redis ───

func (r *RedisConn) GetKeys(ctx context.Context, pattern string) (keys []string, more bool, err error) {
	var cursor uint64
	for {
		batch, next, err := r.cl.Scan(ctx, cursor, pattern, 500).Result()
		if err != nil {
			return nil, false, err
		}
		keys = append(keys, batch...)
		cursor = next
		if cursor == 0 {
			break
		}
		if len(keys) >= MaxRows {
			more = true
			break
		}
	}
	if len(keys) > MaxRows {
		keys, more = keys[:MaxRows], true
	}
	// SCAN 은 리해시 중 같은 키를 두 번 줄 수 있다
	sort.Strings(keys)
	keys = slices.Compact(keys)
	return keys, more, nil
}

func (r *RedisConn) GetValue(ctx context.Context, key string) (*Result, error) {
	start := time.Now()
	typ, err := r.cl.Type(ctx, key).Result()
	if err != nil {
		return nil, err
	}
	ttl, err := r.cl.TTL(ctx, key).Result()
	if err != nil {
		return nil, err
	}
	ttlText := "∞"
	if ttl >= 0 {
		ttlText = ttl.String()
	}
	res := &Result{Text: fmt.Sprintf("%s · %s · TTL %s", key, typ, ttlText)}
	mem, ok, err := r.memoryUsage(ctx, key)
	if err != nil {
		return nil, err
	}
	if ok {
		res.Text += " · 메모리 " + humanBytes(mem)
	}
	switch typ {
	case "string":
		size, err := r.cl.StrLen(ctx, key).Result()
		if err != nil {
			return nil, err
		}
		v, err := r.cl.GetRange(ctx, key, 0, maxStringBytes-1).Result()
		if err != nil {
			return nil, err
		}
		res.Text += "\n\n" + PrettyJSON(v)
		if size > maxStringBytes {
			res.Truncated = true
			res.Text += fmt.Sprintf("\n\n… %d바이트 중 앞 %d바이트만 보여 줌", size, maxStringBytes)
		}
	case "hash":
		// HGETALL 은 큰 hash 에서 상한이 없어 HSCAN 으로 MaxRows 근처까지만 읽는다.
		// 한계: Redis 에는 원소 일부만 읽는 명령이 없어 원소 하나가 크면 통째로 받은 뒤 자른다(명세 R4.1)
		res.Cols = []string{"field", "value"}
		var cursor uint64
		for {
			kv, next, err := r.cl.HScan(ctx, key, cursor, "*", 500).Result()
			if err != nil {
				return nil, err
			}
			for i := 0; i+1 < len(kv); i += 2 {
				res.Rows = append(res.Rows, []string{CapCell(kv[i]), CapCell(kv[i+1])})
			}
			cursor = next
			if cursor == 0 || len(res.Rows) >= MaxRows {
				res.Truncated = cursor != 0
				break
			}
		}
		sort.Slice(res.Rows, func(i, j int) bool { return res.Rows[i][0] < res.Rows[j][0] })
	case "list":
		vs, err := r.cl.LRange(ctx, key, 0, MaxRows).Result() // 하나 더 받아 정확히 1,000개와 잘림을 가른다
		if err != nil {
			return nil, err
		}
		if len(vs) > MaxRows {
			vs, res.Truncated = vs[:MaxRows], true
		}
		res.Cols = []string{"#", "value"}
		for i, v := range vs {
			res.Rows = append(res.Rows, []string{strconv.Itoa(i), CapCell(v)})
		}
	case "set":
		res.Cols = []string{"member"}
		var cursor uint64
		for {
			vs, next, err := r.cl.SScan(ctx, key, cursor, "*", 500).Result()
			if err != nil {
				return nil, err
			}
			for _, v := range vs {
				res.Rows = append(res.Rows, []string{CapCell(v)})
			}
			cursor = next
			if cursor == 0 || len(res.Rows) >= MaxRows {
				res.Truncated = cursor != 0
				break
			}
		}
		sort.Slice(res.Rows, func(i, j int) bool { return res.Rows[i][0] < res.Rows[j][0] })
	case "zset":
		zs, err := r.cl.ZRangeWithScores(ctx, key, 0, MaxRows).Result()
		if err != nil {
			return nil, err
		}
		if len(zs) > MaxRows {
			zs, res.Truncated = zs[:MaxRows], true
		}
		res.Cols = []string{"member", "score"}
		for _, z := range zs {
			res.Rows = append(res.Rows, []string{CapCell(fmt.Sprint(z.Member)), strconv.FormatFloat(z.Score, 'f', -1, 64)})
		}
	case "stream":
		xs, err := r.cl.XRangeN(ctx, key, "-", "+", 200).Result()
		if err != nil {
			return nil, err
		}
		res.Cols = []string{"id", "fields"}
		for _, x := range xs {
			b, err := json.Marshal(x.Values)
			if err != nil {
				return nil, err
			}
			res.Rows = append(res.Rows, []string{x.ID, CapCell(string(b))})
		}
	case "none":
		res.Text += "\n\n(키 없음)"
	default:
		res.Text += "\n\n(보기 미지원 타입)"
	}
	res.Elapsed = time.Since(start)
	return res, nil
}

// run 은 편집기 세션 연결로 명령 하나를 보낸다.
// 사용자가 직접 친 명령이라 서버 응답 크기는 막지 않고, 화면에 싣는 줄 수와 칸 크기만 제한한다(E63).
func (r *RedisConn) Run(ctx context.Context, args []string) (*Result, error) {
	start := time.Now()
	s := r.sess
	if s.closed.Load() {
		return nil, errClosed
	}
	if err := acquire(ctx, s.sem); err != nil {
		return nil, err
	}
	defer func() { <-s.sem }()
	if s.closed.Load() {
		return nil, errClosed
	}
	if s.lost {
		s.lost = false
		return nil, errors.New("이전 Redis 세션 연결이 끊겨 SELECT 한 DB 같은 세션 상태가 사라졌다. 다시 실행하면 새 연결에서 돈다")
	}
	if s.cl == nil {
		// 연결 하나만 쓰고, 유휴 교체를 끄고, 두 번째 연결 맺기는 거부한다.
		// 몰래 다시 붙으면 SELECT 가 풀린 채 명령이 다른 DB 로 가기 때문이다(명세 R4.2).
		o := r.opts
		o.PoolSize, o.MinIdleConns, o.ConnMaxIdleTime, o.ConnMaxLifetime = 1, 0, -1, 0
		var dials atomic.Int32
		o.OnConnect = func(context.Context, *redis.Conn) error {
			if dials.Add(1) > 1 {
				return errSessionReset
			}
			return nil
		}
		s.cl = redis.NewClient(&o)
	}
	in := make([]any, len(args))
	for i, a := range args {
		in[i] = a
	}
	// go-redis 는 컨텍스트의 마감만 보고 취소는 보지 않아 BLPOP 같은 명령이 Ctrl+C 로 끊기지 않는다.
	// 취소되면 세션 클라이언트를 닫아 소켓을 끊고, 다음 명령은 새 세션으로 보낸다.
	// 새 세션은 SELECT 한 DB 를 잊는다. 명령은 서버에서 이미 돌았을 수 있다(E18).
	sc := s.cl
	done := make(chan redisReply, 1)
	go func() {
		v, err := sc.Do(ctx, in...).Result()
		done <- redisReply{v, err}
	}()
	var rep redisReply
	select {
	case <-ctx.Done():
		_ = sc.Close() // 소켓을 끊으려고 닫는 것이라 닫기 오류는 의미가 없다
		<-done
		s.cl = nil
		return nil, ctx.Err() // 취소는 사용자가 한 일이라 따로 세션 유실을 알리지 않는다(R4.2 명세에 적음)
	case rep = <-done:
	}
	if errors.Is(rep.err, errSessionReset) {
		_ = s.cl.Close() // 다시 붙지 못하게 막은 세션을 버린다
		s.cl, s.lost = nil, true
	}
	if rep.err != nil && !errors.Is(rep.err, redis.Nil) {
		return nil, rep.err
	}
	res := &Result{}
	switch x := rep.v.(type) {
	case []any:
		res.Cols = []string{"#", "value"}
		for i, e := range x {
			if i >= MaxRows {
				res.Truncated = true
				break
			}
			res.Rows = append(res.Rows, []string{strconv.Itoa(i), formatRedis(e)})
		}
	case map[any]any:
		res.Cols = []string{"key", "value"}
		for k, e := range x {
			if len(res.Rows) >= MaxRows {
				res.Truncated = true // 맵은 순서가 없어 앞 1,000개가 정렬상 앞이라는 보장은 없다
				break
			}
			res.Rows = append(res.Rows, []string{formatRedis(k), formatRedis(e)})
		}
		sort.Slice(res.Rows, func(i, j int) bool { return res.Rows[i][0] < res.Rows[j][0] })
	default:
		v, ok := rep.v.(string) // string 은 칸 상한(64KiB)이 아니라 1MiB 까지 보여 준다
		if !ok {
			v = formatRedis(rep.v)
		}
		if len(v) > maxStringBytes {
			v = truncateBytes(v[:maxStringBytes]) + fmt.Sprintf("\n\n… %d바이트 중 앞 %d바이트만 보여 줌", len(v), maxStringBytes)
			res.Truncated = true
		}
		res.Text = PrettyJSON(v)
	}
	res.Elapsed = time.Since(start)
	return res, nil
}

// CapCell 은 칸을 MaxCellBytes(E63)에서 자르고 `…(N바이트)` 꼬리를 붙인다. IsCappedCell 이 이 꼬리로 잘린 칸을 알아보므로 다른 저장소 패키지도 이것을 쓴다
func CapCell[T string | []byte](s T) string {
	if len(s) <= MaxCellBytes {
		return string(s)
	}
	return truncateBytes(string(s[:MaxCellBytes])) + fmt.Sprintf("…(%d바이트)", len(s))
}

// cappedRe 는 capCell·formatRedis 가 자른 칸의 꼬리다.
var cappedRe = regexp.MustCompile(`…\((\d+바이트|잘림)\)$`)

// IsCappedCell 은 칸이 MaxCellBytes 에서 잘렸는지 본다. 잘린 칸은 MaxCellBytes 를 넘으므로 길이도 같이 본다.
func IsCappedCell(s string) bool { return len(s) > MaxCellBytes && cappedRe.MatchString(s) }

// truncateBytes 는 바이트로 자른 끝에 걸린 깨진 UTF-8 조각을 버린다.
func truncateBytes(s string) string { return strings.ToValidUTF8(s, "") }

// formatRedis 는 값을 문자열로 바꾸되 MaxCellBytes 를 넘으면 더 만들지 않는다(E63).
func formatRedis(v any) string {
	var b redisWriter
	b.write(v)
	if b.Len() > MaxCellBytes {
		return truncateBytes(b.String()[:MaxCellBytes]) + "…(잘림)"
	}
	return b.String()
}

func (b *redisWriter) write(v any) {
	if b.Len() > MaxCellBytes {
		return
	}
	switch x := v.(type) {
	case nil:
		b.WriteString("(nil)")
	case string:
		b.WriteString(x[:min(len(x), MaxCellBytes+1-b.Len())]) // 남은 예산만 복사한다
	case map[any]any:
		b.WriteString("{")
		for k, e := range x {
			b.write(k)
			b.WriteString(": ")
			b.write(e)
			b.WriteString(", ")
			if b.Len() > MaxCellBytes {
				return
			}
		}
		b.WriteString("}")
	case []any:
		b.WriteString("[")
		for i, e := range x {
			if i > 0 {
				b.WriteString(", ")
			}
			b.write(e)
			if b.Len() > MaxCellBytes {
				return
			}
		}
		b.WriteString("]")
	default:
		fmt.Fprint(b, x)
	}
}

func PrettyJSON(s string) string {
	t := strings.TrimSpace(s)
	if !(strings.HasPrefix(t, "{") || strings.HasPrefix(t, "[")) {
		return s
	}
	var buf bytes.Buffer
	if json.Indent(&buf, []byte(t), "", "  ") != nil {
		return s
	}
	return buf.String()
}
