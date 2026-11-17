//! 읽기 전용 저장소 접속의 query·logs·metrics·tables(R60~R63·R67·R68). Go `cli.go` 의 runSourceCommand 다.
//! prod 는 읽기도 --prod 가 필요하고(R58.2), 쓰기로 보이는 질의·--force 없는 비싼 질의(R60.2)·DB_ALLOW_BILLED 없는
//! 요금 조회(R72)는 3이다.

use std::io::Read;

use idb_core::project::Kind;
use idb_core::result::QueryResult;
use idb_sources::source::{self, Query};
use idb_sources::{Error, with_deadline};

use crate::command::{CmdError, Env, pick_conn, read_input, refused, usage};
use crate::output::CliResult;

const MSG_READ_ONLY_SOURCE: &str = "읽기 전용 저장소라 보내지 않음";
const MSG_EXPENSIVE: &str = "비싼 질의";
const MSG_UNSUPPORTED: &str = "이 접속 종류에서는 안 됨";
/// R67.1: aws 접속은 편집기 질의 언어가 없다
pub const MSG_TREE_ONLY: &str = "aws 접속은 트리로 본다";

/// 저장소 오류의 보일 글이다. 읽기 전용 거절은 정한 한국어 안내로, 비싼 질의는 까닭을 붙여 바꾼다(`비싼 질의: 전체 스캔`, R60.2).
pub fn source_err_text(e: &Error) -> String {
    match e {
        Error::Write(_) => MSG_READ_ONLY_SOURCE.into(),
        Error::NeedsConfirm(r) if !r.is_empty() => format!("{MSG_EXPENSIVE}: {r}"),
        Error::NeedsConfirm(_) => MSG_EXPENSIVE.into(),
        e => e.to_string(),
    }
}

/// logs·metrics 가 받는 접속 종류다.
fn cmd_kinds(cmd: &str) -> &'static [Kind] {
    match cmd {
        "logs" => &[Kind::Loki, Kind::Grafana],
        "metrics" => &[Kind::Prometheus, Kind::Grafana],
        _ => &[],
    }
}

pub async fn run_source_command(env: &Env<'_>, kind: Kind, stdin: Option<Box<dyn Read + Send>>) -> Result<Vec<CliResult>, CmdError> {
    let o = env.o;
    match o.cmd.as_str() {
        "query" | "tables" => {
            if o.cmd == "query" && kind == Kind::Aws {
                return Err(usage(format!("{MSG_TREE_ONLY} (db tables -c {})", o.conn)));
            }
        }
        "logs" | "metrics" => {
            if !cmd_kinds(&o.cmd).contains(&kind) {
                return Err(usage(format!("{} 는 {} 접속이 아님 ({kind})", o.conn, o.cmd)));
            }
        }
        _ => return Err(usage(format!("{}({kind}): {MSG_UNSUPPORTED}", o.cmd))),
    }
    if o.write {
        return Err(usage("읽기 전용 저장소라 --write 를 받지 않음"));
    }
    if !o.params.is_empty() {
        return Err(usage(format!("--param 은 {MSG_UNSUPPORTED}")));
    }
    let c = pick_conn(env.proj, o, "-c", &o.conn, kind)?;
    let mut q = Query::default();
    if o.cmd != "tables" {
        let text = read_input(env, stdin).await?;
        q = Query {
            text: text.trim().to_string(),
            since_ns: o.since_ns.unwrap_or(0),
            until_ns: o.until_ns.unwrap_or(0),
            limit: o.max_rows as usize,
            force: o.force,
            // 받은 데까지 돌려줄 수 있는 종류(DynamoDB)는 마감을 스스로 본다
            deadline: Some(env.deadline),
            ..Default::default()
        };
    }
    let res = with_deadline(env.deadline, async {
        let b = source::open(c).await?;
        if o.cmd == "tables" {
            let nodes = b.children("").await?;
            return Ok(QueryResult {
                cols: vec!["name".into(), "info".into()],
                rows: nodes.into_iter().map(|n| vec![n.name, n.info]).collect(),
                ..Default::default()
            });
        }
        b.run(&q).await
    })
    .await;
    let res = match res {
        Ok(r) => r,
        Err(Error::Write(_)) => return Err(refused(MSG_READ_ONLY_SOURCE)),
        Err(Error::Billed) => return Err(refused(Error::Billed.to_string())),
        Err(e @ Error::NeedsConfirm(_)) => return Err(refused(format!("{} (--force)", source_err_text(&e)))),
        Err(e) => return Err(e.into()),
    };
    Ok(vec![CliResult {
        conn: c.name.clone(),
        res: Some(res),
        ..Default::default()
    }])
}
