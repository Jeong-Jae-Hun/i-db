//! SQL 의 :name·$n 자리표시를 찾아 PostgreSQL 바인딩 형식($n)으로 맞춘다. Go `internal/params` 와 같다.
//! 토큰은 sqlfmt 의 tokenize 로 읽는다.

use crate::project::go_atoi;
use crate::sqlfmt::{Kind, Token, tokenize};
use crate::sqlsafe::quote_literal;

/// PostgreSQL 이 한 문장에 받는 바인딩 인자의 상한이다. 넘거나 정수로 못 읽는 $n 은 이름 목록을 만들지 않는다
const MAX_PARAMS: i64 = 65535;

/// :name 을 $k 로 바꾼 SQL 과, $k 마다 물어볼 이름(names[k-1])을 돌려준다.
/// $1·$2 처럼 이미 번호인 자리는 그대로 두고 이름을 "1"·"2" 로 쓴다. 같은 :name 은 같은 번호를 받고,
/// 번호와 섞이면 :name 은 가장 큰 $n 다음 번호부터 받는다. 문자열·따옴표 이름·주석·$$ 안과
/// ::type 캐스트, a[1:2]·a[lo:hi]·a[:n] 처럼 ':' 앞뒤가 붙은 배열 조각은 건드리지 않는다.
pub fn bind(sql: &str) -> (String, Vec<String>) {
    let toks = tokenize(sql);
    let text = |t: &Token| &sql[t.start..t.end];
    let highest = toks
        .iter()
        .filter(|t| t.kind == Kind::Param)
        .filter_map(|t| go_atoi(&text(t)[1..]).filter(|&n| n <= MAX_PARAMS))
        .max()
        .unwrap_or(0);
    let mut names: Vec<String> = (1..=highest).map(|n| n.to_string()).collect();

    let mut b = String::new();
    let mut last = 0;
    let mut nums: Vec<(&str, usize)> = Vec::new();
    let mut i = 0;
    while i + 1 < toks.len() {
        let (colon, word) = (&toks[i], &toks[i + 1]);
        if colon.kind != Kind::Punct || text(colon) != ":" || colon.end != word.start {
            i += 1;
            continue;
        }
        if text(word) == ":" {
            i += 2; // :: 캐스트 뒤 타입 이름은 자리표시가 아니다
            continue;
        }
        // ':' 바로 앞에 이름·숫자가 붙으면 a[lo:hi] 같은 배열 조각이다. 바꾸면 x$1·$$1 처럼 앞 토큰과 붙어
        // 다른 낱말이나 $$ 인용이 되기도 한다. a[:n] 처럼 '[' 바로 뒤도 아래쪽을 생략한 배열 조각이다
        let glued = i > 0 && toks[i - 1].end == colon.start && {
            let p = &toks[i - 1];
            matches!(p.kind, Kind::Word | Kind::Number | Kind::Quoted | Kind::Param) || matches!(text(p), "$" | "[")
        };
        if word.kind != Kind::Word || glued {
            i += 1;
            continue;
        }
        let name = text(word);
        let k = match nums.iter().find(|(n, _)| *n == name) {
            Some(&(_, k)) => k,
            None => {
                names.push(name.to_string());
                nums.push((name, names.len()));
                names.len()
            }
        };
        b.push_str(&sql[last..colon.start]);
        b.push('$');
        b.push_str(&k.to_string());
        last = word.end;
        i += 1;
    }
    if last == 0 {
        return (sql.to_string(), names);
    }
    b.push_str(&sql[last..]);
    (b, names)
}

/// sql 의 $n 자리마다 repl(n, "$n") 이 준 글로 바꾼다. None 이면 그대로 둔다. 문자열·주석 안의 $n 은 토큰이 아니라 건드리지 않는다.
pub fn replace_params(sql: &str, mut repl: impl FnMut(i64, &str) -> Option<String>) -> String {
    let mut b = String::new();
    let mut last = 0;
    for t in tokenize(sql).iter().filter(|t| t.kind == Kind::Param) {
        let text = &sql[t.start..t.end];
        let Some(with) = go_atoi(&text[1..]).and_then(|n| repl(n, text)) else {
            continue;
        };
        b.push_str(&sql[last..t.start]);
        b.push_str(&with);
        last = t.end;
    }
    b.push_str(&sql[last..]);
    b
}

