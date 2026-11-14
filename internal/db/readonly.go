package db

import (
	"context"
	"errors"
	"fmt"
	"strings"
	"time"

	"github.com/jackc/pgx/v5"
)

const readOnlyTimeout = "30s"

var (
	// ErrNotReadOnly 는 IsReadOnlySQL 이 아닌 문장을 거부할 때 쓴다
	ErrNotReadOnly = errors.New("only read-only statements are allowed (select, with, values, table, show, explain without analyze)")

	// 부수효과가 있는 함수다. READ ONLY 트랜잭션에서도 돈다(백엔드 종료·알림·advisory lock·파일 읽기·외부 접속 등).
	// 한계: 이름을 아는 함수만 막는다. 이 함수들을 감싼 사용자 함수·뷰, 새 확장의 함수는 통과하므로
	// 근본 대책은 MCP 전용 읽기 전용 DB 역할이다. query_to_xml·ts_stat 은 문자열로 받은 SQL 을 실행해 이 검사를 비켜 가서 막는다.
	blockedFuncs = map[string]bool{
		"pg_terminate_backend": true, "pg_cancel_backend": true, "pg_signal_backend": true, "pg_notify": true,
		"pg_reload_conf": true, "pg_rotate_logfile": true, "pg_rotate_logfile_old": true, "pg_switch_wal": true, "pg_switch_xlog": true,
		"pg_create_restore_point": true, "pg_logical_emit_message": true, "pg_promote": true,
		"pg_read_file": true, "pg_read_file_old": true, "pg_read_binary_file": true, "pg_stat_file": true,
		"loread": true, "lowrite": true, "set_config": true, "ts_stat": true, "cursor_to_xml": true, "cursor_to_xmlschema": true,
		"pg_log_backend_memory_contexts": true, "pg_import_system_collations": true, "pg_prewarm": true,
		"pg_truncate_visibility_map": true, "nextval": true, "setval": true,
	}
	blockedFuncPrefixes = []string{
		"dblink", "postgres_fdw_", "pg_advisory", "pg_try_advisory", "lo_", "pg_ls_", "pg_file_",
		"pg_stat_reset", "pg_stat_statements_reset", "pg_wal_replay_", "pg_backup_", "pg_start_backup", "pg_stop_backup",
		"pg_create_physical_replication_slot", "pg_create_logical_replication_slot", "pg_drop_replication_slot",
		"pg_replication_slot_advance", "pg_logical_slot_", "pg_copy_", "pg_replication_origin_", "query_to_xml",
	}
)

// CheckReadOnlySQL 은 RunReadOnly 가 실행하기 전에 하는 검사다. 호출부는 접속을 열기 전에 같은 검사로 미리 거를 수 있다.
func CheckReadOnlySQL(sql string) error {
	// copy ... to program·commit·set transaction read write·call·do 는 READ ONLY 트랜잭션 안에서도 돈다
	if !IsReadOnlySQL(sql) {
		return ErrNotReadOnly
	}
	name, err := blockedCall(sql)
	switch {
	case err != nil:
		return err
	case name != "":
		return fmt.Errorf("function %s is not allowed: it has side effects even in a read-only transaction", name)
	}
	return nil
}

// blockedCall 은 blockedFuncs 호출의 이름을 돌려준다. 없으면 빈 문자열이다.
// 쓰기 경로(CLI --write, R58)도 같은 목록으로 위험을 본다.
func blockedCall(sql string) (string, error) {
	// 아래는 blockedFuncs 호출(식별자 뒤 '(')을 찾는다. 문자열·$태그$·주석은 건너뛰고,
	// 스키마 한정(pg_catalog.x)과 "따옴표 이름"도 마지막 식별자로 대소문자 없이 본다. 사이의 공백·주석은 무시한다.
	// 문자열은 standard_conforming_strings=on 규칙으로 읽는다(RunReadOnly 가 트랜잭션에 그 값을 고정한다).
	prev := ""    // 바로 앞 토큰이 식별자면 소문자 이름
	prevEnd := -1 // 그 식별자가 끝난 위치. E'...' 처럼 접두어가 붙었는지 본다
	for i := 0; i < len(sql); {
		c := sql[i]
		switch {
		case strings.HasPrefix(sql[i:], "--"):
			j := strings.IndexByte(sql[i:], '\n')
			if j < 0 {
				return "", nil
			}
			i += j + 1
		case strings.HasPrefix(sql[i:], "/*"):
			// 블록 주석은 중첩된다
			depth := 0
			for i < len(sql) {
				if strings.HasPrefix(sql[i:], "/*") {
					depth, i = depth+1, i+2
				} else if strings.HasPrefix(sql[i:], "*/") {
					depth, i = depth-1, i+2
					if depth == 0 {
						break
					}
				} else {
					i++
				}
			}
		case c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == '\f' || c == '\v':
			i++
		case c == '\'' || c == '"':
			escapes := c == '\'' && prev == "e" && prevEnd == i
			j := i + 1
			for ; j < len(sql); j++ {
				if escapes && sql[j] == '\\' {
					j++
					continue
				}
				if sql[j] != c {
					continue
				}
				if j+1 < len(sql) && sql[j+1] == c { // '' 이스케이프
					j++
					continue
				}
				break
			}
			prev, prevEnd = "", -1
			if c == '"' {
				prev = strings.ToLower(strings.ReplaceAll(sql[i+1:min(j, len(sql))], `""`, `"`))
			}
			i = j + 1
		case c == '$':
			i++ // $1 같은 위치 인자나 연산자의 $
			if tag := dollarTagRe.FindString(sql[i-1:]); tag != "" {
				if k := strings.Index(sql[i-1+len(tag):], tag); k >= 0 {
					i += 2*len(tag) + k - 1
				} else {
					i = len(sql)
				}
			}
			prev, prevEnd = "", -1
		case IsIdentByte(c) && (c < '0' || c > '9'):
			j := i
			for j < len(sql) && IsIdentByte(sql[j]) {
				j++
			}
			prev, prevEnd = strings.ToLower(sql[i:j]), j
			// U&"..." 은 \XXXX 로 글자를 숨긴 식별자라 이름을 대조할 수 없다
			if prev == "u" && strings.HasPrefix(sql[j:], `&"`) {
				return "", errors.New(`unicode-escaped identifiers (U&"...") are not allowed`)
			}
			i = j
		case c == '(':
			blocked := blockedFuncs[prev]
			for _, p := range blockedFuncPrefixes {
				blocked = blocked || prev != "" && strings.HasPrefix(prev, p)
			}
			if blocked {
				return prev, nil
			}
			prev, prevEnd = "", -1
			i++
		default:
			prev, prevEnd = "", -1
			i++
		}
	}
	return "", nil
}

