//! 편집기 세션 연결. Go `PGConn.sess`(openSession·release·RunArgs·ApplyChanges·SetSearchPath)와 같다.
//! BEGIN·SET 이 다음 문장까지 이어져야 해서 풀이 아니라 연결 하나를 계속 쓴다.

use std::time::Instant;

use idb_core::result::QueryResult;
use idb_core::sqlsafe::quote_ident;
use tokio::sync::Mutex;

use super::TableRef;
use super::edit::{self, Applied, ChangeSet, Origin, text_array};
use super::ops::PRIMARY_KEY_SQL;
use super::{Pg, Pooled, run_on, wire};
use crate::Error;

/// 끊긴 세션을 한 번 알리는 글이다(명세 R3.6).
pub const SESSION_LOST: &str = "이전 세션 연결이 끊겨 트랜잭션·SET 같은 세션 상태가 사라졌다. 다시 실행하면 새 연결에서 돈다";

/// 접속 하나의 편집기 세션이다. 실행은 한 번에 하나다(잠금을 기다리는 동안 버리면 취소된다).
pub struct Session {
    pg: Pg,
    st: Mutex<State>,
}

#[derive(Default)]
struct State {
    conn: Option<Pooled>,
    /// 실행 중에 퓨처가 버려지면(취소) 연결이 함께 닫히고 이 표시만 남는다
    running: bool,
    /// 연결이 끊겨 세션 상태를 잃었고 아직 알리지 않았다
    lost: bool,
    /// set_search_path 로 고른 스키마(R27.2). 새 연결·트랜잭션 끝에 다시 보낸다
    search_path: String,
}

/// 실행 하나의 결과와 그 뒤의 트랜잭션 상태다. 실패해도 트랜잭션이 열려 있으면 화면이 알 수 있게 in_tx 를 싣는다(R3.6).
pub struct Ran {
    pub res: Result<QueryResult, Error>,
    pub in_tx: bool,
    /// 결과가 한 테이블에서 왔고 PK 를 다 담았을 때만(R15)
    pub origin: Option<Origin>,
    /// apply 만 채운다(R50)
    pub applied: Option<Applied>,
}

