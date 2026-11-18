//! 접속 고르는 화면의 글과 항목이다. 글자가 안 보이는 사고(텍스트 시스템 누락)를 겪어 문구를 여기 모아 시험한다.

use super::runtime::Target;

pub const TITLE: &str = "i-db";
pub const SUBTITLE: &str = "접속할 곳을 고른다. 비밀번호·.env 는 접속이 있는 쪽에만 있다.";
pub const REMOTE_SECTION: &str = "원격 (db serve over ssh)";
pub const LOCAL_SECTION: &str = "로컬 프로젝트";
pub const RECENT_SECTION: &str = "최근";
pub const DIR_PLACEHOLDER: &str = "프로젝트 폴더 (예: ~/Projects/app/db)";
pub const PICK_DIR: &str = "폴더 선택…";
pub const OPEN: &str = "열기";
pub const NO_REMOTES: &str = "원격이 없다 · ~/.config/db/remote.json 에 이름을 더하면 여기 보인다";

/// 원격 버튼 하나다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    pub label: String,
    pub target: Target,
}

pub fn remote_choices(names: &[String]) -> Vec<Choice> {
    names
        .iter()
        .map(|n| Choice {
            label: format!("{n}  (ssh)"),
            target: Target::Remote(n.clone()),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 고르는_화면의_글은_모두_비어_있지_않다() {
        for s in [
            TITLE,
            SUBTITLE,
            REMOTE_SECTION,
            LOCAL_SECTION,
            RECENT_SECTION,
            DIR_PLACEHOLDER,
            PICK_DIR,
            OPEN,
            NO_REMOTES,
        ] {
            assert!(!s.trim().is_empty());
        }
    }

    #[test]
    fn 원격_버튼은_이름과_ssh_표시를_보인다() {
        let c = remote_choices(&["devbox".into()]);
        assert_eq!(c[0].label, "devbox  (ssh)");
        assert_eq!(c[0].target, Target::Remote("devbox".into()));
    }

    /// gpui-pre-platform 의 font-kit 이 꺼지면 macOS 에서 NoopTextSystem 이 붙어 글자가 하나도 안 그려진다.
    #[test]
    fn macOS_텍스트_시스템_기능을_켠다() {
        let toml = include_str!("../../Cargo.toml");
        let line = toml.lines().find(|l| l.starts_with("gpui_platform")).expect("gpui_platform 의존성");
        assert!(line.contains("\"font-kit\""), "{line}");
    }
}
