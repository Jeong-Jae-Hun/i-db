//! PostgreSQL·Redis 하위 명령(query·explain·schemas·tables·describe·ddl·search·redis·linked·schema-diff·user360).
//! Go `cli.go` 의 runCommand·pickConn·readInput·pgStatement·tableRef·decryptResults·cliErrText 와 `lookup.go`·`linked.go` 의 CLI 쪽이다.
//! 안전장치(prod·--write·위험 문장·KEYS)는 접속을 열기 전에 거절한다. 그래야 prod 에 연결 자체를 하지 않는다.

use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use idb_core::gostrconv::duration_string;
use idb_core::params::{bind, inline_args};
use idb_core::project::{Conn, Kind, Project, env_name};
use idb_core::redact::redact;
use idb_core::result::{MAX_ROWS, QueryResult};
use idb_core::sqlsafe::{
    ExplainOpt, check_read_only_sql, danger_of, danger_of_redis, explain_sql, first_sql_word, hidden_write_of, is_read_only_redis,
    split_args, split_statements,
};
use idb_core::statcrypt;
use idb_core::textutil::{go_upper, printable};
use idb_core::user360;
use idb_sources::linked::{LINKED_CAP, LINKED_MAX_COLUMNS, LinkedColumn, linked_base};
use idb_sources::schemadiff::diff_snapshots;
use idb_sources::{Clients, Error, TableRef, join_indexed, with_deadline};
use tokio::time::Instant;

use crate::cli::{EXIT_ERR, EXIT_REFUSED, EXIT_USAGE, Opts};
use crate::output::CliResult;

/// 표준 입력으로 받는 SQL·명령의 상한. 실수로 덤프를 흘려 넣어도 메모리를 다 쓰지 않게 한다
const CLI_MAX_STDIN: usize = 16 << 20;
/// query --write 가 실행하는 문장의 첫 단어다. select·with 는 R25 밖의 숨은 쓰기(select into·CTE 쓰기)를
/// --force 로 받으려고 넣는다. COPY(서버 파일·프로그램), DO·CALL(임의 코드), SET·BEGIN 같은 세션 명령은 받지 않는다.
const CLI_WRITE_VERBS: &[&str] = &[
    "insert", "update", "delete", "merge", "create", "alter", "drop", "truncate", "comment", "grant", "revoke", "refresh", "vacuum",
    "analyze", "reindex", "cluster", "select", "with",
];
const MSG_UNSUPPORTED: &str = "이 접속 종류에서는 안 됨";
/// 접속 풀이 4 연결이라 하나는 다른 조회에 남긴다
const LINKED_WORKERS: usize = 3;
/// 같은 그룹 접속이 여럿이어도 한 번에 찾는 접속 수다. 접속마다 LINKED_WORKERS 개씩 세므로 동시 쿼리는 최대 6개다
const LINKED_CONN_WORKERS: usize = 2;
const LINKED_TIMEOUT: Duration = Duration::from_secs(20);
/// 0행 줄이 이보다 많으면 표에서 빼고 요약에 개수만 둔다
const LINKED_ZERO_KEEP: usize = 50;

/// 종료 코드를 정한 오류다. Run 은 실행 오류(1)다.
pub enum CmdError {
    Usage(String),
    Refused(String),
    Run(Error),
}

impl CmdError {
    pub fn code(&self) -> i32 {
        match self {
            CmdError::Usage(_) => EXIT_USAGE,
            CmdError::Refused(_) => EXIT_REFUSED,
            CmdError::Run(_) => EXIT_ERR,
        }
    }

    fn text(&self) -> String {
        match self {
            CmdError::Usage(s) | CmdError::Refused(s) => s.clone(),
            CmdError::Run(e) => e.to_string(),
        }
    }
}

impl From<Error> for CmdError {
    fn from(e: Error) -> CmdError {
        CmdError::Run(e)
    }
}

fn usage(s: impl Into<String>) -> CmdError {
    CmdError::Usage(s.into())
}

fn refused(s: impl Into<String>) -> CmdError {
    CmdError::Refused(s.into())
}

/// 명령 하나의 실행 상태다. Go 의 ctx(마감)·proj·clients 묶음이다.
pub struct Env<'a> {
    pub o: &'a Opts,
    pub proj: &'a Project,
    pub clients: Arc<Clients>,
    pub deadline: Instant,
}

