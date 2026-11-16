//! 실행한 쿼리를 JSONL 파일 하나에 쌓고 최근 것부터 찾아 준다. 기록 줄이 만든 결과 사본(R11.4)도 둔다.
//! Go `internal/history` 와 같은 파일(경로·줄 모양·권한·잠금)을 읽고 써서 두 판을 바꿔 써도 기록을 잃지 않는다.
//! 다른 상태 파일(session·paramstore·recent·favorites)이 같이 쓰는 Go 호환 JSON 도움도 여기 둔다.

use std::collections::{BTreeMap, HashMap};
use std::env;
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, FileExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use chrono::{DateTime, Datelike, Days, FixedOffset, Local, NaiveDate, NaiveTime, TimeDelta, TimeZone, Timelike, Utc};
use flate2::Compression;
use flate2::read::MultiGzDecoder;
use flate2::write::GzEncoder;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::atomicfile;

// ─── Go time.Time ───

/// Go `time.Time` 과 같은 JSON 글(RFC3339Nano, 끝의 0 을 뗀 소수 초와 오프셋)을 읽고 쓰는 시각이다.
/// 기본값은 Go 의 0 시각(`0001-01-01T00:00:00Z`)이다. 비교는 순간으로 하므로 오프셋이 달라도 같은 순간이면 같다.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GoTime(pub DateTime<FixedOffset>);

impl Default for GoTime {
    fn default() -> Self {
        Utc.with_ymd_and_hms(1, 1, 1, 0, 0, 0).unwrap().into()
    }
}

impl<Tz: TimeZone> From<DateTime<Tz>> for GoTime {
    fn from(t: DateTime<Tz>) -> Self {
        GoTime(t.fixed_offset())
    }
}

impl GoTime {
    /// 지금 지역 시각이다(Go `time.Now()`).
    pub fn now() -> Self {
        Local::now().into()
    }

    pub fn is_zero(&self) -> bool {
        *self == GoTime::default()
    }

    /// 초 아래를 버린다(Go `Truncate(time.Second)`).
    pub fn truncate_secs(self) -> Self {
        GoTime(self.0.with_nanosecond(0).unwrap_or(self.0))
    }

    /// self - other 를 나노초로 돌려준다. Go `Sub` 처럼 i64 를 넘으면 끝값에 멈춘다.
    pub fn sub_nanos(self, other: GoTime) -> i64 {
        delta_nanos(self.0.signed_duration_since(other.0))
    }

    pub fn parse(s: &str) -> Option<GoTime> {
        DateTime::parse_from_rfc3339(s).ok().map(GoTime)
    }

    /// Go `time.RFC3339Nano` 꼴이다. 오프셋 0 은 `Z`, 소수 초는 끝의 0 을 떼고 0 이면 빠진다.
    pub fn format(&self) -> String {
        let t = self.0.naive_local();
        let mut s = format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
            t.year(),
            t.month(),
            t.day(),
            t.hour(),
            t.minute(),
            t.second()
        );
        let ns = t.nanosecond() % 1_000_000_000;
        if ns > 0 {
            s.push('.');
            s.push_str(format!("{ns:09}").trim_end_matches('0'));
        }
        let off = self.0.offset().local_minus_utc();
        if off == 0 {
            s.push('Z');
        } else {
            let a = off.abs();
            s.push_str(&format!("{}{:02}:{:02}", if off < 0 { '-' } else { '+' }, a / 3600, a / 60 % 60));
        }
        s
    }
}

impl fmt::Display for GoTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.format())
    }
}

impl Serialize for GoTime {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.format())
    }
}

impl<'de> Deserialize<'de> for GoTime {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // Go 는 null 을 0 시각으로 둔다
        match Option::<String>::deserialize(d)? {
            None => Ok(GoTime::default()),
            Some(s) => GoTime::parse(&s).ok_or_else(|| serde::de::Error::custom(format!("parsing time {s:?}"))),
        }
    }
}

fn delta_nanos(d: TimeDelta) -> i64 {
    d.num_nanoseconds()
        .unwrap_or(if d < TimeDelta::zero() { i64::MIN } else { i64::MAX })
}

// ─── Go 호환 JSON ───

/// Go `encoding/json` 과 같은 바이트를 낸다. escape_html 이면 json.Marshal 기본값처럼 `<` `>` `&` 를 `<` 꼴로 바꾸고,
/// U+2028·U+2029 는 늘 이스케이프한다. indent 면 `SetIndent("", "  ")` 꼴이다(끝 줄바꿈은 부르는 쪽 몫).
/// serde_json 은 그 글자들을 문자열 안에서만 날것으로 쓰므로 출력 전체를 바꿔도 된다.
pub(crate) fn go_json<T: Serialize + ?Sized>(v: &T, escape_html: bool, indent: bool) -> io::Result<Vec<u8>> {
    let s = if indent {
        serde_json::to_string_pretty(v)
    } else {
        serde_json::to_string(v)
    }
    .map_err(io::Error::other)?;
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' if escape_html => out.push_str("\\u003c"),
            '>' if escape_html => out.push_str("\\u003e"),
            '&' if escape_html => out.push_str("\\u0026"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c => out.push(c),
        }
    }
    Ok(out.into_bytes())
}

/// Go 가 nil 슬라이스·맵을 `null` 로 쓴 칸을 빈 값으로 읽는다.
pub(crate) fn null_default<'de, D: Deserializer<'de>, T: Deserialize<'de> + Default>(d: D) -> Result<T, D::Error> {
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// 값이 `null` 인 항목은 빼고 맵을 읽는다. Go 는 nil 포인터 값을 `null` 로 쓴다.
pub(crate) fn skip_null_values<'de, D: Deserializer<'de>, V: Deserialize<'de>>(d: D) -> Result<BTreeMap<String, V>, D::Error> {
    let m: Option<BTreeMap<String, Option<V>>> = Option::deserialize(d)?;
    Ok(m.unwrap_or_default().into_iter().filter_map(|(k, v)| v.map(|v| (k, v))).collect())
}

pub(crate) fn is_false(b: &bool) -> bool {
    !*b
}

pub(crate) fn is_zero_int(n: &i64) -> bool {
    *n == 0
}

/// Go `bytes.TrimSpace(data)` 가 비는지다.
pub(crate) fn blank(data: &[u8]) -> bool {
    std::str::from_utf8(data).map_or_else(|_| data.trim_ascii().is_empty(), |s| s.trim().is_empty())
}

/// Go `strings.ToLower` 처럼 글자마다 소문자로 바꾼다. str::to_lowercase 의 낱말 끝 시그마 규칙은 Go 에 없다.
pub(crate) fn go_lower(s: &str) -> String {
    s.chars().flat_map(char::to_lowercase).collect()
}

/// s 를 앞 max 바이트까지 남긴다. 바이트로 자른 끝의 깨진 UTF-8 조각은 버린다(Go `strings.ToValidUTF8(s[:max], "")`).
pub(crate) fn cap_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut n = max;
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    &s[..n]
}

/// Go `os.MkdirAll(dir, mode)` 이다. 이미 있는 폴더의 권한은 바꾸지 않는다.
pub(crate) fn mkdir_all(dir: &Path, mode: u32) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(mode).create(dir)
}

