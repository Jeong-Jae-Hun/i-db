//! 편집기 탭. 탭마다 접속·파일·고침 여부를 들고, 실행할 문장은 선택이 있으면 선택, 없으면 커서가 든 문장이다.

use std::ops::Range;
use std::path::PathBuf;

use idb_core::sqlsafe::statement_at;

/// 탭에 붙은 파일이다. 프로젝트 `queries/` 파일은 엔진 op 로만 읽고 쓰고(로컬·원격 같다), ⌘O 로 고른 이 Mac 의
/// 파일만 직접 쓴다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TabFile {
    /// 프로젝트 상대 경로(`queries/app/a.sql`)
    Query(String),
    Local(PathBuf),
}

impl TabFile {
    pub fn query(&self) -> Option<&str> {
        match self {
            TabFile::Query(r) => Some(r),
            TabFile::Local(_) => None,
        }
    }

    pub fn display(&self) -> String {
        match self {
            TabFile::Query(r) => r.clone(),
            TabFile::Local(p) => p.display().to_string(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tab {
    pub id: u64,
    pub conn: Option<String>,
    pub file: Option<TabFile>,
    pub dirty: bool,
    /// 파일이 없는 탭의 이름(테이블 보기·Redis). 없으면 `쿼리 N`
    pub name: Option<String>,
    n: u64,
}

impl Tab {
    /// 탭 이름이다. 첫 실행 저장이 지은 `<접속>-<날짜>-<시각>.sql` 은 사람이 지은 이름이 아니라 `쿼리 N` 으로 둔다
    /// (전체 경로는 툴팁에 있다).
    pub fn label(&self) -> String {
        match &self.file {
            Some(TabFile::Query(r)) => {
                let name = r.rsplit('/').next().unwrap_or(r);
                if is_auto_name(name) {
                    format!("쿼리 {}", self.n)
                } else {
                    name.to_string()
                }
            }
            Some(TabFile::Local(p)) => p
                .file_name()
                .map_or_else(|| p.display().to_string(), |f| f.to_string_lossy().into_owned()),
            None => self.name.clone().unwrap_or_else(|| format!("쿼리 {}", self.n)),
        }
    }

    /// 툴팁: 파일이면 전체 경로
    pub fn tooltip(&self) -> String {
        self.file.as_ref().map_or_else(|| self.label(), TabFile::display)
    }

    pub fn title(&self) -> String {
        let base = match &self.file {
            Some(TabFile::Query(_)) => self.label(),
            Some(TabFile::Local(p)) => p
                .file_name()
                .map_or_else(|| p.display().to_string(), |f| f.to_string_lossy().into_owned()),
            None => self.name.clone().unwrap_or_else(|| format!("쿼리 {}", self.n)),
        };
        if self.dirty { format!("{base} *") } else { base }
    }
}

/// 첫 실행 저장 이름(`app-stg-20261118-093000.sql`, 같은 초면 `-2` 가 붙는다)인지
pub fn is_auto_name(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".sql") else { return false };
    let parts: Vec<&str> = stem.rsplitn(4, '-').collect();
    // 뒤에서: [번호?] 시각 날짜 접속
    let digits = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_digit());
    match parts.as_slice() {
        [time, date, conn] | [time, date, conn, ..] if digits(time, 6) && digits(date, 8) && !conn.is_empty() => true,
        [seq, time, date, ..] if seq.bytes().all(|b| b.is_ascii_digit()) && digits(time, 6) && digits(date, 8) => true,
        _ => false,
    }
}

/// 가운데를 줄인다(`orders_2026…_backup`). max 는 글자 수다.
pub fn middle_ellipsis(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max || max < 5 {
        return s.to_string();
    }
    let keep = max - 1;
    let head = keep.div_ceil(2);
    let tail = keep - head;
    let h: String = s.chars().take(head).collect();
    let t: String = s.chars().skip(n - tail).collect();
    format!("{h}…{t}")
}

#[derive(Default)]
pub struct Tabs {
    pub tabs: Vec<Tab>,
    pub active: usize,
    next: u64,
}

impl Tabs {
    /// 새 탭을 열고 고른다. 그 id 를 돌려준다.
    pub fn open(&mut self, conn: Option<String>, file: Option<TabFile>) -> u64 {
        self.open_tab(conn, file, None)
    }

    pub fn open_named(&mut self, conn: Option<String>, name: String) -> u64 {
        self.open_tab(conn, None, Some(name))
    }

    fn open_tab(&mut self, conn: Option<String>, file: Option<TabFile>, name: Option<String>) -> u64 {
        self.next += 1;
        self.tabs.push(Tab {
            id: self.next,
            conn,
            file,
            dirty: false,
            name,
            n: self.next,
        });
        self.active = self.tabs.len() - 1;
        self.next
    }

    pub fn active(&self) -> Option<&Tab> {
        self.tabs.get(self.active)
    }

    pub fn active_mut(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.active)
    }

    /// 탭을 고른다. 이미 닫힌 자리(그린 뒤 닫힌 탭의 클릭)는 무시한다.
    pub fn activate(&mut self, i: usize) {
        if i < self.tabs.len() {
            self.active = i;
        }
    }

    pub fn index_of(&self, id: u64) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == id)
    }

    /// 닫고, 고른 탭은 오른쪽(없으면 왼쪽)으로 옮긴다.
    pub fn close(&mut self, i: usize) -> Option<Tab> {
        if i >= self.tabs.len() {
            return None;
        }
        let t = self.tabs.remove(i);
        if self.active > i || self.active >= self.tabs.len() {
            self.active = self.active.saturating_sub(1);
        }
        Some(t)
    }
}

