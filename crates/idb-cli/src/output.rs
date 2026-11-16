//! CLI 결과 출력(json·tsv·csv·table). Go `writeResults` 와 바이트가 같다.

use idb_core::gojson;
use idb_core::result::{NULL_TEXT, QueryResult, is_numeric_type};
use idb_core::textutil::{printable, string_width, truncate};

use crate::cli::{EXIT_ERR, EXIT_OK, Opts};

/// table 형식에서 칸 하나가 차지하는 최대 폭. 긴 JSON 한 칸이 줄 전체를 밀어내지 않게 자른다
const CLI_CELL_WIDTH: usize = 60;
/// 표 칸 하나에 담는 최대 바이트(E63)
pub const MAX_CELL_BYTES: usize = 64 << 10;

/// 출력할 결과 하나다. user360 처럼 여러 개면 title 로 나눈다.
#[derive(Default)]
pub struct CliResult {
    pub title: String,
    pub conn: String,
    pub res: Option<QueryResult>,
    pub err: Option<String>,
    /// --decrypt 로 풀지 못한 칸 수
    pub decrypt_failed: usize,
    /// 결과는 찍되 일부가 실패해 1 로 끝낼 때다
    pub failed: bool,
}

/// MAX_CELL_BYTES 에서 잘린 칸인지다. 잘린 칸은 끝에 `…(N바이트)`·`…(잘림)` 이 붙는다.
pub fn is_capped_cell(s: &str) -> bool {
    if s.len() <= MAX_CELL_BYTES {
        return false;
    }
    let Some(rest) = s.strip_suffix(')') else { return false };
    let Some(i) = rest.rfind("…(") else { return false };
    let inner = &rest[i + "…(".len()..];
    inner == "잘림"
        || inner
            .strip_suffix("바이트")
            .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
}

fn capped_cells(res: &QueryResult) -> usize {
    res.rows.iter().flatten().filter(|v| is_capped_cell(v)).count()
}

fn json_of(r: &CliResult, out: &mut String) {
    out.push('{');
    if !r.title.is_empty() {
        out.push_str("\"title\":");
        gojson::write_str(out, &r.title);
        out.push(',');
    }
    out.push_str("\"conn\":");
    gojson::write_str(out, &r.conn);
    if let Some(err) = &r.err {
        out.push_str(",\"error\":");
        gojson::write_str(out, err);
        out.push('}');
        return;
    }
    let res = r.res.as_ref().expect("result without error");
    let ms = res.elapsed.as_millis();
    if res.cols.is_empty() {
        out.push_str(",\"text\":");
        gojson::write_str(out, &res.text);
        out.push_str(&format!(",\"elapsed_ms\":{ms}}}"));
        return;
    }
    out.push_str(",\"cols\":");
    gojson::str_array(out, res.cols.iter().map(String::as_str));
    out.push_str(",\"types\":");
    gojson::str_array(out, res.types.iter().flatten().map(String::as_str));
    out.push_str(",\"rows\":[");
    for (i, row) in res.rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('[');
        for (j, v) in row.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            // PostgreSQL 결과(타입이 있음)의 "NULL" 칸은 null 이다
            if res.types.is_some() && v == NULL_TEXT {
                out.push_str("null");
            } else {
                gojson::write_str(out, v);
            }
        }
        out.push(']');
    }
    out.push(']');
    if !res.text.is_empty() {
        out.push_str(",\"text\":");
        gojson::write_str(out, &res.text);
    }
    out.push_str(&format!(",\"truncated\":{}", res.truncated));
    let capped = capped_cells(res);
    if capped > 0 {
        out.push_str(&format!(",\"cells_truncated\":{capped}"));
    }
    if r.decrypt_failed > 0 {
        out.push_str(&format!(",\"decrypt_failed\":{}", r.decrypt_failed));
    }
    out.push_str(&format!(",\"elapsed_ms\":{ms}}}"));
}

