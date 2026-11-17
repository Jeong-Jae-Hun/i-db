//! 결과 표 편집(R15)·제출 되돌리기(R50). Go `internal/db` 의 change.go·edit.go·origin.go 와 같다.
//! 결과가 한 테이블에서 왔고 PK 열이 모두 있을 때만 출처(Origin)를 만들고, 행은 PK 로만 찾는다(ctid 를 쓰지 않는다).

use std::collections::{BTreeMap, HashMap};

use idb_core::result::{MAX_ROWS, NULL_TEXT, QueryResult, cap_cell};
use idb_core::sqlsafe::{quote_ident, quote_literal};
use postgres_protocol::Oid;
use serde::{Deserialize, Serialize};

use super::{TableRef, types, wire};
use crate::Error;

const EDIT_SAVEPOINT: &str = "SAVEPOINT dbtui_edit";
const EDIT_RELEASE: &str = "RELEASE SAVEPOINT dbtui_edit";
const EDIT_ROLLBACK: &str = "ROLLBACK TO SAVEPOINT dbtui_edit; RELEASE SAVEPOINT dbtui_edit";

/// 결과 열의 출처 테이블을 OID 로 한 번에 읽는다. 뷰·구체화 뷰·외부 테이블은 편집하지 않는다.
/// 생성 열과 GENERATED ALWAYS IDENTITY 는 값을 쓸 수 없어 따로 싣는다(attgenerated 는 PostgreSQL 12+).
const ORIGIN_QUERY: &str = "select n.nspname, c.relname, c.relkind in ('r', 'p'),
	coalesce((select i.indkey::int2[] from pg_index i where i.indrelid = c.oid and i.indisprimary), '{}'),
	array(select a.attnum from pg_attribute a
		where a.attrelid = c.oid and a.attnum > 0 and not a.attisdropped order by a.attnum),
	array(select a.attname::text from pg_attribute a
		where a.attrelid = c.oid and a.attnum > 0 and not a.attisdropped order by a.attnum),
	array(select a.attgenerated <> '' from pg_attribute a
		where a.attrelid = c.oid and a.attnum > 0 and not a.attisdropped order by a.attnum),
	array(select a.attidentity = 'a' from pg_attribute a
		where a.attrelid = c.oid and a.attnum > 0 and not a.attisdropped order by a.attnum)
	from pg_class c join pg_namespace n on n.oid = c.relnamespace where c.oid = $1";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeOp {
    Update,
    Insert,
    Delete,
}

impl ChangeOp {
    fn verb(self) -> &'static str {
        match self {
            ChangeOp::Update => "UPDATE",
            ChangeOp::Insert => "INSERT",
            ChangeOp::Delete => "DELETE",
        }
    }
}

/// 결과의 출처 테이블이다. cols[i] 는 결과 i 번째 열의 테이블 열 이름이고 식 열·생성 열이면 "" 다.
/// always 는 GENERATED ALWAYS IDENTITY 인 PK 열이다. 행을 찾는 데는 쓰지만 INSERT 값으로는 쓸 수 없다.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    pub table: TableRef,
    pub pk: Vec<String>,
    pub cols: Vec<String>,
    #[serde(default)]
    pub always: Vec<String>,
}

/// 행 하나의 바꿈이다. key 는 PK 열 → 원래 값(update·delete), values 는 열 → 새 값(None 은 NULL)이다.
/// old 는 읽었을 때의 값이다. 행이 그새 바뀌었거나 다른 행을 가리키면 0행이 되어 묶음 전체가 되돌려진다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub op: ChangeOp,
    #[serde(default)]
    pub key: BTreeMap<String, String>,
    #[serde(default)]
    pub values: BTreeMap<String, Option<String>>,
    #[serde(default)]
    pub old: BTreeMap<String, Option<String>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeSet {
    pub table: TableRef,
    pub pk: Vec<String>,
    pub changes: Vec<Change>,
    /// INSERT 에 OVERRIDING SYSTEM VALUE 를 붙인다. 되돌리기가 지운 행을 IDENTITY 키 그대로 되살릴 때다
    #[serde(default, rename = "override")]
    pub override_system: bool,
}

