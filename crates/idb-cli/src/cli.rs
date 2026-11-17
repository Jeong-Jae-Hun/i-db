//! `db <명령> …` 화면 없는 하위 명령(R58). Go `cli.go` 를 옮긴다. 아직 옮기지 않은 명령은 "not yet ported" 로 1 을 낸다.

use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use idb_core::project::{self, Conn, Kind, Project, SOURCE_DB, folder_of, go_quote};
use idb_core::result::QueryResult;

use crate::command::{self, Env};
use crate::goflag::{FlagDef, FlagSet, FlagType, ParseError, Parsed};
use crate::output::{self, CliResult};
use crate::usage_text::{CONN_USAGE, HISTORY_USAGE, UPDATE_USAGE};
use idb_core::gostrconv::{duration_string, parse_day_duration};
use idb_core::textutil::printable;
use idb_engine::{CliInput, CliRequest, CliResponse, Engine, EngineError, RemoteEngine, RemoteSpec};
use idb_sources::Clients;
use tokio::time::Instant;

pub const EXIT_OK: i32 = 0;
pub const EXIT_ERR: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_REFUSED: i32 = 3;

const CLI_DEFAULT_ROWS: i64 = 200;
const CLI_MAX_ROWS: i64 = 10000;
/// 저장소 질의의 기본 한도다(R62.2). --max-rows 를 주면 그 값이다
const SOURCE_DEFAULT_ROWS: i64 = 1000;
pub(crate) const PROJECT_USAGE: &str = "DataGrip project directory (.idea 가 있는 폴더). 기본값은 $DBTUI_PROJECT, 없으면 현재 폴더";

const FORMATS: [&str; 4] = ["table", "tsv", "csv", "json"];

struct Command {
    name: &'static str,
    usage: &'static str,
    desc: &'static str,
}

