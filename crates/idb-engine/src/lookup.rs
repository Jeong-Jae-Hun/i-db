//! 여러 접속을 거치는 조회: 연결된 곳 찾기(R73)·스키마 비교·사용자 한눈에 보기(R51).
//! CLI(`db linked`·`schema-diff`·`user360`)와 serve 의 op 가 같이 쓴다. Go `lookup.go`·`linked.go` 의 데이터 쪽이다.

use std::sync::Arc;
use std::time::Duration;

use idb_core::params::inline_args;
use idb_core::project::{Conn, Kind, Project};
use idb_core::result::{MAX_ROWS, QueryResult};
use idb_core::statcrypt;
use idb_core::user360;
use idb_sources::linked::{LINKED_CAP, LINKED_MAX_COLUMNS, LinkedColumn, linked_base};
use idb_sources::schemadiff::diff_snapshots;
use idb_sources::{Clients, Error, join_indexed, with_deadline};
use tokio::time::Instant;

/// 접속 풀이 4 연결이라 하나는 다른 조회에 남긴다
const LINKED_WORKERS: usize = 3;
/// 같은 그룹 접속이 여럿이어도 한 번에 찾는 접속 수다. 접속마다 LINKED_WORKERS 개씩 세므로 동시 쿼리는 최대 6개다
const LINKED_CONN_WORKERS: usize = 2;
const LINKED_TIMEOUT: Duration = Duration::from_secs(20);
/// 0행 줄이 이보다 많으면 표에서 빼고 요약에 개수만 둔다
const LINKED_ZERO_KEEP: usize = 50;

// ─── 사용자 한눈에 보기 (R51) ───

/// 조회 하나의 결과다. 조회마다 결과 한 칸이다.
pub struct User360Row {
    pub title: String,
    pub conn: String,
    pub res: Option<QueryResult>,
    /// 접속 비밀번호를 지우기 전 글이다. 부르는 쪽이 가린다
    pub err: Option<String>,
}

/// `.dbtui/user360.json` 으로 찾을 값의 조회들을 만든다. 암호화 칸은 그 접속 그룹 키로 값을 암호화해 넣는다.
pub fn user360_plan(proj: &Project, query: &str) -> Result<Vec<user360::Query>, Error> {
    let lookups = user360::load(&proj.dir).map_err(|e| Error::msg(format!("사용자 조회 설정 읽기 실패: {e}")))?;
    if lookups.is_empty() {
        return Err(Error::msg("사용자 조회 설정이 없음: .dbtui/user360.json"));
    }
    let qs = user360::plan(&lookups, query, &|name, plain| {
        let Some(c) = proj.conn_by_name(name) else {
            return user360::Encrypted::NoKey;
        };
        let (key, iv) = proj.aes_key_iv(c);
        if key.is_empty() || iv.is_empty() {
            return user360::Encrypted::NoKey;
        }
        match statcrypt::encrypt(plain, &key, &iv) {
            Ok(s) => user360::Encrypted::Ok(s),
            Err(e) => user360::Encrypted::Err(e.to_string()),
        }
    });
    if qs.is_empty() {
        return Err(Error::msg("맞는 조회가 없음"));
    }
    Ok(qs)
}

/// 사람이 쓰기를 확인하지 않는 여러 접속 조회라 풀 연결의 읽기 전용 트랜잭션으로만 동시에 돈다.
pub async fn user360_run(proj: &Project, clients: Arc<Clients>, qs: &[user360::Query], deadline: Instant) -> Vec<User360Row> {
    let mut jobs = Vec::new();
    let mut out: Vec<User360Row> = Vec::with_capacity(qs.len());
    for (i, q) in qs.iter().enumerate() {
        let mut r = User360Row {
            title: q.title.clone(),
            conn: String::new(),
            res: None,
            err: None,
        };
        match (&q.err, proj.conn_by_name(&q.conn)) {
            (Some(e), _) => r.err = Some(e.clone()),
            (None, None) => r.err = Some(format!("접속 없음: {}", q.conn)),
            (None, Some(c)) if c.kind != Some(Kind::Pg) => r.err = Some(format!("PostgreSQL 접속이 아님: {}", q.conn)),
            (None, Some(c)) => {
                r.conn = c.name.clone();
                let (clients, c, sql) = (clients.clone(), c.clone(), inline_args(&q.sql, &q.args));
                jobs.push((i, async move {
                    with_deadline(deadline, async {
                        let pg = clients.pg(&c).await?;
                        pg.run_read_only_rows(&sql, MAX_ROWS).await
                    })
                    .await
                }));
            }
        }
        out.push(r);
    }
    // 패닉한 조회도 그 칸의 오류(internal error)로 남겨 결과가 조용히 빠지지 않게 한다
    for (i, done) in join_indexed(jobs).await {
        match done.and_then(|r| r) {
            Ok(res) => out[i].res = Some(res),
            Err(e) => out[i].err = Some(e.to_string()),
        }
    }
    out
}