impl Env<'_> {
    /// 오류를 찍을 글로 만든다. 서버가 비밀번호·키를 되돌려줘도 나오지 않게 conns(접속 이름)의 비밀번호와
    /// 모든 AES 키·IV 를 지운다. 비밀번호를 모든 접속 것으로 넓히지 않는 까닭: test 같은 짧은 로컬 비밀번호가 접속 이름까지 가린다.
    pub fn err_text(&self, err: &CmdError, conns: &[&str]) -> String {
        self.err_msg(&err.text(), matches!(err, CmdError::Run(Error::Deadline)), conns)
    }

    fn err_msg(&self, msg: &str, deadline: bool, conns: &[&str]) -> String {
        let mut msg = msg.to_string();
        if deadline || Instant::now() >= self.deadline {
            msg = format!("시간 초과(--timeout {}): {msg}", duration_string(self.o.timeout_ns));
        }
        for c in self.proj.conns.iter().filter(|c| conns.contains(&c.name.as_str())) {
            msg = redact(&msg, &c.password);
        }
        for v in self.proj.aes_env.values() {
            msg = redact(&msg, v);
        }
        printable(&msg, true)
    }
}

/// 명령을 실행해 출력할 결과들을 돌려준다. stderr 에는 enc() 알림·건너뛴 테이블처럼 결과에 딸린 글을 쓴다.
pub async fn run_command(env: &Env<'_>, stdin: Option<Box<dyn Read + Send>>, stderr: &mut String) -> Result<Vec<CliResult>, CmdError> {
    let o = env.o;
    let proj = env.proj;
    // --force 는 PostgreSQL·Redis 에서는 --write 와 함께만 쓴다(R58.1)
    if o.force && !o.write {
        return Err(usage("--force 는 --write 와 함께 씀"));
    }
    match o.cmd.as_str() {
        "logs" | "metrics" => {
            let kinds = if o.cmd == "logs" { "Loki·Grafana" } else { "Prometheus·Grafana" };
            return Err(usage(format!("{} 는 {kinds} 접속에서만 됨 (db conns)", o.cmd)));
        }
        "user360" => return user360(env).await,
        "redis" => return redis(env, stdin).await,
        "linked" => return linked(env).await,
        "schema-diff" => return schema_diff(env).await,
        _ => {}
    }

    // 나머지는 PostgreSQL 접속 하나에서 돈다
    let c = pick_conn(proj, o, "-c", &o.conn, Kind::Pg)?;
    let (mut sql, mut args, mut table) = (String::new(), Vec::new(), TableRef::default());
    match o.cmd.as_str() {
        "describe" | "ddl" => table = table_ref(o)?,
        "search" if o.args.is_empty() => return Err(usage("찾을 글이 필요함")),
        "query" => {
            (sql, args) = pg_statement(env, c, stdin, stderr).await?;
            if o.write {
                let verb = first_sql_word(&sql);
                let danger = [danger_of(&sql), hidden_write_of(&sql)]
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join(", ");
                if c.env() == "prod" {
                    return Err(refused("prod 쓰기는 CLI 에서 하지 않음 (화면에서 R6·R54 를 거침)"));
                }
                if !CLI_WRITE_VERBS.contains(&verb.as_str()) {
                    return Err(refused(format!("--write 로 실행하지 않는 문장: {}", go_upper(&verb))));
                }
                if !o.force && !danger.is_empty() {
                    return Err(refused(format!("위험: {danger} (--force)")));
                }
            } else if let Err(e) = check_read_only_sql(&sql) {
                return Err(refused(format!("읽기 문장이 아님 (쓰기는 --write): {e}")));
            }
        }
        "explain" => {
            (sql, args) = pg_statement(env, c, stdin, stderr).await?;
            // 직접 쓴 EXPLAIN 은 감싸지 않으므로 ANALYZE 는 여기서 거절한다(문장이 실제로 실행된다)
            sql = explain_sql(&sql, &ExplainOpt::default());
            if let Err(e) = check_read_only_sql(&sql) {
                return Err(refused(format!("EXPLAIN ANALYZE·쓰기 문장 EXPLAIN 은 실행하지 않음: {e}")));
            }
        }
        _ => {}
    }

    let res = with_deadline(env.deadline, async {
        let pg = env.clients.pg(c).await?;
        Ok(match o.cmd.as_str() {
            "schemas" => names_result("schema", pg.find_schemas().await?),
            "tables" => names_result("table", pg.find_tables(or(&o.schema, "public")).await?),
            "describe" => pg.describe_table(&table).await?,
            "ddl" => QueryResult {
                text: pg.table_ddl(&table).await?,
                ..Default::default()
            },
            "search" => {
                let (res, skipped) = pg.search_data(&o.schema, &o.args.join(" ")).await?;
                if !skipped.is_empty() {
                    // 테이블 이름과 함께 오는 오류 글도 서버 응답이라 가리고 제어 문자를 바꾼다
                    stderr.push_str(&env.err_msg(&format!("건너뛴 테이블: {}", skipped.join(", ")), false, &[&o.conn]));
                    stderr.push('\n');
                }
                res
            }
            // explain 은 --write 를 받지 않지만 실행 경로도 명령으로 갈라 늘 READ ONLY 로 돈다
            "query" if o.write => {
                let args: Vec<String> = args.iter().map(|a: &Option<String>| a.clone().unwrap_or_default()).collect();
                pg.run_args(&sql, &args).await?
            }
            // 읽기 전용 실행은 바인딩을 받지 않아 값을 리터럴로 넣는다(R51 과 같은 방식)
            "query" | "explain" => pg.run_read_only_rows(&inline_args(&sql, &args), o.max_rows as usize).await?,
            _ => return Err(Error::msg(format!("db {}: not yet ported to db-rs", o.cmd))),
        })
    })
    .await?;
    Ok(vec![CliResult {
        conn: c.name.clone(),
        res: Some(res),
        ..Default::default()
    }])
}