enum Job<'a> {
    Run {
        sql: &'a str,
        args: &'a [Option<String>],
        read_only: bool,
    },
    Apply(&'a ChangeSet),
    SearchPath(&'a str),
    PrimaryKey(&'a TableRef),
}

fn search_path_sql(schema: &str) -> String {
    format!("SET search_path TO {}, public", quote_ident(schema))
}

impl Session {
    pub fn new(pg: Pg) -> Session {
        Session {
            pg,
            st: Mutex::new(State::default()),
        }
    }

    /// 문장 하나를 세션 연결에서 실행한다. read_only 면 트랜잭션 밖에서는 READ ONLY 트랜잭션을 열고 닫고,
    /// 안이면 SAVEPOINT 안에서만 읽기 전용으로 두고 그 SAVEPOINT 로 되돌린다(R54.1). 읽기만 했으므로 되돌려도 잃는 것이 없다.
    pub async fn run(&self, sql: &str, args: &[Option<String>], read_only: bool) -> Ran {
        self.job(Job::Run { sql, args, read_only }).await
    }

    /// 편집 묶음을 한 트랜잭션(열린 트랜잭션 안이면 SAVEPOINT)으로 반영한다(R15.2).
    pub async fn apply(&self, cs: &ChangeSet) -> Ran {
        self.job(Job::Apply(cs)).await
    }

    /// 테이블의 PK 열이다. 세션 연결에서 찾아 그 트랜잭션 안에서 만들거나 바꾼 테이블·PK 도 본다(apply 와 같은 눈).
    /// 열린 트랜잭션이면 SAVEPOINT 로 감싸 찾다 실패해도 사용자 트랜잭션이 깨지지 않는다.
    pub async fn primary_key(&self, t: &TableRef) -> Result<Vec<String>, Error> {
        self.job(Job::PrimaryKey(t))
            .await
            .res
            .map(|r| r.rows.into_iter().next().unwrap_or_default())
    }

    /// `SET search_path TO "<schema>", public` 을 보내고 성공하면 기억한다(R27.2).
    pub async fn set_search_path(&self, schema: &str) -> Ran {
        self.job(Job::SearchPath(schema)).await
    }

    /// 고른 스키마다. 고른 적 없으면 빈 글이다.
    pub async fn search_path(&self) -> String {
        self.st.lock().await.search_path.clone()
    }

    async fn job(&self, job: Job<'_>) -> Ran {
        let start = Instant::now();
        let fail = |e: Error| Ran {
            res: Err(e),
            in_tx: false,
            origin: None,
            applied: None,
        };
        let mut st = self.st.lock().await;
        if std::mem::take(&mut st.running) || st.conn.as_mut().is_some_and(|p| p.conn().is_broken()) {
            st.conn = None;
            st.lost = true;
        }
        if std::mem::take(&mut st.lost) {
            // 끊긴 세션의 상태(트랜잭션·SET)를 모르고 이어 쓰지 않도록 한 번은 실행하지 않고 알린다
            return fail(Error::msg(SESSION_LOST));
        }
        let mut p = match st.conn.take() {
            Some(p) => p,
            None => match self.pg.acquire().await {
                Ok(mut p) => {
                    if !st.search_path.is_empty()
                        && let Err(e) = p.conn().exec(&search_path_sql(&st.search_path)).await
                    {
                        return fail(Error::msg(format!("restore search_path: {e}")));
                    }
                    p
                }
                Err(e) => return fail(e),
            },
        };
        st.running = true;
        let was_in_tx = p.conn().in_tx();
        let c = p.conn();
        let (mut origin, mut applied) = (None, None);
        let res = match job {
            Job::Run { sql, args, read_only } => {
                let params: Vec<Option<&str>> = args.iter().map(Option::as_deref).collect();
                match run_session(c, sql, &params, read_only).await {
                    Ok((r, sources)) => {
                        origin = edit::lookup_origin(c, &sources).await;
                        Ok(r)
                    }
                    Err(e) => Err(e),
                }
            }
            Job::Apply(cs) => edit::apply_changes(c, cs).await.map(|(r, a)| {
                applied = Some(a);
                r
            }),
            // 결과는 PK 열 이름을 행 하나에 담는다
            Job::PrimaryKey(t) => primary_key_on(c, t).await.map(|pk| QueryResult {
                rows: vec![pk],
                ..Default::default()
            }),
            Job::SearchPath(schema) => match c.exec(&search_path_sql(schema)).await {
                Ok(()) => {
                    st.search_path = schema.to_string();
                    Ok(QueryResult {
                        text: "SET".into(),
                        ..Default::default()
                    })
                }
                Err(e) => Err(e),
            },
        };
        // 트랜잭션 안에서 고른 search_path 는 ROLLBACK 이 되돌린다. 실패하면 연결이 닫히고 다음 실행이 세션 유실로 알린다
        if was_in_tx && !c.in_tx() && !c.is_broken() && !st.search_path.is_empty() {
            let _ = c.exec(&search_path_sql(&st.search_path)).await;
        }
        st.running = false;
        let in_tx = c.in_tx();
        if c.is_broken() {
            st.lost = true;
        } else {
            st.conn = Some(p);
        }
        Ran {
            res: res.map(|mut r| {
                r.elapsed = start.elapsed();
                r
            }),
            in_tx,
            origin,
            applied,
        }
    }

    /// 세션이 트랜잭션 안인지다. 실행 중이면 끝날 때까지 기다린다.
    pub async fn in_tx(&self) -> bool {
        let mut st = self.st.lock().await;
        !st.running && st.conn.as_mut().is_some_and(|p| p.conn().in_tx())
    }

    /// 세션 연결을 Terminate 로 닫는다. 열린 트랜잭션은 서버가 되돌린다.
    pub async fn close(&self) {
        let conn = self.st.lock().await.conn.take();
        if let Some(p) = conn {
            p.close().await;
        }
    }
}

type Sourced = (QueryResult, Vec<(postgres_protocol::Oid, i16)>);

async fn run_session(c: &mut wire::Conn, sql: &str, params: &[Option<&str>], read_only: bool) -> Result<Sourced, Error> {
    if !read_only {
        return run_on(c, sql, params).await;
    }
    let (begin, end, abort) = if c.in_tx() {
        let end = "ROLLBACK TO SAVEPOINT dbtui_ro; RELEASE SAVEPOINT dbtui_ro";
        ("SAVEPOINT dbtui_ro; SET LOCAL transaction_read_only = on", end, end)
    } else {
        ("BEGIN READ ONLY", "COMMIT", "ROLLBACK")
    };
    c.exec(begin).await?;
    match run_on(c, sql, params).await {
        Ok(res) => {
            c.exec(end).await?;
            Ok(res)
        }
        Err(e) if c.is_broken() => Err(e),
        Err(e) => match c.exec(abort).await {
            Ok(()) => Err(e),
            Err(re) => Err(Error::msg(format!("{e}\nrollback: {re}"))),
        },
    }
}

async fn primary_key_on(c: &mut wire::Conn, t: &TableRef) -> Result<Vec<String>, Error> {
    let in_tx = c.in_tx();
    if in_tx {
        c.exec("SAVEPOINT dbtui_pk").await?;
    }
    let rows = c.query(PRIMARY_KEY_SQL, &[Some(&t.quoted())], 1).await;
    if in_tx {
        let end = if rows.is_ok() {
            "RELEASE SAVEPOINT dbtui_pk"
        } else {
            "ROLLBACK TO SAVEPOINT dbtui_pk; RELEASE SAVEPOINT dbtui_pk"
        };
        c.exec(end).await?;
    }
    let rows = rows?;
    Ok(rows.rows.first().map(|_| text_array(&rows.text(0, 0))).unwrap_or_default())
}
