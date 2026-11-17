//! 자주·최근 쓴 동작·접속·테이블과 테이블 보기 설정, 입력 기록, 트리 펼침을 기억한다(명세 R75).
//! WHERE·거르기 글에 개인정보가 들 수 있어 세션(R41)처럼 레포가 아닌 $XDG_STATE_HOME/db/recent.json(0600)에 둔다.
//! 동작·거르기 기록은 모든 프로젝트가 같이 쓰고, 접속·테이블·트리는 프로젝트 절대 경로를 키로 따로 둔다.
//! 기억은 덤이라 읽지 못하면 빈 기록으로 시작한다. Go `internal/recent` 와 같은 파일을 읽고 쓴다.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::atomicfile;
use crate::history::{GoTime, blank, go_json, mkdir_all, null_default, skip_null_values};
use crate::session::{self, abs_key};

/// 기억할 동작 수. 넘으면 frecency 가 가장 낮은 것부터 버린다
const MAX_ACTIONS: usize = 50;
/// 프로젝트마다 기억할 접속 수
const MAX_CONNS: usize = 50;
/// 프로젝트마다 기억할 테이블 수. 넘으면 가장 오래 안 연 것부터 버린다(LRU)
const MAX_TABLES: usize = 500;
/// 결과 거르기 기록(모든 프로젝트 공통)
const MAX_FILTERS: usize = 100;
/// 테이블마다 WHERE 입력 기록
const MAX_WHERES: usize = 20;
/// 테이블마다 기억할 열 폭 수
const MAX_WIDTHS: usize = 100;
/// 프로젝트마다 기억할 펼친 노드 수
const MAX_TREE: usize = 500;
const MAX_TEXT: usize = 4 << 10;
/// 사용 횟수는 HALF_LIFE 마다 반으로 줄여 센다. 예전에 많이 쓴 것보다 요즘 쓴 것이 앞에 오게 한다
const HALF_LIFE: Duration = Duration::from_secs(7 * 24 * 3600);

/// write 가 recent.json.lock 을 기다리는 기본 시간이다. 다른 db 가 잠금을 쥔 채 멈춰도 저장이 끝없이 서지 않게 한다.
pub const LOCK_WAIT: Duration = Duration::from_secs(2);

/// 횟수와 마지막으로 쓴 때다.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Use {
    pub n: i64,
    pub at: GoTime,
}

impl Use {
    /// frecency 점수다. 횟수에 마지막 사용 뒤 지난 시간만큼 줄어드는 가중치를 곱한다.
    fn score(&self, now: GoTime) -> f64 {
        let age = now.sub_nanos(self.at).max(0);
        self.n as f64 * (-(age as f64) / HALF_LIFE.as_nanos() as f64).exp2()
    }
}