fn or<'a>(s: &'a str, def: &'a str) -> &'a str {
    if s.is_empty() { def } else { s }
}

fn names_result(col: &str, names: Vec<String>) -> QueryResult {
    QueryResult {
        cols: vec![col.into()],
        rows: names.into_iter().map(|n| vec![n]).collect(),
        ..Default::default()
    }
}

/// -c(또는 --to) 이름의 접속을 찾고 prod 허용을 확인한다. 접속을 열기 전에 거절해야 prod 에 연결 자체를 하지 않는다.
fn pick_conn<'a>(proj: &'a Project, o: &Opts, flag: &str, name: &str, kind: Kind) -> Result<&'a Conn, CmdError> {
    if name.is_empty() {
        return Err(usage(format!("{flag} <접속 이름> 이 필요함 (db conns)")));
    }
    let Some(c) = proj.conn_by_name(name) else {
        return Err(usage(format!("접속 없음: {name} (db conns)")));
    };
    if c.kind != Some(kind) {
        if c.kind.is_some_and(Kind::is_source) {
            return Err(usage(format!("{name}({}): {MSG_UNSUPPORTED}", c.kind_str())));
        }
        return Err(usage(format!("{name} 는 {kind} 접속이 아님")));
    }
    if c.env() == "prod" && !o.prod {
        return Err(refused("prod 접속은 --prod 가 필요함"));
    }
    Ok(c)
}

/// 위치 인자, -f 파일, 표준 입력 순으로 SQL·명령 글을 읽는다. 빈 글이면 사용법 오류다.
async fn read_input(env: &Env<'_>, stdin: Option<Box<dyn Read + Send>>) -> Result<String, CmdError> {
    let text = match &env.o.input {
        // serve 는 클라이언트가 읽어 보낸 글만 쓴다. 서버의 -f 파일·표준 입력(프로토콜 줄)은 읽지 않는다
        Some(input) => input.clone().map_err(Error::msg)?,
        None => fetch_input(env.o, stdin, env.deadline).await?,
    };
    if text.trim().is_empty() {
        return Err(usage("실행할 글이 없음 (인자, -f 파일, 표준 입력)"));
    }
    Ok(text)
}

