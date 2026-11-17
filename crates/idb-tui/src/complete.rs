//! 자동완성 후보(R8). SQL 은 커서 앞 키워드로 문맥(테이블·열·키워드)을 정하고, 별칭(`o.`)·스키마(`public.`)를 푼다.
//! Redis 는 명령 이름과 트리에 읽힌 키다(새로 SCAN 하지 않는다, R8.2).
//! 단, Go `internal/complete` 의 INSERT 열 목록·하위 명령 같은 세밀한 문맥은 아직 옮기지 않았다.

use idb_engine::Catalog;

use crate::editor::Item;

const SQL_KEYWORDS: &str = "SELECT FROM WHERE AND OR NOT IN IS NULL LIKE ILIKE BETWEEN EXISTS AS ON JOIN LEFT RIGHT INNER OUTER FULL CROSS \
NATURAL LATERAL USING GROUP BY ORDER HAVING LIMIT OFFSET FETCH FIRST NEXT ROWS ONLY DISTINCT ALL ANY SOME UNION \
INTERSECT EXCEPT CASE WHEN THEN ELSE END ASC DESC NULLS LAST INSERT INTO VALUES DEFAULT UPDATE SET DELETE \
RETURNING WITH RECURSIVE MATERIALIZED CONFLICT DO NOTHING TRUNCATE CREATE ALTER DROP TABLE VIEW INDEX UNIQUE \
SEQUENCE SCHEMA DATABASE FUNCTION PROCEDURE TRIGGER TYPE DOMAIN EXTENSION ROLE USER GRANT REVOKE PRIVILEGES \
CONCURRENTLY IF CASCADE RESTRICT CONSTRAINT PRIMARY KEY FOREIGN REFERENCES CHECK COLUMN ADD RENAME TO OWNER \
TEMPORARY TEMP UNLOGGED BEGIN COMMIT ROLLBACK SAVEPOINT RELEASE TRANSACTION ISOLATION LEVEL READ WRITE \
COMMITTED SERIALIZABLE REPEATABLE EXPLAIN ANALYZE VERBOSE BUFFERS COSTS FORMAT VACUUM REINDEX CLUSTER COPY \
SHOW RESET LOCK FOR SHARE NOWAIT SKIP LOCKED WINDOW OVER PARTITION RANGE PRECEDING FOLLOWING UNBOUNDED CURRENT \
ROW FILTER WITHIN TRUE FALSE CAST COLLATE ARRAY SIMILAR ESCAPE INTERVAL TIMESTAMP DATE TIME ZONE AT LOCAL \
CURRENT_DATE CURRENT_TIMESTAMP CURRENT_USER INTEGER BIGINT SMALLINT NUMERIC DECIMAL REAL DOUBLE PRECISION \
BOOLEAN TEXT VARCHAR CHAR UUID JSON JSONB BYTEA SERIAL BIGSERIAL COMMENT NOTIFY LISTEN PREPARE EXECUTE \
DEALLOCATE REFRESH TABLESAMPLE GENERATED ALWAYS IDENTITY STORED OVERRIDING SYSTEM VALUE";

/// 따옴표 없이는 이름이 될 수 없는 낱말(pg_get_keywords 의 R·T)이다.
const RESERVED: &str = "all analyse analyze and any array as asc asymmetric both case cast check collate column constraint create \
current_catalog current_date current_role current_time current_timestamp current_user default deferrable desc \
distinct do else end except false fetch for foreign from grant group having in initially intersect into lateral \
leading limit localtime localtimestamp not null offset on only or order placing primary references returning \
select session_user some symmetric system_user table then to trailing true union unique user using variadic \
when where window with authorization binary collation concurrently cross current_schema freeze full ilike inner is isnull join left \
like natural notnull outer overlaps right similar tablesample verbose";

