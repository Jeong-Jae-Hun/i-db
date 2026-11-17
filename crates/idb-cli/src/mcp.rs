//! `db mcp`: 접속을 MCP(stdio) 도구로 내보낸다. Go `internal/mcpserver` 를 옮긴다. 모든 도구는 읽기 전용이다.
//! 도구는 Engine 위에서 돌아 `--remote 이름` 이면 원격 serve 의 접속을 쓴다. prod·종류·읽기 전용 검사는
//! Engine 을 부르기 전에 여기서 해 prod 에는 연결도 하지 않는다. 도구 이름·설명·인자 꼴은 Go 판이 내던
//! tools/list 를 그대로 담은 tools.json 이다(Claude 가 쓰던 부름이 그대로 듣게).

mod guard;
#[cfg(test)]
mod tests;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use idb_core::project::{Kind, go_quote};
use idb_core::result::MAX_ROWS;
use idb_core::sqlsafe::{check_read_only_sql, is_read_only_redis};
use idb_engine::{
    ConnSummary, Engine, EngineError, LocalEngine, Node, REASON_CONFIRM_EXPENSIVE, REASON_READ_ONLY, RemoteEngine, RemoteSpec, Request,
    ResultSet, ScanResult, SourceRunRequest,
};
use idb_sources::source::{MSG_EXPENSIVE, MSG_READ_ONLY_SOURCE};
use rmcp::ServiceExt;
use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorData, Implementation, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cli::{EXIT_ERR, EXIT_OK, EXIT_USAGE, PROJECT_USAGE, default_project_dir, load_project};
use crate::goflag::{FlagDef, FlagSet, FlagType, ParseError, Parsed};

const DEFAULT_MAX_ROWS: usize = 200;
const DEFAULT_KEYS: usize = 100;
// 응답 하나가 LLM 컨텍스트를 채우지 않도록 칸과 전체 크기를 db 의 화면용 상한(64KiB·1MiB)보다 작게 둔다
const MAX_CELL_BYTES: usize = 4 << 10;
const MAX_OUTPUT_BYTES: usize = 256 << 10;
const CLIP_MARK: &str = "…";
/// redis_scan 의 시간 상한이다. SCAN 은 패턴에 맞는 키가 드물면 키 공간 전체를 돌아서다
const SCAN_TIMEOUT: Duration = Duration::from_secs(10);
/// Go 판 tools/list 응답의 tools 다(이름 순)
const TOOLS_JSON: &str = include_str!("mcp/tools.json");
/// query 가 받는 종류다. aws 는 질의 언어가 없어 aws_tree·aws_open 으로 본다(R67.1)
const QUERY_KINDS: &[Kind] = &[
    Kind::Pg,
    Kind::OpenSearch,
    Kind::Loki,
    Kind::Prometheus,
    Kind::Grafana,
    Kind::DynamoDb,
    Kind::Athena,
];

// ─── 실행 ───

/// Engine 요청 하나를 보내는 함수다. 시험은 가짜를 넣는다.
pub(crate) type CallFn = Arc<dyn Fn(Request) -> Pin<Box<dyn Future<Output = Result<Value, EngineError>> + Send>> + Send + Sync>;

pub(crate) struct Options {
    /// false 면 env 가 prod 인 접속을 쓰는 도구를 모두 거부한다
    pub allow_prod: bool,
    /// 표 결과의 기본·최대 행 수. 0 이면 200, MAX_ROWS 를 넘으면 MAX_ROWS
    pub max_rows: usize,
    pub scan_timeout: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            allow_prod: false,
            max_rows: 0,
            scan_timeout: SCAN_TIMEOUT,
        }
    }
}

fn flag_set() -> FlagSet {
    let def = |name, ty, usage: &str| FlagDef {
        name,
        usage: usage.into(),
        ty,
        default: None,
    };
    let mut p = def("p", FlagType::Str, PROJECT_USAGE);
    p.default = Some(default_project_dir());
    FlagSet {
        defs: vec![
            p,
            def("allow-prod", FlagType::Bool, "allow tools on prod connections (read-only)"),
            def(
                "max-rows",
                FlagType::Int,
                "default and maximum rows per result (0 = server default)",
            ),
            def(
                "remote",
                FlagType::Str,
                "serve the connections of a remote db (~/.config/db/remote.json)",
            ),
        ],
        check: |_, _| Ok(()),
    }
}