/// 테이블 하나의 기억이다. where_·wheres 는 prod 접속이면 비운다.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Table {
    /// 접속 uuid
    pub conn: String,
    pub schema: String,
    pub name: String,
    pub at: GoTime,
    #[serde(skip_serializing_if = "BTreeMap::is_empty", deserialize_with = "null_default")]
    pub widths: BTreeMap<String, i64>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub order: String,
    #[serde(rename = "where", skip_serializing_if = "String::is_empty")]
    pub where_: String,
    /// WHERE 입력 기록, 최근 것부터
    #[serde(skip_serializing_if = "Vec::is_empty", deserialize_with = "null_default")]
    pub wheres: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct Project {
    #[serde(skip_serializing_if = "BTreeMap::is_empty", deserialize_with = "null_default")]
    conns: BTreeMap<String, Use>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty", deserialize_with = "skip_null_values")]
    tables: BTreeMap<String, Table>,
    /// 펼친 노드 id
    #[serde(skip_serializing_if = "Vec::is_empty", deserialize_with = "null_default")]
    open: Vec<String>,
    /// 접은 그룹 id(그룹은 처음에 펼쳐져 있다)
    #[serde(skip_serializing_if = "Vec::is_empty", deserialize_with = "null_default")]
    closed: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct File {
    #[serde(skip_serializing_if = "BTreeMap::is_empty", deserialize_with = "null_default")]
    actions: BTreeMap<String, Use>,
    #[serde(skip_serializing_if = "Vec::is_empty", deserialize_with = "null_default")]
    filters: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty", deserialize_with = "skip_null_values")]
    projects: BTreeMap<String, Project>,
}

/// 한 프로젝트에서 본 기억이다. 화면 스레드에서만 쓴다.
pub struct Store {
    path: PathBuf,
    key: String,
    actions: BTreeMap<String, Use>,
    filters: Vec<String>,
    p: Project,
    /// write 가 잠금을 기다리는 시간이다. 기본은 LOCK_WAIT
    pub lock_wait: Duration,
}

/// 기억 파일 경로다. 세션 파일과 같은 폴더의 recent.json 이다.
pub fn path() -> io::Result<PathBuf> {
    Ok(session::path()?.with_file_name("recent.json"))
}

/// project_dir 의 기억을 읽는다. 파일이 없으면 빈 기억이다. 읽지 못하거나 깨졌으면 빈 기억과 그 오류를 같이 돌려준다
/// (다음 save 가 새로 쓴다). 프로젝트 경로를 절대 경로로 바꾸지 못할 때만 Err 이다.
pub fn load(path: &Path, project_dir: &Path) -> io::Result<(Store, Option<io::Error>)> {
    let key = abs_key(project_dir)?;
    let (mut f, err) = match read_file(path) {
        Ok(f) => (f, None),
        Err(e) => (File::default(), Some(e)),
    };
    let p = f.projects.remove(&key).unwrap_or_default();
    let s = Store {
        path: path.to_path_buf(),
        key,
        actions: f.actions,
        filters: f.filters,
        p,
        lock_wait: LOCK_WAIT,
    };
    Ok((s, err))
}

fn read_file(path: &Path) -> io::Result<File> {
    let data = match fs::read(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(File::default()),
        r => r?,
    };
    if blank(&data) {
        return Ok(File::default());
    }
    let f: Option<File> =
        serde_json::from_slice(&data).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, crate::gojson::json_error(&e)))?;
    Ok(f.unwrap_or_default())
}

/// write 할 기억의 사본이다. 화면 스레드에서 만들고 다른 스레드에서 써도 Store 와 나누는 것이 없다.
pub struct Snapshot {
    path: PathBuf,
    key: String,
    actions: BTreeMap<String, Use>,
    filters: Vec<String>,
    p: Project,
    lock_wait: Duration,
}

impl Store {
    /// 지금 기억을 떠 둔다.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            path: self.path.clone(),
            key: self.key.clone(),
            actions: self.actions.clone(),
            filters: self.filters.clone(),
            p: self.p.clone(),
            lock_wait: self.lock_wait,
        }
    }

    /// snapshot 을 바로 쓴다(시험·CLI 처럼 화면이 없을 때).
    pub fn save(&self) -> io::Result<()> {
        self.snapshot().write()
    }
}

impl Snapshot {
    /// 파일을 다시 읽어 이 프로젝트 항목을 사본 것으로 바꾸고 동작·거르기 기록은 사본 것으로 덮어 쓴다.
    /// 다른 프로젝트 항목은 지킨다. 같은 폴더 임시 파일(0600)에 다 쓴 뒤 바꿔 끼운다. 폴더는 0700 으로 만든다.
    /// 다시 읽기부터 바꿔 끼우기까지는 옆 .lock 파일의 flock(lock_wait 까지 기다림)으로 프로세스끼리 줄 세워 다른 프로젝트 항목을 덮지 않는다.
    /// 단, 동시에 켠 다른 db 가 그사이 남긴 동작·거르기 기록은 나중에 쓴 쪽이 이긴다.
    pub fn write(self) -> io::Result<()> {
        mkdir_all(self.path.parent().unwrap_or(Path::new(".")), 0o700)?;
        let _lock = atomicfile::try_lock(&self.path, self.lock_wait)?;
        // 깨진 파일은 새로 쓴다
        let mut disk = read_file(&self.path).unwrap_or_default();
        disk.actions = self.actions;
        disk.filters = self.filters;
        disk.projects.insert(self.key, self.p);
        let data = go_json(&disk, true, false)?;
        atomicfile::write(&self.path, ".recent-*.json", &data, 0o600)
    }
}

