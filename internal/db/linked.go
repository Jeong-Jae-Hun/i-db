package db

import (
	"context"
	"errors"
	"fmt"
	"math"
	"regexp"
	"slices"
	"strconv"
	"strings"
	"sync"

	"github.com/jackc/pgx/v5"
	"github.com/jackc/pgx/v5/pgconn"
)

// ─── 연결된 곳 찾기 (R73) ───
// FK 가 없어도 이름 규칙(user_id → *_user_id, users.id)으로 같은 값이 들었을 열을 찾고 센다.
// 서버를 지키려고 인덱스 첫 열만 세고, 한 테이블은 LinkedCap+1 행에서 멈추며 2초 안에 끝나야 한다.

const (
	// LinkedMaxColumns 는 접속 하나에서 볼 후보 열 수 상한이다
	LinkedMaxColumns = 200
	// LinkedCap 은 테이블 하나에서 셀 행 수다. 넘으면 LinkedCap+1 이 와 화면은 100+ 로 보인다
	LinkedCap = 100
	// linkedStatementTimeout 은 테이블 하나 세기의 서버 시간 제한이다
	linkedStatementTimeout = "2s"
)

// linkedColumnsQuery 는 후보 열이다. $1 같은 이름(id 는 빈 글), $2 base, $3 주인 테이블 이름들, $4 받을 타입, $5 상한.
// 뷰·파티션 자식은 빼고(부모에서 센다) 쓸 수 있는 인덱스의 첫 열(indkey[0])인지 같이 싣는다.
// 만드는 중·깨진 인덱스(indisvalid·indisready·indislive)와 부분 인덱스(indpred)는 col = 값 에 못 쓸 수 있어 뺀다.
// 식 인덱스는 indkey[0] 이 0 이라 열 번호(attnum > 0)와 맞지 않아 저절로 빠진다.
// *_<base>_id 는 LIKE 대신 right() 로 본다. base 의 _ 가 LIKE 와일드카드라서다.
const linkedColumnsQuery = `select n.nspname::text, c.relname::text, a.attname::text, t.typname::text,
	lower(a.attname) = 'id' and lower(c.relname) = any($3) as owner,
	exists(select 1 from pg_index i where i.indrelid = c.oid and i.indkey[0] = a.attnum
		and i.indisvalid and i.indisready and i.indislive and i.indpred is null) as indexed
from pg_attribute a
join pg_class c on c.oid = a.attrelid
join pg_namespace n on n.oid = c.relnamespace
join pg_type t on t.oid = a.atttypid
where c.relkind in ('r', 'p') and not c.relispartition
	and n.nspname not like 'pg\_%' and n.nspname <> 'information_schema'
	and a.attnum > 0 and not a.attisdropped
	and t.typname = any($4)
	and (lower(a.attname) = $1
		or ($2 <> '' and (lower(a.attname) = $2 || '_id' or right(lower(a.attname), length($2) + 4) = '_' || $2 || '_id'))
		or (lower(a.attname) = 'id' and lower(c.relname) = any($3)))
order by owner desc, n.nspname, c.relname, a.attname
limit $5`

var (
	linkedIntRe  = regexp.MustCompile(`^-?(0|[1-9][0-9]*)$`)
	linkedUUIDRe = regexp.MustCompile(`^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$`)
)

// LinkedColumn 은 값이 들었을 수 있는 열 하나다. Owner 면 base 이름 테이블의 id 다.
type LinkedColumn struct {
	Table   TableRef
	Column  string
	Type    string
	Owner   bool
	Indexed bool
}

// LinkedBase 는 열 이름에서 이름 뿌리를 뽑는다. user_id → user, 원본 테이블 t_users 의 id → user.
// 그 밖은 "" 이고 같은 이름 열만 찾는다.
func LinkedBase(col, table string) string {
	col = strings.ToLower(col)
	if b, ok := strings.CutSuffix(col, "_id"); ok && b != "" {
		return b
	}
	if col != "id" || table == "" {
		return ""
	}
	t := strings.ToLower(table)
	for _, p := range []string{"tb_", "t_"} {
		if s, ok := strings.CutPrefix(t, p); ok && s != "" {
			t = s
			break
		}
	}
	// 영어 복수형 몇 가지만 푼다. types → type 처럼 -es 를 통째로 떼면 틀리는 이름이 많아 s·x·z·ch·sh 뒤에서만 뗀다
	switch {
	case len(t) > 3 && strings.HasSuffix(t, "ies"):
		return t[:len(t)-3] + "y"
	case slices.ContainsFunc([]string{"sses", "xes", "zes", "ches", "shes"}, func(s string) bool { return strings.HasSuffix(t, s) }):
		return t[:len(t)-2]
	case strings.HasSuffix(t, "ss"), strings.HasSuffix(t, "us"): // address·status 는 단수다
		return t
	case len(t) > 1 && strings.HasSuffix(t, "s"):
		return t[:len(t)-1]
	}
	return t
}