/// 도움말 순서다. Go `cliCommands` 와 같다.
const COMMANDS: &[Command] = &[
    Command {
        name: "conns",
        usage: "db conns [-p 경로]",
        desc: "접속 목록(이름·그룹·종류·환경·호스트·DB, 비밀번호 없음)",
    },
    Command {
        name: "schemas",
        usage: "db schemas -c 접속",
        desc: "스키마 목록",
    },
    Command {
        name: "tables",
        usage: "db tables -c 접속 [-s 스키마]",
        desc: "테이블·뷰 목록(기본 스키마 public). 저장소 접속은 맨 위 목록(인덱스·라벨·지표·테이블)",
    },
    Command {
        name: "describe",
        usage: "db describe -c 접속 [-s 스키마] 테이블",
        desc: "열·제약·인덱스(tsv·csv 에서 제약·인덱스는 stderr). 테이블은 스키마.테이블 로도 쓴다",
    },
    Command {
        name: "ddl",
        usage: "db ddl -c 접속 [-s 스키마] 테이블",
        desc: "CREATE 문",
    },
    Command {
        name: "query",
        usage: "db query -c 접속 [SQL | -f 파일 | < 표준입력] [--param 이름=값] [--write [--force]]",
        desc: "SQL 한 문장 실행(기본 READ ONLY). 저장소 접속은 그 질의(OpenSearch·LogQL·PromQL·PartiQL)를 보내고 비싼 질의는 --force",
    },
    Command {
        name: "logs",
        usage: "db logs -c Loki접속 'LogQL' [--since 1h] [--until 0] [--force]",
        desc: "Loki 로그 질의(query 와 같고 범위 플래그를 받음, Grafana 접속도)",
    },
    Command {
        name: "metrics",
        usage: "db metrics -c Prometheus접속 'PromQL' [--since 1h] [--until 0] [--force]",
        desc: "Prometheus 지표 질의(Grafana 접속도)",
    },
    Command {
        name: "explain",
        usage: "db explain -c 접속 [SQL | -f 파일 | < 표준입력]",
        desc: "실행 계획(ANALYZE 없음)",
    },
    Command {
        name: "redis",
        usage: "db redis -c 접속 명령 [인자…] [--write [--force]]",
        desc: "Redis 명령 하나(기본 읽기 명령만). - 로 시작하는 인자는 -- 뒤에 쓴다",
    },
    Command {
        name: "user360",
        usage: "db user360 --prod 이메일|uid|id|닉네임",
        desc: "사용자 한눈에 보기(R51). prod 를 조회해 --prod 가 필요하다",
    },
    Command {
        name: "search",
        usage: "db search -c 접속 [-s 스키마] 찾을글",
        desc: "스키마의 글자 열 전체 검색(테이블당 5행)",
    },
    Command {
        name: "linked",
        usage: "db linked -c 접속 --column 열 --value 값 [--table 스키마.테이블]",
        desc: "칸 값이 든 다른 테이블을 이름 규칙으로 찾아 셈(R73). 같은 그룹·환경의 PostgreSQL 접속도, 실패가 있으면 1",
    },
    Command {
        name: "schema-diff",
        usage: "db schema-diff -c 접속A --to 접속B [-s 스키마] [--to-schema 스키마2]",
        desc: "두 스키마의 테이블·열·제약·인덱스 비교",
    },
    Command {
        name: "status",
        usage: "db status [-c 접속 …] [--format table|tsv|json] [--prod]",
        desc: "모든 접속(또는 -c 로 고른 접속)을 동시에 확인해 상태·지연·버전·주요 수치를 찍음(R64.3). prod 는 --prod 일 때만, 하나라도 실패하면 1",
    },
    Command {
        name: "flows",
        usage: "db flows [-f 흐름 …] [--format table|tsv|json] [--prod]",
        desc: "이벤트 흐름(.dbtui/flows.json: 아웃박스 → SNS → SQS → DLQ·Lambda)을 한 번 풀어 상자마다 한 줄을 찍음(R69). prod 는 --prod 일 때만, 설정 오류·실패가 있으면 1",
    },
    Command {
        name: "board",
        usage: "db board [이름] [--format table|tsv|json] [--prod]",
        desc: "쿼리 보드(.dbtui/boards/<이름>.json)의 패널을 한 번 돌려 패널마다 한 줄을 찍음(R70). 이름이 없으면 보드 목록, prod 패널은 --prod 일 때만, 실패한 패널이 있으면 1",
    },
    Command {
        name: "history",
        usage: "db history [--conn 접속] [--since 24h] [--format table|tsv|json] · db history show <id>",
        desc: "실행 기록 목록과 결과 사본(R11.5). db help history",
    },
    Command {
        name: "update",
        usage: "db update [--check] [--version vX.Y.Z] [--force]",
        desc: "GitHub 릴리스의 최신 판으로 db 를 바꿈(R65). --check 는 새 판이 있으면 10. db help update",
    },
    Command {
        name: "conn",
        usage: "db conn add|edit|rm|test …",
        desc: "db 전용 접속 추가·고치기·지우기·시험(R59, .dbtui/connections.json). db help conn",
    },
];

/// 일부 명령만 받는 플래그와 그 명령들이다. 여기 없는 플래그(-p·-c·--format·--prod 등)는 모든 명령이 받는다.
fn cmd_takes(cmd: &str, flag: &str) -> bool {
    let cmds: &[&str] = match flag {
        "write" => &["query", "redis"],
        "force" => &["query", "redis", "logs", "metrics"],
        "since" => &["query", "logs", "metrics"],
        "decrypt" => &["query", "user360", "search"],
        "param" => &["query", "explain"],
        "f" => &["query", "explain", "redis", "logs", "metrics"],
        "s" => &["tables", "describe", "ddl", "search", "schema-diff"],
        "to" => &["schema-diff"],
        "column" => &["linked"],
        _ => return true,
    };
    cmds.contains(&cmd)
}

fn find_command(name: &str) -> Option<&'static Command> {
    COMMANDS.iter().find(|c| c.name == name)
}

pub fn is_cli_command(name: &str) -> bool {
    name == "help" || find_command(name).is_some()
}

pub(crate) fn default_project_dir() -> String {
    std::env::var("DBTUI_PROJECT")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| ".".into())
}

