//! PartiQL 낱말(R63). 쓰기 판정과 파티션 키 조건 찾기에 필요한 만큼만 자른다. 문법 검사는 DynamoDB 가 한다.
//! 주석(-- …, /* … */)은 버리고, 문자열('…')·따옴표 이름("…") 안의 글자는 낱말로 보지 않는다. Go `internal/dynamo/partiql.go` 다.

use idb_core::sqlsafe::quote_ident;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TokKind {
    /// 따옴표 없는 이름·예약어
    Word,
    /// "이름"
    Ident,
    /// '문자열'
    Str,
    Number,
    Punct,
}

#[derive(Clone, Debug)]
pub(crate) struct Token {
    pub kind: TokKind,
    /// Ident·Str 은 따옴표를 벗기고 겹따옴표('' "")를 푼 값
    pub text: String,
    /// 원문에서의 끝 자리. 문장을 고쳐 쓸 때(ORDER BY 바꾸기) 쓴다
    pub end: usize,
}

impl Token {
    fn is(&self, word: &str) -> bool {
        self.kind == TokKind::Word && self.text.eq_ignore_ascii_case(word)
    }

    /// 이름 자리(따옴표 유무 무관) 낱말인지 본다.
    fn is_name(&self) -> bool {
        matches!(self.kind, TokKind::Word | TokKind::Ident)
    }
}

fn is_word_byte(c: u8) -> bool {
    c == b'_' || c.is_ascii_alphanumeric() || c >= 0x80
}

pub(crate) fn tokenize(s: &str) -> Vec<Token> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            b' ' | b'\t' | b'\n' | b'\r' => i += 1,
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => match s[i + 2..].find("*/") {
                None => i = b.len(),
                Some(end) => i += 2 + end + 2,
            },
            b'\'' | b'"' => {
                let mut v = Vec::new();
                let mut j = i + 1;
                while j < b.len() {
                    if b[j] == c {
                        if b.get(j + 1) == Some(&c) {
                            v.push(c);
                            j += 2;
                            continue;
                        }
                        j += 1;
                        break;
                    }
                    v.push(b[j]);
                    j += 1;
                }
                let kind = if c == b'"' { TokKind::Ident } else { TokKind::Str };
                out.push(Token {
                    kind,
                    text: String::from_utf8_lossy(&v).into_owned(),
                    end: j,
                });
                i = j;
            }
            c if is_word_byte(c) => {
                let mut j = i;
                while j < b.len() && is_word_byte(b[j]) {
                    j += 1;
                }
                let kind = if c.is_ascii_digit() { TokKind::Number } else { TokKind::Word };
                out.push(Token {
                    kind,
                    text: String::from_utf8_lossy(&b[i..j]).into_owned(),
                    end: j,
                });
                i = j;
            }
            _ => {
                out.push(Token {
                    kind: TokKind::Punct,
                    text: (c as char).to_string(),
                    end: i + 1,
                });
                i += 1;
            }
        }
    }
    out
}

fn punct_at(toks: &[Token], i: isize, p: &str) -> bool {
    i >= 0 && (i as usize) < toks.len() && toks[i as usize].kind == TokKind::Punct && toks[i as usize].text == p
}

// ─── 판정 ───

/// 문장 하나짜리 SELECT 인지 본다. 끝의 ; 하나는 받되 그 뒤에 다른 문장이 있으면 아니다.
pub(crate) fn is_select(toks: &[Token]) -> bool {
    if toks.is_empty() || !toks[0].is("SELECT") {
        return false;
    }
    match toks.iter().position(|t| t.kind == TokKind::Punct && t.text == ";") {
        Some(i) => i == toks.len() - 1,
        None => true,
    }
}

/// FROM 뒤 테이블과 인덱스 이름이다("t"."idx"). 찾지 못하면 None 이다.
pub(crate) fn target(toks: &[Token]) -> Option<(String, String)> {
    let i = top_level(toks, 0, "FROM")?;
    if i + 1 >= toks.len() || !toks[i + 1].is_name() {
        return None;
    }
    let mut index = String::new();
    if i + 3 < toks.len() && punct_at(toks, (i + 2) as isize, ".") && toks[i + 3].is_name() {
        index = toks[i + 3].text.clone();
    }
    Some((toks[i + 1].text.clone(), index))
}

