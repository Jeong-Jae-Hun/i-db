package db

import (
	"context"
	"errors"
	"fmt"
	"io"
	"strings"

	"github.com/jackc/pgx/v5"
	"github.com/jackc/pgx/v5/pgconn"
)

// ─── 전체 내보내기 (R33.1) ───

// ErrExportNotSelect 는 읽기 문장이지만 COPY (...) 안에 넣을 수 없는 문장(SHOW·EXPLAIN)일 때다.
var ErrExportNotSelect = errors.New("only select, with, values and table statements can be exported")

// progressWriter 는 쓴 바이트를 누적해 매번 progress 에 알린다.
type progressWriter struct {
	w        io.Writer
	n        int64
	progress func(int64)
}

func (pw *progressWriter) Write(b []byte) (int, error) {
	n, err := pw.w.Write(b)
	pw.n += int64(n)
	if pw.progress != nil {
		pw.progress(pw.n)
	}
	return n, err
}

// CopyOut 은 읽기 문장 하나를 1,000행 제한 없이 COPY (...) TO STDOUT WITH (FORMAT csv, HEADER) 로 w 에 쓴다.
// progress 는 받은 누적 바이트로 불린다(nil 이면 안 부른다). 돌려주는 값은 COPY 가 보낸 행 수다.
// 편집기 세션을 오래 붙잡지 않도록 풀 연결의 READ ONLY 트랜잭션에서 돈다. 첫 단어 판정(IsReadOnlySQL)이 놓치는
// CTE 안 쓰기는 서버의 READ ONLY 가 막는다. ctx 를 취소하면 pgx 가 서버에 취소를 보내고 연결은 풀이 버린다.
// SetSearchPath 로 고른 스키마가 있으면 편집기 세션과 같은 search_path 로 돈다(R27.2). 풀 연결은 그 SET 을 받지 않았다.
func (p *PGConn) CopyOut(ctx context.Context, sql string, w io.Writer, progress func(bytes int64)) (rows int64, err error) {
	stmt := trimStatement(sql)
	switch n := len(SplitStatements(stmt)); {
	case n == 0:
		return 0, errors.New("empty statement")
	case n > 1:
		return 0, errors.New("only one statement can be exported")
	}
	if !IsReadOnlySQL(stmt) {
		return 0, ErrNotReadOnly
	}
	switch FirstSQLWord(stmt) {
	case "select", "with", "values", "table":
	default:
		return 0, ErrExportNotSelect
	}
	// CopyTo 는 simple protocol 이라 문장을 감싸는 괄호가 어긋나면 ") TO ... (" 사이로 다른 COPY 옵션을 끼울 수 있다
	depth := 0
	for _, c := range []byte(MaskSQL(stmt)) {
		switch c {
		case '(':
			depth++
		case ')':
			depth--
		}
		if depth < 0 {
			break
		}
	}
	if depth != 0 {
		return 0, errors.New("unbalanced parentheses")
	}
	c, err := p.pool.Acquire(ctx)
	if err != nil {
		return 0, err
	}
	// 트랜잭션이 열린 채(실패·취소) 돌려주면 풀이 그 연결을 버린다
	defer c.Release()
	if _, err = c.Exec(ctx, "BEGIN READ ONLY"); err != nil {
		return 0, err
	}
	// 트랜잭션 안의 SET 이라 ROLLBACK 이 되돌려 풀 연결에 남지 않는다
	if sp := p.SearchPath(); sp != "" {
		if _, err = c.Exec(ctx, searchPathSQL(sp)); err != nil {
			return 0, err
		}
	}
	pw := &progressWriter{w: w, progress: progress}
	// 문장이 줄 주석으로 끝나도 닫는 괄호가 주석에 먹히지 않게 줄을 바꾼다
	tag, err := c.Conn().PgConn().CopyTo(ctx, pw, "COPY ("+stmt+"\n) TO STDOUT WITH (FORMAT csv, HEADER)")
	if err != nil {
		return 0, err
	}
	if _, err = c.Exec(ctx, "ROLLBACK"); err != nil {
		return 0, err
	}
	return tag.RowsAffected(), nil
}