/// `<상태 폴더>/db/<name>` 이다. XDG 명세는 상대 경로면 무시하라고 해서 XDG_STATE_HOME 이 절대 경로일 때만 따른다.
pub(crate) fn state_file(name: &str) -> io::Result<PathBuf> {
    state_file_from(env::var_os("XDG_STATE_HOME"), env::var_os("HOME"), name)
}

pub(crate) fn state_file_from(xdg: Option<OsString>, home: Option<OsString>, name: &str) -> io::Result<PathBuf> {
    if let Some(dir) = xdg.map(PathBuf::from).filter(|d| d.is_absolute()) {
        return Ok(dir.join("db").join(name));
    }
    // Go os.UserHomeDir 는 $HOME 만 본다
    match home.filter(|h| !h.is_empty()) {
        Some(h) => Ok(PathBuf::from(h).join(".local").join("state").join("db").join(name)),
        None => Err(io::Error::new(io::ErrorKind::NotFound, "$HOME is not defined")),
    }
}

fn lock_mutex<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

// ─── 기록 ───

// 상한을 넘으면 4/5 까지 줄인다. 상한에 딱 맞추면 그다음 append 마다 파일 전체를 다시 쓰게 된다(E63).
const MAX_ENTRIES: usize = 5000;
const MAX_BYTES: u64 = 5 << 20;
/// 큰 스크립트 하나가 기록을 다 밀어내지 않게 문장 하나는 앞 64KiB 만 남긴다
const MAX_SQL_BYTES: usize = 64 << 10;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Entry {
    /// 결과 사본 파일 이름(R11.4). 옛 줄은 비어 있다
    #[serde(skip_serializing_if = "String::is_empty")]
    pub id: String,
    pub time: GoTime,
    pub conn: String,
    pub sql: String,
    /// 나노초다(Go `time.Duration`)
    pub elapsed: i64,
    pub rows: i64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub err: String,
    /// 결과 사본을 썼다. 그 뒤 보존 기간·d 로 지워졌을 수 있다
    #[serde(skip_serializing_if = "is_false")]
    pub result: bool,
}

/// query 는 SQL 이나 접속 이름에서 대소문자 없이 찾을 부분 문자열, conn 은 정확히 같은 접속 이름이다.
/// from·to 는 [from, to) 시각 범위이고 None 이면 그쪽 끝이 없다. limit 0 은 전부다.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Filter {
    pub query: String,
    pub conn: String,
    pub from: Option<GoTime>,
    pub to: Option<GoTime>,
    pub limit: usize,
}

/// 기록 창 거르기 글을 Filter 로 바꾼다. 낱말 `오늘`·`today`·`어제`·`yesterday` 는 now 의 지역 날짜로
/// 거르고(여럿이면 마지막 것), 나머지 낱말은 빈칸 하나로 이어 SQL·접속 이름에서 찾는다.
///   - 예: "어제 orders" → 어제 0시~오늘 0시 기록 가운데 orders 가 든 것
pub fn parse_query<Tz: TimeZone>(s: &str, now: &DateTime<Tz>) -> Filter {
    let mut f = Filter::default();
    let mut rest = Vec::new();
    for w in s.split_whitespace() {
        let back = match go_lower(w).as_str() {
            "오늘" | "today" => 0,
            "어제" | "yesterday" => 1,
            _ => {
                rest.push(w);
                continue;
            }
        };
        let day = now.date_naive() - Days::new(back);
        f.from = Some(midnight(&now.timezone(), day));
        f.to = Some(midnight(&now.timezone(), day + Days::new(1)));
    }
    f.query = rest.join(" ");
    f
}

fn midnight<Tz: TimeZone>(tz: &Tz, day: NaiveDate) -> GoTime {
    let naive = day.and_time(NaiveTime::MIN);
    tz.from_local_datetime(&naive)
        .earliest()
        .unwrap_or_else(|| tz.from_utc_datetime(&naive))
        .into()
}

/// 기록 하나의 이름이다. 시각(UTC, 정렬되게)에 무작위 6자를 붙여 같은 초의 기록도 겹치지 않는다.
///   - 예: 20261116-011530-a3f9c1
pub fn new_id(t: GoTime) -> String {
    let r = atomicfile::random_u64() & 0xff_ffff;
    format!("{}-{r:06x}", t.0.with_timezone(&Utc).format("%Y%m%d-%H%M%S"))
}

/// 기록 파일 경로다. XDG_STATE_HOME 이 절대 경로일 때만 따른다.
pub fn default_path() -> io::Result<PathBuf> {
    state_file("history.jsonl")
}

/// 기록을 메모리(오래된 것부터)와 파일에 같이 둔다. 메서드는 동시에 불러도 된다.
/// 다른 프로세스(MCP 서버, Go 판 db 등)가 같은 파일에 붙여 쓴 기록은 다시 open 하거나 줄일 때 읽힌다.
/// 붙여 쓰기와 줄이기는 옆 .lock 파일의 flock 으로 프로세스끼리도 순서를 맞춘다.
pub struct Store {
    path: PathBuf,
    inner: Mutex<Inner>,
}

struct Inner {
    entries: Vec<Entry>,
    /// 파일 크기
    size: u64,
}

impl Inner {
    fn over(&self) -> bool {
        self.entries.len() > MAX_ENTRIES || self.size > MAX_BYTES
    }
}

impl Store {
    /// 파일이 없으면 만든다(폴더 0700, 파일 0600). 읽지 못하는 줄은 건너뛴다.
    pub fn open(path: &Path) -> io::Result<Store> {
        mkdir_all(path.parent().unwrap_or(Path::new(".")), 0o700)?;
        // Go 는 O_CREATE|O_RDONLY 로 연다. 있는 파일은 쓰기 권한 없이도 열려야 해서 없을 때만 쓰기로 만든다
        match File::open(path) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                OpenOptions::new().write(true).create(true).truncate(false).mode(0o600).open(path)?;
            }
            Err(e) => return Err(e),
        }
        let (entries, size) = read_tail(path)?;
        let s = Store {
            path: path.to_path_buf(),
            inner: Mutex::new(Inner { entries, size }),
        };
        let mut inner = lock_mutex(&s.inner);
        if inner.over() {
            let _lock = atomicfile::lock(path)?;
            s.compact_locked(&mut inner)?;
        }
        drop(inner);
        Ok(s)
    }

    pub fn append(&self, mut e: Entry) -> io::Result<()> {
        e.sql = cap_bytes(&e.sql, MAX_SQL_BYTES).to_string();
        let mut line = go_json(&e, true, false)?;
        line.push(b'\n');
        let mut inner = lock_mutex(&self.inner);
        let _lock = atomicfile::lock(&self.path)?;
        // 잠금을 잡은 뒤 열어야 다른 프로세스가 줄이며 바꿔 끼운 새 파일에 쓴다
        let mut f = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .mode(0o600)
            .open(&self.path)?;
        let size = f.metadata()?.len();
        // 쓰다 끊긴 줄(줄바꿈 없는 끝)에 이어 쓰면 새 기록까지 깨지므로 줄을 먼저 끝낸다
        if size > 0 {
            let mut last = [0u8];
            f.read_exact_at(&mut last, size - 1)
                .map_err(|e| io::Error::new(e.kind(), format!("read history: {e}")))?;
            if last[0] != b'\n' {
                line.insert(0, b'\n');
            }
        }
        f.write_all(&line)?;
        drop(f);
        inner.size = size + line.len() as u64;
        inner.entries.push(e);
        if inner.over() {
            return self.compact_locked(&mut inner);
        }
        Ok(())
    }

    /// 최근 것부터 돌려준다.
    pub fn list(&self, f: &Filter) -> Vec<Entry> {
        let q = go_lower(&f.query);
        let inner = lock_mutex(&self.inner);
        let mut out = Vec::new();
        for e in inner.entries.iter().rev() {
            if f.limit > 0 && out.len() >= f.limit {
                break;
            }
            if (!f.conn.is_empty() && e.conn != f.conn) || f.from.is_some_and(|t| e.time < t) || f.to.is_some_and(|t| e.time >= t) {
                continue;
            }
            if go_lower(&e.sql).contains(&q) || go_lower(&e.conn).contains(&q) {
                out.push(e.clone());
            }
        }
        out
    }

    /// 파일을 다시 읽어(다른 프로세스가 붙인 기록까지) 최근 것만 남기고, 임시 파일에 다 쓴 뒤 바꿔 끼운다.
    /// 파일 잠금을 잡은 채로 불러야 다시 읽은 뒤 바꿔 끼우기 전에 남이 붙인 줄을 잃지 않는다.
    fn compact_locked(&self, inner: &mut Inner) -> io::Result<()> {
        let (mut entries, _) = read_tail(&self.path)?;
        let mut lines = Vec::new();
        let mut total = 0usize;
        for e in entries.iter().rev() {
            if lines.len() >= MAX_ENTRIES * 4 / 5 {
                break;
            }
            let line = go_json(e, true, false)?;
            if (total + line.len() + 1) as u64 > MAX_BYTES * 4 / 5 {
                break;
            }
            total += line.len() + 1;
            lines.push(line);
        }
        let mut buf = Vec::with_capacity(total);
        for line in lines.iter().rev() {
            buf.extend_from_slice(line);
            buf.push(b'\n');
        }
        atomicfile::write(&self.path, ".history-*.jsonl", &buf, 0o600)?;
        let keep = entries.len() - lines.len();
        inner.entries = entries.split_off(keep);
        inner.size = buf.len() as u64;
        Ok(())
    }
}

