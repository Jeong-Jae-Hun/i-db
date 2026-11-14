package complete

import (
	"slices"
	"strings"
)

const (
	clauseNone   clause = iota // 식별자로 본다(별칭이 될 수 있다)
	clauseTable                // 뒤에 테이블 이름이 온다
	clauseColumn               // 뒤에 열·식이 온다
	clauseStop                 // 뒤에 특정 키워드·값이 온다. 키워드만 제안한다
	clauseExpr                 // 식 안에 끼는 낱말. 문맥을 정하지 않고 더 앞을 본다
)

const (
	ctxKeyword sqlCtx = iota
	ctxTable
	ctxColumn
	ctxInsertColumns
)

// clauseOf 는 커서 앞을 거슬러 올라가며 문맥을 정할 때와 별칭 여부를 가릴 때 쓴다.
// 따옴표를 씌울지는 reservedWords 로 따로 정한다. 여기에는 update·set 처럼 이름으로 써도 되는 낱말도 있다.
var clauseOf = func() map[string]clause {
	m := map[string]clause{}
	for c, words := range map[clause]string{
		clauseTable:  `from join update into table truncate`,
		clauseColumn: `select where on by set returning having and or when then else using`,
		clauseStop: `limit offset order group partition left right inner outer full cross natural insert delete
			create alter drop values fetch window union intersect except with default for do grant revoke
			begin commit rollback explain analyze analyse vacuum copy comment`,
		clauseExpr: `as not in is null like ilike similar between distinct asc desc case end exists any all some
			true false user current_user session_user current_date current_time current_timestamp localtime
			localtimestamp only lateral array cast collate check column constraint primary references unique
			foreign both leading trailing symmetric asymmetric variadic nulls first last`,
	} {
		for _, w := range strings.Fields(words) {
			m[w] = c
		}
	}
	return m
}()

// reservedWords 는 따옴표 없이는 열·테이블 이름이 될 수 없는 낱말이다. quoteIdent 만 쓴다.
// PostgreSQL pg_get_keywords() 의 catcode 'R'(예약어)과 'T'(함수·타입 이름으로만 쓰는 낱말)를 옮겼다.
// 통합 테스트가 접속한 서버의 목록을 다 담는지 확인한다.
var reservedWords = strings.Fields(`
	all analyse analyze and any array as asc asymmetric both case cast check collate column constraint create
	current_catalog current_date current_role current_time current_timestamp current_user default deferrable desc
	distinct do else end except false fetch for foreign from grant group having in initially intersect into lateral
	leading limit localtime localtimestamp not null offset on only or order placing primary references returning
	select session_user some symmetric system_user table then to trailing true union unique user using variadic
	when where window with
	authorization binary collation concurrently cross current_schema freeze full ilike inner is isnull join left
	like natural notnull outer overlaps right similar tablesample verbose
`)

var sqlKeywords = strings.Fields(`
	SELECT FROM WHERE AND OR NOT IN IS NULL LIKE ILIKE BETWEEN EXISTS AS ON JOIN LEFT RIGHT INNER OUTER FULL CROSS
	NATURAL LATERAL USING GROUP BY ORDER HAVING LIMIT OFFSET FETCH FIRST NEXT ROWS ONLY DISTINCT ALL ANY SOME UNION
	INTERSECT EXCEPT CASE WHEN THEN ELSE END ASC DESC NULLS LAST INSERT INTO VALUES DEFAULT UPDATE SET DELETE
	RETURNING WITH RECURSIVE MATERIALIZED CONFLICT DO NOTHING TRUNCATE CREATE ALTER DROP TABLE VIEW INDEX UNIQUE
	SEQUENCE SCHEMA DATABASE FUNCTION PROCEDURE TRIGGER TYPE DOMAIN EXTENSION ROLE USER GRANT REVOKE PRIVILEGES
	CONCURRENTLY IF CASCADE RESTRICT CONSTRAINT PRIMARY KEY FOREIGN REFERENCES CHECK COLUMN ADD RENAME TO OWNER
	TEMPORARY TEMP UNLOGGED BEGIN COMMIT ROLLBACK SAVEPOINT RELEASE TRANSACTION ISOLATION LEVEL READ WRITE
	COMMITTED SERIALIZABLE REPEATABLE EXPLAIN ANALYZE VERBOSE BUFFERS COSTS FORMAT VACUUM REINDEX CLUSTER COPY
	SHOW RESET LOCK FOR SHARE NOWAIT SKIP LOCKED WINDOW OVER PARTITION RANGE PRECEDING FOLLOWING UNBOUNDED CURRENT
	ROW FILTER WITHIN TRUE FALSE CAST COLLATE ARRAY SIMILAR ESCAPE INTERVAL TIMESTAMP DATE TIME ZONE AT LOCAL
	CURRENT_DATE CURRENT_TIMESTAMP CURRENT_USER INTEGER BIGINT SMALLINT NUMERIC DECIMAL REAL DOUBLE PRECISION
	BOOLEAN TEXT VARCHAR CHAR UUID JSON JSONB BYTEA SERIAL BIGSERIAL COMMENT NOTIFY LISTEN PREPARE EXECUTE
	DEALLOCATE REFRESH TABLESAMPLE GENERATED ALWAYS IDENTITY STORED OVERRIDING SYSTEM VALUE
`)