/// 제출이 RETURNING * 로 받은 행이다. rows[i] 는 changes[i] 의 반영 뒤 행(UPDATE·INSERT)이나 지운 행(DELETE)으로
/// 열 이름 → 자르지 않은 텍스트이고 None 은 NULL 이다. generated 는 묶음에 DELETE 가 있을 때만 채우는 생성 열이다.
/// no_returning 은 RETURNING * 가 권한으로 거절돼 그것 없이 반영했다는 뜻이라 되돌릴 수 없다.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Applied {
    pub rows: Vec<Option<BTreeMap<String, Option<String>>>>,
    #[serde(default)]
    pub generated: Vec<String>,
    #[serde(default)]
    pub no_returning: bool,
}

// ─── 문장 ───

/// 같은 문장을 바인딩($n)으로도, 미리 보기용 리터럴로도 만든다.
struct Builder {
    sql: String,
    args: Vec<Option<String>>,
    inline: bool,
    returning: bool,
}

impl Builder {
    fn value(&mut self, v: Option<&str>) {
        if !self.inline {
            self.args.push(v.map(String::from));
            self.sql.push_str(&format!("${}", self.args.len()));
            return;
        }
        match v {
            Some(v) => self.sql.push_str(&quote_literal(v)),
            None => self.sql.push_str(NULL_TEXT),
        }
    }

    fn returning(&mut self) {
        if self.returning {
            self.sql.push_str(" RETURNING *");
        }
    }
}

/// i 번째 바꿈의 문장이다. 열 순서는 이름순이라 미리 보기와 실행이 같은 문장이 된다.
fn build(cs: &ChangeSet, i: usize, b: &mut Builder) -> Result<(), String> {
    let ch = &cs.changes[i];
    let table = cs.table.quoted();
    match ch.op {
        ChangeOp::Insert => {
            if ch.values.is_empty() {
                b.sql.push_str(&format!("INSERT INTO {table} DEFAULT VALUES"));
                b.returning();
                return Ok(());
            }
            let cols: Vec<String> = ch.values.keys().map(|c| quote_ident(c)).collect();
            b.sql.push_str(&format!("INSERT INTO {table} ({})", cols.join(", ")));
            if cs.override_system {
                b.sql.push_str(" OVERRIDING SYSTEM VALUE");
            }
            b.sql.push_str(" VALUES (");
            for (j, v) in ch.values.values().enumerate() {
                if j > 0 {
                    b.sql.push_str(", ");
                }
                b.value(v.as_deref());
            }
            b.sql.push(')');
            b.returning();
            return Ok(());
        }
        ChangeOp::Update => {
            if ch.values.is_empty() {
                return Err("update has no changed columns".into());
            }
            b.sql.push_str(&format!("UPDATE {table} SET "));
            for (j, (col, v)) in ch.values.iter().enumerate() {
                if j > 0 {
                    b.sql.push_str(", ");
                }
                b.sql.push_str(&format!("{} = ", quote_ident(col)));
                b.value(v.as_deref());
            }
        }
        ChangeOp::Delete => b.sql.push_str(&format!("DELETE FROM {table}")),
    }
    if cs.pk.is_empty() {
        return Err("change set has no primary key".into());
    }
    for (j, k) in cs.pk.iter().enumerate() {
        let Some(v) = ch.key.get(k) else {
            return Err(format!("row key is missing primary key column {}", go_quote(k)));
        };
        b.sql.push_str(if j == 0 { " WHERE " } else { " AND " });
        b.sql.push_str(&format!("{} = ", quote_ident(k)));
        b.value(Some(v));
    }
    // 옛값은 읽을 때 받은 텍스트 표기와 견준다. format('%s') 는 출력 함수를 써서 json·bool·inet 도 맞는다.
    // format 은 NULL 을 '' 로 바꾸므로 NULL 여부를 따로 본다
    for (col, v) in &ch.old {
        let q = quote_ident(col);
        match v {
            None => b.sql.push_str(&format!(" AND {q} IS NULL")),
            Some(v) => {
                b.sql.push_str(&format!(" AND {q} IS NOT NULL AND format('%s', {q}) = "));
                b.value(Some(v));
            }
        }
    }
    b.returning();
    Ok(())
}

