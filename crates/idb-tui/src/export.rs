//! 전체 내보내기(R33.1 F7), 복호화 보기(R37 D), CSV 가져오기(R19 트리 I).
//! 내보내기 조각과 CSV 파일은 이 기계에서 읽고 쓴다(원격이어도 serve 쪽에 파일을 만들지 않는다). 복호화는 serve 가 그룹 키로 한다.

use std::collections::HashMap;
use std::io::Write;

use idb_engine::{Decrypted, EngineError, ExportChunk, ExportStarted, Request, TableRef};
use serde_json::Value;

use crate::app::{App, Overlay, Pend};
use crate::grid::Screen;
use crate::tree::NodeKind;

/// 진행 중인 전체 내보내기다. id 는 serve 의 내보내기 번호(받기 전이면 None)다.
pub struct Exporting {
    pub conn: usize,
    pub export: Option<u64>,
    pub job: u64,
    pub path: String,
    pub file: std::fs::File,
    pub bytes: u64,
}

fn decode<T: serde::de::DeserializeOwned>(v: Value) -> Result<T, EngineError> {
    serde_json::from_value(v).map_err(|e| EngineError::run(format!("bad result: {e}")))
}

/// CSV 를 칸으로 읽는다(RFC 4180: 따옴표 칸 안의 쉼표·줄바꿈은 값, 따옴표 두 개는 따옴표). 빈 줄은 건너뛴다.
pub fn parse_csv(s: &str) -> Vec<Vec<String>> {
    let s = s.strip_prefix('\u{feff}').unwrap_or(s);
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut cur = String::new();
    let (mut quoted, mut started) = (false, false);
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if quoted {
            match c {
                '"' if it.peek() == Some(&'"') => {
                    it.next();
                    cur.push('"');
                }
                '"' => quoted = false,
                c => cur.push(c),
            }
            continue;
        }
        match c {
            '"' if cur.is_empty() => {
                quoted = true;
                started = true;
            }
            ',' => {
                row.push(std::mem::take(&mut cur));
                started = true;
            }
            '\r' => {}
            '\n' => {
                if started || !cur.is_empty() {
                    row.push(std::mem::take(&mut cur));
                    rows.push(std::mem::take(&mut row));
                }
                started = false;
            }
            c => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started || !cur.is_empty() {
        row.push(cur);
        rows.push(row);
    }
    rows
}

impl App {
    // ─── 전체 내보내기 (R33.1) ───

    /// 지금 보이는 표 결과를 만든 성공한 문장을 COPY 로 1,000행 제한 없이 파일에 쓴다.
    pub(crate) fn full_export(&mut self) {
        if self.exporting.is_some() {
            self.status = "전체 내보내기 중 (esc 취소)".into();
            return;
        }
        let t = self.cur();
        let why = if t.sub.is_some() || t.snapshot {
            "고정·문장별 결과는 전체 내보내지 않음"
        } else if !t.err.is_empty() || t.export_sql.is_empty() || t.grid.is_none() {
            "전체 내보낼 표 결과가 없음 (성공한 SELECT 문 결과에서)"
        } else if t.result_conn.is_none_or(|c| !self.is_pg(c)) {
            "PostgreSQL 결과만 전체 내보내기"
        } else if t.result_conn != t.conn {
            "보이는 결과는 다른 접속의 것이라 막음 (다시 실행하거나 새로 여세요)"
        } else {
            ""
        };
        if !why.is_empty() {
            self.status = why.into();
            return;
        }
        let (conn, sql, title) = (t.result_conn.unwrap(), t.export_sql.clone(), t.display_name());
        let (path, file) = match crate::files::create_export(&title, "csv") {
            Ok(x) => x,
            Err(e) => {
                self.status = format!("전체 내보내기 실패: {e}");
                return;
            }
        };
        let name = self.conns[conn].name.clone();
        let job = self.call(Request::ExportStart { conn: name, sql }, Pend::ExportStart);
        self.exporting = Some(Exporting {
            conn,
            export: None,
            job,
            path,
            file,
            bytes: 0,
        });
        self.status = "전체 내보내기 시작… (esc 취소)".into();
    }

    pub(crate) fn export_started(&mut self, res: Result<Value, EngineError>) {
        match res.and_then(decode::<ExportStarted>) {
            Ok(s) => {
                let Some(x) = &mut self.exporting else { return };
                x.export = Some(s.export);
                self.export_next();
            }
            Err(e) => self.export_fail(&e.error),
        }
    }

    fn export_next(&mut self) {
        let Some(x) = &self.exporting else { return };
        let Some(id) = x.export else { return };
        let job = self.call(Request::ExportRead { export: id }, Pend::ExportRead);
        if let Some(x) = &mut self.exporting {
            x.job = job;
        }
    }

    pub(crate) fn export_read(&mut self, res: Result<Value, EngineError>) {
        let chunk = match res.and_then(decode::<ExportChunk>) {
            Ok(c) => c,
            Err(e) => return self.export_fail(&e.error),
        };
        let Some(x) = &mut self.exporting else { return };
        if let Err(e) = x.file.write_all(&chunk.data) {
            let msg = e.to_string();
            return self.export_fail(&msg);
        }
        x.bytes = chunk.bytes;
        if chunk.done {
            let x = self.exporting.take().unwrap();
            let _ = x.file.sync_all();
            self.status = format!("{}행 내보냄: {}", idb_core::textutil::group_digits(chunk.rows), x.path);
            return;
        }
        self.status = format!("전체 내보내기 {} 받음 (esc 취소)", human_bytes(x.bytes));
        self.export_next();
    }

    fn export_fail(&mut self, err: &str) {
        if let Some(x) = self.exporting.take() {
            self.pending.remove(&x.job);
            let _ = std::fs::remove_file(&x.path);
        }
        self.status = format!("전체 내보내기 실패: {}", err.replace('\n', " "));
    }

    /// esc: 내보내기를 멈추고 쓰던 파일을 지운다. 취소는 알리지 않는다(R30).
    pub(crate) fn export_cancel(&mut self) {
        let Some(x) = self.exporting.take() else { return };
        self.cancel(x.job);
        self.pending.remove(&x.job);
        if let Some(id) = x.export {
            self.call(Request::ExportCancel { export: id }, Pend::Ignore);
        }
        let _ = std::fs::remove_file(&x.path);
        self.status = "전체 내보내기 취소함".into();
    }

    // ─── 복호화 보기 (R37) ───

    /// D: 암호문 꼴 칸을 serve 에서 풀어 보이고, 다시 D 면 원래 값이다.
    pub(crate) fn toggle_decrypt(&mut self) {
        let ti = self.tab;
        let conn = self.tabs[ti].result_conn;
        let tab = self.tabs[ti].id;
        let Some(g) = self.tabs[ti].shown_mut() else { return };
        if g.dec.take().is_some() {
            self.status = "원래 값".into();
            return;
        }
        if g.pending() > 0 {
            self.status = "변경을 먼저 제출하거나 되돌리세요".into();
            return;
        }
        let cells = g.enc_cells();
        if cells.is_empty() {
            self.status = "복호화할 칸이 없음 (<hex>^|^<hex> 꼴)".into();
            return;
        }
        let Some(conn) = conn else { return };
        let req = Request::Decrypt {
            conn: self.conns[conn].name.clone(),
            cells: cells.iter().map(|c| c.2.clone()).collect(),
        };
        let at = cells.into_iter().map(|(i, c, _)| (i, c)).collect();
        self.call(req, Pend::Decrypt { tab, at });
        self.status = "복호화하는 중…".into();
    }

    pub(crate) fn decrypted(&mut self, tab: u64, at: Vec<(usize, usize)>, res: Result<Value, EngineError>) {
        let d = match res.and_then(decode::<Decrypted>) {
            Ok(d) => d,
            Err(e) => {
                self.status = e.error;
                return;
            }
        };
        let Some(t) = self.tab_by_id(tab) else { return };
        let Some(g) = t.shown_mut() else { return };
        let map: HashMap<(usize, usize), String> = at.into_iter().zip(d.values).filter_map(|(k, v)| v.map(|v| (k, v))).collect();
        let n = map.len();
        if g.screen == Screen::Edit {
            g.screen = Screen::Table;
        }
        g.dec = Some(map);
        self.status = if d.failed > 0 {
            format!("복호화 {n}칸 · {}칸 풀지 못함", d.failed)
        } else {
            format!("복호화 {n}칸 (D 로 원래 값)")
        };
    }

    // ─── CSV 가져오기 (R19) ───

    /// 트리 테이블 줄 I: CSV 경로를 받아 열을 맞추고 앞 5행을 미리 보인 뒤 묻는다.
    pub(crate) fn csv_import(&mut self, n: usize) {
        let NodeKind::Table { conn, schema, name } = self.tree.node(n).kind.clone() else {
            return;
        };
        let Some(cols) = self.catalogs.get(&conn).and_then(|c| {
            c.schemas
                .iter()
                .find(|s| s.name == schema)
                .and_then(|s| s.tables.iter().find(|t| t.name == name))
                .map(|t| t.columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>())
        }) else {
            self.ensure_catalog(conn);
            self.status = "열 목록을 읽는 중 — 잠시 뒤 다시 I".into();
            return;
        };
        let base = if self.files.is_some() {
            format!("{}/", self.dir)
        } else {
            String::new()
        };
        let table = TableRef { schema, table: name };
        self.prompt("CSV 경로: ", &base, move |a, path| {
            a.csv_preview(conn, table, cols, path.trim().to_string())
        });
    }

    /// 파일 읽기·풀기는 화면 밖 스레드에서 한다. 큰 파일이 화면을 멈추지 않게 하려는 것이다.
    fn csv_preview(&mut self, conn: usize, table: TableRef, cols: Vec<String>, path: String) {
        let path = match path.strip_prefix("~/") {
            Some(rest) => format!("{}/{rest}", std::env::var("HOME").unwrap_or_default()),
            None => path,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let p = path.clone();
        std::thread::spawn(move || {
            let _ = tx.send(prepare_csv(&p, &cols));
        });
        self.csv_job = Some(CsvJob { rx, conn, table, path });
        self.status = "CSV 를 읽는 중…".into();
    }

    /// 화면 밖에서 준비한 CSV 가 오면 미리 보기와 확인 창을 연다. Runner 가 돌 때마다 부른다.
    pub fn poll_bg(&mut self) {
        let Some(job) = &self.csv_job else { return };
        let prep = match job.rx.try_recv() {
            Ok(p) => p,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Err("CSV 를 읽지 못함".into()),
        };
        let CsvJob { conn, table, path, .. } = self.csv_job.take().unwrap();
        let prep = match prep {
            Ok(p) => p,
            Err(e) => {
                self.status = e;
                return;
            }
        };
        let c = &self.conns[conn];
        let n = prep.data.len();
        let q = if c.env == "prod" {
            format!("prod({}) {} 에 {n}행 가져오기", c.name, table.quoted())
        } else {
            format!("{} 에 {n}행 가져오기", table.quoted())
        };
        let prod = c.env == "prod";
        let name = c.name.clone();
        let CsvPrep { columns, data, preview } = prep;
        self.overlay = Overlay::Confirm {
            q,
            then: Box::new(move |a| {
                let req = Request::ImportCsv {
                    conn: name,
                    schema: table.schema.clone(),
                    table: table.table.clone(),
                    columns,
                    rows: data,
                    confirm_prod: prod,
                };
                a.call(req, Pend::Import { conn, table, file: path });
                a.status = "가져오는 중…".into();
            }),
            preview,
            top: 0,
        };
    }

    pub(crate) fn imported(&mut self, conn: usize, table: TableRef, file: String, res: Result<Value, EngineError>) {
        match res {
            Ok(v) => {
                let n = v.get("rows").and_then(Value::as_i64).unwrap_or(0);
                let note = format!("-- CSV 가져오기 {file} → {} ({n}행)", table.quoted());
                self.record(conn, &note, 0, None, "");
                self.status = format!("{}행 가져옴", idb_core::textutil::group_digits(n));
            }
            Err(e) => self.status = format!("가져오지 못함: {}", e.error.replace('\n', " ")),
        }
    }
}

/// serve 가 한 번에 받는 가져오기 상한과 같다(idb-sources import_csv: 10만 행·50MiB). 넘는 파일은 읽기 전에 막는다
pub const MAX_IMPORT_BYTES: u64 = 50 << 20;
pub const MAX_IMPORT_ROWS: usize = 100_000;

/// 화면 밖 스레드의 CSV 준비 일감이다.
pub struct CsvJob {
    pub rx: std::sync::mpsc::Receiver<Result<CsvPrep, String>>,
    pub conn: usize,
    pub table: TableRef,
    pub path: String,
}

#[derive(Debug)]
pub struct CsvPrep {
    pub columns: Vec<String>,
    pub data: Vec<Vec<Option<String>>>,
    pub preview: Vec<String>,
}

/// CSV 를 읽어 테이블 열에 맞춘다(R19.1). 일반 파일만, 상한 안에서만 읽는다(`/dev/zero`·폴더·큰 파일이 메모리를 채우지 않게).
pub fn prepare_csv(path: &str, cols: &[String]) -> Result<CsvPrep, String> {
    use std::io::Read;
    let meta = std::fs::metadata(path).map_err(|e| format!("CSV 를 읽지 못함: {e}"))?;
    if !meta.is_file() {
        return Err("CSV 를 읽지 못함: 일반 파일이 아님".into());
    }
    if meta.len() > MAX_IMPORT_BYTES {
        return Err(format!("CSV 가 {}MiB 를 넘어 가져오지 않음", MAX_IMPORT_BYTES >> 20));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|f| f.take(MAX_IMPORT_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|e| format!("CSV 를 읽지 못함: {e}"))?;
    if bytes.len() as u64 > MAX_IMPORT_BYTES {
        return Err(format!("CSV 가 {}MiB 를 넘어 가져오지 않음", MAX_IMPORT_BYTES >> 20));
    }
    let mut rows = parse_csv(&String::from_utf8_lossy(&bytes));
    drop(bytes);
    if rows.is_empty() {
        return Err("CSV 가 비어 있음".into());
    }
    if rows.len() - 1 > MAX_IMPORT_ROWS {
        return Err(format!("CSV 가 {MAX_IMPORT_ROWS}행을 넘어 가져오지 않음"));
    }
    let head = rows.remove(0);
    // 머리 줄을 테이블 열 이름에 대소문자 없이 맞추고 맞지 않는 열은 버린다
    let map: Vec<(usize, String)> = head
        .iter()
        .enumerate()
        .filter_map(|(i, h)| cols.iter().find(|c| c.eq_ignore_ascii_case(h.trim())).map(|c| (i, c.clone())))
        .collect();
    if map.is_empty() {
        return Err("테이블 열과 맞는 CSV 열이 없음".into());
    }
    let columns: Vec<String> = map.iter().map(|m| m.1.clone()).collect();
    let data: Vec<Vec<Option<String>>> = rows
        .into_iter()
        .map(|mut r| {
            map.iter()
                .map(|(i, _)| r.get_mut(*i).filter(|v| !v.is_empty()).map(std::mem::take))
                .collect()
        })
        .collect();
    let mut preview = vec![format!(
        "열: {}",
        head.iter()
            .enumerate()
            .map(|(i, h)| match map.iter().find(|m| m.0 == i) {
                Some((_, c)) => format!("{h} → {c}"),
                None => format!("{h} (버림)"),
            })
            .collect::<Vec<_>>()
            .join(", ")
    )];
    for r in data.iter().take(5) {
        preview.push(
            r.iter()
                .map(|v| v.clone().unwrap_or_else(|| "NULL".into()))
                .collect::<Vec<_>>()
                .join(" | "),
        );
    }
    Ok(CsvPrep { columns, data, preview })
}

fn human_bytes(n: u64) -> String {
    match n {
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / (1 << 20) as f64),
        n if n >= 1 << 10 => format!("{:.1} KB", n as f64 / 1024.0),
        n => format!("{n} B"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn CSV_는_따옴표_칸의_쉼표와_줄바꿈을_값으로_읽는다() {
        assert_eq!(
            parse_csv("\u{feff}a,b\r\n\"x,1\",\"q\"\"\n2\"\n\n3,\n"),
            [vec!["a", "b"], vec!["x,1", "q\"\n2"], vec!["3", ""]]
        );
        assert_eq!(human_bytes(2 << 20), "2.0 MB");
    }

    #[test]
    fn CSV_는_일반_파일만_상한_안에서_읽는다() {
        let cols = vec!["id".to_string()];
        assert_eq!(prepare_csv("/dev/zero", &cols).unwrap_err(), "CSV 를 읽지 못함: 일반 파일이 아님");
        let d = tempfile::tempdir().unwrap();
        assert!(prepare_csv(d.path().to_str().unwrap(), &cols).is_err());
        let big = d.path().join("big.csv");
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(MAX_IMPORT_BYTES + 1).unwrap();
        assert_eq!(
            prepare_csv(big.to_str().unwrap(), &cols).unwrap_err(),
            "CSV 가 50MiB 를 넘어 가져오지 않음"
        );
        let ok = d.path().join("ok.csv");
        std::fs::write(&ok, "ID,x\n1,a\n").unwrap();
        let p = prepare_csv(ok.to_str().unwrap(), &cols).unwrap();
        assert_eq!((p.columns, p.data), (vec!["id".to_string()], vec![vec![Some("1".to_string())]]));
    }
}
