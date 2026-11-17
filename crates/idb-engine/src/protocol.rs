//! serve 프로토콜의 요청·응답 모양. 줄마다 JSON 하나다(docs/serve-protocol.md).
//! 요청은 `{"id":N,"op":"…",…}`, 응답은 `{"id":N,"ok":true,"result":…}` 또는 `{"id":N,"ok":false,"code":3,"error":"…"}` 이다.
//! 어느 메시지에도 비밀번호·키를 싣지 않는다. 오류 글은 서버가 이미 가린 것이다.

use std::collections::BTreeMap;
use std::fmt;

use idb_core::result::QueryResult;
pub use idb_sources::health::Metric;
pub use idb_sources::source::Node;
pub use idb_sources::{Applied, Catalog, Change, ChangeOp, ChangeSet, Fk, Object, Origin, TableRef};

pub use crate::flows::FlowBox;

pub use crate::redis::{PatternKeys, RawValue, RedisEdit};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 1;

/// 실행 오류(CLI 종료 코드 1)
pub const CODE_RUN: i32 = 1;
/// 요청·사용법 오류(2)
pub const CODE_USAGE: i32 = 2;
/// 안전장치 거절(3)
pub const CODE_REFUSED: i32 = 3;

/// code 3 의 까닭이다. 화면은 이 값으로 확인 창을 띄우거나 잠금 안내를 보인다.
pub const REASON_PROD_LOCKED: &str = "prod_locked";
pub const REASON_CONFIRM_PROD: &str = "confirm_prod";
pub const REASON_CONFIRM_DANGER: &str = "confirm_danger";
/// 환경과 상관없이 늘 묻는 확인(백엔드 신호 R22.1, 패턴 일괄 삭제 R42). `confirm:true` 로 다시 보낸다
pub const REASON_CONFIRM: &str = "confirm";
/// 읽기 전용이 아닌 문장의 EXPLAIN ANALYZE 는 문장을 실제로 실행해 환경과 상관없이 묻는다(R9.2). `confirm_analyze:true` 로 다시 보낸다
pub const REASON_CONFIRM_ANALYZE: &str = "confirm_analyze";
/// 읽기 전용 저장소에 쓰기로 보이는 질의다(R60). 다시 보낼 길이 없다
pub const REASON_READ_ONLY: &str = "read_only";
/// Redis KEYS 다. 서버를 멈춰 확인으로도 보내지 않는다(SCAN 을 쓴다)
pub const REASON_KEYS: &str = "keys";
/// 전체 스캔·큰 범위 같은 비싼 저장소 질의다(R60.2). 확인 후 `force:true` 로 다시 보낸다
pub const REASON_CONFIRM_EXPENSIVE: &str = "confirm_expensive";
/// 요청마다 요금이 붙는 AWS 조회라 serve 쪽 DB_ALLOW_BILLED=1 없이는 막는다(R72). 확인으로 풀리지 않는다
pub const REASON_BILLED: &str = "billed";
/// serve(원격)에서는 접속 파일을 고치지 않고 비밀번호를 받지 않는다(R59). 확인으로 풀리지 않는다
pub const REASON_REMOTE_CONN_EDIT: &str = "remote_conn_edit";

