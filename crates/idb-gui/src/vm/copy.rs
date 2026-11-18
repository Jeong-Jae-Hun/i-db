//! 화면 글(빈 상태·오류·확인)을 한곳에 모은다. 말투는 짧고 차분하게, 느낌표 없이 습니다체가 아닌 평서형으로 맞춘다.
//! 단추 글은 동사 하나, 빈 상태는 제목 한 줄 + 할 일 한 줄이다.

pub const CANCEL: &str = "취소";

/// 빈 상태(제목, 할 일)
pub const EMPTY_NO_TAB: (&str, &str) = ("열린 탭이 없다", "⌘T 로 새 탭을 연다");
pub const EMPTY_NO_CONN: (&str, &str) = ("접속을 고른다", "왼쪽 목록에서 접속을 누르면 이 탭에서 쓴다");
pub const EMPTY_READY: (&str, &str) = ("⌘↵ 로 실행한다", "커서가 든 문장(선택이 있으면 선택)을 실행한다 · ⇧⌘↵ 모두 실행");
pub const EMPTY_ROWS: &str = "행이 없다";
pub const EMPTY_FILTER: &str = "맞는 행이 없다";
pub const EMPTY_KEYS: &str = "맞는 키가 없다 · 패턴을 바꿔 다시 찾는다";
pub const EMPTY_VALUE: &str = "키를 고르면 값을 보인다";
pub const EMPTY_SOURCE: &str = "왼쪽에서 항목을 고르거나 질의를 실행한다";
pub const EMPTY_PALETTE: &str = "맞는 것이 없다 · 낱말을 줄이거나 초성으로 찾는다";
