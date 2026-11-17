//! 연결된 곳 찾기(R73)의 이름 규칙·문장. Go `internal/db/linked.go` 와 같다.
//! FK 가 없어도 이름 규칙(user_id → *_user_id, users.id)으로 같은 값이 들었을 열을 찾고 센다.
//! 서버를 지키려고 인덱스 첫 열만 세고, 한 테이블은 LINKED_CAP+1 행에서 멈추며 2초 안에 끝나야 한다.

use idb_core::sqlsafe::{quote_ident, quote_literal};

use crate::pg::TableRef;

/// 접속 하나에서 볼 후보 열 수 상한이다
pub const LINKED_MAX_COLUMNS: usize = 200;
/// 테이블 하나에서 셀 행 수다. 넘으면 LINKED_CAP+1 이 와 화면은 100+ 로 보인다
pub const LINKED_CAP: i64 = 100;
/// 테이블 하나 세기의 서버 시간 제한이다
pub(crate) const STATEMENT_TIMEOUT: &str = "2s";

/// 후보 열이다. $1 같은 이름(id 는 빈 글), $2 base, $3 주인 테이블 이름들, $4 받을 타입, $5 상한.
/// 뷰·파티션 자식은 빼고(부모에서 센다) 쓸 수 있는 인덱스의 첫 열(indkey[0])인지 같이 싣는다.
/// 만드는 중·깨진 인덱스와 부분 인덱스(indpred)는 col = 값 에 못 쓸 수 있어 뺀다.
/// *_<base>_id 는 LIKE 대신 right() 로 본다. base 의 _ 가 LIKE 와일드카드라서다.
pub(crate) const COLUMNS_QUERY: &str = r"select n.nspname::text, c.relname::text, a.attname::text, t.typname::text,
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
limit $5";

/// 값이 들었을 수 있는 열 하나다. owner 면 base 이름 테이블의 id 다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkedColumn {
    pub table: TableRef,
    pub column: String,
    pub typ: String,
    pub owner: bool,
    pub indexed: bool,
}

/// 열 이름에서 이름 뿌리를 뽑는다. user_id → user, 원본 테이블 t_users 의 id → user. 그 밖은 "" 이고 같은 이름 열만 찾는다.
pub fn linked_base(col: &str, table: &str) -> String {
    let col = col.to_lowercase();
    if let Some(b) = col.strip_suffix("_id")
        && !b.is_empty()
    {
        return b.to_string();
    }
    if col != "id" || table.is_empty() {
        return String::new();
    }
    let mut t = table.to_lowercase();
    for p in ["tb_", "t_"] {
        if let Some(s) = t.strip_prefix(p)
            && !s.is_empty()
        {
            t = s.to_string();
            break;
        }
    }
    // 영어 복수형 몇 가지만 푼다. types → type 처럼 -es 를 통째로 떼면 틀리는 이름이 많아 s·x·z·ch·sh 뒤에서만 뗀다
    if t.len() > 3 && t.ends_with("ies") {
        return format!("{}y", &t[..t.len() - 3]);
    }
    if ["sses", "xes", "zes", "ches", "shes"].iter().any(|s| t.ends_with(s)) {
        return t[..t.len() - 2].to_string();
    }
    // address·status 는 단수다
    if t.ends_with("ss") || t.ends_with("us") {
        return t;
    }
    if t.len() > 1 && t.ends_with('s') {
        return t[..t.len() - 1].to_string();
    }
    t
}

/// base 의 주인 테이블로 볼 이름이다(소문자). user → user·users·t_users·tb_user …
pub fn owner_names(base: &str) -> Vec<String> {
    if base.is_empty() {
        return Vec::new();
    }
    let mut forms = vec![base.to_string(), format!("{base}s"), format!("{base}es")];
    if let Some(stem) = base.strip_suffix('y')
        && !stem.is_empty()
    {
        forms.push(format!("{stem}ies"));
    }
    ["", "t_", "tb_"]
        .iter()
        .flat_map(|p| forms.iter().map(move |f| format!("{p}{f}")))
        .collect()
}

