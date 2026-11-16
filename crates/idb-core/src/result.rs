//! 질의 결과 한 벌. Go `db.Result` 와 같은 모양이다.

use std::time::Duration;

/// PostgreSQL 결과의 NULL 칸 글이다. 결과는 칸을 글로 싣기 때문에 값이 정말 "NULL" 인 글도 NULL 로 보인다.
pub const NULL_TEXT: &str = "NULL";

#[derive(Clone, Debug, Default)]
pub struct QueryResult {
    pub cols: Vec<String>,
    /// 열 타입 이름(PostgreSQL 만). None 이면 NULL 칸을 따로 가르지 않는다
    pub types: Option<Vec<String>>,
    pub rows: Vec<Vec<String>>,
    /// 표가 아닌 결과(명령 태그, string 값 등)나 표에 딸린 글
    pub text: String,
    pub truncated: bool,
    pub elapsed: Duration,
}

/// 숫자로 정렬·집계·오른쪽 맞춤할 PostgreSQL 타입이다. money 는 통화 글자가 붙어 뺀다.
pub fn is_numeric_type(typ: &str) -> bool {
    matches!(typ, "int2" | "int4" | "int8" | "float4" | "float8" | "numeric" | "oid")
}
