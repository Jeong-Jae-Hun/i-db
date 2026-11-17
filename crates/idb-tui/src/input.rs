//! 한 줄 입력(거르기·칸 고치기·WHERE·확인 창 프롬프트). 붙여넣은 줄바꿈·탭은 보이는 대로 공백으로 바꾼다.

use ratatui::buffer::Buffer;
use ratatui::style::{Modifier, Style};
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LineInput {
    pub text: String,
    /// 바이트 커서
    pub cur: usize,
    /// 줄바꿈을 그대로 받는다(칸 값 편집)
    pub multiline: bool,
}

impl LineInput {
    pub fn new(text: &str) -> LineInput {
        LineInput {
            text: text.to_string(),
            cur: text.len(),
            multiline: false,
        }
    }

    pub fn set(&mut self, text: &str) {
        self.text = text.to_string();
        self.cur = self.text.len();
    }

    pub fn insert(&mut self, s: &str) {
        let s = if self.multiline {
            s.replace("\r\n", "\n").replace('\r', "\n")
        } else {
            s.replace("\r\n", " ").replace(['\n', '\r', '\t'], " ")
        };
        self.text.insert_str(self.cur, &s);
        self.cur += s.len();
    }

    /// 편집 키면 처리하고 true 다. Enter·esc·Tab·위아래 같은 키는 부르는 쪽 몫이라 false 다.
    pub fn key(&mut self, k: &str) -> bool {
        match k {
            "backspace" => {
                if let Some((i, _)) = self.text[..self.cur].char_indices().next_back() {
                    self.text.replace_range(i..self.cur, "");
                    self.cur = i;
                }
            }
            "delete" => {
                if let Some(c) = self.text[self.cur..].chars().next() {
                    self.text.replace_range(self.cur..self.cur + c.len_utf8(), "");
                }
            }
            "left" => {
                if let Some((i, _)) = self.text[..self.cur].char_indices().next_back() {
                    self.cur = i;
                }
            }
            "right" => {
                if let Some(c) = self.text[self.cur..].chars().next() {
                    self.cur += c.len_utf8();
                }
            }
            "home" | "ctrl+a" => self.cur = 0,
            "end" | "ctrl+e" => self.cur = self.text.len(),
            "ctrl+u" => {
                self.text.replace_range(..self.cur, "");
                self.cur = 0;
            }
            "space" => self.insert(" "),
            _ => {
                let mut cs = k.chars();
                match (cs.next(), cs.next()) {
                    (Some(c), None) if !c.is_control() => self.insert(k),
                    _ => return false,
                }
            }
        }
        true
    }

    /// x, y 에 폭 w 로 그린다. 긴 글은 커서가 보이도록 앞을 자른다.
    pub fn render(&self, buf: &mut Buffer, x: u16, y: u16, w: usize, style: Style, cursor: bool) {
        if w == 0 {
            return;
        }
        let before = idb_core::textutil::printable(&self.text[..self.cur], false);
        let after = idb_core::textutil::printable(&self.text[self.cur..], false);
        let mut before = before.as_str();
        while before.width() + 1 > w && !before.is_empty() {
            let c = before.chars().next().unwrap();
            before = &before[c.len_utf8()..];
        }
        let bw = before.width();
        buf.set_stringn(x, y, before, w, style);
        let mut rest = after.chars();
        let glyph = rest.next().map(|c| c.to_string()).unwrap_or_else(|| " ".into());
        if bw < w {
            let cst = if cursor { style.add_modifier(Modifier::REVERSED) } else { style };
            buf.set_stringn(x + bw as u16, y, &glyph, w - bw, cst);
            let gw = glyph.width().max(1);
            if bw + gw < w {
                buf.set_stringn(x + (bw + gw) as u16, y, rest.as_str(), w - bw - gw, style);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 글자를_커서_자리에_넣고_붙여넣은_줄바꿈은_공백이다() {
        let mut i = LineInput::new("ac");
        i.key("left");
        i.key("b");
        assert_eq!(i.text, "abc");
        i.insert("x\ny");
        assert_eq!(i.text, "abx yc");
        i.key("backspace");
        i.key("end");
        i.key("한");
        assert_eq!(i.text, "abx c한");
        assert!(!i.key("enter"));
    }
}