/// 위치 인자, -f 파일, 표준 입력 순으로 글을 읽는다. `--remote` 는 이것으로 읽은 글을 원격에 보낸다.
/// 표준 입력은 닫히지 않는 파이프에서도 deadline 에 끝나게 따로 읽고, CLI_MAX_STDIN 을 넘으면 오류다.
pub async fn fetch_input(o: &Opts, stdin: Option<Box<dyn Read + Send>>, deadline: Instant) -> Result<String, Error> {
    if !o.args.is_empty() {
        return Ok(o.args.join(" "));
    }
    if !o.file.is_empty() {
        let b = std::fs::read(&o.file).map_err(|e| Error::msg(format!("open {}: {}", o.file, go_io_text(&e))))?;
        return Ok(String::from_utf8_lossy(&b).into_owned());
    }
    let Some(mut stdin) = stdin else {
        return Ok(String::new());
    };
    let (tx, rx) = tokio::sync::oneshot::channel();
    // 시간 초과로 돌아가도 읽는 스레드는 남는다. CLI 는 곧 끝나므로 둔다
    std::thread::spawn(move || {
        let mut b = Vec::new();
        let r = (&mut stdin).take(CLI_MAX_STDIN as u64 + 1).read_to_end(&mut b).map(|_| b);
        let _ = tx.send(r);
    });
    match tokio::time::timeout_at(deadline, rx).await {
        Err(_) => Err(Error::msg("표준 입력 읽기: context deadline exceeded")),
        Ok(Err(_)) => Err(Error::msg("표준 입력 읽기 실패")),
        Ok(Ok(Err(e))) => Err(Error::msg(go_io_text(&e))),
        Ok(Ok(Ok(b))) if b.len() > CLI_MAX_STDIN => Err(Error::msg("표준 입력이 16MiB 를 넘음 (-f 파일로도 같은 문장 하나만 실행함)")),
        Ok(Ok(Ok(b))) => Ok(String::from_utf8_lossy(&b).into_owned()),
    }
}

/// Go os 오류 글의 까닭 부분이다.
fn go_io_text(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::NotFound => "no such file or directory".into(),
        std::io::ErrorKind::PermissionDenied => "permission denied".into(),
        _ => {
            let s = e.to_string();
            s.split(" (os error").next().unwrap_or(&s).to_lowercase()
        }
    }
}

/// 입력에서 문장 하나를 골라 enc() 를 확장하고(R52) :이름 을 $n 으로 바꾼 글과 인자를 돌려준다.
async fn pg_statement(
    env: &Env<'_>,
    c: &Conn,
    stdin: Option<Box<dyn Read + Send>>,
    stderr: &mut String,
) -> Result<(String, Vec<Option<String>>), CmdError> {
    let text = read_input(env, stdin).await?;
    let stmts = split_statements(&text);
    if stmts.len() != 1 {
        return Err(usage(format!("문장 하나만 실행함 ({}개)", stmts.len())));
    }
    let (sent, note) = env.proj.expand_enc(c, stmts[0]).map_err(usage)?;
    if !note.is_empty() {
        stderr.push_str(&note);
        stderr.push('\n');
    }
    let (sql, names) = bind(&sent);
    let param = |n: &str| env.o.params.iter().rev().find(|(k, _)| k == n).map(|(_, v)| v.clone());
    let mut args = Vec::with_capacity(names.len());
    for n in &names {
        match param(n) {
            Some(v) => args.push(Some(v)),
            None => return Err(usage(format!("파라미터 값이 없음: {n} (--param {n}=값)"))),
        }
    }
    if let Some((k, _)) = env.o.params.iter().find(|(k, _)| !names.contains(k)) {
        return Err(usage(format!("문장에 없는 파라미터: {k}")));
    }
    Ok((sql, args))
}

/// describe·ddl 의 대상이다. -s 가 없으면 스키마.테이블 꼴을 나누고, 그것도 없으면 public 이다.
fn table_ref(o: &Opts) -> Result<TableRef, CmdError> {
    if o.args.len() != 1 {
        return Err(usage("테이블 이름 하나가 필요함"));
    }
    let arg = &o.args[0];
    let (schema, table) = if !o.schema.is_empty() {
        (o.schema.as_str(), arg.as_str())
    } else {
        arg.split_once('.').unwrap_or(("public", arg))
    };
    Ok(TableRef {
        schema: schema.into(),
        table: table.into(),
    })
}

// ─── Redis ───