/// 실행할 문장이다. 선택이 있으면 선택, 없으면 커서가 든 문장(R3)이다. 비면 None.
pub fn target_sql(text: &str, selection: Range<usize>, cursor: usize) -> Option<String> {
    // 선택이 글자 경계가 아니면(있어선 안 되지만) 자르다 패닉하지 않게 커서 문장으로 간다
    let ok = |i: usize| text.is_char_boundary(i);
    let s = if selection.start < selection.end && selection.end <= text.len() && ok(selection.start) && ok(selection.end) {
        text[selection].trim()
    } else {
        let mut c = cursor.min(text.len());
        while !ok(c) {
            c -= 1;
        }
        statement_at(text, c)
    };
    let s = s.trim().trim_end_matches(';').trim();
    (!s.is_empty()).then(|| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 탭_제목은_파일_이름이고_고치면_별표가_붙는다() {
        let mut t = Tabs::default();
        t.open(Some("a".into()), None);
        t.open(None, Some(TabFile::Local("/q/report.sql".into())));
        assert_eq!(t.tabs[0].title(), "쿼리 1");
        assert_eq!(t.active().unwrap().title(), "report.sql");
        t.active_mut().unwrap().dirty = true;
        assert_eq!(t.active().unwrap().title(), "report.sql *");
        t.open_named(Some("a".into()), "public.users".into());
        assert_eq!(t.active().unwrap().title(), "public.users");
        t.open(None, Some(TabFile::Query("queries/app/a.sql".into())));
        assert_eq!(t.active().unwrap().title(), "a.sql");
    }

    #[test]
    fn 자동_저장_이름은_쿼리_N_으로_보이고_긴_이름은_가운데를_줄인다() {
        assert!(is_auto_name("app-stg-20261118-093000.sql"));
        assert!(is_auto_name("shop-stg-read-20261118-093000.sql"));
        assert!(is_auto_name("app-stg-20261118-093000-2.sql"));
        assert!(!is_auto_name("daily-orders.sql"));
        assert!(!is_auto_name("report-2026.sql"));
        let mut t = Tabs::default();
        t.open(None, None);
        t.open(
            Some("app-stg".into()),
            Some(TabFile::Query("queries/app/app-stg-20261118-093000.sql".into())),
        );
        assert_eq!(t.active().unwrap().label(), "쿼리 2");
        assert_eq!(t.active().unwrap().tooltip(), "queries/app/app-stg-20261118-093000.sql");
        assert_eq!(middle_ellipsis("orders_2026_11_backup_final", 11), "order…final");
        assert_eq!(middle_ellipsis("short", 11), "short");
    }

    #[test]
    fn 닫으면_고른_탭이_옆으로_옮겨간다() {
        let mut t = Tabs::default();
        let a = t.open(None, None);
        let b = t.open(None, None);
        let c = t.open(None, None);
        t.active = 1;
        t.close(1);
        assert_eq!(t.active().unwrap().id, c);
        t.close(1);
        assert_eq!(t.active().unwrap().id, a);
        assert_eq!(t.index_of(b), None);
        t.close(0);
        assert!(t.active().is_none());
        assert!(t.close(0).is_none());
    }

    #[test]
    fn 닫힌_자리를_고르면_무시한다() {
        let mut t = Tabs::default();
        t.open(None, None);
        t.open(None, None);
        t.activate(5);
        assert_eq!(t.active, 1);
        t.activate(0);
        assert_eq!(t.active, 0);
    }

    #[test]
    fn 한글_경계가_아닌_선택_커서도_넘어지지_않는다() {
        let text = "select '가나';\nselect 2;";
        assert_eq!(target_sql(text, 8..9, 0).as_deref(), Some("select '가나'"));
        assert_eq!(target_sql(text, 0..0, 9).as_deref(), Some("select '가나'"));
        assert_eq!(target_sql(text, 0..0, text.len() - 2).as_deref(), Some("select 2"));
        // 끝 ';' 뒤 커서는 빈 문장이다(넘는 자리여도 넘어지지 않는다)
        assert_eq!(target_sql(text, 0..0, 999), None);
    }

    #[test]
    fn 선택이_없으면_커서가_든_문장을_실행한다() {
        let text = "select 1;\nselect 2;\n";
        assert_eq!(target_sql(text, 0..0, 3).as_deref(), Some("select 1"));
        assert_eq!(target_sql(text, 0..0, 12).as_deref(), Some("select 2"));
        assert_eq!(target_sql(text, 10..18, 0).as_deref(), Some("select 2"));
        assert_eq!(target_sql("  ;  ", 0..0, 1), None);
    }
}
