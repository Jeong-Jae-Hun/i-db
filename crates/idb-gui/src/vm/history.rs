//! 입력 기록(테이블 보기 WHERE·ORDER BY). 최근 것이 앞이고 같은 글은 한 번만 둔다.

pub const MAX: usize = 20;

/// 앞에 넣는다. 빈 글은 넣지 않는다.
pub fn push(items: &mut Vec<String>, s: &str) {
    let s = s.trim();
    if s.is_empty() {
        return;
    }
    items.retain(|x| x != s);
    items.insert(0, s.to_string());
    items.truncate(MAX);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 최근_것이_앞이고_같은_글은_한_번이다() {
        let mut h = Vec::new();
        push(&mut h, "id > 1");
        push(&mut h, " name = 'a' ");
        push(&mut h, "id > 1");
        push(&mut h, "   ");
        assert_eq!(h, vec!["id > 1", "name = 'a'"]);
        for i in 0..30 {
            push(&mut h, &format!("x = {i}"));
        }
        assert_eq!(h.len(), MAX);
        assert_eq!(h[0], "x = 29");
    }
}