// nextKeywords 는 앞 키워드 바로 뒤에 거의 늘 오는 키워드다. 키워드 문맥에서 맨 앞에 둔다.
var nextKeywords = map[string]string{
	"order": "BY", "group": "BY", "partition": "BY", "left": "JOIN OUTER", "right": "JOIN OUTER",
	"full": "JOIN OUTER", "outer": "JOIN", "inner": "JOIN", "cross": "JOIN", "natural": "JOIN", "insert": "INTO",
	"delete": "FROM", "is": "NULL NOT DISTINCT", "union": "ALL SELECT", "create": "TABLE VIEW INDEX SCHEMA FUNCTION",
	"drop": "TABLE VIEW INDEX SCHEMA FUNCTION", "alter": "TABLE VIEW INDEX SCHEMA FUNCTION", "nulls": "FIRST LAST",
	"explain": "ANALYZE SELECT", "do": "NOTHING UPDATE", "primary": "KEY", "foreign": "KEY",
}

var sqlFunctions = strings.Fields(`
	count sum avg min max coalesce nullif greatest least now lower upper length trim substring replace concat
	concat_ws split_part position left right lpad rpad regexp_replace regexp_match to_char to_date to_timestamp
	to_number date_trunc date_part extract age abs round ceil floor mod random gen_random_uuid string_agg
	array_agg array_length unnest json_agg jsonb_agg json_build_object jsonb_build_object jsonb_set
	jsonb_array_elements jsonb_each to_jsonb row_number rank dense_rank lag lead first_value last_value
	generate_series pg_sleep
`)

type clause uint8

type sqlCtx uint8

type tableRef struct {
	schema, name, alias string
}