/// `db mcp [-p 폴더] [-allow-prod] [-max-rows N] [--remote 이름]`. 표준 출력은 프로토콜 통로라 안내·오류는 stderr 로 보낸다.
pub fn run_mcp(args: &[String]) -> i32 {
    let fs = flag_set();
    let mut p = Parsed::default();
    let mut err = String::new();
    let usage = |out: &mut String| {
        out.push_str("Usage of db mcp:\n");
        out.push_str(&flag_set().defaults());
    };
    let pos = fs.parse_interleaved(&mut p, args, &mut err, &usage);
    eprint!("{err}");
    let pos = match pos {
        Ok(pos) => pos,
        Err(ParseError::Help) => return EXIT_OK,
        Err(ParseError::Failed) => return EXIT_USAGE,
    };
    if !pos.is_empty() {
        eprintln!("unexpected arguments: [{}]", pos.join(" "));
        return EXIT_USAGE;
    }
    let max_rows = p.ints.get("max-rows").copied().unwrap_or(0);
    if max_rows < 0 {
        eprintln!("max-rows must not be negative: {max_rows}");
        return EXIT_USAGE;
    }
    let opt = Options {
        allow_prod: p.bools.get("allow-prod").copied().unwrap_or(false),
        max_rows: max_rows as usize,
        ..Default::default()
    };
    let remote = p.strs.get("remote").cloned().unwrap_or_default();
    let dir = p.strs.get("p").cloned().unwrap_or_else(default_project_dir);
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("{e}");
            return EXIT_ERR;
        }
    };
    let res = rt.block_on(async {
        let engine = if remote.is_empty() {
            Engine::Local(Arc::new(LocalEngine::new(load_project(&dir)?, None)))
        } else {
            let spec = RemoteSpec::resolve(&remote)?;
            Engine::Remote(Arc::new(RemoteEngine::connect(&spec).await.map_err(|e| e.error)?))
        };
        let r = serve_stdio(engine.clone(), opt).await;
        engine.close().await;
        r
    });
    // stdin 읽기는 취소되지 않아 런타임을 기다리지 않고 끝낸다
    rt.shutdown_background();
    match res {
        Ok(()) => EXIT_OK,
        Err(e) => {
            eprintln!("{e}");
            EXIT_ERR
        }
    }
}

async fn serve_stdio(engine: Engine, opt: Options) -> Result<(), String> {
    let conns = engine.project().await.map_err(|e| e.error)?.conns;
    let call: CallFn = Arc::new(move |req| {
        let engine = engine.clone();
        Box::pin(async move { engine.call(req).await })
    });
    let server = McpServer::new(call, conns, opt)
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| e.to_string())?;
    // 끊긴 연결 오류는 클라이언트가 끝낸 것이라 성공으로 본다
    let _ = server.waiting().await;
    Ok(())
}

// ─── 서버 ───

pub(crate) struct McpServer {
    call: CallFn,
    conns: Vec<ConnSummary>,
    opt: Options,
    tools: Vec<Tool>,
}

impl McpServer {
    pub(crate) fn new(call: CallFn, conns: Vec<ConnSummary>, mut opt: Options) -> McpServer {
        if opt.max_rows == 0 {
            opt.max_rows = DEFAULT_MAX_ROWS;
        }
        opt.max_rows = opt.max_rows.min(MAX_ROWS);
        // 빌드에 넣은 파일이라 못 읽으면 시험이 먼저 깨진다
        let tools = serde_json::from_str(TOOLS_JSON).expect("mcp/tools.json");
        McpServer { call, conns, opt, tools }
    }

