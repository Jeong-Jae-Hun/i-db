//! 파라미터 창(:name)에 마지막으로 넣은 값을 프로젝트마다 기억한다(명세 R55).
//! 값에 이메일 같은 개인정보가 들 수 있어 세션(R41)처럼 레포가 아닌 $XDG_STATE_HOME/db/params.json(0600)에 두고,
//! 프로젝트 절대 경로를 키로 따로 둔다. Go `internal/paramstore` 와 같은 파일을 읽고 쓴다.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::atomicfile;
use crate::history::{blank, go_json, mkdir_all, state_file};
use crate::session::{abs_key, corrupt, move_bad};

/// 값 하나의 상한. 넘는 값은 잘라 두면 다른 값이 되어 기억하지 않는다
const MAX_VALUE: usize = 4 << 10;
/// 프로젝트 하나에 기억할 이름 수
const MAX_NAMES: usize = 200;

type All = BTreeMap<String, BTreeMap<String, String>>;

/// 기억 파일 경로다. session 과 같이 XDG_STATE_HOME 이 절대 경로일 때만 따른다.
pub fn path() -> io::Result<PathBuf> {
    state_file("params.json")
}

/// 비밀 값이 들 만한 이름이다(Go textutil.LooksSecret, `(?i)pass|secret|token|key|pwd`). 넓게 잡아 api_key·monkey 처럼
/// 비밀이 아닌 이름도 걸리지만 가리는 쪽이 안전하다. Go 의 (?i) 는 K(U+212A)·ſ(U+017F) 도 k·s 로 본다.
fn looks_secret(name: &str) -> bool {
    let folded: String = name
        .chars()
        .map(|c| match c {
            '\u{212A}' => 'k',
            '\u{017F}' => 's',
            c => c.to_ascii_lowercase(),
        })
        .collect();
    ["pass", "secret", "token", "key", "pwd"].iter().any(|w| folded.contains(w))
}

/// project_dir 의 이름 → 마지막 값이다. 파일·항목이 없으면 빈 맵이다. 비밀 같은 이름은 예전 파일에 있어도 돌려주지 않는다.
/// 깨진 파일은 params.json.bad 로 옮기고 오류를 돌려준다. 다음 save 는 새 파일에 쓴다.
pub fn load(path: &Path, project_dir: &Path) -> io::Result<BTreeMap<String, String>> {
    let key = abs_key(project_dir)?;
    let all = read_all(path).map_err(|e| if crate::session::is_corrupt(&e) { move_bad(e, path) } else { e })?;
    let mut out = all.and_then(|mut a| a.remove(&key)).unwrap_or_default();
    out.retain(|name, _| !looks_secret(name));
    Ok(out)
}