// SuggestSQL 은 커서가 있는 문장과 그 앞 낱말로 문맥을 정해 후보를 돌려준다.
// cat 이 nil 이면 키워드·함수만 제안한다. 문자열·주석 안에서는 아무것도 제안하지 않는다.
func SuggestSQL(cat *Catalog, q Query) []Item {
	text, off := q.Text, clampOffset(q)

	// 커서가 있는 문장의 토큰만 남긴다. 경계는 문자열·주석 밖의 ';' 다
	var stmt []token
	for _, t := range tokenize(text) {
		if t.start >= off && t.isPunct(';') {
			break
		}
		if (t.kind == tokString || t.kind == tokComment) && t.start < off && (off < t.end || off == t.end && t.open) {
			return nil
		}
		switch {
		case t.kind == tokComment:
		case t.isPunct(';'):
			stmt = nil
		default:
			stmt = append(stmt, t)
		}
	}

	// before 는 커서 앞 토큰이다. 커서에 걸친 단어는 접두어로 떼어 낸다
	n := 0
	for n < len(stmt) && stmt[n].start < off {
		n++
	}
	before, prefix, prefixStart := stmt[:n], "", off
	if n > 0 && stmt[n-1].end >= off {
		last := stmt[n-1]
		switch {
		case isIdent(last):
			before, prefixStart = stmt[:n-1], last.start
			prefix = identName(text, token{kind: last.kind, start: last.start, end: off, open: true})
		case last.kind == tokNumber || last.kind == tokString:
			return nil
		}
	}
	m := len(before)
	w := typed{name: prefix, size: off - prefixStart}

	// 커서를 감싼 괄호(서브쿼리) 안의 테이블을 먼저 둔다. 바깥 테이블은 상관 서브쿼리용으로 뒤에 붙인다
	lo, hi, depth := 0, len(stmt), 0
	for i := m - 1; i >= 0 && lo == 0; i-- {
		switch {
		case before[i].isPunct(')'):
			depth++
		case before[i].isPunct('(') && depth > 0:
			depth--
		case before[i].isPunct('('):
			lo = i + 1
		}
	}
	for i, d := n, 0; lo > 0 && i < len(stmt) && hi == len(stmt); i++ {
		switch {
		case stmt[i].isPunct('('):
			d++
		case stmt[i].isPunct(')') && d > 0:
			d--
		case stmt[i].isPunct(')'):
			hi = i
		}
	}
	refs := tableRefs(text, stmt[lo:hi])
	if lo > 0 {
		refs = append(refs, tableRefs(text, stmt)...)
	}

	// 한정자: "a.", "스키마.테이블."
	if m >= 2 && before[m-1].isPunct('.') && before[m-1].end == prefixStart && isIdent(before[m-2]) && before[m-2].end == before[m-1].start {
		qual := tableRef{name: identName(text, before[m-2])}
		if m >= 4 && before[m-3].isPunct('.') && isIdent(before[m-4]) && before[m-4].end == before[m-3].start && before[m-3].end == before[m-2].start {
			qual.schema = identName(text, before[m-4])
			return rank(columnItems(cat.table(qual)), w)
		}
		for _, r := range refs {
			// "from it." 처럼 치는 중인 참조는 스키마 이름이 테이블로 잡히므로 찾은 테이블만 쓴다
			if t := cat.table(r); t != nil && (strings.EqualFold(r.alias, qual.name) || r.alias == "" && strings.EqualFold(r.name, qual.name)) {
				return rank(columnItems(t), w)
			}
		}
		for si := 0; cat != nil && si < len(cat.Schemas); si++ {
			if s := &cat.Schemas[si]; strings.EqualFold(s.Name, qual.name) {
				var items []Item
				for i := range s.Tables {
					items = append(items, tableItem(&s.Tables[i], ""))
				}
				return rank(items, w)
			}
		}
		return rank(columnItems(cat.table(qual)), w)
	}

	if m > 0 && keywordOf(text, before[m-1]) == "as" {
		return nil // 별칭은 새 이름이라 제안할 것이 없다
	}

	// 커서 앞을 거슬러 올라가 같은 괄호 깊이에서 처음 만나는 절 키워드로 문맥을 정한다
	ctx := ctxKeyword
	depth = 0
	var insertInto *Table
walk:
	for i := m - 1; i >= 0; i-- {
		t := before[i]
		switch {
		case t.isPunct(')'):
			depth++
		case t.isPunct('('):
			if depth > 0 {
				depth--
				continue
			}
			// insert into [스키마.]테이블 ( ← 열 목록
			j, ref := i-1, tableRef{}
			if j >= 0 && isIdent(before[j]) {
				ref.name = identName(text, before[j])
				if j >= 2 && before[j-1].isPunct('.') && isIdent(before[j-2]) {
					ref.schema, j = identName(text, before[j-2]), j-2
				}
				if j >= 1 && keywordOf(text, before[j-1]) == "into" {
					ctx, insertInto = ctxInsertColumns, cat.table(ref)
					break walk
				}
			}
		case depth == 0:
			switch kw := keywordOf(text, t); clauseOf[kw] {
			case clauseTable:
				if i == m-1 || kw == "from" && before[m-1].isPunct(',') {
					ctx = ctxTable
				}
				break walk
			case clauseColumn:
				ctx = ctxColumn
				break walk
			case clauseStop:
				break walk
			}
		}
	}

	var items []Item
	switch ctx {
	case ctxInsertColumns:
		return rank(columnItems(insertInto), w)
	case ctxTable:
		if cat != nil {
			// public 은 search_path 기본값이라 스키마 없이, 나머지는 스키마를 붙여 넣는다
			for _, s := range cat.Schemas {
				for i := range s.Tables {
					if s.Name == "public" {
						items = append(items, tableItem(&s.Tables[i], ""))
					}
				}
			}
			for _, s := range cat.Schemas {
				for i := range s.Tables {
					if s.Name != "public" {
						items = append(items, tableItem(&s.Tables[i], s.Name))
					}
				}
			}
			for _, s := range cat.Schemas {
				items = append(items, Item{Label: s.Name, Kind: KindSchema, Insert: quoteIdent(s.Name)})
			}
		}
		return rank(items, w)
	case ctxColumn:
		for _, r := range refs {
			items = append(items, columnItems(cat.table(r))...)
		}
		for _, r := range refs {
			if r.alias != "" {
				items = append(items, Item{Label: r.alias, Kind: KindTable, Detail: r.name, Insert: quoteIdent(r.alias)})
			}
		}
		if cat != nil {
			for _, f := range cat.Functions {
				items = append(items, Item{Label: f, Kind: KindFunction, Insert: quoteIdent(f) + "("})
			}
		}
		for _, f := range sqlFunctions {
			items = append(items, Item{Label: f, Kind: KindFunction, Insert: f + "("})
		}
	}
	keywords := sqlKeywords
	if ctx == ctxKeyword && m > 0 {
		keywords = append(strings.Fields(nextKeywords[keywordOf(text, before[m-1])]), sqlKeywords...)
	}
	for _, k := range keywords {
		items = append(items, Item{Label: k, Kind: KindKeyword, Insert: caseLike(k, prefix)})
	}
	return rank(items, w)
}