// ─── 스키마 비교 ───

/// 두 접속 스키마의 테이블·열·제약·인덱스 차이다.
pub async fn schema_diff(
    clients: Arc<Clients>,
    a: &Conn,
    b: &Conn,
    schema: &str,
    to_schema: &str,
    deadline: Instant,
) -> Result<QueryResult, Error> {
    let snap = |c: &Conn, schema: String| {
        let (clients, c) = (clients.clone(), c.clone());
        with_deadline(deadline, async move {
            let pg = clients.pg(&c).await?;
            pg.schema_snapshot(&schema).await.map_err(|e| prefix_err(&c.name, e))
        })
    };
    let sa = snap(a, schema.to_string()).await?;
    let sb = snap(b, to_schema.to_string()).await?;
    Ok(diff_snapshots(&sa, &sb))
}

/// Go fmt.Errorf("%s: %w") 다. 마감 오류는 그대로 둬 시간 초과로 알린다.
fn prefix_err(name: &str, e: Error) -> Error {
    match e {
        Error::Deadline => Error::Deadline,
        e => Error::msg(format!("{name}: {e}")),
    }
}

// ─── 연결된 곳 찾기 (R73) ───

/// 접속 하나의 찾기 결과다. err 는 접속·후보 목록 실패다. counts[i] 가 -1 이면 세지 않았다.
#[derive(Default)]
struct LinkedFound {
    cols: Vec<LinkedColumn>,
    counts: Vec<i64>,
    errs: Vec<Option<Error>>,
    truncated: bool,
    err: Option<Error>,
}

/// 칸 값이 든 다른 테이블을 FK 없이 이름 규칙으로 찾고, 같은 그룹·환경의 PostgreSQL 접속까지 센다(R73).
/// 다른 접속은 환경이 같을 때만 든다. 돌려주는 표의 text 는 요약이고 bool 은 접속·테이블 하나라도 실패했는지다.
/// Err 는 사용법 오류 글이다.
#[allow(clippy::too_many_arguments)]
pub async fn linked(
    proj: &Project,
    clients: Arc<Clients>,
    c: &Conn,
    column: &str,
    table: &str,
    value: &str,
    deadline: Instant,
    err_text: &(dyn Fn(&Conn, &Error) -> String + Sync),
) -> Result<(QueryResult, bool), String> {
    if column.is_empty() || value.is_empty() {
        return Err("--column 과 --value 가 필요함".into());
    }
    let short = table.split_once('.').map_or("", |(_, t)| t);
    let base = linked_base(column, if short.is_empty() { table } else { short });
    if column.eq_ignore_ascii_case("id") && base.is_empty() {
        return Err("id 열은 --table 이 있어야 찾음".into());
    }
    let conns: Vec<Conn> = linked_conns(proj, c).into_iter().cloned().collect();
    let found = find_linked(clients, deadline, &conns, column, &base, value).await;
    let (mut res, summary, failed) = linked_table(&conns, &found, |i, e| err_text(&conns[i], e));
    res.text = summary;
    Ok((res, failed))
}

/// 읽기 전용 접속인지다(이름의 -read, 호스트의 read.·.cluster-ro-).
fn is_read_conn(c: &Conn) -> bool {
    c.name.contains("-read") || c.host.starts_with("read.") || c.host.contains(".cluster-ro-")
}

/// conn 과 그룹·환경(R6.2)이 같은 PostgreSQL 접속이다. conn 이 맨 앞이고 그룹이 없으면 conn 하나다.
/// 같은 데이터베이스의 읽기·쓰기 접속 짝은 같은 데이터라 하나만 센다(R73.4): 읽기 접속을 빼되 conn 자신은 남기고,
/// conn 이 읽기 접속이면 짝인 쓰기 접속을 뺀다.
pub fn linked_conns<'a>(proj: &'a Project, cur: &'a Conn) -> Vec<&'a Conn> {
    let mut out = vec![cur];
    if cur.group.is_empty() {
        return out;
    }
    let same: Vec<&Conn> = proj
        .conns
        .iter()
        .filter(|c| !std::ptr::eq(*c, cur) && c.kind == Some(Kind::Pg) && c.group == cur.group && c.env() == cur.env())
        .collect();
    let all: Vec<&Conn> = std::iter::once(cur).chain(same.iter().copied()).collect();
    for c in same {
        // 쓰기 접속은 conn 이 같은 DB 의 읽기 접속일 때만, 읽기 접속은 같은 DB 의 쓰기 접속이 함께 들 때 뺀다
        let twin = all
            .iter()
            .any(|o| !std::ptr::eq(*o, c) && o.db == c.db && is_read_conn(o) != is_read_conn(c));
        if twin && (is_read_conn(c) || is_read_conn(cur) && cur.db == c.db) {
            continue;
        }
        out.push(c);
    }
    out
}