/// 요청 하나다. op 이름은 snake_case 다. 모르는 필드는 무시해 새 필드를 더해도 옛 서버가 받는다.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// 첫 요청이어야 한다. 판이 다르면 code 2 다
    Hello {
        version: u32,
    },
    /// 진행 중인 요청 target 을 멈춘다. target 은 code 1 `canceled` 로 끝난다
    Cancel {
        target: u64,
    },
    /// 프로젝트와 접속 목록(비밀번호 없음)
    Project,
    Schemas {
        conn: String,
    },
    Tables {
        conn: String,
        #[serde(default)]
        schema: String,
    },
    Describe {
        conn: String,
        #[serde(default)]
        schema: String,
        table: String,
    },
    Ddl {
        conn: String,
        #[serde(default)]
        schema: String,
        table: String,
    },
    Search {
        conn: String,
        #[serde(default)]
        schema: String,
        text: String,
    },
    /// 편집기 세션에서 문장 하나(R3·R6·R25·R52·R54)
    Run(RunRequest),
    /// EXPLAIN 으로 감싸 편집기 세션에서 실행한다. ANALYZE 쓰기 문장은 Run 과 같은 확인을 거친다
    Explain {
        #[serde(flatten)]
        run: RunRequest,
        #[serde(default)]
        analyze: bool,
        #[serde(default)]
        buffers: bool,
        /// TEXT·JSON·YAML·XML, 비면 TEXT
        #[serde(default)]
        format: String,
    },
    /// 테이블 보기 한 페이지(R16)
    TablePage(TablePageRequest),
    TxBegin {
        conn: String,
    },
    TxCommit {
        conn: String,
        #[serde(default)]
        confirm_prod: bool,
    },
    TxRollback {
        conn: String,
    },
    TxState {
        conn: String,
    },
    /// prod 쓰기 잠금 상태(R54). unlock 이 true 면 열고 false 면 잠그고, 없으면 상태만 본다
    ProdLock {
        conn: String,
        #[serde(default)]
        unlock: Option<bool>,
    },
    RedisScan {
        conn: String,
        #[serde(default)]
        pattern: String,
    },
    RedisValue {
        conn: String,
        key: String,
    },
    /// Redis 세션에서 명령 하나. 값 편집도 이 명령으로 보낸다
    RedisRun {
        conn: String,
        args: Vec<String>,
        #[serde(default)]
        confirm_prod: bool,
        #[serde(default)]
        confirm_danger: bool,
    },
    /// CLI 하위 명령 하나를 서버에서 돌린다(`db <명령> --remote`)
    Cli(CliRequest),

    // ─── 판 1 에 더한 op (모두 더하기만 했다) ───
    /// 같은 서버의 접속할 수 있는 데이터베이스(R27.1)
    Databases {
        conn: String,
    },
    /// 이 접속을 다른 데이터베이스로 다시 연다. 빈 db 는 프로젝트의 DB 로 돌아간다. 열린 트랜잭션이 있으면 거절한다
    SwitchDb {
        conn: String,
        #[serde(default)]
        db: String,
    },
    /// 편집기 세션에 `SET search_path TO "<schema>", public`(R27.2)
    SetSearchPath {
        conn: String,
        schema: String,
    },
    /// 운영 화면(R22.1)
    Activity {
        conn: String,
    },
    Locks {
        conn: String,
    },
    TableSizes {
        conn: String,
    },
    /// pid 의 쿼리 취소(terminate 면 연결 종료). 늘 confirm 을 요구하고 prod 는 잠금·confirm_prod 도 거친다
    SignalBackend {
        conn: String,
        pid: i64,
        #[serde(default)]
        terminate: bool,
        #[serde(default)]
        confirm: bool,
        #[serde(default)]
        confirm_prod: bool,
    },
    /// t 가 가리키는 FK, incoming 이면 t 를 가리키는 FK(R36)
    ForeignKeys {
        conn: String,
        schema: String,
        table: String,
        #[serde(default)]
        incoming: bool,
    },
    /// 이름을 테이블·뷰·함수로 찾는다(R26)
    Resolve {
        conn: String,
        name: String,
    },
    FunctionDef {
        conn: String,
        schema: String,
        name: String,
    },
    /// 자동완성·찾기 목록(R8·R22.3·R56)
    Catalog {
        conn: String,
    },
    EnumLabels {
        conn: String,
        types: Vec<String>,
    },
    /// UPDATE·DELETE 하나의 예상 행 수(R39, 실행하지 않음)
    EstimateRows {
        conn: String,
        sql: String,
        #[serde(default)]
        params: BTreeMap<String, String>,
    },
    /// 결과 표 편집 반영(R15.2)
    ApplyChanges {
        conn: String,
        changes: ChangeSet,
        #[serde(default)]
        confirm_prod: bool,
    },
    /// 반영한 묶음 되돌리기(R50). applied 는 apply_changes 가 돌려준 것이다
    UndoChanges {
        conn: String,
        changes: ChangeSet,
        applied: Applied,
        #[serde(default)]
        confirm_prod: bool,
    },
    /// 묶음의 미리 보기 문장. applied 가 있으면 되돌리기 묶음과 그 문장이다
    ChangesSql {
        changes: ChangeSet,
        #[serde(default)]
        applied: Option<Applied>,
    },
    /// CSV 가져오기(COPY). 파일은 클라이언트가 읽어 행으로 보낸다
    ImportCsv {
        conn: String,
        schema: String,
        table: String,
        columns: Vec<String>,
        rows: Vec<Vec<Option<String>>>,
        #[serde(default)]
        confirm_prod: bool,
    },
    Linked {
        conn: String,
        column: String,
        value: String,
        #[serde(default)]
        table: String,
    },
    SchemaDiff {
        conn: String,
        to: String,
        #[serde(default)]
        schema: String,
        #[serde(default)]
        to_schema: String,
    },
    User360 {
        query: String,
    },
    /// 값 편집기용 잘리지 않은 값
    RedisRawValue {
        conn: String,
        key: String,
    },
    /// 값 편집 하나를 명령으로 만들어 세션에서 차례로 실행한다
    RedisEdit {
        conn: String,
        edit: RedisEdit,
        #[serde(default)]
        confirm_prod: bool,
        #[serde(default)]
        confirm_danger: bool,
    },
    /// Redis 모니터(R34.1)
    RedisInfo {
        conn: String,
    },
    RedisSlowlog {
        conn: String,
        #[serde(default)]
        n: i64,
    },
    RedisBigKeys {
        conn: String,
        #[serde(default)]
        sample: usize,
        #[serde(default)]
        top: usize,
    },
    /// 패턴 일괄 삭제(R42) 1단계: 맞는 키(100,000개까지)
    RedisPatternKeys {
        conn: String,
        pattern: String,
    },
    /// 2단계: 센 키만 UNLINK 한다. 늘 confirm, prod 는 잠금·confirm_prod 도
    RedisUnlink {
        conn: String,
        keys: Vec<String>,
        #[serde(default)]
        confirm: bool,
        #[serde(default)]
        confirm_prod: bool,
    },
    // ─── 저장소(R60~R68): OpenSearch·Loki·Prometheus·Grafana·DynamoDB·Athena·aws ───
    /// 트리 한 단계. path "" 는 맨 위다
    SourceChildren {
        conn: String,
        #[serde(default)]
        path: String,
    },
    /// 트리 Enter 미리 보기(Loki 100줄, 나머지 50행)
    SourceOpen {
        conn: String,
        path: String,
    },
    /// 편집기 질의. logs·metrics 범위는 since_ns·until_ns 다
    SourceRun(SourceRunRequest),
    /// 대시보드 확인 한 번(R64.2). 모든 종류
    Health {
        conn: String,
    },
    /// 흐름 풀이(R69). names 가 비면 모두. prod 가 거짓이면 prod 접속은 묻지 않는다
    Flows {
        #[serde(default)]
        names: Vec<String>,
        #[serde(default)]
        prod: bool,
    },
    /// .dbtui/boards 의 보드 목록
    BoardList,
    /// 보드 하나를 한 번 돌린다(R70). prod 가 거짓이면 prod 패널은 묻지 않는다
    BoardRun {
        name: String,
        #[serde(default)]
        prod: bool,
    },

    // ─── 복호화·전체 내보내기·접속 편집 ───
    /// staticEncrypt 꼴(`<hex>^|^<hex>`) 칸을 그 접속 그룹 키로 serve 안에서 푼다(R37). 키는 serve 밖으로 나가지 않는다
    Decrypt {
        conn: String,
        cells: Vec<String>,
    },
    /// 읽기 문장 하나를 COPY 로 전체 내보내기 시작(R33.1). 조각은 export_read 로 받는다
    ExportStart {
        conn: String,
        sql: String,
    },
    /// 다음 조각. 조각이 올 때까지 기다리고, 끝이면 done 이다. 끝·오류 뒤 그 id 는 사라진다
    ExportRead {
        export: u64,
    },
    ExportCancel {
        export: u64,
    },
    /// db 전용 접속의 고칠 값(비밀번호 없음, R59.3)
    ConnGet {
        name: String,
    },
    /// 저장하지 않고 접속을 시험한다(R59.2 Ctrl+T). conn 이 없으면 name 의 접속을 그대로,
    /// conn 의 비밀번호가 비면 old_name(없으면 conn.name)의 저장된 비밀번호로 시험한다
    ConnTest {
        #[serde(default)]
        name: String,
        #[serde(default)]
        conn: Option<ConnFields>,
        #[serde(default)]
        old_name: String,
    },
    /// old_name 이 비면 더하고 아니면 그 db 전용 접속을 고친다. 비밀번호가 비면 그대로 둔다
    ConnSave {
        #[serde(default)]
        old_name: String,
        conn: ConnFields,
    },
    ConnDelete {
        name: String,
    },
    /// 접속들을 트리 폴더로 옮긴다(R66.2). 빈 folder 는 원래 그룹으로 되돌린다
    ConnFolder {
        names: Vec<String>,
        #[serde(default)]
        folder: String,
    },
}