#[derive(Default)]
pub struct Opts {
    pub cmd: String,
    pub dir: String,
    pub conn: String,
    pub format: String,
    pub file: String,
    pub schema: String,
    pub to: String,
    pub to_schema: String,
    pub column: String,
    pub value: String,
    pub table: String,
    pub max_rows: i64,
    pub max_rows_set: bool,
    pub timeout_ns: i64,
    pub prod: bool,
    pub write: bool,
    pub force: bool,
    pub decrypt: bool,
    /// --param 이름=값(같은 이름은 뒤 값)
    pub params: Vec<(String, String)>,
    /// status 의 -c, flows 의 -f(여러 번)
    pub conns: Vec<String>,
    pub since_ns: Option<i64>,
    pub until_ns: Option<i64>,
    pub args: Vec<String>,
    /// serve 가 받은 클라이언트 입력(-f 파일·표준 입력을 클라이언트가 읽은 글, 또는 읽다 난 오류)
    pub input: Option<Result<String, String>>,
}

/// Value 형 플래그의 값 검사(Go paramFlag·durFlag·namesFlag 의 Set).
fn check_value(name: &str, v: &str) -> Result<(), String> {
    match name {
        "param" => match v.split_once('=') {
            Some((k, _)) if !k.is_empty() => Ok(()),
            _ => Err(format!("--param must be name=value: {}", go_quote(v))),
        },
        "since" | "until" => parse_day_duration(v).map(|_| ()),
        _ => Ok(()),
    }
}

fn flag_set(cmd: &str) -> FlagSet {
    let mut defs = Vec::new();
    let mut add = |name: &'static str, ty: FlagType, usage: &str, default: Option<String>| {
        defs.push(FlagDef {
            name,
            usage: usage.into(),
            ty,
            default,
        })
    };
    add(
        "p",
        FlagType::Str,
        PROJECT_USAGE,
        Some(default_project_dir()).filter(|d| !d.is_empty()),
    );
    if cmd == "status" {
        add("c", FlagType::Value, "확인할 접속 이름(여러 번, 없으면 모두)", None);
    } else {
        add("c", FlagType::Str, "접속 이름(db conns)", None);
    }
    add(
        "format",
        FlagType::Str,
        "출력 형식 table|tsv|csv|json (기본: 터미널이면 table, 아니면 tsv)",
        None,
    );
    add(
        "max-rows",
        FlagType::Int,
        &format!("최대 행 수(상한 {CLI_MAX_ROWS})"),
        Some(CLI_DEFAULT_ROWS.to_string()),
    );
    add(
        "timeout",
        FlagType::Duration,
        "전체 시간 제한",
        Some(duration_string(30_000_000_000)),
    );
    add("prod", FlagType::Bool, "prod 접속 허용(읽기만)", None);
    if cmd_takes(cmd, "write") {
        add("write", FlagType::Bool, "쓰기 허용(prod 는 거절)", None);
    }
    if cmd_takes(cmd, "force") {
        add(
            "force",
            FlagType::Bool,
            "--write 에서 위험 문장(R25)·숨은 쓰기 허용, 저장소는 비싼 질의 허용(R60.2)",
            None,
        );
    }
    if cmd_takes(cmd, "since") {
        add(
            "since",
            FlagType::Value,
            "Loki·Prometheus 범위 시작(지금부터 거슬러, 1h·15m·7d, 기본 1h)",
            None,
        );
        add("until", FlagType::Value, "범위 끝(지금부터 거슬러, 기본 0)", None);
    }
    if cmd_takes(cmd, "decrypt") {
        add("decrypt", FlagType::Bool, "staticEncrypt 칸을 접속 그룹 키로 풀어 찍기", None);
    }
    if cmd_takes(cmd, "param") {
        add("param", FlagType::Value, "이름=값, SQL 의 :이름 에 바인딩(여러 번)", None);
    }
    if cmd_takes(cmd, "f") {
        add("f", FlagType::Str, "SQL·명령을 읽을 파일", None);
    }
    if cmd == "flows" {
        add("f", FlagType::Value, "풀 흐름 이름(여러 번, 없으면 모두)", None);
    }
    if cmd_takes(cmd, "s") {
        add("s", FlagType::Str, "스키마", None);
    }
    if cmd_takes(cmd, "to") {
        add("to", FlagType::Str, "비교할 접속", None);
        add("to-schema", FlagType::Str, "비교할 스키마(기본: -s 와 같음)", None);
    }
    if cmd_takes(cmd, "column") {
        add("column", FlagType::Str, "칸의 열 이름(user_id 면 *_user_id·users.id 도 찾음)", None);
        add("value", FlagType::Str, "찾을 값(DB 값 그대로)", None);
        add("table", FlagType::Str, "칸의 원본 테이블(열이 id 일 때 이름 뿌리를 뽑음)", None);
    }
    FlagSet { defs, check: check_value }
}