/// sql 의 $n 자리를 args[n-1] 리터럴(None 은 NULL)로 바꾼다. 읽기 전용 실행이 바인딩을 받지 않아서다.
/// 값은 quote_literal 로 감싸 문장을 바꾸지 못한다.
pub fn inline_args(sql: &str, args: &[Option<String>]) -> String {
    replace_params(sql, |n, _| {
        let i = usize::try_from(n).ok()?.checked_sub(1)?;
        let v = args.get(i)?;
        Some(v.as_deref().map_or_else(|| "NULL".to_string(), quote_literal))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 이름_자리를_번호로_바꾸고_나머지는_건드리지_않는다() {
        let (sql, names) = bind("select ':x', \"a:b\", x::int, :id, :id, arr[1:2], a[lo:hi], $$ :no $$, :name -- :c\n/* :d */");
        assert_eq!(
            sql,
            "select ':x', \"a:b\", x::int, $1, $1, arr[1:2], a[lo:hi], $$ :no $$, $2 -- :c\n/* :d */"
        );
        assert_eq!(names, ["id", "name"]);
    }

    #[test]
    fn 아래쪽을_생략한_배열_조각은_자리로_보지_않는다() {
        assert_eq!(
            bind("select a[:n], b[ :m], :id"),
            ("select a[:n], b[ $1], $2".into(), vec!["m".into(), "id".into()])
        );
    }

    #[test]
    fn 번호_자리는_그대로_두고_이름은_가장_큰_번호_다음을_받는다() {
        let (sql, names) = bind("select $2, :a, $1, :b, :a");
        assert_eq!(sql, "select $2, $3, $1, $4, $3");
        assert_eq!(names, ["1", "2", "a", "b"]);
        let (sql, names) = bind("select $1 where x = $1");
        assert_eq!(sql, "select $1 where x = $1");
        assert_eq!(names, ["1"]);
    }

    #[test]
    fn 자리가_없으면_그대로_돌려준다() {
        let input = "select 1 where a = ':b' and $9999999999999999999 is null";
        assert_eq!(bind(input), (input.into(), vec![]));
    }

    #[test]
    fn 붙여_쓴_비교와_한글_이름도_찾는다() {
        let (sql, names) = bind("select * from t where id=:id and 이름 = :이름");
        assert_eq!(sql, "select * from t where id=$1 and 이름 = $2");
        assert_eq!(names, ["id", "이름"]);
    }

    /// Go 퍼즈 시드다. go test 가 시드만 돌리듯 같은 성질을 시드로 본다.
    #[test]
    fn 패닉없이_번호는_이름_수를_넘지_않는다() {
        for input in ["select :a, $2, x::int, ':b'", "::a:b:::c$1:"] {
            let (sql, names) = bind(input);
            assert!(!names.is_empty() || sql == input, "changed without names: {input:?} -> {sql:?}");
            // 바꾼 결과에는 이름 자리가 남지 않아 다시 묶으면 그대로이고 이름은 "1".."N" 이다
            let (again, numbered) = bind(&sql);
            assert!(
                again == sql && numbered.len() == names.len(),
                "rebind {sql:?} -> {again:?}, names {names:?} -> {numbered:?}"
            );
        }
    }

    #[test]
    fn 닫히지_않은_E_문자열은_끝까지_한_토큰이다() {
        let toks = tokenize("e'\\");
        assert_eq!((toks.len(), toks[0].kind, toks[0].end), (1, Kind::String, 3));
    }

    #[test]
    fn 번호_자리를_리터럴로_넣고_문자열_안은_두다() {
        let args = [Some("a'b".to_string()), None, Some("c\\d".to_string())];
        assert_eq!(
            inline_args("select $1, $2, '$1', $3, $4 -- $1", &args),
            "select 'a''b', NULL, '$1', E'c\\\\d', $4 -- $1"
        );
    }
}
