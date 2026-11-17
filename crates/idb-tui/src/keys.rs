//! 키 이름과 맥 ⌘ 키(R23·R47). 화면은 키를 bubbletea 꼴 이름(`ctrl+r`·`alt+e`·`f5`·`a`)으로 다룬다.
//! Go 와 같은 이름을 써서 도움말 항목(help.rs)이 팔레트가 다시 누를 키를 그대로 싣는다.
//! kitty 키 프로토콜이 켜지면 터미널이 ⌘ 를 SUPER 로 넘기고, 여기서 같은 동작의 Ctrl·Alt·F 키로 옮긴다.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// ⌘ 조합 하나다. labels 는 같은 동작의 기존 키 안내 글자, mac 은 맥에서 대신 보일 글자다.
/// labels 가 비면 받기만 하는 별칭이다(⌘↩ 는 ⌘R 과 같다). 같은 label 이 여럿이면 앞 줄의 mac 을 보인다.
struct Binding {
    /// 소문자 글자(또는 '\r'·'\x7f')
    key: char,
    shift: bool,
    alt: bool,
    name: &'static str,
    labels: &'static [&'static str],
    mac: &'static str,
}

const fn b(key: char, shift: bool, alt: bool, name: &'static str, labels: &'static [&'static str], mac: &'static str) -> Binding {
    Binding {
        key,
        shift,
        alt,
        name,
        labels,
        mac,
    }
}

/// 표에 없는 ⌘ 조합은 버린다(R23.2). F 키는 맥에서 fn 을 같이 눌러야 해서 ⌘ 대안을 둔다(R47.2).
const BINDINGS: &[Binding] = &[
    b('r', false, false, "ctrl+r", &["Ctrl+R", "F5"], "⌘R"),
    b('\r', false, false, "ctrl+r", &[], ""),
    b('e', false, false, "ctrl+e", &["Ctrl+E"], "⌘E"),
    b('e', false, true, "alt+e", &["Alt+E"], "⌥⌘E"),
    b('s', false, false, "ctrl+s", &["Ctrl+S"], "⌘S"),
    b('o', false, false, "ctrl+o", &["Ctrl+O"], "⌘O"),
    b('/', false, false, "ctrl+/", &["Ctrl+/"], "⌘/"),
    b('z', false, false, "ctrl+z", &["Ctrl+Z"], "⌘Z"),
    b('z', true, false, "ctrl+y", &["Ctrl+Y"], "⌘⇧Z"),
    b('d', false, false, "ctrl+d", &["Ctrl+D"], "⌘D"),
    b('k', false, false, "ctrl+k", &["Ctrl+K"], "⌘K"),
    b('a', false, false, "ctrl+a", &["Ctrl+A"], "⌘A"),
    b('b', false, false, "ctrl+b", &["Ctrl+B", "F12"], "⌘B"),
    b('p', false, false, "ctrl+p", &["Ctrl+P"], "⌘P"),
    b('t', false, false, "ctrl+t", &["Ctrl+T"], "⌘T"),
    b('w', false, false, "ctrl+w", &["Ctrl+W"], "⌘W"),
    b('n', false, false, "ctrl+n", &["Ctrl+N"], "⌘N"),
    b('h', false, false, "ctrl+h", &["Ctrl+H"], "⌘H"),
    b('1', false, false, "alt+1", &["Alt+1"], "⌘1"),
    b('2', false, false, "alt+2", &["Alt+2"], "⌘2"),
    b('3', false, false, "alt+3", &["Alt+3"], "⌘3"),
    b('4', false, false, "alt+4", &["Alt+4"], "⌘4"),
    b('5', false, false, "alt+5", &["Alt+5"], "⌘5"),
    b('6', false, false, "alt+6", &["Alt+6"], "⌘6"),
    b('7', false, false, "alt+7", &["Alt+7"], "⌘7"),
    b('8', false, false, "alt+8", &["Alt+8"], "⌘8"),
    b('9', false, false, "alt+9", &["Alt+9"], "⌘9"),
    b('/', true, false, "f1", &["F1"], "⌘?"),
    b('?', true, false, "f1", &[], ""),
    b('?', false, false, "f1", &[], ""),
    b('o', true, false, "f3", &["F3"], "⌘⇧O"),
    b('s', true, false, "f4", &["F4"], "⌘⇧S"),
    b('d', true, false, "f6", &["F6"], "⌘⇧D"),
    b('e', true, false, "f7", &["F7"], "⌘⇧E"),
    b('l', false, true, "f8", &["F8"], "⌥⌘L"),
    b('f', true, false, "f8", &[], ""),
    b('m', true, false, "f9", &["F9"], "⌘⇧M"),
    b('\r', true, false, "f10", &["F10"], "⌘⇧↩"),
    b('\x7f', true, false, "f11", &["F11"], "⌘⇧⌫"),
    // ⌘V 는 터미널이 붙여넣기로 보내 표에 두지 않는다
    b('u', false, false, "ctrl+u", &["Ctrl+U"], "⌘U"),
    b('l', false, false, "ctrl+l", &["Ctrl+L"], "⌘L"),
    b('c', true, false, "alt+c", &["Alt+C"], "⌘⇧C"),
    b('r', true, false, "alt+r", &["Alt+R"], "⌘⇧R"),
    b('p', true, false, "alt+p", &["Alt+P"], "⌘⇧P"),
    b('g', false, false, "ctrl+g", &["Ctrl+G"], "⌘G"),
    b('b', true, false, "alt+b", &["Alt+B"], "⌘⇧B"),
];

/// 키를 이름으로 바꾼다. 놓기(release) 이벤트와 표에 없는 ⌘ 조합은 None 이다(R23.2).
pub fn name(k: &KeyEvent) -> Option<String> {
    if k.kind == KeyEventKind::Release {
        return None;
    }
    let m = k.modifiers;
    if m.contains(KeyModifiers::SUPER) {
        return super_name(k);
    }
    let ctrl = m.contains(KeyModifiers::CONTROL);
    let alt = m.contains(KeyModifiers::ALT);
    let shift = m.contains(KeyModifiers::SHIFT);
    let base = match k.code {
        KeyCode::Char(c) => {
            if ctrl {
                // 레거시 터미널은 Ctrl+Space·/·] 를 0x00·0x1f·0x1d 로 보내고 crossterm 이 숫자로 읽는다
                let n = match c {
                    ' ' | '@' | '2' => "ctrl+@".to_string(),
                    '/' | '_' | '7' => "ctrl+/".to_string(),
                    ']' | '5' => "ctrl+]".to_string(),
                    c => format!("ctrl+{}", c.to_ascii_lowercase()),
                };
                return Some(if alt { format!("alt+{n}") } else { n });
            }
            let s = if c == ' ' { "space".to_string() } else { c.to_string() };
            return Some(if alt { format!("alt+{s}") } else { s });
        }
        KeyCode::Enter => "enter".into(),
        KeyCode::Esc => "esc".into(),
        KeyCode::Tab => {
            return Some(if shift { "shift+tab" } else { "tab" }.into());
        }
        KeyCode::BackTab => return Some("shift+tab".into()),
        KeyCode::Backspace => "backspace".into(),
        KeyCode::Delete => "delete".into(),
        KeyCode::Insert => "insert".into(),
        KeyCode::Up => "up".into(),
        KeyCode::Down => "down".into(),
        KeyCode::Left => "left".into(),
        KeyCode::Right => "right".into(),
        KeyCode::Home => "home".into(),
        KeyCode::End => "end".into(),
        KeyCode::PageUp => "pgup".into(),
        KeyCode::PageDown => "pgdown".into(),
        KeyCode::F(n) => format!("f{n}"),
        _ => return None,
    };
    let mut s = String::new();
    if ctrl {
        s.push_str("ctrl+");
    }
    if alt {
        s.push_str("alt+");
    }
    if shift {
        s.push_str("shift+");
    }
    s.push_str(&base);
    Some(s)
}

fn super_name(k: &KeyEvent) -> Option<String> {
    let m = k.modifiers;
    let shift = m.contains(KeyModifiers::SHIFT);
    let alt = m.contains(KeyModifiers::ALT);
    // ⌘ 방향키는 줄 처음·끝(←→), 문서 처음·끝(↑↓)이다
    match k.code {
        KeyCode::Left => return Some("home".into()),
        KeyCode::Right => return Some("end".into()),
        KeyCode::Up => return Some("ctrl+home".into()),
        KeyCode::Down => return Some("ctrl+end".into()),
        _ => {}
    }
    let (key, shift) = match k.code {
        KeyCode::Char(c) if c.is_ascii_uppercase() => (c.to_ascii_lowercase(), true),
        KeyCode::Char(c) => (c, shift),
        KeyCode::Enter => ('\r', shift),
        KeyCode::Backspace => ('\x7f', shift),
        _ => return None,
    };
    BINDINGS
        .iter()
        .find(|bd| bd.key == key && bd.shift == shift && bd.alt == alt)
        .map(|bd| bd.name.to_string())
}

/// 맥 글자로 보일지다(R47.1). `DB_KEYS` 가 먼저고, 아니면 macOS 거나 kitty 키 프로토콜이 켜졌을 때다.
pub fn mac_style(kitty_on: bool) -> bool {
    match std::env::var("DB_KEYS").as_deref() {
        Ok("cmd") => true,
        Ok("ctrl") => false,
        _ => cfg!(target_os = "macos") || kitty_on,
    }
}

/// kitty 키 감싸기를 켤지다(R23.3). `DB_KITTY_KEYS` 가 먼저고, 맥이거나 프로토콜을 아는 터미널에서만 켠다.
pub fn kitty_wanted() -> bool {
    match std::env::var("DB_KITTY_KEYS").as_deref() {
        Ok("1") => return true,
        Ok("0") => return false,
        _ => {}
    }
    if std::env::var_os("WT_SESSION").is_some() {
        return false;
    }
    let tp = std::env::var("TERM_PROGRAM").unwrap_or_default();
    let term = std::env::var("TERM").unwrap_or_default();
    cfg!(target_os = "macos")
        || matches!(tp.as_str(), "ghostty" | "WezTerm" | "iTerm.app" | "kitty")
        || matches!(term.as_str(), "xterm-kitty" | "xterm-ghostty")
}

fn mac_label(word: &str) -> Option<&'static str> {
    BINDINGS.iter().find(|bd| bd.labels.contains(&word)).map(|bd| bd.mac)
}