fn command_usage(cmd: &str, fs: &FlagSet) -> String {
    let c = find_command(cmd).expect("known command");
    format!("사용법: {}\n  {}\n\n플래그:\n{}", c.usage, c.desc, fs.defaults())
}

/// 명령 플래그를 읽고 기본값·범위를 맞춘다(Go parseCLI). Err 는 종료 코드이고 글은 이미 stderr 에 썼다.
fn parse_cli(cmd: &str, args: &[String], tty: bool, stderr: &mut String) -> Result<Opts, i32> {
    let fs = flag_set(cmd);
    let usage = |out: &mut String| out.push_str(&command_usage(cmd, &fs));
    let mut p = Parsed::default();
    let pos = match fs.parse_interleaved(&mut p, args, stderr, &usage) {
        Ok(pos) => pos,
        Err(ParseError::Help) => return Err(EXIT_OK),
        Err(ParseError::Failed) => return Err(EXIT_USAGE),
    };
    let s = |n: &str| p.strs.get(n).cloned().unwrap_or_default();
    let b = |n: &str| p.bools.get(n).copied().unwrap_or(false);
    let vals = |n: &str| p.values.get(n).cloned().unwrap_or_default();
    let mut o = Opts {
        cmd: cmd.into(),
        dir: p.strs.get("p").cloned().unwrap_or_else(default_project_dir),
        conn: s("c"),
        format: s("format"),
        file: s("f"),
        schema: s("s"),
        to: s("to"),
        to_schema: s("to-schema"),
        column: s("column"),
        value: s("value"),
        table: s("table"),
        max_rows: p.ints.get("max-rows").copied().unwrap_or(CLI_DEFAULT_ROWS),
        max_rows_set: p.set.contains("max-rows"),
        timeout_ns: p.durs.get("timeout").copied().unwrap_or(30_000_000_000),
        prod: b("prod"),
        write: b("write"),
        force: b("force"),
        decrypt: b("decrypt"),
        params: vals("param")
            .iter()
            .filter_map(|v| v.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
            .collect(),
        conns: if cmd == "status" {
            vals("c")
        } else if cmd == "flows" {
            vals("f")
        } else {
            Vec::new()
        },
        since_ns: vals("since").last().and_then(|v| parse_day_duration(v).ok()),
        until_ns: vals("until").last().and_then(|v| parse_day_duration(v).ok()),
        args: pos,
        input: None,
    };
    let fail = |stderr: &mut String, msg: String| {
        stderr.push_str(&format!("db {cmd}: {msg}\n"));
        Err(EXIT_USAGE)
    };
    if o.format.is_empty() {
        o.format = if tty { "table".into() } else { "tsv".into() };
    } else if !FORMATS.contains(&o.format.as_str()) {
        return fail(stderr, format!("--format must be one of {}", FORMATS.join("|")));
    }
    if o.max_rows < 1 {
        return fail(stderr, "--max-rows must be at least 1".into());
    }
    o.max_rows = o.max_rows.min(CLI_MAX_ROWS);
    if o.timeout_ns <= 0 {
        return fail(stderr, "--timeout must be positive".into());
    }
    Ok(o)
}

fn cli_help(args: &[String], stdout: &mut String, stderr: &mut String) -> i32 {
    match args.first().map(String::as_str) {
        Some("conn") => {
            stdout.push_str(CONN_USAGE);
            return EXIT_OK;
        }
        Some("update") => {
            stdout.push_str(UPDATE_USAGE);
            return EXIT_OK;
        }
        Some("history") => {
            stdout.push_str(HISTORY_USAGE);
            return EXIT_OK;
        }
        Some(name) => {
            if find_command(name).is_none() {
                stderr.push_str(&format!("알 수 없는 명령: {name}\n"));
                return EXIT_USAGE;
            }
            stdout.push_str(&command_usage(name, &flag_set(name)));
            return EXIT_OK;
        }
        None => {}
    }
    stdout.push_str("사용법: db                 화면(TUI) 실행\n");
    stdout.push_str("        db mcp             읽기 전용 MCP 서버\n");
    stdout.push_str("        db <명령> [플래그]  화면 없이 실행하고 끝냄\n");
    stdout.push_str("\n명령:\n");
    for c in COMMANDS {
        // Go %-12s 는 글자(rune) 수로 채운다. 명령 이름은 ASCII 라 바이트 폭과 같다
        stdout.push_str(&format!("  {:<12} {}\n", c.name, c.desc));
    }
    stdout.push_str("\n공통 플래그: -p 경로 -c 접속 --format table|tsv|csv|json --max-rows N --timeout 30s --prod --decrypt\n");
    stdout.push_str("종료 코드: 0 성공 · 1 실행 오류 · 2 사용법 · 3 안전장치 거절\n");
    stdout.push_str("명령별 사용법: db help <명령> 또는 db <명령> -h\n");
    EXIT_OK
}

/// `db <명령> …` 을 실행하고 종료 코드를 돌려준다. stdin 이 None 이면(터미널) 표준 입력에서 SQL 을 읽지 않는다.
/// tty 는 표준 출력이 터미널인지다(기본 출력 형식). `--remote 이름` 이 있으면 원격 `db serve` 에서 돌린다.
pub fn run_cli(args: &[String], stdin: Option<Box<dyn Read + Send>>, stdout: &mut String, stderr: &mut String, tty: bool) -> i32 {
    let args = match take_remote(args) {
        Ok((args, Some(name))) if args[0] != "help" => return run_remote(&args, &name, stdin, stdout, stderr, tty),
        Ok((args, _)) => args,
        Err(msg) => {
            stderr.push_str(&format!("db {}: {msg}\n", args[0]));
            return EXIT_USAGE;
        }
    };
    run_cli_with(&args, stdin, stdout, stderr, tty, None)
}

/// serve 가 `cli` 요청으로 돌리는 명령이다. 읽기 명령과 안전장치를 거치는 쓰기(query·redis --write)만 받는다.
/// conn·update·history 같은 원격 기계의 파일을 바꾸거나 읽는 명령은 받지 않는다.
const REMOTE_COMMANDS: &[&str] = &[
    "conns",
    "schemas",
    "tables",
    "describe",
    "ddl",
    "query",
    "explain",
    "search",
    "redis",
    "linked",
    "schema-diff",
    "user360",
    "logs",
    "metrics",
    "status",
    "flows",
    "board",
];

/// 인자에서 `--remote 이름`(`-remote`, `=` 꼴도)을 떼어 낸다. `--` 뒤는 명령 인자라 보지 않는다.
fn take_remote(args: &[String]) -> Result<(Vec<String>, Option<String>), String> {
    let (mut out, mut name) = (vec![args[0].clone()], None);
    let mut it = args[1..].iter();
    while let Some(a) = it.next() {
        if a == "--" {
            out.push(a.clone());
            out.extend(it.cloned());
            break;
        }
        match a.as_str() {
            "--remote" | "-remote" => match it.next() {
                Some(v) if !v.is_empty() => name = Some(v.clone()),
                _ => return Err("--remote needs a remote name (~/.config/db/remote.json)".into()),
            },
            _ => match a.strip_prefix("--remote=").or_else(|| a.strip_prefix("-remote=")) {
                Some("") => return Err("--remote needs a remote name (~/.config/db/remote.json)".into()),
                Some(v) => name = Some(v.into()),
                None => out.push(a.clone()),
            },
        }
    }
    Ok((out, name))
}

/// `--remote`: 플래그를 여기서 한 번 읽어 사용법 오류는 바로 알리고, -f 파일·표준 입력은 이 기계에서 읽어 보낸다.
/// 나머지(접속 고르기·안전장치·실행·가리기·출력 글)는 원격 serve 가 이 명령 그대로 만든다.
fn run_remote(
    args: &[String],
    name: &str,
    stdin: Option<Box<dyn Read + Send>>,
    stdout: &mut String,
    stderr: &mut String,
    tty: bool,
) -> i32 {
    let cmd = args[0].as_str();
    if !REMOTE_COMMANDS.contains(&cmd) {
        stderr.push_str(&format!("db {cmd}: not available with --remote\n"));
        return EXIT_USAGE;
    }
    let o = match parse_cli(cmd, &args[1..], tty, stderr) {
        Ok(o) => o,
        Err(code) => return code,
    };
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            stderr.push_str(&format!("{e}\n"));
            return EXIT_ERR;
        }
    };
    let timeout = Duration::from_nanos(o.timeout_ns as u64);
    let res = rt.block_on(async {
        let input = if matches!(cmd, "query" | "explain" | "redis" | "logs" | "metrics") && o.args.is_empty() {
            Some(match command::fetch_input(&o, stdin, Instant::now() + timeout).await {
                Ok(t) => CliInput::Text(t),
                Err(e) => CliInput::Error(e.to_string()),
            })
        } else {
            None
        };
        let spec = RemoteSpec::resolve(name).map_err(EngineError::run)?;
        let eng = Engine::Remote(Arc::new(RemoteEngine::connect(&spec).await?));
        let req = CliRequest {
            args: args.to_vec(),
            input,
            tty,
        };
        // 원격도 --timeout 으로 끝나므로 ssh 가 멈춘 경우만 막는다
        let r = tokio::time::timeout(timeout + Duration::from_secs(60), eng.cli(req))
            .await
            .unwrap_or_else(|_| Err(EngineError::run(format!("remote {name}: no answer"))));
        eng.close().await;
        r
    });
    match res {
        Ok(r) => {
            stdout.push_str(&r.stdout);
            stderr.push_str(&r.stderr);
            r.code
        }
        Err(e) => {
            stderr.push_str(&format!("db {cmd}: {}\n", printable(&e.error, true)));
            e.code
        }
    }
}

