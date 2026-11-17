//! GUI 글꼴. 기본은 D2Coding 이고 `~/.config/db/gui.json` 의 `font`·`font_size` 로 바꾼다.
//! 설치된 글꼴을 먼저 쓰고, 고른 글꼴이 없으면 앱에 넣어 둔 D2Coding(SIL OFL 1.1, assets/fonts)을 등록해 쓴다.

pub const DEFAULT_FONT: &str = "D2Coding";
pub const DEFAULT_SIZE: f32 = 13.0;
const SIZE_RANGE: std::ops::RangeInclusive<f32> = 9.0..=24.0;

/// 앱에 넣어 둔 D2Coding 1.3.2 정식 배포본(고치지 않음)
pub const D2CODING_REGULAR: &[u8] = include_bytes!("../../assets/fonts/D2Coding-Ver1.3.2-20180524.ttf");
pub const D2CODING_BOLD: &[u8] = include_bytes!("../../assets/fonts/D2CodingBold-Ver1.3.2-20180524.ttf");

#[derive(Clone, Debug, PartialEq)]
pub struct FontChoice {
    pub family: String,
    pub size: f32,
    /// 넣어 둔 D2Coding 을 등록해야 한다
    pub embed: bool,
}

/// 설정값과 설치된 글꼴 이름으로 쓸 글꼴을 정한다.
pub fn resolve(font: &str, size: f32, installed: &[String]) -> FontChoice {
    let size = if size.is_finite() {
        size.clamp(*SIZE_RANGE.start(), *SIZE_RANGE.end())
    } else {
        DEFAULT_SIZE
    };
    let font = font.trim();
    let has = |f: &str| installed.iter().any(|i| i == f);
    if !font.is_empty() && has(font) {
        return FontChoice {
            family: font.to_string(),
            size,
            embed: false,
        };
    }
    // 고른 글꼴이 없으면(비었거나 설치 안 됨) D2Coding 으로 떨어진다
    FontChoice {
        family: DEFAULT_FONT.to_string(),
        size,
        embed: !has(DEFAULT_FONT),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// sfnt `name` 표에서 글꼴 가족 이름(nameID 1, Windows 유니코드)을 읽는다. 바이트가 글꼴로 읽히는지 보는 데 쓴다.
    fn family_name(ttf: &[u8]) -> Option<String> {
        let u16_at = |o: usize| ttf.get(o..o + 2).map(|b| u16::from_be_bytes([b[0], b[1]]));
        let u32_at = |o: usize| ttf.get(o..o + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
        if u32_at(0)? != 0x0001_0000 {
            return None;
        }
        let name = (0..u16_at(4)? as usize)
            .map(|i| 12 + i * 16)
            .find(|&r| ttf.get(r..r + 4) == Some(b"name"))?;
        let table = u32_at(name + 8)? as usize;
        let (count, strings) = (u16_at(table + 2)? as usize, table + u16_at(table + 4)? as usize);
        (0..count).map(|i| table + 6 + i * 12).find_map(|r| {
            let (platform, name_id) = (u16_at(r)?, u16_at(r + 6)?);
            if platform != 3 || name_id != 1 {
                return None;
            }
            let (len, off) = (u16_at(r + 8)? as usize, u16_at(r + 10)? as usize);
            let raw = ttf.get(strings + off..strings + off + len)?;
            let units: Vec<u16> = raw.as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes(*c)).collect();
            String::from_utf16(&units).ok()
        })
    }

    #[test]
    fn 기본_설정은_D2Coding_13px_이다() {
        let p = crate::vm::runtime::Prefs::default();
        assert_eq!(p.font, DEFAULT_FONT);
        assert_eq!(p.font_size, DEFAULT_SIZE);
        let c = resolve(&p.font, p.font_size, &[]);
        assert_eq!(c.family, "D2Coding");
        assert_eq!(c.size, 13.0);
        assert!(c.embed);
    }

    #[test]
    fn 설치된_글꼴을_먼저_쓴다() {
        let installed = vec!["D2Coding".to_string(), "Menlo".to_string()];
        assert_eq!(
            resolve("D2Coding", 13.0, &installed),
            FontChoice {
                family: "D2Coding".into(),
                size: 13.0,
                embed: false
            }
        );
        assert_eq!(resolve("Menlo", 14.0, &installed).family, "Menlo");
    }

    #[test]
    fn 없는_글꼴은_넣어_둔_D2Coding_으로_떨어진다() {
        let c = resolve("NoSuchFont", 13.0, &["Menlo".into()]);
        assert_eq!(c.family, "D2Coding");
        assert!(c.embed);
        assert!(resolve("  ", 13.0, &[]).embed);
    }

    #[test]
    fn 크기는_읽을_만한_범위로_자른다() {
        assert_eq!(resolve("D2Coding", 2.0, &[]).size, 9.0);
        assert_eq!(resolve("D2Coding", 99.0, &[]).size, 24.0);
        assert_eq!(resolve("D2Coding", f32::NAN, &[]).size, DEFAULT_SIZE);
    }

    #[test]
    fn 넣어_둔_글꼴은_D2Coding_으로_읽힌다() {
        assert_eq!(family_name(D2CODING_REGULAR).as_deref(), Some("D2Coding"));
        assert!(family_name(D2CODING_BOLD).is_some_and(|f| f.starts_with("D2Coding")));
        assert_eq!(family_name(b"not a font"), None);
    }
}
