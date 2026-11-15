package db

import (
	"context"
	"errors"
	"fmt"
	"strings"

	"github.com/jackc/pgx/v5"
)

// 카탈로그 조회는 테이블 하나 범위라 크기가 정해져 있다(열은 PostgreSQL 이 1,600개로 막는다). 표에는 MaxRows 까지만 싣는다(E63).
const (
	// 파티션은 부모가 pg_inherits 에 하나뿐이다. relpartbound 는 "FOR VALUES ..." 나 "DEFAULT" 로 풀린다
	relQuery = `select c.oid, c.relkind::text, coalesce(obj_description(c.oid, 'pg_class'), ''),
		coalesce(pg_get_partkeydef(c.oid), ''),
		coalesce(case when c.relkind in ('v', 'm') then pg_get_viewdef(c.oid, true) end, ''),
		coalesce(pn.nspname, ''), coalesce(pc.relname, ''), coalesce(pg_get_expr(c.relpartbound, c.oid), '')
		from pg_class c join pg_namespace n on n.oid = c.relnamespace
		left join pg_inherits h on c.relispartition and h.inhrelid = c.oid
		left join pg_class pc on pc.oid = h.inhparent
		left join pg_namespace pn on pn.oid = pc.relnamespace
		where n.nspname = $1 and c.relname = $2`
	// attidentity·attgenerated 는 PostgreSQL 12 부터 있다
	colQuery = `select a.attname, format_type(a.atttypid, a.atttypmod), a.attnotnull,
		coalesce(pg_get_expr(d.adbin, d.adrelid), ''), a.attidentity::text, a.attgenerated::text,
		coalesce(col_description(a.attrelid, a.attnum), '')
		from pg_attribute a left join pg_attrdef d on d.adrelid = a.attrelid and d.adnum = a.attnum
		where a.attrelid = $1 and a.attnum > 0 and not a.attisdropped order by a.attnum`
	// NOT NULL 은 열 정의에 붙이므로 뺀다(PostgreSQL 18 은 contype 'n' 으로도 저장한다). 상속받은 제약은 부모가 만든다
	conQuery = `select conname, pg_get_constraintdef(oid) from pg_constraint
		where conrelid = $1 and contype in ('p', 'u', 'c', 'f', 'x') and conislocal
		order by case contype when 'p' then 0 when 'u' then 1 when 'c' then 2 when 'x' then 3 else 4 end, conname`
	idxQuery = `select ic.relname, pg_get_indexdef(i.indexrelid),
		exists (select 1 from pg_constraint k where k.conindid = i.indexrelid and k.conrelid = i.indrelid
			and k.contype in ('p', 'u', 'x')),
		exists (select 1 from pg_inherits h where h.inhrelid = i.indexrelid)
		from pg_index i join pg_class ic on ic.oid = i.indexrelid where i.indrelid = $1 order by ic.relname`
)

var relKindWords = map[string]string{"r": "TABLE", "p": "TABLE", "v": "VIEW", "m": "MATERIALIZED VIEW"}

type TableRef struct{ Schema, Table string }

// 아래 구조체는 pgx.RowToStructByPos 로 select 열 순서대로 채운다. 필드 순서를 쿼리와 맞춰야 한다.
type relInfo struct {
	OID                    uint32
	Kind, Comment, PartKey string
	ViewDef                string
	// 파티션일 때만 채워진다
	ParentSchema, ParentTable, PartBound string
}

type colInfo struct {
	Name, Type          string
	NotNull             bool
	Default             string
	Identity, Generated string
	Comment             string
}

type conInfo struct{ Name, Def string }

type idxInfo struct {
	Name, Def string
	Backing   bool // PRIMARY KEY·UNIQUE·EXCLUDE 제약이 만든 인덱스다. DDL 에서는 제약이 다시 만든다
	Inherited bool // 부모 파티션 테이블의 인덱스가 붙인 것이다. DDL 에서는 PARTITION OF 가 다시 만든다
}

type tableMeta struct {
	rel  relInfo
	cols []colInfo
	cons []conInfo
	idx  []idxInfo
}

// QuoteIdent 는 식별자를 늘 큰따옴표로 감싼다. 예약어·대문자 판정 없이도 항상 맞다.
func QuoteIdent(s string) string { return `"` + strings.ReplaceAll(s, `"`, `""`) + `"` }