/// 파일 끝 MAX_BYTES 만 읽어 기록으로 바꾼다. 파일이 아무리 커져도 읽는 양이 정해져 있다(E63).
/// 오래된 것부터 최대 MAX_ENTRIES 개와 파일 전체 크기를 돌려준다.
fn read_tail(path: &Path) -> io::Result<(Vec<Entry>, u64)> {
    let f = File::open(path)?;
    let size = f.metadata()?.len();
    let from = size.saturating_sub(MAX_BYTES);
    let mut buf = vec![0u8; (size - from) as usize];
    // 다른 프로세스가 그사이 파일을 줄였으면 덜 읽힌다. 읽힌 만큼만 쓴다
    let mut n = 0;
    while n < buf.len() {
        match f.read_at(&mut buf[n..], from + n as u64) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(io::Error::new(e.kind(), format!("read history: {e}"))),
        }
    }
    buf.truncate(n);
    let mut body: &[u8] = &buf;
    if from > 0 {
        // 중간부터 읽었으면 첫 줄은 잘린 조각이다
        body = body.iter().position(|&b| b == b'\n').map_or(&[], |i| &body[i + 1..]);
    }
    let mut entries: Vec<Entry> = body
        .split(|&b| b == b'\n')
        // 깨진 줄(쓰다 끊긴 줄, 손으로 고친 줄)은 기록 하나만 잃고 나머지를 살리려고 건너뛴다
        .filter(|line| !blank(line))
        .filter_map(|line| serde_json::from_slice(line).ok())
        .collect();
    if entries.len() > MAX_ENTRIES {
        entries.drain(..entries.len() - MAX_ENTRIES);
    }
    Ok((entries, size))
}

// ─── 결과 사본 (R11.4) ───
// 기록 한 줄이 만든 표·글 결과를 <상태 폴더>/db/results/<id>.json.gz 에 둔다. 사본은 덤이라 쓰지 못하면 끄고 기록은 이어 간다.

/// 사본에 남기는 앞 행 수
pub const SNAPSHOT_ROWS: usize = 1000;
/// 사본 JSON(압축 전) 상한
pub const SNAPSHOT_BYTES: usize = 1 << 20;

const SNAPSHOT_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 3600);
const SNAPSHOT_MAX_TOTAL: u64 = 200 << 20;
/// 쓸 때마다 폴더를 훑지 않게 줄이기는 이 간격에 한 번이다
const PRUNE_EVERY: Duration = Duration::from_secs(10 * 60);
const SNAP_EXT: &str = ".json.gz";
/// 쓰다 죽은 프로세스가 남긴 임시 파일(atomicfile 의 .snap-*)은 이만큼 지나면 지운다. 쓰는 중인 파일은 건드리지 않게 넉넉히 둔다
const SNAP_TEMP: &str = ".snap-";
const SNAP_TEMP_MAX_AGE: Duration = Duration::from_secs(3600);

/// ROWS 필드 머리 `,"rows":[` 와 끝 `]` 의 길이에서 쉼표 하나를 뺀 값이다. 행마다 (크기+1) 를 더한다
const ROWS_FRAME: usize = r#","rows":[]"#.len() - 1;

/// 사본 이름으로 받는 꼴(`^[0-9]{8}-[0-9]{6}-[0-9a-f]{6}$`, new_id)인지다. CLI 가 받은 글로 폴더 밖 경로를 열지 않게 한다.
fn valid_id(id: &str) -> bool {
    let b = id.as_bytes();
    b.len() == 22
        && b.iter().enumerate().all(|(i, &c)| match i {
            8 | 15 => c == b'-',
            0..15 => c.is_ascii_digit(),
            _ => c.is_ascii_digit() || (b'a'..=b'f').contains(&c),
        })
}

fn bad_id(id: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, format!("bad snapshot id {id:?}"))
}

/// 결과 하나의 사본이다. row_count 는 자르기 전 받은 행 수, truncated 는 사본이 행·크기 상한에서 잘렸다는 뜻,
/// result_truncated 는 결과 자체가 화면 상한(db.MaxRows)에서 잘렸다는 뜻이다.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Snapshot {
    pub id: String,
    pub time: GoTime,
    pub conn: String,
    pub sql: String,
    #[serde(skip_serializing_if = "Vec::is_empty", deserialize_with = "null_default")]
    pub cols: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", deserialize_with = "null_default")]
    pub types: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", deserialize_with = "null_default")]
    pub rows: Vec<Vec<String>>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub text: String,
    /// 나노초다(Go `time.Duration`)
    pub elapsed: i64,
    pub row_count: i64,
    #[serde(skip_serializing_if = "is_false")]
    pub truncated: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub result_truncated: bool,
}

fn json_len<T: Serialize + ?Sized>(v: &T) -> usize {
    go_json(v, true, false).map_or(0, |b| b.len())
}