// ─── 동작·접속 (frecency) ───

impl Store {
    /// 동작 key 를 한 번 쓴 것으로 센다.
    pub fn use_action(&mut self, key: &str, now: GoTime) {
        use_key(&mut self.actions, key, now, MAX_ACTIONS);
    }

    /// frecency 높은 순으로 n 개 동작 key 다.
    pub fn top_actions(&self, now: GoTime, n: usize) -> Vec<String> {
        top(&self.actions, now, n)
    }

    /// 접속 uuid 를 한 번 쓴 것으로 센다.
    pub fn use_conn(&mut self, uuid: &str, now: GoTime) {
        use_key(&mut self.p.conns, uuid, now, MAX_CONNS);
    }

    /// frecency 높은 순으로 n 개 접속 uuid 다.
    pub fn top_conns(&self, now: GoTime, n: usize) -> Vec<String> {
        top(&self.p.conns, now, n)
    }
}

fn use_key(m: &mut BTreeMap<String, Use>, key: &str, now: GoTime, limit: usize) {
    if key.is_empty() {
        return;
    }
    let n = m.get(key).map_or(0, |u| u.n);
    m.insert(key.to_string(), Use { n: n + 1, at: now });
    if m.len() > limit {
        let keep = top(m, now, limit);
        m.retain(|k, _| keep.contains(k));
    }
}

/// 점수 높은 순, 같으면 최근 순, 그것도 같으면 key 순으로 n 개다.
fn top(m: &BTreeMap<String, Use>, now: GoTime, n: usize) -> Vec<String> {
    let mut keys: Vec<(&String, &Use)> = m.iter().collect();
    keys.sort_by(|(a, ua), (b, ub)| {
        ub.score(now)
            .partial_cmp(&ua.score(now))
            .unwrap_or(Ordering::Equal)
            .then(ub.at.cmp(&ua.at))
            .then(a.cmp(b))
    });
    keys.into_iter().take(n).map(|(k, _)| k.clone()).collect()
}

// ─── 테이블 ───

fn table_key(conn: &str, schema: &str, name: &str) -> String {
    format!("{conn}\x00{schema}\x00{name}")
}

impl Store {
    /// 기억한 테이블을 돌려주고 없으면 만든다. 넘치면 가장 오래 안 연 것부터 버린다.
    /// 새 테이블의 연 때(now)가 가장 오래돼 바로 버려지면 None 이다(Go 판은 맵에서 빠진 테이블을 고쳐 아무 일도 남지 않는다).
    fn ensure(&mut self, conn: &str, schema: &str, name: &str, now: GoTime) -> Option<&mut Table> {
        let k = table_key(conn, schema, name);
        if !self.p.tables.contains_key(&k) {
            let t = Table {
                conn: conn.to_string(),
                schema: schema.to_string(),
                name: name.to_string(),
                at: now,
                ..Default::default()
            };
            self.p.tables.insert(k.clone(), t);
            while self.p.tables.len() > MAX_TABLES {
                let oldest = self
                    .p
                    .tables
                    .iter()
                    .min_by(|(ka, a), (kb, b)| a.at.cmp(&b.at).then(ka.cmp(kb)))
                    .map(|(k, _)| k.clone());
                if let Some(o) = oldest {
                    self.p.tables.remove(&o);
                }
            }
        }
        self.p.tables.get_mut(&k)
    }

