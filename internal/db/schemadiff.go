package db

import (
	"context"
	"maps"
	"slices"

	"github.com/jackc/pgx/v5"
)

// ─── 스키마 비교 (R53) ───

// 스키마 하나의 테이블·열·인덱스·제약을 읽는다. search_path 를 그 스키마로 두고 읽어
// pg_get_viewdef·pg_get_constraintdef·format_type·regclass 가 같은 스키마 이름을 붙이지 않게 한다. 그래야 두 스키마(또는 두 DB)의 정의를 글자로 견줄 수 있다.
const (
	snapRelQuery = `select c.relname, c.relkind::text,
		coalesce(case when c.relkind in ('v', 'm') then pg_get_viewdef(c.oid, true) end, '')
		from pg_class c join pg_namespace n on n.oid = c.relnamespace
		where n.nspname = $1 and c.relkind in ('r', 'p', 'v', 'm', 'f')`
	snapColQuery = `select c.relname, a.attname, format_type(a.atttypid, a.atttypmod), a.attnotnull,
		coalesce(pg_get_expr(d.adbin, d.adrelid), ''), a.attidentity::text, a.attgenerated::text
		from pg_attribute a join pg_class c on c.oid = a.attrelid join pg_namespace n on n.oid = c.relnamespace
		left join pg_attrdef d on d.adrelid = a.attrelid and d.adnum = a.attnum
		where n.nspname = $1 and c.relkind in ('r', 'p', 'v', 'm', 'f') and a.attnum > 0 and not a.attisdropped
		order by c.relname, a.attnum`
	// 인덱스(true)와 제약(false)은 같은 꼴(테이블, 이름, 정의)이라 한 번에 읽는다
	// pg_get_indexdef 는 search_path 와 무관하게 테이블을 늘 스키마로 한정해서 ' 스키마.' 를 지운다
	snapObjQuery = `select c.relname, true, ic.relname, replace(pg_get_indexdef(i.indexrelid), ' ' || quote_ident(n.nspname) || '.', ' ')
		from pg_index i join pg_class c on c.oid = i.indrelid join pg_class ic on ic.oid = i.indexrelid
		join pg_namespace n on n.oid = c.relnamespace where n.nspname = $1
		union all
		select c.relname, false, k.conname, pg_get_constraintdef(k.oid)
		from pg_constraint k join pg_class c on c.oid = k.conrelid join pg_namespace n on n.oid = c.relnamespace
		where n.nspname = $1 and k.contype in ('p', 'u', 'c', 'f', 'x')`
)

// Snapshot 은 스키마 하나의 구조다. 테이블 이름 → 정의.
type Snapshot struct {
	Schema string
	Tables map[string]*SnapTable
}

// SnapTable 의 Columns 는 열 번호 순서, Indexes·Constraints 는 이름 → 정의다.
// Kind 는 TABLE·VIEW 등이고 View 는 뷰 정의다.
type SnapTable struct {
	Kind, View  string
	Columns     []SnapColumn
	Indexes     map[string]string
	Constraints map[string]string
}

// SnapColumn 의 Def 는 "타입 NOT NULL DEFAULT …" 처럼 열 정의 글이다.
type SnapColumn struct{ Name, Def string }

var snapKindWords = map[string]string{"r": "TABLE", "p": "PARTITIONED TABLE", "v": "VIEW", "m": "MATERIALIZED VIEW", "f": "FOREIGN TABLE"}

// SchemaSnapshot 은 스키마 구조를 읽기 전용 트랜잭션 하나에서 읽는다. 편집기 세션이 아니라 풀을 쓴다.
// 세 조회가 같은 스냅숏을 보도록 REPEATABLE READ 로 연다. READ COMMITTED 면 조회 사이에 만든 테이블의 열이 테이블 없이 온다.
// 없는 스키마는 빈 스냅숏이다.
func (p *PGConn) SchemaSnapshot(ctx context.Context, schema string) (Snapshot, error) {
	tx, err := p.pool.BeginTx(ctx, pgx.TxOptions{IsoLevel: pgx.RepeatableRead, AccessMode: pgx.ReadOnly})
	if err != nil {
		return Snapshot{Schema: schema, Tables: map[string]*SnapTable{}}, err
	}
	// 읽기만 하므로 끝은 늘 되돌리기다. set_config(…, true) 도 트랜잭션과 함께 사라진다
	defer func() { _ = tx.Rollback(context.WithoutCancel(ctx)) }()
	if _, err := tx.Exec(ctx, "select set_config('search_path', $1, true)", QuoteIdent(schema)); err != nil {
		return Snapshot{Schema: schema, Tables: map[string]*SnapTable{}}, err
	}
	return readSnapshot(ctx, tx, schema)
}

// snapQuerier 는 readSnapshot 이 쓰는 pgx.Tx 의 조회 부분이다. 시험이 가짜 행으로 바꿔 끼운다.
type snapQuerier interface {
	Query(ctx context.Context, sql string, args ...any) (pgx.Rows, error)
}