fn go_quote(s: &str) -> String {
    idb_core::project::go_quote(s)
}

/// 확인 창에 보일 문장이다. 값을 리터럴로 넣을 뿐 실행하지 않는다. 실패한 바꿈은 그 자리에 오류 주석으로 남는다.
pub fn changes_sql(cs: &ChangeSet) -> Vec<String> {
    (0..cs.changes.len())
        .map(|i| {
            let mut b = Builder {
                sql: String::new(),
                args: Vec::new(),
                inline: true,
                returning: true,
            };
            match build(cs, i, &mut b) {
                Ok(()) => b.sql + ";",
                Err(e) => format!("-- {e}"),
            }
        })
        .collect()
}

/// 반영한 묶음을 되돌리는 묶음이다(R50). 거꾸로 된 순서이고, 옛값 확인은 반영 뒤 행으로 해서
/// 제출 뒤 다른 사람이 고친 행은 덮지 않고 묶음째 실패한다.
pub fn undo_changes(cs: &ChangeSet, applied: &Applied) -> Result<ChangeSet, String> {
    if applied.rows.len() != cs.changes.len() {
        return Err("applied rows do not match the change set; nothing to undo".into());
    }
    if applied.no_returning {
        return Err("RETURNING * was refused (insufficient privilege); changed rows are unknown, cannot undo".into());
    }
    if cs.pk.is_empty() {
        return Err("change set has no primary key".into());
    }
    let mut undo = ChangeSet {
        table: cs.table.clone(),
        pk: cs.pk.clone(),
        changes: Vec::with_capacity(cs.changes.len()),
        override_system: false,
    };
    for i in (0..cs.changes.len()).rev() {
        let ch = &cs.changes[i];
        let Some(post) = &applied.rows[i] else {
            return Err(format!("change {} has no returned row; cannot undo", i + 1));
        };
        let mut key = BTreeMap::new();
        for k in &cs.pk {
            match post.get(k) {
                Some(Some(v)) => key.insert(k.clone(), v.clone()),
                _ => return Err(format!("returned row has no primary key column {}; cannot undo", go_quote(k))),
            };
        }
        let post_of = |col: &str| post.get(col).cloned().flatten();
        undo.changes.push(match ch.op {
            ChangeOp::Update => {
                let (mut values, mut old) = (BTreeMap::new(), BTreeMap::new());
                for col in ch.values.keys() {
                    let Some(prev) = ch.old.get(col) else {
                        // 잘린 칸 등 읽은 값을 모르는 열은 되돌릴 값이 없다
                        return Err(format!("old value of column {} is unknown; cannot undo", go_quote(col)));
                    };
                    values.insert(col.clone(), prev.clone());
                    old.insert(col.clone(), post_of(col));
                }
                Change {
                    op: ChangeOp::Update,
                    key,
                    values,
                    old,
                }
            }
            ChangeOp::Insert => Change {
                op: ChangeOp::Delete,
                key,
                values: BTreeMap::new(),
                old: post
                    .iter()
                    .filter(|(c, _)| !cs.pk.contains(c))
                    .map(|(c, v)| (c.clone(), v.clone()))
                    .collect(),
            },
            ChangeOp::Delete => {
                undo.override_system = true;
                Change {
                    op: ChangeOp::Insert,
                    key: BTreeMap::new(),
                    values: post
                        .iter()
                        .filter(|(c, _)| !applied.generated.contains(c))
                        .map(|(c, v)| (c.clone(), v.clone()))
                        .collect(),
                    old: BTreeMap::new(),
                }
            }
        });
    }
    Ok(undo)
}

// ─── 반영 ───

/// 명령 태그의 영향 행 수다(`UPDATE 1`, `INSERT 0 1`).
fn rows_affected(tag: &str) -> i64 {
    tag.rsplit(' ').next().and_then(|n| n.parse().ok()).unwrap_or(0)
}

/// 편집 묶음을 세션 연결에서 한 번에 반영한다(Go ApplyChanges). UPDATE·DELETE 가 정확히 한 행을 바꾸지 않으면 전부 되돌린다.
/// RETURNING * 가 권한(42501)으로 거절되면 묶음째 되돌린 뒤 RETURNING 없이 한 번 더 낸다.
pub(crate) async fn apply_changes(c: &mut wire::Conn, cs: &ChangeSet) -> Result<(QueryResult, Applied), Error> {
    match apply_once(c, cs, true).await {
        Err(Error::Pg(e)) if e.code == "42501" => apply_once(c, cs, false).await,
        r => r,
    }
}