    /// 테이블을 연 때를 남긴다. 접속이나 테이블이 비면 남기지 않는다.
    pub fn touch_table(&mut self, conn: &str, schema: &str, name: &str, now: GoTime) {
        if conn.is_empty() || name.is_empty() {
            return;
        }
        if let Some(t) = self.ensure(conn, schema, name, now) {
            t.at = now;
        }
    }

    /// 기억한 테이블의 사본이다.
    pub fn table(&self, conn: &str, schema: &str, name: &str) -> Option<Table> {
        self.p.tables.get(&table_key(conn, schema, name)).cloned()
    }

    /// 최근 연 순으로 n 개 테이블이다. 이름·연 때만 채운다.
    pub fn recent_tables(&self, n: usize) -> Vec<Table> {
        let mut out: Vec<Table> = self
            .p
            .tables
            .values()
            .map(|t| Table {
                conn: t.conn.clone(),
                schema: t.schema.clone(),
                name: t.name.clone(),
                at: t.at,
                ..Default::default()
            })
            .collect();
        out.sort_by(|a, b| {
            b.at.cmp(&a.at)
                .then_with(|| table_key(&a.conn, &a.schema, &a.name).cmp(&table_key(&b.conn, &b.schema, &b.name)))
        });
        out.truncate(n);
        out
    }

    /// 마지막 정렬·WHERE 를 남기고 바뀌었는지 돌려준다. prod 면 정렬·WHERE·WHERE 기록을 남기지 않고 지운다
    /// (정렬 글도 식에 값이 들 수 있어 prod 는 열 폭만 남긴다).
    #[allow(clippy::too_many_arguments)]
    pub fn set_view(&mut self, conn: &str, schema: &str, name: &str, order: &str, where_: &str, prod: bool, now: GoTime) -> bool {
        if conn.is_empty() || name.is_empty() || order.len() > MAX_TEXT || where_.len() > MAX_TEXT {
            return false;
        }
        let (order, where_) = if prod { ("", "") } else { (order, where_) };
        let exists = self.p.tables.contains_key(&table_key(conn, schema, name));
        if !exists && order.is_empty() && where_.is_empty() {
            return false;
        }
        let mut evicted = Table::default();
        let t = self.ensure(conn, schema, name, now).unwrap_or(&mut evicted);
        let changed = t.order != order || t.where_ != where_ || (prod && !t.wheres.is_empty());
        t.order = order.to_string();
        t.where_ = where_.to_string();
        if prod {
            t.wheres.clear();
        }
        changed
    }

    /// 열 폭을 남긴다.
    pub fn set_width(&mut self, conn: &str, schema: &str, name: &str, col: &str, w: i64, now: GoTime) {
        if conn.is_empty() || name.is_empty() || col.is_empty() {
            return;
        }
        let Some(t) = self.ensure(conn, schema, name, now) else { return };
        if !t.widths.contains_key(col) && t.widths.len() >= MAX_WIDTHS {
            return;
        }
        t.widths.insert(col.to_string(), w);
    }

    /// WHERE 입력 기록 맨 앞에 where_ 를 둔다. prod 면 남기지 않는다.
    pub fn add_where(&mut self, conn: &str, schema: &str, name: &str, where_: &str, prod: bool, now: GoTime) {
        let where_ = where_.trim();
        if prod || conn.is_empty() || name.is_empty() || where_.is_empty() || where_.len() > MAX_TEXT {
            return;
        }
        let Some(t) = self.ensure(conn, schema, name, now) else { return };
        push_front(&mut t.wheres, where_, MAX_WHERES);
    }

    /// 테이블의 WHERE 입력 기록, 최근 것부터다. prod 면 비어 있다.
    pub fn wheres(&self, conn: &str, schema: &str, name: &str, prod: bool) -> Vec<String> {
        match self.p.tables.get(&table_key(conn, schema, name)) {
            Some(t) if !prod => t.wheres.clone(),
            _ => Vec::new(),
        }
    }

