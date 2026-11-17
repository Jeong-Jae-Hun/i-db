//! Redis 값 편집(R34). 화면에서 고른 동작을 엔진 `redis_edit` 요청으로 바꾼다. 어떤 명령을 보낼지와 안전 확인은
//! 엔진이 정하고(`edit_commands`), 여기서는 읽은 값(RawValue)으로 대상·옛값을 채우고 뻔한 입력 오류만 거른다.

use idb_engine::{RawValue, RedisEdit};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    SetString(String),
    HashSet {
        field: String,
        value: String,
    },
    HashDel(String),
    ListPush(String),
    ListSet {
        index: usize,
        value: String,
    },
    ListDel(usize),
    SetAdd(String),
    SetDel(String),
    ZSet {
        member: String,
        score: String,
    },
    ZDel(String),
    /// 초. 비거나 -1 이면 만료를 없앤다
    Ttl(String),
    Rename(String),
    DeleteKey,
}

fn base(raw: &RawValue, op: &str) -> RedisEdit {
    RedisEdit {
        key: raw.key.clone(),
        typ: raw.typ.clone(),
        op: op.into(),
        ..Default::default()
    }
}

/// 잘린 값(일부만 읽음)은 옛값을 확인할 수 없어 칸 편집을 막는다.
pub fn build(raw: &RawValue, a: Action) -> Result<RedisEdit, String> {
    let need = |t: &str| {
        if raw.typ == t {
            Ok(())
        } else {
            Err(format!("{} 키에는 쓸 수 없는 편집이다", raw.typ))
        }
    };
    let mut e;
    match a {
        Action::SetString(v) => {
            need("string")?;
            if raw.truncated {
                return Err("값이 커서 일부만 읽었다. 고칠 수 없다".into());
            }
            e = base(raw, "edit");
            e.old = raw.string.clone();
            e.text = v;
            // SET 은 TTL 을 지운다. TTL 이 있으면 지키게 한다
            e.keep_ttl = raw.ttl_secs > 0;
        }
        Action::HashSet { field, value } => {
            need("hash")?;
            if field.is_empty() {
                return Err("필드 이름이 비었다".into());
            }
            match raw.fields.iter().find(|[f, _]| *f == field) {
                Some([_, old]) => {
                    e = base(raw, "edit");
                    e.old = old.clone();
                }
                None => e = base(raw, "add"),
            }
            e.target = field;
            e.text = value;
        }
        Action::HashDel(field) => {
            need("hash")?;
            e = base(raw, "delete");
            e.target = field;
        }
        Action::ListPush(v) => {
            need("list")?;
            e = base(raw, "add");
            e.text = v;
        }
        Action::ListSet { index, value } => {
            need("list")?;
            let old = raw.items.get(index).ok_or_else(|| format!("{index}번 칸이 없다"))?;
            e = base(raw, "edit");
            e.index = index as i64;
            e.old = old.clone();
            e.text = value;
        }
        Action::ListDel(index) => {
            need("list")?;
            let old = raw.items.get(index).ok_or_else(|| format!("{index}번 칸이 없다"))?;
            e = base(raw, "delete");
            e.index = index as i64;
            e.old = old.clone();
        }
        Action::SetAdd(m) => {
            need("set")?;
            e = base(raw, "add");
            e.text = m;
        }
        Action::SetDel(m) => {
            need("set")?;
            e = base(raw, "delete");
            e.target = m;
        }
        Action::ZSet { member, score } => {
            need("zset")?;
            if score.trim().parse::<f64>().is_err() {
                return Err("점수는 수여야 한다".into());
            }
            e = base(
                raw,
                if raw.zmembers.iter().any(|[m, _]| *m == member) {
                    "edit"
                } else {
                    "add"
                },
            );
            e.target = member;
            e.text = score.trim().to_string();
        }
        Action::ZDel(m) => {
            need("zset")?;
            e = base(raw, "delete");
            e.target = m;
        }
        Action::Ttl(s) => {
            let s = s.trim();
            if !(s.is_empty() || s == "-1" || s.parse::<i64>().is_ok_and(|n| n > 0)) {
                return Err("TTL 은 양의 초, 또는 비우거나 -1(만료 없음)".into());
            }
            e = base(raw, "ttl");
            e.text = s.to_string();
        }
        Action::Rename(n) => {
            if n.trim().is_empty() {
                return Err("새 키 이름이 비었다".into());
            }
            e = base(raw, "rename");
            e.text = n;
        }
        Action::DeleteKey => e = base(raw, "delete_key"),
    }
    Ok(e)
}

