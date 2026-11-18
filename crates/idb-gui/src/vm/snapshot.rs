//! 화면 사진 모드(개발용). `IDB_GUI_SNAPSHOT=<폴더>` 면 앱이 정해 둔 장면을 차례로 띄우고 자기 창을 PNG 로 찍은 뒤 끝난다.
//! 데이터는 네트워크 없이 늘 같게 나오도록 이 모듈의 가짜 serve(`i-db --snapshot-serve`)가 준다. 앱은 그것을
//! `DB_REMOTE_CMD` 로 띄워 원격 엔진처럼 붙으므로 화면 코드는 실제와 같은 길을 탄다. 사용자 프로젝트·접속에는 닿지 않는다.

use std::io::{BufRead as _, Write as _};

use idb_engine::{
    BoardResult, BoardSummary, Catalog, ConnSummary, EngineError, FlowBox, FlowResult, HealthInfo, Hello, LockState, Metric, Node,
    OutboxExhausted, PROTOCOL_VERSION, PanelOut, ProjectInfo, QueryFile, QueryText, RawValue, Request, ResultSet, ScanResult, SessionState,
    TablePage, TxState,
};
use serde_json::Value;

pub const ENV_DIR: &str = "IDB_GUI_SNAPSHOT";
pub const ENV_SCRIPT: &str = "IDB_GUI_SNAPSHOT_SCRIPT";
pub const SERVE_ARG: &str = "--snapshot-serve";
/// 가짜 serve 에 붙을 원격 이름
pub const REMOTE: &str = "snapshot";

// ─── 장면 ───

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scene {
    Picker,
    Tree,
    Sql,
    Table,
    Redis,
    Search,
    Logs,
    Dashboard,
    Settings,
    Shortcuts,
    Confirm,
}

impl Scene {
    pub const ALL: [Scene; 11] = [
        Scene::Picker,
        Scene::Tree,
        Scene::Sql,
        Scene::Table,
        Scene::Redis,
        Scene::Search,
        Scene::Logs,
        Scene::Dashboard,
        Scene::Settings,
        Scene::Shortcuts,
        Scene::Confirm,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Scene::Picker => "picker",
            Scene::Tree => "tree",
            Scene::Sql => "sql",
            Scene::Table => "table",
            Scene::Redis => "redis",
            Scene::Search => "search",
            Scene::Logs => "logs",
            Scene::Dashboard => "dashboard",
            Scene::Settings => "settings",
            Scene::Shortcuts => "shortcuts",
            Scene::Confirm => "confirm",
        }
    }
}

/// 찍을 한 장이다. 장면·테마·확대를 고른다.
#[derive(Clone, Debug, PartialEq)]
pub struct Shot {
    pub scene: Scene,
    pub dark: bool,
    pub zoom: f32,
}

impl Shot {
    pub fn file_name(&self, n: usize) -> String {
        format!(
            "{n:02}-{}-{}-{}.png",
            self.scene.name(),
            if self.dark { "dark" } else { "light" },
            (self.zoom * 100.0).round() as u32
        )
    }
}

/// 기본 순서: 모든 장면을 어둡게·밝게 100% 로, 접속 트리와 SQL 결과는 140% 로도 찍는다.
pub fn default_shots() -> Vec<Shot> {
    let mut out = Vec::new();
    for dark in [true, false] {
        out.extend(Scene::ALL.iter().map(|&scene| Shot { scene, dark, zoom: 1.0 }));
    }
    for scene in [Scene::Tree, Scene::Sql] {
        out.push(Shot {
            scene,
            dark: true,
            zoom: 1.4,
        });
    }
    out
}

/// 장면 글. 한 줄에 `장면 [dark|light] [확대%]`, `#` 뒤는 주석이다. 예: `sql light 140`.
pub fn parse_script(text: &str) -> Result<Vec<Shot>, String> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let name = parts.next().unwrap_or("");
        let scene = Scene::ALL
            .into_iter()
            .find(|s| s.name() == name)
            .ok_or_else(|| format!("{}행: 모르는 장면 {name}", i + 1))?;
        let mut shot = Shot {
            scene,
            dark: true,
            zoom: 1.0,
        };
        for p in parts {
            match p {
                "dark" => shot.dark = true,
                "light" => shot.dark = false,
                z => {
                    let pct: f32 = z.trim_end_matches('%').parse().map_err(|_| format!("{}행: 모르는 값 {z}", i + 1))?;
                    shot.zoom = pct / 100.0;
                }
            }
        }
        out.push(shot);
    }
    if out.is_empty() {
        return Err("장면이 없다".into());
    }
    Ok(out)
}