/// serve 의 `cli` 요청을 돌린다. 프로젝트는 serve 의 -p 이고, 클라이언트가 준 -p·-f 는 쓰지 않는다.
pub fn run_cli_serve(req: CliRequest, dir: &str) -> CliResponse {
    let (mut out, mut err) = (String::new(), String::new());
    let Some(cmd) = req.args.first().filter(|c| REMOTE_COMMANDS.contains(&c.as_str())) else {
        let cmd = req.args.first().map_or("", String::as_str);
        return CliResponse {
            stdout: out,
            stderr: format!("db {cmd}: not available with --remote\n"),
            code: EXIT_USAGE,
        };
    };
    let args = match take_remote(&req.args) {
        Ok((args, _)) => args,
        Err(msg) => {
            return CliResponse {
                stdout: out,
                stderr: format!("db {cmd}: {msg}\n"),
                code: EXIT_USAGE,
            };
        }
    };
    let serve = Serve {
        dir: dir.to_string(),
        input: req.input.map(|i| match i {
            CliInput::Text(t) => Ok(t),
            CliInput::Error(e) => Err(e),
        }),
    };
    let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_cli_with(&args, None, &mut out, &mut err, req.tty, Some(serve))
    }));
    let code = run.unwrap_or_else(|_| {
        err.push_str("internal error\n");
        EXIT_ERR
    });
    CliResponse {
        stdout: out,
        stderr: err,
        code,
    }
}