// readSnapshot 은 SchemaSnapshot 의 세 조회를 읽어 합친다. 첫 조회에 없던 테이블의 열·인덱스·제약은 버린다(같은 스냅숏이면 생기지 않는다).
func readSnapshot(ctx context.Context, tx snapQuerier, schema string) (Snapshot, error) {
	snap := Snapshot{Schema: schema, Tables: map[string]*SnapTable{}}
	rows, err := tx.Query(ctx, snapRelQuery, schema)
	if err != nil {
		return snap, err
	}
	var name, kind, view string
	_, err = pgx.ForEachRow(rows, []any{&name, &kind, &view}, func() error {
		snap.Tables[name] = &SnapTable{Kind: snapKindWords[kind], View: view, Indexes: map[string]string{}, Constraints: map[string]string{}}
		return nil
	})
	if err != nil {
		return snap, err
	}

	if rows, err = tx.Query(ctx, snapColQuery, schema); err != nil {
		return snap, err
	}
	var c colInfo
	_, err = pgx.ForEachRow(rows, []any{&name, &c.Name, &c.Type, &c.NotNull, &c.Default, &c.Identity, &c.Generated}, func() error {
		def := c.Type
		if c.NotNull {
			def += " NOT NULL"
		}
		if d := columnDefault(c); d != "" {
			def += " " + d
		}
		if t, ok := snap.Tables[name]; ok {
			t.Columns = append(t.Columns, SnapColumn{c.Name, def})
		}
		return nil
	})
	if err != nil {
		return snap, err
	}

	if rows, err = tx.Query(ctx, snapObjQuery, schema); err != nil {
		return snap, err
	}
	var idx bool
	var obj, def string
	_, err = pgx.ForEachRow(rows, []any{&name, &idx, &obj, &def}, func() error {
		t, ok := snap.Tables[name]
		switch {
		case !ok:
		case idx:
			t.Indexes[obj] = def
		default:
			t.Constraints[obj] = def
		}
		return nil
	})
	if err != nil {
		return snap, err
	}
	return snap, nil
}

// DiffSnapshots 는 두 스냅숏의 다른 점을 표로 만든다. 구분은 +(오른쪽에만), −(왼쪽에만), ~(다름)이다.
// 순서는 테이블 이름순, 한 테이블 안에서는 테이블 → 열(왼쪽 열 순서, 오른쪽에만 있는 열은 뒤) → 제약 → 인덱스 이름순이다.
// 한쪽에만 있는 테이블은 테이블 줄 하나로만 보인다. 열 순서만 다른 것은 다름으로 치지 않는다.
func DiffSnapshots(a, b Snapshot) *Result {
	res := &Result{Cols: []string{"구분", "대상", "왼쪽", "오른쪽"}, Types: []string{"text", "text", "text", "text"}}
	for _, n := range unionKeys(a.Tables, b.Tables) {
		lt, lok := a.Tables[n]
		rt, rok := b.Tables[n]
		diffRow(res, "테이블 "+n, tableLabel(lt), tableLabel(rt), lok, rok)
		if !lok || !rok {
			continue
		}
		right := map[string]string{}
		for _, c := range rt.Columns {
			right[c.Name] = c.Def
		}
		for _, c := range lt.Columns {
			r, ok := right[c.Name]
			diffRow(res, "열 "+n+"."+c.Name, c.Def, r, true, ok)
			delete(right, c.Name)
		}
		// 오른쪽에만 있는 열도 오른쪽 열 순서로 보이려고 맵이 아니라 목록을 돈다
		for _, c := range rt.Columns {
			if _, ok := right[c.Name]; ok {
				diffRow(res, "열 "+n+"."+c.Name, "", c.Def, false, true)
			}
		}
		for _, k := range unionKeys(lt.Constraints, rt.Constraints) {
			l, lok := lt.Constraints[k]
			r, rok := rt.Constraints[k]
			diffRow(res, "제약 "+n+"."+k, l, r, lok, rok)
		}
		for _, k := range unionKeys(lt.Indexes, rt.Indexes) {
			l, lok := lt.Indexes[k]
			r, rok := rt.Indexes[k]
			diffRow(res, "인덱스 "+n+"."+k, l, r, lok, rok)
		}
	}
	return res
}

// diffRow 는 한쪽에만 있거나 정의가 다를 때 줄을 더한다.
func diffRow(res *Result, target, l, r string, lok, rok bool) {
	mark := "~"
	switch {
	case !rok:
		mark = "−"
	case !lok:
		mark = "+"
	case l == r:
		return
	}
	res.Rows = append(res.Rows, []string{mark, target, l, r})
}

// unionKeys 는 두 맵의 키를 겹치지 않게 이름순으로 돌려준다.
func unionKeys[V any](a, b map[string]V) []string {
	keys := slices.Collect(maps.Keys(a))
	for k := range b {
		if _, ok := a[k]; !ok {
			keys = append(keys, k)
		}
	}
	slices.Sort(keys)
	return keys
}

// tableLabel 은 테이블 줄에 보일 글이다. 뷰는 정의까지 견준다.
func tableLabel(t *SnapTable) string {
	if t == nil {
		return ""
	}
	if t.View != "" {
		return t.Kind + " AS " + t.View
	}
	return t.Kind
}