/// 결과들을 형식대로 쓰고 종료 코드를 돌려준다. 결과의 실패 표시가 있으면 1 이다.
pub fn write_results(w: &mut String, stderr: &mut String, o: &Opts, results: Vec<CliResult>) -> i32 {
    let multi = o.cmd == "user360";
    let failed = results.iter().any(|r| r.err.is_some() || r.failed || r.decrypt_failed > 0);
    // 잘린 칸은 형식과 상관없이 stderr 로 알린다. 칸 끝의 …(N바이트) 를 값으로 오해하지 않게 한다
    for r in &results {
        let (None, Some(res)) = (&r.err, &r.res) else { continue };
        let n = capped_cells(res);
        if n > 0 {
            let prefix = if r.title.is_empty() {
                String::new()
            } else {
                format!("{}: ", printable(&r.title, false))
            };
            stderr.push_str(&format!("{prefix}칸 {n}개가 64KiB 에서 잘림\n"));
        }
    }
    if o.format == "json" {
        if multi {
            w.push('[');
            for (i, r) in results.iter().enumerate() {
                if i > 0 {
                    w.push(',');
                }
                json_of(r, w);
            }
            w.push(']');
        } else {
            json_of(&results[0], w);
        }
        w.push('\n');
        return if failed { EXIT_ERR } else { EXIT_OK };
    }
    for (i, r) in results.iter().enumerate() {
        if multi {
            if i > 0 {
                w.push('\n');
            }
            w.push_str(&format!("== {} ({}) ==\n", printable(&r.title, false), r.conn));
        }
        if let Some(err) = &r.err {
            w.push_str(&format!("오류: {err}\n"));
            continue;
        }
        let res = r.res.as_ref().expect("result without error");
        if res.cols.is_empty() {
            let text = if o.format == "table" {
                printable(&res.text, true)
            } else {
                res.text.clone()
            };
            w.push_str(text.trim_end_matches('\n'));
            w.push('\n');
            continue;
        }
        match o.format.as_str() {
            "table" => write_table(w, res, o.write),
            "csv" => w.push_str(&to_csv(res)),
            _ => {
                w.push_str(&row_tsv(&res.cols));
                w.push('\n');
                for row in &res.rows {
                    w.push_str(&row_tsv(row));
                    w.push('\n');
                }
            }
        }
        if o.format != "table" {
            // 표에 딸린 글은 tsv·csv 를 깨지 않게 stderr 로 보낸다
            if !res.text.is_empty() {
                stderr.push_str(&res.text);
                stderr.push('\n');
            }
            if res.truncated {
                stderr.push_str(&format!("결과가 잘림: {}행까지 ({})\n", res.rows.len(), trunc_hint(o.write)));
            }
        }
    }
    if failed { EXIT_ERR } else { EXIT_OK }
}

/// 결과가 잘린 까닭이다. --write 는 1,000행에서 자르고 --max-rows 로 늘지 않는다.
fn trunc_hint(write: bool) -> &'static str {
    if write {
        "--write 는 1,000행 상한"
    } else {
        "--max-rows 로 늘림"
    }
}

/// 행 하나를 탭으로 이은 한 줄이다. 칸 안의 `\`·탭·줄바꿈은 이스케이프한다.
pub fn row_tsv(row: &[String]) -> String {
    row.iter()
        .map(|v| {
            let mut s = String::with_capacity(v.len());
            for c in v.chars() {
                match c {
                    '\\' => s.push_str("\\\\"),
                    '\t' => s.push_str("\\t"),
                    '\n' => s.push_str("\\n"),
                    '\r' => s.push_str("\\r"),
                    c => s.push(c),
                }
            }
            s
        })
        .collect::<Vec<_>>()
        .join("\t")
}