    /// 도구 하나를 돌린다. 도구 오류는 isError 결과이고, 인자가 스키마에 맞지 않으면 Err(invalid params)다(Go SDK 와 같다).
    pub(crate) async fn call_tool_json(&self, name: &str, args: Value) -> Result<CallToolResult, String> {
        let out = match name {
            "list_connections" => self.list_connections(args),
            "list_schemas" => self.list_schemas(args).await,
            "list_tables" => self.list_tables(args).await,
            "describe_table" => self.describe_table(args).await,
            "query" => self.query(args).await,
            "aws_tree" => self.aws_tree(args).await,
            "aws_open" => self.aws_open(args).await,
            "redis_scan" => self.redis_scan(args).await,
            "redis_get" => self.redis_get(args).await,
            "redis_command" => self.redis_command(args).await,
            _ => Err(ToolErr::Args(format!("unknown tool {}", go_quote(name)))),
        };
        match out {
            Ok(v) => Ok(CallToolResult::structured(v)),
            Err(ToolErr::Tool(e)) => Ok(CallToolResult::error(vec![ContentBlock::text(e)])),
            Err(ToolErr::Args(e)) => Err(e),
        }
    }

    async fn engine<T: DeserializeOwned>(&self, req: Request) -> Result<T, EngineError> {
        let v = (self.call)(req).await?;
        serde_json::from_value(v).map_err(|e| EngineError::run(format!("bad result: {e}")))
    }

    /// 이름으로 접속을 찾고 종류와 prod 허용을 확인한다. 접속을 열기 전에 거부해야 prod 에 연결 자체를 하지 않는다.
    fn lookup(&self, name: &str, kinds: &[Kind]) -> Result<&ConnSummary, String> {
        let Some(c) = self.conns.iter().find(|c| c.name == name) else {
            return Err(format!("unknown connection {}; call list_connections", go_quote(name)));
        };
        if !kinds.iter().any(|k| k.as_str() == c.kind) {
            let names: Vec<&str> = kinds.iter().map(|k| k.as_str()).collect();
            let mut err = format!("connection {} is {}, not {}", go_quote(name), c.kind, names.join("/"));
            if c.kind == Kind::Aws.as_str() {
                err.push_str("; browse aws connections with aws_tree and aws_open");
            }
            return Err(err);
        }
        if c.env == "prod" && !self.opt.allow_prod {
            return Err(format!(
                "connection {} is prod; this MCP server was started without prod access",
                go_quote(name)
            ));
        }
        Ok(c)
    }

    fn list_connections(&self, args: Value) -> ToolResult {
        parse::<NoArgs>(args)?;
        let connections = self
            .conns
            .iter()
            .map(|c| ConnInfo {
                name: c.name.clone(),
                group: if c.group.is_empty() { "기타".into() } else { c.group.clone() },
                kind: c.kind.clone(),
                env: c.env.clone(),
                region: c.region.clone(),
                profile: c.profile.clone(),
            })
            .collect();
        to_value(ListConnectionsOutput { connections })
    }

    async fn list_schemas(&self, args: Value) -> ToolResult {
        let a: ConnInput = parse(args)?;
        let c = self.lookup(&a.connection, &[Kind::Pg])?;
        let names: Vec<String> = self.engine(Request::Schemas { conn: c.name.clone() }).await.map_err(err_text)?;
        to_value(bound_names(&names, MAX_ROWS))
    }

    async fn list_tables(&self, args: Value) -> ToolResult {
        let a: SchemaInput = parse(args)?;
        let c = self.lookup(&a.connection, &[Kind::Pg])?;
        let req = Request::Tables {
            conn: c.name.clone(),
            schema: a.schema,
        };
        let names: Vec<String> = self.engine(req).await.map_err(err_text)?;
        to_value(bound_names(&names, MAX_ROWS))
    }

    async fn describe_table(&self, args: Value) -> ToolResult {
        let a: TableInput = parse(args)?;
        let c = self.lookup(&a.connection, &[Kind::Pg])?;
        let req = Request::Describe {
            conn: c.name.clone(),
            schema: a.schema,
            table: a.table,
        };
        let res: ResultSet = self.engine(req).await.map_err(err_text)?;
        to_value(bound_result(&res, MAX_ROWS))
    }