pub const ENV_DISPLAY: &str = "IDB_GUI_SNAPSHOT_DISPLAY";

/// 찍을 화면을 고른다. 정한 번호가 있으면 그것, 없으면 주 화면(메뉴 막대)이 아닌 첫 화면, 하나뿐이면 주 화면이다.
/// 사용자가 쓰는 주 화면을 가리지 않으려는 것이다.
pub fn pick_display(count: usize, primary: usize, chosen: Option<usize>) -> usize {
    if let Some(i) = chosen.filter(|&i| i < count) {
        return i;
    }
    (0..count).find(|&i| i != primary).unwrap_or(primary.min(count.saturating_sub(1)))
}

// ─── 가짜 serve ───

/// 표준 입출력으로 serve 프로토콜을 받아 늘 같은 답을 준다.
pub fn serve_stdio() {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let (id, req) = idb_engine::parse_request(&line);
        let res = req.and_then(|r| handle(&r));
        if writeln!(out, "{}", idb_engine::response_line(id, &res)).is_err() || out.flush().is_err() {
            break;
        }
    }
}

fn conn(name: &str, kind: &str, env: &str, group: &str, host: &str, db: &str, n: u8) -> ConnSummary {
    ConnSummary {
        name: name.into(),
        group: group.into(),
        kind: kind.into(),
        env: env.into(),
        host: host.into(),
        db: db.into(),
        source: "datagrip".into(),
        uuid: format!("00000000-0000-4000-8000-0000000000{n:02}"),
        ..Default::default()
    }
}

pub fn project() -> ProjectInfo {
    ProjectInfo {
        dir: "~/Projects/shop".into(),
        conns: vec![
            conn(
                "app-stg",
                "postgresql",
                "stg",
                "app",
                "app-stg.cluster-c7x2.ap-northeast-2.rds.amazonaws.com",
                "shop",
                1,
            ),
            conn(
                "app-prod",
                "postgresql",
                "prod",
                "app",
                "app-prod.cluster-c7x2.ap-northeast-2.rds.amazonaws.com",
                "shop",
                2,
            ),
            conn("billing-local", "postgresql", "local", "billing", "localhost:5432", "billing", 3),
            conn(
                "cache-stg",
                "redis",
                "stg",
                "cache",
                "cache-stg.x1y2.apn2.cache.amazonaws.com:6379",
                "",
                4,
            ),
            conn(
                "search-stg",
                "opensearch",
                "stg",
                "search",
                "search-stg.ap-northeast-2.es.amazonaws.com",
                "",
                5,
            ),
            conn("logs", "loki", "stg", "obs", "loki.internal:3100", "", 6),
        ],
        warnings: Vec::new(),
    }
}

fn to<T: serde::Serialize>(v: T) -> Result<Value, EngineError> {
    serde_json::to_value(v).map_err(|e| EngineError::run(e.to_string()))
}

fn table(name: &str, view: bool, cols: &[(&str, &str)]) -> Value {
    serde_json::json!({
        "name": name,
        "view": view,
        "columns": cols.iter().map(|(n, t)| serde_json::json!({"name": n, "type": t})).collect::<Vec<_>>(),
    })
}

fn catalog() -> Catalog {
    let id = ("id", "bigint");
    let at = ("created_at", "timestamptz");
    let v = serde_json::json!({
        "schemas": [
            {"name": "public", "tables": [
                table("users", false, &[id, ("email", "text"), ("name", "text"), ("status", "user_status"), at, ("last_login_at", "timestamptz"), ("deleted_at", "timestamptz")]),
                table("orders", false, &[id, ("user_id", "bigint"), ("status", "order_status"), ("total", "numeric(12,2)"), at]),
                table("order_items", false, &[id, ("order_id", "bigint"), ("product_id", "bigint"), ("qty", "integer"), ("price", "numeric(12,2)")]),
                table("products", false, &[id, ("sku", "text"), ("title", "text"), ("price", "numeric(12,2)"), ("stock", "integer")]),
                table("payments", false, &[id, ("order_id", "bigint"), ("method", "text"), ("amount", "numeric(12,2)"), ("paid_at", "timestamptz")]),
                table("coupons", false, &[id, ("code", "text"), ("discount", "integer"), ("expires_at", "timestamptz")]),
                table("outbox_events", false, &[id, ("topic", "text"), ("payload", "jsonb"), ("retry_count", "integer"), at]),
                table("active_users", true, &[id, ("email", "text"), ("last_login_at", "timestamptz")]),
            ]},
            {"name": "billing", "tables": [
                table("invoices", false, &[id, ("order_id", "bigint"), ("amount", "numeric(12,2)"), ("issued_at", "timestamptz")]),
                table("refunds", false, &[id, ("payment_id", "bigint"), ("amount", "numeric(12,2)"), ("reason", "text")]),
            ]},
            {"name": "audit", "tables": [
                table("events", false, &[id, ("actor", "text"), ("action", "text"), at]),
            ]},
        ],
        "functions": ["public.touch_updated_at", "public.order_total"],
    });
    serde_json::from_value(v).expect("canned catalog")
}

