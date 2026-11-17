//! 이메일·uid·id·닉네임 하나로 여러 DB 의 사용자 행을 한 번에 찾는 조회 묶음을 만든다(명세 R51). Go `internal/user360` 과 같다.
//! 조회 목록은 프로젝트의 .dbtui/user360.json 이고, 없으면 빈 목록이다. 이 모듈은 SQL·인자만 만든다.

use std::fs;
use std::io;
use std::path::Path;

use serde::Deserialize;

use crate::gojson::json_error;
use crate::params::{bind, replace_params};

/// 프로젝트 폴더 기준 설정 파일 자리다. 테이블·열 이름만 들어 레포에 커밋한다.
pub const FILE: &str = ".dbtui/user360.json";

const KIND_EMAIL: &str = "email";
const KIND_UID: &str = "uid";
const KIND_ID: &str = "id";
const KIND_NICKNAME: &str = "nickname";
/// 이메일을 그 접속 그룹 키로 암호화한 값(staticEncrypt 결과)이다
const PARAM_EMAIL_ENC: &str = "email_enc";

const KINDS: [&str; 4] = [KIND_EMAIL, KIND_UID, KIND_ID, KIND_NICKNAME];
/// SQL 에 쓸 수 있는 :이름. 입력 종류와 email_enc 다
const PARAM_NAMES: [&str; 5] = [KIND_EMAIL, KIND_UID, KIND_ID, KIND_NICKNAME, PARAM_EMAIL_ENC];

/// 조회 하나다. conn 은 접속 이름이고, by 의 입력 종류 중 하나라도 입력과 맞을 때만 돈다.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Lookup {
    pub title: String,
    pub conn: String,
    pub by: Vec<String>,
    pub sql: String,
}

/// Go json 처럼 null 칸은 빈 값으로 읽는다.
#[derive(Deserialize)]
struct RawLookup {
    title: Option<String>,
    conn: Option<String>,
    by: Option<Vec<String>>,
    sql: Option<String>,
}

/// 실행할 문장 하나다. args[k-1] 은 SQL 의 $k 값(None 은 NULL)이다. err 가 있으면 실행하지 말고 그 칸에 오류를 보인다.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Query {
    pub title: String,
    pub conn: String,
    pub sql: String,
    pub args: Vec<Option<String>>,
    pub err: Option<String>,
}

/// 암호화 결과다. NoKey 면 그 접속의 클라이언트 키·IV 가 없어 서버 staticEncrypt 로 감싼다.
pub enum Encrypted {
    Ok(String),
    NoKey,
    Err(String),
}

/// projectDir 의 설정을 읽는다. 파일이 없으면 빈 목록이다.
/// 항목마다 제목·접속·종류·SQL 이 있는지, 종류와 :이름이 알려진 것인지 검사하고 틀리면 몇 번째 항목인지 알린다.
pub fn load(project_dir: &Path) -> Result<Vec<Lookup>, String> {
    let data = match fs::read(project_dir.join(FILE)) {
        Ok(d) => d,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.to_string()),
    };
    let raw: Vec<RawLookup> = serde_json::from_slice(&data).map_err(|e| format!("{FILE}: {}", json_error(&e)))?;
    let ls: Vec<Lookup> = raw
        .into_iter()
        .map(|r| Lookup {
            title: r.title.unwrap_or_default(),
            conn: r.conn.unwrap_or_default(),
            by: r.by.unwrap_or_default(),
            sql: r.sql.unwrap_or_default(),
        })
        .collect();
    for (i, l) in ls.iter().enumerate() {
        validate(l).map_err(|e| format!("{FILE}: lookup {}: {e}", i + 1))?;
    }
    Ok(ls)
}