// RunReadOnly 는 편집기 세션이 아니라 풀 연결에서 문장 하나를 읽기 전용 트랜잭션으로 실행한다.
// MCP 처럼 사람이 확인하지 않는 호출을 위한 것이라, 호출부와 별개로 CheckReadOnlySQL 에 걸리는 문장은 여기서도 거부하고
// 첫 단어 판정이 놓치는 CTE 안 쓰기·select ... into 는 서버의 READ ONLY 가 막게 한다. 결과는 MaxRows 행까지 싣는다.
func (p *PGConn) RunReadOnly(ctx context.Context, sql string) (*Result, error) {
	return p.RunReadOnlyRows(ctx, sql, MaxRows)
}

// RunReadOnlyRows 는 RunReadOnly 와 같되 결과를 limit 행까지 싣는다. CLI(R58)의 --max-rows 가 MaxRows 를 넘을 수 있어서다.
func (p *PGConn) RunReadOnlyRows(ctx context.Context, sql string, limit int) (res *Result, err error) {
	if err = CheckReadOnlySQL(sql); err != nil {
		return nil, err
	}
	start := time.Now()
	c, err := p.pool.Acquire(ctx)
	if err != nil {
		return nil, err
	}
	// ROLLBACK 해도 advisory lock·LISTEN·SET 같은 세션 상태는 남는다. 풀에 돌려주지 않고 떼어 내 닫아
	// 세션째 버린다. pgx 문장 캐시가 서버 쪽 prepared statement 를 기억해 DISCARD ALL 로 비우면 어긋난다.
	defer func() {
		cctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		if cerr := c.Hijack().Close(cctx); cerr != nil && err == nil {
			res, err = nil, fmt.Errorf("close read-only connection: %w", cerr)
		}
	}()
	if _, err = c.Exec(ctx, "BEGIN READ ONLY"); err != nil {
		return nil, err
	}
	if _, err = c.Exec(ctx, "SET LOCAL statement_timeout = '"+readOnlyTimeout+"'"); err != nil {
		return nil, err
	}
	// CheckReadOnlySQL 의 함수 차단은 표준 문자열 규칙(백슬래시가 이스케이프가 아님)으로 문장을 읽는다.
	// 서버 설정이 off 면 같은 문장을 다르게 읽어 차단을 비켜 갈 수 있어 고정한다
	if _, err = c.Exec(ctx, "SET LOCAL standard_conforming_strings = on"); err != nil {
		return nil, err
	}

	// select·values·table·with 는 커서로 MaxRows+1 행만 받는다. 실행 취소로 끊으면 연결이 닫혀서다.
	// 커서에 못 넣는 show·explain 은 결과가 작아 끝까지 읽고 버린다(collectRows).
	// 두 경로 모두 extended protocol 이라 여러 문장은 서버가 거부한다. pgx Exec 는 인자가 없으면
	// simple protocol 로 바꿔 여러 문장을 실행하므로 사용자 문장에는 쓰지 않는다.
	switch FirstSQLWord(sql) {
	case "select", "values", "table", "with":
		decl := "DECLARE dbtui_ro_cursor NO SCROLL CURSOR FOR " + trimStatement(sql)
		if _, err = c.Conn().PgConn().ExecParams(ctx, decl, nil, nil, nil, nil).Close(); err != nil {
			return nil, err
		}
		// FETCH 문자열은 매번 같아 문장 캐시를 쓰면 앞 커서의 열 정보가 남는다. 매번 describe 한다
		sql = fmt.Sprintf("FETCH FORWARD %d FROM dbtui_ro_cursor", limit+1)
	case "":
		return nil, errors.New("empty statement")
	}
	rows, err := c.Query(ctx, sql, pgx.QueryExecModeDescribeExec, pgx.QueryResultFormats{pgx.TextFormatCode})
	if err != nil {
		return nil, err
	}
	res = collectRows(rows, limit, nil)
	if err = rows.Err(); err != nil {
		return nil, err
	}
	if len(res.Cols) == 0 {
		res.Text = rows.CommandTag().String()
	}
	res.Elapsed = time.Since(start)
	return res, nil
}
