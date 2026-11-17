//! JSON 값 나무(R32). 칸 값 보기에서 값이 JSON 이면 접이식 나무로 보이고 `t` 로 원문과 오간다.

use serde_json::Value;

/// 이보다 큰 값은 나무를 만들지 않고 원문으로만 보인다(R32.1)
pub const MAX_TREE_BYTES: usize = 1 << 20;

#[derive(Clone, Debug)]
pub struct Node {
    pub depth: usize,
    pub key: String,
    /// `$.items[0].name` 꼴 경로
    pub path: String,
    pub value: Value,
    pub folded: bool,
}

#[derive(Clone, Debug)]
pub struct JsonTree {
    pub nodes: Vec<Node>,
    pub sel: usize,
    pub top: usize,
}

fn is_ident(k: &str) -> bool {
    !k.is_empty() && k.chars().all(|c| c.is_alphanumeric() || c == '_') && !k.starts_with(|c: char| c.is_ascii_digit())
}

fn walk(v: &Value, key: String, path: String, depth: usize, out: &mut Vec<Node>) {
    out.push(Node {
        depth,
        key,
        path: path.clone(),
        value: v.clone(),
        folded: false,
    });
    match v {
        Value::Object(m) => {
            for (k, c) in m {
                let p = if is_ident(k) {
                    format!("{path}.{k}")
                } else {
                    format!("{path}[{}]", serde_json::to_string(k).unwrap_or_default())
                };
                walk(c, k.clone(), p, depth + 1, out);
            }
        }
        Value::Array(a) => {
            for (i, c) in a.iter().enumerate() {
                walk(c, format!("[{i}]"), format!("{path}[{i}]"), depth + 1, out);
            }
        }
        _ => {}
    }
}

impl JsonTree {
    /// 값이 JSON 객체·배열로 읽히면 나무를 만든다. 크면 None 이다.
    pub fn parse(s: &str, json_type: bool) -> Option<JsonTree> {
        if s.len() > MAX_TREE_BYTES {
            return None;
        }
        let t = s.trim_start();
        if !json_type && !t.starts_with(['{', '[']) {
            return None;
        }
        let v: Value = serde_json::from_str(s).ok()?;
        let mut nodes = Vec::new();
        walk(&v, "$".into(), "$".into(), 0, &mut nodes);
        Some(JsonTree { nodes, sel: 0, top: 0 })
    }

    /// 접힌 노드의 자손을 뺀 보이는 노드 자리다.
    pub fn visible(&self) -> Vec<usize> {
        let mut out = Vec::new();
        let mut skip_below: Option<usize> = None;
        for (i, n) in self.nodes.iter().enumerate() {
            if let Some(d) = skip_below {
                if n.depth > d {
                    continue;
                }
                skip_below = None;
            }
            out.push(i);
            if n.folded {
                skip_below = Some(n.depth);
            }
        }
        out
    }

    pub fn current(&self) -> Option<&Node> {
        self.visible().get(self.sel).map(|&i| &self.nodes[i])
    }

    pub fn toggle(&mut self) {
        if let Some(&i) = self.visible().get(self.sel)
            && matches!(self.nodes[i].value, Value::Object(_) | Value::Array(_))
        {
            self.nodes[i].folded = !self.nodes[i].folded;
        }
    }

    pub fn move_by(&mut self, d: isize) {
        let n = self.visible().len() as isize;
        self.sel = (self.sel as isize + d).clamp(0, (n - 1).max(0)) as usize;
    }

    /// 화면 줄이다: 들여쓰기 · 접기 표시 · 키 · 값(모음은 크기).
    pub fn lines(&self) -> Vec<String> {
        self.visible()
            .into_iter()
            .map(|i| {
                let n = &self.nodes[i];
                let mark = match &n.value {
                    Value::Object(_) | Value::Array(_) if n.folded => "▸ ",
                    Value::Object(_) | Value::Array(_) => "▾ ",
                    _ => "  ",
                };
                let val = match &n.value {
                    Value::Object(m) => format!("{{{}}}", m.len()),
                    Value::Array(a) => format!("[{}]", a.len()),
                    v => v.to_string(),
                };
                format!("{}{mark}{}: {val}", "  ".repeat(n.depth), n.key)
            })
            .collect()
    }
}

/// 노드 값의 복사 글이다. 문자열은 따옴표 없이, 나머지는 JSON 이다.
pub fn copy_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        v => serde_json::to_string_pretty(v).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 나무는_접고_펴며_경로를_만든다() {
        let mut t = JsonTree::parse(r#"{"items":[{"name":"x"}],"a b":1}"#, false).unwrap();
        let paths: Vec<&str> = t.nodes.iter().map(|n| n.path.as_str()).collect();
        assert_eq!(paths, ["$", "$[\"a b\"]", "$.items", "$.items[0]", "$.items[0].name"]);
        t.sel = 2;
        t.toggle();
        assert_eq!(t.lines(), ["▾ $: {2}", "    a b: 1", "  ▸ items: [1]"]);
        assert!(JsonTree::parse("plain", false).is_none());
        assert!(JsonTree::parse("\"s\"", true).is_some());
        assert_eq!(copy_value(&Value::String("q".into())), "q");
    }
}