fn validate(l: &Lookup) -> Result<(), String> {
    if l.title.trim().is_empty() {
        return Err("title is empty".into());
    }
    if l.conn.trim().is_empty() {
        return Err("conn is empty".into());
    }
    if l.sql.trim().is_empty() {
        return Err("sql is empty".into());
    }
    if l.by.is_empty() {
        return Err("by is empty".into());
    }
    if let Some(k) = l.by.iter().find(|k| !KINDS.contains(&k.as_str())) {
        return Err(format!(
            "unknown kind {} in by (want one of {})",
            crate::project::go_quote(k),
            KINDS.join(", ")
        ));
    }
    let (_, names) = bind(&l.sql);
    if let Some(n) = names.iter().find(|n| !PARAM_NAMES.contains(&n.as_str())) {
        return Err(format!("unknown parameter {} (want :{})", param_label(n), PARAM_NAMES.join(", :")));
    }
    Ok(())
}

/// 오류에 보일 자리표시다. bind 는 $1 을 이름 "1" 로 준다
fn param_label(name: &str) -> String {
    if crate::project::go_atoi(name).is_some() {
        format!("${name}")
    } else {
        format!(":{name}")
    }
}

/// 앞뒤 공백을 뗀 입력의 종류와 값이다. '@' 가 있으면 email(대소문자는 입력 그대로), 숫자만이고 bigint 에 들면 id,
/// uuid 꼴이면 uid(소문자로), 그 밖은 nickname 이다. 빈 입력은 None 이다.
/// 이메일을 소문자로 바꾸지 않는 까닭: 암호문은 글자가 하나만 달라도 달라서, 저장한 쪽이 소문자로 맞췄는지 모르는 채 바꾸면
/// 맞던 값도 놓친다. 입력한 그대로 찾고, 대소문자가 다르게 저장됐으면 사용자가 그 꼴로 다시 친다.
fn classify(input: &str) -> Option<(&'static str, String)> {
    let v = input.trim();
    if v.is_empty() {
        return None;
    }
    if v.contains('@') {
        return Some((KIND_EMAIL, v.to_string()));
    }
    if v.bytes().all(|b| b.is_ascii_digit()) && v.parse::<i64>().is_ok() {
        return Some((KIND_ID, v.to_string()));
    }
    if is_uuid(v) {
        return Some((KIND_UID, v.to_ascii_lowercase()));
    }
    Some((KIND_NICKNAME, v.to_string()))
}