const NAMES: [&str; 12] = [
    "김민준",
    "이서연",
    "박도윤",
    "최하은",
    "정시우",
    "강지우",
    "조서준",
    "윤하린",
    "장예준",
    "임수아",
    "한지호",
    "오채원",
];
const STATUS: [&str; 4] = ["active", "active", "dormant", "active"];

/// 결과 표. NULL 칸이 섞여 있다.
fn users(n: usize) -> ResultSet {
    let rows = (0..n)
        .map(|i| {
            let id = 1001 + i;
            vec![
                id.to_string(),
                format!("user{id}@example.com"),
                NAMES[i % NAMES.len()].to_string(),
                STATUS[i % STATUS.len()].to_string(),
                format!("2026-0{}-{:02} 09:{:02}:00+09", 1 + i % 9, 1 + i % 28, i % 60),
                if i % 3 == 2 {
                    "NULL".into()
                } else {
                    format!("2026-11-{:02} 1{}:{:02}:00+09", 1 + i % 17, i % 10, (i * 7) % 60)
                },
                if i % 11 == 7 {
                    "2026-10-30 18:00:00+09".into()
                } else {
                    "NULL".into()
                },
            ]
        })
        .collect();
    ResultSet {
        cols: ["id", "email", "name", "status", "created_at", "last_login_at", "deleted_at"]
            .map(String::from)
            .to_vec(),
        types: Some(
            ["int8", "text", "text", "user_status", "timestamptz", "timestamptz", "timestamptz"]
                .map(String::from)
                .to_vec(),
        ),
        rows,
        text: format!("SELECT {n}"),
        elapsed_ms: 14,
        in_tx: Some(false),
        ..Default::default()
    }
}

fn rs(cols: &[&str], rows: Vec<Vec<String>>) -> ResultSet {
    ResultSet {
        cols: cols.iter().map(|c| c.to_string()).collect(),
        text: format!("{} rows", rows.len()),
        rows,
        elapsed_ms: 31,
        ..Default::default()
    }
}

fn redis_keys() -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for id in [1001, 1002, 1007, 1013, 1042] {
        keys.push(format!("user:{id}:profile"));
        keys.push(format!("user:{id}:cart"));
    }
    for s in ["7f3a9c", "a81d02", "c4e5f6"] {
        keys.push(format!("session:{s}"));
    }
    for k in [
        "rate:api:login",
        "rate:api:search",
        "feature:flags",
        "config:checkout",
        "queue:mail:pending",
        "lock:order:88231",
    ] {
        keys.push(k.into());
    }
    keys.sort();
    keys
}

fn node(name: &str, path: &str, info: &str, has_children: bool, openable: bool, warn: bool) -> Node {
    Node {
        name: name.into(),
        path: path.into(),
        info: info.into(),
        has_children,
        openable,
        warn,
    }
}

fn source_children(conn: &str, path: &str) -> Vec<Node> {
    match (conn, path) {
        ("search-stg", "") => vec![
            node("orders-2026.11", "orders-2026.11", "1.2M docs · 3.4GB", true, true, false),
            node("orders-2026.10", "orders-2026.10", "1.9M docs · 5.1GB", true, true, false),
            node("products", "products", "48k docs · 210MB", true, true, false),
            node("users-search", "users-search", "312k docs · 980MB", true, true, false),
            node(".kibana_1", ".kibana_1", "92 docs", true, true, false),
        ],
        ("search-stg", idx) => [
            "order_id:long",
            "user_id:long",
            "status:keyword",
            "amount:double",
            "@timestamp:date",
            "items:nested",
        ]
        .iter()
        .map(|f| {
            let (n, t) = f.split_once(':').unwrap_or((f, ""));
            node(n, &format!("{idx}/{n}"), t, false, false, false)
        })
        .collect(),
        ("logs", "") => vec![
            node("app", "app", "6 values", true, false, false),
            node("env", "env", "3 values", true, false, false),
            node("level", "level", "4 values", true, false, false),
        ],
        ("logs", "app") => ["api", "worker", "web", "batch", "gateway", "admin"]
            .iter()
            .map(|v| node(v, &format!("app/{v}"), "", false, true, false))
            .collect(),
        ("logs", label) => ["error", "warn", "info", "debug"]
            .iter()
            .map(|v| node(v, &format!("{label}/{v}"), "", false, true, *v == "error"))
            .collect(),
        _ => Vec::new(),
    }
}