// tableRefs 는 문장 안 FROM·JOIN·UPDATE·INTO 뒤의 테이블과 별칭을 모은다.
// 커서 뒤 FROM 도 본다. "select | from users u" 에서 users 열을 제안하려면 필요하다.
func tableRefs(text string, toks []token) []tableRef {
	var refs []tableRef
	for i, t := range toks {
		kw := keywordOf(text, t)
		if clauseOf[kw] != clauseTable {
			continue
		}
		for j := i + 1; j < len(toks) && isIdent(toks[j]) && clauseOf[keywordOf(text, toks[j])] == clauseNone; j++ {
			ref := tableRef{name: identName(text, toks[j])}
			if j+2 < len(toks) && toks[j+1].isPunct('.') && isIdent(toks[j+2]) {
				ref.schema, ref.name, j = ref.name, identName(text, toks[j+2]), j+2
			}
			switch {
			case j+2 < len(toks) && keywordOf(text, toks[j+1]) == "as" && isIdent(toks[j+2]):
				ref.alias, j = identName(text, toks[j+2]), j+2
			case j+1 < len(toks) && isIdent(toks[j+1]) && clauseOf[keywordOf(text, toks[j+1])] == clauseNone:
				ref.alias, j = identName(text, toks[j+1]), j+1
			}
			refs = append(refs, ref)
			// FROM a, b 처럼 쉼표로 이어지는 목록은 FROM 에만 있다
			if kw != "from" || j+1 >= len(toks) || !toks[j+1].isPunct(',') {
				break
			}
			j++
		}
	}
	return refs
}

// table 은 이름이 같은 테이블을 찾는다. 스키마가 없으면 public 을 먼저, 없으면 처음 찾은 것을 쓴다.
func (c *Catalog) table(ref tableRef) *Table {
	if c == nil {
		return nil
	}
	var found *Table
	for i := range c.Schemas {
		s := &c.Schemas[i]
		if ref.schema != "" && !strings.EqualFold(s.Name, ref.schema) {
			continue
		}
		for j := range s.Tables {
			if !strings.EqualFold(s.Tables[j].Name, ref.name) {
				continue
			}
			if ref.schema != "" || s.Name == "public" {
				return &s.Tables[j]
			}
			if found == nil {
				found = &s.Tables[j]
			}
		}
	}
	return found
}

func columnItems(t *Table) []Item {
	if t == nil {
		return nil
	}
	items := make([]Item, 0, len(t.Columns))
	for _, c := range t.Columns {
		items = append(items, Item{Label: c.Name, Kind: KindColumn, Detail: c.Type, Insert: quoteIdent(c.Name)})
	}
	return items
}

// tableItem 의 schema 가 비어 있지 않으면 "스키마.테이블" 로 넣는다.
func tableItem(t *Table, schema string) Item {
	it := Item{Label: t.Name, Kind: KindTable, Insert: quoteIdent(t.Name)}
	if t.View {
		it.Kind = KindView
	}
	if schema != "" {
		it.Label, it.Detail, it.Insert = schema+"."+t.Name, schema, quoteIdent(schema)+"."+it.Insert
	}
	return it
}

// quoteIdent 는 PostgreSQL 이 소문자로 접지 않고 그대로 읽도록 필요할 때만 따옴표를 씌운다.
func quoteIdent(name string) string {
	plain := name != "" && !isDigit(name[0]) && !slices.Contains(reservedWords, name)
	for i := 0; plain && i < len(name); i++ {
		c := name[i]
		plain = c == '_' || c == '$' && i > 0 || isDigit(c) || c >= 'a' && c <= 'z'
	}
	if plain {
		return name
	}
	return `"` + strings.ReplaceAll(name, `"`, `""`) + `"`
}
