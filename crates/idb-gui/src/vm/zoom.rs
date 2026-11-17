//! 화면 확대(⌘= ⌘- ⌘0). 단계는 고정 목록이고 글자 크기·rem(여백·높이)을 같은 배율로 키운다.

/// 고를 수 있는 배율이다. 브라우저처럼 정해진 단계로 움직여 몇 번 누르면 원래대로 돌아온다
pub const STEPS: [f32; 10] = [0.75, 0.85, 0.9, 1.0, 1.1, 1.25, 1.4, 1.6, 1.8, 2.0];
pub const DEFAULT: f32 = 1.0;
/// GPUI 기본 rem 이다. 여백(p_2 등)과 text_xs 같은 크기가 이 값을 따른다
const BASE_REM: f32 = 16.0;

/// 설정 파일에서 읽은 값을 가장 가까운 단계로 맞춘다.
pub fn normalize(z: f32) -> f32 {
    if !z.is_finite() {
        return DEFAULT;
    }
    STEPS
        .iter()
        .copied()
        .min_by(|a, b| (a - z).abs().total_cmp(&(b - z).abs()))
        .unwrap_or(DEFAULT)
}

pub fn zoom_in(z: f32) -> f32 {
    let z = normalize(z);
    STEPS.iter().copied().find(|&s| s > z).unwrap_or(z)
}

pub fn zoom_out(z: f32) -> f32 {
    let z = normalize(z);
    STEPS.iter().rev().copied().find(|&s| s < z).unwrap_or(z)
}

/// 배율을 곱한 글자 크기(px). 반 픽셀은 흐릿해 정수로 둔다.
pub fn font_px(base: f32, z: f32) -> f32 {
    (base * normalize(z)).round()
}

pub fn rem_px(z: f32) -> f32 {
    (BASE_REM * normalize(z)).round()
}

pub fn label(z: f32) -> String {
    format!("{}%", (normalize(z) * 100.0).round() as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 확대_축소는_단계를_따라가고_끝에서_멈춘다() {
        assert_eq!(zoom_in(1.0), 1.1);
        assert_eq!(zoom_out(1.0), 0.9);
        assert_eq!(zoom_in(2.0), 2.0);
        assert_eq!(zoom_out(0.75), 0.75);
        let mut z = DEFAULT;
        for _ in 0..3 {
            z = zoom_in(z);
        }
        for _ in 0..3 {
            z = zoom_out(z);
        }
        assert_eq!(z, DEFAULT);
    }

    #[test]
    fn 이상한_값은_가까운_단계로_맞춘다() {
        assert_eq!(normalize(1.12), 1.1);
        assert_eq!(normalize(9.0), 2.0);
        assert_eq!(normalize(0.1), 0.75);
        assert_eq!(normalize(f32::NAN), 1.0);
    }

    #[test]
    fn 글자와_rem_을_같은_배율로_키운다() {
        assert_eq!(font_px(13.0, 1.0), 13.0);
        assert_eq!(font_px(13.0, 1.25), 16.0);
        assert_eq!(rem_px(1.25), 20.0);
        assert_eq!(rem_px(0.75), 12.0);
        assert_eq!(label(1.25), "125%");
    }
}