fn source_rows(conn: &str) -> ResultSet {
    if conn == "logs" {
        let lines = [
            ("info", "GET /api/orders/88231 200 23ms"),
            ("warn", "slow query: SELECT * FROM orders WHERE user_id = $1 (812ms)"),
            ("info", "POST /api/checkout 201 141ms"),
            ("error", "payment gateway timeout after 5000ms (order_id=88229)"),
            ("info", "GET /api/products?page=2 200 11ms"),
            ("info", "worker: outbox order-created published 12 events"),
            ("debug", "cache hit user:1042:profile"),
            ("info", "GET /healthz 200 1ms"),
        ];
        let rows = (0..24)
            .map(|i| {
                let (lvl, msg) = lines[i % lines.len()];
                vec![
                    format!("2026-11-18 14:{:02}:{:02}.{:03}", 59 - i / 4, (i * 13) % 60, (i * 97) % 1000),
                    format!("app=api level={lvl}"),
                    msg.to_string(),
                ]
            })
            .collect();
        return rs(&["time", "labels", "line"], rows);
    }
    let st = ["paid", "shipped", "pending", "cancelled", "paid"];
    let rows = (0..30)
        .map(|i| {
            vec![
                format!("Yk{:06}x", 381_000 + i * 17),
                (88_200 + i).to_string(),
                (1001 + i % 13).to_string(),
                st[i % st.len()].to_string(),
                format!("{}.{:02}", 12_000 + i * 731 % 90_000, i * 7 % 100),
                format!("2026-11-18T0{}:{:02}:00Z", i % 10, (i * 11) % 60),
            ]
        })
        .collect();
    rs(&["_id", "order_id", "user_id", "status", "amount", "@timestamp"], rows)
}

fn health(conn: &str) -> HealthInfo {
    let m = |n: &str, v: &str, w: bool| Metric {
        name: n.into(),
        value: v.into(),
        warn: w,
    };
    let (version, metrics) = match conn {
        "cache-stg" => (
            "Redis 7.2.4",
            vec![m("memory", "1.21GB", false), m("keys", "482,113", false), m("hit", "97.8%", false)],
        ),
        "search-stg" => (
            "OpenSearch 2.13",
            vec![m("", "green", false), m("docs", "3.4M", false), m("heap", "61%", false)],
        ),
        "logs" => ("Loki 3.1", vec![m("ingest", "2.3MB/s", false)]),
        "app-prod" => (
            "PostgreSQL 16.4",
            vec![m("conns", "312/500", true), m("cache hit", "99.4%", false), m("lag", "0s", false)],
        ),
        _ => (
            "PostgreSQL 16.4",
            vec![m("conns", "42/500", false), m("cache hit", "99.1%", false), m("lag", "0s", false)],
        ),
    };
    HealthInfo {
        ok: true,
        latency_ms: 18 + conn.len() as u64 * 3,
        version: version.into(),
        metrics,
        summary: String::new(),
        err: String::new(),
    }
}

fn boxes() -> Vec<FlowBox> {
    let b = |stage: &str, name: &str, metrics: &str, warn: bool| FlowBox {
        stage: stage.into(),
        name: name.into(),
        metrics: metrics.into(),
        warn,
        ..Default::default()
    };
    vec![
        b("db", "outbox_events", "대기 3 · 소진 0", false),
        b("sns", "order-created", "발행 1.2k/h", false),
        b("sqs", "order-created-mail", "대기 0 · DLQ 2", true),
        b("lambda", "send-order-mail", "오류 0.4%", false),
    ]
}