    async fn query(&self, args: Value) -> ToolResult {
        let a: QueryInput = parse(args)?;
        let c = self.lookup(&a.connection, QUERY_KINDS)?;
        let limit = match a.max_rows {
            Some(n) if n > 0 => (n as usize).min(self.opt.max_rows),
            _ => self.opt.max_rows,
        };
        if c.kind != Kind::Pg.as_str() {
            return self.query_source(c, a, limit).await;
        }
        // 접속을 열기 전에 거른다(prod 에는 연결도 하지 않는다). 놓친 쓰기는 serve 의 READ ONLY 트랜잭션이 막는다
        check_read_only_sql(&a.sql).map_err(|e| e.to_string())?;
        let req = Request::ReadQuery {
            conn: c.name.clone(),
            sql: a.sql,
            limit,
        };
        let res: ResultSet = self.engine(req).await.map_err(err_text)?;
        to_value(bound_result(&res, limit))
    }

    /// 저장소 질의다. 쓰기 거절·비싼 질의 확인은 serve 의 Backend 가 보내기 전에 한다(R60.2).
    /// MCP 는 사람에게 y/N 을 물을 수 없어 force 가 그 확인을 대신한다(CLI --force 와 같다).
    async fn query_source(&self, c: &ConnSummary, a: QueryInput, limit: usize) -> ToolResult {
        let text = a.sql.trim();
        if text.is_empty() {
            return Err("sql is empty".into());
        }
        let req = Request::SourceRun(SourceRunRequest {
            conn: c.name.clone(),
            text: text.into(),
            limit,
            force: a.force,
            ..Default::default()
        });
        let res: ResultSet = self.engine(req).await.map_err(|e| match e.reason.as_deref() {
            Some(REASON_CONFIRM_EXPENSIVE) => format!(
                "{}; nothing was sent. Retry with force=true only if the user accepts the cost",
                source_text(&e)
            ),
            _ => source_text(&e),
        })?;
        to_value(bound_result(&res, limit))
    }

    async fn aws_tree(&self, args: Value) -> ToolResult {
        let a: AwsPathInput = parse(args)?;
        let c = self.lookup(&a.connection, &[Kind::Aws])?;
        let req = Request::SourceChildren {
            conn: c.name.clone(),
            path: a.path,
        };
        let nodes: Vec<Node> = self.engine(req).await.map_err(|e| source_text(&e))?;
        let mut out = TreeOutput {
            nodes: Vec::new(),
            truncated: false,
        };
        let mut budget = MAX_OUTPUT_BYTES as isize;
        for (i, n) in nodes.iter().enumerate() {
            if i >= MAX_ROWS || budget <= 0 {
                out.truncated = true;
                break;
            }
            let t = TreeNode {
                name: clip(&n.name, MAX_CELL_BYTES),
                path: n.path.clone(),
                info: clip(&n.info, MAX_CELL_BYTES),
                warn: n.warn,
                has_children: n.has_children,
                openable: n.openable,
            };
            out.truncated = out.truncated || t.name != n.name || t.info != n.info;
            budget -= (t.name.len() + t.path.len() + t.info.len()) as isize;
            out.nodes.push(t);
        }
        to_value(out)
    }

    async fn aws_open(&self, args: Value) -> ToolResult {
        let a: AwsPathInput = parse(args)?;
        let c = self.lookup(&a.connection, &[Kind::Aws])?;
        let req = Request::SourceOpen {
            conn: c.name.clone(),
            path: a.path,
            limit: self.opt.max_rows,
        };
        let res: ResultSet = self.engine(req).await.map_err(|e| source_text(&e))?;
        to_value(bound_result(&res, self.opt.max_rows))
    }

