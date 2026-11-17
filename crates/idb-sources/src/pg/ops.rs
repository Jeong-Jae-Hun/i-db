//! 화면이 쓰는 풀 조회. Go `internal/db` 의 ops.go(활동·잠금·크기·백엔드 신호)·fk.go·define.go·catalog.go·
//! explain.go(EstimateRows)·importcsv.go·backend.go(Databases) 와 같다. 모두 편집기 세션이 아니라 풀에서 돌아
//! 편집기의 트랜잭션·SET 과 섞이지 않는다.

use idb_core::redact::mask_secrets;
use idb_core::result::{MAX_ROWS, QueryResult};
use idb_core::sqlsafe::{
    ExplainOpt, explain_sql, first_sql_word, mask_sql, modify_table_rows, quote_ident, split_statements, trim_statement,
};
use serde::{Deserialize, Serialize};

/// 테이블의 PK 열 이름 배열(순서대로) 하나를 돌려준다. $1 은 따옴표 친 테이블 이름이다
pub(crate) const PRIMARY_KEY_SQL: &str =
    "select coalesce((select array(select a.attname::text from unnest(i.indkey) with ordinality k(n, o)
			join pg_attribute a on a.attrelid = i.indrelid and a.attnum = k.n order by k.o)
		from pg_index i where i.indrelid = $1::regclass and i.indisprimary), '{}')";

use super::edit::text_array;
use super::{Pg, TableRef, to_result};
use crate::Error;

// 쿼리 글은 2048자(바이트가 아니라 글자)까지만 싣는다
const ACTIVITY_QUERY: &str = "select pid, coalesce(usename, '') as usename, coalesce(application_name, '') as application_name,
		coalesce(host(client_addr), '') as client_addr, coalesce(state, '') as state,
		coalesce(wait_event_type || ':' || wait_event, '') as wait,
		date_trunc('second', now() - xact_start) as xact_age,
		date_trunc('second', now() - query_start) as query_age,
		coalesce(left(query, 2048), '') as query
		from pg_stat_activity where pid <> pg_backend_pid()
		order by state = 'active' desc, query_start nulls last";
// pg_blocking_pids 는 잠금을 기다리지 않는 세션에 빈 배열을 돌려주므로 Lock 대기만 훑어 비용을 줄인다
const LOCK_PAIRS_QUERY: &str = "select w.pid as blocked_pid, coalesce(w.usename, '') as blocked_user,
		date_trunc('second', now() - w.query_start) as waiting,
		coalesce((select string_agg(distinct coalesce(l.relation::regclass::text, l.locktype) || ' ' || l.mode, ', ')
			from pg_locks l where l.pid = w.pid and not l.granted), '') as waiting_for,
		b.pid as blocking_pid, coalesce(b.usename, '') as blocking_user, coalesce(b.state, '') as blocking_state,
		coalesce(left(w.query, 2048), '') as blocked_query, coalesce(left(b.query, 2048), '') as blocking_query
		from pg_stat_activity w
		cross join lateral unnest(pg_blocking_pids(w.pid)) as bp(pid)
		join pg_stat_activity b on b.pid = bp.pid
		where w.wait_event_type = 'Lock'
		order by w.query_start, w.pid, b.pid";
const LOCK_MODES_QUERY: &str = "select mode, count(*) filter (where granted), count(*) filter (where not granted)
		from pg_locks group by mode order by count(*) desc, mode";