const REDIS_COMMANDS: &str = "GET SET DEL UNLINK EXISTS TYPE TTL PTTL EXPIRE PEXPIRE EXPIREAT PERSIST RENAME RENAMENX COPY DUMP TOUCH KEYS SCAN \
RANDOMKEY DBSIZE MGET MSET MSETNX SETNX SETEX GETSET GETDEL GETEX APPEND STRLEN GETRANGE SETRANGE INCR INCRBY INCRBYFLOAT DECR DECRBY \
GETBIT SETBIT BITCOUNT HGET HSET HSETNX HMGET HGETALL HDEL HEXISTS HKEYS HVALS HLEN HINCRBY HSCAN LPUSH RPUSH LPOP RPOP LRANGE LLEN \
LINDEX LSET LREM LTRIM LMOVE RPOPLPUSH SADD SREM SMEMBERS SISMEMBER SCARD SPOP SRANDMEMBER SSCAN SINTER SUNION SDIFF SMOVE ZADD ZREM \
ZRANGE ZRANGEBYSCORE ZREVRANGE ZCARD ZSCORE ZRANK ZINCRBY ZCOUNT ZSCAN XADD XRANGE XREVRANGE XLEN XDEL XTRIM PFADD PFCOUNT GEOADD \
GEOPOS PUBLISH SUBSCRIBE WATCH MULTI EXEC DISCARD SELECT PING ECHO INFO TIME EVAL EVALSHA SLOWLOG CLIENT MEMORY CONFIG OBJECT";

const TABLE_WORDS: &[&str] = &["from", "join", "update", "into", "table", "truncate"];
const COLUMN_WORDS: &[&str] = &[
    "select",
    "where",
    "on",
    "by",
    "set",
    "returning",
    "having",
    "and",
    "or",
    "when",
    "then",
    "else",
    "using",
];

pub fn is_sql_keyword(lower: &str) -> bool {
    SQL_KEYWORDS.split(' ').any(|k| k.eq_ignore_ascii_case(lower))
}

