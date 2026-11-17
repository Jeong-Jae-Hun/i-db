//! 메뉴·단축키·단축키 표가 함께 보는 명령 목록이다. 한 곳에 두어 메뉴 글과 실제 묶음과 ⌘/ 표가 어긋나지 않게 한다.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Cmd {
    About,
    Settings,
    Quit,
    NewTab,
    OpenFile,
    SaveFile,
    CloseTab,
    SwitchConn,
    Undo,
    Redo,
    Cut,
    Copy,
    Paste,
    SelectAll,
    Find,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    ToggleSidebar,
    ToggleResults,
    Dashboard,
    Run,
    RunAll,
    Explain,
    Cancel,
    TxBegin,
    TxCommit,
    TxRollback,
    ProdLock,
    Minimize,
    Shortcuts,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuName {
    App,
    File,
    Edit,
    View,
    Query,
    Window,
    Help,
}

impl MenuName {
    pub const ALL: [MenuName; 7] = [
        MenuName::App,
        MenuName::File,
        MenuName::Edit,
        MenuName::View,
        MenuName::Query,
        MenuName::Window,
        MenuName::Help,
    ];

    pub fn title(self) -> &'static str {
        match self {
            MenuName::App => "i-db",
            MenuName::File => "파일",
            MenuName::Edit => "편집",
            MenuName::View => "보기",
            MenuName::Query => "쿼리",
            MenuName::Window => "윈도우",
            MenuName::Help => "도움말",
        }
    }
}

/// 묶는 맥락이다. 편집 명령은 gpui-component 입력칸이 이미 묶어 두어 다시 묶지 않는다(`Own` 이 아님).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bind {
    /// 앱 어디서나
    Global,
    /// 작업 공간 안
    Workspace,
    /// 작업 공간 안이면서 편집기 줄바꿈(⌘↵)보다 앞서야 한다
    WorkspaceOverInput,
    /// 입력칸·표가 이미 가진 묶음(메뉴와 표에는 보인다)
    Builtin,
}

#[derive(Clone, Copy, Debug)]
pub struct Entry {
    pub cmd: Cmd,
    pub menu: MenuName,
    pub label: &'static str,
    /// GPUI 키 꼴(`cmd-shift-enter`). 없으면 메뉴로만 쓴다
    pub key: Option<&'static str>,
    pub bind: Bind,
    /// 메뉴에서 이 항목 앞에 구분선을 둔다
    pub sep_before: bool,
}

const fn e(cmd: Cmd, menu: MenuName, label: &'static str, key: Option<&'static str>, bind: Bind, sep_before: bool) -> Entry {
    Entry {
        cmd,
        menu,
        label,
        key,
        bind,
        sep_before,
    }
}

use Bind::*;
use Cmd::*;
use MenuName as M;

pub const ENTRIES: &[Entry] = &[
    e(About, M::App, "i-db 정보", None, Global, false),
    e(Settings, M::App, "설정…", Some("cmd-,"), Global, true),
    e(Quit, M::App, "i-db 끝내기", Some("cmd-q"), Global, true),
    e(NewTab, M::File, "새 탭", Some("cmd-t"), Workspace, false),
    e(OpenFile, M::File, "열기…", Some("cmd-o"), Workspace, false),
    e(SaveFile, M::File, "저장", Some("cmd-s"), Workspace, false),
    e(CloseTab, M::File, "탭 닫기", Some("cmd-w"), Workspace, true),
    e(SwitchConn, M::File, "접속 바꾸기…", Some("cmd-shift-o"), Workspace, true),
    e(Undo, M::Edit, "실행 취소", Some("cmd-z"), Builtin, false),
    e(Redo, M::Edit, "다시 실행", Some("cmd-shift-z"), Builtin, false),
    e(Cut, M::Edit, "잘라내기", Some("cmd-x"), Builtin, true),
    e(Copy, M::Edit, "복사", Some("cmd-c"), Builtin, false),
    e(Paste, M::Edit, "붙여넣기", Some("cmd-v"), Builtin, false),
    e(SelectAll, M::Edit, "모두 선택", Some("cmd-a"), Builtin, false),
    e(Find, M::Edit, "찾기", Some("cmd-f"), Builtin, true),
    e(ZoomIn, M::View, "확대", Some("cmd-="), Global, false),
    e(ZoomOut, M::View, "축소", Some("cmd--"), Global, false),
    e(ZoomReset, M::View, "실제 크기", Some("cmd-0"), Global, false),
    e(ToggleSidebar, M::View, "사이드바 보이기/숨기기", Some("cmd-b"), Workspace, true),
    e(ToggleResults, M::View, "결과 패널 보이기/숨기기", Some("cmd-j"), Workspace, false),
    e(Dashboard, M::View, "대시보드 (접속 상태·흐름·보드)", Some("cmd-g"), Workspace, true),
    e(Run, M::Query, "실행 (커서 문장·선택)", Some("cmd-enter"), WorkspaceOverInput, false),
    e(RunAll, M::Query, "모두 실행", Some("cmd-shift-enter"), WorkspaceOverInput, false),
    e(Explain, M::Query, "EXPLAIN", Some("cmd-e"), Workspace, false),
    e(Cancel, M::Query, "멈춤", Some("cmd-."), Workspace, false),
    e(TxBegin, M::Query, "BEGIN", None, Workspace, true),
    e(TxCommit, M::Query, "COMMIT", None, Workspace, false),
    e(TxRollback, M::Query, "ROLLBACK", None, Workspace, false),
    e(ProdLock, M::Query, "prod 잠금 열기/잠그기", Some("cmd-l"), Workspace, true),
    e(Minimize, M::Window, "최소화", Some("cmd-m"), Global, false),
    e(Shortcuts, M::Help, "단축키", Some("cmd-/"), Global, false),
];

