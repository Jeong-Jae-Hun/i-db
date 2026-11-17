//! 편집기 탭. 탭마다 접속·파일·고침 여부를 들고, 실행할 문장은 선택이 있으면 선택, 없으면 커서가 든 문장이다.

use std::ops::Range;
use std::path::PathBuf;

use idb_core::sqlsafe::statement_at;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tab {
    pub id: u64,
    pub conn: Option<String>,
    pub path: Option<PathBuf>,
    pub dirty: bool,
    /// 파일이 없는 탭의 이름(테이블 보기·Redis). 없으면 `쿼리 N`
    pub name: Option<String>,
    n: u64,
}

impl Tab {
    pub fn title(&self) -> String {
        let base = match &self.path {
            Some(p) => p
                .file_name()
                .map_or_else(|| p.display().to_string(), |f| f.to_string_lossy().into_owned()),
            None => self.name.clone().unwrap_or_else(|| format!("쿼리 {}", self.n)),
        };
        if self.dirty { format!("{base} *") } else { base }
    }
}

#[derive(Default)]
pub struct Tabs {
    pub tabs: Vec<Tab>,
    pub active: usize,
    next: u64,
}

impl Tabs {
    /// 새 탭을 열고 고른다. 그 id 를 돌려준다.
    pub fn open(&mut self, conn: Option<String>, path: Option<PathBuf>) -> u64 {
        self.open_tab(conn, path, None)
    }

    pub fn open_named(&mut self, conn: Option<String>, name: String) -> u64 {
        self.open_tab(conn, None, Some(name))
    }

    fn open_tab(&mut self, conn: Option<String>, path: Option<PathBuf>, name: Option<String>) -> u64 {
        self.next += 1;
        self.tabs.push(Tab {
            id: self.next,
            conn,
            path,
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
        t.open(None, Some("/q/report.sql".into()));
        assert_eq!(t.tabs[0].title(), "쿼리 1");
        assert_eq!(t.active().unwrap().title(), "report.sql");
        t.active_mut().unwrap().dirty = true;
        assert_eq!(t.active().unwrap().title(), "report.sql *");
        t.open_named(Some("a".into()), "public.users".into());
        assert_eq!(t.active().unwrap().title(), "public.users");
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
