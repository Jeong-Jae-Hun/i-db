//! 결과 표. 원본 행은 그대로 두고 거르기·정렬은 보이는 순서(`view`)만 바꾼다. 그리는 쪽은 보이는 줄만
//! 묻는다(가상 스크롤). 칸 글은 엔진이 준 그대로이고 NULL 은 `"NULL"` 이다.

use std::cmp::Ordering;

use idb_engine::ResultSet;

pub const NULL: &str = "NULL";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Asc,
    Desc,
}

#[derive(Default)]
pub struct Grid {
    pub cols: Vec<String>,
    pub types: Vec<String>,
    rows: Vec<Vec<String>>,
    view: Vec<usize>,
    sort: Option<(usize, Dir)>,
    filter: String,
}

impl Grid {
    pub fn new(rs: ResultSet) -> Grid {
        let mut g = Grid {
            cols: rs.cols,
            types: rs.types.unwrap_or_default(),
            rows: rs.rows,
            ..Default::default()
        };
        g.refresh();
        g
    }

    /// 다음 쪽(table_page)을 뒤에 붙인다. 거르기·정렬은 유지한다.
    pub fn append(&mut self, rows: Vec<Vec<String>>) {
        self.rows.extend(rows);
        self.refresh();
    }

    pub fn source_len(&self) -> usize {
        self.rows.len()
    }

    pub fn len(&self) -> usize {
        self.view.len()
    }

    pub fn is_empty(&self) -> bool {
        self.view.is_empty()
    }

    /// 보이는 i 번째 줄의 원본 자리다. 거르기로 줄이 줄었는데 표가 옛 선택(줄 번호)을 들고 있을 수 있어 범위를 본다.
    pub fn source_row(&self, i: usize) -> Option<usize> {
        self.view.get(i).copied()
    }

    pub fn source(&self, src: usize) -> &[String] {
        self.rows.get(src).map_or(&[], Vec::as_slice)
    }

    pub fn cell(&self, i: usize, col: usize) -> &str {
        self.source_row(i).and_then(|r| self.rows[r].get(col)).map_or("", String::as_str)
    }

    pub fn sort_state(&self) -> Option<(usize, Dir)> {
        self.sort
    }

    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// None 이면 원래 순서로 돌린다.
    pub fn sort_by(&mut self, s: Option<(usize, Dir)>) {
        self.sort = s;
        self.refresh();
    }

    /// 어느 칸이든 글이 든 줄만 보인다(대소문자 무시).
    pub fn set_filter(&mut self, f: &str) {
        self.filter = f.to_lowercase();
        self.refresh();
    }

    fn refresh(&mut self) {
        let f = &self.filter;
        self.view = (0..self.rows.len())
            .filter(|&i| f.is_empty() || self.rows[i].iter().any(|c| c.to_lowercase().contains(f.as_str())))
            .collect();
        if let Some((col, dir)) = self.sort {
            let rows = &self.rows;
            // 안정 정렬이라 같은 값은 원래 순서를 지킨다
            self.view.sort_by(|&a, &b| {
                let (x, y) = (
                    rows[a].get(col).map_or("", String::as_str),
                    rows[b].get(col).map_or("", String::as_str),
                );
                // NULL 은 방향과 상관없이 늘 뒤다
                match (x == NULL, y == NULL) {
                    (true, true) => Ordering::Equal,
                    (true, false) => Ordering::Greater,
                    (false, true) => Ordering::Less,
                    _ => {
                        let o = compare(x, y);
                        if dir == Dir::Desc { o.reverse() } else { o }
                    }
                }
            });
        }
    }

    // ─── 내보내기 ───

    /// 보이는 줄 rows·열 cols 를 탭으로 나눈 글로 만든다(붙여넣기용). head 면 열 이름 줄을 앞에 둔다.
    pub fn tsv(&self, rows: impl IntoIterator<Item = usize>, cols: &[usize], head: bool) -> String {
        let clean = |s: &str| s.replace(['\t', '\n', '\r'], " ");
        let mut out = Vec::new();
        if head {
            out.push(cols.iter().map(|&c| clean(&self.cols[c])).collect::<Vec<_>>().join("\t"));
        }
        for i in rows {
            out.push(cols.iter().map(|&c| clean(self.cell(i, c))).collect::<Vec<_>>().join("\t"));
        }
        out.join("\n")
    }

    pub fn all_cols(&self) -> Vec<usize> {
        (0..self.cols.len()).collect()
    }

    /// 보이는 줄 전부를 RFC 4180 CSV 로 만든다.
    pub fn csv(&self) -> String {
        let mut out = String::new();
        let line = |cells: &mut dyn Iterator<Item = &str>, out: &mut String| {
            let v: Vec<String> = cells.map(csv_field).collect();
            out.push_str(&v.join(","));
            out.push_str("\r\n");
        };
        line(&mut self.cols.iter().map(String::as_str), &mut out);
        for i in 0..self.len() {
            line(&mut (0..self.cols.len()).map(|c| self.cell(i, c)), &mut out);
        }
        out
    }

