//! 명령 팔레트(⌘K)와 테이블로 가기(⌘P)의 고르기·순위다(TUI R40·R75.2 와 같은 뜻).
//! 빈 글이면 자주 쓴 것(frecency) 5개를 맨 위에, 그다음 종류 순서로 보인다. 글이 있으면 부분 글자(퍼지)로 걸러
//! 점수 순으로 놓고, 같은 점수면 자주 쓴 것이 앞이다. 한글은 초성(ㄱㄴㄷ)으로도 찾는다.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

/// 무엇을 하는 항목인지. 화면이 이것을 보고 실행한다(확인이 필요한 명령은 화면의 guarded 흐름을 그대로 탄다).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// vm::keymap 명령(이름은 Debug 꼴)
    Command(super::keymap::Cmd),
    Theme(super::settings::ThemePref),
    Conn(String),
    Table {
        conn: String,
        schema: String,
        table: String,
        view: bool,
    },
    File(String),
    Tab(u64),
    Key {
        conn: String,
        key: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Group {
    Frequent,
    Command,
    Conn,
    Table,
    File,
    Tab,
    Key,
}

impl Group {
    pub fn title(self) -> &'static str {
        match self {
            Group::Frequent => "자주 쓴 것",
            Group::Command => "명령",
            Group::Conn => "접속",
            Group::Table => "테이블",
            Group::File => "쿼리 파일",
            Group::Tab => "열린 탭",
            Group::Key => "Redis 키",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub group: Group,
    /// 자주 씀을 세는 열쇠(`cmd:Run`, `conn:<uuid|이름>`, `table:conn.schema.t`)
    pub key: String,
    pub title: String,
    /// 오른쪽 흐린 글(접속·스키마·경로)
    pub detail: String,
    /// 단축키 글(`⌘↵`)
    pub hint: String,
    /// 환경(prod·stg·local). 점 색에 쓴다
    pub env: String,
    pub target: Target,
}

/// 같은 이름 접속·테이블을 `#2`·`#3` 으로 가른다(R40.2).
pub fn disambiguate(items: &mut [Item]) {
    let mut seen: HashMap<(Group, String), usize> = HashMap::new();
    for it in items.iter_mut() {
        let n = seen.entry((it.group, it.title.clone())).or_insert(0);
        *n += 1;
        if *n > 1 {
            it.title = format!("{} #{n}", it.title);
        }
    }
}

// ─── 자주 씀 ───

/// 쓴 횟수와 마지막 때(유닉스 초). 7일마다 반으로 줄여 센다(idb_core::recent 와 같은 반감기).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Use {
    pub n: u32,
    pub at: i64,
}

const HALF_LIFE_SECS: f64 = 7.0 * 24.0 * 3600.0;
/// 기억할 열쇠 수. 넘으면 점수 낮은 것부터 버린다
const MAX_USES: usize = 200;

pub type Uses = BTreeMap<String, Use>;

pub fn frecency(u: &Use, now: i64) -> f64 {
    let age = (now - u.at).max(0) as f64;
    u.n as f64 * (-age / HALF_LIFE_SECS).exp2()
}

pub fn record(uses: &mut Uses, key: &str, now: i64) {
    let u = uses.entry(key.to_string()).or_default();
    u.n = u.n.saturating_add(1);
    u.at = now;
    if uses.len() > MAX_USES {
        let mut v: Vec<(String, f64)> = uses.iter().map(|(k, u)| (k.clone(), frecency(u, now))).collect();
        v.sort_by(|a, b| a.1.total_cmp(&b.1));
        for (k, _) in v.into_iter().take(uses.len() - MAX_USES) {
            uses.remove(&k);
        }
    }
}

// ─── 부분 글자 맞추기 ───

const CHOSEONG: [char; 19] = [
    'ㄱ', 'ㄲ', 'ㄴ', 'ㄷ', 'ㄸ', 'ㄹ', 'ㅁ', 'ㅂ', 'ㅃ', 'ㅅ', 'ㅆ', 'ㅇ', 'ㅈ', 'ㅉ', 'ㅊ', 'ㅋ', 'ㅌ', 'ㅍ', 'ㅎ',
];

/// 한글 음절의 초성. 음절이 아니면 None.
fn choseong(c: char) -> Option<char> {
    let u = c as u32;
    (0xAC00..=0xD7A3).contains(&u).then(|| CHOSEONG[((u - 0xAC00) / 588) as usize])
}

fn same(q: char, c: char) -> bool {
    q == c || (CHOSEONG.contains(&q) && choseong(c) == Some(q))
}

/// 한 낱말(공백 없는 글)을 text 에 맞춘다. 이어진 부분 글이면 높게, 흩어진 글자(순서만 맞음)면 낮게 친다.
/// 돌려주는 위치는 text 의 글자 번호다.
fn match_word(word: &[char], text: &[char]) -> Option<(i64, Vec<usize>)> {
    if word.is_empty() {
        return Some((0, Vec::new()));
    }
    // 이어진 부분 글: 시작이 앞일수록, 낱말 경계일수록 높다
    let boundary = |i: usize| i == 0 || matches!(text[i - 1], '_' | '-' | '.' | ' ' | '/' | ':');
    let mut best: Option<(i64, usize)> = None;
    if word.len() <= text.len() {
        for start in 0..=(text.len() - word.len()) {
            if word.iter().enumerate().all(|(k, &q)| same(q, text[start + k])) {
                let s = 1000 - start as i64 * 3 + if boundary(start) { 200 } else { 0 } + if start == 0 { 300 } else { 0 };
                if best.is_none_or(|(b, _)| s > b) {
                    best = Some((s, start));
                }
            }
        }
    }
    if let Some((s, start)) = best {
        return Some((s + word.len() as i64 * 10, (start..start + word.len()).collect()));
    }
    // 흩어진 글자: 앞에서부터 차례로 맞추고, 사이가 벌어질수록 깎는다
    let mut pos = Vec::with_capacity(word.len());
    let mut j = 0;
    for &q in word {
        while j < text.len() && !same(q, text[j]) {
            j += 1;
        }
        if j == text.len() {
            return None;
        }
        pos.push(j);
        j += 1;
    }
    let spread = pos.last().unwrap_or(&0) - pos.first().unwrap_or(&0);
    let bonus: i64 = pos.iter().filter(|&&i| boundary(i)).count() as i64 * 30;
    Some((300 - spread as i64 * 4 + bonus, pos))
}

/// 질의(공백으로 나눈 낱말 모두)가 text 에 맞으면 점수와 맞은 글자 위치다. 대소문자는 가리지 않는다.
pub fn fuzzy(query: &str, text: &str) -> Option<(i64, Vec<usize>)> {
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let mut score = 0;
    let mut hits = Vec::new();
    for w in query.to_lowercase().split_whitespace() {
        let w: Vec<char> = w.chars().collect();
        let (s, p) = match_word(&w, &t)?;
        score += s;
        hits.extend(p);
    }
    hits.sort_unstable();
    hits.dedup();
    Some((score, hits))
}

/// 고른 결과 한 줄이다. hits 는 title 안의 맞은 글자 위치(강조).
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub index: usize,
    pub group: Group,
    pub hits: Vec<usize>,
}