const LABEL_MODS: &[(&str, &str)] = &[("Ctrl+", "⌃"), ("Alt+", "⌥"), ("Shift+", "⇧")];

/// mac 이면 s 안의 키 안내(Ctrl+R·Alt+E·F3)를 맥 글자로 바꾼다. ⌘ 표에 있는 키는 그 ⌘ 조합으로,
/// 없는 수정자 조합은 ⌃⌥⇧ 기호로, 표에 없는 F 키는 그대로 둔다. 같은 ⌘ 키가 되는 대안(`Ctrl+R / F5`)은 하나로 줄인다.
/// 사용자 값(탭 제목·오류·DB 값)이 섞인 글에는 쓰지 않는다. F5 같은 낱말까지 키로 읽어 바꾸기 때문이다.
pub fn label(s: &str, mac: bool) -> String {
    if !mac {
        return s.to_string();
    }
    let mut out = String::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < s.len() {
        if i > 0 && bytes[i - 1].is_ascii_alphanumeric() {
            let ch = s[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        let mut j = i;
        let mut mods = String::new();
        let mut matched = true;
        while matched {
            matched = false;
            for (m, sym) in LABEL_MODS {
                if s[j..].starts_with(m) {
                    mods.push_str(sym);
                    j += m.len();
                    matched = true;
                }
            }
        }
        let mut k = j;
        while k < s.len() && bytes[k].is_ascii_alphanumeric() {
            k += 1;
        }
        if k == j && j > i && j < s.len() && bytes[j] != b' ' {
            k = j + s[j..].chars().next().unwrap().len_utf8();
        }
        if let Some(l) = mac_label(&s[i..k]) {
            out.push_str(l);
            i = k;
            continue;
        }
        if j == i {
            let ch = s[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        out.push_str(&mods);
        out.push_str(&s[j..k]);
        i = k;
    }
    // `⌘R / ⌘R` 처럼 겹친 대안은 하나로 줄인다
    let mut parts: Vec<&str> = out.split(" / ").collect();
    parts.dedup();
    parts.join(" / ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, m)
    }

    #[test]
    fn 수정자와_특수_키는_bubbletea_이름이다() {
        assert_eq!(name(&key(KeyCode::Char('r'), KeyModifiers::CONTROL)).unwrap(), "ctrl+r");
        assert_eq!(name(&key(KeyCode::Char('A'), KeyModifiers::SHIFT)).unwrap(), "A");
        assert_eq!(name(&key(KeyCode::Char('e'), KeyModifiers::ALT)).unwrap(), "alt+e");
        assert_eq!(name(&key(KeyCode::Char(' '), KeyModifiers::CONTROL)).unwrap(), "ctrl+@");
        assert_eq!(name(&key(KeyCode::Char('7'), KeyModifiers::CONTROL)).unwrap(), "ctrl+/");
        assert_eq!(name(&key(KeyCode::Up, KeyModifiers::CONTROL)).unwrap(), "ctrl+up");
        assert_eq!(name(&key(KeyCode::BackTab, KeyModifiers::SHIFT)).unwrap(), "shift+tab");
        assert_eq!(name(&key(KeyCode::F(5), KeyModifiers::NONE)).unwrap(), "f5");
        assert_eq!(name(&key(KeyCode::Char(' '), KeyModifiers::NONE)).unwrap(), "space");
    }

    #[test]
    fn 커맨드_조합은_같은_동작의_키로_바꾸고_표에_없으면_버린다() {
        let s = KeyModifiers::SUPER;
        assert_eq!(name(&key(KeyCode::Char('r'), s)).unwrap(), "ctrl+r");
        assert_eq!(name(&key(KeyCode::Enter, s)).unwrap(), "ctrl+r");
        assert_eq!(name(&key(KeyCode::Char('e'), s | KeyModifiers::ALT)).unwrap(), "alt+e");
        assert_eq!(name(&key(KeyCode::Char('z'), s | KeyModifiers::SHIFT)).unwrap(), "ctrl+y");
        assert_eq!(name(&key(KeyCode::Char('F'), s)).unwrap(), "f8");
        assert_eq!(name(&key(KeyCode::Char('1'), s)).unwrap(), "alt+1");
        assert_eq!(name(&key(KeyCode::Left, s)).unwrap(), "home");
        assert_eq!(name(&key(KeyCode::Char('q'), s)), None);
    }

    #[test]
    fn 키_안내는_맥이면_커맨드_글자로_보인다() {
        assert_eq!(label("Ctrl+R 실행 · F8 정렬", true), "⌘R 실행 · ⌥⌘L 정렬");
        assert_eq!(label("Ctrl+R / F5", true), "⌘R");
        assert_eq!(label("Ctrl+Space 자동완성", true), "⌃Space 자동완성");
        assert_eq!(label("Ctrl+← / Ctrl+→", true), "⌃← / ⌃→");
        assert_eq!(label("Alt+1..9", true), "⌘1..9");
        assert_eq!(label("F2 고치기", true), "F2 고치기");
        assert_eq!(label("Ctrl+R 실행", false), "Ctrl+R 실행");
    }
}
