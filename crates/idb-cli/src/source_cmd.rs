//! 저장소 접속의 query·logs·metrics·tables(R60~R63). Go `cli.go` 의 runSourceCommand 와 `sources.go` 의 sourceErrText 다.
//! prod 는 읽기도 --prod 가 필요하고(R58.2), 쓰기로 보이는 질의(Write)·--force 없는 비싼 질의(R60.2)·
//! DB_ALLOW_BILLED 없는 요금 조회(Billed, R72)는 3이다.

use std::io::Read;

use idb_core::project::Kind;
use idb_core::result::QueryResult;
use idb_sources::Error;
use idb_sources::source::{self, Backend, Query};

use crate::command::{CmdError, Env, MSG_UNSUPPORTED, pick_conn, read_input, refused, usage};
use crate::output::CliResult;

const MSG_READ_ONLY_SOURCE: &str = "읽기 전용 저장소라 보내지 않음";
const MSG_EXPENSIVE: &str = "비싼 질의";
/// R67.1: aws 접속은 편집기 질의 언어가 없다
const MSG_TREE_ONLY: &str = "aws 접속은 트리로 본다";

/// logs·metrics 가 받는 접속 종류다.
fn cmd_kinds(cmd: &str) -> &'static [Kind] {
    match cmd {
        "logs" => &[Kind::Loki, Kind::Grafana],
        _ => &[Kind::Prometheus, Kind::Grafana],
    }
}

/// 저장소 오류의 보일 글이다. 읽기 전용 거절은 정한 한국어 안내로, 비싼 질의는 까닭을 붙여 바꾼다(`비싼 질의: 전체 스캔`).
fn source_err_text(err: &source::Error) -> String {
    match err {
        source::Error::Write(_) => MSG_READ_ONLY_SOURCE.into(),
        source::Error::NeedsConfirm(r) if r.is_empty() => MSG_EXPENSIVE.into(),
        source::Error::NeedsConfirm(r) => format!("{MSG_EXPENSIVE}: {r}"),
        e => e.to_string(),
    }
}

fn run_err(e: source::Error) -> CmdError {
    match e {
        source::Error::Write(_) | source::Error::Billed => refused(source_err_text(&e)),
        source::Error::NeedsConfirm(_) => refused(format!("{} (--force)", source_err_text(&e))),
        e => CmdError::Run(Error::msg(e.to_string())),
    }
}

/// 마감까지만 기다린다. 넘으면 Go context.DeadlineExceeded 글이고 err_text 가 시간 초과를 붙인다.
async fn until_deadline<T>(
    env: &Env<'_>,
    fut: impl Future<Output = Result<T, source::Error>>,
) -> Result<Result<T, source::Error>, CmdError> {
    tokio::time::timeout_at(env.deadline, fut)
        .await
        .map_err(|_| CmdError::Run(Error::Deadline))
}

/// 저장소 접속의 query·logs·metrics·tables 를 Backend 로 돌린다. 다른 명령은 사용법 오류다.
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
            ..Default::default()
        };
    }
    if !kind.is_http() {
        return Err(CmdError::Run(Error::msg(format!("{kind} source is not yet ported to db-rs"))));
    }
    let b = until_deadline(env, Backend::open(c)).await?.map_err(run_err)?;
    let res = if o.cmd == "tables" {
        let nodes = until_deadline(env, b.children("")).await?.map_err(run_err)?;
        QueryResult {
            cols: vec!["name".into(), "info".into()],
            rows: nodes.into_iter().map(|n| vec![n.name, n.info]).collect(),
            ..Default::default()
        }
    } else {
        until_deadline(env, b.run(&q)).await?.map_err(run_err)?
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

    #[test]
    fn 저장소_오류는_정한_안내와_종료_코드로_바뀐다() {
        let cases = [
            (source::Error::Write("delete".into()), 3, MSG_READ_ONLY_SOURCE.to_string()),
            (source::Error::NeedsConfirm(String::new()), 3, "비싼 질의 (--force)".into()),
            (
                source::Error::NeedsConfirm("범위 7일 초과".into()),
                3,
                "비싼 질의: 범위 7일 초과 (--force)".into(),
            ),
            (source::Error::Billed, 3, source::Error::Billed.to_string()),
            (source::Error::msg("loki 400 Bad Request: x"), 1, "loki 400 Bad Request: x".into()),
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