/// 빈 질의에서 보일 자주 쓴 항목 수(R75.2)
pub const FREQUENT: usize = 5;
/// 한 번에 보일 최대 줄 수
pub const LIMIT: usize = 80;

/// 항목을 고르고 줄 세운다. 빈 질의: 자주 쓴 것 5개(명령·접속) → 종류 순서. 질의: 점수(제목 맞음 > 설명 맞음) 순.
pub fn rank(items: &[Item], query: &str, uses: &Uses, now: i64) -> Vec<Hit> {
    let fr = |it: &Item| uses.get(&it.key).map_or(0.0, |u| frecency(u, now));
    if query.trim().is_empty() {
        let mut freq: Vec<usize> = (0..items.len())
            .filter(|&i| matches!(items[i].group, Group::Command | Group::Conn) && fr(&items[i]) > 0.0)
            .collect();
        freq.sort_by(|&a, &b| fr(&items[b]).total_cmp(&fr(&items[a])));
        freq.truncate(FREQUENT);
        let mut out: Vec<Hit> = freq
            .iter()
            .map(|&i| Hit {
                index: i,
                group: Group::Frequent,
                hits: Vec::new(),
            })
            .collect();
        let mut rest: Vec<usize> = (0..items.len()).filter(|i| !freq.contains(i)).collect();
        rest.sort_by_key(|&i| items[i].group);
        out.extend(rest.into_iter().map(|i| Hit {
            index: i,
            group: items[i].group,
            hits: Vec::new(),
        }));
        out.truncate(LIMIT);
        return out;
    }
    let mut scored: Vec<(i64, f64, Hit)> = items
        .iter()
        .enumerate()
        .filter_map(|(i, it)| {
            // 제목에서 맞으면 설명에서만 맞은 것보다 늘 앞이다
            let title = fuzzy(query, &it.title).map(|(s, h)| (s + 5000, h));
            let detail = || fuzzy(query, &format!("{} {}", it.title, it.detail)).map(|(s, _)| (s, Vec::new()));
            let (s, hits) = title.or_else(detail)?;
            Some((
                s,
                fr(it),
                Hit {
                    index: i,
                    group: it.group,
                    hits,
                },
            ))
        })
        .collect();
    // 종류별로 묶는다(같은 종류가 두 번 나뉘어 보이지 않게). 종류 순서는 그 종류의 가장 좋은 점수 순이고, 안에서는 점수 순이다
    let mut best: HashMap<Group, i64> = HashMap::new();
    for (s, _, h) in &scored {
        let b = best.entry(h.group).or_insert(*s);
        *b = (*b).max(*s);
    }
    scored.sort_by(|a, b| {
        best[&b.2.group]
            .cmp(&best[&a.2.group])
            .then(a.2.group.cmp(&b.2.group))
            .then(b.0.cmp(&a.0))
            .then(b.1.total_cmp(&a.1))
            .then(a.2.index.cmp(&b.2.index))
    });
    scored.into_iter().take(LIMIT).map(|(_, _, h)| h).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::keymap::Cmd;

    fn item(group: Group, title: &str, detail: &str) -> Item {
        Item {
            group,
            key: format!("{group:?}:{title}"),
            title: title.into(),
            detail: detail.into(),
            hint: String::new(),
            env: String::new(),
            target: Target::Command(Cmd::Run),
        }
    }

    #[test]
    fn 이어진_글이_흩어진_글보다_앞이고_앞쪽·경계가_높다() {
        let (a, _) = fuzzy("ord", "orders").unwrap();
        let (b, _) = fuzzy("ord", "order_items").unwrap();
        let (c, _) = fuzzy("ord", "coupon_redeemed").unwrap();
        assert!(a >= b && b > c, "{a} {b} {c}");
        let (d, _) = fuzzy("items", "order_items").unwrap();
        let (e, _) = fuzzy("tems", "order_items").unwrap();
        assert!(d > e, "낱말 경계가 높다");
        assert_eq!(fuzzy("xyz", "orders"), None);
        assert_eq!(fuzzy("OrD", "orders").unwrap().1, vec![0, 1, 2], "대소문자를 가리지 않는다");
    }

    #[test]
    fn 여러_낱말은_모두_맞아야_하고_한글은_초성으로도_찾는다() {
        assert!(fuzzy("app users", "app-stg · public.users").is_some());
        assert!(fuzzy("app nope", "app-stg · public.users").is_none());
        assert_eq!(fuzzy("ㅅㅎ", "실행 (커서 문장·선택)").unwrap().1, vec![0, 1]);
        assert!(fuzzy("ㄷㅅㅂㄷ", "대시보드").is_some());
        assert!(fuzzy("대시", "대시보드 (접속 상태)").is_some());
    }

    #[test]
    fn 빈_질의는_자주_쓴_5개를_먼저_두고_종류_순서다() {
        let items = vec![
            item(Group::Table, "orders", "app-stg"),
            item(Group::Command, "실행", ""),
            item(Group::Conn, "app-stg", ""),
            item(Group::Command, "새 탭", ""),
        ];
        let mut uses = Uses::new();
        let now = 1_800_000_000;
        record(&mut uses, &items[2].key, now);
        record(&mut uses, &items[2].key, now);
        record(&mut uses, &items[3].key, now - 30 * 24 * 3600);
        let r = rank(&items, "", &uses, now);
        assert_eq!(r[0].index, 2, "자주 쓴 접속이 맨 위");
        assert_eq!(r[0].group, Group::Frequent);
        assert_eq!(r[1].index, 3, "오래전에 쓴 것도 자주 쓴 것에 든다(점수 순)");
        assert_eq!(
            r.iter().skip(2).map(|h| h.group).collect::<Vec<_>>(),
            vec![Group::Command, Group::Table]
        );
    }

    #[test]
    fn 질의는_제목_점수가_먼저고_같으면_자주_쓴_것이_앞이다() {
        let items = vec![
            item(Group::Table, "users", "billing-local · public"),
            item(Group::Table, "users", "app-stg · public"),
            item(Group::Table, "user_roles", "app-stg · public"),
            item(Group::Command, "새 탭", "users 와 상관없음"),
        ];
        let mut uses = Uses::new();
        // 제목이 같아 점수가 같다 → 자주 씀으로 가른다
        let mut items2 = items.clone();
        items2[1].key = "table:app-stg.public.users".into();
        record(&mut uses, "table:app-stg.public.users", 100);
        let r = rank(&items2, "users", &uses, 100);
        assert_eq!(r[0].index, 1, "자주 쓴 users 가 앞");
        assert_eq!(r[1].index, 0);
        assert_eq!(r[2].index, 2);
        assert_eq!(r.last().unwrap().index, 3, "설명에서만 맞으면 뒤");
        assert!(r[3].hits.is_empty());
    }

    #[test]
    fn 질의_결과는_종류별로_한_번씩만_묶인다() {
        let items = vec![
            item(Group::Table, "orders", ""),
            item(Group::Command, "새 order 열기", ""),
            item(Group::Table, "zz_order_log", ""),
            item(Group::File, "orders.sql", ""),
        ];
        let r = rank(&items, "order", &Uses::new(), 0);
        let groups: Vec<Group> = r.iter().map(|h| h.group).collect();
        let mut seen = Vec::new();
        for g in &groups {
            if seen.last() != Some(g) {
                assert!(!seen.contains(g), "종류가 나뉘었다: {groups:?}");
                seen.push(*g);
            }
        }
        assert_eq!(r[0].index, 0, "가장 좋은 점수의 종류가 먼저");
        assert_eq!(r[1].index, 2, "같은 종류는 붙어 있다");
    }

    #[test]
    fn 같은_이름은_번호를_붙이고_자주_씀은_반감기로_줄어든다() {
        let mut items = vec![
            item(Group::Conn, "app", ""),
            item(Group::Conn, "app", ""),
            item(Group::Table, "app", ""),
        ];
        disambiguate(&mut items);
        assert_eq!(items[1].title, "app #2");
        assert_eq!(items[2].title, "app", "종류가 다르면 그대로");
        let u = Use { n: 4, at: 0 };
        assert!((frecency(&u, 7 * 24 * 3600) - 2.0).abs() < 1e-9);
        let mut uses = Uses::new();
        for i in 0..(MAX_USES + 5) {
            record(&mut uses, &format!("k{i}"), i as i64);
        }
        assert_eq!(uses.len(), MAX_USES);
        assert!(!uses.contains_key("k0"), "점수 낮은 것을 버린다");
    }
}