async fn redis(env: &Env<'_>, stdin: Option<Box<dyn Read + Send>>) -> Result<Vec<CliResult>, CmdError> {
    let o = env.o;
    let c = pick_conn(env.proj, o, "-c", &o.conn, Kind::Redis)?;
    let mut args = o.args.clone();
    if args.len() <= 1 {
        args = split_args(read_input(env, stdin).await?.trim());
    }
    if args.first().is_some_and(|a| a.eq_ignore_ascii_case("KEYS")) {
        // KEYS 는 키 전체를 한 번에 훑어 그동안 서버가 다른 요청을 받지 못한다
        let pat = args.get(1).map_or("…", String::as_str);
        return Err(refused(format!(
            "KEYS 는 CLI 에서 하지 않음 (서버를 멈춤): SCAN 0 MATCH {pat} COUNT 1000 을 쓰세요"
        )));
    }
    if !o.write && !is_read_only_redis(&args) {
        return Err(refused("읽기 명령이 아님 (쓰기는 --write)"));
    }
    if o.write && c.env() == "prod" {
        return Err(refused("prod 쓰기는 CLI 에서 하지 않음 (화면에서 R6·R54 를 거침)"));
    }
    let danger = danger_of_redis(&args);
    if o.write && !o.force && !danger.is_empty() {
        return Err(refused(format!("위험: {danger} (--force)")));
    }
    let res = with_deadline(env.deadline, async {
        let r = env.clients.redis(c).await?;
        let mut r = r.lock().await;
        r.run(&args).await
    })
    .await?;
    Ok(vec![CliResult {
        conn: c.name.clone(),
        res: Some(res),
        ..Default::default()
    }])
}

// ─── 사용자 한눈에 보기 (R51) ───

