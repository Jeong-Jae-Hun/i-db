//! F1 도움말·명령 팔레트(R40)·이 화면의 키(R74.2)가 같이 쓰는 항목과 상태 줄 키 안내(R74.1).
//! 항목은 (키, 설명, 팔레트가 다시 누를 키)이고 셋째가 비면 팔레트에 넣지 않는다. 첫째가 빈 항목은 묶음 안의 작은 제목이다.
//! 키를 더하거나 바꾸면 이 표 한 곳만 고친다(Go ui.go helpSections 와 같은 글).

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    Keep,
    Tree,
    Editor,
    Result,
}

pub struct Section {
    pub title: &'static str,
    pub at: Focus,
    pub keys: &'static [(&'static str, &'static str, &'static str)],
}

pub const SECTIONS: &[Section] = &[
    Section {
        title: "전역",
        at: Focus::Keep,
        keys: &[
            ("Tab / Shift+Tab", "포커스 이동 (자동완성 창이 열려 있으면 Tab 은 고르기)", "tab"),
            ("Ctrl+R / F5", "실행: 선택한 글 전부, 없으면 커서 문장", "ctrl+r"),
            ("Ctrl+E", "EXPLAIN (실행하지 않음)", "ctrl+e"),
            ("Alt+E", "EXPLAIN ANALYZE, BUFFERS (쓰기 문장은 y/N)", "alt+e"),
            (
                "Ctrl+P",
                "명령 팔레트 (동작·테이블·접속을 이름으로 찾아 실행·열기, 자주 쓴 동작·최근 테이블 먼저)",
                "",
            ),
            (
                "Ctrl+H",
                "실행 기록 (Enter ◆ 결과 사본을 새 탭에 · Ctrl+E 편집기에 · Ctrl+D 사본 지우기 · 오늘·어제 거르기)",
                "ctrl+h",
            ),
            ("Ctrl+N", "객체 찾기 (테이블·뷰·Redis 키, 최근 연 테이블 먼저)", "ctrl+n"),
            ("Ctrl+O / Ctrl+S", "파일 열기 / 저장 (결과 편집이 있으면 제출)", "ctrl+o"),
            ("F9", "트랜잭션 Auto / Manual (Manual 은 실행 전 BEGIN)", "f9"),
            ("F10 / F11", "COMMIT (prod 는 확인) / ROLLBACK", "f10"),
            ("F3 / F4", "즐겨찾기 열기 (새 탭) / 저장 (선택, 없으면 전체)", "f3"),
            ("F6", "스키마 고르기 (search_path)", "f6"),
            ("F7", "마지막 표 결과 전체 내보내기 CSV (esc 취소)", "f7"),
            ("Ctrl+U", "사용자 한눈에 보기 (이메일·uid·id·닉네임으로 여러 DB 조회)", "ctrl+u"),
            ("Ctrl+G", "대시보드 (모든 접속 상태·지연·버전·수치, 30초마다 다시 확인)", "ctrl+g"),
            (
                "Alt+F",
                "흐름 (아웃박스 → SNS → SQS → DLQ·Lambda, .dbtui/flows.json, 30초마다 다시 확인)",
                "alt+f",
            ),
            ("Alt+B", "보드 (.dbtui/boards/ 의 쿼리 패널 묶음, refresh 마다 다시 돌림)", "alt+b"),
            ("Alt+C", "같은 쿼리 다른 접속에서 비교 (기본 stg↔prod)", "alt+c"),
            ("Ctrl+L", "prod 쓰기 잠금 열기·닫기 (탭 접속, DB_PROD_UNLOCK_MIN 분)", "ctrl+l"),
            ("Ctrl+]", "Loki·Prometheus 범위 15m·1h·6h·24h·7d (탭마다)", "ctrl+]"),
            ("Ctrl+C", "실행 취소, 실행 중이 아니면 종료", "ctrl+c"),
            ("Ctrl+← / Ctrl+→", "트리 폭 (트리·결과 포커스, 다음에도 그 크기)", "ctrl+right"),
            ("Ctrl+↑ / Ctrl+↓", "편집기 높이 (트리·결과 포커스, 다음에도 그 크기)", "ctrl+down"),
            ("F1", "도움말", "f1"),
            ("DB_RESTORE=0", "끝낼 때 탭 저장·다음에 되살리기 끄기 (창 크기는 남김)", ""),
            ("DB_AUTO_SAVE·OPEN=0", "자동 저장 / 접속 고를 때 최근 파일 열기 끄기", ""),
            (
                "DB_RECENT=0",
                "최근 기록 끄기 (팔레트 자주 쓴 동작·테이블 정렬·WHERE·열 폭·입력 기록·트리 펼침)",
                "",
            ),
            ("DB_KEYS=cmd / ctrl", "키 안내를 ⌘ / Ctrl 로 (기본은 맥·kitty 키면 ⌘)", ""),
            (
                "DB_CLIPBOARD=osc52 / system",
                "복사를 OSC 52 만 / 시스템 클립보드만 (기본은 둘 다)",
                "",
            ),
            ("enc('글')", "실행 전에 그룹 키·IV 로 staticEncrypt 암호문으로 바꿈", ""),
        ],
    },
    Section {
        title: "맥 ⌘ (Ghostty·kitty·WezTerm·iTerm2)",
        at: Focus::Keep,
        keys: &[
            ("⌘R / ⌘↩", "실행 (= Ctrl+R)", ""),
            ("⌘E / ⌥⌘E", "EXPLAIN / EXPLAIN ANALYZE", ""),
            ("⌘S / ⌘O", "저장·제출 / 열기", ""),
            ("⌘/ · ⌘D · ⌘K · ⌘A", "주석 · 줄 복제 · 복사 (결과는 복사 꼴 고르기) · 전체 선택", ""),
            ("⌘Z / ⌘⇧Z", "되돌리기 / 다시 하기", ""),
            ("⌘B", "정의로 이동 · 결과는 연결된 곳 찾기 (= Ctrl+B)", ""),
            ("⌘P", "명령 팔레트 (= Ctrl+P)", ""),
            ("⌘T ⌘W ⌘1..9 ⌘N ⌘H ⌘?", "= Ctrl+T Ctrl+W Alt+1..9 Ctrl+N Ctrl+H F1", ""),
            ("⌘⇧O ⌘⇧S ⌘⇧D ⌘⇧E", "= F3 F4 F6 F7", ""),
            ("⌥⌘L ⌘⇧M ⌘⇧↩ ⌘⇧⌫", "= F8(⌘⇧F 도) F9 F10 F11", ""),
            ("⌘U ⌘L ⌘G ⌘⇧C ⌘⇧R ⌘⇧P ⌘⇧B", "= Ctrl+U Ctrl+L Ctrl+G Alt+C Alt+R Alt+P Alt+B", ""),
            ("⌘V", "결과 표에 시트 칸 붙여넣기", ""),
            ("⌘←→ / ⌘↑↓", "줄 처음·끝 / 문서 처음·끝", ""),
        ],
    },
    Section {
        title: "쿼리 탭",
        at: Focus::Keep,
        keys: &[
            ("Ctrl+T", "새 탭", "ctrl+t"),
            ("Ctrl+W", "탭 닫기", "ctrl+w"),
            ("Alt+1..9", "탭 이동", "alt+1"),
            (
                "Ctrl+PgUp / Ctrl+PgDn",
                "이전 / 다음 탭 (파일 탭은 옮길 때 자동 저장)",
                "ctrl+pgdown",
            ),
            ("Alt+R", "탭 이름 바꾸기", "alt+r"),
            ("Alt+P", "탭 고정 (앞에 모으고 닫을 때 묻기)", "alt+p"),
        ],
    },
    Section {
        title: "편집기",
        at: Focus::Editor,
        keys: &[
            ("Ctrl+Space", "자동완성 (Tab·Enter 고르기, esc 닫기)", "ctrl+@"),
            ("Shift+방향키", "선택", "shift+right"),
            ("Ctrl+← / Ctrl+→", "단어 이동", "ctrl+right"),
            ("Ctrl+↑ / Ctrl+↓", "스크롤", "ctrl+down"),
            ("Ctrl+A", "전체 선택", "ctrl+a"),
            ("Ctrl+X / Ctrl+K / Ctrl+V", "잘라내기 / 복사 / 붙여넣기", "ctrl+k"),
            ("Ctrl+Z / Ctrl+Y", "되돌리기 / 다시", "ctrl+z"),
            ("Ctrl+D", "줄 복제", "ctrl+d"),
            ("Ctrl+/", "주석 토글", "ctrl+/"),
            ("단어 + Tab", "라이브 템플릿 (sel selw selc lim ins upd del cnt)", "tab"),
            ("F8", "SQL 정렬 (선택, 없으면 전체)", "f8"),
            ("Ctrl+B / F12", "정의로 이동 (테이블 구조 · 함수 정의)", "ctrl+b"),
            (":name / $1", "실행 전에 값을 묻고 바인딩", ""),
            (
                "자동 저장",
                "파일 없는 탭은 첫 실행 때 queries/ 에 저장, 파일 탭은 고친 1초 뒤 저장",
                "",
            ),
        ],
    },
    Section {
        title: "트리",
        at: Focus::Tree,
        keys: &[
            ("", "이동·열기", ""),
            ("j k / ↑ ↓", "이동", "j"),
            ("Enter / l", "펼치기 · 테이블·키 열기", "enter"),
            ("h", "접기 · 부모로", "h"),
            ("Enter (테이블)", "테이블 보기 (서버 페이지·거르기·정렬)", "enter"),
            ("Enter (접속)", "펼치기 · 그 접속을 쓰는 탭이 없으면 최근 파일 열기", "enter"),
            ("Enter (쿼리 파일)", "파일 열기 (이미 열린 탭이면 그 탭으로)", "enter"),
            (
                "Enter (저장소)",
                "인덱스·라벨 값·지표·테이블 미리 보기 (OpenSearch·Loki·Prometheus·Grafana·DynamoDB·Athena, 펼치기는 l)",
                "enter",
            ),
            ("", "보기·운영", ""),
            ("d / D", "테이블 구조 / DDL 을 새 탭에", "d"),
            ("I", "CSV 가져오기 (테이블)", "I"),
            ("A / L / S", "활동 / 잠금 / 테이블 크기 (PostgreSQL 접속)", "A"),
            ("A / L / S", "INFO / SLOWLOG / 큰 키 (Redis 접속)", "A"),
            ("B", "데이터베이스 바꾸기 (PostgreSQL 접속)", "B"),
            (
                "F",
                "데이터 검색 (PostgreSQL 접속·스키마) · aws SNS 토픽은 흐름 (DB_ALLOW_BILLED=1)",
                "F",
            ),
            ("=", "다른 접속과 스키마 비교 (PostgreSQL 접속·스키마)", "="),
            ("X", "패턴 맞는 키 일괄 삭제 (Redis 접속·이름공간, esc 멈춤)", "X"),
            ("", "접속", ""),
            ("N", "새 접속 (URL 붙여넣기 · Ctrl+T 시험, .dbtui/connections.json)", "N"),
            ("E", "접속 고치기 (db 전용 접속) · 폴더 줄은 폴더 이름 바꾸기", "E"),
            ("M", "접속을 다른 폴더로 옮기기 (빈 값이면 원래 그룹, .dbtui/folders.json)", "M"),
            ("X (PostgreSQL·저장소 접속)", "접속 지우기 (db 전용 접속, .env 비밀번호도)", ""),
            ("Alt+X", "접속 지우기 (db 전용 접속, Redis 포함)", "alt+X"),
            ("", "거르기·기타", ""),
            ("/", "거르기 (Redis 키는 SCAN 패턴)", "/"),
            ("esc", "거르기 해제", "esc"),
            ("r", "새로고침 (접속은 자동완성 목록도, 쿼리 파일 목록도)", "r"),
            ("?", "이 화면의 키 (전체는 F1)", "?"),
            ("q", "종료", "q"),
        ],
    },
    Section {
        title: "결과",
        at: Focus::Result,
        keys: &[
            ("", "이동", ""),
            ("h j k l / 방향키", "셀 이동", "j"),
            ("PgUp / PgDn · g / G", "쪽 이동 · 처음 / 끝", "pgdown"),
            ("0 / $", "첫 열 / 끝 열", "0"),
            ("< / >", "열 폭", ">"),
            ("", "보기", ""),
            ("Enter / v", "셀 전체 값 / 레코드 세로 보기", "enter"),
            ("o", "셀 전체 값", "o"),
            ("w", "텍스트 줄바꿈 (테이블 보기는 WHERE, / 도 같음)", "w"),
            ("t · esc", "실행 계획 나무 ↔ JSON · 닫기", "t"),
            ("r (기록 사본 탭)", "그 기록의 접속에서 다시 실행해 사본을 갈음 (R11.4)", ""),
            ("c", "차트 (다시 c·esc 로 표)", "c"),
            ("D", "staticEncrypt 칸 복호화 보기 (다시 D 원래 값)", "D"),
            ("", "정렬·거르기", ""),
            ("s · /", "정렬(오름→내림→없음) · 거르기 abc !abc col:abc col=v col>v col=null", "s"),
            ("O", "테이블 보기 ORDER BY 입력 (Tab 열 이름 완성)", "O"),
            (
                "[ / ] · #",
                "테이블 보기 이전·다음 페이지 · 정확한 개수 (일반 결과 ] 는 다음 1,000행 더 읽기)",
                "]",
            ),
            ("+ / -", "테이블 보기 WHERE 에 칸 값 = / <> 더하기·빼기", "+"),
            ("", "편집", ""),
            ("Enter·F2 / esc", "칸 고치기 / 취소 (Ctrl+N 은 NULL)", "f2"),
            ("a · Ctrl+D · d · u", "행 추가 · 복제 · 삭제 표시 · 되돌리기", "a"),
            ("Ctrl+S", "변경 미리 보기 뒤 제출", "ctrl+s"),
            ("Ctrl+Z", "마지막 제출 되돌리기 (미리 보기 뒤 확인)", "ctrl+z"),
            ("붙여넣기", "시트 칸(TSV)을 커서부터 덮어쓰기 (넘치면 새 행)", ""),
            (
                "칸 고치는 중",
                "space bool 순환 · now/today · Ctrl+O 큰 편집기 · enum 은 고르기 창",
                "",
            ),
            ("", "복사·내보내기", ""),
            ("y / Y", "셀 / 행 복사", "y"),
            (
                "Ctrl+K · 오른쪽 클릭",
                "복사 꼴 고르기: 행·선택·보이는 행·열 × TSV·CSV(헤더)·JSON·Markdown·INSERT·IN (마지막 꼴 기억)",
                "ctrl+k",
            ),
            ("V", "사각형 선택 · 합·평균·최소·최대 (y 복사, esc·V 끝)", "V"),
            ("C", "열(선택) 값을 IN 목록으로 복사", "C"),
            ("i / U", "행을 INSERT / UPDATE 문으로 복사", "i"),
            ("e / E", "CSV / JSON 내보내기", "e"),
            ("", "찾기·비교", ""),
            ("F / R", "외래 키 따라가기 / 이 행을 참조하는 테이블", "F"),
            (
                "Ctrl+B",
                "연결된 곳 찾기 (칸 값이 든 다른 테이블 · 같은 그룹·환경 접속, Enter 로 열기)",
                "ctrl+b",
            ),
            ("p / P · { / }", "결과 고정 / 풀기 · 결과 탭 이동", "p"),
            ("=", "고정한 결과와 비교 (esc 닫기)", "="),
            ("", "기타", ""),
            ("K / X", "pid 행의 쿼리 취소 / 연결 종료", "K"),
            ("?", "이 화면의 키 (전체는 F1)", "?"),
            ("q", "종료", "q"),
        ],
    },
    Section {
        title: "대시보드 (Ctrl+G)",
        at: Focus::Keep,
        keys: &[
            ("r", "바로 다시 확인", ""),
            ("Enter", "트리에서 그 접속 고르기", ""),
            ("/ · esc", "그룹·이름·환경으로 거르기 · 해제", ""),
            ("j k / ↑ ↓", "이동", ""),
            ("?", "이 화면의 키", ""),
        ],
    },
    Section {
        title: "흐름 (Alt+F)",
        at: Focus::Keep,
        keys: &[
            ("r", "바로 다시 풀기", ""),
            ("↑ ↓ / j k", "흐름 고르기", ""),
            ("← → / h l", "상자 고르기", ""),
            ("Enter", "상자 상세 (SNS 구독·큐 속성 등)", ""),
            ("?", "이 화면의 키", ""),
        ],
    },
    Section {
        title: "보드 (Alt+B)",
        at: Focus::Keep,
        keys: &[
            ("r", "바로 다시 돌리기 (파일도 다시 읽음)", ""),
            ("방향키 / h j k l", "패널 고르기", ""),
            ("Enter", "패널 질의를 새 쿼리 탭에서 실행 · 흐름 패널은 흐름 탭", ""),
            ("?", "이 화면의 키", ""),
        ],
    },
    Section {
        title: "JSON 값 보기 (o)",
        at: Focus::Result,
        keys: &[
            ("t", "나무 ↔ 원문", "t"),
            ("Enter / space", "접기 · 펴기", "enter"),
            ("y / Y", "값 / 경로($.a[0].b) 복사", "y"),
        ],
    },
    Section {
        title: "Redis 값 편집",
        at: Focus::Result,
        keys: &[
            ("e / Enter", "값·항목 고치기", "e"),
            ("a · d", "항목 추가 · 지우기", "a"),
            ("t · r · D", "TTL · 이름 바꾸기 · 키 지우기", "t"),
            ("esc", "편집기 닫기", "esc"),
        ],
    },
    Section {
        title: "마우스",
        at: Focus::Keep,
        keys: &[
            ("클릭", "영역 포커스 · 행·칸 선택 · 탭 전환", ""),
            ("끌기", "편집기 선택", ""),
            ("더블클릭 / ▸ 클릭", "펼치기 · 열기 · 단어 선택", ""),
            ("휠", "트리 · 편집기 · 결과 스크롤", ""),
            ("+ 클릭", "새 탭", ""),
            ("경계 끌기", "트리 폭 · 편집기 높이 (다음에도 그 크기)", ""),
            ("열 머리 클릭", "정렬 (테이블 보기는 서버 정렬, Shift 는 정렬 열 더하기)", ""),
            ("WHERE · ORDER BY 칸 클릭", "테이블 보기 조건·정렬 입력", ""),
        ],
    },
];