/// `db serve [-p 폴더]`: 표준 입출력으로 serve 프로토콜을 받는다(docs/serve-protocol.md). 포트를 열지 않는다.
/// 표준 출력은 프로토콜 줄만 쓰므로 오류는 stderr 에 쓴다.
pub fn run_serve(args: &[String]) -> i32 {
    let mut dir = default_project_dir();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-p" | "--p" => match it.next() {
                Some(v) => dir = v.clone(),
                None => {
                    eprintln!("db serve: flag needs an argument: -p");
                    return EXIT_USAGE;
                }
            },
            _ => match a.strip_prefix("-p=").or_else(|| a.strip_prefix("--p=")) {
                Some(v) => dir = v.into(),
                None => {
                    eprintln!("사용법: db serve [-p 경로]  (stdio 로 엔진 프로토콜, docs/serve-protocol.md)");
                    return EXIT_USAGE;
                }
            },
        }
    }
    let proj = match load_project(&dir) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return EXIT_ERR;
        }
    };
    let dir = proj.dir.to_string_lossy().into_owned();
    let cli: idb_engine::CliHandler = Arc::new(move |req| run_cli_serve(req, &dir));
    let engine = Arc::new(idb_engine::LocalEngine::new(proj, Some(cli)));
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("{e}");
            return EXIT_ERR;
        }
    };
    let input = tokio::io::BufReader::new(tokio::io::stdin());
    match rt.block_on(idb_engine::serve::serve(engine, input, tokio::io::stdout())) {
        Ok(()) => EXIT_OK,
        Err(e) => {
            eprintln!("db serve: {e}");
            EXIT_ERR
        }
    }
}