    // ─── 거르기 기록 ───

    /// 결과 거르기 기록 맨 앞에 f 를 둔다.
    pub fn add_filter(&mut self, f: &str) {
        if f.is_empty() || f.len() > MAX_TEXT {
            return;
        }
        push_front(&mut self.filters, f, MAX_FILTERS);
    }

    /// 결과 거르기 기록, 최근 것부터다.
    pub fn filters(&self) -> Vec<String> {
        self.filters.clone()
    }

    // ─── 트리 펼침 ───

    /// 펼친 노드 id 와 접은 그룹 id 다.
    pub fn tree(&self) -> (Vec<String>, Vec<String>) {
        (self.p.open.clone(), self.p.closed.clone())
    }

    /// 펼친 노드 id 와 접은 그룹 id 를 바꾸고 바뀌었는지 돌려준다.
    pub fn set_tree(&mut self, open: &[String], closed: &[String]) -> bool {
        let open = &open[..open.len().min(MAX_TREE)];
        let closed = &closed[..closed.len().min(MAX_TREE)];
        if open == self.p.open.as_slice() && closed == self.p.closed.as_slice() {
            return false;
        }
        self.p.open = open.to_vec();
        self.p.closed = closed.to_vec();
        true
    }
}

fn push_front(list: &mut Vec<String>, v: &str, limit: usize) {
    list.retain(|x| x != v);
    list.insert(0, v.to_string());
    list.truncate(limit);
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeDelta, TimeZone, Utc};
    use std::os::unix::fs::PermissionsExt;
    use std::thread;

    fn t0() -> GoTime {
        Utc.with_ymd_and_hms(2026, 11, 16, 9, 0, 0).unwrap().into()
    }

    fn add(t: GoTime, d: TimeDelta) -> GoTime {
        GoTime(t.0 + d)
    }

    fn new_store() -> (tempfile::TempDir, Store, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db").join("recent.json");
        let (s, err) = load(&path, &dir.path().join("proj")).unwrap();
        assert!(err.is_none());
        (dir, s, path)
    }

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn frecency_자주_쓴_것이_먼저고_오래되면_요즘_쓴_것에_밀린다() {
        let (_d, mut s, _) = new_store();
        for _ in 0..3 {
            s.use_action("a", t0());
        }
        s.use_action("b", t0());
        s.use_action("c", add(t0(), TimeDelta::minutes(1)));
        assert_eq!(
            s.top_actions(add(t0(), TimeDelta::minutes(1)), 3),
            strs(&["a", "c", "b"]),
            "횟수 먼저, 같으면 최근 것"
        );
        // 4주 뒤에는 3번 쓴 a(가중치 1/16)보다 어제 한 번 쓴 d 가 앞이다
        let later = add(t0(), TimeDelta::days(28));
        s.use_action("d", add(later, -TimeDelta::days(1)));
        assert_eq!(s.top_actions(later, 1), strs(&["d"]), "오래된 사용은 줄어든다");
    }

    #[test]
    fn frecency_동작은_50개까지_점수_낮은_것부터_버린다() {
        let (_d, mut s, _) = new_store();
        s.use_action("keep", t0());
        s.use_action("keep", t0());
        for i in 0..60 {
            s.use_action(&format!("x{i:02}"), add(t0(), TimeDelta::seconds(i)));
        }
        assert_eq!(s.actions.len(), MAX_ACTIONS, "50개까지");
        assert!(s.actions.contains_key("keep"), "두 번 쓴 동작은 남는다");
        assert!(!s.actions.contains_key("x00"), "가장 점수 낮은 것부터 버린다");
    }

    #[test]
    fn tables_500개를_넘으면_가장_오래_안_연_테이블부터_버린다() {
        let (_d, mut s, _) = new_store();
        for i in 0..MAX_TABLES + 10 {
            s.touch_table("u", "public", &format!("t{i:03}"), add(t0(), TimeDelta::seconds(i as i64)));
        }
        assert_eq!(s.p.tables.len(), MAX_TABLES, "500개까지");
        assert!(s.table("u", "public", "t009").is_none(), "가장 오래된 것부터 버린다");
        let got = s.recent_tables(2);
        assert!(got[0].name == "t509" && got[1].name == "t508", "최근 연 순: {got:?}");
        s.touch_table("u", "public", "t010", add(t0(), TimeDelta::hours(1)));
        assert_eq!(s.recent_tables(1)[0].name, "t010", "다시 열면 맨 앞");
    }

    #[test]
    fn tables_prod_는_정렬_where_와_그_기록을_남기지_않고_열_폭만_남긴다() {
        let (_d, mut s, path) = new_store();
        // 예전 판이 남긴 prod 정렬도 다음 set_view 가 지운다
        s.set_view("p", "public", "users", "id DESC", "", false, t0());
        s.add_where("p", "public", "users", "email = 'a@b.com'", true, t0());
        s.set_view("p", "public", "users", "email = 'c@d.com' DESC", "email = 'a@b.com'", true, t0());
        s.set_width("p", "public", "users", "email", 30, t0());
        s.save().unwrap();
        let data = fs::read_to_string(&path).unwrap();
        assert!(
            !data.contains("a@b.com") && !data.contains("c@d.com") && !data.contains("DESC"),
            "prod 정렬·WHERE 는 파일에 없다: {data}"
        );
        let tb = s.table("p", "public", "users").unwrap();
        assert!(
            tb.order.is_empty() && tb.where_.is_empty() && tb.widths["email"] == 30,
            "열 폭만: {tb:?}"
        );
        assert!(s.wheres("p", "public", "users", true).is_empty());

        s.set_view("s", "public", "users", "", "status = 'A'", false, t0());
        s.add_where("s", "public", "users", "status = 'A'", false, t0());
        let tb = s.table("s", "public", "users").unwrap();
        assert!(
            tb.where_ == "status = 'A'" && tb.wheres == strs(&["status = 'A'"]),
            "prod 가 아니면 WHERE 도 남긴다: {tb:?}"
        );
        // 나중에 prod 로 보이게 된 접속이면 남은 WHERE 기록도 지운다
        s.set_view("s", "public", "users", "", "status = 'A'", true, t0());
        let tb = s.table("s", "public", "users").unwrap();
        assert!(tb.where_.is_empty() && tb.wheres.is_empty(), "prod 면 지운다: {tb:?}");
    }

    #[test]
    fn inputs_거르기는_100개_where_는_테이블마다_20개까지_최근_것부터_겹치지_않게() {
        let (_d, mut s, _) = new_store();
        for i in 0..120 {
            s.add_filter(&format!("f{i}"));
        }
        s.add_filter("f50");
        let got = s.filters();
        assert!(
            got.len() == MAX_FILTERS && got[0] == "f50" && got[1] == "f119" && !got[1..].contains(&"f50".to_string()),
            "거르기 기록"
        );
        for i in 0..25 {
            s.add_where("s", "public", "t", &format!("id = {i}"), false, t0());
        }
        let w = s.wheres("s", "public", "t", false);
        assert!(w.len() == MAX_WHERES && w[0] == "id = 24", "WHERE 기록: {w:?}");
    }

    #[test]
    fn save_0600_으로_쓰고_다른_프로젝트를_지키며_다시_읽힌다() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db").join("recent.json");
        let (mut a, _) = load(&path, Path::new("/proj/a")).unwrap();
        a.use_conn("ua", t0());
        a.save().unwrap();
        let (mut b, _) = load(&path, Path::new("/proj/b")).unwrap();
        b.use_conn("ub", t0());
        b.set_tree(&strs(&["c:ub"]), &strs(&["g:etc"]));
        b.save().unwrap();
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let (a2, err) = load(&path, Path::new("/proj/a")).unwrap();
        assert!(err.is_none());
        assert_eq!(a2.top_conns(t0(), 5), strs(&["ua"]), "다른 프로젝트 기록을 지킨다");
        let (b2, _) = load(&path, Path::new("/proj/b")).unwrap();
        assert_eq!(b2.tree(), (strs(&["c:ub"]), strs(&["g:etc"])), "트리");
    }

    #[test]
    fn load_깨진_파일은_빈_기억과_오류이고_저장하면_새로_쓴다() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recent.json");
        fs::write(&path, "{not json").unwrap();
        let (mut s, err) = load(&path, &dir.path().join("p")).unwrap();
        assert!(
            err.is_some() && s.filters().is_empty() && s.top_actions(t0(), 5).is_empty(),
            "빈 기억과 오류"
        );
        s.add_filter("x");
        s.save().unwrap();
        let (s2, err) = load(&path, &dir.path().join("p")).unwrap();
        assert!(err.is_none() && s2.filters() == strs(&["x"]), "새로 쓴 파일");
    }

    #[test]
    fn save_여러_프로세스가_동시에_써도_다른_프로젝트_항목을_잃지_않는다() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db").join("recent.json");
        const N: usize = 8;
        let stores: Vec<Store> = (0..N)
            .map(|i| {
                let (mut s, _) = load(&path, &dir.path().join(format!("p{i}"))).unwrap();
                s.lock_wait = Duration::from_secs(60); // 시험 기계가 느려도 줄을 서다 포기하지 않게
                s
            })
            .collect();
        // 각 Store 는 따로 켠 db 처럼 다시 읽기·합치기·바꿔 끼우기를 되풀이한다
        thread::scope(|sc| {
            for mut s in stores {
                sc.spawn(move || {
                    for j in 0..20 {
                        s.touch_table("c", "public", &format!("t{j}"), t0());
                        s.save().unwrap();
                    }
                });
            }
        });
        assert_eq!(read_file(&path).unwrap().projects.len(), N, "프로젝트가 다 남는다");
    }

    /// Go 판이 쓴 recent.json 이다(internal/recent.Store.Save). 테이블 키에 \x00 이 든다.
    const GO_RECENT: &str = r#"{"actions":{"run":{"n":2,"at":"2026-11-16T10:15:30.12+09:00"}},"filters":["foo\u0026bar"],"projects":{"/proj/a":{"conns":{"uuid-1":{"n":1,"at":"2026-11-16T10:15:30.12+09:00"}},"tables":{"uuid-1\u0000public\u0000users":{"conn":"uuid-1","schema":"public","name":"users","at":"2026-11-16T10:15:30.12+09:00","widths":{"email":30},"order":"id DESC","where":"a \u003c 1","wheres":["a \u003c 1"]}},"open":["c:uuid-1"],"closed":["g:etc"]}}}"#;

    #[test]
    fn go_판이_쓴_기억을_읽고_같은_바이트로_다시_쓴다() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recent.json");
        fs::write(&path, GO_RECENT).unwrap();
        let (s, err) = load(&path, Path::new("/proj/a")).unwrap();
        assert!(err.is_none(), "{err:?}");
        let at = GoTime::parse("2026-11-16T10:15:30.12+09:00").unwrap();
        assert_eq!(s.actions["run"], Use { n: 2, at });
        assert_eq!(s.filters(), strs(&["foo&bar"]));
        assert_eq!(s.top_conns(at, 5), strs(&["uuid-1"]));
        let t = s.table("uuid-1", "public", "users").unwrap();
        assert!(
            t.order == "id DESC" && t.where_ == "a < 1" && t.wheres == strs(&["a < 1"]) && t.widths["email"] == 30,
            "{t:?}"
        );
        assert_eq!(s.tree(), (strs(&["c:uuid-1"]), strs(&["g:etc"])));
        s.save().unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), GO_RECENT);
    }
}