/// 요청 하나에 답한다. 모르는 요청은 code 2 다.
pub fn handle(req: &Request) -> Result<Value, EngineError> {
    match req {
        Request::Hello { .. } => to(Hello {
            version: PROTOCOL_VERSION,
            server: "db".into(),
        }),
        Request::Project => to(project()),
        Request::Catalog { conn } if is_pg(conn) => to(catalog()),
        Request::Schemas { conn } if is_pg(conn) => to(catalog().schemas.into_iter().map(|s| s.name).collect::<Vec<_>>()),
        Request::Databases { conn } if is_pg(conn) => to(vec!["shop", "postgres"]),
        Request::SwitchDb { conn, db } => to(SessionState {
            conn: conn.clone(),
            db: if db.is_empty() { "shop".into() } else { db.clone() },
            search_path: "public".into(),
            in_tx: false,
        }),
        Request::SetSearchPath { conn, schema } => to(SessionState {
            conn: conn.clone(),
            db: "shop".into(),
            search_path: schema.clone(),
            in_tx: false,
        }),
        Request::TxState { .. } => to(TxState::default()),
        Request::ProdLock { conn, .. } => to(LockState {
            conn: conn.clone(),
            prod: conn == "app-prod",
            locked: false,
            remaining_secs: 540,
            unlock_min: 10,
        }),
        Request::Run(r) => {
            let first = r.sql.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
            let write = matches!(first.as_str(), "update" | "delete" | "insert");
            if write && r.conn == "app-prod" && !r.confirm_prod {
                return Err(EngineError::refused("confirm_prod", "prod(app-prod)에 쓰기 실행: y/N"));
            }
            if write {
                return to(ResultSet {
                    text: "UPDATE 1".into(),
                    elapsed_ms: 6,
                    ..Default::default()
                });
            }
            to(users(42))
        }
        Request::TablePage(p) => to(TablePage {
            result: users(60),
            sql: format!("SELECT * FROM {}.{} ORDER BY id LIMIT 200", p.schema, p.table),
            count: Some(1284),
            estimate: Some(1290),
        }),
        Request::RedisScan { .. } => to(ScanResult {
            keys: redis_keys(),
            more: false,
        }),
        Request::RedisValue { key, .. } => to(ResultSet {
            cols: vec!["field".into(), "value".into()],
            rows: profile().into_iter().map(|[f, v]| vec![f, v]).collect(),
            text: format!("{key} · hash · 6 fields · TTL 3598s"),
            ..Default::default()
        }),
        Request::RedisRawValue { key, .. } => to(RawValue {
            key: key.clone(),
            typ: "hash".into(),
            ttl_secs: 3598,
            fields: profile(),
            ..Default::default()
        }),
        Request::SourceChildren { conn, path } => to(source_children(conn, path)),
        Request::SourceOpen { conn, .. } => to(source_rows(conn)),
        Request::SourceRun(r) => to(source_rows(&r.conn)),
        Request::Health { conn } => to(health(conn)),
        Request::OutboxExhausted => to(vec![
            OutboxExhausted {
                flow: "order-created".into(),
                conn: "app-stg".into(),
                exhausted: 0,
            },
            OutboxExhausted {
                flow: "payment-settled".into(),
                conn: "app-prod".into(),
                exhausted: 3,
            },
        ]),
        Request::Flows { .. } => to(vec![FlowResult {
            flow: "order-created".into(),
            boxes: boxes(),
            opens: Vec::new(),
        }]),
        Request::BoardList => to(vec![BoardSummary {
            name: "ops".into(),
            title: "운영 보드".into(),
            panels: 3,
            error: String::new(),
        }]),
        Request::BoardRun { .. } => to(BoardResult {
            title: "운영 보드".into(),
            error: String::new(),
            panels: vec![
                PanelOut {
                    title: "오늘 주문".into(),
                    conn: "app-stg".into(),
                    view: "number".into(),
                    value: Some("1,284".into()),
                    ..Default::default()
                },
                PanelOut {
                    title: "결제 실패(1h)".into(),
                    conn: "app-stg".into(),
                    view: "number".into(),
                    value: Some("7".into()),
                    warn: true,
                    ..Default::default()
                },
                PanelOut {
                    title: "주문 흐름".into(),
                    flow: "order-created".into(),
                    view: "flow".into(),
                    boxes: boxes(),
                    ..Default::default()
                },
            ],
            every_secs: 0,
        }),
        Request::QueryList => to(vec![
            qf("queries/app/daily-orders.sql", "app-stg", 5),
            qf("queries/app/dormant-users.sql", "app-stg", 4),
            qf("queries/app/refund-check.sql", "app-prod", 3),
            qf("queries/billing/invoice-gap.sql", "billing-local", 2),
            qf("queries/scratch.sql", "", 1),
        ]),
        Request::QueryRead { .. } => to(QueryText {
            content: "select id, email, status\nfrom users\nwhere status = 'dormant'\norder by id;\n".into(),
            hash: String::new(),
        }),
        _ => Err(EngineError::usage("snapshot serve: 이 화면 사진 모드에서는 쓰지 않는 요청")),
    }
}

