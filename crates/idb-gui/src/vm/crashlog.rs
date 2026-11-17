//! 패닉 기록. GPUI 는 AppKit 콜백 안에서 패닉이 나면 풀지 못하고 abort 하므로, 죽기 전에 패닉 종류·자리·
//! 백트레이스를 `~/Library/Logs/i-db/crash.log`(0600)에 남긴다. 패닉 글 원문은 싣지 않는다: Debug 로 찍힌 오류나
//! 문자열 자르기 패닉이 쿼리 글·값·비밀번호를 담을 수 있어서다. 정해 둔 종류 이름만 쓴다.

use std::io::Write as _;
use std::path::{Path, PathBuf};

pub fn log_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
    Some(PathBuf::from(home).join("Library/Logs/i-db/crash.log"))
}

/// 패닉 글을 정해 둔 종류로 줄인다. 엔티티 이중 대여는 어느 화면인지 알아야 고칠 수 있어 타입 경로만 덧붙인다
/// (러스트 식별자·`::` 만 이루어진 낱말일 때만).
pub fn category(msg: &str) -> String {
    if msg.starts_with("cannot update ") || msg.starts_with("cannot read ") {
        let ty = msg
            .split_whitespace()
            .nth(2)
            .filter(|t| t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':'));
        return match ty {
            Some(t) => format!("엔티티 이중 대여 ({t})"),
            None => "엔티티 이중 대여".into(),
        };
    }
    let kind = if msg.contains("already borrowed") || msg.contains("already mutably borrowed") {
        "RefCell 이중 대여"
    } else if msg.contains("index out of bounds") || msg.contains("out of range for slice") || msg.contains("range end index") {
        "범위 밖 접근"
    } else if msg.contains("char boundary") {
        "문자열 경계"
    } else if msg.starts_with("called `Option::unwrap()`") || msg.starts_with("called `Result::unwrap()`") {
        "unwrap 실패"
    } else if msg.starts_with("attempt to ") {
        "산술 넘침·0 나누기"
    } else {
        "패닉"
    };
    kind.into()
}

pub fn report(when: &str, version: &str, message: &str, location: Option<&str>, backtrace: &str) -> String {
    format!(
        "=== i-db {version} panic {when} ===\nkind: {}\nlocation: {}\nbacktrace:\n{backtrace}\n",
        category(message),
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
    fn 패닉_글은_종류로만_남기고_원문은_싣지_않는다() {
        let m = "byte index 5 is not a char boundary; it is inside '가' (bytes 3..6) of `select password from users`";
        assert_eq!(category(m), "문자열 경계");
        let m = "called `Result::unwrap()` on an `Err` value: Db { password: \"s3cret\" }";
        assert_eq!(category(m), "unwrap 실패");
        assert_eq!(category("index out of bounds: the len is 3 but the index is 5"), "범위 밖 접근");
        assert_eq!(
            category("cannot update idb_gui::ui::workspace::Workspace while it is already being updated"),
            "엔티티 이중 대여 (idb_gui::ui::workspace::Workspace)"
        );
        assert_eq!(category("cannot read `pw=x` while"), "엔티티 이중 대여");
        assert_eq!(category("token=abc expired"), "패닉");
        let r = report(
            "t",
            "0.1.1",
            "called `Result::unwrap()` on an `Err` value: s3cret",
            Some("a.rs:1:1"),
            "bt",
        );
        assert!(!r.contains("s3cret") && r.contains("kind: unwrap 실패"), "{r}");
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
        assert!(got.contains("location: src/x.rs:1:2") && got.contains("0: main") && got.contains("kind: 범위 밖 접근"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }
}