async fn apply_once(c: &mut wire::Conn, cs: &ChangeSet, returning: bool) -> Result<(QueryResult, Applied), Error> {
    if cs.changes.is_empty() {
        return Err(Error::msg("no changes to apply"));
    }
    if c.tx_failed() {
        return Err(Error::msg("current transaction is aborted; roll back before applying changes"));
    }
    let (begin, commit, rollback) = if c.in_tx() {
        (EDIT_SAVEPOINT, EDIT_RELEASE, EDIT_ROLLBACK)
    } else {
        ("BEGIN", "COMMIT", "ROLLBACK")
    };
    c.exec(begin).await?;
    let r = apply_body(c, cs, returning).await;
    let (res, applied) = match r {
        Ok(v) => v,
        Err(e) => {
            if c.is_broken() {
                return Err(e);
            }
            return match c.exec(rollback).await {
                Ok(()) => Err(e),
                Err(re) => Err(Error::msg(format!("{e}\nrollback: {re}"))),
            };
        }
    };
    if let Err(e) = c.exec(commit).await {
        // SAVEPOINT 해제가 끊기면 바깥 트랜잭션째 사라져 결과가 분명하지만, COMMIT 은 서버가 이미 반영했을 수 있다
        if commit == "COMMIT" && !matches!(e, Error::Pg(_)) {
            return Err(Error::msg(format!("commit outcome unknown: {e}; verify by re-querying")));
        }
        return Err(e);
    }
    Ok((res, applied))
}

async fn apply_body(c: &mut wire::Conn, cs: &ChangeSet, returning: bool) -> Result<(QueryResult, Applied), Error> {
    let mut res = QueryResult::default();
    let mut applied = Applied {
        rows: vec![None; cs.changes.len()],
        generated: Vec::new(),
        no_returning: !returning,
    };
    if cs.changes.iter().any(|ch| ch.op == ChangeOp::Delete) {
        // 지운 행을 되살릴 때 생성 열은 값을 넣을 수 없어 빼야 한다
        let rows = c
            .query(
                "select coalesce(array_agg(attname::text order by attnum), '{}') from pg_attribute
			where attrelid = $1::regclass and attnum > 0 and not attisdropped and attgenerated <> ''",
                &[Some(&cs.table.quoted())],
                1,
            )
            .await?;
        applied.generated = text_array(&rows.text(0, 0));
    }
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for (i, ch) in cs.changes.iter().enumerate() {
        let mut b = Builder {
            sql: String::new(),
            args: Vec::new(),
            inline: false,
            returning,
        };
        build(cs, i, &mut b).map_err(Error::msg)?;
        let params: Vec<Option<&str>> = b.args.iter().map(Option::as_deref).collect();
        // RETURNING * 의 행은 자르지 않고 남긴다. 되돌리기가 이 값으로 행을 찾고 되살린다
        let rows = c.query(&b.sql, &params, MAX_ROWS).await?;
        if let Some(last) = rows.rows.last() {
            applied.rows[i] = Some(
                rows.fields
                    .iter()
                    .zip(last)
                    .map(|((name, _), v)| (name.clone(), v.clone()))
                    .collect(),
            );
        }
        let n = rows_affected(&rows.tag);
        if ch.op == ChangeOp::Insert && cs.override_system && (n != 1 || returning && applied.rows[i].is_none()) {
            return Err(Error::msg(format!(
                "INSERT restoring a deleted row affected {n} rows, expected 1 (a trigger may have skipped it); no changes were applied"
            )));
        }
        if ch.op != ChangeOp::Insert && n != 1 {
            let key: Vec<String> = cs
                .pk
                .iter()
                .map(|k| format!("{k}={}", ch.key.get(k).map_or("", String::as_str)))
                .collect();
            let verb = ch.op.verb();
            return Err(Error::msg(if n == 0 {
                format!(
                    "{verb} failed: row ({}) changed since it was read or was deleted; no changes were applied",
                    key.join(", ")
                )
            } else {
                format!(
                    "{verb} of row ({}) affected {n} rows, expected 1; no changes were applied",
                    key.join(", ")
                )
            }));
        }
        if ch.op == ChangeOp::Insert {
            res.cols = rows.fields.iter().map(|(n, _)| n.clone()).collect();
            res.types = Some(rows.fields.iter().map(|&(_, oid)| types::type_name(oid)).collect());
            res.rows.extend(rows.rows.iter().map(|r| {
                r.iter()
                    .map(|v| v.as_deref().map_or_else(|| NULL_TEXT.to_string(), |s| cap_cell(s.as_bytes())))
                    .collect::<Vec<_>>()
            }));
        }
        *counts.entry(ch.op.verb()).or_default() += 1;
    }
    res.text = ["UPDATE", "INSERT", "DELETE"]
        .iter()
        .filter_map(|v| counts.get(v).map(|n| format!("{v} {n}")))
        .collect::<Vec<_>>()
        .join(" · ");
    Ok((res, applied))
}