    async fn redis_scan(&self, args: Value) -> ToolResult {
        let a: RedisScanInput = parse(args)?;
        let c = self.lookup(&a.connection, &[Kind::Redis])?;
        let pattern = if a.pattern.is_empty() { "*".to_string() } else { a.pattern };
        let req = Request::RedisScan {
            conn: c.name.clone(),
            pattern,
        };
        // 퓨처를 버리면 엔진이 요청을 취소한다(원격이면 cancel 을 보내고, 로컬이면 그 연결을 버린다)
        let res: ScanResult = match tokio::time::timeout(self.opt.scan_timeout, self.engine(req)).await {
            Ok(r) => r.map_err(err_text)?,
            Err(_) => {
                return Err(format!(
                    "redis_scan stopped after {}; use a narrower pattern",
                    go_duration(self.opt.scan_timeout)
                )
                .into());
            }
        };
        let limit = match a.limit {
            Some(n) if n > 0 => (n as usize).min(MAX_ROWS),
            _ => DEFAULT_KEYS,
        };
        let mut out = bound_names(&res.keys, limit);
        out.truncated = out.truncated || res.more;
        to_value(out)
    }

    async fn redis_get(&self, args: Value) -> ToolResult {
        let a: RedisGetInput = parse(args)?;
        let c = self.lookup(&a.connection, &[Kind::Redis])?;
        let req = Request::RedisValue {
            conn: c.name.clone(),
            key: a.key,
        };
        let res: ResultSet = self.engine(req).await.map_err(err_text)?;
        to_value(bound_result(&res, self.opt.max_rows))
    }

    async fn redis_command(&self, args: Value) -> ToolResult {
        let a: RedisCommandInput = parse(args)?;
        let args = a.args.unwrap_or_default();
        if !is_read_only_redis(&args) {
            return Err("only read-only Redis commands are allowed".into());
        }
        guard::check_redis_args(&args, self.opt.max_rows)?;
        let c = self.lookup(&a.connection, &[Kind::Redis])?;
        // 읽기 명령만 보내므로 serve 의 쓰기 확인(confirm_*)은 묻지 않는다
        let req = Request::RedisRun {
            conn: c.name.clone(),
            args,
            confirm_prod: false,
            confirm_danger: false,
        };
        let res: ResultSet = self.engine(req).await.map_err(err_text)?;
        to_value(bound_result(&res, self.opt.max_rows))
    }
}

impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().enable_tool_list_changed().build())
            .with_server_info(Implementation::new("db", "v2"))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(self.tools.clone()))
    }

    async fn call_tool(&self, request: CallToolRequestParams, _context: RequestContext<RoleServer>) -> Result<CallToolResponse, ErrorData> {
        if !self.tools.iter().any(|t| t.name == request.name) {
            return Err(ErrorData::invalid_params(format!("unknown tool {}", go_quote(&request.name)), None));
        }
        let args = Value::Object(request.arguments.unwrap_or_default());
        match self.call_tool_json(&request.name, args).await {
            Ok(r) => Ok(r.into()),
            Err(e) => Err(ErrorData::invalid_params(e, None)),
        }
    }
}

// ─── 인자·결과 꼴 (tools.json 의 inputSchema·outputSchema 와 같다) ───

/// 도구 실패다. Args 는 스키마에 맞지 않는 인자라 MCP 오류(-32602)로, Tool 은 isError 결과로 돌려준다.
enum ToolErr {
    Args(String),
    Tool(String),
}

impl From<String> for ToolErr {
    fn from(e: String) -> Self {
        ToolErr::Tool(e)
    }
}

impl From<&str> for ToolErr {
    fn from(e: &str) -> Self {
        ToolErr::Tool(e.into())
    }
}

type ToolResult = Result<Value, ToolErr>;

fn parse<T: DeserializeOwned>(args: Value) -> Result<T, ToolErr> {
    serde_json::from_value(args).map_err(|e| ToolErr::Args(format!("invalid params: {e}")))
}

