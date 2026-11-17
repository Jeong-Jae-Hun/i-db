//! 테이블 하나의 열·제약·인덱스. Go `internal/db/meta.go` 와 같다.
//! 카탈로그 조회는 테이블 하나 범위라 크기가 정해져 있다(열은 PostgreSQL 이 1,600개로 막는다). 표에는 MAX_ROWS 까지만 싣는다(E63).

use idb_core::result::{MAX_ROWS, QueryResult, cap_cell};
use idb_core::sqlsafe::{quote_ident, quote_literal, trim_statement};

use super::wire::Conn;
use crate::Error;
use crate::schemadiff::column_default;

// 파티션은 부모가 pg_inherits 에 하나뿐이다. relpartbound 는 "FOR VALUES ..." 나 "DEFAULT" 로 풀린다
const REL_QUERY: &str = "select c.oid, c.relkind::text, coalesce(obj_description(c.oid, 'pg_class'), ''),
		coalesce(pg_get_partkeydef(c.oid), ''),
		coalesce(case when c.relkind in ('v', 'm') then pg_get_viewdef(c.oid, true) end, ''),
		coalesce(pn.nspname, ''), coalesce(pc.relname, ''), coalesce(pg_get_expr(c.relpartbound, c.oid), '')
		from pg_class c join pg_namespace n on n.oid = c.relnamespace
		left join pg_inherits h on c.relispartition and h.inhrelid = c.oid
		left join pg_class pc on pc.oid = h.inhparent
		left join pg_namespace pn on pn.oid = pc.relnamespace
		where n.nspname = $1 and c.relname = $2";
// attidentity·attgenerated 는 PostgreSQL 12 부터 있다
const COL_QUERY: &str = "select a.attname, format_type(a.atttypid, a.atttypmod), a.attnotnull,
		coalesce(pg_get_expr(d.adbin, d.adrelid), ''), a.attidentity::text, a.attgenerated::text,
		coalesce(col_description(a.attrelid, a.attnum), '')
		from pg_attribute a left join pg_attrdef d on d.adrelid = a.attrelid and d.adnum = a.attnum
		where a.attrelid = $1 and a.attnum > 0 and not a.attisdropped order by a.attnum";
// NOT NULL 은 열 정의에 붙이므로 뺀다(PostgreSQL 18 은 contype 'n' 으로도 저장한다). 상속받은 제약은 부모가 만든다
const CON_QUERY: &str = "select conname, pg_get_constraintdef(oid) from pg_constraint
		where conrelid = $1 and contype in ('p', 'u', 'c', 'f', 'x') and conislocal
		order by case contype when 'p' then 0 when 'u' then 1 when 'c' then 2 when 'x' then 3 else 4 end, conname";
const IDX_QUERY: &str = "select ic.relname, pg_get_indexdef(i.indexrelid),
		exists (select 1 from pg_constraint k where k.conindid = i.indexrelid and k.conrelid = i.indrelid
			and k.contype in ('p', 'u', 'x')),
		exists (select 1 from pg_inherits h where h.inhrelid = i.indexrelid)
		from pg_index i join pg_class ic on ic.oid = i.indexrelid where i.indrelid = $1 order by ic.relname";

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TableRef {
    pub schema: String,
    pub table: String,
}

impl TableRef {
    pub fn quoted(&self) -> String {
        format!("{}.{}", quote_ident(&self.schema), quote_ident(&self.table))
    }
}

#[derive(Default)]
pub(crate) struct RelInfo {
    kind: String,
    comment: String,
    part_key: String,
    view_def: String,
    // 파티션일 때만 채워진다
    parent_schema: String,
    parent_table: String,
    part_bound: String,
}

pub(crate) struct ColInfo {
    name: String,
    typ: String,
    not_null: bool,
    default: String,
    identity: String,
    generated: String,
    comment: String,
}

pub(crate) struct IdxInfo {
    name: String,
    def: String,
    /// PRIMARY KEY·UNIQUE·EXCLUDE 제약이 만든 인덱스다. DDL 에서는 제약이 다시 만든다
    backing: bool,
    /// 부모 파티션 테이블의 인덱스가 붙인 것이다. DDL 에서는 PARTITION OF 가 다시 만든다
    inherited: bool,
}

#[derive(Default)]
pub(crate) struct TableMeta {
    rel: RelInfo,
    cols: Vec<ColInfo>,
    cons: Vec<(String, String)>,
    idx: Vec<IdxInfo>,
}

