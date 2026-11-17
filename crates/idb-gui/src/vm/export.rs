//! 전체 내보내기(R33.1)와 복호화 보기(R37)의 화면 쪽 규칙이다.

use std::collections::HashMap;

use idb_core::sqlsafe::quote_ident;
use idb_core::statcrypt::looks_encrypted;

/// 테이블 보기의 전체 내보내기 문장이다(WHERE·정렬 그대로, 행 제한 없음).
pub fn table_sql(schema: &str, table: &str, filter: &str, order: &str) -> String {
    let mut s = format!("SELECT * FROM {}.{}", quote_ident(schema), quote_ident(table));
    if !filter.trim().is_empty() {
        s.push_str(&format!(" WHERE {}", filter.trim()));
    }
    if !order.trim().is_empty() {
        s.push_str(&format!(" ORDER BY {}", order.trim()));
    }
    s
}

pub fn default_name(conn: &str, now: chrono::DateTime<chrono::Local>) -> String {
    let c = idb_core::mapping::path_safe(conn);
    format!("{}-{}.csv", if c.is_empty() { "export" } else { &c }, now.format("%Y%m%d-%H%M%S"))
}

/// 진행 글이다(받은 바이트, 끝나면 행 수).
pub fn progress(bytes: u64, rows: Option<i64>) -> String {
    let size = if bytes >= 1 << 30 {
        format!("{:.1}GiB", bytes as f64 / (1u64 << 30) as f64)
    } else if bytes >= 1 << 20 {
        format!("{:.1}MiB", bytes as f64 / (1u64 << 20) as f64)
    } else if bytes >= 1 << 10 {
        format!("{:.1}KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes}B")
    };
    match rows {
        Some(n) => format!("{n}행 · {size}"),
        None => format!("{size} 받는 중"),
    }
}

/// 내보내기 파일. 같은 폴더의 0600 임시 파일에 쓰고 끝나면 바꿔 끼운다. 그래서 있던 0644 파일을 덮어도 0600 이 되고,
/// 중간에 멈추거나 실패하면(버리면) 임시 파일을 지워 반쪽 파일이 남지 않는다.
pub struct PrivateFile {
    tmp: std::path::PathBuf,
    dest: std::path::PathBuf,
    file: Option<std::fs::File>,
}

impl PrivateFile {
    pub fn create(dest: &std::path::Path) -> std::io::Result<PrivateFile> {
        use std::os::unix::fs::OpenOptionsExt as _;
        let dir = dest
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .unwrap_or(std::path::Path::new("."));
        let name = dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let tmp = dir.join(format!(".{name}.part-{}-{nanos}", std::process::id()));
        let file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        Ok(PrivateFile {
            tmp,
            dest: dest.to_path_buf(),
            file: Some(file),
        })
    }

    pub fn write(&mut self, data: &[u8]) -> std::io::Result<()> {
        match &mut self.file {
            Some(f) => std::io::Write::write_all(f, data),
            None => Err(std::io::Error::other("export file is closed")),
        }
    }

    pub fn commit(mut self) -> std::io::Result<()> {
        if let Some(f) = self.file.take() {
            f.sync_all()?;
        }
        std::fs::rename(&self.tmp, &self.dest)
    }
}

impl Drop for PrivateFile {
    fn drop(&mut self) {
        // commit 하지 않고 버렸다(실패·멈춤)
        if self.file.is_some() || self.tmp.exists() {
            let _ = std::fs::remove_file(&self.tmp);
        }
    }
}

/// 내보내기 id 를 받은 뒤의 일을 돌리고, 실패하면 serve 의 내보내기를 멈춘다(COPY 가 연결을 붙잡지 않게).
pub async fn cancel_on_error<T, E, W, C>(work: W, cancel: C) -> Result<T, E>
where
    W: std::future::Future<Output = Result<T, E>>,
    C: std::future::Future<Output = ()>,
{
    let r = work.await;
    if r.is_err() {
        cancel.await;
    }
    r
}

