//! 글자·간격 눈금 하나(배율 1 기준: 글 14, 보조 12, 머리 11·12, 간격 4/8/12/16/24). 확대 배율은 이 눈금 전체를 곱한다.
//! 줄 높이는 늘 "위아래 여백 + 글 줄 높이 합 + 줄 사이"로 셈해서 내용보다 작은 고정 높이를 두지 않는다
//! (v1.1.0 은 두 줄 접속 줄을 28px 로 묶어 다음 줄과 글자가 겹쳤다).

use super::zoom;

/// 눌러지는 것의 가장 작은 높이(px, 배율 1)
pub const MIN_HIT: f32 = 28.0;
/// 글 줄 높이 = 글자 크기 × 이 값(한글이 위아래로 붙지 않게)
pub const LINE: f32 = 1.45;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Scale {
    pub zoom: f32,
}

impl Scale {
    pub fn new(zoom: f32) -> Scale {
        Scale {
            zoom: zoom::normalize(zoom),
        }
    }

    /// 설정 글자 크기(기본 14)까지 곱한 눈금이다. 13 이면 전체가 13/14 로 줄어든다.
    pub fn with_font(zoom: f32, font_size: f32) -> Scale {
        let base = if font_size.is_finite() && font_size > 0.0 {
            font_size / 14.0
        } else {
            1.0
        };
        Scale {
            zoom: zoom::normalize(zoom) * base,
        }
    }

    fn px(&self, v: f32) -> f32 {
        (v * self.zoom).round()
    }

    // ─── 글자 ───

    pub fn text(&self) -> f32 {
        self.px(14.0)
    }
    pub fn small(&self) -> f32 {
        self.px(12.0)
    }
    /// 사이드바 묶음 머리(접속 폴더) 글
    pub fn group(&self) -> f32 {
        self.px(12.0)
    }
    /// 구역 머리(접속·테이블·쿼리) 글
    pub fn section(&self) -> f32 {
        self.px(11.0)
    }
    pub fn pill(&self) -> f32 {
        self.px(11.0)
    }
    pub fn line(&self, font: f32) -> f32 {
        (font * LINE).ceil()
    }

    // ─── 간격 ───

    pub fn s4(&self) -> f32 {
        self.px(4.0)
    }
    pub fn s8(&self) -> f32 {
        self.px(8.0)
    }
    pub fn s12(&self) -> f32 {
        self.px(12.0)
    }
    pub fn s16(&self) -> f32 {
        self.px(16.0)
    }
    pub fn s24(&self) -> f32 {
        self.px(24.0)
    }
    /// 두 줄 줄의 줄 사이
    pub fn line_gap(&self) -> f32 {
        self.px(2.0)
    }
    /// 목록 줄 사이
    pub fn row_gap(&self) -> f32 {
        self.px(2.0)
    }
    pub fn icon(&self) -> f32 {
        self.px(14.0)
    }
    /// 단추 한 변(눌리기에 충분한 크기)
    pub fn hit(&self) -> f32 {
        self.px(MIN_HIT)
    }
    /// 제목 줄 높이(탭이 들어간다)
    pub fn titlebar(&self) -> f32 {
        self.tab().max(self.hit()) + self.s8()
    }

    // ─── 줄 높이(내용에서 셈) ───

    fn at_least_hit(&self, h: f32) -> f32 {
        h.max(self.px(MIN_HIT))
    }

    /// 한 줄 목록 줄(테이블·쿼리·키·트리): 위아래 4 + 글 줄
    pub fn row(&self) -> f32 {
        self.at_least_hit(self.s4() * 2.0 + self.line(self.text()))
    }
    /// 이름 + 보조 줄(접속): 위아래 8 + 이름 줄 + 줄 사이 + 보조 줄
    pub fn row2(&self) -> f32 {
        self.at_least_hit(self.s8() * 2.0 + self.line(self.text()) + self.line_gap() + self.line(self.small()))
    }
    /// 묶음 머리(폴더): 한 줄 목록 줄과 같다
    pub fn group_row(&self) -> f32 {
        self.at_least_hit(self.s4() * 2.0 + self.line(self.group()))
    }
    /// 제목 줄 안 탭
    pub fn tab(&self) -> f32 {
        self.at_least_hit(self.s4() * 2.0 + self.line(self.small()))
    }
    /// 상태 줄(접속 고르기 단추가 들어간다)
    pub fn status_bar(&self) -> f32 {
        self.at_least_hit(self.s4() * 2.0 + self.line(self.small()))
    }
    /// 알약(환경 배지) 높이
    pub fn pill_h(&self) -> f32 {
        self.line(self.pill()) + self.px(2.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 모든_줄은_배율마다_내용보다_크고_눌리기에_충분하다() {
        for z in zoom::STEPS {
            let s = Scale::new(z);
            let rows = [
                ("한 줄", s.row(), s.line(s.text())),
                ("두 줄", s.row2(), s.line(s.text()) + s.line_gap() + s.line(s.small())),
                ("묶음 머리", s.group_row(), s.line(s.group())),
                ("탭", s.tab(), s.line(s.small())),
                ("상태 줄", s.status_bar(), s.line(s.small())),
            ];
            for (name, h, content) in rows {
                assert!(h >= content, "{z}배 {name}: 높이 {h} < 내용 {content}");
                assert!(h >= (MIN_HIT * z).round(), "{z}배 {name}: 높이 {h} 가 눌리기 어렵다");
            }
            // 두 줄 줄은 위아래 여백까지 담는다
            assert!(s.row2() >= s.line(s.text()) + s.line(s.small()) + 2.0 * s.s8(), "{z}배");
            assert!(s.pill_h() < s.line(s.text()) + s.s8(), "{z}배 알약이 이름 줄보다 크다");
        }
    }

    #[test]
    fn 글자_크기를_바꿔도_줄은_내용을_담는다() {
        for size in [9.0, 13.0, 14.0, 18.0, 24.0] {
            for z in zoom::STEPS {
                let s = Scale::with_font(z, size);
                assert!(
                    s.row2() >= s.line(s.text()) + s.line_gap() + s.line(s.small()) + 2.0 * s.s8() - 1.0,
                    "{size} {z}"
                );
                assert!(s.row() >= s.line(s.text()), "{size} {z}");
            }
        }
        assert_eq!(Scale::with_font(1.0, 14.0), Scale::new(1.0));
    }

    #[test]
    fn 배율_1의_눈금은_정한_값이다() {
        let s = Scale::new(1.0);
        assert_eq!((s.text(), s.small(), s.group(), s.section()), (14.0, 12.0, 12.0, 11.0));
        assert_eq!((s.s4(), s.s8(), s.s12(), s.s16(), s.s24()), (4.0, 8.0, 12.0, 16.0, 24.0));
        assert!(s.row2() >= 44.0, "두 줄 줄은 44px 이상");
        assert_eq!(Scale::new(2.0).text(), 28.0);
    }
}