/// 값과 견줄 수 있는 열 타입(pg_type.typname)이다. 글자 열은 늘 받는다.
/// 열을 형 변환하면 인덱스를 못 써서 값이 그대로 들어갈 타입만 고른다. 범위를 넘는 int2·int4 는 서버가 오류를 내 뺀다.
pub fn linked_types(v: &str) -> Vec<String> {
    let mut types = vec!["text".to_string(), "varchar".to_string()];
    let digits = v.strip_prefix('-').unwrap_or(v);
    let int_form = !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) && (digits == "0" || !digits.starts_with('0'));
    if int_form && let Ok(n) = v.parse::<i64>() {
        types.push("int8".into());
        if i32::try_from(n).is_ok() {
            types.push("int4".into());
        }
        if i16::try_from(n).is_ok() {
            types.push("int2".into());
        }
    }
    if idb_core::user360::is_uuid(v) {
        types.push("uuid".into());
    }
    types
}

/// 열 하나에서 값이 든 행을 LINKED_CAP+1 행까지 세는 문장이다.
pub fn count_sql(c: &LinkedColumn, value: &str) -> String {
    format!(
        "SELECT count(*) FROM (SELECT 1 FROM {} WHERE {} = {} LIMIT {}) s",
        c.table.quoted(),
        quote_ident(&c.column),
        quote_literal(value),
        LINKED_CAP + 1
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 이름_뿌리는_열_꼬리와_테이블_복수형에서_뽑는다() {
        for (col, table, want) in [
            ("user_id", "", "user"),
            ("User_ID", "orders", "user"),
            ("sender_user_id", "", "sender_user"),
            ("_id", "", ""),
            ("id", "", ""),
            ("id", "t_users", "user"),
            ("id", "tb_categories", "category"),
            ("id", "boxes", "box"),
            ("id", "addresses", "address"),
            ("id", "status", "status"),
            ("id", "types", "type"),
            ("id", "s", "s"),
            ("name", "users", ""),
        ] {
            assert_eq!(linked_base(col, table), want, "{col} {table}");
        }
    }

    #[test]
    fn 주인_테이블_이름과_값_타입() {
        assert_eq!(
            owner_names("city"),
            [
                "city",
                "citys",
                "cityes",
                "cities",
                "t_city",
                "t_citys",
                "t_cityes",
                "t_cities",
                "tb_city",
                "tb_citys",
                "tb_cityes",
                "tb_cities"
            ]
        );
        assert!(owner_names("").is_empty());
        assert_eq!(linked_types("12"), ["text", "varchar", "int8", "int4", "int2"]);
        assert_eq!(linked_types("70000"), ["text", "varchar", "int8", "int4"]);
        assert_eq!(linked_types("3000000000"), ["text", "varchar", "int8"]);
        assert_eq!(linked_types("012"), ["text", "varchar"]);
        assert_eq!(linked_types("9999999999"), ["text", "varchar", "int8"]);
        assert_eq!(linked_types("99999999999999999999"), ["text", "varchar"]);
        assert_eq!(linked_types("-5"), ["text", "varchar", "int8", "int4", "int2"]);
        assert_eq!(linked_types("abc"), ["text", "varchar"]);
        assert_eq!(linked_types("-0"), ["text", "varchar", "int8", "int4", "int2"]);
        assert_eq!(linked_types("6f1c2a3b-4d5e-4f60-8a9b-0c1d2e3f4a5b"), ["text", "varchar", "uuid"]);
    }

    #[test]
    fn 세기_문장은_값을_리터럴로_넣고_상한을_건다() {
        let c = LinkedColumn {
            table: TableRef {
                schema: "s".into(),
                table: "t".into(),
            },
            column: "user_id".into(),
            typ: "int8".into(),
            owner: false,
            indexed: true,
        };
        assert_eq!(
            count_sql(&c, "1'"),
            r#"SELECT count(*) FROM (SELECT 1 FROM "s"."t" WHERE "user_id" = '1''' LIMIT 101) s"#
        );
        let weird = LinkedColumn {
            table: TableRef {
                schema: "s".into(),
                table: r#"t"x"#.into(),
            },
            column: r#"we"ird"#.into(),
            ..c
        };
        assert_eq!(
            count_sql(&weird, r"a'b\c"),
            r#"SELECT count(*) FROM (SELECT 1 FROM "s"."t""x" WHERE "we""ird" = E'a''b\\c' LIMIT 101) s"#
        );
    }
}