// reltuples 는 ANALYZE 전이면 -1(PostgreSQL 14+)이라 NULL 로 보여 준다
const TABLE_SIZES_QUERY: &str = r"select n.nspname as schema, c.relname as name, c.relkind::text as kind,
		case when c.reltuples < 0 then null else c.reltuples::bigint end as rows_estimate,
		pg_size_pretty(pg_relation_size(c.oid)) as table_size,
		pg_size_pretty(pg_indexes_size(c.oid)) as index_size,
		pg_size_pretty(coalesce(pg_total_relation_size(nullif(c.reltoastrelid, 0)), 0)) as toast_size,
		pg_size_pretty(pg_total_relation_size(c.oid)) as total_size,
		pg_total_relation_size(c.oid) as total_bytes
		from pg_class c join pg_namespace n on n.oid = c.relnamespace
		where c.relkind in ('r', 'p', 'm') and n.nspname not in ('pg_catalog', 'information_schema')
			and n.nspname not like 'pg\_toast%'
		order by pg_total_relation_size(c.oid) desc, n.nspname, c.relname
		limit 200";
/// 다른 세션의 쿼리 글을 싣는 열. ALTER ROLE … PASSWORD '…' 같은 문장이 평문으로 나오지 않게 가린다
const SECRET_COLS: &[&str] = &["query", "blocked_query", "blocking_query"];

// 열 이름은 conkey·confkey 배열 순서대로 모은다. 파티션의 복제 제약(conparentid)은 같은 FK 라 뺀다
const FK_SELECT: &str = "select con.conname::text, sn.nspname::text, sc.relname::text,
		array(select a.attname::text from unnest(con.conkey) with ordinality k(n, i)
			join pg_attribute a on a.attrelid = con.conrelid and a.attnum = k.n order by k.i),
		rn.nspname::text, rc.relname::text,
		array(select a.attname::text from unnest(con.confkey) with ordinality k(n, i)
			join pg_attribute a on a.attrelid = con.confrelid and a.attnum = k.n order by k.i)
	from pg_constraint con
	join pg_class sc on sc.oid = con.conrelid join pg_namespace sn on sn.oid = sc.relnamespace
	join pg_class rc on rc.oid = con.confrelid join pg_namespace rn on rn.oid = rc.relnamespace
	left join pg_constraint pc on pc.oid = con.conparentid
	where con.contype = 'f' ";
const FK_OUT: &str = "and sn.nspname = $1 and sc.relname = $2 and (pc.oid is null or pc.conrelid <> con.conrelid)
	order by con.conname";
const FK_IN: &str = "and rn.nspname = $1 and rc.relname = $2 and (pc.oid is null or pc.confrelid <> con.confrelid)
	order by sn.nspname, sc.relname, con.conname";

// $1 이 null 이면 current_schemas(false) 순서로 찾고 pg_catalog 를 맨 뒤에 붙인다. 같은 스키마에서는 테이블·뷰를 먼저 본다.
// 집계 함수(prokind 'a')는 pg_get_functiondef 가 거부해 뺀다
const RESOLVE_QUERY: &str = "with s as (
			select nsp, ord from unnest(coalesce($1::text[], current_schemas(false) || array['pg_catalog']::text[]))
				with ordinality as u(nsp, ord))
		select o.kind, n.nspname::text, o.name from (
			select case c.relkind when 'v' then 'view' when 'm' then 'matview' else 'table' end as kind,
				c.relnamespace as ns, c.relname::text as name, 0 as pri
			from pg_class c where c.relname = $2 and c.relkind in ('r', 'p', 'v', 'm', 'f')
			union all
			select 'function', p.pronamespace, p.proname::text, 1
			from pg_proc p where p.proname = $2 and p.prokind in ('f', 'p', 'w')
		) o join pg_namespace n on n.oid = o.ns join s on s.nsp = n.nspname
		order by s.ord, o.pri limit 1";
const FUNCTION_DEF_QUERY: &str = "select string_agg(pg_get_functiondef(p.oid), E'\\n' order by p.oid)
		from pg_proc p join pg_namespace n on n.oid = p.pronamespace
		where n.nspname = $1 and p.proname = $2 and p.prokind in ('f', 'p', 'w')";

// 자동완성 목록은 키 입력마다 훑으므로 큰 DB 에서도 메모리·지연이 늘지 않게 끊는다
const CATALOG_TABLE_LIMIT: usize = 5000;
const CATALOG_COLUMN_LIMIT: usize = 20000;
const CATALOG_FUNCTION_LIMIT: usize = 2000;

