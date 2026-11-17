//! i-db 브랜드 값(assets/brand/README.md). 화면은 이 값만 쓴다.

pub const NAME: &str = "i-db";
pub const TAGLINE: &str = "DataGrip 프로젝트를 그대로 여는 DB 도구";
/// 강조색. 마크의 점·글자표의 `-`·주 버튼·초점 고리에만 쓴다
pub const ACCENT: &str = "#0d9488";
pub const ACCENT_HOVER: &str = "#0f766e";
pub const ACCENT_ACTIVE: &str = "#115e59";
pub const REPO: &str = "https://github.com/Jeong-Jae-Hun/i-db";

/// 릴리스 빌드는 IDB_VERSION(태그)을 넣는다. 없으면 소스 빌드다(db 의 update_cmd 와 같다).
pub fn version() -> &'static str {
    option_env!("IDB_VERSION").unwrap_or("dev")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 화면_강조색은_브랜드_원본_SVG_와_같다() {
        for svg in [
            include_str!("../../../../assets/brand/mark.svg"),
            include_str!("../../../../assets/brand/mark-dark.svg"),
            include_str!("../../../../assets/brand/lockup.svg"),
            include_str!("../../../../assets/brand/app-icon.svg"),
        ] {
            assert!(svg.contains(ACCENT), "브랜드 SVG 강조색이 {ACCENT} 가 아니다");
        }
        assert!(include_str!("../../../../assets/brand/README.md").contains(ACCENT));
    }

    #[test]
    fn 이름과_한_줄_소개() {
        assert_eq!(NAME, "i-db");
        assert!(!TAGLINE.is_empty() && !version().is_empty());
    }
}