pub const SEP: &str = " · ";

// ─── 키 안내 (R74.1) ───

pub const EDITOR_HINTS: &[&str] = &[
    "Ctrl+R 실행",
    "Ctrl+E 실행 계획",
    "F8 정렬",
    "F1 도움말",
    "Ctrl+B 정의",
    "Ctrl+Space 자동완성",
    "Ctrl+P 팔레트",
];
pub const EDIT_TAIL: &[&str] = &["d 삭제", "Ctrl+D 복제", "Ctrl+K 복사", "Ctrl+B 연결된 곳", "s 정렬", "/ 거르기"];
pub const FILTER_HINTS: &[&str] = &["Enter 확정", "esc 취소"];
pub const CELL_EDIT_HINTS: &[&str] = &["Enter 확정", "esc 취소", "Ctrl+N NULL", "Ctrl+O 큰 편집기"];
pub const SELECT_HINTS: &[&str] = &["y 복사", "C IN 목록", "Ctrl+K 꼴 고르기", "esc 끝"];
pub const RECORD_HINTS: &[&str] = &["↑↓ 칸", "Enter 값", "y 값 복사", "Y 행 복사", "esc 표로"];
pub const VALUE_HINTS: &[&str] = &["y 복사", "esc 닫기"];
pub const VALUE_JSON_HINTS: &[&str] = &["t 나무", "y 복사", "esc 닫기"];
pub const TEXT_HINTS: &[&str] = &["↑↓ 스크롤", "w 줄바꿈", "y 복사", "p 고정", "? 키"];
pub const NONE_HINTS: &[&str] = &["Shift+Tab 편집기", "Ctrl+R 실행", "F1 도움말"];
pub const WHERE_HINTS: &[&str] = &["Enter 적용", "esc 취소", "Tab 열 이름"];
pub const REDIS_HINTS: &[&str] = &["t TTL", "r 이름 바꾸기", "D 키 지우기", "esc 닫기", "? 키"];
pub const REDIS_EDIT_HINTS: &[&str] = &["Enter 확정", "esc 취소"];
pub const REDIS_ASK_HINTS: &[&str] = &["y 삭제", "esc 취소"];

