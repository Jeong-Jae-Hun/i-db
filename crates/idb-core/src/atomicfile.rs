//! 같은 폴더 임시 파일에 다 쓰고 디스크에 내린 뒤 바꿔 끼우는 쓰기다(R5.2).
//! 쓰기·바꿔 끼우기가 실패하면 원래 파일이 그대로 남고 임시 파일은 지운다. Go `internal/atomicfile` 과 같다.
//!
//! 한계: 바꿔 끼운 뒤 폴더는 fsync 하지 않아 전원이 나가면 이름 바뀜이 사라질 수 있다(내용은 옛 판이나 새 판 중 하나).
//! 새 파일은 perm 만 맞추며 원래 파일의 소유자·그룹·setuid/setgid·확장 속성은 지키지 않는다.

use std::fs::{self, File, OpenOptions, Permissions, TryLockError};
use std::hash::{BuildHasher, RandomState};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// try_lock 이 잠금을 다시 시도하는 간격이다
const LOCK_POLL: Duration = Duration::from_millis(10);

/// try_lock 이 기다린 시간 안에 잠금을 얻지 못했을 때 오류 글의 끝이다. 오류 종류는 `TimedOut` 이다
pub const LOCK_TIMEOUT: &str = "lock wait timed out";

/// data 를 path 에 바꿔 끼워 쓴다. pattern 은 Go `os.CreateTemp` 의 임시 파일 이름 꼴(마지막 `*` 자리에 수), perm 은 새 파일 권한이다.
pub fn write(path: &Path, pattern: &str, data: &[u8], perm: u32) -> io::Result<()> {
    let (tmp, mut f) = create_temp(path.parent().unwrap_or(Path::new(".")), pattern)?;
    let w = f.write_all(data);
    let s = f.sync_all();
    drop(f);
    let c = fs::set_permissions(&tmp, Permissions::from_mode(perm));
    if let Err(e) = join([w, s, c]) {
        return Err(join_err(e, fs::remove_file(&tmp)));
    }
    if let Err(e) = fs::rename(&tmp, path) {
        return Err(join_err(e, fs::remove_file(&tmp)));
    }
    Ok(())
}

/// path 옆 .lock 파일에 배타 flock 을 건다. 돌려준 파일을 닫으면(drop) 풀린다. 다시 읽고 합쳐 바꿔 끼우는 쓰기를
/// 프로세스끼리 줄 세운다. 바꿔 끼우는 파일 자체에 걸면 잠금이 옛 inode 에 남으므로 따로 둔다.
/// std 의 `File::lock` 은 linux·darwin 에서 flock 이라 Go 판(syscall.Flock)과 같은 잠금을 나눈다.
pub fn lock(path: &Path) -> io::Result<File> {
    let f = open_lock(path)?;
    f.lock().map_err(|e| lock_err(path, e.kind(), e))?;
    Ok(f)
}

/// lock 과 같되 wait 동안만 기다린다. 잠금을 쥔 프로세스가 멈춰도 부른 쪽이 끝없이 서지 않게 한다.
/// 시간이 다 되면 `TimedOut` 종류의 `lock <이름>: lock wait timed out` 오류다.
pub fn try_lock(path: &Path, wait: Duration) -> io::Result<File> {
    let f = open_lock(path)?;
    let deadline = Instant::now() + wait;
    loop {
        match f.try_lock() {
            Ok(()) => return Ok(f),
            Err(TryLockError::Error(e)) => return Err(lock_err(path, e.kind(), e)),
            Err(TryLockError::WouldBlock) if Instant::now() > deadline => {
                return Err(lock_err(path, io::ErrorKind::TimedOut, LOCK_TIMEOUT));
            }
            Err(TryLockError::WouldBlock) => thread::sleep(LOCK_POLL),
        }
    }
}

fn open_lock(path: &Path) -> io::Result<File> {
    let mut p = path.as_os_str().to_owned();
    p.push(".lock");
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(p)
}

fn lock_err(path: &Path, kind: io::ErrorKind, e: impl std::fmt::Display) -> io::Error {
    let base = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
    io::Error::new(kind, format!("lock {base}: {e}"))
}

// ─── 임시 파일 ───

/// Go `os.CreateTemp` 처럼 pattern 의 마지막 `*` 를 무작위 수로 바꾼 새 파일(0600)을 만든다. 겹치면 다시 고른다.
fn create_temp(dir: &Path, pattern: &str) -> io::Result<(PathBuf, File)> {
    let (prefix, suffix) = pattern.rsplit_once('*').unwrap_or((pattern, ""));
    let mut tries = 0;
    loop {
        let name = dir.join(format!("{prefix}{}{suffix}", random_u64() as u32));
        match OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(&name) {
            Ok(f) => return Ok((name, f)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && tries < 10000 => tries += 1,
            Err(e) => return Err(e),
        }
    }
}

/// 이름을 겹치지 않게 할 무작위 수다. 암호용이 아니다. RandomState 의 씨앗은 OS 난수에서 온다
pub(crate) fn random_u64() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    RandomState::new().hash_one(COUNTER.fetch_add(1, Ordering::Relaxed))
}

// ─── 오류 잇기 ───

/// Go `errors.Join` 처럼 실패만 모아 줄바꿈으로 잇는다. 오류 종류는 첫 실패의 것이다.
pub(crate) fn join<I: IntoIterator<Item = io::Result<()>>>(results: I) -> io::Result<()> {
    let mut acc = Ok(());
    for r in results {
        acc = match acc {
            Ok(()) => r,
            Err(e) => Err(join_err(e, r)),
        };
    }
    acc
}

pub(crate) fn join_err(e: io::Error, more: io::Result<()>) -> io::Error {
    match more {
        Ok(()) => e,
        Err(m) => io::Error::new(e.kind(), format!("{e}\n{m}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 권한대로_바꿔_끼우고_실패하면_원래_것과_임시_파일을_남기지_않는다() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.json");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, Permissions::from_mode(0o644)).unwrap();
        write(&path, ".a-*", b"new", 0o600).unwrap();
        let st = fs::metadata(&path).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(st.permissions().mode() & 0o777, 0o600);
        // 폴더 자리에는 바꿔 끼울 수 없다
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        assert!(write(&sub, ".a-*", b"x", 0o600).is_err(), "폴더 자리 쓰기는 실패해야 한다");
        assert!(fs::metadata(&sub).unwrap().is_dir(), "원래 것이 남아야 한다");
        let left: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".a-"))
            .collect();
        assert!(left.is_empty(), "임시 파일이 남았다: {left:?}");
    }

    #[test]
    fn 다른_쪽이_쥐고_있으면_기다린_시간_뒤_포기하고_풀리면_얻는다() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.json");
        let held = lock(&path).unwrap();
        let start = Instant::now();
        let err = try_lock(&path, Duration::from_millis(50)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert_eq!(err.to_string(), "lock a.json: lock wait timed out");
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "정해진 시간만 기다린다: {:?}",
            start.elapsed()
        );
        drop(held);
        try_lock(&path, Duration::from_millis(50)).expect("풀리면 얻는다");
    }

    #[test]
    fn 임시_파일_이름은_go_처럼_별_자리에_수를_넣는다() {
        let dir = tempfile::tempdir().unwrap();
        let (p, _f) = create_temp(dir.path(), ".history-*.jsonl").unwrap();
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        let mid = name.strip_prefix(".history-").and_then(|s| s.strip_suffix(".jsonl")).unwrap();
        assert!(!mid.is_empty() && mid.bytes().all(|b| b.is_ascii_digit()), "{name}");
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
