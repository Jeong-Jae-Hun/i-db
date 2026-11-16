//! 끝낼 때의 탭(제목·편집기 글·커서·연 파일·접속)을 개인 상태 폴더에 두고 다음에 되살린다(R41).
//! 편집기 글에 개인정보가 들 수 있어 레포가 아닌 $XDG_STATE_HOME/db/session.json 한 파일에 두고,
//! 프로젝트마다 절대 경로를 키로 따로 둔다. Go `internal/session` 과 같은 파일을 읽고 쓴다.

use std::collections::BTreeMap;
use std::env;
use std::fmt;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize, Serializer};
use sha2::{Digest, Sha256};

use crate::atomicfile;
use crate::history::{GoTime, blank, go_json, is_false, is_zero_int, mkdir_all, null_default, state_file};

/// 프로젝트 하나의 편집기 글 상한. 큰 덤프를 붙여 둔 탭 몇 개로 끝낼 때마다 수십 MB 를 다시 쓰지 않게 한다.
/// 넘으면 활성 탭부터 차례로 넣고 들어가지 않는 탭은 글을 비우고 dropped 로 표시한다. 되살릴 때 그 수를 알린다.
/// 연 파일이 있으면 파일에서 다시 읽고, 없으면 빈 탭이다. 앞부분만 잘라 두면 실행할 수 있는 반쪽 SQL 이 남아 자르지 않는다.
const MAX_PROJECT_TEXT: usize = 1 << 20;

/// 접속마다 남길 최근 파일 수다. 자동 열기(R44)는 앞에서부터 지운 파일을 건너뛰며 고른다
const MAX_RECENT: usize = 20;

/// 남길 최근 테이블 수다(R40.3)
const MAX_RECENT_TABLES: usize = 50;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Tab {
    pub title: String,
    /// 편집기 글. 연 파일이 있으면 저장 안 한 고침이 있을 때만(디스크 내용과 다를 때만) 둔다
    #[serde(skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub file: String,
    /// text 를 둔 파일 탭의, 저장할 때 디스크 내용 해시(disk_hash). 되살릴 때 파일이 그사이 바뀌었는지 가른다
    #[serde(skip_serializing_if = "String::is_empty")]
    pub disk: String,
    /// 접속 uuid
    #[serde(skip_serializing_if = "String::is_empty")]
    pub conn: String,
    #[serde(skip_serializing_if = "is_zero_int")]
    pub cursor: i64,
    /// 상한에 걸려 글을 비웠다
    #[serde(skip_serializing_if = "is_false")]
    pub dropped: bool,
    /// 사용자가 바꾼 탭 이름과 고정(R57). 이름이 비면 title·파일 이름을 보인다
    #[serde(skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(skip_serializing_if = "is_false")]
    pub pinned: bool,
}