/// 접속 창의 칸이다(R59.2·R60.1). 비밀번호는 요청에만 싣고 답에는 싣지 않는다(conn_get 은 has_password 만).
#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConnFields {
    pub name: String,
    #[serde(default)]
    pub group: String,
    /// postgresql·redis·opensearch·loki·prometheus·grafana·dynamodb·athena·aws
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub db: String,
    #[serde(default)]
    pub user: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub password: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub auth: String,
    #[serde(default)]
    pub region: String,
    #[serde(default)]
    pub profile: String,
    #[serde(default)]
    pub workgroup: String,
    #[serde(default)]
    pub output: String,
    #[serde(default)]
    pub sslmode: String,
    /// conn_get 의 답만: `.env` 에 비밀번호(토큰)가 있다
    #[serde(default)]
    pub has_password: bool,
}

// 디버그 출력에도 비밀번호가 나오지 않게 한다
impl fmt::Debug for ConnFields {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}({} {}:{}/{} {})",
            self.name, self.kind, self.host, self.port, self.db, self.url
        )
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SourceRunRequest {
    pub conn: String,
    pub text: String,
    /// 0 이면 종류 기본(1시간 전부터 지금까지)
    #[serde(default)]
    pub since_ns: i64,
    #[serde(default)]
    pub until_ns: i64,
    /// 0 이면 1,000
    #[serde(default)]
    pub limit: usize,
    /// Loki 방향 "forward"·"backward"(기본)
    #[serde(default)]
    pub direction: String,
    /// confirm_expensive 를 확인받은 뒤
    #[serde(default)]
    pub force: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RunRequest {
    pub conn: String,
    pub sql: String,
    /// `:이름` 값(R21). 값을 서버가 바인딩한다
    #[serde(default)]
    pub params: BTreeMap<String, String>,
    /// 사용자가 `prod(<이름>)에 쓰기 실행: y/N` 에 y 했다(R6)
    #[serde(default)]
    pub confirm_prod: bool,
    /// 사용자가 `위험: … y/N` 에 y 했다(R25)
    #[serde(default)]
    pub confirm_danger: bool,
    /// 사용자가 `EXPLAIN ANALYZE 는 문장을 실제로 실행합니다: y/N` 에 y 했다(R9.2). explain 만 본다
    #[serde(default)]
    pub confirm_analyze: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TablePageRequest {
    pub conn: String,
    pub schema: String,
    pub table: String,
    /// WHERE 식(R16.4). 최상위 `;` 는 거절한다
    #[serde(default, rename = "where")]
    pub filter: String,
    /// ORDER BY 글(`name, "Note" DESC`)
    #[serde(default)]
    pub order_by: String,
    #[serde(default)]
    pub offset: u64,
    /// 0 이면 200, 1,000 을 넘으면 1,000
    #[serde(default)]
    pub limit: u64,
    /// 정확한 개수(count(*))도 센다
    #[serde(default)]
    pub count: bool,
    /// 테이블 전체 추정치(reltuples)도 읽는다
    #[serde(default)]
    pub estimate: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CliRequest {
    /// 명령과 인자(`query -c a "select 1"`). `-p`·`-f`·`--remote` 는 서버가 쓰지 않는다
    pub args: Vec<String>,
    /// 클라이언트가 읽은 SQL·명령 글(-f 파일·표준 입력)
    #[serde(default)]
    pub input: Option<CliInput>,
    /// 클라이언트 표준 출력이 터미널인지(기본 형식 table·tsv)
    #[serde(default)]
    pub tty: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CliInput {
    Text(String),
    /// 읽다 난 오류 글이다. 서버가 실행 오류로 찍는다
    Error(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CliResponse {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}

// ─── 결과 ───

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Hello {
    pub version: u32,
    pub server: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ProjectInfo {
    pub dir: String,
    pub conns: Vec<ConnSummary>,
    pub warnings: Vec<String>,
}

/// 접속 하나의 보이는 정보다. 비밀번호·사용자·키는 싣지 않는다.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConnSummary {
    pub name: String,
    pub group: String,
    pub folder: String,
    pub kind: String,
    pub env: String,
    pub host: String,
    pub db: String,
    /// datagrip 또는 db(R59.1)
    pub source: String,
    /// 화면이 세션·최근 기록·쿼리 파일 매핑에 남기는 접속 열쇠다. 옛 serve 는 보내지 않아 빈 글이다
    #[serde(default)]
    pub uuid: String,
}

/// 표 결과다. Go `db.Result` 의 Cols·Types·Rows·Text·Truncated·InTx·Elapsed 다.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResultSet {
    pub cols: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub types: Option<Vec<String>>,
    pub rows: Vec<Vec<String>>,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub elapsed_ms: u64,
    /// 편집기 세션 실행이면 그 뒤 트랜잭션 상태
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_tx: Option<bool>,
    /// enc() 확장 알림(R52) 같은 딸린 글
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    /// 결과가 한 테이블에서 왔고 PK 를 다 담았을 때만 있다. 없으면 표는 읽기 전용이다(R15.1)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<Origin>,
}

impl ResultSet {
    pub fn from_query(r: QueryResult) -> ResultSet {
        ResultSet {
            cols: r.cols,
            types: r.types,
            rows: r.rows,
            text: r.text,
            truncated: r.truncated,
            elapsed_ms: r.elapsed.as_millis() as u64,
            in_tx: None,
            note: String::new(),
            origin: None,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TablePage {
    pub result: ResultSet,
    /// 실제로 보낸 SELECT 다
    pub sql: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<i64>,
    /// 음수면 모른다(R16.2)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimate: Option<i64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TxState {
    pub in_tx: bool,
    /// 명령 태그(BEGIN·COMMIT·ROLLBACK)
    #[serde(default)]
    pub text: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LockState {
    pub conn: String,
    pub prod: bool,
    pub locked: bool,
    /// 열린 동안 남은 초
    pub remaining_secs: u64,
    /// 열면 열리는 분(DB_PROD_UNLOCK_MIN)
    pub unlock_min: u64,
}

/// set_search_path·switch_db 뒤 세션 상태다.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionState {
    pub conn: String,
    pub db: String,
    pub search_path: String,
    pub in_tx: bool,
}

/// apply_changes·undo_changes 의 답이다. result 는 INSERT 가 돌려준 행과 `UPDATE 1 · DELETE 1` 요약, sql 은 실행한 문장의 미리 보기다.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppliedResult {
    pub result: ResultSet,
    pub applied: Applied,
    pub sql: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChangesPreview {
    /// applied 를 줬으면 되돌리기 묶음, 아니면 받은 묶음 그대로다
    pub changes: ChangeSet,
    pub sql: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Count {
    pub rows: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LinkedResult {
    /// text 는 요약(`연결된 곳 3 · 접속 2곳 …`)이다
    pub result: ResultSet,
    /// 접속·테이블 하나라도 실패했다
    pub failed: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct User360Item {
    pub title: String,
    pub conn: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<ResultSet>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RedisEditResult {
    /// 실행한 명령마다의 결과(첫 오류에서 멈춘다)
    pub results: Vec<ResultSet>,
    /// 다시 읽을 키. 이름을 바꿨으면 새 이름, 키를 지웠으면 빈 글이다
    pub key: String,
    pub status: String,
}

/// 대시보드 확인 결과다. err 는 비밀값을 지운 한 줄이다.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthInfo {
    pub ok: bool,
    pub latency_ms: u64,
    pub version: String,
    pub metrics: Vec<Metric>,
    pub summary: String,
    pub err: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FlowResult {
    pub flow: String,
    pub boxes: Vec<FlowBox>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BoardSummary {
    pub name: String,
    pub title: String,
    pub panels: usize,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
}

/// 보드 한 번의 결과다. error 는 파일 단위 설정 오류(JSON·refresh)다.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BoardResult {
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    pub panels: Vec<PanelOut>,
}

/// 패널 한 칸이다. 숫자 패널은 value·warn, 표·차트는 result, 흐름은 boxes 다.
/// error 는 `설정 오류: …` 이거나 실행 오류이고, note 는 묻지 않은 prod 패널 안내다.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PanelOut {
    pub title: String,
    pub conn: String,
    pub flow: String,
    pub view: String,
    pub prod: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    pub warn: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<ResultSet>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub boxes: Vec<FlowBox>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

/// decrypt 의 답이다. values[i] 는 cells[i] 를 푼 값이고, 암호문 꼴이 아니거나 풀지 못한 칸은 null(원래 값 그대로)이다.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Decrypted {
    pub values: Vec<Option<String>>,
    /// 암호문 꼴인데 풀지 못한 칸 수(`N칸 풀지 못함`)
    pub failed: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExportStarted {
    pub export: u64,
    /// enc() 확장 알림(R52)
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

/// export_read 의 답이다. data 는 줄에서 base64 이고 CSV 바이트 그대로다(글자 경계와 상관없이 자른다).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExportChunk {
    #[serde(with = "base64_bytes")]
    pub data: Vec<u8>,
    /// 끝났다. 이때 data 는 비어 있다
    pub done: bool,
    /// 지금까지 받은 바이트(이 조각 포함)
    pub bytes: u64,
    /// done 일 때 COPY 가 보낸 행 수
    #[serde(default)]
    pub rows: i64,
}

mod base64_bytes {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(v))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        STANDARD.decode(s).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Canceled {
    pub canceled: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConnTested {
    /// `PostgreSQL 16.4`·`OpenSearch · 12개` 꼴. 화면은 `접속됨 (…)` 으로 보인다
    pub version: String,
    /// 평문·AWS 밖 서명 경고(R59.1·R60.1). 비면 없다
    #[serde(default)]
    pub warning: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConnSaved {
    pub conn: ConnSummary,
    #[serde(default)]
    pub warning: String,
    /// 접속은 저장됐지만 .env 비밀번호 줄을 옮기지 못했다(`connection saved but .env was not updated: …`)
    #[serde(default)]
    pub env_error: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScanResult {
    pub keys: Vec<String>,
    /// 1,000개에서 끊었다
    pub more: bool,
}

// ─── 오류 ───

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineError {
    pub code: i32,
    pub error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl EngineError {
    pub fn run(msg: impl Into<String>) -> EngineError {
        EngineError {
            code: CODE_RUN,
            error: msg.into(),
            reason: None,
        }
    }

    pub fn usage(msg: impl Into<String>) -> EngineError {
        EngineError {
            code: CODE_USAGE,
            error: msg.into(),
            reason: None,
        }
    }

    pub fn refused(reason: &str, msg: impl Into<String>) -> EngineError {
        EngineError {
            code: CODE_REFUSED,
            error: msg.into(),
            reason: Some(reason.into()),
        }
    }
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.error)
    }
}

impl std::error::Error for EngineError {}

// ─── 줄 ───

/// 요청 한 줄을 만든다.
pub fn request_line(id: u64, req: &Request) -> String {
    let mut v = serde_json::to_value(req).expect("request serializes");
    v["id"] = Value::from(id);
    v.to_string()
}

/// 요청 한 줄을 읽는다. 읽은 id 는 오류여도 돌려준다(없으면 0).
pub fn parse_request(line: &str) -> (u64, Result<Request, EngineError>) {
    let mut v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return (0, Err(EngineError::usage(format!("bad request: {e}")))),
    };
    let id = v.get("id").and_then(Value::as_u64).unwrap_or(0);
    if let Some(m) = v.as_object_mut() {
        m.remove("id");
    }
    (
        id,
        serde_json::from_value(v).map_err(|e| EngineError::usage(format!("bad request: {e}"))),
    )
}

/// 응답 한 줄을 만든다.
pub fn response_line(id: u64, res: &Result<Value, EngineError>) -> String {
    let v = match res {
        Ok(result) => serde_json::json!({"id": id, "ok": true, "result": result}),
        Err(e) => {
            let mut v = serde_json::json!({"id": id, "ok": false, "code": e.code, "error": e.error});
            if let Some(r) = &e.reason {
                v["reason"] = Value::from(r.as_str());
            }
            v
        }
    };
    v.to_string()
}

#[derive(Deserialize)]
struct WireResponse {
    id: u64,
    ok: bool,
    #[serde(default)]
    result: Value,
    #[serde(default)]
    code: i32,
    #[serde(default)]
    error: String,
    #[serde(default)]
    reason: Option<String>,
}

/// 응답 한 줄을 읽는다.
pub fn parse_response(line: &str) -> Result<(u64, Result<Value, EngineError>), String> {
    let r: WireResponse = serde_json::from_str(line).map_err(|e| format!("bad response: {e}"))?;
    let res = if r.ok {
        Ok(r.result)
    } else {
        Err(EngineError {
            code: if r.code == 0 { CODE_RUN } else { r.code },
            error: r.error,
            reason: r.reason,
        })
    };
    Ok((r.id, res))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 요청은_id_와_op_를_한_줄에_싣고_다시_읽힌다() {
        let req = Request::Run(RunRequest {
            conn: "a".into(),
            sql: "select 1".into(),
            ..Default::default()
        });
        let line = request_line(7, &req);
        assert!(!line.contains('\n'));
        assert!(line.contains("\"op\":\"run\""), "{line}");
        let (id, back) = parse_request(&line);
        assert_eq!(id, 7);
        assert!(matches!(back, Ok(Request::Run(r)) if r.conn == "a" && r.sql == "select 1"));
    }

    #[test]
    fn 모르는_필드는_무시하고_모르는_op_는_사용법_오류다() {
        let (id, r) = parse_request(r#"{"id":3,"op":"schemas","conn":"a","later":1}"#);
        assert_eq!(id, 3);
        assert!(matches!(r, Ok(Request::Schemas { conn }) if conn == "a"));
        let (id, r) = parse_request(r#"{"id":4,"op":"nope"}"#);
        assert_eq!(id, 4);
        assert_eq!(r.unwrap_err().code, CODE_USAGE);
        let (id, r) = parse_request("not json");
        assert_eq!(id, 0);
        assert_eq!(r.unwrap_err().code, CODE_USAGE);
    }

    #[test]
    fn 응답은_성공과_거절을_같은_꼴로_오간다() {
        let ok = response_line(1, &Ok(serde_json::json!({"x": 1})));
        assert_eq!(ok, r#"{"id":1,"ok":true,"result":{"x":1}}"#);
        assert_eq!(parse_response(&ok).unwrap(), (1, Ok(serde_json::json!({"x": 1}))));
        let err = EngineError::refused(REASON_PROD_LOCKED, "prod(a) 쓰기 잠김");
        let line = response_line(2, &Err(err.clone()));
        assert_eq!(
            line,
            r#"{"code":3,"error":"prod(a) 쓰기 잠김","id":2,"ok":false,"reason":"prod_locked"}"#
        );
        assert_eq!(parse_response(&line).unwrap(), (2, Err(err)));
    }

    #[test]
    fn explain_은_run_필드를_같은_층에_싣는다() {
        let (_, r) = parse_request(r#"{"id":1,"op":"explain","conn":"a","sql":"select 1","analyze":true,"confirm_prod":true}"#);
        let Ok(Request::Explain { run, analyze, .. }) = r else {
            panic!("{r:?}")
        };
        assert!(analyze && run.confirm_prod && run.conn == "a" && run.sql == "select 1");
        let (_, r) = parse_request(r#"{"id":2,"op":"table_page","conn":"a","schema":"s","table":"t","where":"id > 1"}"#);
        assert!(matches!(r, Ok(Request::TablePage(p)) if p.filter == "id > 1" && p.limit == 0));
    }

    #[test]
    fn 판_인사와_CLI_입력은_정한_모양이다() {
        let (_, r) = parse_request(r#"{"id":1,"op":"hello","version":1}"#);
        assert!(matches!(r, Ok(Request::Hello { version: 1 })));
        let req = Request::Cli(CliRequest {
            args: vec!["query".into()],
            input: Some(CliInput::Text("select 1".into())),
            tty: false,
        });
        let line = request_line(2, &req);
        assert!(line.contains(r#""input":{"text":"select 1"}"#), "{line}");
    }

    #[test]
    fn 내보내기_조각은_base64_로_오가고_접속_칸의_비밀번호는_디버그에_없다() {
        let c = ExportChunk {
            data: vec![0xff, b'a', b'\n'],
            done: false,
            bytes: 3,
            rows: 0,
        };
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!(v["data"], "/2EK");
        assert_eq!(serde_json::from_value::<ExportChunk>(v).unwrap(), c);
        let (_, r) = parse_request(r#"{"id":1,"op":"conn_save","conn":{"name":"a","password":"pw-x"}}"#);
        let Ok(Request::ConnSave { old_name, conn }) = r else {
            panic!("{r:?}")
        };
        assert!(old_name.is_empty() && conn.password == "pw-x");
        assert!(!format!("{conn:?}").contains("pw-x"));
        let (_, r) = parse_request(r#"{"id":2,"op":"decrypt","conn":"a","cells":["x"]}"#);
        assert!(matches!(r, Ok(Request::Decrypt { cells, .. }) if cells == ["x"]));
    }
}
