//! 설정 창(⌘,)의 값과 규칙이다. 안전 규칙은 바꿀 수 없어 글로만 보인다.

use serde::{Deserialize, Serialize};

pub const DEFAULT_ROW_LIMIT: u64 = 500;
/// 엔진의 table_page 상한
pub const MAX_ROW_LIMIT: u64 = 1000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemePref {
    #[default]
    System,
    Light,
    Dark,
}

impl ThemePref {
    pub const ALL: [ThemePref; 3] = [ThemePref::System, ThemePref::Light, ThemePref::Dark];

    pub fn label(self) -> &'static str {
        match self {
            ThemePref::System => "시스템",
            ThemePref::Light => "밝게",
            ThemePref::Dark => "어둡게",
        }
    }
}

/// 입력칸 글을 행 수로 읽는다. 1..=1000 밖이거나 수가 아니면 None.
pub fn parse_row_limit(s: &str) -> Option<u64> {
    s.trim().parse::<u64>().ok().filter(|n| (1..=MAX_ROW_LIMIT).contains(n))
}

/// 설정 창에 읽기 전용으로 보이는 안전 규칙이다. 엔진(serve)이 정하고 GUI 는 바꾸지 못한다.
pub const SAFETY_RULES: &[&str] = &[
    "기본은 읽기 전용이다. 쓰는 문장은 확인을 거친다.",
    "prod 접속은 잠겨 있다. ⌘L 로 잠시 열고, 열려 있어도 쓰기마다 확인한다.",
    "WHERE 없는 UPDATE·DELETE, DROP·TRUNCATE 같은 위험 문장은 한 번 더 묻는다.",
    "읽기 전용이 아닌 문장의 EXPLAIN ANALYZE 는 실제로 실행하므로 묻는다.",
    "Redis KEYS 처럼 서버를 멈추는 명령은 확인으로도 풀리지 않는다.",
    "비밀번호·AES 키는 화면·오류·기록에 나오지 않는다.",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 행_수는_1부터_1000까지만_받는다() {
        assert_eq!(parse_row_limit(" 200 "), Some(200));
        assert_eq!(parse_row_limit("0"), None);
        assert_eq!(parse_row_limit("1001"), None);
        assert_eq!(parse_row_limit("abc"), None);
    }

    #[test]
    fn 테마는_소문자로_저장한다() {
        assert_eq!(serde_json::to_string(&ThemePref::Dark).unwrap(), "\"dark\"");
        assert_eq!(serde_json::from_str::<ThemePref>("\"light\"").unwrap(), ThemePref::Light);
        assert!(ThemePref::ALL.iter().all(|t| !t.label().is_empty()));
    }
}