// ─── 데이터 검색 (R33.2) ───

const (
	searchRowsPerTable = 5
	searchMaxTables    = 200
	searchTimeout      = "5s"
	// 글자 열이 있는 일반·파티션 부모 테이블. 파티션은 부모가 함께 읽어 뺀다. SELECT 권한이 없는 테이블도 뺀다.
	// $1 이 빈 문자열이면 풀 연결의 current_schema() 다
	searchTablesQuery = `select n.nspname::text, c.relname::text,
			array_agg(a.attname::text order by a.attnum),
			coalesce((select array_agg(pa.attname::text order by k.ord)
				from pg_index i cross join unnest(i.indkey) with ordinality as k(attnum, ord)
				join pg_attribute pa on pa.attrelid = i.indrelid and pa.attnum = k.attnum
				where i.indrelid = c.oid and i.indisprimary), '{}')
		from pg_class c join pg_namespace n on n.oid = c.relnamespace
		join pg_attribute a on a.attrelid = c.oid and a.attnum > 0 and not a.attisdropped
		join pg_type t on t.oid = a.atttypid
		where n.nspname = coalesce(nullif($1, ''), current_schema()) and c.relkind in ('r', 'p') and not c.relispartition
			and t.typname in ('text', 'varchar', 'bpchar', 'citext', 'json', 'jsonb', 'uuid')
			and has_table_privilege(c.oid, 'SELECT')
		group by n.nspname, c.oid, c.relname order by c.relname`
)

// searchTable·searchHit 은 searchTablesQuery·searchSQL 의 한 행이다. 필드 순서를 쿼리와 맞춘다.
type searchTable struct {
	Schema, Name string
	Cols, PK     []string
}

type searchHit struct{ Col, PK, Val string }

// CountSearchTables 는 SearchData 가 볼 테이블 수다(200개 상한 전). prod 확인 창에 N 을 보이려고 쓴다.
// schema 가 비었으면 SetSearchPath 로 고른 스키마, 그것도 없으면 풀 연결의 current_schema() 다.
func (p *PGConn) CountSearchTables(ctx context.Context, schema string) (n int, err error) {
	if schema == "" {
		schema = p.SearchPath()
	}
	err = p.pool.QueryRow(ctx, "select count(*) from ("+searchTablesQuery+") s", schema).Scan(&n)
	return n, err
}

