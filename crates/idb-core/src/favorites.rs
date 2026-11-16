//! 이름 붙인 SQL 을 프로젝트의 .dbtui/favorites.json 에 둔다(R29). Go `internal/favorites` 와 같은 파일을 읽고 쓴다.
//! 파일은 커밋해 여러 기기가 같이 쓰므로, 이름순으로 정렬해 쓰고 두 칸 들여쓴다. 넣은 순서로 쓰면
//! 두 기기가 각자 끝에 붙인 변경이 매번 같은 줄에서 충돌하고, 다시 쓸 때마다 순서가 흔들린다.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};

use crate::atomicfile;
use crate::history::{GoTime, blank, go_json, mkdir_all};

const DIR_NAME: &str = ".dbtui";
const FILE_NAME: &str = "favorites.json";
/// 목록 한 줄에 들어갈 만큼. 글자(룬) 수다
const MAX_NAME_LEN: usize = 100;
const DIR_PERM: u32 = 0o755;
/// 새 파일 권한. 커밋하는 파일이라 비밀이 아니다(R29.3)
const FILE_PERM: u32 = 0o644;
const TEMP_PATTERN: &str = ".favorites-*.json";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Fav {
    pub name: String,
    /// 저장 당시 활성 접속 uuid. 없으면 빈 글
    pub conn: String,
    pub sql: String,
    pub saved: GoTime,
}

/// 마지막으로 읽거나 쓴 즐겨찾기를 이름순으로 들고 있다. 메서드는 동시에 불러도 된다.
/// put·delete 는 파일을 다시 읽어 그 위에 바꾸므로 다른 프로세스가 그사이 저장한 다른 이름은 지키고,
/// 같은 이름은 나중에 쓴 쪽이 이긴다. 다시 읽기와 바꿔 끼우기 사이(밀리초)에 다른 프로세스가 쓴 변경은 잃는다.
/// 사람이 키를 눌러 저장하는 빈도라 잠금 파일(커밋 폴더에 남는다)을 두지 않았다. 잃으면 안 되게 되면 history 처럼 flock 을 건다.
pub struct Store {
    path: PathBuf,
    favs: Mutex<Vec<Fav>>,
}

/// 프로젝트 폴더의 즐겨찾기 파일 경로다.
pub fn path(project_dir: &Path) -> PathBuf {
    project_dir.join(DIR_NAME).join(FILE_NAME)
}

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg)
}

/// 앞뒤 공백을 떼고 이름으로 쓸 수 있는지 본다. 부르는 쪽이 덮어쓰기를 묻기 전에 같은 규칙으로 고를 수 있게 연다.
pub fn normalize_name(name: &str) -> io::Result<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(invalid("favorite name is empty".into()));
    }
    if name.chars().count() > MAX_NAME_LEN {
        return Err(invalid(format!("favorite name is longer than {MAX_NAME_LEN} characters")));
    }
    // 줄바꿈이 들어가면 목록 한 줄이 깨진다
    if name.chars().any(char::is_control) {
        return Err(invalid("favorite name contains control characters".into()));
    }
    Ok(name.to_string())
}

impl Store {
    /// 파일이 없으면 빈 Store 를 돌려주고 폴더를 만들지 않는다(첫 저장 때 만든다). 깨진 파일은 오류다.
    pub fn load(project_dir: &Path) -> io::Result<Store> {
        let path = path(project_dir);
        let favs = read_file(&path)?;
        Ok(Store {
            path,
            favs: Mutex::new(favs),
        })
    }

    fn favs(&self) -> std::sync::MutexGuard<'_, Vec<Fav>> {
        self.favs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 이름순 사본이다.
    pub fn list(&self) -> Vec<Fav> {
        self.favs().clone()
    }

    /// 이름의 앞뒤 공백을 떼고 찾는다. 있는지만 볼 때도 이것을 쓴다.
    pub fn get(&self, name: &str) -> Option<Fav> {
        let name = name.trim();
        let favs = self.favs();
        favs.binary_search_by(|f| f.name.as_str().cmp(name)).ok().map(|i| favs[i].clone())
    }

