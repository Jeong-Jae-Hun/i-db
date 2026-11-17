//! 패닉 기록. GPUI 는 AppKit 콜백 안에서 패닉이 나면 풀지 못하고 abort 하므로, 죽기 전에 패닉 글·자리·
//! 백트레이스를 `~/Library/Logs/i-db/crash.log`(0600)에 남긴다. 쿼리 글·값·비밀번호는 싣지 않는다:
//! 패닉 글 안의 `…` 로 감싼 원문(문자열 자르기 패닉이 원문을 싣는다)은 지우고 길이를 자른다.

use std::io::Write as _;
use std::path::{Path, PathBuf};

const MAX_MESSAGE: usize = 400;

pub fn log_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
    Some(PathBuf::from(home).join("Library/Logs/i-db/crash.log"))
}

/// 패닉 글에서 백틱으로 감싼 원문을 지우고 길이를 자른다.
pub fn redact(msg: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for c in msg.chars() {
        if c == '`' {
            if !inside {
                out.push_str("`…");
            } else {
                out.push('`');
            }
            inside = !inside;
            continue;
        }
        if !inside {
            out.push(c);
        }
    }
    if inside {
        out.push('`');
    }
    if out.chars().count() > MAX_MESSAGE {
        out = out.chars().take(MAX_MESSAGE).collect::<String>() + "…";
    }
    out
}

pub fn report(when: &str, version: &str, message: &str, location: Option<&str>, backtrace: &str) -> String {
    format!(
        "=== i-db {version} panic {when} ===\nmessage: {}\nlocation: {}\nbacktrace:\n{backtrace}\n",
        redact(message),
        location.unwrap_or("?")
    )
}

/// 디렉터리를 만들고 0600 파일 끝에 붙인다.
pub fn append(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    f.write_all(text.as_bytes())
}

/// 패닉 훅을 건다. 기록하고 stderr 에도 쓴다. abort 는 GPUI·AppKit 쪽이 한다.
pub fn install() {
    std::panic::set_hook(Box::new(|info| {
        let payload = info.payload();
        let message = payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "(알 수 없는 패닉)".into());
        let location = info.location().map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()));
        let bt = std::backtrace::Backtrace::force_capture().to_string();
        let when = chrono::Local::now().format("%Y-%m-%d %H:%M:%S %z").to_string();
        let text = report(&when, env!("CARGO_PKG_VERSION"), &message, location.as_deref(), &bt);
        eprintln!("{text}");
        if let Some(p) = log_path() {
            let _ = append(&p, &text);
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 패닉_글의_원문은_지우고_길이를_자른다() {
        let m = "byte index 5 is not a char boundary; it is inside '가' (bytes 3..6) of `select password from users`";
        let r = redact(m);
        assert!(!r.contains("password"), "{r}");
        assert!(r.contains("not a char boundary"));
        assert!(redact("열린 `원문").ends_with("`…`"));
        assert!(redact(&"a".repeat(1000)).chars().count() <= MAX_MESSAGE + 1);
    }

    #[test]
    fn 기록은_자리와_백트레이스를_싣고_0600_파일에_붙인다() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Logs/i-db/crash.log");
        let text = report(
            "2026-11-17",
            "0.1.1",
            "index out of bounds: the len is 3 but the index is 5",
            Some("src/x.rs:1:2"),
            "0: main",
        );
        append(&path, &text).unwrap();
        append(&path, &text).unwrap();
        let got = std::fs::read_to_string(&path).unwrap();
        assert_eq!(got.matches("=== i-db 0.1.1 panic").count(), 2);
        assert!(got.contains("location: src/x.rs:1:2") && got.contains("0: main"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }
}
