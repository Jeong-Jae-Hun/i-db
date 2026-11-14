package db

import (
	"context"
	"fmt"
	"strings"
	"time"

	"github.com/jackc/pgx/v5"
)

// 운영 화면(세션·잠금·크기, 세션 취소·종료)은 TUI 전용이다. MCP 서버는 읽기 전용 원칙(R12.2)에 따라
// 이 함수들을 노출하지 않고, 읽기 전용 판정도 pg_cancel_backend·pg_terminate_backend 를 계속 막는다.
// 모두 편집기 세션이 아니라 풀에서 돌아 편집기의 트랜잭션·SET 과 섞이지 않는다.
const (
	// 쿼리 글은 2048자(바이트가 아니라 글자, UTF-8 중간을 자르지 않게)까지만 싣는다
	activityQuery = `select pid, coalesce(usename, '') as usename, coalesce(application_name, '') as application_name,
		coalesce(host(client_addr), '') as client_addr, coalesce(state, '') as state,
		coalesce(wait_event_type || ':' || wait_event, '') as wait,
		date_trunc('second', now() - xact_start) as xact_age,
		date_trunc('second', now() - query_start) as query_age,
		coalesce(left(query, 2048), '') as query
		from pg_stat_activity where pid <> pg_backend_pid()
		order by state = 'active' desc, query_start nulls last`
	// pg_blocking_pids 는 잠금을 기다리지 않는 세션에 빈 배열을 돌려주므로 Lock 대기만 훑어 비용을 줄인다
	lockPairsQuery = `select w.pid as blocked_pid, coalesce(w.usename, '') as blocked_user,
		date_trunc('second', now() - w.query_start) as waiting,
		coalesce((select string_agg(distinct coalesce(l.relation::regclass::text, l.locktype) || ' ' || l.mode, ', ')
			from pg_locks l where l.pid = w.pid and not l.granted), '') as waiting_for,
		b.pid as blocking_pid, coalesce(b.usename, '') as blocking_user, coalesce(b.state, '') as blocking_state,
		coalesce(left(w.query, 2048), '') as blocked_query, coalesce(left(b.query, 2048), '') as blocking_query
		from pg_stat_activity w
		cross join lateral unnest(pg_blocking_pids(w.pid)) as bp(pid)
		join pg_stat_activity b on b.pid = bp.pid
		where w.wait_event_type = 'Lock'
		order by w.query_start, w.pid, b.pid`
	lockModesQuery = `select mode, count(*) filter (where granted), count(*) filter (where not granted)
		from pg_locks group by mode order by count(*) desc, mode`
	// reltuples 는 ANALYZE 전이면 -1(PostgreSQL 14+)이라 NULL 로 보여 준다. toast 는 pg_table_size 에서 따로 떼어 본다
	tableSizesQuery = `select n.nspname as schema, c.relname as name, c.relkind::text as kind,
		case when c.reltuples < 0 then null else c.reltuples::bigint end as rows_estimate,
		pg_size_pretty(pg_relation_size(c.oid)) as table_size,
		pg_size_pretty(pg_indexes_size(c.oid)) as index_size,
		pg_size_pretty(coalesce(pg_total_relation_size(nullif(c.reltoastrelid, 0)), 0)) as toast_size,
		pg_size_pretty(pg_total_relation_size(c.oid)) as total_size,
		pg_total_relation_size(c.oid) as total_bytes
		from pg_class c join pg_namespace n on n.oid = c.relnamespace
		where c.relkind in ('r', 'p', 'm') and n.nspname not in ('pg_catalog', 'information_schema')
			and n.nspname not like 'pg\_toast%'
		order by pg_total_relation_size(c.oid) desc, n.nspname, c.relname
		limit 200`
	cancelQuery    = `select pg_cancel_backend($1)`
	terminateQuery = `select pg_terminate_backend($1)`
)

// 다른 세션의 쿼리 글을 싣는 열. ALTER ROLE … PASSWORD '…' 같은 문장이 화면·복사본에 평문으로 나오지 않게 가린다
var secretCols = map[string]bool{"query": true, "blocked_query": true, "blocking_query": true}