/// 파일을 다시 읽어 project_dir 항목에 values 를 덮어 합친다. 다른 이름·프로젝트는 지킨다.
/// 빈 값은 그 이름을 지우고, 이름이 비었거나 비밀 같은 이름이거나 MAX_VALUE 를 넘는 값은 건너뛴다. 합친 이름이 MAX_NAMES 를 넘으면
/// 이번에 저장한 이름을 먼저 남기고 나머지 기존 이름은 이름순으로 채운다.
/// 단, 이름순 채우기는 최근에 쓴 이름이라도 밀려날 수 있다. 자주 넘치면 값마다 쓴 시각을 두고 오래된 것부터 지운다.
/// 같은 폴더 임시 파일(0600)에 다 쓴 뒤 바꿔 끼우므로 실패하면 원래 파일이 남는다. 폴더는 0700 으로 만든다.
/// 깨진 파일은 params.json.bad 로 옮기고 이 프로젝트 항목만 든 새 파일을 쓴다.
pub fn save(path: &Path, project_dir: &Path, values: &BTreeMap<String, String>) -> io::Result<()> {
    let key = abs_key(project_dir)?;
    let all = match read_all(path) {
        Err(e) if crate::session::is_corrupt(&e) => {
            fs::rename(path, crate::session::bad_path(path))?;
            None
        }
        r => r?,
    };
    let mut all = all.unwrap_or_default();
    let mut cur = all.get(&key).cloned().unwrap_or_default();
    // values 는 BTreeMap 이라 fresh 는 이미 이름순이다
    let mut fresh = Vec::new();
    for (name, v) in values {
        if name.is_empty() || v.len() > MAX_VALUE || looks_secret(name) {
            continue;
        }
        if v.is_empty() {
            cur.remove(name);
        } else {
            cur.insert(name.clone(), v.clone());
            fresh.push(name.clone());
        }
    }
    if cur.len() > MAX_NAMES {
        let mut next = BTreeMap::new();
        for name in fresh.iter().take(MAX_NAMES).chain(cur.keys()) {
            if next.len() == MAX_NAMES {
                break;
            }
            next.insert(name.clone(), cur[name].clone());
        }
        cur = next;
    }
    if cur.is_empty() {
        all.remove(&key);
    } else {
        all.insert(key, cur);
    }
    let data = go_json(&all, true, false)?;
    mkdir_all(path.parent().unwrap_or(Path::new(".")), 0o700)?;
    atomicfile::write(path, ".params-*.json", &data, 0o600)
}

