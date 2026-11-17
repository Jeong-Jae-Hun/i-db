//! 읽기 전용 저장소 접속의 query·logs·metrics·tables(R60~R63·R67·R68). Go `cli.go` 의 runSourceCommand 와 `sources.go` 의 sourceErrText 다.
//! prod 는 읽기도 --prod 가 필요하고(R58.2), 쓰기로 보이는 질의·--force 없는 비싼 질의(R60.2)·DB_ALLOW_BILLED 없는
//! 요금 조회(R72)는 3이다.

use std::io::Read;

use idb_core::project::Kind;
use idb_core::result::QueryResult;
use idb_sources::source::{self, Query, err_text as source_err_text};
use idb_sources::{Error, with_deadline};

use crate::command::{CmdError, Env, MSG_UNSUPPORTED, pick_conn, read_input, refused, usage};
use crate::output::CliResult;

/// R67.1: aws 접속은 편집기 질의 언어가 없다
pub const MSG_TREE_ONLY: &str = "aws 접속은 트리로 본다";

/// 질의(run) 오류의 종료 코드다. 쓰기·요금·비싼 질의는 거절(3)이고 나머지는 실행 오류(1)다.
fn run_err(e: Error) -> CmdError {
    match e {
        Error::Write(_) | Error::Billed => refused(source_err_text(&e)),
        Error::NeedsConfirm(_) => refused(format!("{} (--force)", source_err_text(&e))),
        e => e.into(),
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
        "query" if kind == Kind::Aws => return Err(usage(format!("{MSG_TREE_ONLY} (db tables -c {})", o.conn))),
        "query" | "tables" => {}
        "logs" | "metrics" if !cmd_kinds(&o.cmd).contains(&kind) => {
            return Err(usage(format!("{} 는 {} 접속이 아님 ({kind})", o.conn, o.cmd)));
        }
        "logs" | "metrics" => {}
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
        q = Query {
            text: read_input(env, stdin).await?.trim().to_string(),
            since_ns: o.since_ns.unwrap_or(0),
            until_ns: o.until_ns.unwrap_or(0),
            limit: o.max_rows as usize,
            force: o.force,
            // 받은 데까지 돌려줄 수 있는 종류(DynamoDB)는 마감을 스스로 본다
            deadline: Some(env.deadline),
            ..Default::default()
        };
    }
    let b = with_deadline(env.deadline, source::open(c)).await?;
    let res = if o.cmd == "tables" {
        // Go 처럼 목록은 요금 거절만 3이고 나머지 오류는 1이다
        let nodes = with_deadline(env.deadline, b.children("")).await.map_err(|e| match e {
            Error::Billed => refused(e.to_string()),
            e => e.into(),
        })?;
        QueryResult {
            cols: vec!["name".into(), "info".into()],
            rows: nodes.into_iter().map(|n| vec![n.name, n.info]).collect(),
            ..Default::default()
        }
    } else {
        with_deadline(env.deadline, b.run(&q)).await.map_err(run_err)?
    };
    Ok(vec![CliResult {
        conn: c.name.clone(),
        res: Some(res),
        ..Default::default()
    }])
}

#[cfg(test)]
mod tests {
    use super::*;
    use idb_sources::source::MSG_READ_ONLY_SOURCE;

    #[test]
    fn 저장소_오류는_정한_안내와_종료_코드로_바뀐다() {
        let cases = [
            (Error::Write("delete".into()), 3, MSG_READ_ONLY_SOURCE.to_string()),
            (Error::NeedsConfirm(String::new()), 3, "비싼 질의 (--force)".into()),
            (
                Error::NeedsConfirm("범위 7일 초과".into()),
                3,
                "비싼 질의: 범위 7일 초과 (--force)".into(),
            ),
            (Error::Billed, 3, Error::Billed.to_string()),
            (Error::msg("loki 400 Bad Request: x"), 1, "loki 400 Bad Request: x".into()),
        ];
        for (e, code, text) in cases {
            let ce = run_err(e);
            let got = match &ce {
                CmdError::Usage(s) | CmdError::Refused(s) => s.clone(),
                CmdError::Run(e) => e.to_string(),
            };
            assert_eq!((ce.code(), got), (code, text));
        }
    }
}