/// WHERE 가 파티션 키를 = 또는 IN 으로 고정하는지 본다. 괄호로 감싼 조건은 벗겨서 다시 본다.
/// 판단이 서지 않으면 false(확인받기) 쪽으로 둔다. OR 로 갈린 조건은 가지마다 키가 있어도 스캔으로 본다(여러 값은 IN).
pub(crate) fn has_key_condition(toks: &[Token], pk: &str) -> bool {
    key_condition(toks, pk, true)
}

/// has_key_condition 이고 allow_in 이 false 면 = 만 본다(ORDER BY 는 파티션 하나여야 한다, R63.3).
pub(crate) fn key_condition(toks: &[Token], pk: &str, allow_in: bool) -> bool {
    let Some(w) = top_level(toks, 0, "WHERE") else { return false };
    let mut end = toks.len();
    for stop in ["ORDER", "LIMIT"] {
        if let Some(j) = top_level(toks, w + 1, stop)
            && j < end
        {
            end = j;
        }
    }
    if end > 0 && punct_at(toks, end as isize - 1, ";") {
        end -= 1;
    }
    branch_has_key(&toks[w + 1..end.max(w + 1)], pk, allow_in)
}

fn branch_has_key(toks: &[Token], pk: &str, allow_in: bool) -> bool {
    if split_top_level(toks, "OR").len() > 1 {
        return false;
    }
    if wrapped(toks) {
        return branch_has_key(&toks[1..toks.len() - 1], pk, allow_in);
    }
    // AND 로 묶인 조각 가운데 하나라도 키를 고정하면 된다. (pk = 'a') AND … 처럼 괄호 친 조각은 벗겨 본다
    split_top_level(toks, "AND")
        .into_iter()
        .any(|part| (wrapped(part) && branch_has_key(&part[1..part.len() - 1], pk, allow_in)) || conjunction_has_key(part, pk, allow_in))
}

/// AND·OR 가 없는 조각에서 맨 바깥의 `pk = 값`·`값 = pk`·`pk IN …` 을 찾는다.
fn conjunction_has_key(toks: &[Token], pk: &str, allow_in: bool) -> bool {
    let mut depth = 0;
    for (i, t) in toks.iter().enumerate() {
        if t.kind == TokKind::Punct {
            match t.text.as_str() {
                "(" | "[" => depth += 1,
                ")" | "]" => depth -= 1,
                _ => {}
            }
        }
        if depth != 0 || !t.is_name() || t.text != pk || (t.kind == TokKind::Word && is_keyword(&t.text)) {
            continue;
        }
        let i = i as isize;
        // a.pk 처럼 중첩 경로의 일부거나 NOT pk = … 은 키 조건이 아니다
        if i > 0 && (punct_at(toks, i - 1, ".") || toks[(i - 1) as usize].is("NOT")) {
            continue;
        }
        let next = (i + 1) as usize;
        if next < toks.len() && ((punct_at(toks, i + 1, "=") && !punct_at(toks, i + 2, "=")) || (allow_in && toks[next].is("IN"))) {
            return true;
        }
        if i >= 2
            && punct_at(toks, i - 1, "=")
            && matches!(toks[(i - 2) as usize].kind, TokKind::Str | TokKind::Number)
            && !punct_at(toks, i - 2, "=")
        {
            return true;
        }
    }
    false
}

/// 가지 전체가 괄호 한 쌍으로 감싸였는지 본다((a) AND (b) 는 아니다).
fn wrapped(toks: &[Token]) -> bool {
    if toks.len() < 2 || !punct_at(toks, 0, "(") || !punct_at(toks, toks.len() as isize - 1, ")") {
        return false;
    }
    let mut depth = 0;
    for (i, t) in toks.iter().enumerate() {
        if t.kind != TokKind::Punct {
            continue;
        }
        match t.text.as_str() {
            "(" => depth += 1,
            ")" => {
                depth -= 1;
                if depth == 0 && i != toks.len() - 1 {
                    return false;
                }
            }
            _ => {}
        }
    }
    true
}