// QuoteLiteral 은 PostgreSQL quote_literal 과 같다. 백슬래시가 있으면 standard_conforming_strings 설정과
// 무관하게 같은 값이 되도록 E” 로 쓴다.
func QuoteLiteral(s string) string {
	q := "'" + strings.ReplaceAll(s, "'", "''") + "'"
	if strings.Contains(s, `\`) {
		return "E" + strings.ReplaceAll(q, `\`, `\\`)
	}
	return q
}

// DescribeTable 은 열마다 이름·타입·NULL 허용·기본값·주석을 행으로, 제약·인덱스를 Text 로 돌려준다.
// 편집기 세션이 아니라 풀을 써서 편집기의 트랜잭션·SET 에 영향받지 않는다.
func (p *PGConn) DescribeTable(ctx context.Context, t TableRef) (*Result, error) {
	m, err := p.loadMeta(ctx, t)
	if err != nil {
		return nil, err
	}
	res := &Result{Cols: []string{"name", "type", "nullable", "default", "comment"}}
	for _, c := range m.cols {
		if len(res.Rows) >= MaxRows {
			res.Truncated = true
			break
		}
		nullable := "YES"
		if c.NotNull {
			nullable = "NO"
		}
		res.Rows = append(res.Rows, []string{c.Name, c.Type, nullable, CapCell(columnDefault(c)), CapCell(c.Comment)})
	}
	var b strings.Builder
	if m.rel.Comment != "" {
		fmt.Fprintf(&b, "comment: %s\n", CapCell(m.rel.Comment))
	}
	if len(m.cons) > 0 {
		b.WriteString("constraints:\n")
		for _, c := range m.cons {
			fmt.Fprintf(&b, "  %s  %s\n", c.Name, c.Def)
		}
	}
	if len(m.idx) > 0 {
		b.WriteString("indexes:\n")
		for _, ix := range m.idx {
			fmt.Fprintf(&b, "  %s  %s\n", ix.Name, ix.Def)
		}
	}
	res.Text = strings.TrimRight(b.String(), "\n")
	return res, nil
}

// TableDDL 은 카탈로그로 CREATE 문을 다시 만든다. 테이블은 열·제약·인덱스·주석, 뷰는 정의와 주석이다.
// 파티션은 PARTITION OF 부모와 범위, 자기만 가진 제약·인덱스·주석이다. 열은 부모에서 물려받는다.
// 단, identity 의 시퀀스 옵션, 열 collation·storage, 파티션에만 준 열 기본값, 트리거·권한·소유자는 넣지 않는다.
func (p *PGConn) TableDDL(ctx context.Context, t TableRef) (string, error) {
	m, err := p.loadMeta(ctx, t)
	if err != nil {
		return "", err
	}
	if relKindWords[m.rel.Kind] == "" {
		return "", fmt.Errorf("DDL not supported for relation kind %q: %s.%s", m.rel.Kind, QuoteIdent(t.Schema), QuoteIdent(t.Table))
	}
	return buildDDL(t, m), nil
}

func (p *PGConn) loadMeta(ctx context.Context, t TableRef) (*tableMeta, error) {
	rows, err := p.pool.Query(ctx, relQuery, t.Schema, t.Table)
	if err != nil {
		return nil, err
	}
	rel, err := pgx.CollectExactlyOneRow(rows, pgx.RowToStructByPos[relInfo])
	if errors.Is(err, pgx.ErrNoRows) {
		return nil, fmt.Errorf("relation %s.%s not found", QuoteIdent(t.Schema), QuoteIdent(t.Table))
	}
	if err != nil {
		return nil, err
	}
	m := &tableMeta{rel: rel}
	if rows, err = p.pool.Query(ctx, colQuery, rel.OID); err != nil {
		return nil, err
	}
	if m.cols, err = pgx.CollectRows(rows, pgx.RowToStructByPos[colInfo]); err != nil {
		return nil, err
	}
	if rows, err = p.pool.Query(ctx, conQuery, rel.OID); err != nil {
		return nil, err
	}
	if m.cons, err = pgx.CollectRows(rows, pgx.RowToStructByPos[conInfo]); err != nil {
		return nil, err
	}
	if rows, err = p.pool.Query(ctx, idxQuery, rel.OID); err != nil {
		return nil, err
	}
	if m.idx, err = pgx.CollectRows(rows, pgx.RowToStructByPos[idxInfo]); err != nil {
		return nil, err
	}
	return m, nil
}

// columnDefault 는 열 정의 뒤에 붙는 기본값 절이다. identity·생성 열은 DEFAULT 대신 GENERATED 절이 된다.
func columnDefault(c colInfo) string {
	switch {
	case c.Identity == "a":
		return "GENERATED ALWAYS AS IDENTITY"
	case c.Identity == "d":
		return "GENERATED BY DEFAULT AS IDENTITY"
	case c.Generated == "s":
		return "GENERATED ALWAYS AS (" + c.Default + ") STORED"
	case c.Generated == "v": // PostgreSQL 18 가상 생성 열
		return "GENERATED ALWAYS AS (" + c.Default + ") VIRTUAL"
	case c.Default != "":
		return "DEFAULT " + c.Default
	}
	return ""
}

// buildDDL 은 m 의 relkind 가 relKindWords 에 있다고 본다(TableDDL 이 거른다).
func buildDDL(t TableRef, m *tableMeta) string {
	name := QuoteIdent(t.Schema) + "." + QuoteIdent(t.Table)
	word := relKindWords[m.rel.Kind]
	var b strings.Builder
	if m.rel.Kind == "v" || m.rel.Kind == "m" {
		// pg_get_viewdef 는 끝에 ';' 를 붙여 돌려준다
		fmt.Fprintf(&b, "CREATE %s %s AS\n%s;\n", word, name, trimStatement(m.rel.ViewDef))
	} else {
		var lines []string
		cols := m.cols
		if m.rel.PartBound != "" {
			cols = nil // 파티션의 열은 부모가 정한다
		}
		for _, c := range cols {
			line := QuoteIdent(c.Name) + " " + c.Type
			if c.NotNull {
				line += " NOT NULL"
			}
			if d := columnDefault(c); d != "" {
				line += " " + d
			}
			lines = append(lines, line)
		}
		for _, c := range m.cons {
			lines = append(lines, "CONSTRAINT "+QuoteIdent(c.Name)+" "+c.Def)
		}
		body := strings.Join(lines, ",\n    ")
		if body != "" {
			body = "\n    " + body + "\n"
		}
		switch {
		case m.rel.PartBound == "":
			fmt.Fprintf(&b, "CREATE TABLE %s (%s)", name, body)
		case body == "":
			fmt.Fprintf(&b, "CREATE TABLE %s PARTITION OF %s.%s %s", name, QuoteIdent(m.rel.ParentSchema), QuoteIdent(m.rel.ParentTable), m.rel.PartBound)
		default:
			fmt.Fprintf(&b, "CREATE TABLE %s PARTITION OF %s.%s (%s) %s", name, QuoteIdent(m.rel.ParentSchema), QuoteIdent(m.rel.ParentTable), body, m.rel.PartBound)
		}
		if m.rel.PartKey != "" {
			b.WriteString(" PARTITION BY " + m.rel.PartKey)
		}
		b.WriteString(";\n")
	}
	for _, ix := range m.idx {
		if !ix.Backing && !ix.Inherited {
			b.WriteString(ix.Def + ";\n")
		}
	}
	if m.rel.Comment != "" {
		fmt.Fprintf(&b, "COMMENT ON %s %s IS %s;\n", word, name, QuoteLiteral(m.rel.Comment))
	}
	for _, c := range m.cols {
		if c.Comment != "" {
			fmt.Fprintf(&b, "COMMENT ON COLUMN %s.%s IS %s;\n", name, QuoteIdent(c.Name), QuoteLiteral(c.Comment))
		}
	}
	return b.String()
}

// EnumLabels 는 enum 타입의 값을 정의 순서(enumsortorder)로 돌려준다(R49). typ 는 Result.Types 에 실린 그대로다.
// 사용자 정의 타입은 Types 에 OID 숫자로 실리고, 이름(스키마 한정 또는 search_path 기준)도 받는다.
// enum 위의 도메인은 한 단계 풀어 본다. enum 이 아니거나 없는 타입이면 빈 목록이다.
func (p *PGConn) EnumLabels(ctx context.Context, typ string) ([]string, error) {
	rows, err := p.pool.Query(ctx, `select e.enumlabel::text from pg_type t
		join pg_enum e on e.enumtypid = case when t.typtype = 'd' then t.typbasetype else t.oid end
		where t.oid = case when $1 ~ '^[0-9]+$' then $1::oid else to_regtype($1)::oid end
		order by e.enumsortorder`, typ)
	if err != nil {
		return nil, err
	}
	return pgx.CollectRows(rows, pgx.RowTo[string])
}