/// 프로젝트 절대 경로 → 이름 → 값이다. 없는 파일·빈 파일·`null` 은 None 이다.
fn read_all(path: &Path) -> io::Result<Option<All>> {
    let data = match fs::read(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        r => r?,
    };
    if blank(&data) {
        return Ok(None);
    }
    // Go 는 프로젝트 값이 null 이면 빈 항목으로 읽는다
    let all: Option<BTreeMap<String, Option<BTreeMap<String, String>>>> =
        serde_json::from_slice(&data).map_err(|e| corrupt(format!("params file is corrupt: {}: {e}", path.display())))?;
    Ok(all.map(|a| a.into_iter().map(|(k, v)| (k, v.unwrap_or_default())).collect()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::state_file_from;
    use crate::session::{bad_path, is_corrupt};
    use std::os::unix::fs::PermissionsExt;

    fn m(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn mode(p: &Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn save_load_프로젝트마다_합쳐_기억하고_0600_이다() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let path = dir.join("state").join("db").join("params.json");
        let (a, b) = (dir.join("a"), dir.join("b"));
        assert!(load(&path, &a).unwrap().is_empty(), "없는 파일");
        save(&path, &a, &m(&[("user_id", "1315"), ("email", "user@example.com")])).unwrap();
        save(&path, &b, &m(&[("user_id", "7")])).unwrap();
        // 덮어쓰기·빈 값 지우기·다른 이름 지키기
        save(&path, &a, &m(&[("user_id", "42"), ("email", ""), ("slug", "nino")])).unwrap();
        assert_eq!(load(&path, &a).unwrap(), m(&[("user_id", "42"), ("slug", "nino")]));
        assert_eq!(load(&path, &b.join(".")).unwrap(), m(&[("user_id", "7")]), "같은 절대 경로");
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
    }

    #[test]
    fn save_긴_값과_빈_이름은_건너뛰고_이름_수를_자른다() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("params.json");
        let proj = tmp.path().join("proj");
        let big = "x".repeat(MAX_VALUE + 1);
        let ok = "y".repeat(MAX_VALUE);
        save(&path, &proj, &m(&[("ok", &ok), ("big", &big), ("", "v")])).unwrap();
        let got = load(&path, &proj).unwrap();
        assert!(got.len() == 1 && got["ok"].len() == MAX_VALUE, "상한: {}개", got.len());
        // 긴 값으로 덮으려 하면 예전 값을 지킨다
        save(&path, &proj, &m(&[("ok", &big)])).unwrap();
        assert_eq!(load(&path, &proj).unwrap()["ok"].len(), MAX_VALUE, "긴 값이 예전 값을 지웠다");

        let old: BTreeMap<String, String> = (0..MAX_NAMES).map(|i| (format!("old{i:03}"), "v".to_string())).collect();
        save(&path, &proj, &old).unwrap();
        save(&path, &proj, &m(&[("zz_new", "n")])).unwrap();
        let got = load(&path, &proj).unwrap();
        assert!(got.len() == MAX_NAMES && got["zz_new"] == "n", "이름 수 {}", got.len());
        // 이름순으로 채워 맨 뒤 기존 이름이 밀려난다
        assert!(!got.contains_key("old199"), "old199 가 남았다");
    }

    #[test]
    fn load_save_깨진_파일은_bad_로_옮기고_새로_시작한다() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let path = dir.join("params.json");
        fs::write(&path, "{broken").unwrap();
        let e = load(&path, dir).unwrap_err();
        assert!(is_corrupt(&e) && e.to_string().starts_with("params file is corrupt: "), "load: {e}");
        assert_eq!(fs::read_to_string(bad_path(&path)).unwrap(), "{broken");
        assert!(load(&path, dir).unwrap().is_empty(), "옮긴 뒤");

        fs::write(&path, r#"{"x": 1}"#).unwrap();
        save(&path, dir, &m(&[("id", "1")])).unwrap();
        assert_eq!(load(&path, dir).unwrap()["id"], "1", "save 뒤");
        assert_eq!(fs::read_to_string(bad_path(&path)).unwrap(), r#"{"x": 1}"#, "save 가 옮긴 bad");
    }

    #[test]
    fn path_xdg_state_home_은_절대_경로일_때만_따른다() {
        let got = state_file_from(Some("/xdg".into()), Some("/home/u".into()), "params.json").unwrap();
        assert_eq!(got, PathBuf::from("/xdg/db/params.json"), "절대");
        let got = state_file_from(Some("rel".into()), Some("/home/u".into()), "params.json").unwrap();
        assert_eq!(got, PathBuf::from("/home/u/.local/state/db/params.json"), "상대");
        assert!(path().unwrap().ends_with("db/params.json"));
    }

    #[test]
    fn save_load_비밀_같은_이름의_값은_기억하지_않는다() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("params.json");
        let dir = tmp.path().join("p");
        save(
            &path,
            &dir,
            &m(&[
                ("user_id", "1"),
                ("password", "p"),
                ("api_TOKEN", "t"),
                ("pwd", "x"),
                ("secret_key", "k"),
            ]),
        )
        .unwrap();
        assert_eq!(load(&path, &dir).unwrap(), m(&[("user_id", "1")]));
        assert!(
            !fs::read_to_string(&path).unwrap().contains(r#""p""#),
            "파일에 비밀 값이 남으면 안 된다"
        );
        assert!(looks_secret("MON\u{212A}EY") && looks_secret("Pa\u{017F}\u{017F}") && !looks_secret("user_id"));
    }

    /// Go 판이 쓴 params.json 이다(internal/paramstore.Save).
    const GO_PARAMS: &str = r#"{"/proj/a":{"email":"a\u0026b@example.com","user_id":"1315","z":"\u003cv\u003e"},"/proj/b":{"id":"7"}}"#;

    #[test]
    fn go_판이_쓴_기억을_읽고_같은_바이트로_다시_쓴다() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("params.json");
        fs::write(&path, GO_PARAMS).unwrap();
        let a = load(&path, Path::new("/proj/a")).unwrap();
        assert_eq!(a, m(&[("email", "a&b@example.com"), ("user_id", "1315"), ("z", "<v>")]));
        assert_eq!(load(&path, Path::new("/proj/b")).unwrap(), m(&[("id", "7")]));
        save(&path, Path::new("/proj/a"), &a).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), GO_PARAMS);
    }
}
