//! 편집기 세션 연결. Go `PGConn.sess`(openSession·release·RunArgs 의 ReadOnly 갈래)와 같다.
//! BEGIN·SET 이 다음 문장까지 이어져야 해서 풀이 아니라 연결 하나를 계속 쓴다.

use std::time::Instant;

use idb_core::result::QueryResult;
use tokio::sync::Mutex;

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
}

/// 실행 하나의 결과와 그 뒤의 트랜잭션 상태다. 실패해도 트랜잭션이 열려 있으면 화면이 알 수 있게 in_tx 를 싣는다(R3.6).
pub struct Ran {
    pub res: Result<QueryResult, Error>,
    pub in_tx: bool,
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
        let start = Instant::now();
        let mut st = self.st.lock().await;
        if std::mem::take(&mut st.running) || st.conn.as_mut().is_some_and(|p| p.conn().is_broken()) {
            st.conn = None;
            st.lost = true;
        }
        if std::mem::take(&mut st.lost) {
            // 끊긴 세션의 상태(트랜잭션·SET)를 모르고 이어 쓰지 않도록 한 번은 실행하지 않고 알린다
            return Ran {
                res: Err(Error::msg(SESSION_LOST)),
                in_tx: false,
            };
        }
        let mut p = match st.conn.take() {
            Some(p) => p,
            None => match self.pg.acquire().await {
                Ok(p) => p,
                Err(e) => return Ran { res: Err(e), in_tx: false },
            },
        };
        st.running = true;
        let params: Vec<Option<&str>> = args.iter().map(Option::as_deref).collect();
        let res = run_session(p.conn(), sql, &params, read_only).await;
        st.running = false;
        let in_tx = p.conn().in_tx();
        if p.conn().is_broken() {
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

async fn run_session(c: &mut wire::Conn, sql: &str, params: &[Option<&str>], read_only: bool) -> Result<QueryResult, Error> {
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
