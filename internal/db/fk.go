package db

import (
	"context"
	"fmt"
	"strings"

	"github.com/jackc/pgx/v5"
)

// FK 는 외래 키 하나다. Table 의 Cols 가 Ref 의 RefCols 를 같은 순서로 가리킨다(명세 R36).
type FK struct {
	Name    string
	Table   TableRef
	Cols    []string
	Ref     TableRef
	RefCols []string
}

// 열 이름은 conkey·confkey 배열 순서대로 모은다. attnum 순서로 모으면 (b, a) 처럼 선언한 FK 가 어긋난다.
// 파티션 테이블의 FK 는 파티션마다 conparentid 가 있는 복제 제약을 만든다. 같은 쪽 테이블이 부모와 같은 복제
// (outgoing 은 참조 대상 파티션마다, incoming 은 참조하는 파티션마다 생기는 것)는 같은 FK 라 뺀다.
const fkSelect = `select con.conname::text, sn.nspname::text, sc.relname::text,
		array(select a.attname::text from unnest(con.conkey) with ordinality k(n, i)
			join pg_attribute a on a.attrelid = con.conrelid and a.attnum = k.n order by k.i),
		rn.nspname::text, rc.relname::text,
		array(select a.attname::text from unnest(con.confkey) with ordinality k(n, i)
			join pg_attribute a on a.attrelid = con.confrelid and a.attnum = k.n order by k.i)
	from pg_constraint con
	join pg_class sc on sc.oid = con.conrelid join pg_namespace sn on sn.oid = sc.relnamespace
	join pg_class rc on rc.oid = con.confrelid join pg_namespace rn on rn.oid = rc.relnamespace
	left join pg_constraint pc on pc.oid = con.conparentid
	where con.contype = 'f' `

const (
	fkOutQuery = fkSelect + `and sn.nspname = $1 and sc.relname = $2 and (pc.oid is null or pc.conrelid <> con.conrelid)
	order by con.conname`
	fkInQuery = fkSelect + `and rn.nspname = $1 and rc.relname = $2 and (pc.oid is null or pc.confrelid <> con.confrelid)
	order by sn.nspname, sc.relname, con.conname`
)

// ForeignKeys 는 t 가 다른 테이블을 가리키는 FK 다. 풀 연결이라 편집기 트랜잭션과 섞이지 않는다.
func (p *PGConn) ForeignKeys(ctx context.Context, t TableRef) ([]FK, error) {
	rows, err := p.pool.Query(ctx, fkOutQuery, t.Schema, t.Table)
	if err != nil {
		return nil, err
	}
	return pgx.CollectRows(rows, scanFK)
}

// ReferencedBy 는 t 를 가리키는 다른 테이블(자기 참조 포함)의 FK 다.
func (p *PGConn) ReferencedBy(ctx context.Context, t TableRef) ([]FK, error) {
	rows, err := p.pool.Query(ctx, fkInQuery, t.Schema, t.Table)
	if err != nil {
		return nil, err
	}
	return pgx.CollectRows(rows, scanFK)
}

// scanFK 는 fkSelect 열 순서대로 읽는다. TableRef 가 이름 있는 필드라 RowToStructByPos 로는 펼쳐지지 않는다.
func scanFK(row pgx.CollectableRow) (FK, error) {
	var f FK
	err := row.Scan(&f.Name, &f.Table.Schema, &f.Table.Table, &f.Cols, &f.Ref.Schema, &f.Ref.Table, &f.RefCols)
	return f, err
}

// FKWhere 는 테이블 보기 WHERE 칸에 넣을 `"c1" = 'v1' AND "c2" = 'v2'` 다. 값은 모두 문자열 리터럴로 쓰고
// 형 변환은 PostgreSQL 이 열 타입으로 한다. vals 와 cols 의 길이가 다르거나 비면 오류다.
func FKWhere(cols []string, vals []string) (string, error) {
	if len(cols) != len(vals) || len(cols) == 0 {
		return "", fmt.Errorf("foreign key has %d columns but %d values", len(cols), len(vals))
	}
	parts := make([]string, len(cols))
	for i, c := range cols {
		parts[i] = QuoteIdent(c) + " = " + QuoteLiteral(vals[i])
	}
	return strings.Join(parts, " AND "), nil
}