fn split_top_level<'a>(toks: &'a [Token], word: &str) -> Vec<&'a [Token]> {
    let mut out = Vec::new();
    let (mut depth, mut start) = (0, 0);
    for (i, t) in toks.iter().enumerate() {
        let ii = i as isize;
        if punct_at(toks, ii, "(") || punct_at(toks, ii, "[") {
            depth += 1;
        } else if punct_at(toks, ii, ")") || punct_at(toks, ii, "]") {
            depth -= 1;
        } else if depth == 0 && t.is(word) {
            out.push(&toks[start..i]);
            start = i + 1;
        }
    }
    out.push(&toks[start..]);
    out
}

/// from 부터 괄호 밖의 예약어 word 자리를 찾는다.
fn top_level(toks: &[Token], from: usize, word: &str) -> Option<usize> {
    let mut depth = 0;
    for i in from..toks.len() {
        let ii = i as isize;
        if punct_at(toks, ii, "(") || punct_at(toks, ii, "[") {
            depth += 1;
        } else if punct_at(toks, ii, ")") || punct_at(toks, ii, "]") {
            depth -= 1;
        } else if depth == 0 && toks[i].is(word) {
            return Some(i);
        }
    }
    None
}

/// 따옴표 없이 쓰면 이름이 아니라 예약어로 읽히는 낱말이다.
fn is_keyword(w: &str) -> bool {
    matches!(
        w.to_ascii_uppercase().as_str(),
        "SELECT" | "FROM" | "WHERE" | "AND" | "OR" | "NOT" | "IN" | "BETWEEN" | "IS" | "ORDER" | "BY" | "LIMIT"
    )
}

// ─── 서버 정렬 (R63.3) ───