pub(crate) async fn load(c: &mut Conn, t: &TableRef) -> Result<TableMeta, Error> {
    let rows = c.query(REL_QUERY, &[Some(&t.schema), Some(&t.table)], usize::MAX).await?;
    if rows.rows.is_empty() {
        return Err(Error::msg(format!("relation {} not found", t.quoted())));
    }
    let oid = rows.text(0, 0);
    let mut m = TableMeta {
        rel: RelInfo {
            kind: rows.text(0, 1),
            comment: rows.text(0, 2),
            part_key: rows.text(0, 3),
            view_def: rows.text(0, 4),
            parent_schema: rows.text(0, 5),
            parent_table: rows.text(0, 6),
            part_bound: rows.text(0, 7),
        },
        ..Default::default()
    };
    let cols = c.query(COL_QUERY, &[Some(&oid)], usize::MAX).await?;
    m.cols = (0..cols.rows.len())
        .map(|r| ColInfo {
            name: cols.text(r, 0),
            typ: cols.text(r, 1),
            not_null: cols.flag(r, 2),
            default: cols.text(r, 3),
            identity: cols.text(r, 4),
            generated: cols.text(r, 5),
            comment: cols.text(r, 6),
        })
        .collect();
    let cons = c.query(CON_QUERY, &[Some(&oid)], usize::MAX).await?;
    m.cons = (0..cons.rows.len()).map(|r| (cons.text(r, 0), cons.text(r, 1))).collect();
    let idx = c.query(IDX_QUERY, &[Some(&oid)], usize::MAX).await?;
    m.idx = (0..idx.rows.len())
        .map(|r| IdxInfo {
            name: idx.text(r, 0),
            def: idx.text(r, 1),
            backing: idx.flag(r, 2),
            inherited: idx.flag(r, 3),
        })
        .collect();
    Ok(m)
}

fn col_default(c: &ColInfo) -> String {
    column_default(&c.identity, &c.generated, &c.default)
}

/// 열마다 이름·타입·NULL 허용·기본값·주석을 행으로, 제약·인덱스를 text 로 돌려준다.
pub(crate) fn describe(m: &TableMeta) -> QueryResult {
    let mut res = QueryResult {
        cols: ["name", "type", "nullable", "default", "comment"].map(String::from).to_vec(),
        ..Default::default()
    };
    for c in &m.cols {
        if res.rows.len() >= MAX_ROWS {
            res.truncated = true;
            break;
        }
        let nullable = if c.not_null { "NO" } else { "YES" };
        res.rows.push(vec![
            c.name.clone(),
            c.typ.clone(),
            nullable.into(),
            cap_cell(col_default(c).as_bytes()),
            cap_cell(c.comment.as_bytes()),
        ]);
    }
    let mut b = String::new();
    if !m.rel.comment.is_empty() {
        b.push_str(&format!("comment: {}\n", cap_cell(m.rel.comment.as_bytes())));
    }
    if !m.cons.is_empty() {
        b.push_str("constraints:\n");
        for (name, def) in &m.cons {
            b.push_str(&format!("  {name}  {def}\n"));
        }
    }
    if !m.idx.is_empty() {
        b.push_str("indexes:\n");
        for ix in &m.idx {
            b.push_str(&format!("  {}  {}\n", ix.name, ix.def));
        }
    }
    res.text = b.trim_end_matches('\n').to_string();
    res
}

fn kind_word(kind: &str) -> &'static str {
    match kind {
        "r" | "p" => "TABLE",
        "v" => "VIEW",
        "m" => "MATERIALIZED VIEW",
        _ => "",
    }
}

