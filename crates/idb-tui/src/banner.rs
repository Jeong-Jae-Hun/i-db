//! 빈 결과 칸의 시작 화면: 마크(원통 위 점 = i)와 글자표, 판, 키 안내다. 강조색은 마크의 점과 글자표의 `-` 에만 쓴다.
//! NO_COLOR(비지 않은 값)면 색 없이 그린다(https://no-color.org).

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use crate::theme;

/// 브랜드 강조색 #0d9488 에 가장 가까운 256색(#008787)
pub const BRAND: Color = Color::Indexed(30);

/// 이 빌드의 판이다. 릴리스 빌드는 IDB_VERSION 으로 넣는다(idb-cli update_cmd 와 같다).
pub fn version() -> &'static str {
    option_env!("IDB_VERSION").unwrap_or("dev")
}

pub fn no_color() -> bool {
    std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty())
}

const MARK: [&str; 4] = ["  ●  ", "╭───╮", "├───┤", "╰───╯"];

/// 줄 수(넉넉하지 않으면 한 줄 안내로 접는다)
pub const HEIGHT: u16 = 4;
const WIDTH: u16 = 44;

/// r 에 시작 화면을 그린다. 자리가 모자라면 한 줄 안내만 그린다.
pub fn draw(buf: &mut Buffer, r: Rect, version: &str, plain: bool) {
    let st = |s: Style| if plain { Style::new() } else { s };
    let dim = st(theme::dim());
    if r.height < HEIGHT + 1 || r.width < WIDTH {
        buf.set_stringn(r.x, r.y, "결과가 여기에 나옵니다", r.width as usize, dim);
        return;
    }
    let ink = st(Style::new().fg(theme::TEXT)).add_modifier(Modifier::BOLD);
    let accent = st(Style::new().fg(BRAND)).add_modifier(Modifier::BOLD);
    let (x, y) = (r.x + 1, r.y + 1);
    for (i, l) in MARK.iter().enumerate() {
        let row = y + i as u16;
        if i == 0 {
            buf.set_string(x, row, "  ", ink);
            buf.set_string(x + 2, row, "●", accent);
        } else {
            buf.set_string(x, row, l, st(Style::new().fg(theme::TEXT)));
        }
    }
    let tx = x + 8;
    let room = r.width.saturating_sub(tx - r.x) as usize;
    buf.set_string(tx, y, "i", ink);
    buf.set_string(tx + 1, y, "-", accent);
    buf.set_string(tx + 2, y, "db", ink);
    buf.set_stringn(tx + 5, y, version, room.saturating_sub(5), dim);
    buf.set_stringn(tx, y + 1, "DataGrip 프로젝트를 그대로 여는 DB 도구", room, dim);
    buf.set_stringn(tx, y + 2, "Ctrl+R 실행 · Ctrl+P 팔레트 · F1 도움말", room, dim);
    buf.set_stringn(tx, y + 3, "결과가 여기에 나옵니다", room, dim);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(buf: &Buffer) -> String {
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn 시작_화면은_마크_글자표_판_키_안내를_그리고_강조색은_점과_줄표뿐이다() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 60, 6));
        let area = buf.area;
        draw(&mut buf, area, "v1.1.0", false);
        let s = text(&buf);
        assert!(s.contains("●") && s.contains("╭───╮") && s.contains("i-db v1.1.0"), "{s}");
        // 넓은 글자 뒤 칸은 빈칸으로 나와 띄어쓰기를 빼고 견준다
        let packed = s.replace(' ', "");
        assert!(packed.contains("Ctrl+R실행") && packed.contains("결과가여기에나옵니다"), "{s}");
        let accented: Vec<&str> = buf.content.iter().filter(|c| c.fg == BRAND).map(|c| c.symbol()).collect();
        assert_eq!(accented, vec!["●", "-"]);
    }

    #[test]
    fn NO_COLOR_면_색을_쓰지_않는다() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 60, 6));
        let area = buf.area;
        draw(&mut buf, area, "dev", true);
        assert!(buf.content.iter().all(|c| c.fg == Color::Reset), "색이 남았다");
        assert!(text(&buf).contains("i-db dev"));
    }

    #[test]
    fn 좁으면_한_줄_안내로_접는다() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 30, 3));
        let area = buf.area;
        draw(&mut buf, area, "dev", false);
        let s = text(&buf);
        assert!(s.replace(' ', "").starts_with("결과가여기에나옵니다") && !s.contains("●"), "{s}");
    }
}