/// 접속을 LINKED_CONN_WORKERS 개씩 동시에 찾는다. 전체는 LINKED_TIMEOUT 안에 끝낸다.
async fn find_linked(clients: Arc<Clients>, deadline: Instant, conns: &[Conn], col: &str, base: &str, value: &str) -> Vec<LinkedFound> {
    let deadline = deadline.min(Instant::now() + LINKED_TIMEOUT);
    let slots = Arc::new(tokio::sync::Semaphore::new(LINKED_CONN_WORKERS));
    let jobs = conns.iter().enumerate().map(|(i, c)| {
        let (clients, c, slots) = (clients.clone(), c.clone(), slots.clone());
        let (col, base, value) = (col.to_string(), base.to_string(), value.to_string());
        (i, async move {
            let _slot = slots.acquire_owned().await;
            let mut f = LinkedFound::default();
            let r = with_deadline(deadline, async {
                let pg = clients.pg(&c).await?;
                let (cols, truncated) = pg.linked_columns(&col, &base, &value).await?;
                let (counts, errs) = pg.count_linked(&cols, &value, LINKED_WORKERS).await;
                Ok((cols, truncated, counts, errs))
            })
            .await;
            match r {
                Ok((cols, truncated, counts, errs)) => (f.cols, f.truncated, f.counts, f.errs) = (cols, truncated, counts, errs),
                Err(e) => f.err = Some(e),
            }
            f
        })
    });
    let mut found: Vec<LinkedFound> = conns.iter().map(|_| LinkedFound::default()).collect();
    // 패닉한 접속은 접속 실패 줄로 보이고 종료 코드 1 이 된다(빈 결과로 보이지 않게)
    for (i, done) in join_indexed(jobs).await {
        found[i] = done.unwrap_or_else(|e| LinkedFound {
            err: Some(e),
            ..Default::default()
        });
    }
    found
}