/// SELECT 의 맨 바깥 ORDER BY 를 `ORDER BY "<sk>" ASC|DESC` 로 바꾸거나 끝에 붙인 글이다.
/// 끝의 ; 와 마지막 낱말 뒤 주석은 뗀다. LIMIT 이 있으면 None 이다.
pub(crate) fn sorted_statement(toks: &[Token], text: &str, sk: &str, desc: bool) -> Option<String> {
    if !is_select(toks) || sk.is_empty() || top_level(toks, 0, "LIMIT").is_some() {
        return None;
    }
    let mut last = toks.len() as isize - 1;
    if punct_at(toks, last, ";") {
        last -= 1;
    }
    if let Some(o) = top_level(toks, 0, "ORDER")
        && o > 0
    {
        last = o as isize - 1;
    }
    if last < 0 {
        return None;
    }
    let dir = if desc { "DESC" } else { "ASC" };
    Some(format!("{} ORDER BY {} {dir}", &text[..toks[last as usize].end], quote_ident(sk)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn SELECT_만_받고_나머지는_쓰기로_본다() {
        let cases = [
            (r#"SELECT * FROM "t""#, true),
            ("  select a FROM t;", true),
            ("-- 메모\n/* 여러\n줄 */ SeLeCt * FROM \"t\"", true),
            (r#"INSERT INTO "t" VALUE {'pk':'a'}"#, false),
            (r#"UPDATE "t" SET a=1 WHERE pk='a'"#, false),
            (r#"DELETE FROM "t" WHERE pk='a'"#, false),
            (r#"EXISTS(SELECT * FROM "t" WHERE pk='a')"#, false),
            ("-- SELECT\nDELETE FROM t", false),
            ("/* SELECT */ DELETE FROM t", false),
            ("SELECT * FROM t; DELETE FROM t", false),
            ("'SELECT' FROM t", false),
            ("", false),
            ("-- 주석만", false),
        ];
        for (q, want) in cases {
            assert_eq!(is_select(&tokenize(q)), want, "{q}");
        }
    }

    #[test]
    fn 테이블과_인덱스_이름을_읽는다() {
        let cases = [
            (r#"SELECT * FROM "orders" WHERE pk='a'"#, Some(("orders", ""))),
            ("SELECT * FROM orders", Some(("orders", ""))),
            (
                r#"SELECT * FROM "orders"."by-status" WHERE status='x'"#,
                Some(("orders", "by-status")),
            ),
            (r#"SELECT * FROM "a""b""#, Some((r#"a"b"#, ""))),
            (r#"SELECT 'FROM x' FROM "real""#, Some(("real", ""))),
            ("SELECT *", None),
        ];
        for (q, want) in cases {
            let got = target(&tokenize(q));
            assert_eq!(got.as_ref().map(|(t, i)| (t.as_str(), i.as_str())), want, "{q}");
        }
    }

    #[test]
    fn 파티션_키_조건이_있을_때만_스캔이_아니다() {
        let cases = [
            (r#"SELECT * FROM "t" WHERE "pk" = 'a'"#, true),
            ("SELECT * FROM t WHERE pk='a' AND sk > 3", true),
            (r#"SELECT * FROM t WHERE sk > 3 AND "pk" = 'a'"#, true),
            ("SELECT * FROM t WHERE 'a' = pk", true),
            ("SELECT * FROM t WHERE pk IN ['a','b']", true),
            (r#"SELECT * FROM t WHERE "pk" in ['a']"#, true),
            ("SELECT * FROM t WHERE (pk = 'a' AND sk > 1)", true),
            ("SELECT * FROM t WHERE (pk = 'a') AND sk > 1", true),
            ("SELECT * FROM t WHERE pk = 'a' OR pk = 'b'", false),
            ("SELECT * FROM t WHERE (pk = 'a' OR pk = 'b') AND sk > 1", false),
            ("SELECT * FROM t WHERE pk = 'a' AND (sk = 1 OR sk = 2)", true),
            ("SELECT * FROM t WHERE pk = 'a';", true),
            ("SELECT * FROM t WHERE pk = 'a' ORDER BY sk DESC", true),
            ("SELECT * FROM t", false),
            ("SELECT * FROM t WHERE status = 'x'", false),
            ("SELECT * FROM t WHERE pk = 'a' OR status = 'x'", false),
            ("SELECT * FROM t WHERE pk > 'a'", false),
            ("SELECT * FROM t WHERE pk <> 'a'", false),
            ("SELECT * FROM t WHERE pk NOT IN ['a']", false),
            ("SELECT * FROM t WHERE NOT pk = 'a'", false),
            ("SELECT * FROM t WHERE begins_with(pk, 'a')", false),
            ("SELECT * FROM t WHERE size(pk = 'a')", false),
            ("SELECT * FROM t WHERE status = 'pk = 1'", false),
            ("SELECT * FROM t WHERE status = 'x' -- pk = 'a'", false),
            ("SELECT * FROM t WHERE info.pk = 'a'", false),
            ("SELECT * FROM t WHERE PK = 'a'", false),
            ("SELECT * FROM t WHERE other = pk", false),
            ("SELECT pk FROM t WHERE status = 'x'", false),
        ];
        for (q, want) in cases {
            assert_eq!(has_key_condition(&tokenize(q), "pk"), want, "{q}");
        }
    }

    #[test]
    fn 맨_바깥_ORDER_BY_를_바꾸거나_붙인다() {
        let cases = [
            (
                r#"SELECT * FROM "t" WHERE pk = 'a'"#,
                false,
                r#"SELECT * FROM "t" WHERE pk = 'a' ORDER BY "sk" ASC"#,
            ),
            (
                "SELECT * FROM \"t\" WHERE pk = 'a'; -- 메모",
                true,
                r#"SELECT * FROM "t" WHERE pk = 'a' ORDER BY "sk" DESC"#,
            ),
            (
                "SELECT * FROM t WHERE pk = 'a' -- 메모\nORDER BY sk ASC",
                true,
                r#"SELECT * FROM t WHERE pk = 'a' ORDER BY "sk" DESC"#,
            ),
            (
                r#"SELECT * FROM t WHERE pk = 'order' ORDER BY "sk" DESC;"#,
                false,
                r#"SELECT * FROM t WHERE pk = 'order' ORDER BY "sk" ASC"#,
            ),
        ];
        for (text, desc, want) in cases {
            assert_eq!(sorted_statement(&tokenize(text), text, "sk", desc).as_deref(), Some(want), "{text}");
        }
        for text in ["SELECT * FROM t WHERE pk = 'a' LIMIT 5", "DELETE FROM t WHERE pk = 'a'"] {
            assert_eq!(sorted_statement(&tokenize(text), text, "sk", false), None, "{text}");
        }
    }
}
