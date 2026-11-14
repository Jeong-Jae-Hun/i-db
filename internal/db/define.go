package db

import (
	"context"
	"errors"
	"fmt"
	"strings"

	"github.com/jackc/pgx/v5"
)

// 정의로 이동(명세 R26)이 찾는 객체 종류. 외부 테이블은 table 로 본다. resolveQuery 가 이 값을 그대로 돌려준다.
const (
	ObjTable    = "table"
	ObjView     = "view"
	ObjMatView  = "matview"
	ObjFunction = "function"
)

// ErrNotFound 는 이름에 맞는 객체가 없을 때다. 화면은 errors.Is 로 가려 `정의를 찾지 못함: <이름>` 을 보인다.
var ErrNotFound = errors.New("definition not found")

const (
	// $1 이 null 이면 풀 연결의 current_schemas(false) 순서로 찾고 pg_catalog 를 맨 뒤에 붙여
	// 사용자 스키마에 같은 이름이 없을 때만 고른다. $1 을 주면 그 목록만 본다(pg_catalog 는 부르는 쪽이 붙인다).
	// 같은 스키마에서는 테이블·뷰를 함수보다 먼저 본다. 집계 함수(prokind 'a')는 pg_get_functiondef 가 거부해 뺀다.
	resolveQuery = `with s as (
			select nsp, ord from unnest(coalesce($1::text[], current_schemas(false) || array['pg_catalog']::text[]))
				with ordinality as u(nsp, ord))
		select o.kind, n.nspname::text, o.name from (
			select case c.relkind when 'v' then '` + ObjView + `' when 'm' then '` + ObjMatView + `' else '` + ObjTable + `' end as kind,
				c.relnamespace as ns, c.relname::text as name, 0 as pri
			from pg_class c where c.relname = $2 and c.relkind in ('r', 'p', 'v', 'm', 'f')
			union all
			select '` + ObjFunction + `', p.pronamespace, p.proname::text, 1
			from pg_proc p where p.proname = $2 and p.prokind in ('f', 'p', 'w')
		) o join pg_namespace n on n.oid = o.ns join s on s.nsp = n.nspname
		order by s.ord, o.pri limit 1`
	// pg_get_functiondef 는 끝에 줄바꿈을 붙여 돌려주므로 줄바꿈 하나로 이으면 정의 사이가 빈 줄이 된다
	functionDefQuery = `select string_agg(pg_get_functiondef(p.oid), E'\n' order by p.oid)
		from pg_proc p join pg_namespace n on n.oid = p.pronamespace
		where n.nspname = $1 and p.proname = $2 and p.prokind in ('f', 'p', 'w')`
)

type Object struct {
	Kind         string // ObjTable·ObjView·ObjMatView·ObjFunction
	Schema, Name string
}

// Resolve 는 편집기 커서 아래 이름을 객체로 찾는다. name 은 `schema.name`, `"따옴표 이름"`, 맨 이름이다.
// 맨 이름은 SetSearchPath 로 고른 스키마가 있으면 [그 스키마, public], 없으면 풀 연결의 search_path 순서로 찾고
// 마지막에 pg_catalog 를 본다. 스키마를 적은 이름은 그 스키마에서만 찾는다(public.now 는 pg_catalog.now 가 아니다).
// 따옴표 없는 부분은 PostgreSQL 처럼 소문자로 접는다. 세 부분(db.schema.name)이면 앞은 버린다.
func (p *PGConn) Resolve(ctx context.Context, name string) (Object, error) {
	parts, err := splitQualified(name)
	if err != nil {
		return Object{}, err
	}
	var schemas []string // nil 이면 SQL 쪽이 current_schemas 를 쓴다
	switch {
	case len(parts) >= 2:
		schemas = parts[len(parts)-2 : len(parts)-1]
	case p.SearchPath() != "":
		schemas = []string{p.SearchPath(), "public", "pg_catalog"}
	}
	var o Object
	err = p.pool.QueryRow(ctx, resolveQuery, schemas, parts[len(parts)-1]).Scan(&o.Kind, &o.Schema, &o.Name)
	if errors.Is(err, pgx.ErrNoRows) {
		return Object{}, fmt.Errorf("%w: %s", ErrNotFound, name)
	}
	return o, err
}

// FunctionDef 는 pg_get_functiondef 결과다. 오버로드가 여럿이면 정의를 빈 줄로 잇는다.
func (p *PGConn) FunctionDef(ctx context.Context, schema, name string) (string, error) {
	var def *string
	if err := p.pool.QueryRow(ctx, functionDefQuery, schema, name).Scan(&def); err != nil {
		return "", err
	}
	if def == nil {
		return "", fmt.Errorf("%w: %s.%s", ErrNotFound, QuoteIdent(schema), QuoteIdent(name))
	}
	return *def, nil
}

// splitQualified 는 `a."B c".d` 를 [a, B c, d] 로 나눈다. 따옴표 밖 ASCII 는 소문자로 접고 앞뒤 공백을 뗀다.
func splitQualified(name string) ([]string, error) {
	var parts []string
	var cur strings.Builder
	quoted := false // 지금 부분이 따옴표로 시작했다
	s := strings.TrimSpace(name)
	for i := 0; i < len(s); i++ {
		switch c := s[i]; {
		case c == '"':
			j := i + 1
			for ; j < len(s); j++ {
				if s[j] != '"' {
					continue
				}
				if j+1 < len(s) && s[j+1] == '"' {
					j++
					continue
				}
				break
			}
			if j >= len(s) {
				return nil, fmt.Errorf("%w: %s (unterminated quote)", ErrNotFound, name)
			}
			cur.WriteString(strings.ReplaceAll(s[i+1:j], `""`, `"`))
			quoted = true
			i = j
		case c == '.':
			parts = append(parts, cur.String())
			cur.Reset()
			quoted = false
		case quoted:
			return nil, fmt.Errorf("%w: %s", ErrNotFound, name)
		case c >= 'A' && c <= 'Z':
			cur.WriteByte(c + 'a' - 'A') // PostgreSQL 은 UTF-8 에서 ASCII 만 소문자로 접는다
		default:
			cur.WriteByte(c)
		}
	}
	parts = append(parts, cur.String())
	for _, part := range parts {
		if part == "" {
			return nil, fmt.Errorf("%w: %s", ErrNotFound, name)
		}
	}
	return parts, nil
}