/// Tab.disk 에 둘 파일 내용 해시다.
pub fn disk_hash(data: &[u8]) -> String {
    Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

/// 깨진 세션 파일을 옮겨 둘 자리다. 이전 것은 덮는다.
pub fn bad_path(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(".bad");
    PathBuf::from(p)
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub active: i64,
    /// Go 는 탭이 없으면 `null` 로 쓴다. 같은 바이트가 되게 빈 목록은 `null` 로 쓴다
    #[serde(serialize_with = "empty_as_null", deserialize_with = "null_default")]
    pub tabs: Vec<Tab>,
    /// 접속 uuid → 그 접속에서 연·실행한·저장한 파일, 최근 것부터(R44)
    #[serde(skip_serializing_if = "BTreeMap::is_empty", deserialize_with = "null_default")]
    pub recent: BTreeMap<String, Vec<Recent>>,
    pub panes: Panes,
    /// 최근 연 테이블 보기, 최근 것부터(R40.3). 명령 팔레트가 맨 위에 보인다
    #[serde(rename = "recentTables", skip_serializing_if = "Vec::is_empty", deserialize_with = "null_default")]
    pub recent_tables: Vec<RecentTable>,
}

fn empty_as_null<S: Serializer>(tabs: &[Tab], s: S) -> Result<S::Ok, S::Error> {
    if tabs.is_empty() { s.serialize_none() } else { tabs.serialize(s) }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecentTable {
    /// 접속 uuid
    pub conn: String,
    pub schema: String,
    pub table: String,
    pub at: GoTime,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Recent {
    pub file: String,
    pub at: GoTime,
}

/// 마우스로 바꾼 창 크기다(R46). 0 이면 기본 크기다
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Panes {
    #[serde(skip_serializing_if = "is_zero_int")]
    pub tree: i64,
    #[serde(skip_serializing_if = "is_zero_int")]
    pub editor: i64,
}

impl State {
    /// conn 의 최근 파일 맨 앞에 file 을 둔다. 같은 파일의 이전 기록은 지우고 MAX_RECENT 개만 남긴다.
    /// 접속이나 파일이 비면 남기지 않는다.
    pub fn touch(&mut self, conn: &str, file: &str, at: GoTime) {
        if conn.is_empty() || file.is_empty() {
            return;
        }
        let list = self.recent.entry(conn.to_string()).or_default();
        list.retain(|r| r.file != file);
        list.insert(
            0,
            Recent {
                file: file.to_string(),
                at,
            },
        );
        list.truncate(MAX_RECENT);
    }

    /// 최근 테이블 맨 앞에 conn 의 schema.table 을 둔다. 같은 테이블의 이전 기록은 지우고 MAX_RECENT_TABLES 개만 남긴다.
    /// 접속이나 테이블이 비면 남기지 않는다.
    pub fn touch_table(&mut self, conn: &str, schema: &str, table: &str, at: GoTime) {
        if conn.is_empty() || table.is_empty() {
            return;
        }
        self.recent_tables
            .retain(|r| !(r.conn == conn && r.schema == schema && r.table == table));
        self.recent_tables.insert(
            0,
            RecentTable {
                conn: conn.to_string(),
                schema: schema.to_string(),
                table: table.to_string(),
                at,
            },
        );
        self.recent_tables.truncate(MAX_RECENT_TABLES);
    }
}

/// 세션 파일 경로다. history 와 같이 XDG_STATE_HOME 이 절대 경로일 때만 따른다.
pub fn path() -> io::Result<PathBuf> {
    state_file("session.json")
}

// ─── 깨진 파일 ───

/// 상태 파일을 JSON 으로 읽을 수 없다는 표시다. io::Error 안에 실어 is_corrupt 로 가른다.
#[derive(Debug)]
pub(crate) struct Corrupt(pub(crate) String);

impl fmt::Display for Corrupt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Corrupt {}

pub(crate) fn corrupt(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, Corrupt(msg))
}

/// 깨진 파일을 bad 로 옮긴다. 옮기기도 실패하면 두 오류를 잇되 깨짐 표시는 지킨다.
pub(crate) fn move_bad(e: io::Error, path: &Path) -> io::Error {
    match fs::rename(path, bad_path(path)) {
        Ok(()) => e,
        Err(r) => corrupt(format!("{e}\n{r}")),
    }
}

/// 세션 파일을 JSON 으로 읽을 수 없어 session.json.bad 로 옮겼다는 오류인지다(Go `ErrCorrupt`).
pub fn is_corrupt(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|i| i.is::<Corrupt>())
}

// ─── 읽고 쓰기 ───

/// Go `filepath.Abs` 와 같은 프로젝트 키다. 상대 경로는 작업 폴더에 붙이고 `.`·`..`·겹친 `/` 를 글자로 정리한다.
/// Go 는 작업 폴더로 $PWD 가 같은 폴더를 가리키면 그것(심볼릭 링크를 풀지 않은 경로)을 써서 같게 맞춘다.
pub(crate) fn abs_key(dir: &Path) -> io::Result<String> {
    let s = dir.to_string_lossy();
    if dir.is_absolute() {
        return Ok(clean(&s));
    }
    Ok(clean(&format!("{}/{s}", getwd()?)))
}

fn getwd() -> io::Result<String> {
    let dot = fs::metadata(".")?;
    if let Some(pwd) = env::var_os("PWD").filter(|p| Path::new(p).is_absolute())
        && let Ok(m) = fs::metadata(&pwd)
        && m.dev() == dot.dev()
        && m.ino() == dot.ino()
    {
        return Ok(pwd.to_string_lossy().into_owned());
    }
    Ok(env::current_dir()?.to_string_lossy().into_owned())
}

/// Go `filepath.Clean` 을 `/` 로 시작하는 경로에 맞춘 것이다.
fn clean(p: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    format!("/{}", parts.join("/"))
}

/// project_dir 의 세션을 읽는다. 파일이나 그 프로젝트 항목이 없으면 None 이다.
/// 깨진 파일은 bad_path 로 옮기고 is_corrupt 인 오류를 돌려준다. 다음 save 는 새 파일에 쓴다.
/// 활성 탭 번호가 탭 범위 밖이면 0 으로 맞춘다.
pub fn load(path: &Path, project_dir: &Path) -> io::Result<Option<State>> {
    let key = abs_key(project_dir)?;
    let all = read_all(path).map_err(|e| if is_corrupt(&e) { move_bad(e, path) } else { e })?;
    let Some(mut st) = all.and_then(|mut m| m.remove(&key)) else {
        return Ok(None);
    };
    if st.active < 0 || st.active >= st.tabs.len() as i64 {
        st.active = 0;
    }
    Ok(Some(st))
}

/// 파일을 다시 읽어 project_dir 항목만 바꿔 쓴다. 다른 프로젝트 항목은 지킨다.
/// 같은 폴더 임시 파일(0600)에 다 쓴 뒤 바꿔 끼우므로 실패하면 원래 파일이 남는다. 폴더는 0700 으로 만든다.
/// 깨진 파일은 bad_path 로 옮겨 두고(다른 프로젝트 세션을 손으로 살릴 수 있게) 이 프로젝트 항목만 든 새 파일을 쓴다.
/// 단, 다시 읽기와 바꿔 끼우기 사이(밀리초)에 다른 db 프로세스가 저장한 다른 프로젝트 항목은 잃는다.
/// 끝낼 때 한 번 쓰는 파일이라 잠금을 두지 않았다(Go 판과 같다).
pub fn save(path: &Path, project_dir: &Path, st: &State) -> io::Result<()> {
    let key = abs_key(project_dir)?;
    let all = match read_all(path) {
        Err(e) if is_corrupt(&e) => {
            fs::rename(path, bad_path(path))?;
            None
        }
        r => r?,
    };
    let mut all = all.unwrap_or_default();
    let mut st = st.clone();
    let active = usize::try_from(st.active).ok().filter(|&a| a < st.tabs.len());
    let order = active.into_iter().chain((0..st.tabs.len()).filter(|&i| Some(i) != active));
    let mut budget = MAX_PROJECT_TEXT;
    for i in order {
        let t = &mut st.tabs[i];
        if t.text.len() <= budget {
            budget -= t.text.len();
            continue;
        }
        t.text.clear();
        t.disk.clear();
        t.cursor = 0;
        t.dropped = true;
    }
    all.insert(key, st);
    let data = go_json(&all, true, false)?;
    mkdir_all(path.parent().unwrap_or(Path::new(".")), 0o700)?;
    atomicfile::write(path, ".session-*.json", &data, 0o600)
}

/// 프로젝트 절대 경로 → 세션이다. 없는 파일·빈 파일·`null` 은 None 이다.
fn read_all(path: &Path) -> io::Result<Option<BTreeMap<String, State>>> {
    let data = match fs::read(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        r => r?,
    };
    if blank(&data) {
        return Ok(None);
    }
    serde_json::from_slice(&data).map_err(|e| corrupt(format!("session file is corrupt: {}: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::state_file_from;
    use chrono::{FixedOffset, TimeDelta, TimeZone, Utc};

    fn temp_path() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state").join("db").join("session.json");
        (dir, p)
    }

    fn tabs(titles: &[&str]) -> Vec<Tab> {
        titles
            .iter()
            .map(|t| Tab {
                title: t.to_string(),
                ..Default::default()
            })
            .collect()
    }

    fn t0() -> GoTime {
        Utc.with_ymd_and_hms(2026, 11, 15, 9, 0, 0).unwrap().into()
    }

    fn add(t: GoTime, d: TimeDelta) -> GoTime {
        GoTime(t.0 + d)
    }

    fn p(s: &str) -> &Path {
        Path::new(s)
    }

    fn mode(p: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn path_xdg_state_home을_따르고_상대_경로면_홈_아래() {
        // 환경 변수는 프로세스 전체라 시험에서 바꾸지 않고 같은 규칙에 값을 넘겨 본다
        let got = state_file_from(Some("/x/state".into()), Some("/home/u".into()), "session.json").unwrap();
        assert_eq!(got, PathBuf::from("/x/state/db/session.json"));
        let got = state_file_from(Some("relative".into()), Some("/home/u".into()), "session.json").unwrap();
        assert_eq!(got, PathBuf::from("/home/u/.local/state/db/session.json"));
        assert!(path().unwrap().ends_with("db/session.json"));
    }

    #[test]
    fn load_파일이나_프로젝트가_없으면_none이다() {
        let (_d, path) = temp_path();
        assert_eq!(load(&path, p("/p")).unwrap(), None, "없는 파일");
        save(
            &path,
            p("/a"),
            &State {
                tabs: tabs(&["Query 1"]),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(load(&path, p("/b")).unwrap(), None, "없는 프로젝트");
    }

    #[test]
    fn load_깨진_파일은_bad로_옮기고_오류를_한_번_알린_뒤_새로_시작한다() {
        let (_d, path) = temp_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(bad_path(&path), "older").unwrap();
        fs::write(&path, "{broken").unwrap();
        let e = load(&path, p("/a")).unwrap_err();
        assert!(is_corrupt(&e), "깨진 파일은 corrupt: {e}");
        assert!(e.to_string().starts_with("session file is corrupt: "), "{e}");
        assert_eq!(
            fs::read_to_string(bad_path(&path)).unwrap(),
            "{broken",
            "깨진 파일을 .bad 로 옮기고 이전 .bad 는 덮는다"
        );
        assert_eq!(load(&path, p("/a")).unwrap(), None, "옮긴 뒤에는 새로 시작한다");
    }

    #[test]
    fn save_깨진_파일은_bad로_옮기고_새_파일에_쓴다() {
        let (_d, path) = temp_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{broken").unwrap();
        save(
            &path,
            p("/a"),
            &State {
                tabs: tabs(&["a"]),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(fs::read_to_string(bad_path(&path)).unwrap(), "{broken");
        assert_eq!(load(&path, p("/a")).unwrap().unwrap().tabs[0].title, "a");
    }

    #[test]
    fn save_되살리고_다른_프로젝트는_지키며_권한은_0600_0700이다() {
        let (_d, path) = temp_path();
        let mut a = State {
            active: 1,
            tabs: vec![
                Tab {
                    title: "Query 1".into(),
                    text: "select 1;\n-- 메모".into(),
                    conn: "uuid-1".into(),
                    cursor: 9,
                    ..Default::default()
                },
                Tab {
                    title: "users.sql".into(),
                    text: "select * from users".into(),
                    file: "queries/users.sql".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let b = State {
            tabs: vec![Tab {
                title: "Query 1".into(),
                text: "select 2".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        save(&path, p("/proj/a"), &a).unwrap();
        save(&path, p("/proj/b"), &b).unwrap();
        a.tabs[0].text = "바뀐 글".into();
        save(&path, p("/proj/a"), &a).unwrap();
        assert_eq!(load(&path, p("/proj/a")).unwrap(), Some(a));
        assert_eq!(load(&path, p("/proj/b")).unwrap(), Some(b));
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert_eq!(mode(&path), 0o600);
        let left: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".session-"))
            .collect();
        assert!(left.is_empty(), "임시 파일이 남았다: {left:?}");
    }

    #[test]
    fn save_프로젝트_경로는_정리한_절대_경로로_같은_항목이다() {
        let (_d, path) = temp_path();
        save(
            &path,
            p("/proj/x/../a/"),
            &State {
                tabs: tabs(&["x"]),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(load(&path, p("/proj/a")).unwrap().is_some(), "정리한 경로로 읽기");
        assert_eq!(clean("/a//b/./c/../d/"), "/a/b/d");
        assert_eq!(clean("/../.."), "/");
    }

    #[test]
    fn save_글이_상한을_넘으면_활성_탭부터_넣고_나머지_글은_비운다() {
        let (_d, path) = temp_path();
        let big = "x".repeat(MAX_PROJECT_TEXT / 2 + 1);
        let st = State {
            active: 2,
            tabs: vec![
                Tab {
                    title: "a".into(),
                    text: big.clone(),
                    cursor: 5,
                    ..Default::default()
                },
                Tab {
                    title: "b".into(),
                    text: "small".into(),
                    cursor: 3,
                    ..Default::default()
                },
                Tab {
                    title: "c".into(),
                    text: big.clone(),
                    file: "c.sql".into(),
                    cursor: 7,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        save(&path, p("/p"), &st).unwrap();
        let got = load(&path, p("/p")).unwrap().unwrap();
        assert!(got.tabs[2].text == big && got.tabs[2].cursor == 7, "활성 탭 글은 남아야 한다");
        let t = &got.tabs[0];
        assert!(
            t.text.is_empty() && t.cursor == 0 && t.title == "a" && t.dropped,
            "넘친 탭은 글만 비우고 비웠다고 표시한다: {t:?}"
        );
        assert!(
            got.tabs[1].text == "small" && !got.tabs[1].dropped && !got.tabs[2].dropped,
            "들어가는 탭은 남는다"
        );
        assert_eq!(st.tabs[0].text, big, "넘긴 State 를 바꾸지 않아야 한다");
    }

    #[test]
    fn load_활성_탭이_범위_밖이면_0이다() {
        let (_d, path) = temp_path();
        save(
            &path,
            p("/p"),
            &State {
                active: 5,
                tabs: tabs(&["a"]),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(load(&path, p("/p")).unwrap().unwrap().active, 0);
    }

    #[test]
    fn touch_최근_것부터_같은_파일은_하나로_접속마다_20개까지() {
        let mut st = State::default();
        let t0 = t0();
        st.touch("c1", "queries/a.sql", t0);
        st.touch("c1", "queries/b.sql", add(t0, TimeDelta::minutes(1)));
        st.touch("c1", "queries/a.sql", add(t0, TimeDelta::minutes(2)));
        st.touch("c2", "queries/z.sql", t0);
        st.touch("", "queries/x.sql", t0);
        st.touch("c1", "", t0);
        let want = vec![
            Recent {
                file: "queries/a.sql".into(),
                at: add(t0, TimeDelta::minutes(2)),
            },
            Recent {
                file: "queries/b.sql".into(),
                at: add(t0, TimeDelta::minutes(1)),
            },
        ];
        assert_eq!(st.recent["c1"], want);
        assert_eq!(st.recent.len(), 2, "빈 접속·파일은 남기지 않는다");
        assert_eq!(st.recent["c2"][0].file, "queries/z.sql");
        for i in 0..25 {
            st.touch("c3", &format!("f{i}.sql"), add(t0, TimeDelta::seconds(i)));
        }
        let c3 = &st.recent["c3"];
        assert!(
            c3.len() == 20 && c3[0].file == "f24.sql" && c3[19].file == "f5.sql",
            "20개까지 최근 것부터"
        );
    }

    #[test]
    fn touch_복사한_state_의_목록을_건드리지_않는다() {
        let mut a = State::default();
        a.touch("c", "x.sql", GoTime::default());
        a.touch("c", "y.sql", GoTime::default());
        let mut b = a.clone();
        b.touch("c", "x.sql", GoTime::default());
        assert!(
            a.recent["c"][0].file == "y.sql" && a.recent["c"][1].file == "x.sql",
            "원래 목록이 바뀜: {:?}",
            a.recent["c"]
        );
    }

    #[test]
    fn save_최근_파일과_창_크기를_되살리고_예전_파일도_읽는다() {
        let (_d, path) = temp_path();
        let at: GoTime = FixedOffset::east_opt(9 * 3600)
            .unwrap()
            .with_ymd_and_hms(2026, 11, 15, 9, 0, 0)
            .unwrap()
            .into();
        let mut st = State {
            tabs: tabs(&["Query 1"]),
            panes: Panes { tree: 40, editor: 12 },
            ..Default::default()
        };
        st.touch("c1", "queries/a.sql", at);
        save(&path, p("/p"), &st).unwrap();
        let got = load(&path, p("/p")).unwrap().unwrap();
        assert_eq!(got.panes, Panes { tree: 40, editor: 12 });
        assert!(got.recent["c1"].len() == 1 && got.recent["c1"][0].at == at);

        fs::write(&path, r#"{"/old":{"active":0,"tabs":[{"title":"Q"}]}}"#).unwrap();
        let old = load(&path, p("/old")).unwrap().unwrap();
        assert!(
            old.recent.is_empty() && old.panes == Panes::default() && old.tabs[0].title == "Q",
            "v5 파일: {old:?}"
        );
    }

    #[test]
    fn touch_table_최근_것부터_같은_테이블은_하나로_50개까지_저장하고_예전_파일도_읽는다() {
        let mut st = State::default();
        let t0 = t0();
        st.touch_table("c1", "public", "users", t0);
        st.touch_table("c2", "public", "users", add(t0, TimeDelta::minutes(1)));
        st.touch_table("c1", "public", "users", add(t0, TimeDelta::minutes(2)));
        st.touch_table("", "public", "x", t0);
        st.touch_table("c1", "public", "", t0);
        let rt = |c: &str, at| RecentTable {
            conn: c.into(),
            schema: "public".into(),
            table: "users".into(),
            at,
        };
        assert_eq!(
            st.recent_tables,
            vec![rt("c1", add(t0, TimeDelta::minutes(2))), rt("c2", add(t0, TimeDelta::minutes(1)))]
        );
        for i in 0..60 {
            st.touch_table("c3", "s", &format!("t{i}"), t0);
        }
        let r = &st.recent_tables;
        assert!(r.len() == 50 && r[0].table == "t59" && r[49].table == "t10", "50개까지");

        let (_d, path) = temp_path();
        save(&path, p("/p"), &st).unwrap();
        assert_eq!(
            load(&path, p("/p")).unwrap().unwrap().recent_tables,
            st.recent_tables,
            "저장·되살림"
        );
        fs::write(&path, r#"{"/old":{"active":0,"tabs":[{"title":"Q"}]}}"#).unwrap();
        assert!(load(&path, p("/old")).unwrap().unwrap().recent_tables.is_empty(), "예전 파일");
    }

    #[test]
    fn disk_hash_는_sha256_hex_다() {
        assert_eq!(disk_hash(b"x"), "2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881");
    }

    /// Go 판이 쓴 session.json 이다(internal/session.Save). 탭이 없는 프로젝트는 `"tabs":null` 이다.
    const GO_SESSION: &str = r#"{"/proj/a":{"active":1,"tabs":[{"title":"Query 1","text":"select 1 where a \u003c 2;\n-- 메모","conn":"uuid-1","cursor":9,"name":"내 탭","pinned":true},{"title":"users.sql","text":"x","file":"queries/users.sql","disk":"2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881","dropped":true}],"recent":{"uuid-1":[{"file":"queries/users.sql","at":"2026-11-16T10:15:30.12+09:00"}]},"panes":{"tree":40},"recentTables":[{"conn":"uuid-1","schema":"public","table":"users","at":"2026-11-16T01:00:00Z"}]},"/proj/empty":{"active":0,"tabs":null,"panes":{}}}"#;

    #[test]
    fn go_판이_쓴_세션을_읽고_같은_바이트로_다시_쓴다() {
        let (_d, path) = temp_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, GO_SESSION).unwrap();
        let a = load(&path, p("/proj/a")).unwrap().unwrap();
        assert_eq!(a.active, 1);
        assert_eq!(a.tabs[0].text, "select 1 where a < 2;\n-- 메모");
        assert!(a.tabs[0].pinned && a.tabs[0].name == "내 탭" && a.tabs[0].cursor == 9);
        assert!(a.tabs[1].dropped && a.tabs[1].disk == disk_hash(b"x"));
        assert_eq!(a.recent["uuid-1"][0].at.format(), "2026-11-16T10:15:30.12+09:00");
        assert_eq!(a.recent_tables[0].at.format(), "2026-11-16T01:00:00Z");
        assert_eq!(a.panes, Panes { tree: 40, editor: 0 });
        let empty = load(&path, p("/proj/empty")).unwrap().unwrap();
        assert!(empty.tabs.is_empty());
        // 다시 쓰면 Go 판과 바이트가 같다
        save(&path, p("/proj/a"), &a).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), GO_SESSION);
    }
}