/// 카탈로그로 CREATE 문을 다시 만든다. 테이블은 열·제약·인덱스·주석, 뷰는 정의와 주석이다.
/// 파티션은 PARTITION OF 부모와 범위, 자기만 가진 제약·인덱스·주석이다. 열은 부모에서 물려받는다.
/// 단, identity 의 시퀀스 옵션, 열 collation·storage, 파티션에만 준 열 기본값, 트리거·권한·소유자는 넣지 않는다.
pub(crate) fn ddl(t: &TableRef, m: &TableMeta) -> Result<String, Error> {
    let word = kind_word(&m.rel.kind);
    if word.is_empty() {
        return Err(Error::msg(format!(
            "DDL not supported for relation kind {}: {}",
            idb_core::project::go_quote(&m.rel.kind),
            t.quoted()
        )));
    }
    let name = t.quoted();
    let mut b = String::new();
    if m.rel.kind == "v" || m.rel.kind == "m" {
        // pg_get_viewdef 는 끝에 ';' 를 붙여 돌려준다
        b.push_str(&format!("CREATE {word} {name} AS\n{};\n", trim_statement(&m.rel.view_def)));
    } else {
        let mut lines = Vec::new();
        // 파티션의 열은 부모가 정한다
        let cols: &[ColInfo] = if m.rel.part_bound.is_empty() { &m.cols } else { &[] };
        for c in cols {
            let mut line = format!("{} {}", quote_ident(&c.name), c.typ);
            if c.not_null {
                line.push_str(" NOT NULL");
            }
            let d = col_default(c);
            if !d.is_empty() {
                line.push(' ');
                line.push_str(&d);
            }
            lines.push(line);
        }
        for (cname, def) in &m.cons {
            lines.push(format!("CONSTRAINT {} {def}", quote_ident(cname)));
        }
        let mut body = lines.join(",\n    ");
        if !body.is_empty() {
            body = format!("\n    {body}\n");
        }
        let parent = format!("{}.{}", quote_ident(&m.rel.parent_schema), quote_ident(&m.rel.parent_table));
        if m.rel.part_bound.is_empty() {
            b.push_str(&format!("CREATE TABLE {name} ({body})"));
        } else if body.is_empty() {
            b.push_str(&format!("CREATE TABLE {name} PARTITION OF {parent} {}", m.rel.part_bound));
        } else {
            b.push_str(&format!("CREATE TABLE {name} PARTITION OF {parent} ({body}) {}", m.rel.part_bound));
        }
        if !m.rel.part_key.is_empty() {
            b.push_str(&format!(" PARTITION BY {}", m.rel.part_key));
        }
        b.push_str(";\n");
    }
    for ix in m.idx.iter().filter(|ix| !ix.backing && !ix.inherited) {
        b.push_str(&format!("{};\n", ix.def));
    }
    if !m.rel.comment.is_empty() {
        b.push_str(&format!("COMMENT ON {word} {name} IS {};\n", quote_literal(&m.rel.comment)));
    }
    for c in m.cols.iter().filter(|c| !c.comment.is_empty()) {
        b.push_str(&format!(
            "COMMENT ON COLUMN {name}.{} IS {};\n",
            quote_ident(&c.name),
            quote_literal(&c.comment)
        ));
    }
    Ok(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, typ: &str, not_null: bool, default: &str, identity: &str, comment: &str) -> ColInfo {
        ColInfo {
            name: name.into(),
            typ: typ.into(),
            not_null,
            default: default.into(),
            identity: identity.into(),
            generated: String::new(),
            comment: comment.into(),
        }
    }

    fn t() -> TableRef {
        TableRef {
            schema: "s".into(),
            table: "t".into(),
        }
    }

    #[test]
    fn 테이블_DDL_은_열_제약_인덱스_주석을_싣는다() {
        let m = TableMeta {
            rel: RelInfo {
                kind: "r".into(),
                comment: "it's".into(),
                ..Default::default()
            },
            cols: vec![
                col("id", "integer", true, "", "a", ""),
                col("name", "text", false, "'x'::text", "", "이름"),
            ],
            cons: vec![("t_pkey".into(), "PRIMARY KEY (id)".into())],
            idx: vec![
                IdxInfo {
                    name: "t_pkey".into(),
                    def: "CREATE UNIQUE INDEX t_pkey ON s.t USING btree (id)".into(),
                    backing: true,
                    inherited: false,
                },
                IdxInfo {
                    name: "t_name".into(),
                    def: "CREATE INDEX t_name ON s.t USING btree (name)".into(),
                    backing: false,
                    inherited: false,
                },
            ],
        };
        assert_eq!(
            ddl(&t(), &m).unwrap(),
            "CREATE TABLE \"s\".\"t\" (\n    \"id\" integer NOT NULL GENERATED ALWAYS AS IDENTITY,\n    \"name\" text DEFAULT 'x'::text,\n    CONSTRAINT \"t_pkey\" PRIMARY KEY (id)\n);\nCREATE INDEX t_name ON s.t USING btree (name);\nCOMMENT ON TABLE \"s\".\"t\" IS 'it''s';\nCOMMENT ON COLUMN \"s\".\"t\".\"name\" IS '이름';\n"
        );
        let d = describe(&m);
        assert_eq!(d.rows[0], ["id", "integer", "NO", "GENERATED ALWAYS AS IDENTITY", ""]);
        assert_eq!(
            d.text,
            "comment: it's\nconstraints:\n  t_pkey  PRIMARY KEY (id)\nindexes:\n  t_pkey  CREATE UNIQUE INDEX t_pkey ON s.t USING btree (id)\n  t_name  CREATE INDEX t_name ON s.t USING btree (name)"
        );
    }

    #[test]
    fn 뷰와_파티션_DDL() {
        let view = TableMeta {
            rel: RelInfo {
                kind: "v".into(),
                view_def: " SELECT 1;".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(ddl(&t(), &view).unwrap(), "CREATE VIEW \"s\".\"t\" AS\nSELECT 1;\n");
        let part = TableMeta {
            rel: RelInfo {
                kind: "r".into(),
                parent_schema: "s".into(),
                parent_table: "p".into(),
                part_bound: "FOR VALUES FROM (1) TO (2)".into(),
                ..Default::default()
            },
            cols: vec![col("id", "integer", true, "", "", "")],
            ..Default::default()
        };
        assert_eq!(
            ddl(&t(), &part).unwrap(),
            "CREATE TABLE \"s\".\"t\" PARTITION OF \"s\".\"p\" FOR VALUES FROM (1) TO (2);\n"
        );
        let seq = TableMeta {
            rel: RelInfo {
                kind: "S".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            ddl(&t(), &seq).unwrap_err().to_string(),
            "DDL not supported for relation kind \"S\": \"s\".\"t\""
        );
    }
}