// LinkedOwnerNames 는 base 의 주인 테이블로 볼 이름이다(소문자). user → user·users·t_users·tb_user …
func LinkedOwnerNames(base string) []string {
	if base == "" {
		return nil
	}
	forms := []string{base, base + "s", base + "es"}
	if stem, ok := strings.CutSuffix(base, "y"); ok && stem != "" {
		forms = append(forms, stem+"ies")
	}
	var out []string
	for _, p := range []string{"", "t_", "tb_"} {
		for _, f := range forms {
			out = append(out, p+f)
		}
	}
	return out
}

// LinkedTypes 는 값과 견줄 수 있는 열 타입(pg_type.typname)이다. 글자 열은 늘 받는다.
// 열을 형 변환하면 인덱스를 못 써서 값이 그대로 들어갈 타입만 고른다. 범위를 넘는 int2·int4 는 서버가 오류를 내 뺀다.
func LinkedTypes(v string) []string {
	types := []string{"text", "varchar"}
	if linkedIntRe.MatchString(v) {
		if n, err := strconv.ParseInt(v, 10, 64); err == nil {
			types = append(types, "int8")
			if n >= math.MinInt32 && n <= math.MaxInt32 {
				types = append(types, "int4")
			}
			if n >= math.MinInt16 && n <= math.MaxInt16 {
				types = append(types, "int2")
			}
		}
	}
	if linkedUUIDRe.MatchString(v) {
		types = append(types, "uuid")
	}
	return types
}

// LinkedColumns 는 이 DB 에서 col·base 규칙과 값 타입에 맞는 열이다. LinkedMaxColumns 를 넘으면 truncated 다.
// col 이 id 면 같은 이름 규칙은 쓰지 않는다. 모든 테이블의 id 가 걸려서다.
func (p *PGConn) LinkedColumns(ctx context.Context, col, base, value string) (cols []LinkedColumn, truncated bool, err error) {
	same := strings.ToLower(col)
	if same == "id" {
		same = ""
	}
	rows, err := p.pool.Query(ctx, linkedColumnsQuery, same, base, LinkedOwnerNames(base), LinkedTypes(value), LinkedMaxColumns+1)
	if err != nil {
		return nil, false, err
	}
	cols, err = pgx.CollectRows(rows, func(row pgx.CollectableRow) (LinkedColumn, error) {
		var c LinkedColumn
		err := row.Scan(&c.Table.Schema, &c.Table.Table, &c.Column, &c.Type, &c.Owner, &c.Indexed)
		return c, err
	})
	if err != nil {
		return nil, false, err
	}
	if len(cols) > LinkedMaxColumns {
		return cols[:LinkedMaxColumns], true, nil
	}
	return cols, false, nil
}

// LinkedCountSQL 은 열 하나에서 값이 든 행을 LinkedCap+1 행까지 세는 문장이다.
func LinkedCountSQL(c LinkedColumn, value string) string {
	return fmt.Sprintf("SELECT count(*) FROM (SELECT 1 FROM %s WHERE %s = %s LIMIT %d) s",
		c.Table.Quoted(), QuoteIdent(c.Column), QuoteLiteral(value), LinkedCap+1)
}

// CountLinked 는 cols 가운데 Indexed 인 열만 workers 개씩 동시에 센다. 세지 않은 열은 counts[i] 가 -1 이다.
func (p *PGConn) CountLinked(ctx context.Context, cols []LinkedColumn, value string, workers int) (counts []int, errs []error) {
	counts, errs = make([]int, len(cols)), make([]error, len(cols))
	jobs := make(chan int)
	var wg sync.WaitGroup
	for range workers {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for i := range jobs {
				counts[i], errs[i] = p.countLinked(ctx, LinkedCountSQL(cols[i], value))
			}
		}()
	}
	for i, c := range cols {
		counts[i] = -1
		if c.Indexed {
			jobs <- i
		}
	}
	close(jobs)
	wg.Wait()
	return counts, errs
}

// countLinked 는 sql(count 하나)을 풀 연결의 READ ONLY 트랜잭션에서 statement_timeout 을 걸고 돌린다.
// db 가 만든 문장이라 RunReadOnly 처럼 세션째 버리지 않고 풀에 돌려준다. 후보마다 새 연결을 열면 prod 에 부담이 커서다.
func (p *PGConn) countLinked(ctx context.Context, sql string) (n int, err error) {
	tx, err := p.pool.BeginTx(ctx, pgx.TxOptions{AccessMode: pgx.ReadOnly})
	if err != nil {
		return 0, err
	}
	defer func() { _ = tx.Rollback(ctx) }()
	if _, err = tx.Exec(ctx, "SET LOCAL statement_timeout = '"+linkedStatementTimeout+"'"); err != nil {
		return 0, err
	}
	err = tx.QueryRow(ctx, sql).Scan(&n)
	return n, err
}

// IsStatementTimeout 은 서버 statement_timeout 으로 멈춘 오류(57014)인지다.
func IsStatementTimeout(err error) bool {
	var pe *pgconn.PgError
	return errors.As(err, &pe) && pe.Code == "57014"
}
