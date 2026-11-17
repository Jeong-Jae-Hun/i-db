//! 질의 결과 한 벌. Go `db.Result` 와 같은 모양이다.

use std::time::Duration;

/// PostgreSQL 결과의 NULL 칸 글이다. 결과는 칸을 글로 싣기 때문에 값이 정말 "NULL" 인 글도 NULL 로 보인다.
pub const NULL_TEXT: &str = "NULL";

/// 편집기·쓰기 실행이 표에 싣는 최대 행 수다
pub const MAX_ROWS: usize = 1000;
/// 표 칸 하나에 담는 최대 바이트(E63)
pub const MAX_CELL_BYTES: usize = 64 << 10;

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

/// 칸을 MAX_CELL_BYTES(E63)에서 자르고 `…(N바이트)` 꼬리를 붙인다. is_capped_cell 이 이 꼬리로 잘린 칸을 알아본다.
/// UTF-8 이 아닌 바이트는 U+FFFD 로 바꾼다(Go 는 바이트 그대로 두고 JSON 에서 바꾼다).
pub fn cap_cell(b: &[u8]) -> String {
    if b.len() <= MAX_CELL_BYTES {
        return String::from_utf8_lossy(b).into_owned();
    }
    format!("{}…({}바이트)", to_valid_utf8(&b[..MAX_CELL_BYTES]), b.len())
}

/// Go `strings.ToValidUTF8(s, "")`: 깨진 UTF-8 조각을 버린다. 바이트로 자른 끝에 걸린 글자 조각을 지울 때 쓴다.
pub fn to_valid_utf8(mut b: &[u8]) -> String {
    let mut out = String::with_capacity(b.len());
    loop {
        match std::str::from_utf8(b) {
            Ok(s) => {
                out.push_str(s);
                return out;
            }
            Err(e) => {
                let (good, rest) = b.split_at(e.valid_up_to());
                out.push_str(std::str::from_utf8(good).unwrap_or_default());
                let skip = e.error_len().unwrap_or(rest.len());
                b = &rest[skip..];
            }
        }
    }
}

/// MAX_CELL_BYTES 에서 잘린 칸인지다. 잘린 칸은 끝에 `…(N바이트)`·`…(잘림)` 이 붙는다.
pub fn is_capped_cell(s: &str) -> bool {
    if s.len() <= MAX_CELL_BYTES {
        return false;
    }
    let Some(rest) = s.strip_suffix(')') else { return false };
    let Some(i) = rest.rfind("…(") else { return false };
    let inner = &rest[i + "…(".len()..];
    inner == "잘림"
        || inner
            .strip_suffix("바이트")
            .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 긴_칸은_자르고_꼬리를_붙이며_끝의_깨진_글자는_버린다() {
        let mut b = vec![b'a'; MAX_CELL_BYTES - 1];
        b.extend_from_slice("한글".as_bytes());
        let s = cap_cell(&b);
        assert_eq!(s, format!("{}…({}바이트)", "a".repeat(MAX_CELL_BYTES - 1), MAX_CELL_BYTES + 5));
        assert!(is_capped_cell(&s));
        assert_eq!(cap_cell(b"x"), "x");
        assert!(!is_capped_cell("x…(3바이트)"));
        assert_eq!(to_valid_utf8(b"a\xffb\xe1\x84"), "ab");
    }
}
