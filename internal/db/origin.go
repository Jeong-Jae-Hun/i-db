package db

import (
	"context"
	"errors"
	"slices"

	"github.com/jackc/pgx/v5"
	"github.com/jackc/pgx/v5/pgconn"
)

// 결과 열의 출처 테이블을 OID 로 한 번에 읽는다. 뷰·구체화 뷰·외부 테이블은 편집하지 않는다.
// indkey 는 PK 열 순서를 지키고, PK 에는 식 열(0)이 없다. 생성 열(attgenerated)과 GENERATED ALWAYS
// IDENTITY(attidentity = 'a')는 값을 쓸 수 없어 따로 싣는다(attgenerated 는 PostgreSQL 12+).
// 테스트가 조회 실패를 흉내 내려고 바꿀 수 있게 var 로 둔다.
var originQuery = `select n.nspname, c.relname, c.relkind in ('r', 'p'),
	coalesce((select i.indkey::int2[] from pg_index i where i.indrelid = c.oid and i.indisprimary), '{}'),
	array(select a.attnum from pg_attribute a
		where a.attrelid = c.oid and a.attnum > 0 and not a.attisdropped order by a.attnum),
	array(select a.attname::text from pg_attribute a
		where a.attrelid = c.oid and a.attnum > 0 and not a.attisdropped order by a.attnum),
	array(select a.attgenerated <> '' from pg_attribute a
		where a.attrelid = c.oid and a.attnum > 0 and not a.attisdropped order by a.attnum),
	array(select a.attidentity = 'a' from pg_attribute a
		where a.attrelid = c.oid and a.attnum > 0 and not a.attisdropped order by a.attnum)
	from pg_class c join pg_namespace n on n.oid = c.relnamespace where c.oid = $1`

type relOrigin struct {
	table     TableRef
	editable  bool
	pk        []uint16
	names     map[uint16]string
	generated map[uint16]bool
	identity  map[uint16]bool // GENERATED ALWAYS AS IDENTITY
}

// Quoted 는 "스키마"."테이블" 이다.
func (t TableRef) Quoted() string { return QuoteIdent(t.Schema) + "." + QuoteIdent(t.Table) }

// singleTableOID 는 식이 아닌 열이 모두 한 테이블에서 왔을 때 그 OID 를, 아니면 0 을 돌려준다.
func singleTableOID(fields []pgconn.FieldDescription) uint32 {
	var oid uint32
	for _, f := range fields {
		if f.TableOID == 0 {
			continue
		}
		if oid != 0 && oid != f.TableOID {
			return 0
		}
		oid = f.TableOID
	}
	return oid
}

// buildOrigin 은 PK 열이 결과에 모두 있을 때만 출처를 만든다. PK 가 없으면 ctid 로 대신하지 않고 편집을 막는다.
// 같은 테이블 열이 두 번 나오면 자기 조인(a.name, b.name)일 수 있다. 프로토콜은 어느 별칭에서 왔는지 알려 주지 않아
// 다른 행의 값을 이 행의 키로 고치게 되므로 출처를 만들지 않는다. 그래서 id, id as id2 같은 결과도 읽기 전용이 된다.
// 별칭이 달라도 열 번호가 겹치지 않는 자기 조인(a.id, b.name)은 여기서 가를 수 없어 제출 때 옛값 확인이 막는다.
func buildOrigin(rel *relOrigin, fields []pgconn.FieldDescription) *Origin {
	if !rel.editable || len(rel.pk) == 0 {
		return nil
	}
	cols := make([]string, len(fields))
	have := map[uint16]bool{}
	for i, f := range fields {
		if f.TableOID == 0 {
			continue
		}
		n := f.TableAttributeNumber
		name, ok := rel.names[n]
		if !ok || have[n] {
			return nil // 시스템 열(ctid 등)이거나 같은 열이 겹쳤다. 확실하지 않으면 읽기 전용으로 둔다
		}
		have[n] = true
		// 생성 열과 PK 가 아닌 IDENTITY ALWAYS 열은 UPDATE·INSERT 에 값을 쓸 수 없어 식 열처럼 다룬다
		if !rel.generated[n] && (!rel.identity[n] || slices.Contains(rel.pk, n)) {
			cols[i] = name
		}
	}
	o := &Origin{Table: rel.table, PK: make([]string, len(rel.pk)), Cols: cols}
	for i, n := range rel.pk {
		if !have[n] {
			return nil
		}
		o.PK[i] = rel.names[n]
		if rel.identity[n] {
			o.Always = append(o.Always, rel.names[n])
		}
	}
	return o
}

// lookupRel 은 편집기 세션 연결에서 테이블 정보를 읽는다. 세션 잠금을 잡은 상태에서 부른다.
// 캐시하지 않는다. 트랜잭션 안 DDL 의 ROLLBACK 이나 다른 세션의 DDL 을 알아챌 방법이 없어 낡은 열 이름으로 엉뚱한 열을 고치게 된다.
func (p *PGConn) lookupRel(ctx context.Context, oid uint32) (*relOrigin, error) {
	rel := &relOrigin{names: map[uint16]string{}, generated: map[uint16]bool{}, identity: map[uint16]bool{}}
	var pk, nums []int16
	var names []string
	var generated, identity []bool
	// 사용자가 세션에서 DEALLOCATE ALL 을 쳐도 깨지지 않게 문장 캐시를 쓰지 않는다
	err := p.sess.conn.QueryRow(ctx, originQuery, pgx.QueryExecModeDescribeExec, oid).
		Scan(&rel.table.Schema, &rel.table.Table, &rel.editable, &pk, &nums, &names, &generated, &identity)
	if errors.Is(err, pgx.ErrNoRows) {
		return rel, nil // 결과를 받는 사이 테이블이 지워졌다. 편집할 수 없는 출처로 다룬다
	}
	if err != nil {
		return nil, err
	}
	for _, n := range pk {
		rel.pk = append(rel.pk, uint16(n))
	}
	// 네 배열은 같은 attnum 순서라 같은 인덱스가 같은 열이다
	for i, n := range nums {
		rel.names[uint16(n)], rel.generated[uint16(n)], rel.identity[uint16(n)] = names[i], generated[i], identity[i]
	}
	return rel, nil
}
