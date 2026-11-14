package db

import (
	"context"

	"github.com/jackc/pgx/v5"

	"github.com/Jeong-Jae-Hun/i-db/internal/complete"
)

// 자동완성 목록은 키 입력마다 훑으므로 큰 DB 에서도 메모리·지연이 늘지 않게 끊는다.
const (
	catalogTableLimit    = 5000
	catalogColumnLimit   = 20000
	catalogFunctionLimit = 2000
	// 트리(FindTables)와 같은 종류: 테이블·파티션 테이블·뷰·구체화 뷰·외부 테이블
	catalogRelkinds = `c.relkind in ('r','p','v','m','f')`
)

type catalogTableKey struct{ schema, table string }

// LoadCatalog 는 자동완성에 쓸 스키마·테이블·뷰·열·함수 이름을 읽는다.
// 편집기 세션이 아니라 pool 에서 읽어 실행 중인 쿼리·열린 트랜잭션과 겹치지 않는다.
// 한도를 넘으면 이름순 뒤쪽 테이블·열이 빠진다.
func (p *PGConn) LoadCatalog(ctx context.Context) (*complete.Catalog, error) {
	// 시스템 스키마 거르기는 FindSchemas 한 곳에만 두고, 이후 조회는 그 목록으로 좁힌다
	schemas, err := p.FindSchemas(ctx)
	if err != nil {
		return nil, err
	}
	cat := &complete.Catalog{Schemas: make([]complete.Schema, len(schemas))}
	schemaIdx := make(map[string]int, len(schemas))
	for i, s := range schemas {
		cat.Schemas[i].Name = s
		schemaIdx[s] = i
	}

	rows, err := p.pool.Query(ctx, `select n.nspname, c.relname, c.relkind in ('v','m')
		from pg_class c join pg_namespace n on n.oid = c.relnamespace
		where n.nspname = any($1) and `+catalogRelkinds+`
		order by 1, 2 limit $2`, schemas, catalogTableLimit)
	if err != nil {
		return nil, err
	}
	tableIdx := map[catalogTableKey][2]int{}
	var schema, table string
	var view bool
	for rows.Next() {
		if err := rows.Scan(&schema, &table, &view); err != nil {
			rows.Close()
			return nil, err
		}
		si := schemaIdx[schema]
		tableIdx[catalogTableKey{schema, table}] = [2]int{si, len(cat.Schemas[si].Tables)}
		cat.Schemas[si].Tables = append(cat.Schemas[si].Tables, complete.Table{Name: table, View: view})
	}
	if err := rows.Err(); err != nil {
		return nil, err
	}

	rows, err = p.pool.Query(ctx, `select n.nspname, c.relname, a.attname, format_type(a.atttypid, a.atttypmod)
		from pg_attribute a join pg_class c on c.oid = a.attrelid join pg_namespace n on n.oid = c.relnamespace
		where n.nspname = any($1) and `+catalogRelkinds+` and a.attnum > 0 and not a.attisdropped
		order by n.nspname, c.relname, a.attnum limit $2`, schemas, catalogColumnLimit)
	if err != nil {
		return nil, err
	}
	var col complete.Column
	for rows.Next() {
		if err := rows.Scan(&schema, &table, &col.Name, &col.Type); err != nil {
			rows.Close()
			return nil, err
		}
		// 테이블 한도로 빠진 테이블의 열은 버린다
		if at, ok := tableIdx[catalogTableKey{schema, table}]; ok {
			t := &cat.Schemas[at[0]].Tables[at[1]]
			t.Columns = append(t.Columns, col)
		}
	}
	if err := rows.Err(); err != nil {
		return nil, err
	}

	rows, err = p.pool.Query(ctx, `select distinct p.proname from pg_proc p join pg_namespace n on n.oid = p.pronamespace
		where n.nspname = any($1) order by 1 limit $2`, schemas, catalogFunctionLimit)
	if err != nil {
		return nil, err
	}
	if cat.Functions, err = pgx.CollectRows(rows, pgx.RowTo[string]); err != nil {
		return nil, err
	}
	return cat, nil
}