/// 결과를 CSV(RFC 4180, 줄 끝 LF)로 만든다. NULL 은 따옴표 없는 빈 칸, 빈 문자열은 "" 로 써서 둘을 가른다.
/// 열이 하나면 빈 줄이 레코드로 읽히지 않으므로 NULL 도 "" 로 쓴다.
pub fn to_csv(r: &QueryResult) -> String {
    let mut b = r.cols.iter().map(|c| csv_field(c)).collect::<Vec<_>>().join(",");
    b.push('\n');
    for row in &r.rows {
        for i in 0..r.cols.len() {
            if i > 0 {
                b.push(',');
            }
            let v = row.get(i).map(String::as_str).unwrap_or("");
            if v != NULL_TEXT {
                b.push_str(&csv_field(v));
            } else if r.cols.len() == 1 {
                b.push_str("\"\"");
            }
        }
        b.push('\n');
    }
    b
}

/// 칸 하나를 CSV 로 쓴다. = + - @ 탭·CR 로 시작하는 글은 스프레드시트 수식 실행을 막으려고 ' 를 붙인다(숫자는 그대로).
fn csv_field(s: &str) -> String {
    let mut s = s.to_string();
    if s.starts_with(['=', '+', '-', '@', '\t', '\r']) && !go_parses_float(&s) {
        s.insert(0, '\'');
    }
    if !s.is_empty() && !s.contains([',', '"', '\r', '\n']) && s.trim() == s {
        return s;
    }
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Go `strconv.ParseFloat(s, 64)` 가 받는지다(부호·소수·지수·inf·nan·16진 부동소수).
fn go_parses_float(s: &str) -> bool {
    let t = s.strip_prefix(['+', '-']).unwrap_or(s).to_ascii_lowercase();
    matches!(t.as_str(), "inf" | "infinity" | "nan")
        || (!t.starts_with("0x") && !t.contains('_') && s.parse::<f64>().is_ok() && !t.starts_with("inf") && !t.starts_with("nan"))
}

/// 열을 맞춘 글 표를 쓴다. 칸의 제어 문자는 보이는 글자로 바꾸고, 폭은 넓은 글자를 2칸으로 센다. 숫자 열은 오른쪽에 붙인다.
fn write_table(b: &mut String, res: &QueryResult, write: bool) {
    let cell = |s: &str| truncate(&printable(s, false), CLI_CELL_WIDTH, "…");
    let head: Vec<String> = res.cols.iter().map(|c| cell(c)).collect();
    let mut widths: Vec<usize> = head.iter().map(|h| string_width(h)).collect();
    let rows: Vec<Vec<String>> = res
        .rows
        .iter()
        .map(|row| {
            (0..res.cols.len())
                .map(|i| {
                    let v = row.get(i).map(|v| cell(v)).unwrap_or_default();
                    widths[i] = widths[i].max(string_width(&v));
                    v
                })
                .collect()
        })
        .collect();
    let types = res.types.as_deref().unwrap_or(&[]);
    write_table_line(b, &head, &widths, types);
    let sep: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
    write_table_line(b, &sep, &widths, types);
    for row in &rows {
        write_table_line(b, row, &widths, types);
    }
    b.push_str(&format!("({}행", res.rows.len()));
    if res.truncated {
        b.push_str(&format!(", 잘림 — {}", trunc_hint(write)));
    }
    b.push_str(")\n");
    if !res.text.is_empty() {
        b.push_str(&printable(&res.text, true));
        b.push('\n');
    }
}

/// 칸들을 widths 에 맞춰 한 줄로 쓴다. 숫자 타입 열은 오른쪽에 붙이고 마지막 칸 뒤에는 공백을 두지 않는다.
fn write_table_line(b: &mut String, cells: &[String], widths: &[usize], types: &[String]) {
    for (i, v) in cells.iter().enumerate() {
        if i > 0 {
            b.push_str("  ");
        }
        let pad = " ".repeat(widths[i] - string_width(v));
        if types.get(i).is_some_and(|t| is_numeric_type(t)) {
            b.push_str(&pad);
            b.push_str(v);
        } else if i == cells.len() - 1 {
            b.push_str(v);
        } else {
            b.push_str(v);
            b.push_str(&pad);
        }
    }
    b.push('\n');
}