// 가져오기는 메모리에 CSV 를 다 만든 뒤 보낸다
const MAX_IMPORT_ROWS: usize = 100_000;
const MAX_IMPORT_BYTES: usize = 50 << 20;

/// 전체 내보내기 조각 크기. 받는 쪽은 조각을 몇 개까지만 쌓아 두어(채널 크기) 메모리가 행 수와 상관없다
pub const EXPORT_CHUNK: usize = 1 << 20;
/// 받는 쪽이 이만큼 읽지 않으면 버려진 내보내기로 보고 연결을 닫는다(풀 연결 하나를 붙잡지 않게)
const EXPORT_IDLE: std::time::Duration = std::time::Duration::from_secs(300);
/// 읽기 문장이지만 COPY (…) 안에 넣을 수 없는 문장(SHOW·EXPLAIN)이다(Go `ErrExportNotSelect`)
pub const ERR_EXPORT_NOT_SELECT: &str = "only select, with, values and table statements can be exported";

/// 정의를 찾지 못한 오류 글의 머리다. 화면은 이 글로 `정의를 찾지 못함: <이름>` 을 보인다.
pub const ERR_NOT_FOUND: &str = "definition not found";

/// 외래 키 하나다. table 의 cols 가 ref 의 ref_cols 를 같은 순서로 가리킨다(R36).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fk {
    pub name: String,
    pub table: TableRef,
    pub cols: Vec<String>,
    #[serde(rename = "ref")]
    pub ref_table: TableRef,
    pub ref_cols: Vec<String>,
}