/// serve 로 도는 명령의 고정값이다.
struct Serve {
    dir: String,
    input: Option<Result<String, String>>,
}

fn run_cli_with(
    args: &[String],
    stdin: Option<Box<dyn Read + Send>>,
    stdout: &mut String,
    stderr: &mut String,
    tty: bool,
    serve: Option<Serve>,
) -> i32 {
    let cmd = args[0].as_str();
    match cmd {
        "help" => return cli_help(&args[1..], stdout, stderr),
        "history" => return crate::history_cmd::run_history(&args[1..], stdout, stderr, tty),
        "conn" => {
            let tty = stdin.is_none();
            let stdin = crate::conn_cmd::Stdin { reader: stdin, tty };
            return crate::conn_cmd::run_conn(&args[1..], stdin, stdout, stderr);
        }
        "update" => return crate::update_cmd::run_update(&crate::update_cmd::default_updater(), &args[1..], stdout, stderr),
        _ => {}
    }
    let mut o = match parse_cli(cmd, &args[1..], tty, stderr) {
        Ok(o) => o,
        Err(code) => return code,
    };
    if let Some(s) = serve {
        o.dir = s.dir;
        o.file.clear();
        o.input = s.input;
    }
    let proj = match load_project(&o.dir) {
        Ok(p) => p,
        Err(e) => {
            stderr.push_str(&format!("{e}\n"));
            return EXIT_ERR;
        }
    };
    let source = proj.conn_by_name(&o.conn).is_some_and(|c| c.kind.is_some_and(Kind::is_source));
    // 저장소 결과 한도는 따로 주지 않으면 1,000행이다(R62.2)
    if source && !o.max_rows_set {
        o.max_rows = SOURCE_DEFAULT_ROWS;
    }
    if o.cmd == "conns" {
        return output::write_results(stdout, stderr, &o, vec![conns_result(&proj, &o)]);
    }
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            stderr.push_str(&format!("{e}\n"));
            return EXIT_ERR;
        }
    };
    let env = Env {
        o: &o,
        proj: &proj,
        clients: Arc::new(Clients::new()),
        deadline: Instant::now() + Duration::from_nanos(o.timeout_ns as u64),
    };
    let sub = match o.cmd.as_str() {
        "status" => Some(rt.block_on(crate::status_cmd::run_status(&env, stdout, stderr))),
        "flows" => Some(rt.block_on(crate::flows_cmd::run_flows(&env, stdout, stderr))),
        "board" => Some(rt.block_on(crate::board_cmd::run_board(&env, stdout, stderr))),
        _ => None,
    };
    if let Some(code) = sub {
        rt.block_on(env.clients.close());
        return code;
    }
    let start = std::time::Instant::now();
    let results = rt.block_on(command::run_command(&env, stdin, stderr));
    let elapsed = start.elapsed();
    rt.block_on(env.clients.close());
    let mut results = match results {
        Ok(r) => r,
        Err(e) => {
            stderr.push_str(&env.err_text(&e, &[&o.conn, &o.to]));
            stderr.push('\n');
            return e.code();
        }
    };
    if o.decrypt
        && let Err(e) = command::decrypt_results(&proj, &mut results, stderr)
    {
        stderr.push_str(&env.err_text(&command::CmdError::Run(idb_sources::Error::msg(e)), &[&o.conn, &o.to]));
        stderr.push('\n');
        return EXIT_ERR;
    }
    for r in &mut results {
        if let Some(err) = &r.err {
            let text = env.err_text(&command::CmdError::Run(idb_sources::Error::msg(err.clone())), &[&r.conn]);
            stderr.push_str(&format!("{}: {text}\n", r.title));
            r.err = Some(text);
            continue;
        }
        let Some(res) = r.res.as_mut() else { continue };
        if res.elapsed.is_zero() {
            res.elapsed = elapsed;
        }
        let max = o.max_rows as usize;
        if res.rows.len() > max {
            res.rows.truncate(max);
            res.truncated = true;
        }
    }
    output::write_results(stdout, stderr, &o, results)
}