/// 사람이 쓰기를 확인하지 않는 여러 접속 조회라 풀 연결의 읽기 전용 트랜잭션으로만 돈다. 조회마다 결과 한 칸이다.
async fn user360(env: &Env<'_>) -> Result<Vec<CliResult>, CmdError> {
    let (o, proj) = (env.o, env.proj);
    if !o.prod {
        return Err(refused("user360 은 prod 를 조회해 --prod 가 필요함"));
    }
    if o.args.is_empty() {
        return Err(usage("찾을 이메일·uid·id·닉네임이 필요함"));
    }
    let lookups = user360::load(&proj.dir).map_err(|e| Error::msg(format!("사용자 조회 설정 읽기 실패: {e}")))?;
    if lookups.is_empty() {
        return Err(Error::msg("사용자 조회 설정이 없음: .dbtui/user360.json").into());
    }
    let qs = user360::plan(&lookups, &o.args.join(" "), &|name, plain| {
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
        return Err(Error::msg("맞는 조회가 없음").into());
    }
    let mut jobs = Vec::new();
    let mut out: Vec<CliResult> = Vec::with_capacity(qs.len());
    for (i, q) in qs.iter().enumerate() {
        let mut r = CliResult {
            title: q.title.clone(),
            ..Default::default()
        };
        match (&q.err, proj.conn_by_name(&q.conn)) {
            (Some(e), _) => r.err = Some(e.clone()),
            (None, None) => r.err = Some(format!("접속 없음: {}", q.conn)),
            (None, Some(c)) if c.kind != Some(Kind::Pg) => r.err = Some(format!("PostgreSQL 접속이 아님: {}", q.conn)),
            (None, Some(c)) => {
                r.conn = c.name.clone();
                let (clients, c, sql, deadline) = (env.clients.clone(), c.clone(), inline_args(&q.sql, &q.args), env.deadline);
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
            // 오류 글은 run_cli 가 접속 비밀번호를 지워 찍는다
            Err(e) => out[i].err = Some(e.to_string()),
        }
    }
    Ok(out)
}

// ─── 스키마 비교 ───

async fn schema_diff(env: &Env<'_>) -> Result<Vec<CliResult>, CmdError> {
    let o = env.o;
    let a = pick_conn(env.proj, o, "-c", &o.conn, Kind::Pg)?;
    let b = pick_conn(env.proj, o, "--to", &o.to, Kind::Pg)?;
    let schema = or(&o.schema, "public");
    let snap = |c: &Conn, schema: String| {
        let (clients, c) = (env.clients.clone(), c.clone());
        with_deadline(env.deadline, async move {
            let pg = clients.pg(&c).await?;
            pg.schema_snapshot(&schema).await.map_err(|e| prefix_err(&c.name, e))
        })
    };
    let sa = snap(a, schema.to_string()).await?;
    let sb = snap(b, or(&o.to_schema, schema).to_string()).await?;
    Ok(vec![CliResult {
        conn: a.name.clone(),
        res: Some(diff_snapshots(&sa, &sb)),
        ..Default::default()
    }])
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

/// 칸 값이 든 다른 테이블을 FK 없이 이름 규칙으로 찾고, 같은 그룹·환경의 PostgreSQL 접속까지 센다.
/// 접속을 열기 전에 prod(--prod)·사용법을 거른다. 다른 접속은 환경이 같을 때만 들어 prod 는 --prod 아래에서만 닿는다.
async fn linked(env: &Env<'_>) -> Result<Vec<CliResult>, CmdError> {
    let o = env.o;
    let c = pick_conn(env.proj, o, "-c", &o.conn, Kind::Pg)?;
    if o.column.is_empty() || o.value.is_empty() {
        return Err(usage("--column 과 --value 가 필요함"));
    }
    let table = o.table.split_once('.').map_or("", |(_, t)| t);
    let base = linked_base(&o.column, or(table, &o.table));
    if o.column.eq_ignore_ascii_case("id") && base.is_empty() {
        return Err(usage("id 열은 --table 이 있어야 찾음"));
    }
    let conns: Vec<Conn> = linked_conns(env.proj, c).into_iter().cloned().collect();
    let found = find_linked(env, &conns, &o.column, &base, &o.value).await;
    let (mut res, summary, failed) = linked_table(&conns, &found, |i, e| env.err_text(&CmdError::Run(e.clone()), &[&conns[i].name]));
    res.text = summary;
    Ok(vec![CliResult {
        conn: c.name.clone(),
        res: Some(res),
        failed,
        ..Default::default()
    }])
}

/// 읽기 전용 접속인지다(이름의 -read, 호스트의 read.·.cluster-ro-).
fn is_read_conn(c: &Conn) -> bool {
    c.name.contains("-read") || c.host.starts_with("read.") || c.host.contains(".cluster-ro-")
}

/// conn 과 그룹·환경(R6.2)이 같은 PostgreSQL 접속이다. conn 이 맨 앞이고 그룹이 없으면 conn 하나다.
/// 같은 데이터베이스의 읽기·쓰기 접속 짝은 같은 데이터라 하나만 센다(R73.4): 읽기 접속을 빼되 conn 자신은 남기고,
/// conn 이 읽기 접속이면 짝인 쓰기 접속을 뺀다.
fn linked_conns<'a>(proj: &'a Project, cur: &'a Conn) -> Vec<&'a Conn> {
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
async fn find_linked(env: &Env<'_>, conns: &[Conn], col: &str, base: &str, value: &str) -> Vec<LinkedFound> {
    let deadline = env.deadline.min(Instant::now() + LINKED_TIMEOUT);
    let slots = Arc::new(tokio::sync::Semaphore::new(LINKED_CONN_WORKERS));
    let jobs = conns.iter().enumerate().map(|(i, c)| {
        let (clients, c, slots) = (env.clients.clone(), c.clone(), slots.clone());
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

// ─── 복호화 ───

/// --decrypt 일 때 staticEncrypt 꼴 칸을 그 접속 그룹 키로 푼다(R37). 풀지 못한 칸은 그대로 둔다.
pub fn decrypt_results(proj: &Project, results: &mut [CliResult], stderr: &mut String) -> Result<(), String> {
    for r in results.iter_mut() {
        let (None, Some(res)) = (&r.err, r.res.as_mut()) else { continue };
        let group = proj.conn_by_name(&r.conn).map(|c| c.group.clone()).unwrap_or_default();
        if group.is_empty() {
            return Err("접속 그룹을 몰라 복호화 키를 고를 수 없음".into());
        }
        let (key, _) = proj.aes_key_iv(proj.conn_by_name(&r.conn).expect("conn with group"));
        if key.is_empty() {
            return Err(format!("{}_AES_KEY 가 .env 에 없음", env_name(&group)));
        }
        let mut failed = 0;
        for v in res.rows.iter_mut().flatten() {
            if !statcrypt::looks_encrypted(v) {
                continue;
            }
            match statcrypt::decrypt(v, &key) {
                Ok(plain) => *v = plain,
                Err(_) => failed += 1,
            }
        }
        r.decrypt_failed = failed;
        if failed > 0 {
            stderr.push_str(&format!("{failed}칸 풀지 못함\n"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    const KEY: &str = "0123456789abcdef0123456789abcdef";
    const IV: &str = "00112233445566778899aabbccddeeff";

    fn project() -> Project {
        let mut c0 = conn("app-stg", "grp", "h", "app");
        c0.password = "sekrit-pw-9".into();
        Project {
            dir: Default::default(),
            conns: vec![c0, conn("nogroup-stg", "", "h", "app")],
            mapping: Default::default(),
            warnings: vec![],
            aes_env: [
                ("GRP_AES_KEY".to_string(), KEY.to_string()),
                ("GRP_AES_IV".to_string(), IV.to_string()),
            ]
            .into(),
        }
    }

    #[test]
    fn 오류_글에서_접속_비밀번호와_키를_지우고_제어문자를_바꾼다() {
        let proj = project();
        let o = Opts {
            timeout_ns: 30_000_000_000,
            ..Default::default()
        };
        let env = Env {
            o: &o,
            proj: &proj,
            clients: Arc::new(Clients::new()),
            deadline: Instant::now() + Duration::from_secs(60),
        };
        let err = CmdError::Run(Error::msg(format!(
            "auth failed for sekrit-pw-9 key={KEY} iv={IV} url=postgres://u:sekrit-pw-9@h\x1b[2J"
        )));
        assert_eq!(
            env.err_text(&err, &["app-stg"]),
            r"auth failed for *** key=*** iv=*** url=postgres://u:***@h\x1b[2J"
        );
        // 다른 접속의 비밀번호로는 가리지 않는다(짧은 로컬 비밀번호가 이름까지 가리지 않게)
        assert!(env.err_text(&err, &["nogroup-stg"]).contains("sekrit-pw-9"));
        assert_eq!(
            env.err_text(&CmdError::Run(Error::Deadline), &[]),
            "시간 초과(--timeout 30s): context deadline exceeded"
        );
    }

    #[test]
    fn decrypt_는_그룹_키로_풀고_못_푼_칸을_센다() {
        let mut proj = project();
        let enc = statcrypt::encrypt("a@b.com", KEY, IV).unwrap();
        let bad = format!("{IV}^|^{}", "00".repeat(16));
        let result = |conn: &str, rows: Vec<Vec<String>>| CliResult {
            conn: conn.into(),
            res: Some(QueryResult {
                cols: vec!["e".into(), "x".into()],
                rows,
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut rs = vec![result("app-stg", vec![vec![enc, "plain".into()], vec![bad.clone(), "NULL".into()]])];
        let mut errw = String::new();
        decrypt_results(&proj, &mut rs, &mut errw).unwrap();
        let rows = &rs[0].res.as_ref().unwrap().rows;
        assert_eq!(rows[0], ["a@b.com", "plain"]);
        assert_eq!(rows[1][0], bad);
        assert_eq!((rs[0].decrypt_failed, errw.as_str()), (1, "1칸 풀지 못함\n"));
        let err = decrypt_results(&proj, &mut [result("nogroup-stg", vec![])], &mut String::new()).unwrap_err();
        assert!(err.contains("접속 그룹을 몰라"), "{err}");
        proj.aes_env.clear();
        let err = decrypt_results(&proj, &mut [result("app-stg", vec![])], &mut String::new()).unwrap_err();
        assert_eq!(err, "GRP_AES_KEY 가 .env 에 없음");
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

    #[test]
    fn 테이블_이름은_스키마를_나누고_없으면_public() {
        let mut o = Opts {
            cmd: "describe".into(),
            ..Default::default()
        };
        o.args = vec!["golden.t".into()];
        assert_eq!(
            table_ref(&o).ok(),
            Some(TableRef {
                schema: "golden".into(),
                table: "t".into()
            })
        );
        o.args = vec!["t".into()];
        assert_eq!(table_ref(&o).ok().map(|t| t.schema), Some("public".into()));
        o.schema = "s".into();
        o.args = vec!["a.b".into()];
        assert_eq!(
            table_ref(&o).ok(),
            Some(TableRef {
                schema: "s".into(),
                table: "a.b".into()
            })
        );
    }
}