/// 복호화할 칸이다. 암호문 꼴만, 같은 글은 한 번, 상한까지.
pub fn cipher_cells<'a>(cells: impl Iterator<Item = &'a str>, max: usize) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for c in cells {
        if out.len() >= max {
            break;
        }
        if looks_encrypted(c) && seen.insert(c) {
            out.push(c.to_string());
        }
    }
    out
}

/// 보낸 칸과 받은 값으로 암호문 → 평문 표를 만든다. 풀지 못한 칸(None)은 넣지 않는다.
pub fn overlay(cells: &[String], values: &[Option<String>]) -> HashMap<String, String> {
    cells
        .iter()
        .zip(values)
        .filter_map(|(c, v)| v.as_ref().map(|v| (c.clone(), v.clone())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENC: &str = "000102030405060708090a0b0c0d0e0f^|^00112233445566778899aabbccddeeff";

    #[test]
    fn 테이블_내보내기_문장은_WHERE_정렬을_그대로_싣는다() {
        assert_eq!(table_sql("public", "users", "", ""), "SELECT * FROM \"public\".\"users\"");
        assert_eq!(
            table_sql("public", "users", " id > 1 ", "id desc"),
            "SELECT * FROM \"public\".\"users\" WHERE id > 1 ORDER BY id desc"
        );
    }

    #[test]
    fn 진행_글은_크기_단위와_행_수를_보인다() {
        assert_eq!(progress(512, None), "512B 받는 중");
        assert_eq!(progress(2048, None), "2.0KiB 받는 중");
        assert_eq!(progress(3 << 20, Some(10)), "10행 · 3.0MiB");
    }

    #[test]
    fn 파일_이름은_접속과_시각이다() {
        let now = chrono::Local::now();
        let n = default_name("app/prod", now);
        assert!(n.starts_with("app-prod-") && n.ends_with(".csv"), "{n}");
    }

    #[test]
    fn 암호문_꼴_칸만_한_번씩_고른다() {
        let cells = ["plain", ENC, ENC, "NULL"];
        assert_eq!(cipher_cells(cells.into_iter(), 10), vec![ENC.to_string()]);
        assert!(cipher_cells([ENC].into_iter(), 0).is_empty());
    }

    #[test]
    fn 내보내기는_있던_0644_파일을_0600_으로_바꿔_끼우고_실패하면_흔적이_없다() {
        use std::os::unix::fs::PermissionsExt as _;
        let d = tempfile::tempdir().unwrap();
        let dest = d.path().join("out.csv");
        std::fs::write(&dest, "old").unwrap();
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mut f = PrivateFile::create(&dest).unwrap();
        f.write(b"a,b\n").unwrap();
        f.commit().unwrap();
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "a,b\n");
        assert_eq!(std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777, 0o600);
        let mut f = PrivateFile::create(&dest).unwrap();
        f.write(b"half").unwrap();
        drop(f);
        assert_eq!(
            std::fs::read_to_string(&dest).unwrap(),
            "a,b\n",
            "실패하면 원래 파일을 건드리지 않는다"
        );
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 1, "임시 파일이 남지 않는다");
    }

    #[test]
    fn 내보내기_id_뒤에_실패하면_멈춤을_보낸다() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let cancelled = std::cell::Cell::new(0);
        let r: Result<u8, &str> = rt.block_on(cancel_on_error(async { Err("disk full") }, async {
            cancelled.set(cancelled.get() + 1)
        }));
        assert!(r.is_err());
        assert_eq!(cancelled.get(), 1);
        let r: Result<u8, &str> = rt.block_on(cancel_on_error(async { Ok(1) }, async { cancelled.set(cancelled.get() + 1) }));
        assert_eq!(r, Ok(1));
        assert_eq!(cancelled.get(), 1, "성공하면 멈추지 않는다");
    }

    #[test]
    fn 풀지_못한_칸은_덮지_않는다() {
        let cells = vec!["a".to_string(), "b".to_string()];
        let m = overlay(&cells, &[Some("A".into()), None]);
        assert_eq!(m.get("a").map(String::as_str), Some("A"));
        assert!(!m.contains_key("b"));
    }
}