    /// 같은 이름이 있으면 덮어쓴다. 덮어쓸지 묻는 것은 부르는 쪽 몫이다(R29.1).
    /// saved 가 비면 지금 시각을 넣고, 초 아래는 버린다(형식 예시 2026-11-14T20:30:00+09:00 와 맞춘다).
    pub fn put(&self, mut f: Fav) -> io::Result<()> {
        f.name = normalize_name(&f.name)?;
        if f.sql.trim().is_empty() {
            return Err(invalid("favorite sql is empty".into()));
        }
        if f.saved.is_zero() {
            f.saved = GoTime::now();
        }
        f.saved = f.saved.truncate_secs();
        let mut mem = self.favs();
        let mut favs = read_file(&self.path)?;
        match favs.binary_search_by(|x| x.name.cmp(&f.name)) {
            Ok(i) => favs[i] = f,
            Err(i) => favs.insert(i, f),
        }
        self.write_locked(&mut mem, favs)
    }

    /// 없는 이름이면 파일을 건드리지 않고 Ok 다(다른 프로세스가 먼저 지웠을 수 있다).
    pub fn delete(&self, name: &str) -> io::Result<()> {
        let name = name.trim();
        let mut mem = self.favs();
        let mut favs = read_file(&self.path)?;
        let Ok(i) = favs.binary_search_by(|f| f.name.as_str().cmp(name)) else {
            *mem = favs;
            return Ok(());
        };
        favs.remove(i);
        self.write_locked(&mut mem, favs)
    }

    /// 같은 폴더 임시 파일에 다 쓴 뒤 바꿔 끼운다(R5.2). 실패하면 원본이 그대로 남는다.
    fn write_locked(&self, mem: &mut Vec<Fav>, favs: Vec<Fav>) -> io::Result<()> {
        // SQL 의 < > & 가 < 꼴로 바뀌면 git diff 를 읽을 수 없어 HTML 이스케이프를 끈다. 다 지워도 null 이 아니라 [] 다
        let mut data = go_json(&favs, false, true)?;
        data.push(b'\n');
        mkdir_all(self.path.parent().unwrap_or(Path::new(".")), DIR_PERM)?;
        // 있던 파일은 그 권한을 따르고(R5.2) 새 파일만 0644 다
        let perm = fs::metadata(&self.path).map_or(FILE_PERM, |m| m.permissions().mode() & 0o777);
        atomicfile::write(&self.path, TEMP_PATTERN, &data, perm)?;
        *mem = favs;
        Ok(())
    }
}