    /// 보이는 줄 전부를 객체 배열 JSON 으로 만든다. NULL 칸은 null 이다.
    pub fn json(&self) -> String {
        let arr: Vec<serde_json::Value> = (0..self.len())
            .map(|i| {
                let mut m = serde_json::Map::new();
                for (c, name) in self.cols.iter().enumerate() {
                    let v = self.cell(i, c);
                    m.insert(name.clone(), if v == NULL { serde_json::Value::Null } else { v.into() });
                }
                serde_json::Value::Object(m)
            })
            .collect();
        serde_json::to_string_pretty(&arr).unwrap_or_default()
    }
}

/// 둘 다 수로 읽히면 수로, 아니면 글로 비교한다.
fn compare(a: &str, b: &str) -> Ordering {
    match (a.parse::<f64>(), b.parse::<f64>()) {
        (Ok(x), Ok(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
        _ => a.cmp(b),
    }
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rs(rows: &[&[&str]]) -> ResultSet {
        ResultSet {
            cols: vec!["id".into(), "name".into()],
            rows: rows.iter().map(|r| r.iter().map(|s| s.to_string()).collect()).collect(),
            ..Default::default()
        }
    }

    fn ids(g: &Grid) -> Vec<String> {
        (0..g.len()).map(|i| g.cell(i, 0).to_string()).collect()
    }

    #[test]
    fn 정렬은_수를_수로_비교하고_NULL_은_늘_뒤다() {
        let mut g = Grid::new(rs(&[&["10", "b"], &["NULL", "x"], &["9", "a"], &["100", "c"]]));
        g.sort_by(Some((0, Dir::Asc)));
        assert_eq!(ids(&g), vec!["9", "10", "100", "NULL"]);
        g.sort_by(Some((0, Dir::Desc)));
        assert_eq!(ids(&g), vec!["100", "10", "9", "NULL"]);
        g.sort_by(None);
        assert_eq!(ids(&g), vec!["10", "NULL", "9", "100"]);
        assert_eq!(g.source_row(1), Some(1));
    }

    #[test]
    fn 거르기는_어느_칸이든_대소문자_없이_찾고_정렬과_같이_돈다() {
        let mut g = Grid::new(rs(&[&["1", "Apple"], &["2", "banana"], &["3", "apricot"]]));
        g.sort_by(Some((0, Dir::Desc)));
        g.set_filter("AP");
        assert_eq!(ids(&g), vec!["3", "1"]);
        assert_eq!(g.source_row(0), Some(2));
        g.set_filter("");
        assert_eq!(g.len(), 3);
    }

    #[test]
    fn 다음_쪽을_붙여도_거르기가_유지된다() {
        let mut g = Grid::new(rs(&[&["1", "a"]]));
        g.set_filter("b");
        g.append(vec![vec!["2".into(), "b".into()], vec!["3".into(), "c".into()]]);
        assert_eq!(g.source_len(), 3);
        assert_eq!(ids(&g), vec!["2"]);
    }

    #[test]
    fn 거르기로_줄어든_뒤_옛_줄_번호를_물어도_넘어지지_않는다() {
        let mut g = Grid::new(rs(&[&["1", "a"], &["2", "b"], &["3", "c"]]));
        g.set_filter("c");
        assert_eq!(g.len(), 1);
        assert_eq!(g.source_row(2), None);
        assert_eq!(g.cell(2, 0), "");
        assert_eq!(g.cell(0, 9), "");
        assert!(g.source(99).is_empty());
        assert_eq!(g.tsv([5], &[0, 1], false), "\t");
    }

    #[test]
    fn 복사는_탭으로_나누고_칸_안_탭_줄바꿈은_공백이다() {
        let g = Grid::new(rs(&[&["1", "a\tb"], &["2", "c\nd"]]));
        assert_eq!(g.tsv(0..2, &[0, 1], true), "id\tname\n1\ta b\n2\tc d");
        assert_eq!(g.tsv([1], &[1], false), "c d");
    }

    #[test]
    fn CSV_는_따옴표가_필요한_칸만_감싼다() {
        let g = Grid::new(rs(&[&["1", "a,b"], &["2", "say \"hi\""]]));
        assert_eq!(g.csv(), "id,name\r\n1,\"a,b\"\r\n2,\"say \"\"hi\"\"\"\r\n");
    }

    #[test]
    fn JSON_은_NULL_칸을_null_로_낸다() {
        let g = Grid::new(rs(&[&["1", "NULL"]]));
        let v: serde_json::Value = serde_json::from_str(&g.json()).unwrap();
        assert_eq!(v, serde_json::json!([{"id": "1", "name": null}]));
    }

    #[test]
    fn 큰_결과도_보이는_순서만_만든다() {
        let rows: Vec<Vec<String>> = (0..200_000).map(|i| vec![i.to_string(), format!("n{i}")]).collect();
        let mut g = Grid::new(ResultSet {
            cols: vec!["id".into(), "name".into()],
            rows,
            ..Default::default()
        });
        g.sort_by(Some((0, Dir::Desc)));
        assert_eq!(g.cell(0, 0), "199999");
        g.set_filter("n19999");
        assert_eq!(g.len(), 11);
    }
}