pub fn ttl_label(secs: i64) -> String {
    match secs {
        -2 => "키 없음".into(),
        s if s < 0 => "만료 없음".into(),
        s if s >= 86_400 => format!("{}일 {}시간", s / 86_400, (s % 86_400) / 3600),
        s if s >= 3600 => format!("{}시간 {}분", s / 3600, (s % 3600) / 60),
        s if s >= 60 => format!("{}분 {}초", s / 60, s % 60),
        s => format!("{s}초"),
    }
}

/// 패턴 일괄 삭제(R42) 확인 글이다.
pub fn unlink_question(pattern: &str, n: usize, truncated: bool) -> String {
    if truncated {
        format!("{pattern} 에 맞는 키가 {n}개를 넘는다. 처음 {n}개를 지울까?")
    } else {
        format!("{pattern} 에 맞는 키 {n}개를 지울까?")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(typ: &str) -> RawValue {
        serde_json::from_value(serde_json::json!({
            "key": "k", "type": typ, "ttl_secs": 30, "string": "old",
            "fields": [["f", "v"]], "items": ["a", "b"], "members": ["m"], "zmembers": [["z", "1"]], "truncated": false
        }))
        .unwrap()
    }

    #[test]
    fn 문자열은_옛값을_싣고_TTL_이_있으면_지킨다() {
        let e = build(&raw("string"), Action::SetString("new".into())).unwrap();
        assert_eq!(
            (e.op.as_str(), e.old.as_str(), e.text.as_str(), e.keep_ttl),
            ("edit", "old", "new", true)
        );
        let mut r = raw("string");
        r.truncated = true;
        assert!(build(&r, Action::SetString("x".into())).is_err());
    }

    #[test]
    fn 해시는_있는_필드면_고치고_없으면_더한다() {
        let e = build(
            &raw("hash"),
            Action::HashSet {
                field: "f".into(),
                value: "w".into(),
            },
        )
        .unwrap();
        assert_eq!((e.op.as_str(), e.target.as_str(), e.old.as_str()), ("edit", "f", "v"));
        let e = build(
            &raw("hash"),
            Action::HashSet {
                field: "g".into(),
                value: "w".into(),
            },
        )
        .unwrap();
        assert_eq!(e.op, "add");
        assert!(
            build(
                &raw("hash"),
                Action::HashSet {
                    field: "".into(),
                    value: "w".into()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn 리스트_칸은_읽은_값으로_확인하고_없는_칸은_막는다() {
        let e = build(
            &raw("list"),
            Action::ListSet {
                index: 1,
                value: "c".into(),
            },
        )
        .unwrap();
        assert_eq!((e.index, e.old.as_str()), (1, "b"));
        assert!(build(&raw("list"), Action::ListDel(5)).is_err());
        assert_eq!(build(&raw("list"), Action::ListDel(0)).unwrap().old, "a");
    }

    #[test]
    fn 종류가_다른_편집과_잘못된_입력은_막는다() {
        assert!(build(&raw("set"), Action::HashDel("f".into())).is_err());
        assert!(
            build(
                &raw("zset"),
                Action::ZSet {
                    member: "z".into(),
                    score: "abc".into()
                }
            )
            .is_err()
        );
        assert_eq!(
            build(
                &raw("zset"),
                Action::ZSet {
                    member: "z".into(),
                    score: " 2 ".into()
                }
            )
            .unwrap()
            .op,
            "edit"
        );
        assert!(build(&raw("string"), Action::Ttl("0".into())).is_err());
        assert_eq!(build(&raw("string"), Action::Ttl("".into())).unwrap().op, "ttl");
        assert!(build(&raw("string"), Action::Rename(" ".into())).is_err());
        assert_eq!(build(&raw("hash"), Action::DeleteKey).unwrap().op, "delete_key");
    }

    #[test]
    fn TTL_글과_패턴_삭제_질문() {
        assert_eq!(ttl_label(-1), "만료 없음");
        assert_eq!(ttl_label(-2), "키 없음");
        assert_eq!(ttl_label(90), "1분 30초");
        assert_eq!(ttl_label(90_000), "1일 1시간");
        assert!(unlink_question("user:*", 3, false).contains("3개를 지울까"));
        assert!(unlink_question("user:*", 100000, true).contains("넘는다"));
    }
}