impl Snapshot {
    /// SQL 을 기록 줄처럼 앞 64KiB 로 자르고, 앞 SNAPSHOT_ROWS 행만 남기고, JSON 이 SNAPSHOT_BYTES 를 넘지 않게 뒤 행과 글을 버린다.
    /// 행·글을 버렸으면 truncated 다. 다 들어가면 그대로 두고, 버려야 하면 `"truncated":true` 를 먼저 달고 그 크기까지 넣어 잰다.
    /// 행마다 크기를 더해 가며 자르므로 큰 결과도 한 번만 직렬화한다. JSON 크기는 Go json.Marshal 결과와 바이트까지 같게 센다.
    /// 단, 열 이름 같은 머리만으로 상한을 넘으면 여기서는 줄이지 못하고 Results::write 가 too large 로 거절한다.
    pub fn cap(&mut self) {
        self.sql = cap_bytes(&self.sql, MAX_SQL_BYTES).to_string();
        if self.rows.len() > SNAPSHOT_ROWS {
            self.rows.truncate(SNAPSHOT_ROWS);
            self.truncated = true;
        }
        let mut rows = std::mem::take(&mut self.rows);
        let sizes: Vec<usize> = rows.iter().map(json_len).collect();
        let all = ROWS_FRAME + sizes.iter().map(|n| n + 1).sum::<usize>();
        let head = json_len(self);
        if !self.truncated && ((rows.is_empty() && head <= SNAPSHOT_BYTES) || head + all <= SNAPSHOT_BYTES) {
            self.rows = rows;
            return;
        }
        self.truncated = true;
        let mut head = json_len(self);
        let budget = SNAPSHOT_BYTES as i64 - head as i64;
        if self.text.len() as i64 > budget / 2 {
            // 글은 JSON 이스케이프로 늘 수 있어 남은 몫의 반만 둔다
            self.text = cap_bytes(&self.text, (budget / 2).max(0) as usize).to_string();
            head = json_len(self);
        }
        let (mut size, mut n) = (head + ROWS_FRAME, 0);
        while n < sizes.len() && size + sizes[n] < SNAPSHOT_BYTES {
            size += sizes[n] + 1;
            n += 1;
        }
        rows.truncate(n);
        self.rows = rows;
    }
}

/// 사본 폴더다. 메서드는 동시에 불러도 된다. have 는 폴더에 있는 사본의 이름과 크기라
/// 기록 창이 줄마다 파일을 보지 않고 ◆ 를 그린다.
pub struct Results {
    dir: PathBuf,
    state: Mutex<ResultsState>,
    off: AtomicBool,
    max_age: Duration,
    max_total: u64,
}

struct ResultsState {
    have: HashMap<String, u64>,
    pruned: Option<GoTime>,
}

/// 기록 파일 옆 results 폴더다.
pub fn results_dir(history_path: &Path) -> PathBuf {
    history_path.parent().unwrap_or(Path::new(".")).join("results")
}

/// Results::write 가 사본을 남기지 않은 까닭이 상한 초과인지다. 쓰기 실패가 아니라 사본을 끄지 않는다.
pub fn is_too_large(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::FileTooLarge
}

impl Results {
    /// 폴더가 없으면 0700 으로 만들고(있으면 0700 으로 좁히고) 있는 사본 목록을 읽는다.
    pub fn open(dir: &Path) -> io::Result<Results> {
        mkdir_all(dir, 0o700)?;
        // 이미 있는 폴더의 권한은 mkdir 이 바꾸지 않는다. 사본은 실제 데이터라 넓게 만들어진 폴더도 좁힌다
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        Results::read_dir(dir)
    }

    /// 폴더를 만들거나 고치지 않고 사본 목록만 읽는다(db history). 폴더가 없으면 NotFound 오류다.
    pub fn read_dir(dir: &Path) -> io::Result<Results> {
        let mut have = HashMap::new();
        for e in fs::read_dir(dir)? {
            let e = e?;
            let name = e.file_name();
            let Some(id) = name.to_str().and_then(|n| n.strip_suffix(SNAP_EXT)).filter(|id| valid_id(id)) else {
                continue;
            };
            if let Ok(m) = e.metadata() {
                have.insert(id.to_string(), m.len());
            }
        }
        Ok(Results {
            dir: dir.to_path_buf(),
            state: Mutex::new(ResultsState { have, pruned: None }),
            off: AtomicBool::new(false),
            max_age: SNAPSHOT_MAX_AGE,
            max_total: SNAPSHOT_MAX_TOTAL,
        })
    }

    fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}{SNAP_EXT}"))
    }

    /// 쓰기 실패로 사본을 껐는지다.
    pub fn off(&self) -> bool {
        self.off.load(Ordering::SeqCst)
    }

    /// 사본을 끈다. 처음 끈 때만 참이라 알림을 한 번만 낸다.
    pub fn disable(&self) -> bool {
        self.off.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_ok()
    }

    /// id 의 사본이 폴더에 있는지다.
    pub fn has(&self, id: &str) -> bool {
        lock_mutex(&self.state).have.contains_key(id)
    }

    /// cap 을 거친 사본을 gzip JSON 으로 바꿔 끼워 쓴다(파일 0600). 그래도 상한을 넘으면 쓰지 않고 `is_too_large` 오류다.
    pub fn write(&self, mut s: Snapshot) -> io::Result<()> {
        if !valid_id(&s.id) {
            return Err(bad_id(&s.id));
        }
        s.cap();
        let data = go_json(&s, true, false)?;
        if data.len() > SNAPSHOT_BYTES {
            return Err(io::Error::new(io::ErrorKind::FileTooLarge, "snapshot too large"));
        }
        let mut zw = GzEncoder::new(Vec::new(), Compression::default());
        zw.write_all(&data)?;
        let gz = zw.finish()?;
        atomicfile::write(&self.path(&s.id), ".snap-*", &gz, 0o600)?;
        lock_mutex(&self.state).have.insert(s.id, gz.len() as u64);
        Ok(())
    }

    /// id 의 사본을 읽는다. 없으면 NotFound 오류다.
    pub fn read(&self, id: &str) -> io::Result<Snapshot> {
        if !valid_id(id) {
            return Err(bad_id(id));
        }
        let f = File::open(self.path(id))?;
        // cap 이 1MiB 로 자르므로 손으로 넣은 큰 파일만 이 상한에 걸린다
        let r = BufReader::new(MultiGzDecoder::new(BufReader::new(f)).take(4 * SNAPSHOT_BYTES as u64));
        // Go json.Decoder 처럼 첫 값만 읽고 뒤는 보지 않는다
        match serde_json::Deserializer::from_reader(r).into_iter::<Snapshot>().next() {
            Some(Ok(s)) => Ok(s),
            Some(Err(e)) => Err(io::Error::new(io::ErrorKind::InvalidData, format!("read snapshot {id}: {e}"))),
            None => Err(io::Error::new(io::ErrorKind::UnexpectedEof, format!("read snapshot {id}: EOF"))),
        }
    }

    /// id 의 사본을 지운다. 없던 사본은 오류가 아니다.
    pub fn delete(&self, id: &str) -> io::Result<()> {
        if !valid_id(id) {
            return Err(bad_id(id));
        }
        match fs::remove_file(self.path(id)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        lock_mutex(&self.state).have.remove(id);
        Ok(())
    }

    /// 마지막 줄이기에서 PRUNE_EVERY 가 지났을 때만 prune 한다. 쓰기 뒤에 부른다.
    pub fn maybe_prune(&self, now: GoTime) -> io::Result<()> {
        let due = lock_mutex(&self.state)
            .pruned
            .is_none_or(|p| now.sub_nanos(p) >= PRUNE_EVERY.as_nanos() as i64);
        if !due {
            return Ok(());
        }
        self.prune(now)
    }

    /// max_age(30일)보다 오래된 사본과 한 시간 넘은 임시 파일을 지우고, 남은 합이 max_total(200MiB)을 넘으면 오래된 것부터 지운다.
    /// 나이는 파일 수정 시각으로 잰다. 다른 프로세스가 같은 폴더를 줄여도 없는 파일은 건너뛴다.
    pub fn prune(&self, now: GoTime) -> io::Result<()> {
        lock_mutex(&self.state).pruned = Some(now);
        let age = |m: &fs::Metadata| {
            m.modified()
                .ok()
                .map(|t| delta_nanos(now.0.signed_duration_since(DateTime::<Utc>::from(t))))
        };
        let mut keep: Vec<(String, std::time::SystemTime, u64)> = Vec::new();
        let mut errs = Vec::new();
        let mut total = 0u64;
        for e in fs::read_dir(&self.dir)? {
            let Ok(e) = e else { continue };
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with(SNAP_TEMP) {
                if let Ok(m) = e.metadata()
                    && age(&m).is_some_and(|a| a > SNAP_TEMP_MAX_AGE.as_nanos() as i64)
                {
                    match fs::remove_file(self.dir.join(&name)) {
                        Err(err) if err.kind() != io::ErrorKind::NotFound => errs.push(Err(err)),
                        _ => {}
                    }
                }
                continue;
            }
            let Some(id) = name.strip_suffix(SNAP_EXT).filter(|id| valid_id(id)) else {
                continue;
            };
            let Ok(m) = e.metadata() else { continue };
            let Ok(at) = m.modified() else { continue };
            if age(&m).is_some_and(|a| a > self.max_age.as_nanos() as i64) {
                errs.push(self.delete(id));
                continue;
            }
            keep.push((id.to_string(), at, m.len()));
            total += m.len();
        }
        keep.sort_by_key(|k| k.1);
        for (id, _, size) in keep {
            if total <= self.max_total {
                break;
            }
            errs.push(self.delete(&id));
            total -= size;
        }
        atomicfile::join(errs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> GoTime {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap().into()
    }

    fn add(t: GoTime, d: TimeDelta) -> GoTime {
        GoTime(t.0 + d)
    }

    fn open_temp() -> (tempfile::TempDir, Store, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state").join("db").join("history.jsonl");
        let s = Store::open(&path).unwrap();
        (dir, s, path)
    }

    fn join_sql(es: &[Entry]) -> String {
        es.iter().map(|e| e.sql.as_str()).collect::<Vec<_>>().join("|")
    }

    fn mode(p: &Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    fn filter() -> Filter {
        Filter::default()
    }

    // ─── 기록 ───

    #[test]
    fn open_폴더는_0700_파일은_0600으로_만든다() {
        let (_d, _s, path) = open_temp();
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn list_최근_것부터_거르고_다시_열어도_남는다() {
        let (_d, s, path) = open_temp();
        let base = utc(2026, 11, 14, 9, 0, 0);
        let es = [
            Entry {
                conn: "a".into(),
                sql: "select 1".into(),
                ..Default::default()
            },
            Entry {
                conn: "b".into(),
                sql: "SELECT name from users".into(),
                err: "boom".into(),
                ..Default::default()
            },
            Entry {
                conn: "a".into(),
                sql: "update t set x = 1".into(),
                rows: 3,
                ..Default::default()
            },
            Entry {
                conn: "a".into(),
                sql: "select * from Users".into(),
                ..Default::default()
            },
        ];
        for (i, mut e) in es.into_iter().enumerate() {
            e.time = add(base, TimeDelta::minutes(i as i64));
            e.elapsed = i as i64 * 1_000_000;
            s.append(e).unwrap();
        }
        assert_eq!(
            join_sql(&s.list(&filter())),
            "select * from Users|update t set x = 1|SELECT name from users|select 1"
        );
        assert_eq!(
            join_sql(&s.list(&Filter {
                query: "USERS".into(),
                ..filter()
            })),
            "select * from Users|SELECT name from users"
        );
        assert_eq!(
            join_sql(&s.list(&Filter {
                conn: "a".into(),
                limit: 2,
                ..filter()
            })),
            "select * from Users|update t set x = 1"
        );
        let re = Store::open(&path).unwrap();
        let got = re.list(&Filter {
            conn: "b".into(),
            ..filter()
        });
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].err, "boom");
        assert_eq!(got[0].time, add(base, TimeDelta::minutes(1)));
        assert_eq!(got[0].elapsed, 1_000_000);
    }

    #[test]
    fn open_깨진_줄은_건너뛴다() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        let data = concat!(
            r#"{"conn":"a","sql":"select 1"}"#,
            "\n",
            r#"{"conn":"a","sql":"#,
            "\ngarbage\n\n",
            r#"{"conn":"b","sql":"select 2"}"#,
            "\n",
            r#"{"conn":"c","sq"#
        );
        fs::write(&path, data).unwrap();
        let s = Store::open(&path).unwrap();
        let got = s.list(&filter());
        assert_eq!(join_sql(&got), "select 2|select 1");
    }

    #[test]
    fn append_개수_상한을_넘으면_최근_것만_남긴다() {
        let (_d, s, path) = open_temp();
        for i in 0..=MAX_ENTRIES {
            s.append(Entry {
                sql: format!("select {i}"),
                ..Default::default()
            })
            .unwrap();
        }
        let got = s.list(&filter());
        assert_eq!(got.len(), MAX_ENTRIES * 4 / 5);
        assert_eq!(got[0].sql, format!("select {MAX_ENTRIES}"));
        let re = Store::open(&path).unwrap();
        assert_eq!(re.list(&filter()).len(), got.len(), "파일과 메모리 개수가 같다");
    }

    #[test]
    fn append_크기_상한과_긴_sql을_자른다() {
        let (_d, s, path) = open_temp();
        let big = "가".repeat(MAX_SQL_BYTES); // 3바이트 글자라 64KiB 경계가 글자 중간에 걸린다
        for _ in 0..2 * MAX_BYTES as usize / MAX_SQL_BYTES {
            s.append(Entry {
                sql: big.clone(),
                ..Default::default()
            })
            .unwrap();
        }
        assert!(fs::metadata(&path).unwrap().len() <= MAX_BYTES);
        let got = &s.list(&Filter { limit: 1, ..filter() })[0].sql;
        assert!(
            got.len() <= MAX_SQL_BYTES && big.starts_with(got.as_str()) && got.len() % 3 == 0,
            "SQL {}바이트",
            got.len()
        );
    }

    #[test]
    fn append_동시에_불러도_기록을_잃지_않는다() {
        let (_d, s, path) = open_temp();
        let s = Arc::new(s);
        let hs: Vec<_> = (0..8)
            .map(|g| {
                let s = Arc::clone(&s);
                thread::spawn(move || {
                    for i in 0..50 {
                        s.append(Entry {
                            conn: g.to_string(),
                            sql: i.to_string(),
                            ..Default::default()
                        })
                        .unwrap();
                        s.list(&Filter {
                            limit: 5,
                            ..Filter::default()
                        });
                    }
                })
            })
            .collect();
        hs.into_iter().for_each(|h| h.join().unwrap());
        assert_eq!(Store::open(&path).unwrap().list(&filter()).len(), 400);
    }

    #[test]
    fn default_path_xdg_state_home을_따르고_없으면_홈_아래() {
        let p = state_file_from(Some("/x/state".into()), Some("/home/u".into()), "history.jsonl").unwrap();
        assert_eq!(p, PathBuf::from("/x/state/db/history.jsonl"));
        let p = state_file_from(Some("relative".into()), Some("/home/u".into()), "history.jsonl").unwrap();
        assert_eq!(p, PathBuf::from("/home/u/.local/state/db/history.jsonl"));
        let p = state_file_from(None, Some("/home/u".into()), "history.jsonl").unwrap();
        assert_eq!(p, PathBuf::from("/home/u/.local/state/db/history.jsonl"));
        assert_eq!(state_file_from(None, None, "x").unwrap_err().to_string(), "$HOME is not defined");
    }

    #[test]
    fn open_상한보다_큰_파일은_끝부분만_읽어_줄인다() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        let mut b = String::new();
        let mut i = 0;
        while b.len() as u64 <= MAX_BYTES + MAX_BYTES / 2 {
            b.push_str(&format!("{{\"conn\":\"a\",\"sql\":\"select {i} {}\"}}\n", "x".repeat(1000)));
            i += 1;
        }
        fs::write(&path, &b).unwrap();
        let s = Store::open(&path).unwrap();
        let size = fs::metadata(&path).unwrap().len();
        let last = b.trim().lines().last().unwrap();
        let newest = s.list(&Filter { limit: 1, ..filter() });
        assert!(size <= MAX_BYTES * 4 / 5, "size {size}");
        assert_eq!(newest.len(), 1);
        assert!(last.contains(&format!("\"{}\"", newest[0].sql)));
    }

    #[test]
    fn append_끊긴_마지막_줄_뒤에도_새_기록을_살린다() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        fs::write(&path, concat!(r#"{"conn":"a","sql":"select 1"}"#, "\n", r#"{"conn":"a","sq"#)).unwrap();
        let s = Store::open(&path).unwrap();
        s.append(Entry {
            conn: "a".into(),
            sql: "select 2".into(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(join_sql(&Store::open(&path).unwrap().list(&filter())), "select 2|select 1");
    }

    #[test]
    fn append_두_store가_붙이고_줄여도_기록을_잃지_않는다() {
        const PER_STORE: usize = 300;
        for round in 0..3 {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("history.jsonl");
            // 상한 바로 아래로 채워 두 Store 가 붙이는 동안 줄이기가 일어나게 한다
            let seed: String = (0..MAX_ENTRIES - 10)
                .map(|i| format!("{{\"conn\":\"seed\",\"sql\":\"seed {i}\"}}\n"))
                .collect();
            fs::write(&path, seed).unwrap();
            let stores = [Store::open(&path).unwrap(), Store::open(&path).unwrap()];
            thread::scope(|sc| {
                for (g, s) in stores.iter().enumerate() {
                    sc.spawn(move || {
                        for i in 0..PER_STORE {
                            s.append(Entry {
                                conn: g.to_string(),
                                sql: format!("{g} {i}"),
                                ..Default::default()
                            })
                            .unwrap();
                        }
                    });
                }
            });
            let re = Store::open(&path).unwrap();
            for g in 0..stores.len() {
                let n = re
                    .list(&Filter {
                        conn: g.to_string(),
                        ..filter()
                    })
                    .len();
                assert_eq!(n, PER_STORE, "{round}회차 Store {g}");
            }
        }
    }

    #[test]
    fn list_거르기는_접속_이름과_날짜_범위도_본다() {
        let (_d, s, _) = open_temp();
        let day = Local.with_ymd_and_hms(2026, 11, 15, 12, 0, 0).unwrap();
        let next = Local.with_ymd_and_hms(2026, 11, 16, 12, 0, 0).unwrap();
        s.append(Entry {
            time: day.into(),
            conn: "app-stg".into(),
            sql: "select 1".into(),
            ..Default::default()
        })
        .unwrap();
        s.append(Entry {
            time: next.into(),
            conn: "log-prod".into(),
            sql: "select 2".into(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            join_sql(&s.list(&Filter {
                query: "PROD".into(),
                ..filter()
            })),
            "select 2",
            "접속 이름으로"
        );
        assert_eq!(join_sql(&s.list(&parse_query("어제", &next))), "select 1");
        let from = GoTime::from(day).0 + TimeDelta::hours(1);
        assert_eq!(
            join_sql(&s.list(&Filter {
                from: Some(GoTime(from)),
                ..filter()
            })),
            "select 2"
        );
    }

    #[test]
    fn parse_query_오늘_어제_낱말은_날짜로_거르고_나머지는_찾을_글이다() {
        let kst = FixedOffset::east_opt(9 * 3600).unwrap();
        let now = kst.with_ymd_and_hms(2026, 11, 16, 0, 30, 0).unwrap();
        let f = parse_query("어제  orders 2", &now);
        assert_eq!(f.query, "orders 2");
        assert_eq!(f.from, Some(kst.with_ymd_and_hms(2026, 11, 15, 0, 0, 0).unwrap().into()));
        assert_eq!(f.to, Some(kst.with_ymd_and_hms(2026, 11, 16, 0, 0, 0).unwrap().into()));
        let f = parse_query("Today", &now);
        assert!(f.query.is_empty() && f.from.unwrap().0.day() == 16, "today: {f:?}");
        let f = parse_query("select 오늘의", &now);
        assert!(f.query == "select 오늘의" && f.from.is_none(), "낱말 통째로 같을 때만 날짜다");
    }

    // ─── 결과 사본 ───

    fn open_results() -> (tempfile::TempDir, Results) {
        let dir = tempfile::tempdir().unwrap();
        let r = Results::open(&dir.path().join("db").join("results")).unwrap();
        (dir, r)
    }

    fn rows_of(n: usize, width: usize) -> Vec<Vec<String>> {
        vec![vec!["x".repeat(width)]; n]
    }

    #[test]
    fn 사본은_gzip_json_0600_으로_쓰고_같은_값으로_읽는다() {
        let (_d, r) = open_results();
        let at = utc(2026, 11, 16, 10, 0, 0);
        let s = Snapshot {
            id: new_id(at),
            time: at,
            conn: "app-stg".into(),
            sql: "select 1".into(),
            cols: vec!["a".into()],
            types: vec!["int4".into()],
            rows: vec![vec!["1".into()]],
            elapsed: 120_000_000,
            row_count: 1,
            ..Default::default()
        };
        r.write(s.clone()).unwrap();
        let path = r.dir.join(format!("{}.json.gz", s.id));
        assert_eq!(mode(&r.dir), 0o700);
        assert_eq!(mode(&path), 0o600);
        let mut raw = String::new();
        MultiGzDecoder::new(File::open(&path).unwrap())
            .read_to_string(&mut raw)
            .expect("gzip 이어야 한다");
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["conn"], "app-stg");
        let got = r.read(&s.id).unwrap();
        assert_eq!(got, s);
        assert!(r.has(&s.id), "쓴 사본은 has");
        assert!(Results::open(&r.dir).unwrap().has(&s.id), "다시 열면 폴더의 사본을 안다");
        r.delete(&s.id).unwrap();
        assert!(!r.has(&s.id), "delete 는 파일과 목록에서 뺀다");
        assert_eq!(r.read(&s.id).unwrap_err().kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn 사본은_1000행과_1mib_에서_잘리고_잘림을_표시한다() {
        let mut s = Snapshot {
            id: new_id(GoTime::now()),
            rows: rows_of(1500, 10),
            row_count: 1500,
            ..Default::default()
        };
        s.cap();
        assert!(s.rows.len() == SNAPSHOT_ROWS && s.truncated && s.row_count == 1500, "행 상한");
        let mut s = Snapshot {
            id: new_id(GoTime::now()),
            rows: rows_of(800, 4000),
            ..Default::default()
        }; // 약 3.2MB
        s.cap();
        let b = go_json(&s, true, false).unwrap();
        assert!(
            b.len() <= SNAPSHOT_BYTES && s.truncated && !s.rows.is_empty() && s.rows.len() < 800,
            "크기 상한: {} {}",
            b.len(),
            s.rows.len()
        );
        let mut s = Snapshot {
            id: new_id(GoTime::now()),
            text: "가".repeat(1 << 20),
            ..Default::default()
        };
        s.cap();
        assert!(go_json(&s, true, false).unwrap().len() <= SNAPSHOT_BYTES && s.truncated, "글 상한");
        let mut s = Snapshot {
            id: new_id(GoTime::now()),
            rows: rows_of(10, 10),
            ..Default::default()
        };
        s.cap();
        assert!(s.rows.len() == 10 && !s.truncated, "작은 결과는 그대로");
    }

    fn set_mtime(p: &Path, t: GoTime) {
        let st = std::time::SystemTime::from(t.0.with_timezone(&Utc));
        File::options()
            .write(true)
            .open(p)
            .unwrap()
            .set_times(fs::FileTimes::new().set_accessed(st).set_modified(st))
            .unwrap();
    }

    #[test]
    fn 줄이기는_30일_넘은_것을_지우고_합이_상한을_넘으면_오래된_것부터_지운다() {
        let (_d, mut r) = open_results();
        let now = GoTime::now();
        let ids: Vec<String> = (0..4)
            .map(|i| {
                let s = Snapshot {
                    id: new_id(add(now, TimeDelta::seconds(i))),
                    rows: rows_of(50, 200),
                    ..Default::default()
                };
                r.write(s.clone()).unwrap();
                s.id
            })
            .collect();
        // 0 은 31일 전, 1·2·3 은 3·2·1시간 전
        let ages = [TimeDelta::days(31), TimeDelta::hours(3), TimeDelta::hours(2), TimeDelta::hours(1)];
        for (id, age) in ids.iter().zip(ages) {
            set_mtime(&r.path(id), add(now, -age));
        }
        let size = fs::metadata(r.path(&ids[1])).unwrap().len();
        r.max_total = 2 * size + 10;
        r.prune(now).unwrap();
        for (i, want) in [false, false, true, true].into_iter().enumerate() {
            assert_eq!(r.has(&ids[i]), want, "{i}: has");
            assert_eq!(r.path(&ids[i]).exists(), want, "{i}: 파일");
        }
        // 줄이기는 간격 안에 다시 하지 않는다
        r.max_total = 0;
        r.maybe_prune(add(now, TimeDelta::minutes(1))).unwrap();
        assert!(r.has(&ids[3]), "간격 안의 maybe_prune 은 아무것도 하지 않는다");
        r.maybe_prune(add(now, TimeDelta::from_std(PRUNE_EVERY).unwrap())).unwrap();
        assert!(!r.has(&ids[3]), "간격이 지나면 줄인다");
    }

    #[test]
    fn 이름_꼴이_아니면_폴더_밖을_읽거나_지우지_않는다() {
        let (_d, r) = open_results();
        for id in ["../history", "", "20261116-101530-a3f9c1/../../x"] {
            let e = r.read(id).unwrap_err();
            assert_ne!(e.kind(), io::ErrorKind::NotFound, "{id:?}");
            assert!(r.delete(id).is_err(), "delete {id:?}");
        }
        assert!(valid_id(&new_id(GoTime::now())), "new_id 는 받는 꼴이어야 한다");
        assert!(valid_id("20261116-011530-a3f9c1"));
        assert!(!valid_id("20261116-011530-A3F9C1") && !valid_id("2026111a-011530-a3f9c1") && !valid_id("20261116-01153a-a3f9c1"));
    }

    #[test]
    fn 한_시간_넘은_임시_파일은_줄일_때_지우고_쓰는_중인_것은_둔다() {
        let (_d, r) = open_results();
        let now = GoTime::now();
        let (stale, fresh) = (r.dir.join(".snap-111"), r.dir.join(".snap-222"));
        for p in [&stale, &fresh] {
            fs::write(p, "x").unwrap();
        }
        set_mtime(&stale, add(now, -TimeDelta::hours(2)));
        r.prune(now).unwrap();
        assert!(!stale.exists(), "오래된 임시 파일은 지운다");
        assert!(fresh.exists(), "쓰는 중일 수 있는 임시 파일은 둔다");
    }

    #[test]
    fn open_results_이미_있는_넓은_폴더도_0700_으로_좁히고_read_dir_는_폴더를_만들지_않는다() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("results");
        assert_eq!(
            Results::read_dir(&dir).err().map(|e| e.kind()),
            Some(io::ErrorKind::NotFound),
            "없는 폴더"
        );
        assert!(!dir.exists(), "read_dir 는 폴더를 만들지 않는다");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap(); // umask 와 상관없이 넓게 둔다
        Results::open(&dir).unwrap();
        assert_eq!(mode(&dir), 0o700);
    }

    #[test]
    fn 큰_sql_은_기록처럼_잘라_상한에_맞추고_머리만으로_넘으면_쓰지_않는다() {
        let (_d, r) = open_results();
        let id = new_id(GoTime::now());
        r.write(Snapshot {
            id: id.clone(),
            sql: "s".repeat(2 << 20),
            rows: rows_of(10, 10),
            ..Default::default()
        })
        .unwrap();
        let s = r.read(&id).unwrap();
        assert!(
            s.sql.len() == MAX_SQL_BYTES && s.rows.len() == 10,
            "2MiB SQL 도 앞 64KiB 로 남기고 다시 읽는다"
        );

        let big = new_id(add(GoTime::now(), TimeDelta::seconds(1)));
        let e = r
            .write(Snapshot {
                id: big.clone(),
                cols: vec!["c".repeat(2 << 20)],
                ..Default::default()
            })
            .unwrap_err();
        assert!(is_too_large(&e), "머리만으로 상한을 넘으면 too large: {e}");
        assert!(!r.has(&big), "쓰지 않은 사본은 없다");
        assert!(!r.path(&big).exists(), "파일도 없다");
    }

    /// JSON 이 정확히 size 바이트인 사본이다. 마지막 행 칸 길이로 맞춘다.
    fn exact_snapshot(size: usize) -> Snapshot {
        let mut s = Snapshot {
            id: "20261116-101530-a3f9c1".into(),
            sql: "select 1".into(),
            rows: rows_of(260, 4000),
            row_count: 260,
            ..Default::default()
        };
        let b = go_json(&s, true, false).unwrap().len();
        let last = s.rows.last_mut().unwrap();
        last[0] = "x".repeat(last[0].len() + size - b);
        assert_eq!(go_json(&s, true, false).unwrap().len(), size, "맞춘 크기");
        s
    }

    #[test]
    fn 상한에_꼭_맞는_사본은_그대로_두고_넘으면_잘림_표시까지_재어_상한_안에_남긴다() {
        let mut s = exact_snapshot(SNAPSHOT_BYTES);
        s.cap();
        assert!(!s.truncated && s.rows.len() == 260, "꼭 맞으면 그대로");
        let (_d, r) = open_results();
        for d in 0..40 {
            let mut s = exact_snapshot(SNAPSHOT_BYTES + d);
            s.id = new_id(add(GoTime::now(), TimeDelta::seconds(d as i64)));
            r.write(s.clone()).unwrap_or_else(|e| panic!("+{d}바이트: {e}"));
            let got = r.read(&s.id).unwrap();
            assert_eq!(d > 0, got.truncated, "+{d}바이트 truncated");
            assert_eq!(d == 0, got.rows.len() == 260, "+{d}바이트 rows");
        }
    }

    // ─── Go 판과 바꿔 쓰기 ───

    #[test]
    fn go_시각_json_은_rfc3339nano_와_같다() {
        let kst = FixedOffset::east_opt(9 * 3600).unwrap();
        let cases = [
            (GoTime::default(), "0001-01-01T00:00:00Z"),
            (
                kst.with_ymd_and_hms(2026, 11, 16, 10, 15, 30).unwrap().into(),
                "2026-11-16T10:15:30+09:00",
            ),
            (
                GoTime(kst.with_ymd_and_hms(2026, 11, 16, 10, 15, 30).unwrap() + TimeDelta::milliseconds(120)),
                "2026-11-16T10:15:30.12+09:00",
            ),
            (
                GoTime(utc(2026, 11, 16, 1, 0, 0).0 + TimeDelta::nanoseconds(5)),
                "2026-11-16T01:00:00.000000005Z",
            ),
            (
                FixedOffset::west_opt(3 * 3600 + 30 * 60)
                    .unwrap()
                    .with_ymd_and_hms(2026, 1, 2, 3, 4, 5)
                    .unwrap()
                    .into(),
                "2026-01-02T03:04:05-03:30",
            ),
        ];
        for (t, want) in cases {
            assert_eq!(t.format(), want);
            assert_eq!(GoTime::parse(want), Some(t));
            assert_eq!(GoTime::parse(want).unwrap().0.offset(), t.0.offset(), "오프셋을 지킨다");
        }
        assert!(GoTime::default().is_zero());
        let t: GoTime = serde_json::from_str("null").unwrap();
        assert!(t.is_zero(), "null 은 0 시각");
    }

    /// Go 판 db 가 쓴 history.jsonl 세 줄이다(go1.23, internal/history.Store.Append).
    const GO_HISTORY: &str = concat!(
        r#"{"id":"20261116-011530-a3f9c1","time":"2026-11-16T10:15:30.12+09:00","conn":"app-stg","sql":"select * from t where a \u003c 1 \u0026\u0026 b \u003e 2 -- \u2028 한","elapsed":1500000,"rows":3,"result":true}"#,
        "\n",
        r#"{"time":"2026-11-16T01:00:00.000000005Z","conn":"log-prod","sql":"selec","elapsed":0,"rows":0,"err":"syntax error at \"selec\"\n\ttab"}"#,
        "\n",
        r#"{"time":"0001-01-01T00:00:00Z","conn":"zero","sql":"select 1","elapsed":0,"rows":0}"#,
        "\n",
    );

    #[test]
    fn go_판이_쓴_기록을_읽고_같은_바이트로_붙여_쓴다() {
        let dir = tempfile::tempdir().unwrap();
        let go = dir.path().join("go.jsonl");
        fs::write(&go, GO_HISTORY).unwrap();
        let got = Store::open(&go).unwrap().list(&filter());
        assert_eq!(got.len(), 3);
        assert_eq!(got[2].sql, "select * from t where a < 1 && b > 2 -- \u{2028} 한");
        assert_eq!(got[2].elapsed, 1_500_000);
        assert!(got[2].result && got[2].rows == 3 && got[2].id == "20261116-011530-a3f9c1");
        assert_eq!(got[1].err, "syntax error at \"selec\"\n\ttab");
        assert!(got[0].time.is_zero());

        let mine = dir.path().join("rs.jsonl");
        let s = Store::open(&mine).unwrap();
        got.into_iter().rev().for_each(|e| s.append(e).unwrap());
        assert_eq!(fs::read_to_string(&mine).unwrap(), GO_HISTORY);
    }

    /// Go 판이 쓴 사본 JSON(압축 전)과 그 .json.gz 바이트다(internal/history.Results.Write).
    const GO_SNAPSHOT: &str = r#"{"id":"20261116-011530-a3f9c1","time":"2026-11-16T10:15:30.12+09:00","conn":"app-stg","sql":"select a, b","cols":["a","b"],"types":["int4","text"],"rows":[["1","\u003cx\u003e"],["2","NULL"]],"elapsed":120000000,"row_count":2,"result_truncated":true}"#;
    const GO_SNAPSHOT_GZ: &[u8] = &[
        31, 139, 8, 0, 0, 0, 0, 0, 0, 255, 44, 142, 209, 78, 195, 48, 12, 69, 127, 101, 186, 175, 184, 200, 78, 217, 164, 229, 27, 38, 158,
        224, 169, 84, 83, 150, 25, 84, 41, 164, 165, 113, 196, 16, 226, 223, 81, 10, 126, 177, 116, 124, 172, 123, 191, 49, 93, 225, 225,
        216, 29, 68, 228, 208, 177, 200, 190, 231, 46, 244, 175, 199, 40, 32, 216, 244, 174, 255, 247, 78, 164, 147, 195, 147, 176, 151,
        189, 239, 249, 94, 220, 29, 31, 61, 51, 8, 113, 206, 25, 30, 97, 89, 186, 98, 111, 32, 148, 143, 4, 143, 162, 73, 163, 237, 2, 237,
        46, 155, 148, 10, 252, 128, 0, 194, 5, 35, 193, 190, 22, 221, 200, 148, 237, 161, 101, 233, 205, 26, 95, 231, 207, 134, 7, 180, 2,
        47, 149, 185, 143, 183, 109, 41, 70, 26, 224, 64, 120, 124, 62, 157, 48, 142, 4, 77, 97, 41, 122, 133, 23, 199, 127, 179, 253, 159,
        227, 92, 179, 193, 59, 194, 170, 165, 38, 59, 219, 90, 115, 12, 214, 84, 91, 171, 254, 252, 6, 0, 0, 255, 255, 193, 122, 116, 169,
        249, 0, 0, 0,
    ];

    #[test]
    fn go_판이_쓴_사본을_읽고_같은_json_을_쓴다() {
        let (_d, r) = open_results();
        let id = "20261116-011530-a3f9c1";
        fs::write(r.path(id), GO_SNAPSHOT_GZ).unwrap();
        let s = Results::open(&r.dir).unwrap().read(id).unwrap();
        assert_eq!(s.rows, vec![vec!["1".to_string(), "<x>".into()], vec!["2".into(), "NULL".into()]]);
        assert!(s.result_truncated && !s.truncated && s.elapsed == 120_000_000 && s.row_count == 2);
        assert_eq!(s.time.format(), "2026-11-16T10:15:30.12+09:00");
        assert_eq!(String::from_utf8(go_json(&s, true, false).unwrap()).unwrap(), GO_SNAPSHOT);
    }
}