fn to_value<T: Serialize>(v: T) -> ToolResult {
    serde_json::to_value(v).map_err(|e| ToolErr::Tool(e.to_string()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoArgs {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConnInput {
    connection: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SchemaInput {
    connection: String,
    schema: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TableInput {
    connection: String,
    schema: String,
    table: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QueryInput {
    connection: String,
    sql: String,
    #[serde(default)]
    max_rows: Option<i64>,
    #[serde(default)]
    force: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AwsPathInput {
    connection: String,
    #[serde(default)]
    path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RedisScanInput {
    connection: String,
    #[serde(default)]
    pattern: String,
    #[serde(default)]
    limit: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RedisGetInput {
    connection: String,
    key: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RedisCommandInput {
    connection: String,
    /// 스키마가 null 도 받는다(Go nil 슬라이스)
    args: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub(crate) struct ConnInfo {
    pub name: String,
    pub group: String,
    pub kind: String,
    pub env: String,
    // 리전·프로필은 비밀값이 아니다. AWS 자격 증명은 프로필로 받고 접속에 저장하지 않는다(R60.1)
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub region: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub profile: String,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct ListConnectionsOutput {
    pub connections: Vec<ConnInfo>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub(crate) struct NamesOutput {
    pub names: Vec<String>,
    pub truncated: bool,
}

#[derive(Serialize, Deserialize, Debug, Default, PartialEq)]
pub(crate) struct ResultOutput {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub columns: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rows: Vec<Vec<String>>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    pub truncated: bool,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub(crate) struct TreeNode {
    pub name: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub info: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub warn: bool,
    pub has_children: bool,
    pub openable: bool,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub(crate) struct TreeOutput {
    pub nodes: Vec<TreeNode>,
    pub truncated: bool,
}

// ─── 오류·자르기 ───

/// serve 가 이미 비밀번호·키를 가린 오류 글이다.
fn err_text(e: EngineError) -> String {
    e.error
}

/// 저장소 거절 글을 Go 판 MCP 글로 되돌린다. serve 는 CLI 글(한국어)로 보내서다. 요금 막음(R72)은 두 판이 같은 글이다.
fn source_text(e: &EngineError) -> String {
    match e.reason.as_deref() {
        Some(REASON_READ_ONLY) => e
            .error
            .replacen(MSG_READ_ONLY_SOURCE, "read-only source: write requests are not sent", 1),
        Some(REASON_CONFIRM_EXPENSIVE) => e.error.replacen(MSG_EXPENSIVE, "expensive query needs confirmation", 1),
        _ => e.error.clone(),
    }
}

/// Go time.Duration 의 글(10s·300ms)이다. 이 파일이 쓰는 값만 맞으면 된다.
fn go_duration(d: Duration) -> String {
    if d.subsec_millis() == 0 {
        format!("{}s", d.as_secs())
    } else {
        format!("{}ms", d.as_millis())
    }
}

/// 행 수·칸 크기·전체 크기를 MCP 상한에 맞춰 자른다.
pub(crate) fn bound_result(res: &ResultSet, max_rows: usize) -> ResultOutput {
    let mut out = ResultOutput {
        columns: res.cols.clone(),
        text: clip(&res.text, MAX_OUTPUT_BYTES),
        truncated: res.truncated,
        rows: Vec::new(),
    };
    let mut budget = (MAX_OUTPUT_BYTES - out.text.len().min(MAX_OUTPUT_BYTES)) as isize;
    for (i, row) in res.rows.iter().enumerate() {
        if i >= max_rows || budget <= 0 {
            out.truncated = true;
            break;
        }
        let mut r = Vec::with_capacity(row.len());
        for cell in row {
            let c = clip(cell, MAX_CELL_BYTES);
            out.truncated = out.truncated || c != *cell;
            budget -= c.len() as isize;
            r.push(c);
        }
        out.rows.push(r);
    }
    out.truncated = out.truncated || out.text != res.text;
    out
}

pub(crate) fn bound_names(names: &[String], limit: usize) -> NamesOutput {
    let mut out = NamesOutput {
        names: Vec::new(),
        truncated: false,
    };
    let mut budget = MAX_OUTPUT_BYTES as isize;
    for (i, n) in names.iter().enumerate() {
        if i >= limit || budget <= 0 {
            out.truncated = true;
            break;
        }
        let c = clip(n, MAX_CELL_BYTES);
        out.truncated = out.truncated || c != *n;
        budget -= c.len() as isize;
        out.names.push(c);
    }
    out
}

/// n 바이트를 넘으면 자르고 끝에 CLIP_MARK 를 붙인다. 바이트로 자른 끝의 깨진 UTF-8 조각은 버린다.
pub(crate) fn clip(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    format!("{}{CLIP_MARK}", &s[..s.floor_char_boundary(n)])
}