type lockMode struct {
	Mode             string
	Granted, Waiting int64
}

type backendSignal struct {
	query string
	pid   int
}

// Activity 는 자기 연결을 뺀 pg_stat_activity 다. 실행 중인 세션이 먼저 온다.
func (p *PGConn) Activity(ctx context.Context) (*Result, error) {
	return p.poolResult(ctx, activityQuery)
}

// Locks 는 막힌 세션·막는 세션 쌍을 행으로, pg_locks 의 잠금 모드별 개수를 Text 로 돌려준다.
func (p *PGConn) Locks(ctx context.Context) (*Result, error) {
	res, err := p.poolResult(ctx, lockPairsQuery)
	if err != nil {
		return nil, err
	}
	rows, err := p.pool.Query(ctx, lockModesQuery)
	if err != nil {
		return nil, err
	}
	modes, err := pgx.CollectRows(rows, pgx.RowToStructByPos[lockMode])
	if err != nil {
		return nil, err
	}
	parts := make([]string, 0, len(modes))
	for _, m := range modes {
		s := fmt.Sprintf("%s %d", m.Mode, m.Granted)
		if m.Waiting > 0 {
			s += fmt.Sprintf(" (+%d waiting)", m.Waiting)
		}
		parts = append(parts, s)
	}
	res.Text = "pg_locks: " + strings.Join(parts, ", ")
	return res, nil
}

// TableSizes 는 전체 크기가 큰 테이블·파티션 부모·구체화 뷰 200개다. 정렬용으로 total_bytes 를 숫자 열로 둔다.
func (p *PGConn) TableSizes(ctx context.Context) (*Result, error) {
	return p.poolResult(ctx, tableSizesQuery)
}

// CancelBackend 는 그 세션의 실행 중인 쿼리를 취소한다. 쓰기라 호출하는 쪽이 확인을 받는다.
func (p *PGConn) CancelBackend(ctx context.Context, pid int) error {
	return p.signalBackend(ctx, backendSignal{cancelQuery, pid})
}

// TerminateBackend 는 그 세션을 끊는다. 열린 트랜잭션은 롤백된다.
func (p *PGConn) TerminateBackend(ctx context.Context, pid int) error {
	return p.signalBackend(ctx, backendSignal{terminateQuery, pid})
}

// signalBackend 는 함수가 false 를 돌려주는 경우(그런 PostgreSQL 세션이 없음)도 오류로 바꾼다.
// 권한이 없으면 서버가 오류를 낸다.
func (p *PGConn) signalBackend(ctx context.Context, s backendSignal) error {
	var ok bool
	if err := p.pool.QueryRow(ctx, s.query, s.pid).Scan(&ok); err != nil {
		return err
	}
	if !ok {
		return fmt.Errorf("backend %d was not signaled (not a PostgreSQL server process?)", s.pid)
	}
	return nil
}

// poolResult 는 풀에서 인자 없는 조회 하나를 텍스트 형식으로 받아 표로 만든다.
func (p *PGConn) poolResult(ctx context.Context, sql string) (*Result, error) {
	start := time.Now()
	rows, err := p.pool.Query(ctx, sql, pgx.QueryResultFormats{pgx.TextFormatCode})
	if err != nil {
		return nil, err
	}
	res := collectRows(rows, MaxRows, nil)
	if err := rows.Err(); err != nil {
		return nil, err
	}
	maskQueryCols(res)
	res.Elapsed = time.Since(start)
	return res, nil
}

// maskQueryCols 는 쿼리 글 열의 값에 MaskSecrets 를 씌운다.
func maskQueryCols(res *Result) {
	for c, name := range res.Cols {
		if !secretCols[name] {
			continue
		}
		// 결과 행은 이 함수만 쥐고 있어 복사하지 않고 그 자리에서 바꾼다
		for _, row := range res.Rows {
			row[c] = MaskSecrets(row[c])
		}
	}
}