// ─── 출처 ───

struct RelOrigin {
    table: TableRef,
    editable: bool,
    pk: Vec<i16>,
    names: HashMap<i16, String>,
    generated: HashMap<i16, bool>,
    identity: HashMap<i16, bool>,
}

/// 식이 아닌 열이 모두 한 테이블에서 왔을 때 그 OID 다. 아니면 0 이다.
pub(crate) fn single_table_oid(sources: &[(Oid, i16)]) -> Oid {
    let mut oid = 0;
    for &(t, _) in sources {
        if t == 0 {
            continue;
        }
        if oid != 0 && oid != t {
            return 0;
        }
        oid = t;
    }
    oid
}

/// 결과의 출처를 읽는다. 캐시하지 않는다(트랜잭션 안 DDL 의 ROLLBACK 이나 다른 세션의 DDL 을 알아챌 방법이 없다).
/// 열린 트랜잭션 안이면 SAVEPOINT 로 감싸 조회가 실패해도 사용자 트랜잭션이 깨지지 않게 한다.
/// 조회가 실패하면 출처 없음(읽기 전용 결과)이다. 문장은 이미 성공했다.
pub(crate) async fn lookup_origin(c: &mut wire::Conn, sources: &[(Oid, i16)]) -> Option<Origin> {
    let oid = single_table_oid(sources);
    if oid == 0 {
        return None;
    }
    let in_tx = c.in_tx();
    if in_tx && c.exec("SAVEPOINT dbtui_origin").await.is_err() {
        return None;
    }
    let oid_text = oid.to_string();
    let rows = c.query(ORIGIN_QUERY, &[Some(&oid_text)], 1).await;
    if in_tx {
        let end = if rows.is_ok() {
            "RELEASE SAVEPOINT dbtui_origin"
        } else {
            "ROLLBACK TO SAVEPOINT dbtui_origin; RELEASE SAVEPOINT dbtui_origin"
        };
        let _ = c.exec(end).await;
    }
    let rows = rows.ok()?;
    if rows.rows.is_empty() {
        return None; // 결과를 받는 사이 테이블이 지워졌다
    }
    let nums: Vec<i16> = text_array(&rows.text(0, 4)).iter().filter_map(|n| n.parse().ok()).collect();
    let names = text_array(&rows.text(0, 5));
    let generated = text_array(&rows.text(0, 6));
    let identity = text_array(&rows.text(0, 7));
    let mut rel = RelOrigin {
        table: TableRef {
            schema: rows.text(0, 0),
            table: rows.text(0, 1),
        },
        editable: rows.flag(0, 2),
        pk: text_array(&rows.text(0, 3)).iter().filter_map(|n| n.parse().ok()).collect(),
        names: HashMap::new(),
        generated: HashMap::new(),
        identity: HashMap::new(),
    };
    // 네 배열은 같은 attnum 순서라 같은 자리가 같은 열이다
    for (i, n) in nums.iter().enumerate() {
        rel.names.insert(*n, names.get(i).cloned().unwrap_or_default());
        rel.generated.insert(*n, generated.get(i).is_some_and(|v| v == "t"));
        rel.identity.insert(*n, identity.get(i).is_some_and(|v| v == "t"));
    }
    build_origin(&rel, sources)
}