/// 정의로 이동(R26)이 찾은 객체다. kind 는 table·view·matview·function 이다.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Object {
    pub kind: String,
    pub schema: String,
    pub name: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Catalog {
    pub schemas: Vec<CatalogSchema>,
    pub functions: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogSchema {
    pub name: String,
    pub tables: Vec<CatalogTable>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogTable {
    pub name: String,
    pub view: bool,
    pub columns: Vec<CatalogColumn>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogColumn {
    pub name: String,
    #[serde(rename = "type")]
    pub typ: String,
}

impl Pg {
    /// 풀에서 인자 없는 조회 하나를 표로 받고 쿼리 글 열을 가린다(Go poolResult).
    async fn pool_result(&self, sql: &str) -> Result<QueryResult, Error> {
        let start = std::time::Instant::now();
        let mut p = self.acquire().await?;
        let rows = p.conn().query_cells(sql, &[], MAX_ROWS).await?;
        p.release();
        let mut res = to_result(rows);
        for (c, name) in res.cols.iter().enumerate() {
            if SECRET_COLS.contains(&name.as_str()) {
                for row in &mut res.rows {
                    row[c] = mask_secrets(&row[c]);
                }
            }
        }
        res.elapsed = start.elapsed();
        Ok(res)
    }

    /// 같은 서버에서 접속할 수 있는 데이터베이스 이름이다(R27.1).
    pub async fn databases(&self) -> Result<Vec<String>, Error> {
        let rows = self
            .pool_query(
                "select datname::text from pg_database
		where datallowconn and not datistemplate and has_database_privilege(datname, 'CONNECT') order by 1",
                &[],
            )
            .await?;
        Ok((0..rows.rows.len()).map(|r| rows.text(r, 0)).collect())
    }

    /// enum 타입(또는 enum 위 도메인)의 값들이다. typ 은 타입 이름이나 OID 글이다. enum 이 아니면 빈 목록이다.
    pub async fn enum_labels(&self, typ: &str) -> Result<Vec<String>, Error> {
        let rows = self
            .pool_query(
                "select e.enumlabel::text from pg_type t
		join pg_enum e on e.enumtypid = case when t.typtype = 'd' then t.typbasetype else t.oid end
		where t.oid = case when $1 ~ '^[0-9]+$' then $1::oid else to_regtype($1)::oid end
		order by e.enumsortorder",
                &[Some(typ)],
            )
            .await?;
        Ok((0..rows.rows.len()).map(|r| rows.text(r, 0)).collect())
    }

    /// 테이블의 PK 열(순서대로)이다. 없으면 빈 목록이다. 편집 묶음의 pk 가 진짜 PK 인지 확인하는 데 쓴다.
    pub async fn primary_key(&self, t: &TableRef) -> Result<Vec<String>, Error> {
        let rows = self.pool_query(PRIMARY_KEY_SQL, &[Some(&t.quoted())]).await?;
        Ok(rows.rows.first().map(|_| text_array(&rows.text(0, 0))).unwrap_or_default())
    }

    /// 자기 연결을 뺀 pg_stat_activity 다(R22.1).
    pub async fn activity(&self) -> Result<QueryResult, Error> {
        self.pool_result(ACTIVITY_QUERY).await
    }

    /// 막힌·막는 세션 쌍을 행으로, 잠금 모드별 개수를 text 로 돌려준다.
    pub async fn locks(&self) -> Result<QueryResult, Error> {
        let mut res = self.pool_result(LOCK_PAIRS_QUERY).await?;
        let modes = self.pool_query(LOCK_MODES_QUERY, &[]).await?;
        let parts: Vec<String> = (0..modes.rows.len())
            .map(|r| {
                let waiting: i64 = modes.text(r, 2).parse().unwrap_or(0);
                let mut s = format!("{} {}", modes.text(r, 0), modes.text(r, 1));
                if waiting > 0 {
                    s.push_str(&format!(" (+{waiting} waiting)"));
                }
                s
            })
            .collect();
        res.text = format!("pg_locks: {}", parts.join(", "));
        Ok(res)
    }

    /// 전체 크기가 큰 테이블·파티션 부모·구체화 뷰 200개다.
    pub async fn table_sizes(&self) -> Result<QueryResult, Error> {
        self.pool_result(TABLE_SIZES_QUERY).await
    }

    /// pg_cancel_backend(terminate 면 pg_terminate_backend)다. 함수가 false 면(그런 세션이 없음) 오류다.
    pub async fn signal_backend(&self, pid: i64, terminate: bool) -> Result<(), Error> {
        let f = if terminate { "pg_terminate_backend" } else { "pg_cancel_backend" };
        let pid_text = pid.to_string();
        let rows = self.pool_query(&format!("select {f}($1)"), &[Some(&pid_text)]).await?;
        if rows.rows.is_empty() || !rows.flag(0, 0) {
            return Err(Error::msg(format!(
                "backend {pid} was not signaled (not a PostgreSQL server process?)"
            )));
        }
        Ok(())
    }

    /// t 가 가리키는 FK(incoming 이면 t 를 가리키는 FK)다.
    pub async fn foreign_keys(&self, t: &TableRef, incoming: bool) -> Result<Vec<Fk>, Error> {
        let sql = format!("{FK_SELECT}{}", if incoming { FK_IN } else { FK_OUT });
        let rows = self.pool_query(&sql, &[Some(&t.schema), Some(&t.table)]).await?;
        Ok((0..rows.rows.len())
            .map(|r| Fk {
                name: rows.text(r, 0),
                table: TableRef {
                    schema: rows.text(r, 1),
                    table: rows.text(r, 2),
                },
                cols: text_array(&rows.text(r, 3)),
                ref_table: TableRef {
                    schema: rows.text(r, 4),
                    table: rows.text(r, 5),
                },
                ref_cols: text_array(&rows.text(r, 6)),
            })
            .collect())
    }

    /// 이름을 객체로 찾는다(R26). 맨 이름은 search_path 가 있으면 [그 스키마, public, pg_catalog], 없으면 풀 연결의
    /// current_schemas 순서다. 스키마를 적은 이름은 그 스키마에서만 찾는다.
    pub async fn resolve(&self, name: &str, search_path: &str) -> Result<Object, Error> {
        let parts = split_qualified(name)?;
        let schemas = match parts.len() {
            n if n >= 2 => Some(super::pg_array(&parts[n - 2..n - 1])),
            _ if !search_path.is_empty() => Some(super::pg_array(&[search_path.into(), "public".into(), "pg_catalog".into()])),
            _ => None,
        };
        let rows = self
            .pool_query(RESOLVE_QUERY, &[schemas.as_deref(), Some(&parts[parts.len() - 1])])
            .await?;
        if rows.rows.is_empty() {
            return Err(Error::msg(format!("{ERR_NOT_FOUND}: {name}")));
        }
        Ok(Object {
            kind: rows.text(0, 0),
            schema: rows.text(0, 1),
            name: rows.text(0, 2),
        })
    }

    /// pg_get_functiondef 결과다. 오버로드가 여럿이면 빈 줄로 잇는다.
    pub async fn function_def(&self, schema: &str, name: &str) -> Result<String, Error> {
        let rows = self.pool_query(FUNCTION_DEF_QUERY, &[Some(schema), Some(name)]).await?;
        match rows.rows.first().and_then(|r| r[0].clone()) {
            Some(def) => Ok(def),
            None => Err(Error::msg(format!(
                "{ERR_NOT_FOUND}: {}.{}",
                quote_ident(schema),
                quote_ident(name)
            ))),
        }
    }

    /// 자동완성·찾기(R8·R22.3·R56)에 쓸 스키마·테이블·뷰·열·함수 이름이다. 한도를 넘으면 이름순 뒤쪽이 빠진다.
    pub async fn catalog(&self) -> Result<Catalog, Error> {
        let schemas = self.find_schemas().await?;
        let arr = super::pg_array(&schemas);
        let mut cat = Catalog {
            schemas: schemas
                .iter()
                .map(|s| CatalogSchema {
                    name: s.clone(),
                    tables: Vec::new(),
                })
                .collect(),
            functions: Vec::new(),
        };
        let limit = CATALOG_TABLE_LIMIT.to_string();
        let rows = self
            .pool_query(
                "select n.nspname, c.relname, c.relkind in ('v','m')
		from pg_class c join pg_namespace n on n.oid = c.relnamespace
		where n.nspname = any($1) and c.relkind in ('r','p','v','m','f')
		order by 1, 2 limit $2",
                &[Some(&arr), Some(&limit)],
            )
            .await?;
        let mut at = std::collections::HashMap::new();
        for r in 0..rows.rows.len() {
            let Some(si) = schemas.iter().position(|s| *s == rows.text(r, 0)) else {
                continue;
            };
            let tables = &mut cat.schemas[si].tables;
            at.insert((rows.text(r, 0), rows.text(r, 1)), (si, tables.len()));
            tables.push(CatalogTable {
                name: rows.text(r, 1),
                view: rows.flag(r, 2),
                columns: Vec::new(),
            });
        }
        let limit = CATALOG_COLUMN_LIMIT.to_string();
        let rows = self
            .pool_query(
                "select n.nspname, c.relname, a.attname, format_type(a.atttypid, a.atttypmod)
		from pg_attribute a join pg_class c on c.oid = a.attrelid join pg_namespace n on n.oid = c.relnamespace
		where n.nspname = any($1) and c.relkind in ('r','p','v','m','f') and a.attnum > 0 and not a.attisdropped
		order by n.nspname, c.relname, a.attnum limit $2",
                &[Some(&arr), Some(&limit)],
            )
            .await?;
        for r in 0..rows.rows.len() {
            // 테이블 한도로 빠진 테이블의 열은 버린다
            if let Some(&(si, ti)) = at.get(&(rows.text(r, 0), rows.text(r, 1))) {
                cat.schemas[si].tables[ti].columns.push(CatalogColumn {
                    name: rows.text(r, 2),
                    typ: rows.text(r, 3),
                });
            }
        }
        let limit = CATALOG_FUNCTION_LIMIT.to_string();
        let rows = self
            .pool_query(
                "select distinct p.proname from pg_proc p join pg_namespace n on n.oid = p.pronamespace
		where n.nspname = any($1) order by 1 limit $2",
                &[Some(&arr), Some(&limit)],
            )
            .await?;
        cat.functions = (0..rows.rows.len()).map(|r| rows.text(r, 0)).collect();
        Ok(cat)
    }

    /// UPDATE·DELETE 하나가 바꿀 행 수의 플래너 추정치다(R39). EXPLAIN 만 하고 실행하지 않는다.
    /// 풀 연결의 읽기 전용 트랜잭션에서 돌리고, 고른 search_path 는 SET LOCAL 로 맞춘다.
    pub async fn estimate_rows(&self, sql: &str, search_path: &str) -> Result<i64, Error> {
        let stmts = split_statements(sql);
        if stmts.len() != 1 {
            return Err(Error::msg("estimate needs exactly one statement"));
        }
        let stmt = trim_statement(stmts[0]);
        if !matches!(first_sql_word(stmt).as_str(), "update" | "delete") {
            return Err(Error::msg("estimate supports only UPDATE or DELETE"));
        }
        // 실행으로 이어질 수 있는 단어는 아예 받지 않는다(PostgreSQL 은 ANALYSE 철자도 받는다)
        let masked = mask_sql(stmt).to_ascii_lowercase();
        if masked
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .any(|w| w == "analyze" || w == "analyse")
        {
            return Err(Error::msg("statement must not contain ANALYZE"));
        }
        let mut p = self.acquire().await?;
        let c = p.conn();
        c.exec("BEGIN READ ONLY").await?;
        let r = async {
            if !search_path.is_empty() {
                c.exec(&format!("SET LOCAL search_path TO {}, public", quote_ident(search_path)))
                    .await
                    .map_err(|e| Error::msg(format!("set search_path: {e}")))?;
            }
            let opt = ExplainOpt {
                format: "JSON".into(),
                ..Default::default()
            };
            let rows = c.query(&explain_sql(stmt, &opt), &[], 1).await?;
            let raw = rows.rows.first().and_then(|r| r[0].clone()).unwrap_or_default();
            modify_table_rows(raw.as_bytes()).map_err(Error::msg)
        }
        .await;
        let _ = c.exec("ROLLBACK").await;
        p.release();
        r
    }

    /// 행을 COPY 한 번으로 넣는다(CSV 가져오기). 중간에 실패하면 한 행도 남지 않는다. None 은 NULL 이다.
    pub async fn import_csv(&self, t: &TableRef, columns: &[String], rows: &[Vec<Option<String>>]) -> Result<i64, Error> {
        if columns.is_empty() {
            return Err(Error::msg("import needs at least one column"));
        }
        if rows.len() > MAX_IMPORT_ROWS {
            return Err(Error::msg(format!("import has {} rows, limit is {MAX_IMPORT_ROWS}", rows.len())));
        }
        // CSV 에서 따옴표 없는 빈 칸만 NULL 이다. 값은 늘 따옴표로 감싸 빈 글·"\N" 이 NULL 이 되지 않게 한다
        let mut buf = String::new();
        for (i, row) in rows.iter().enumerate() {
            if row.len() != columns.len() {
                return Err(Error::msg(format!(
                    "row {} has {} values, expected {}",
                    i + 1,
                    row.len(),
                    columns.len()
                )));
            }
            for (j, v) in row.iter().enumerate() {
                if j > 0 {
                    buf.push(',');
                }
                if let Some(v) = v {
                    buf.push_str(&quote_ident(v)); // CSV 따옴표 규칙(" 를 "" 로)이 식별자 따옴표 규칙과 같다
                }
            }
            buf.push('\n');
            if buf.len() > MAX_IMPORT_BYTES {
                return Err(Error::msg(format!("import data exceeds {} MiB", MAX_IMPORT_BYTES >> 20)));
            }
        }
        let cols: Vec<String> = columns.iter().map(|c| quote_ident(c)).collect();
        let sql = format!("COPY {} ({}) FROM STDIN (FORMAT csv)", t.quoted(), cols.join(", "));
        let mut p = self.acquire().await?;
        let tag = match p.conn().copy_in(&sql, buf.as_bytes()).await {
            Ok(tag) => tag,
            // COPY 는 자동 커밋이라 응답을 받기 전에 끊기면 이미 들어갔을 수 있다
            Err(e @ Error::Pg(_)) => return Err(e),
            Err(e) => return Err(Error::msg(format!("import outcome unknown: {e}; verify by re-querying"))),
        };
        p.release();
        Ok(tag.rsplit(' ').next().and_then(|n| n.parse().ok()).unwrap_or(0))
    }

    /// 읽기 문장 하나를 1,000행 제한 없이 `COPY (…) TO STDOUT WITH (FORMAT csv, HEADER)` 로 받아 out 에 조각으로 보낸다(R33.1).
    /// 돌려주는 값은 COPY 가 보낸 행 수다. Go `PGConn.CopyOut` 과 같다: 편집기 세션을 오래 붙잡지 않도록 풀 연결의
    /// READ ONLY 트랜잭션에서 돌고, 첫 단어 판정이 놓치는 CTE 안 쓰기는 서버의 READ ONLY 가 막는다. 고른 search_path 는
    /// 트랜잭션 안의 SET 이라 ROLLBACK 이 되돌린다(R27.2). 중간에 멈추면(out 닫힘·실패) 연결은 풀에 돌아가지 않고 닫힌다.
    pub async fn copy_out(&self, sql: &str, search_path: &str, out: &tokio::sync::mpsc::Sender<Vec<u8>>) -> Result<i64, Error> {
        let stmt = trim_statement(sql);
        match split_statements(stmt).len() {
            0 => return Err(Error::msg("empty statement")),
            1 => {}
            _ => return Err(Error::msg("only one statement can be exported")),
        }
        if !idb_core::sqlsafe::is_read_only_sql(stmt) {
            return Err(Error::msg(idb_core::sqlsafe::ReadOnlyError::NotReadOnly.to_string()));
        }
        if !matches!(first_sql_word(stmt).as_str(), "select" | "with" | "values" | "table") {
            return Err(Error::msg(ERR_EXPORT_NOT_SELECT));
        }
        // simple protocol 이라 문장을 감싸는 괄호가 어긋나면 ") TO ... (" 사이로 다른 COPY 옵션을 끼울 수 있다
        let mut depth = 0i64;
        for b in mask_sql(stmt).bytes() {
            match b {
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ => {}
            }
            if depth < 0 {
                break;
            }
        }
        if depth != 0 {
            return Err(Error::msg("unbalanced parentheses"));
        }
        let mut p = self.acquire().await?;
        let c = p.conn();
        c.exec("BEGIN READ ONLY").await?;
        if !search_path.is_empty() {
            c.exec(&format!("SET LOCAL search_path TO {}, public", quote_ident(search_path)))
                .await?;
        }
        // 문장이 줄 주석으로 끝나도 닫는 괄호가 주석에 먹히지 않게 줄을 바꾼다
        let copy = format!("COPY ({stmt}\n) TO STDOUT WITH (FORMAT csv, HEADER)");
        let tag = c.copy_out(&copy, EXPORT_CHUNK, out, EXPORT_IDLE).await?;
        c.exec("ROLLBACK").await?;
        p.release();
        Ok(tag.rsplit(' ').next().and_then(|n| n.parse().ok()).unwrap_or(0))
    }
}

/// `a."B c".d` 를 [a, B c, d] 로 나눈다. 따옴표 밖 ASCII 는 소문자로 접는다.
fn split_qualified(name: &str) -> Result<Vec<String>, Error> {
    let not_found = |why: &str| Error::msg(format!("{ERR_NOT_FOUND}: {name}{why}"));
    let s = name.trim().as_bytes();
    let (mut parts, mut cur, mut quoted) = (Vec::new(), Vec::new(), false);
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if c == b'"' {
            let mut j = i + 1;
            while j < s.len() {
                if s[j] == b'"' {
                    if j + 1 < s.len() && s[j + 1] == b'"' {
                        j += 2;
                        continue;
                    }
                    break;
                }
                j += 1;
            }
            if j >= s.len() {
                return Err(not_found(" (unterminated quote)"));
            }
            cur.extend(String::from_utf8_lossy(&s[i + 1..j]).replace("\"\"", "\"").bytes());
            quoted = true;
            i = j;
        } else if c == b'.' {
            parts.push(String::from_utf8_lossy(&cur).into_owned());
            cur.clear();
            quoted = false;
        } else if quoted {
            return Err(not_found(""));
        } else {
            cur.push(c.to_ascii_lowercase());
        }
        i += 1;
    }
    parts.push(String::from_utf8_lossy(&cur).into_owned());
    if parts.iter().any(String::is_empty) {
        return Err(not_found(""));
    }
    Ok(parts)
}

/// 테이블 보기 WHERE 칸에 넣을 `"c1" = 'v1' AND "c2" = 'v2'` 다(R36). 값은 문자열 리터럴이고 형 변환은 서버가 한다.
pub fn fk_where(cols: &[String], vals: &[String]) -> Result<String, String> {
    if cols.len() != vals.len() || cols.is_empty() {
        return Err(format!("foreign key has {} columns but {} values", cols.len(), vals.len()));
    }
    Ok(cols
        .iter()
        .zip(vals)
        .map(|(c, v)| format!("{} = {}", quote_ident(c), idb_core::sqlsafe::quote_literal(v)))
        .collect::<Vec<_>>()
        .join(" AND "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn 전체_내보내기는_접속하기_전에_문장을_거른다() {
        // 열지 않은 풀이라 검사를 지나면 접속 오류가 난다
        let pg = Pg::unopened("127.0.0.1", 1);
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let err = |sql: &'static str| {
            let (pg, tx) = (pg.clone(), tx.clone());
            async move { pg.copy_out(sql, "", &tx).await.unwrap_err().to_string() }
        };
        assert_eq!(err(" ; ").await, "empty statement");
        assert_eq!(err("select 1; select 2").await, "only one statement can be exported");
        assert!(err("delete from t").await.starts_with("only read-only statements"));
        assert_eq!(err("show search_path").await, ERR_EXPORT_NOT_SELECT);
        assert_eq!(err("select 1) to stdout (format text").await, "unbalanced parentheses");
        assert_eq!(err("select (1").await, "unbalanced parentheses");
        assert!(err("select ')' from t;").await.starts_with("failed to connect"));
    }

    #[test]
    fn 이름은_따옴표를_풀고_밖은_소문자로_접는다() {
        assert_eq!(split_qualified(r#"a."B c".D"#).unwrap(), ["a", "B c", "d"]);
        assert_eq!(split_qualified(r#""x""y""#).unwrap(), [r#"x"y"#]);
        assert!(
            split_qualified(r#""open"#)
                .unwrap_err()
                .to_string()
                .ends_with("(unterminated quote)")
        );
        assert!(split_qualified("a..b").is_err());
        assert!(split_qualified(r#""a"b"#).is_err());
    }

    #[test]
    fn 외래_키_조건은_열마다_리터럴을_AND_로_잇는다() {
        let c = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            fk_where(&c(&["a", "B"]), &c(&["1", "it's"])).unwrap(),
            r#""a" = '1' AND "B" = 'it''s'"#
        );
        assert!(fk_where(&c(&["a"]), &[]).is_err());
    }
}