/// 이름순으로 돌려준다. 손으로 고치거나 git 병합으로 같은 이름이 둘 생기면 뒤의 것을 남긴다.
/// 없는 파일·빈 파일은 빈 목록이다.
fn read_file(path: &Path) -> io::Result<Vec<Fav>> {
    let data = match fs::read(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        r => r?,
    };
    if blank(&data) {
        return Ok(Vec::new());
    }
    // 깨진 파일 위에 새로 쓰면 남은 즐겨찾기를 다 잃으므로 읽기·쓰기 모두 멈춘다
    let raw: Option<Vec<Fav>> = serde_json::from_slice(&data).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("read {}: {}", path.display(), crate::gojson::json_error(&e)),
        )
    })?;
    let by_name: BTreeMap<String, Fav> = raw.unwrap_or_default().into_iter().map(|f| (f.name.clone(), f)).collect();
    Ok(by_name.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, TimeDelta, TimeZone, Utc};

    fn names(favs: &[Fav]) -> String {
        favs.iter().map(|f| f.name.as_str()).collect::<Vec<_>>().join("|")
    }

    fn fav(name: &str, sql: &str) -> Fav {
        Fav {
            name: name.into(),
            sql: sql.into(),
            ..Default::default()
        }
    }

    fn mode(p: &Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn load_파일이_없으면_빈_목록이고_폴더를_만들지_않는다() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::load(dir.path()).unwrap();
        assert!(s.list().is_empty());
        assert!(!dir.path().join(".dbtui").exists(), ".dbtui 를 만들었다");
    }

    #[test]
    fn put_이름순으로_두_칸_들여써_저장하고_권한은_0755_0644다() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::load(dir.path()).unwrap();
        let kst = FixedOffset::east_opt(9 * 3600).unwrap();
        let saved = GoTime(kst.with_ymd_and_hms(2026, 11, 14, 20, 30, 0).unwrap() + TimeDelta::nanoseconds(123));
        let sql1 = "select count(*) from users where created_at > now() - '1 day'::interval";
        s.put(Fav {
            name: "  오늘 가입자 ".into(),
            conn: "u1".into(),
            sql: sql1.into(),
            saved,
        })
        .unwrap();
        s.put(Fav {
            name: "a<b".into(),
            conn: "u2".into(),
            sql: "select 1 where 1 < 2 && true".into(),
            saved,
        })
        .unwrap();
        let data = fs::read_to_string(path(dir.path())).unwrap();
        let want = format!(
            r#"[
  {{
    "name": "a<b",
    "conn": "u2",
    "sql": "select 1 where 1 < 2 && true",
    "saved": "2026-11-14T20:30:00+09:00"
  }},
  {{
    "name": "오늘 가입자",
    "conn": "u1",
    "sql": "{sql1}",
    "saved": "2026-11-14T20:30:00+09:00"
  }}
]
"#
        );
        assert_eq!(data, want);
        assert_eq!(mode(path(dir.path()).parent().unwrap()), 0o755);
        assert_eq!(mode(&path(dir.path())), 0o644);
        assert_eq!(names(&Store::load(dir.path()).unwrap().list()), "a<b|오늘 가입자", "다시 읽은 목록");
    }

    #[test]
    fn put_같은_이름은_덮어쓰고_saved가_비면_지금_시각이다() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::load(dir.path()).unwrap();
        s.put(Fav {
            name: "q".into(),
            conn: "old".into(),
            sql: "select 1".into(),
            ..Default::default()
        })
        .unwrap();
        let before = GoTime(GoTime::now().0 - TimeDelta::seconds(1));
        s.put(Fav {
            name: " q".into(),
            conn: "new".into(),
            sql: "select 2".into(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(s.list().len(), 1);
        let f = s.get("q ").unwrap();
        assert!(f.conn == "new" && f.sql == "select 2", "get: {f:?}");
        assert!(
            f.saved >= before && f.saved <= GoTime::now() && f.saved.0.timestamp_subsec_nanos() == 0,
            "saved: {}",
            f.saved
        );
    }

    #[test]
    fn put_잘못된_이름과_빈_sql은_거부하고_파일을_만들지_않는다() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::load(dir.path()).unwrap();
        let long = "가".repeat(101);
        let cases = [
            ("", "select 1"),
            (" \t\n", "select 1"),
            (long.as_str(), "select 1"),
            ("줄\n바꿈", "select 1"),
            ("빈 SQL", "  \n"),
        ];
        for (name, sql) in cases {
            assert!(s.put(fav(name, sql)).is_err(), "{name:?} 를 받아들였다");
        }
        assert!(!path(dir.path()).exists(), "파일을 만들었다");
        s.put(fav(&"가".repeat(100), "select 1")).expect("100자 이름");
        assert_eq!(normalize_name("").unwrap_err().to_string(), "favorite name is empty");
        assert_eq!(
            normalize_name(&long).unwrap_err().to_string(),
            "favorite name is longer than 100 characters"
        );
        assert_eq!(
            normalize_name("a\nb").unwrap_err().to_string(),
            "favorite name contains control characters"
        );
        assert_eq!(s.put(fav("x", " ")).unwrap_err().to_string(), "favorite sql is empty");
    }

    #[test]
    fn delete_지우고_없는_이름은_오류_없이_넘어간다() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::load(dir.path()).unwrap();
        for n in ["a", "b"] {
            s.put(fav(n, "select 1")).unwrap();
        }
        s.delete(" a ").unwrap();
        s.delete("없음").unwrap();
        assert_eq!(names(&Store::load(dir.path()).unwrap().list()), "b");
        s.delete("b").unwrap();
        assert_eq!(fs::read_to_string(path(dir.path())).unwrap(), "[]\n", "다 지운 파일");
    }

    #[test]
    fn put_다른_프로세스가_그사이_저장한_이름을_지킨다() {
        let dir = tempfile::tempdir().unwrap();
        let mine = Store::load(dir.path()).unwrap();
        let other = Store::load(dir.path()).unwrap();
        other.put(fav("남의 것", "select 1")).unwrap();
        mine.put(fav("내 것", "select 2")).unwrap();
        assert_eq!(names(&Store::load(dir.path()).unwrap().list()), "남의 것|내 것");
        assert_eq!(names(&mine.list()), "남의 것|내 것", "쓴 뒤 메모리 목록");
    }

    #[test]
    fn load_같은_이름이_둘이면_뒤의_것을_남기고_빈_파일은_빈_목록이다() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(path(dir.path()).parent().unwrap()).unwrap();
        fs::write(
            path(dir.path()),
            r#"[{"name":"b","sql":"1"},{"name":"a","sql":"old"},{"name":"a","sql":"new"}]"#,
        )
        .unwrap();
        let s = Store::load(dir.path()).unwrap();
        assert_eq!(names(&s.list()), "a|b");
        assert_eq!(s.get("a").unwrap().sql, "new");
        fs::write(path(dir.path()), " \n").unwrap();
        assert!(Store::load(dir.path()).unwrap().list().is_empty(), "빈 파일");
    }

    #[test]
    fn put_깨진_파일이면_오류이고_파일을_덮어쓰지_않는다() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(path(dir.path()).parent().unwrap()).unwrap();
        let broken = r#"[{"name":"a","sql":"#;
        fs::write(path(dir.path()), broken).unwrap();
        let e = Store::load(dir.path()).err().expect("load 가 깨진 파일을 받아들였다");
        assert!(e.to_string().starts_with(&format!("read {}: ", path(dir.path()).display())), "{e}");
        let s = Store {
            path: path(dir.path()),
            favs: Mutex::new(Vec::new()),
        };
        assert!(s.put(fav("b", "select 1")).is_err(), "put 이 깨진 파일 위에 썼다");
        assert!(s.delete("a").is_err(), "delete 가 깨진 파일 위에 썼다");
        assert_eq!(fs::read_to_string(path(dir.path())).unwrap(), broken, "원본이 바뀌었다");
    }

    #[test]
    fn put_임시_파일을_남기지_않는다() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::load(dir.path()).unwrap();
        for n in ["a", "b", "c"] {
            s.put(fav(n, "select 1")).unwrap();
        }
        s.delete("a").unwrap();
        let entries: Vec<String> = fs::read_dir(path(dir.path()).parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["favorites.json"]);
    }

    #[test]
    fn put_쓰기에_실패하면_원본이_남고_임시_파일도_없다() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::load(dir.path()).unwrap();
        s.put(fav("a", "select 1")).unwrap();
        let sub = path(dir.path()).parent().unwrap().to_path_buf();
        let before = fs::read(path(dir.path())).unwrap();
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o555)).unwrap();
        // root 는 권한 없는 폴더에도 쓴다
        if fs::write(sub.join("probe"), "x").is_ok() {
            fs::set_permissions(&sub, fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }
        let res = s.put(fav("b", "select 2"));
        let after = fs::read(path(dir.path())).unwrap();
        let entries = fs::read_dir(&sub).unwrap().count();
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(res.is_err(), "읽기 전용 폴더에 썼다");
        assert_eq!(after, before, "원본이 바뀌었다");
        assert_eq!(names(&s.list()), "a", "실패한 쓰기가 메모리에 남았다");
        assert_eq!(entries, 1);
    }

    #[test]
    fn put_delete_는_있던_파일의_권한을_지킨다() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::load(dir.path()).unwrap();
        s.put(fav("a", "select 1")).unwrap();
        fs::set_permissions(path(dir.path()), fs::Permissions::from_mode(0o600)).unwrap();
        s.put(fav("b", "select 2")).unwrap();
        s.delete("a").unwrap();
        assert_eq!(mode(&path(dir.path())), 0o600);
    }

    /// Go 판이 쓴 favorites.json 이다(internal/favorites.Store.Put). HTML 이스케이프 없이 U+2028 만 이스케이프한다.
    const GO_FAVORITES: &str = r#"[
  {
    "name": "a<b",
    "conn": "u2",
    "sql": "select 1 where 1 < 2 && true \u2028",
    "saved": "2026-11-16T10:15:30+09:00"
  },
  {
    "name": "오늘",
    "conn": "",
    "sql": "select now()",
    "saved": "2026-11-16T01:00:00Z"
  }
]
"#;

    #[test]
    fn go_판이_쓴_즐겨찾기를_읽고_같은_바이트로_쓴다() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(path(dir.path()).parent().unwrap()).unwrap();
        fs::write(path(dir.path()), GO_FAVORITES).unwrap();
        let s = Store::load(dir.path()).unwrap();
        let favs = s.list();
        assert_eq!(names(&favs), "a<b|오늘");
        assert_eq!(favs[0].sql, "select 1 where 1 < 2 && true \u{2028}");
        assert_eq!(favs[0].saved.format(), "2026-11-16T10:15:30+09:00");
        assert_eq!(favs[1].saved, GoTime::from(Utc.with_ymd_and_hms(2026, 11, 16, 1, 0, 0).unwrap()));
        // Rust 판이 같은 값을 새 파일에 넣으면 Go 판과 바이트가 같다
        let other = tempfile::tempdir().unwrap();
        let mine = Store::load(other.path()).unwrap();
        favs.into_iter().for_each(|f| mine.put(f).unwrap());
        assert_eq!(fs::read_to_string(path(other.path())).unwrap(), GO_FAVORITES);
    }
}