/// PK 열이 결과에 모두 있을 때만 출처를 만든다. 같은 테이블 열이 두 번 나오면 자기 조인일 수 있어 만들지 않는다.
fn build_origin(rel: &RelOrigin, sources: &[(Oid, i16)]) -> Option<Origin> {
    if !rel.editable || rel.pk.is_empty() {
        return None;
    }
    let mut cols = vec![String::new(); sources.len()];
    let mut have = Vec::new();
    for (i, &(t, n)) in sources.iter().enumerate() {
        if t == 0 {
            continue;
        }
        // 시스템 열(ctid 등)이거나 같은 열이 겹쳤다. 확실하지 않으면 읽기 전용으로 둔다
        let name = rel.names.get(&n)?;
        if have.contains(&n) {
            return None;
        }
        have.push(n);
        // 생성 열과 PK 가 아닌 IDENTITY ALWAYS 열은 UPDATE·INSERT 에 값을 쓸 수 없어 식 열처럼 다룬다
        if !rel.generated[&n] && (!rel.identity[&n] || rel.pk.contains(&n)) {
            cols[i] = name.clone();
        }
    }
    let mut o = Origin {
        table: rel.table.clone(),
        pk: Vec::new(),
        cols,
        always: Vec::new(),
    };
    for n in &rel.pk {
        if !have.contains(n) {
            return None;
        }
        o.pk.push(rel.names[n].clone());
        if rel.identity[n] {
            o.always.push(rel.names[n].clone());
        }
    }
    Some(o)
}