fn is_pg(conn: &str) -> bool {
    project().conns.iter().any(|c| c.name == conn && c.kind == "postgresql")
}

fn qf(path: &str, conn: &str, age: i64) -> QueryFile {
    QueryFile {
        path: path.into(),
        conn: conn.into(),
        mtime: 1_795_000_000 - age * 3600,
        size: 120,
        ..Default::default()
    }
}

fn profile() -> Vec<[String; 2]> {
    [
        ("name", "이서연"),
        ("email", "user1002@example.com"),
        ("tier", "gold"),
        ("points", "12850"),
        ("last_seen", "2026-11-18T05:12:44Z"),
        ("prefs", r#"{"lang":"ko","push":true}"#),
    ]
    .map(|(a, b)| [a.to_string(), b.to_string()])
    .to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok<T: serde::de::DeserializeOwned>(req: Request) -> T {
        serde_json::from_value(handle(&req).expect("ok")).expect("typed")
    }

    #[test]
    fn 가짜_serve_답은_화면이_읽는_타입으로_풀린다() {
        let p: ProjectInfo = ok(Request::Project);
        assert!(p.conns.iter().any(|c| c.kind == "redis"));
        let c: Catalog = ok(Request::Catalog { conn: "app-stg".into() });
        assert!(c.schemas[0].tables.iter().any(|t| t.name == "users"));
        let r: ResultSet = ok(Request::Run(idb_engine::RunRequest {
            conn: "app-stg".into(),
            sql: "select * from users".into(),
            ..Default::default()
        }));
        assert!(r.rows.iter().flatten().any(|v| v == "NULL"), "NULL 칸이 있다");
        let _: ScanResult = ok(Request::RedisScan {
            conn: "cache-stg".into(),
            pattern: "*".into(),
        });
        let _: RawValue = ok(Request::RedisRawValue {
            conn: "cache-stg".into(),
            key: "user:1002:profile".into(),
        });
        let n: Vec<Node> = ok(Request::SourceChildren {
            conn: "logs".into(),
            path: String::new(),
        });
        assert!(!n.is_empty());
        let _: HealthInfo = ok(Request::Health { conn: "app-prod".into() });
        let _: BoardResult = ok(Request::BoardRun {
            name: "ops".into(),
            prod: false,
        });
        let _: Vec<QueryFile> = ok(Request::QueryList);
        assert!(handle(&Request::Catalog { conn: "cache-stg".into() }).is_err());
    }

    #[test]
    fn prod_쓰기는_확인을_묻는다() {
        let e = handle(&Request::Run(idb_engine::RunRequest {
            conn: "app-prod".into(),
            sql: "update users set status = 'active' where id = 1001".into(),
            ..Default::default()
        }))
        .unwrap_err();
        assert_eq!(e.reason.as_deref(), Some("confirm_prod"));
    }

    #[test]
    fn 찍을_화면은_주_화면이_아닌_곳을_고른다() {
        assert_eq!(pick_display(3, 0, None), 1);
        assert_eq!(pick_display(3, 1, None), 0);
        assert_eq!(pick_display(1, 0, None), 0, "하나뿐이면 주 화면");
        assert_eq!(pick_display(3, 0, Some(2)), 2);
        assert_eq!(pick_display(3, 0, Some(9)), 1, "없는 번호는 무시");
    }

    #[test]
    fn 장면_글을_읽는다() {
        let s = parse_script("# 주석\nsql light 140\n\ntree\n").unwrap();
        assert_eq!(
            s,
            vec![
                Shot {
                    scene: Scene::Sql,
                    dark: false,
                    zoom: 1.4
                },
                Shot {
                    scene: Scene::Tree,
                    dark: true,
                    zoom: 1.0
                },
            ]
        );
        assert!(parse_script("nope").is_err());
        assert!(parse_script("").is_err());
        assert_eq!(s[0].file_name(3), "03-sql-light-140.png");
        assert_eq!(default_shots().len(), Scene::ALL.len() * 2 + 2);
    }
}