/// 8-4-4-4-12 꼴 16진수인지다.
pub fn is_uuid(v: &str) -> bool {
    let b = v.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// 입력 종류와 by 가 겹치는 조회마다 :이름을 $n 으로 바꾼 문장과 인자를 만든다. 입력이 이메일이면
/// :email_enc 는 encrypt(접속, 이메일) 값이다. NoKey 면 그 자리를 staticEncrypt($n) 으로 감싸고 인자로 이메일을 넘긴다.
/// 그 밖의 암호화 오류는 Query.err 다. 순서는 lookups 순서다.
pub fn plan(lookups: &[Lookup], input: &str, encrypt: &dyn Fn(&str, &str) -> Encrypted) -> Vec<Query> {
    let Some((kind, value)) = classify(input) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for l in lookups.iter().filter(|l| l.by.iter().any(|k| k == kind)) {
        let mut q = Query {
            title: l.title.clone(),
            conn: l.conn.clone(),
            ..Default::default()
        };
        let (sql, names) = bind(&l.sql);
        q.args = vec![None; names.len()];
        // staticEncrypt(...) 로 감쌀 $n
        let mut server: Vec<i64> = Vec::new();
        for (k, name) in names.iter().enumerate() {
            if !PARAM_NAMES.contains(&name.as_str()) {
                q.err = Some(format!("unknown parameter {}", param_label(name)));
                break;
            }
            if name != PARAM_EMAIL_ENC {
                if name == kind {
                    q.args[k] = Some(value.clone());
                }
                continue;
            }
            if kind != KIND_EMAIL {
                continue;
            }
            match encrypt(&l.conn, &value) {
                Encrypted::NoKey => {
                    q.args[k] = Some(value.clone());
                    server.push(k as i64 + 1);
                }
                Encrypted::Err(e) => q.err = Some(format!("encrypt email for {}: {e}", l.conn)),
                Encrypted::Ok(enc) => q.args[k] = Some(enc),
            }
        }
        q.sql = if server.is_empty() {
            sql
        } else {
            replace_params(&sql, |n, text| server.contains(&n).then(|| format!("staticEncrypt({text})")))
        };
        if q.err.is_some() {
            q.args = Vec::new();
        }
        out.push(q);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(title: &str, conn: &str, by: &[&str], sql: &str) -> Lookup {
        Lookup {
            title: title.into(),
            conn: conn.into(),
            by: by.iter().map(|s| s.to_string()).collect(),
            sql: sql.into(),
        }
    }

    fn enc_for(conn: &str, plain: &str) -> Encrypted {
        Encrypted::Ok(format!("ENC[{conn}:{plain}]"))
    }

    #[test]
    fn 입력_꼴로_종류를_가른다() {
        let cases: &[(&str, Option<(&str, &str)>)] = &[
            ("  a@b.com ", Some((KIND_EMAIL, "a@b.com"))),
            ("1315", Some((KIND_ID, "1315"))),
            ("12345678901234567890", Some((KIND_NICKNAME, "12345678901234567890"))),
            ("9223372036854775807", Some((KIND_ID, "9223372036854775807"))),
            ("9999999999999999999", Some((KIND_NICKNAME, "9999999999999999999"))),
            ("A.Kim@Example.com", Some((KIND_EMAIL, "A.Kim@Example.com"))),
            (
                "6F1C2A3B-4D5E-4F60-8A9B-0C1D2E3F4A5B",
                Some((KIND_UID, "6f1c2a3b-4d5e-4f60-8a9b-0c1d2e3f4a5b")),
            ),
            (
                "6f1c2a3b-4d5e-4f60-8a9b-0c1d2e3f4a5",
                Some((KIND_NICKNAME, "6f1c2a3b-4d5e-4f60-8a9b-0c1d2e3f4a5")),
            ),
            ("닉네임", Some((KIND_NICKNAME, "닉네임"))),
            ("12a", Some((KIND_NICKNAME, "12a"))),
            ("+12", Some((KIND_NICKNAME, "+12"))),
            ("   ", None),
        ];
        for (input, want) in cases {
            let got = classify(input);
            assert_eq!(got.as_ref().map(|(k, v)| (*k, v.as_str())), *want, "{input:?}");
        }
    }

    #[test]
    fn 종류가_겹치는_조회만_골라_이름을_번호로_바꾼다() {
        let ls = [
            lookup(
                "a",
                "auth-prod",
                &[KIND_EMAIL, KIND_UID],
                "select * from users where email = :email_enc or uid = :uid or :email = 'x'",
            ),
            lookup(
                "b",
                "app-prod",
                &[KIND_UID, KIND_ID],
                "select * from t where sub = :uid or user_id = :id or id = :id",
            ),
            lookup("c", "app-prod", &[KIND_NICKNAME], "select :nickname"),
        ];
        let got = plan(&ls, " a@b.com", &enc_for);
        assert_eq!(
            got,
            [Query {
                title: "a".into(),
                conn: "auth-prod".into(),
                sql: "select * from users where email = $1 or uid = $2 or $3 = 'x'".into(),
                args: vec![Some("ENC[auth-prod:a@b.com]".into()), None, Some("a@b.com".into())],
                err: None,
            }]
        );
        let got = plan(&ls, "1315", &enc_for);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].sql, "select * from t where sub = $1 or user_id = $2 or id = $2");
        assert_eq!(got[0].args, [None, Some("1315".into())]);

        // uid 입력이면 email_enc 는 암호화하지 않고 NULL 이다
        let called = std::cell::Cell::new(false);
        let got = plan(&ls, "6f1c2a3b-4d5e-4f60-8a9b-0c1d2e3f4a5b", &|_, _| {
            called.set(true);
            Encrypted::Ok(String::new())
        });
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].args[..2], [None, Some("6f1c2a3b-4d5e-4f60-8a9b-0c1d2e3f4a5b".into())]);
        assert!(!called.get());
        assert!(plan(&ls, "  ", &enc_for).is_empty());
    }

    #[test]
    fn 키가_없으면_서버_staticEncrypt_로_감싸고_다른_오류는_칸_오류다() {
        let ls = [lookup(
            "a",
            "shop-prod",
            &[KIND_EMAIL],
            "select ':email_enc', email from users where email = :email_enc or alt = :email_enc -- :email_enc",
        )];
        let got = plan(&ls, "a@b.com", &|_, _| Encrypted::NoKey);
        assert_eq!(
            got[0].sql,
            "select ':email_enc', email from users where email = staticEncrypt($1) or alt = staticEncrypt($1) -- :email_enc"
        );
        assert_eq!(got[0].args, [Some("a@b.com".into())]);
        assert_eq!(got[0].err, None);
        let got = plan(&ls, "a@b.com", &|_, _| Encrypted::Err("bad key".into()));
        assert_eq!(got[0].err.as_deref(), Some("encrypt email for shop-prod: bad key"));
        assert!(got[0].args.is_empty());
    }

    #[test]
    fn 예시_설정은_검사를_통과하며_읽기_전용_LIMIT_50_이다() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".dbtui")).unwrap();
        let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/user360.json");
        fs::copy(example, dir.path().join(FILE)).unwrap();
        let ls = load(dir.path()).unwrap();
        assert!(!ls.is_empty());
        for l in &ls {
            let s = l.sql.to_lowercase();
            assert!(
                s.starts_with("select ") && s.ends_with(" limit 50") && !s.contains(';'),
                "{}",
                l.sql
            );
        }
        for input in ["a@b.com", "6f1c2a3b-4d5e-4f60-8a9b-0c1d2e3f4a5b", "1315", "nick"] {
            assert!(!plan(&ls, input, &enc_for).is_empty(), "{input}");
        }
    }

    #[test]
    fn 파일이_없으면_빈_목록이고_틀리면_항목을_알린다() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()).unwrap(), []);
        fs::create_dir_all(dir.path().join(".dbtui")).unwrap();
        let write = |body: &str| fs::write(dir.path().join(FILE), body).unwrap();
        write(r#"[{"title":"x","conn":"auth-stg","by":["email"],"sql":"select * from users where email = :email_enc"}]"#);
        assert_eq!(
            load(dir.path()).unwrap(),
            [lookup("x", "auth-stg", &["email"], "select * from users where email = :email_enc")]
        );
        write("[]");
        assert_eq!(load(dir.path()).unwrap(), []);
        for body in [
            "[{",
            r#"[{"conn":"c","by":["id"],"sql":"select 1"}]"#,
            r#"[{"title":"t","by":["id"],"sql":"select 1"}]"#,
            r#"[{"title":"t","conn":"c","sql":"select 1"}]"#,
            r#"[{"title":"t","conn":"c","by":["phone"],"sql":"select 1"}]"#,
            r#"[{"title":"t","conn":"c","by":["id"]}]"#,
            r#"[{"title":"t","conn":"c","by":["id"],"sql":"select :phone"}]"#,
            r#"[{"title":"t","conn":"c","by":["id"],"sql":"select $1"}]"#,
        ] {
            write(body);
            let err = load(dir.path()).unwrap_err();
            assert!(err.contains(FILE), "{body}: {err}");
        }
        write(r#"[{"title":"t","conn":"c","by":["id"],"sql":"select 1"},{"title":"t","conn":"c","by":["id"],"sql":"select :phone"}]"#);
        let err = load(dir.path()).unwrap_err();
        assert!(err.contains("lookup 2") && err.contains(":phone"), "{err}");
    }
}