/// PostgreSQL 배열 텍스트 표기(`{a,"b c",NULL}`)의 원소다. NULL 은 빈 글이다. 1차원만 읽는다.
/// 아래 경계가 1 이 아닌 배열(int2vector 를 바꾼 indkey 등)은 `[0:0]={1}` 처럼 앞에 경계가 붙어 떼고 읽는다.
pub(crate) fn text_array(s: &str) -> Vec<String> {
    let s = s.trim();
    let s = if s.starts_with('[') {
        s.split_once('=').map_or(s, |(_, r)| r)
    } else {
        s
    };
    let inner = s.strip_prefix('{').and_then(|r| r.strip_suffix('}')).unwrap_or("");
    let mut out = Vec::new();
    if inner.is_empty() {
        return out;
    }
    let mut chars = inner.chars().peekable();
    loop {
        let mut cur = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => cur.extend(chars.next()),
                    '"' => break,
                    c => cur.push(c),
                }
            }
            while chars.peek().is_some_and(|&c| c != ',') {
                chars.next();
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c == ',' {
                    break;
                }
                cur.push(c);
                chars.next();
            }
            if cur == "NULL" {
                cur.clear();
            }
        }
        out.push(cur);
        if chars.next().is_none() {
            return out;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Option<String> {
        Some(v.into())
    }

    fn cs(changes: Vec<Change>) -> ChangeSet {
        ChangeSet {
            table: TableRef {
                schema: "public".into(),
                table: "t".into(),
            },
            pk: vec!["id".into()],
            changes,
            override_system: false,
        }
    }

    fn update() -> Change {
        Change {
            op: ChangeOp::Update,
            key: BTreeMap::from([("id".into(), "1".into())]),
            values: BTreeMap::from([("name".into(), s("q")), ("note".into(), None)]),
            old: BTreeMap::from([("name".into(), s("a")), ("note".into(), None)]),
        }
    }

    #[test]
    fn 미리_보기는_이름순_열과_PK_옛값_확인을_싣는다() {
        let c = cs(vec![
            update(),
            Change {
                op: ChangeOp::Insert,
                key: BTreeMap::new(),
                values: BTreeMap::new(),
                old: BTreeMap::new(),
            },
            Change {
                op: ChangeOp::Delete,
                key: BTreeMap::new(),
                values: BTreeMap::new(),
                old: BTreeMap::new(),
            },
        ]);
        assert_eq!(
            changes_sql(&c),
            [
                r#"UPDATE "public"."t" SET "name" = 'q', "note" = NULL WHERE "id" = '1' AND "name" IS NOT NULL AND format('%s', "name") = 'a' AND "note" IS NULL RETURNING *;"#,
                r#"INSERT INTO "public"."t" DEFAULT VALUES RETURNING *;"#,
                r#"-- row key is missing primary key column "id""#,
            ]
        );
    }

    #[test]
    fn 바인딩_문장은_값을_자리로_보낸다() {
        let mut b = Builder {
            sql: String::new(),
            args: Vec::new(),
            inline: false,
            returning: false,
        };
        build(&cs(vec![update()]), 0, &mut b).unwrap();
        assert_eq!(
            b.sql,
            r#"UPDATE "public"."t" SET "name" = $1, "note" = $2 WHERE "id" = $3 AND "name" IS NOT NULL AND format('%s', "name") = $4 AND "note" IS NULL"#
        );
        assert_eq!(b.args, [s("q"), None, s("1"), s("a")]);
    }

    #[test]
    fn 되돌리기는_거꾸로_반영_뒤_행으로_만든다() {
        let c = cs(vec![
            update(),
            Change {
                op: ChangeOp::Delete,
                key: BTreeMap::from([("id".into(), "2".into())]),
                values: BTreeMap::new(),
                old: BTreeMap::new(),
            },
        ]);
        let applied = Applied {
            rows: vec![
                Some(BTreeMap::from([
                    ("id".into(), s("1")),
                    ("name".into(), s("q")),
                    ("note".into(), None),
                ])),
                Some(BTreeMap::from([
                    ("id".into(), s("2")),
                    ("name".into(), s("b")),
                    ("g".into(), s("x")),
                ])),
            ],
            generated: vec!["g".into()],
            no_returning: false,
        };
        let u = undo_changes(&c, &applied).unwrap();
        assert!(u.override_system);
        assert_eq!(u.changes[0].op, ChangeOp::Insert);
        assert_eq!(
            u.changes[0].values,
            BTreeMap::from([("id".into(), s("2")), ("name".into(), s("b"))])
        );
        assert_eq!(
            u.changes[1].values,
            BTreeMap::from([("name".into(), s("a")), ("note".into(), None)])
        );
        assert_eq!(u.changes[1].old, BTreeMap::from([("name".into(), s("q")), ("note".into(), None)]));
        let no = Applied {
            no_returning: true,
            ..applied.clone()
        };
        assert!(undo_changes(&c, &no).unwrap_err().starts_with("RETURNING * was refused"));
    }

    #[test]
    fn 출처는_PK_가_모두_있고_열이_겹치지_않을_때만_만든다() {
        let rel = RelOrigin {
            table: TableRef {
                schema: "s".into(),
                table: "t".into(),
            },
            editable: true,
            pk: vec![1],
            names: HashMap::from([(1, "id".into()), (2, "name".into()), (3, "g".into())]),
            generated: HashMap::from([(1, false), (2, false), (3, true)]),
            identity: HashMap::from([(1, true), (2, false), (3, false)]),
        };
        let o = build_origin(&rel, &[(9, 1), (9, 2), (0, 0), (9, 3)]).unwrap();
        assert_eq!(o.cols, ["id", "name", "", ""]);
        assert_eq!((o.pk, o.always), (vec!["id".to_string()], vec!["id".to_string()]));
        assert!(build_origin(&rel, &[(9, 2)]).is_none(), "PK 없음");
        assert!(build_origin(&rel, &[(9, 1), (9, 1)]).is_none(), "같은 열 두 번");
        assert!(build_origin(&rel, &[(9, 1), (9, -1)]).is_none(), "시스템 열");
        assert_eq!(single_table_oid(&[(9, 1), (0, 0), (8, 1)]), 0);
    }

    #[test]
    fn 배열_글은_따옴표와_NULL_을_읽는다() {
        assert_eq!(text_array("{}"), Vec::<String>::new());
        assert_eq!(text_array(r#"{a,"b c","d\"e",NULL}"#), ["a", "b c", "d\"e", ""]);
        assert_eq!(text_array("[0:1]={1,2}"), ["1", "2"]);
        assert_eq!(rows_affected("INSERT 0 1"), 1);
        assert_eq!(rows_affected("UPDATE 0"), 0);
    }
}