pub(crate) fn load_project(dir: &str) -> Result<Project, String> {
    let abs = std::path::absolute(Path::new(dir)).map_err(|e| e.to_string())?;
    // Go filepath.Abs 는 결과를 Clean 한다(./a/../b → b)
    project::load_project(&clean_path(&abs)).map_err(|e| e.to_string())
}

/// Go `filepath.Clean` 처럼 `.`·`..` 마디를 접는다(심볼릭 링크는 풀지 않는다).
fn clean_path(p: &Path) -> std::path::PathBuf {
    let mut out = std::path::PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

/// 사람이 읽는 표는 db 전용 접속 이름에 (db) 를 붙인다(R59.1). 기계가 읽는 형식은 -c 에 그대로 넣게 이름만 두고 source 열로 가른다.
fn conns_result(proj: &Project, o: &Opts) -> CliResult {
    let mut res = QueryResult {
        cols: ["name", "group", "kind", "env", "host", "db", "source", "folder"]
            .map(String::from)
            .to_vec(),
        ..Default::default()
    };
    for c in &proj.conns {
        let source = if c.source == SOURCE_DB { SOURCE_DB } else { "datagrip" };
        let name = if o.format == "table" { conn_label(c) } else { c.name.clone() };
        let host = if c.host.is_empty() { c.url.clone() } else { c.host.clone() };
        res.rows.push(vec![
            name,
            c.group.clone(),
            c.kind_str().into(),
            c.env().into(),
            host,
            c.db.clone(),
            source.into(),
            folder_of(c).into(),
        ]);
    }
    CliResult {
        res: Some(res),
        ..Default::default()
    }
}

/// 트리·표에 보일 접속 이름이다. db 전용 접속은 (db) 를 붙인다.
pub fn conn_label(c: &Conn) -> String {
    if c.source == SOURCE_DB {
        format!("{} (db)", c.name)
    } else {
        c.name.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn remote_플래그는_어디에_있든_떼고_대시_대시_뒤는_둔다() {
        let (args, name) = take_remote(&v(&["query", "-c", "a", "--remote", "devbox", "select 1"])).unwrap();
        assert_eq!((args, name.as_deref()), (v(&["query", "-c", "a", "select 1"]), Some("devbox")));
        let (args, name) = take_remote(&v(&["redis", "-remote=h", "-c", "r", "--", "GET", "--remote"])).unwrap();
        assert_eq!(
            (args, name.as_deref()),
            (v(&["redis", "-c", "r", "--", "GET", "--remote"]), Some("h"))
        );
        assert!(take_remote(&v(&["conns", "--remote"])).is_err());
        assert!(take_remote(&v(&["conns", "--remote="])).is_err());
    }

    #[test]
    fn serve_는_허용하지_않은_명령을_사용법_오류로_돌려준다() {
        let r = run_cli_serve(
            CliRequest {
                args: v(&["update", "--force"]),
                ..Default::default()
            },
            "/nonexistent",
        );
        assert_eq!(
            (r.code, r.stderr.as_str()),
            (EXIT_USAGE, "db update: not available with --remote\n")
        );
    }
}