/// 찾기 결과를 표로 만든다. 줄은 접속 실패, 주인 테이블, 센 행 수 큰 순, 세지 않은 열, 0행 순이고 같으면 이름순이다.
/// failed 는 접속·테이블 하나라도 실패했는지다.
fn linked_table(conns: &[Conn], found: &[LinkedFound], err_text: impl Fn(usize, &Error) -> String) -> (QueryResult, String, bool) {
    struct Row {
        rank: i32,
        rows: i64,
        cells: Vec<String>,
    }
    let mut rows: Vec<Row> = Vec::new();
    let (mut hit, mut zero, mut skipped, mut fails) = (0, 0, 0, 0);
    let mut cut: Vec<&str> = Vec::new();
    for (i, f) in found.iter().enumerate() {
        let name = &conns[i].name;
        if let Some(e) = &f.err {
            fails += 1;
            rows.push(Row {
                rank: -1,
                rows: -1,
                cells: vec![
                    name.clone(),
                    "-".into(),
                    "-".into(),
                    "-".into(),
                    format!("접속 실패: {}", err_text(i, e)),
                ],
            });
            continue;
        }
        if f.truncated {
            cut.push(name);
        }
        for (j, c) in f.cols.iter().enumerate() {
            let mut r = Row {
                rank: 0,
                rows: -1,
                cells: Vec::new(),
            };
            let mut count = "-".to_string();
            let mut notes: Vec<String> = Vec::new();
            if c.owner {
                notes.push("주인 테이블".into());
            }
            let err = f.errs.get(j).and_then(Option::as_ref);
            let n = f.counts.get(j).copied().unwrap_or(-1);
            if !c.indexed {
                skipped += 1;
                r.rank = 2;
                notes.push("인덱스 없음 · 안 셈".into());
            } else if err.is_some_and(Error::is_statement_timeout) {
                fails += 1;
                r.rank = 2;
                notes.push("2초 넘어 멈춤".into());
            } else if let Some(e) = err {
                fails += 1;
                r.rank = 2;
                notes.push(format!("오류: {}", err_text(i, e)));
            } else if n > LINKED_CAP {
                hit += 1;
                (r.rank, r.rows, count) = (1, n, format!("{LINKED_CAP}+"));
            } else if n > 0 {
                hit += 1;
                (r.rank, r.rows, count) = (1, n, n.to_string());
            } else {
                zero += 1;
                (r.rank, r.rows, count) = (3, 0, "0".into());
            }
            if c.owner {
                r.rank = 0;
            }
            r.cells = vec![
                name.clone(),
                format!("{}.{}", c.table.schema, c.table.table),
                c.column.clone(),
                count,
                notes.join(" · "),
            ];
            rows.push(r);
        }
    }
    let dropped = zero > LINKED_ZERO_KEEP;
    if dropped {
        rows.retain(|r| r.rank != 3);
    }
    rows.sort_by(|a, b| {
        a.rank
            .cmp(&b.rank)
            .then(b.rows.cmp(&a.rows))
            .then_with(|| a.cells[0].cmp(&b.cells[0]))
            .then_with(|| a.cells[1].cmp(&b.cells[1]))
            .then_with(|| a.cells[2].cmp(&b.cells[2]))
    });
    let res = QueryResult {
        cols: ["connection", "table", "column", "rows", "note"].map(String::from).to_vec(),
        types: Some(vec!["text".to_string(); 5]),
        rows: rows.into_iter().map(|r| r.cells).collect(),
        ..Default::default()
    };
    let mut parts = vec![format!("연결된 곳 {hit}"), format!("접속 {}곳", conns.len())];
    if dropped {
        parts.push(format!("0행 {zero}개 뺌"));
    } else if zero > 0 {
        parts.push(format!("0행 {zero}"));
    }
    if skipped > 0 {
        parts.push(format!("인덱스 없어 안 셈 {skipped}"));
    }
    if fails > 0 {
        parts.push(format!("실패 {fails}"));
    }
    if !cut.is_empty() {
        parts.push(format!("후보 {LINKED_MAX_COLUMNS}열에서 끊음: {}", cut.join(", ")));
    }
    (res, parts.join(" · "), fails > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use idb_sources::TableRef;

    fn conn(name: &str, group: &str, host: &str, db: &str) -> Conn {
        Conn {
            name: name.into(),
            uuid: name.into(),
            group: group.into(),
            kind: Some(Kind::Pg),
            host: host.into(),
            db: db.into(),
            ..Default::default()
        }
    }

    #[test]
    fn 같은_그룹_환경_접속을_찾고_읽기_쓰기_짝은_하나만_센다() {
        let proj = Project {
            dir: Default::default(),
            conns: vec![
                conn("app-stg", "g", "h", "app"),
                conn("app-stg-read", "g", "h", "app"),
                conn("auth-stg", "g", "h", "auth"),
                conn("app-prod", "g", "h", "app"),
                conn("other-stg", "o", "h", "app"),
            ],
            mapping: Default::default(),
            warnings: vec![],
            aes_env: Default::default(),
        };
        let names = |c: &Conn| linked_conns(&proj, c).iter().map(|c| c.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&proj.conns[0]), ["app-stg", "auth-stg"]);
        // 읽기 접속에서 찾으면 짝인 쓰기 접속을 뺀다
        assert_eq!(names(&proj.conns[1]), ["app-stg-read", "auth-stg"]);
        assert_eq!(names(&proj.conns[3]), ["app-prod"]);
    }

    fn col(table: &str, column: &str, owner: bool, indexed: bool) -> LinkedColumn {
        LinkedColumn {
            table: TableRef {
                schema: "public".into(),
                table: table.into(),
            },
            column: column.into(),
            typ: "int8".into(),
            owner,
            indexed,
        }
    }

    #[test]
    fn 찾기_표는_실패_주인_센_순_안_센_것_0행_순이다() {
        let conns = vec![conn("a", "", "", ""), conn("b", "", "", "")];
        let found = vec![
            LinkedFound {
                cols: vec![
                    col("orders", "user_id", false, true),
                    col("users", "id", true, true),
                    col("logs", "user_id", false, false),
                    col("big", "user_id", false, true),
                    col("none", "user_id", false, true),
                ],
                counts: vec![3, 1, -1, 500, 0],
                errs: vec![None; 5],
                ..Default::default()
            },
            LinkedFound {
                err: Some(Error::msg("boom")),
                ..Default::default()
            },
        ];
        let (res, summary, failed) = linked_table(&conns, &found, |_, e| e.to_string());
        let rows: Vec<String> = res.rows.iter().map(|r| r.join("|")).collect();
        assert_eq!(
            rows,
            [
                "b|-|-|-|접속 실패: boom",
                "a|public.users|id|1|주인 테이블",
                "a|public.big|user_id|100+|",
                "a|public.orders|user_id|3|",
                "a|public.logs|user_id|-|인덱스 없음 · 안 셈",
                "a|public.none|user_id|0|",
            ]
        );
        assert_eq!(summary, "연결된 곳 3 · 접속 2곳 · 0행 1 · 인덱스 없어 안 셈 1 · 실패 1");
        assert!(failed);
    }
}