// SearchData 는 스키마의 테이블마다 글자 열을 ::text ILIKE '%text%' 로 찾는다. 테이블당 5행, 테이블 200개까지,
// 문장마다 statement_timeout 5초, 읽기 전용 트랜잭션이다. 결과 열은 테이블·열·PK·값이고 PK 칸은 `id=3` 꼴이다.
// PK 가 없으면 PK 칸을 비운다. ctid 는 VACUUM 등으로 바뀌어 행을 가리키는 값으로 쓰지 않는다(R15 와 같은 이유).
// 시간이 넘었거나 서버 오류가 난 테이블은 SAVEPOINT 로 되돌리고 skipped 에 넣은 뒤 다음 테이블로 간다.
// 200개를 넘으면 앞 200개만 보고 Truncated 를 세운다. text 안의 % _ \ 는 글자 그대로 찾는다.
func (p *PGConn) SearchData(ctx context.Context, schema, text string) (res *Result, skipped []string, err error) {
	if text == "" {
		return nil, nil, errors.New("empty search text")
	}
	if schema == "" {
		schema = p.SearchPath()
	}
	c, err := p.pool.Acquire(ctx)
	if err != nil {
		return nil, nil, err
	}
	// 트랜잭션이 열린 채(실패·취소) 돌려주면 풀이 그 연결을 버린다
	defer c.Release()
	if _, err = c.Exec(ctx, "BEGIN READ ONLY"); err != nil {
		return nil, nil, err
	}
	// SET LOCAL 은 뒤의 ROLLBACK TO SAVEPOINT 로도 풀리지 않는다(SAVEPOINT 전에 정했다)
	if _, err = c.Exec(ctx, "SET LOCAL statement_timeout = '"+searchTimeout+"'"); err != nil {
		return nil, nil, err
	}
	rows, err := c.Query(ctx, searchTablesQuery+fmt.Sprintf(" limit %d", searchMaxTables+1), schema)
	if err != nil {
		return nil, nil, err
	}
	tables, err := pgx.CollectRows(rows, pgx.RowToStructByPos[searchTable])
	if err != nil {
		return nil, nil, err
	}
	res = &Result{Cols: []string{"테이블", "열", "PK", "값"}}
	if len(tables) > searchMaxTables {
		tables, res.Truncated = tables[:searchMaxTables], true
	}
	// LIKE 의 기본 이스케이프 글자는 백슬래시다. 패턴은 인자로 보내 standard_conforming_strings 와 무관하다
	pattern := strings.NewReplacer(`\`, `\\`, `%`, `\%`, `_`, `\_`).Replace(text)
	for _, t := range tables {
		if _, err = c.Exec(ctx, "SAVEPOINT dbtui_search"); err != nil {
			return nil, nil, err
		}
		var hits []searchHit
		if rows, err = c.Query(ctx, searchSQL(t), pattern); err == nil {
			hits, err = pgx.CollectRows(rows, pgx.RowToStructByPos[searchHit])
		}
		if ctx.Err() != nil {
			return nil, nil, ctx.Err()
		}
		var pgErr *pgconn.PgError
		if errors.As(err, &pgErr) {
			if _, err = c.Exec(ctx, "ROLLBACK TO SAVEPOINT dbtui_search"); err != nil {
				return nil, nil, err
			}
			name := t.Name
			if pgErr.Code != "57014" { // query_canceled 는 statement_timeout 이다. 그 밖의 오류는 까닭을 붙인다
				name += " (" + pgErr.Message + ")"
			}
			skipped = append(skipped, name)
			continue
		}
		if err != nil {
			return nil, nil, err
		}
		for _, h := range hits {
			res.Rows = append(res.Rows, []string{t.Name, h.Col, h.PK, capCell(h.Val)})
		}
	}
	if _, err = c.Exec(ctx, "ROLLBACK"); err != nil {
		return nil, nil, err
	}
	return res, skipped, nil
}

// searchSQL 은 테이블 하나에서 맞는 (행, 열) 쌍을 5개까지 찾는 문장이다. 한 행이 여러 열에서 맞으면 쌍도 여럿이다.
// 결과는 열 이름, `a=1, b=x` 꼴 PK(없으면 빈 글), 값 순서다. 이름은 카탈로그에서 왔지만 그래도 따옴표로 감싼다.
func searchSQL(t searchTable) string {
	vals := make([]string, len(t.Cols))
	for i, col := range t.Cols {
		vals[i] = "(" + QuoteLiteral(col) + ", s." + QuoteIdent(col) + "::text)"
	}
	pk := "''"
	if len(t.PK) > 0 {
		parts := make([]string, len(t.PK))
		for i, col := range t.PK {
			parts[i] = QuoteLiteral(col+"=") + " || s." + QuoteIdent(col) + "::text"
		}
		pk = "concat_ws(', ', " + strings.Join(parts, ", ") + ")"
	}
	return fmt.Sprintf("select m.col, %s, m.val from %s.%s as s cross join lateral (values %s) as m(col, val) "+
		"where m.val ilike '%%' || $1 || '%%' limit %d",
		pk, QuoteIdent(t.Schema), QuoteIdent(t.Name), strings.Join(vals, ", "), searchRowsPerTable)
}