/// 소문자 단순 이름이 아니거나 예약어면 따옴표로 감싼다.
pub fn quote_ident(name: &str) -> String {
    let plain = !name.is_empty()
        && !name.as_bytes()[0].is_ascii_digit()
        && !RESERVED.split(' ').any(|w| w == name)
        && name
            .bytes()
            .enumerate()
            .all(|(i, c)| c == b'_' || (c == b'$' && i > 0) || c.is_ascii_digit() || c.is_ascii_lowercase());
    if plain {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

/// prefix 가 소문자면 소문자로, 아니면 대문자로 쓴다.
fn case_like(word: &str, prefix: &str) -> String {
    if !prefix.is_empty() && prefix == prefix.to_lowercase() && prefix != prefix.to_uppercase() {
        word.to_lowercase()
    } else {
        word.to_uppercase()
    }
}

/// 커서 앞 낱말(점·따옴표 포함)이다.
fn typed_word(text: &str, off: usize) -> &str {
    let b = text.as_bytes();
    let mut s = off;
    while s > 0 {
        let c = b[s - 1];
        if c.is_ascii_alphanumeric() || c == b'_' || c == b'.' || c == b'"' || c == b'$' || c >= 0x80 {
            s -= 1;
        } else {
            break;
        }
    }
    while !text.is_char_boundary(s) {
        s += 1;
    }
    &text[s..off]
}

fn unquote(s: &str) -> String {
    let t = s.trim_matches('"');
    if s.starts_with('"') {
        t.replace("\"\"", "\"")
    } else {
        t.to_lowercase()
    }
}

/// 문장 안 FROM·JOIN·UPDATE·INTO 뒤의 테이블과 별칭(없으면 테이블 이름)이다.
fn table_refs(stmt: &str) -> Vec<(String, String, String)> {
    let words: Vec<&str> = stmt
        .split(|c: char| c.is_whitespace() || c == ',' || c == '(' || c == ')' || c == ';')
        .filter(|w| !w.is_empty())
        .collect();
    let mut out = Vec::new();
    for i in 0..words.len() {
        if !TABLE_WORDS.contains(&words[i].to_lowercase().as_str()) {
            continue;
        }
        let Some(name) = words.get(i + 1) else { continue };
        if is_sql_keyword(&name.to_lowercase()) {
            continue;
        }
        let (schema, table) = match name.rsplit_once('.') {
            Some((s, t)) => (unquote(s), unquote(t)),
            None => (String::new(), unquote(name)),
        };
        let mut j = i + 2;
        if words.get(j).is_some_and(|w| w.eq_ignore_ascii_case("as")) {
            j += 1;
        }
        let alias = match words.get(j) {
            Some(a) if !is_sql_keyword(&a.to_lowercase()) => unquote(a),
            _ => table.clone(),
        };
        out.push((schema, table, alias));
    }
    out
}

/// `insert into <테이블> (` 로 연 괄호가 아직 닫히지 않았으면 그 테이블(스키마, 이름)이다.
fn insert_columns_ctx(before: &str) -> Option<(String, String)> {
    let low = before.to_lowercase();
    let at = low.rfind("insert into")?;
    let rest = &before[at + "insert into".len()..];
    let open = rest.find('(')?;
    if rest[open..].contains(')') {
        return None;
    }
    let name = rest[..open].trim();
    if name.is_empty() || name.contains(char::is_whitespace) {
        return None;
    }
    Some(match name.rsplit_once('.') {
        Some((s, t)) => (unquote(s), unquote(t)),
        None => (String::new(), unquote(name)),
    })
}

/// 하위 명령이 있는 Redis 명령이다(Go internal/complete redis.go 와 같은 목록).
const REDIS_SUBS: &[(&str, &[&str])] = &[
    ("CLIENT", &["LIST", "INFO", "GETNAME", "SETNAME", "ID", "KILL", "PAUSE", "UNPAUSE"]),
    ("CONFIG", &["GET", "SET", "RESETSTAT", "REWRITE"]),
    ("SLOWLOG", &["GET", "LEN", "RESET"]),
    ("MEMORY", &["USAGE", "STATS", "DOCTOR", "PURGE"]),
    ("OBJECT", &["ENCODING", "FREQ", "IDLETIME", "REFCOUNT"]),
    ("XINFO", &["STREAM", "GROUPS", "CONSUMERS"]),
    ("SCRIPT", &["LOAD", "EXISTS", "FLUSH", "KILL"]),
    ("CLUSTER", &["INFO", "NODES", "SLOTS", "SHARDS", "KEYSLOT"]),
    ("COMMAND", &["COUNT", "DOCS", "INFO", "LIST"]),
    ("ACL", &["WHOAMI", "LIST", "USERS", "CAT"]),
    ("PUBSUB", &["CHANNELS", "NUMSUB", "NUMPAT"]),
];

/// 키를 받지 않는 명령(이 자리에서 키를 내밀지 않는다)이다.
const REDIS_NO_KEY: &[&str] = &[
    "PING",
    "ECHO",
    "INFO",
    "TIME",
    "DBSIZE",
    "RANDOMKEY",
    "SELECT",
    "MULTI",
    "EXEC",
    "DISCARD",
    "KEYS",
    "SCAN",
    "PUBLISH",
    "SUBSCRIBE",
    "EVAL",
    "EVALSHA",
];

/// SQL 후보다. stmt 는 커서가 든 문장, off 는 문장 안 커서 자리다.
pub fn sql(cat: Option<&Catalog>, stmt: &str, off: usize) -> Vec<Item> {
    let word = typed_word(stmt, off);
    let before = &stmt[..off - word.len()];
    let prev = before
        .split(|c: char| c.is_whitespace() || c == ',' || c == '(')
        .rev()
        .find(|w| !w.is_empty())
        .unwrap_or("")
        .to_lowercase();
    let mut items = Vec::new();
    let Some((qual, prefix)) = word.rsplit_once('.').map(|(a, b)| (Some(a), b)).or(Some((None, word))) else {
        return items;
    };
    let plen = prefix.len();
    let push_cols = |items: &mut Vec<Item>, schema: &str, table: &str| {
        let Some(cat) = cat else { return };
        for s in &cat.schemas {
            if !schema.is_empty() && s.name != schema {
                continue;
            }
            for t in s.tables.iter().filter(|t| t.name == table) {
                for c in &t.columns {
                    items.push(Item {
                        label: c.name.clone(),
                        insert: quote_ident(&c.name),
                        kind: "column",
                        detail: c.typ.clone(),
                        prefix_len: plen,
                    });
                }
            }
        }
    };
    if let Some(q) = qual {
        let q = unquote(q);
        // 별칭·테이블 이름이면 그 열, 스키마 이름이면 그 테이블
        for (schema, table, alias) in table_refs(stmt) {
            if alias == q || table == q {
                push_cols(&mut items, &schema, &table);
            }
        }
        if let Some(cat) = cat {
            for s in cat.schemas.iter().filter(|s| s.name == q) {
                for t in &s.tables {
                    items.push(Item {
                        label: t.name.clone(),
                        insert: quote_ident(&t.name),
                        kind: if t.view { "view" } else { "table" },
                        detail: s.name.clone(),
                        prefix_len: plen,
                    });
                }
            }
        }
        return rank(items, prefix);
    }
    // INSERT INTO t ( … 의 열 목록 안이면 그 테이블 열만이다
    if let Some((schema, table)) = insert_columns_ctx(before) {
        push_cols(&mut items, &schema, &table);
        return rank(items, prefix);
    }
    let table_ctx = TABLE_WORDS.contains(&prev.as_str());
    if let Some(cat) = cat {
        if table_ctx || !COLUMN_WORDS.contains(&prev.as_str()) {
            for s in &cat.schemas {
                for t in &s.tables {
                    let public = s.name == "public";
                    items.push(Item {
                        label: if public { t.name.clone() } else { format!("{}.{}", s.name, t.name) },
                        insert: if public {
                            quote_ident(&t.name)
                        } else {
                            format!("{}.{}", quote_ident(&s.name), quote_ident(&t.name))
                        },
                        kind: if t.view { "view" } else { "table" },
                        detail: s.name.clone(),
                        prefix_len: plen,
                    });
                }
            }
        }
        if !table_ctx {
            for (schema, table, _) in table_refs(stmt) {
                push_cols(&mut items, &schema, &table);
            }
            for f in &cat.functions {
                items.push(Item {
                    label: f.clone(),
                    kind: "function",
                    prefix_len: plen,
                    ..Item::default()
                });
            }
        }
    }
    if !table_ctx {
        for k in SQL_KEYWORDS.split(' ') {
            items.push(Item {
                label: k.to_string(),
                insert: case_like(k, prefix),
                kind: "keyword",
                prefix_len: plen,
                ..Item::default()
            });
        }
    }
    rank(items, prefix)
}

/// Redis 후보다. 줄 첫 낱말이면 명령, 그 뒤면 읽힌 키다.
pub fn redis(line: &str, keys: &[String]) -> Vec<Item> {
    let word_start = line.rfind(char::is_whitespace).map_or(0, |i| i + 1);
    let prefix = &line[word_start..];
    let first = line[..word_start].trim().is_empty();
    let items: Vec<Item> = if first {
        REDIS_COMMANDS
            .split(' ')
            .map(|c| Item {
                label: c.to_string(),
                insert: case_like(c, prefix),
                kind: "command",
                prefix_len: prefix.len(),
                ..Item::default()
            })
            .collect()
    } else {
        let args: Vec<&str> = line[..word_start].split_whitespace().collect();
        let cmd = args[0].to_uppercase();
        if let Some((_, subs)) = REDIS_SUBS.iter().find(|(c, _)| *c == cmd)
            && args.len() == 1
        {
            let items = subs
                .iter()
                .map(|c| Item {
                    label: c.to_string(),
                    insert: case_like(c, prefix),
                    kind: "command",
                    prefix_len: prefix.len(),
                    ..Item::default()
                })
                .collect();
            return rank(items, prefix);
        }
        if REDIS_NO_KEY.contains(&cmd.as_str())
            || (REDIS_SUBS.iter().any(|(c, _)| *c == cmd) && !matches!(cmd.as_str(), "MEMORY" | "OBJECT" | "XINFO"))
        {
            return Vec::new();
        }
        keys.iter()
            .map(|k| Item {
                label: k.clone(),
                kind: "key",
                prefix_len: prefix.len(),
                ..Item::default()
            })
            .collect()
    };
    rank(items, prefix)
}

/// 앞부분이 맞는 것을 먼저, 들어 있기만 한 것을 뒤에 두고 같은 이름은 하나만 둔다. 빈 접두어면 모두다.
fn rank(items: Vec<Item>, prefix: &str) -> Vec<Item> {
    let p = prefix.trim_start_matches('"').to_lowercase();
    let mut seen = std::collections::HashSet::new();
    let (mut head, mut tail) = (Vec::new(), Vec::new());
    for it in items {
        if !seen.insert(it.label.clone()) {
            continue;
        }
        let key = it.label.to_lowercase();
        let short = key.rsplit('.').next().unwrap_or(&key).to_string();
        if key.starts_with(&p) || short.starts_with(&p) {
            head.push(it);
        } else if key.contains(&p) {
            tail.push(it);
        }
    }
    head.extend(tail);
    head.truncate(200);
    head
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cat() -> Catalog {
        serde_json::from_value(
            serde_json::json!({"schemas":[{"name":"public","tables":[{"name":"orders","view":false,
            "columns":[{"name":"id","type":"bigint"},{"name":"userId","type":"text"}]}]}],"functions":["now"]}),
        )
        .unwrap()
    }

    #[test]
    fn 별칭_뒤_점은_그_테이블의_열이고_따옴표가_필요하면_감싼다() {
        let c = cat();
        let s = "select o.u from orders o";
        let items = sql(Some(&c), s, "select o.u".len());
        assert_eq!(items[0].label, "userId");
        assert_eq!(items[0].insert, "\"userId\"");
    }

    #[test]
    fn FROM_뒤는_테이블만_그_밖은_키워드도_소문자로_맞춘다() {
        let c = cat();
        let items = sql(Some(&c), "select * from or", "select * from or".len());
        assert_eq!(items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(), ["orders"]);
        let items = sql(Some(&c), "sel", 3);
        assert!(items.iter().any(|i| i.label == "SELECT" && i.insert == "select"));
    }

    #[test]
    fn INSERT_열_목록과_레디스_하위_명령_문맥() {
        let c = cat();
        let s = "insert into orders (us";
        let items = sql(Some(&c), s, s.len());
        assert_eq!(items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(), ["userId"]);
        assert!(
            sql(Some(&c), "insert into orders (id) values (", 31)
                .iter()
                .any(|i| i.kind == "keyword")
        );
        assert_eq!(redis("SLOWLOG g", &[])[0].label, "GET");
        assert!(redis("PING x", &["x1".into()]).is_empty());
        assert_eq!(redis("MEMORY USAGE u", &["user:1".into()])[0].label, "user:1");
    }

    #[test]
    fn 레디스는_첫_낱말이_명령이고_그_뒤는_읽힌_키다() {
        assert_eq!(redis("hge", &[])[0].label, "HGET");
        let keys = vec!["user:1".to_string(), "solo".to_string()];
        assert_eq!(redis("GET us", &keys)[0].label, "user:1");
    }
}
