//! 실행 계획 나무(R9·R22.2). `EXPLAIN (FORMAT JSON)` 결과를 노드 줄로 펴고 비싼 노드를 강조한다.
//! 몫은 ANALYZE 면 자기 시간(루프 합에서 자식 시간을 뺀 것), 아니면 자기 비용이다(Go `internal/plantree` 를 줄인 것).

use serde_json::Value;

#[derive(Clone, Debug)]
pub struct PNode {
    pub depth: usize,
    pub label: String,
    pub detail: String,
    /// 전체에서 자기 몫 0~100
    pub percent: f64,
    pub folded: bool,
    pub kids: bool,
}

#[derive(Clone, Debug)]
pub struct PlanView {
    pub nodes: Vec<PNode>,
    pub sel: usize,
    pub top: usize,
    pub raw: String,
    pub show_raw: bool,
    pub summary: String,
}

fn f(v: &Value, k: &str) -> f64 {
    v.get(k).and_then(Value::as_f64).unwrap_or(0.0)
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

/// 루프를 더한 시간(ms)이다.
fn total_time(n: &Value) -> f64 {
    f(n, "Actual Total Time") * f(n, "Actual Loops").max(1.0)
}

fn walk(n: &Value, depth: usize, analyzed: bool, total: f64, out: &mut Vec<PNode>) {
    let kids: Vec<&Value> = n
        .get("Plans")
        .and_then(Value::as_array)
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    let own = if analyzed {
        (total_time(n) - kids.iter().map(|k| total_time(k)).sum::<f64>()).max(0.0)
    } else {
        (f(n, "Total Cost") - kids.iter().map(|k| f(k, "Total Cost")).sum::<f64>()).max(0.0)
    };
    let mut label = s(n, "Node Type").to_string();
    if !s(n, "Join Type").is_empty() {
        label = format!("{} {label}", s(n, "Join Type"));
    }
    if !s(n, "Relation Name").is_empty() {
        label.push_str(&format!(" on {}", s(n, "Relation Name")));
        if !s(n, "Alias").is_empty() && s(n, "Alias") != s(n, "Relation Name") {
            label.push_str(&format!(" {}", s(n, "Alias")));
        }
    }
    if !s(n, "Index Name").is_empty() {
        label.push_str(&format!(" using {}", s(n, "Index Name")));
    }
    let mut detail = format!("cost {:.2} · rows {}", f(n, "Total Cost"), f(n, "Plan Rows") as i64);
    if analyzed {
        detail.push_str(&format!(
            " · actual {:.3}ms × {} · rows {}",
            f(n, "Actual Total Time"),
            f(n, "Actual Loops") as i64,
            f(n, "Actual Rows") as i64
        ));
    }
    for k in ["Index Cond", "Filter", "Hash Cond", "Join Filter"] {
        if !s(n, k).is_empty() {
            detail.push_str(&format!(" · {k}: {}", s(n, k)));
        }
    }
    out.push(PNode {
        depth,
        label,
        detail,
        percent: if total > 0.0 { (100.0 * own / total).min(100.0) } else { 0.0 },
        folded: false,
        kids: !kids.is_empty(),
    });
    for k in kids {
        walk(k, depth + 1, analyzed, total, out);
    }
}

impl PlanView {
    /// JSON 계획 글이면 나무를 만든다. 아니면 None 이다(표로 보인다).
    pub fn parse(text: &str) -> Option<PlanView> {
        let v: Value = serde_json::from_str(text.trim()).ok()?;
        let doc = match &v {
            Value::Array(a) => a.first()?.clone(),
            Value::Object(_) => v.clone(),
            _ => return None,
        };
        let root = doc.get("Plan")?;
        if s(root, "Node Type").is_empty() {
            return None;
        }
        let analyzed = doc.get("Execution Time").is_some();
        let total = if analyzed { total_time(root) } else { f(root, "Total Cost") };
        let mut nodes = Vec::new();
        walk(root, 0, analyzed, total, &mut nodes);
        let mut summary = format!("비용 {:.2}", f(root, "Total Cost"));
        if analyzed {
            summary = format!(
                "계획 {:.3}ms · 실행 {:.3}ms · {summary}",
                f(&doc, "Planning Time"),
                f(&doc, "Execution Time")
            );
        }
        Some(PlanView {
            nodes,
            sel: 0,
            top: 0,
            raw: serde_json::to_string_pretty(&v).unwrap_or_else(|_| text.to_string()),
            show_raw: false,
            summary,
        })
    }

    pub fn visible(&self) -> Vec<usize> {
        let mut out = Vec::new();
        let mut skip: Option<usize> = None;
        for (i, n) in self.nodes.iter().enumerate() {
            if let Some(d) = skip {
                if n.depth > d {
                    continue;
                }
                skip = None;
            }
            out.push(i);
            if n.folded {
                skip = Some(n.depth);
            }
        }
        out
    }

    /// 결과 영역 키다. 썼으면 true, esc 는 닫기라 부르는 쪽 몫이다(false).
    pub fn key(&mut self, k: &str) -> bool {
        let n = self.visible().len();
        match k {
            "t" => self.show_raw = !self.show_raw,
            "down" | "j" => self.sel = (self.sel + 1).min(n.saturating_sub(1)),
            "up" | "k" => self.sel = self.sel.saturating_sub(1),
            "enter" | "space" if !self.show_raw => {
                if let Some(&i) = self.visible().get(self.sel)
                    && self.nodes[i].kids
                {
                    self.nodes[i].folded = !self.nodes[i].folded;
                }
            }
            _ => return false,
        }
        true
    }

    /// 화면 줄과 강조 단계(0 보통, 1 노랑 10% 이상, 2 빨강 30% 이상)다.
    pub fn lines(&self) -> Vec<(String, u8)> {
        self.visible()
            .into_iter()
            .map(|i| {
                let n = &self.nodes[i];
                let mark = if !n.kids {
                    "  "
                } else if n.folded {
                    "▸ "
                } else {
                    "▾ "
                };
                let hot = if n.percent >= 30.0 {
                    2
                } else if n.percent >= 10.0 {
                    1
                } else {
                    0
                };
                (
                    format!("{}{mark}{:>3.0}% {}  ({})", "  ".repeat(n.depth), n.percent, n.label, n.detail),
                    hot,
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 계획_나무는_자기_몫으로_비싼_노드를_가르고_접는다() {
        let j = r#"[{"Plan":{"Node Type":"Hash Join","Join Type":"Inner","Total Cost":100.0,"Plan Rows":10,
            "Plans":[{"Node Type":"Seq Scan","Relation Name":"orders","Alias":"o","Total Cost":80.0,"Plan Rows":1000,"Filter":"(x > 1)"},
                     {"Node Type":"Index Scan","Relation Name":"users","Alias":"users","Index Name":"users_pk","Total Cost":5.0,"Plan Rows":1}]}}]"#;
        let mut p = PlanView::parse(j).unwrap();
        let l = p.lines();
        assert!(l[0].0.starts_with("▾  15% Inner Hash Join"), "{}", l[0].0);
        assert!(l[1].0.contains("80% Seq Scan on orders o") && l[1].1 == 2, "{}", l[1].0);
        assert!(l[2].0.contains("Index Scan on users using users_pk") && l[2].1 == 0);
        p.key("enter");
        assert_eq!(p.lines().len(), 1);
        p.key("t");
        assert!(p.show_raw && p.raw.contains("\"Node Type\""));
        assert!(PlanView::parse("Seq Scan on t").is_none());
    }
}