pub fn redis_type_hints(t: &str) -> &'static [&'static str] {
    match t {
        "string" => &["e 고치기"],
        "hash" | "list" | "set" | "zset" => &["e 고치기", "a 항목 추가", "d 항목 지우기"],
        _ => &[],
    }
}

/// 항목을 앞에서부터 w 칸에 들어가는 만큼만 잇는다. 항목 중간에서 자르지 않는다(R13.5).
pub fn fit(items: &[String], w: usize) -> String {
    let mut out = String::new();
    for it in items {
        let next = if out.is_empty() { it.clone() } else { format!("{out}{SEP}{it}") };
        if unicode_width::UnicodeWidthStr::width(next.as_str()) > w {
            break;
        }
        out = next;
    }
    out
}

/// 팔레트 동작 하나다(R40.1). 묶음 제목과 키로 기억한다(R75.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaletteAction {
    pub label: String,
    pub section: &'static str,
    pub key: &'static str,
    pub press: &'static str,
    pub at: Focus,
}

impl PaletteAction {
    /// 최근 기록 열쇠다. ⌘ 글자(R47.1)와 상관없다
    pub fn memo(&self) -> String {
        format!("{}\u{1f}{}", self.section, self.key)
    }
}

pub fn palette_actions(mac: bool) -> Vec<PaletteAction> {
    let mut out = Vec::new();
    for s in SECTIONS {
        for (k, d, p) in s.keys {
            if p.is_empty() || k.is_empty() {
                continue;
            }
            out.push(PaletteAction {
                label: format!("{} — {} · {}", d, crate::keys::label(k, mac), s.title),
                section: s.title,
                key: k,
                press: p,
                at: s.at,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 키_안내는_항목_중간에서_자르지_않는다() {
        let items: Vec<String> = ["Ctrl+R 실행", "F8 정렬", "F1 도움말"].iter().map(|s| s.to_string()).collect();
        assert_eq!(fit(&items, 22), "Ctrl+R 실행 · F8 정렬");
        assert_eq!(fit(&items, 5), "");
    }

    #[test]
    fn 팔레트는_다시_누를_키가_있는_항목만_담는다() {
        let acts = palette_actions(false);
        assert!(acts.iter().any(|a| a.press == "ctrl+t" && a.label.starts_with("새 탭 — Ctrl+T")));
        assert!(!acts.iter().any(|a| a.key == "Ctrl+P"));
    }
}