/// `⌘+` 도 확대로 받는다(US 자판은 = 와 + 가 같은 자리).
pub const EXTRA_KEYS: &[(&str, Cmd)] = &[("cmd-+", ZoomIn), ("cmd-shift-=", ZoomIn)];

pub fn entries_in(menu: MenuName) -> impl Iterator<Item = &'static Entry> {
    ENTRIES.iter().filter(move |e| e.menu == menu)
}

/// `cmd-shift-enter` → `⇧⌘↵`. macOS 메뉴처럼 수식 키를 ⌃⌥⇧⌘ 순으로 앞에 둔다.
pub fn display_key(key: &str) -> String {
    let parts: Vec<&str> = if let Some(stripped) = key.strip_suffix("--") {
        stripped.split('-').filter(|p| !p.is_empty()).chain(["-"]).collect()
    } else {
        key.split('-').collect()
    };
    let (mods, last) = parts.split_at(parts.len().saturating_sub(1));
    let mut out = String::new();
    for (m, sym) in [("ctrl", "⌃"), ("alt", "⌥"), ("shift", "⇧"), ("cmd", "⌘")] {
        if mods.contains(&m) {
            out.push_str(sym);
        }
    }
    out.push_str(match last.first().copied().unwrap_or("") {
        "enter" => "↵",
        "escape" => "⎋",
        "backspace" => "⌫",
        "space" => "Space",
        k if k.chars().count() == 1 => return out + &k.to_uppercase(),
        k => k,
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn 같은_맥락에_같은_키를_두_번_묶지_않는다() {
        let mut seen = HashSet::new();
        for e in ENTRIES {
            if let Some(k) = e.key {
                // Workspace·WorkspaceOverInput 는 둘 다 작업 공간에 묶이므로 같은 칸으로 센다
                let ctx = match e.bind {
                    Global => "global",
                    Builtin => "builtin",
                    _ => "workspace",
                };
                assert!(seen.insert((ctx, k)), "{k} 가 두 번 묶였다");
                assert!(seen.insert(("any", k)) || ctx == "builtin", "{k} 가 맥락을 달리해 겹친다");
            }
        }
    }

    #[test]
    fn 모든_메뉴에_항목이_있고_요청한_단축키가_있다() {
        for m in MenuName::ALL {
            assert!(entries_in(m).count() > 0, "{m:?}");
            assert!(!m.title().is_empty());
        }
        let key = |c: Cmd| ENTRIES.iter().find(|e| e.cmd == c).and_then(|e| e.key);
        assert_eq!(key(Run), Some("cmd-enter"));
        assert_eq!(key(Explain), Some("cmd-e"));
        assert_eq!(key(Cancel), Some("cmd-."));
        assert_eq!(key(ProdLock), Some("cmd-l"));
        assert_eq!(key(ToggleSidebar), Some("cmd-b"));
        assert_eq!(key(Settings), Some("cmd-,"));
        assert_eq!(key(Shortcuts), Some("cmd-/"));
        assert_eq!(key(ZoomIn), Some("cmd-="));
        assert_eq!(key(ZoomOut), Some("cmd--"));
        assert_eq!(key(ZoomReset), Some("cmd-0"));
        assert!(EXTRA_KEYS.iter().any(|(k, c)| *k == "cmd-+" && *c == ZoomIn));
    }

    #[test]
    fn 명령은_한_번씩만_나온다() {
        let mut seen = HashSet::new();
        for e in ENTRIES {
            assert!(seen.insert(e.cmd), "{:?}", e.cmd);
            assert!(!e.label.is_empty());
        }
    }

    #[test]
    fn 단축키는_맥_기호로_보인다() {
        assert_eq!(display_key("cmd-enter"), "⌘↵");
        assert_eq!(display_key("cmd-shift-enter"), "⇧⌘↵");
        assert_eq!(display_key("cmd--"), "⌘-");
        assert_eq!(display_key("cmd-="), "⌘=");
        assert_eq!(display_key("cmd-l"), "⌘L");
        assert_eq!(display_key("cmd-,"), "⌘,");
    }
}
