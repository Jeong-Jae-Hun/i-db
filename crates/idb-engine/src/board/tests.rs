use super::*;

fn proj(dir: &Path) -> Project {
    let conn = |name: &str, kind: Kind| Conn {
        name: name.into(),
        uuid: name.into(),
        kind: Some(kind),
        ..Default::default()
    };
    Project {
        dir: dir.to_path_buf(),
        conns: vec![
            conn("app-stg", Kind::Pg),
            conn("app-prod", Kind::Pg),
            conn("cache-stg", Kind::Redis),
            conn("logs-stg", Kind::Loki),
            conn("aws-stg", Kind::Aws),
        ],
        mapping: Default::default(),
        warnings: Vec::new(),
        aes_env: Default::default(),
    }
}

fn write_board(dir: &Path, name: &str, body: &str) {
    std::fs::create_dir_all(dir.join(BOARD_DIR)).unwrap();
    std::fs::write(dir.join(BOARD_DIR).join(format!("{name}.json")), body).unwrap();
}

#[test]
fn 패널마다_검사하고_틀린_패널만_설정_오류다() {
    let dir = tempfile::tempdir().unwrap();
    write_board(
        dir.path(),
        "b",
        r#"{"title":"보드","panels":[
        {"title":"ok","conn":"app-stg","view":"number","query":"select 1","warn":">0"},
        {"conn":"app-stg","view":"table","query":"delete from t"},
        {"conn":"app-stg","view":"table","query":"select 1; select 2"},
        {"conn":"cache-stg","view":"number","query":"HGETALL h"},
        {"conn":"cache-stg","view":"number","query":"KEYS *"},
        {"conn":"cache-stg","view":"table","query":"LRANGE l 0 49"},
        {"conn":"aws-stg","view":"table","query":"x"},
        {"conn":"nope","view":"table","query":"x"},
        {"conn":"app-stg","view":"pie","query":"select 1"},
        {"conn":"app-stg","view":"table","query":"select 1","warn":">1"},
        {"conn":"logs-stg","view":"chart","query":"{a=\"b\"}","since":"7d"},
        {"conn":"app-stg","view":"table","query":"select 1","since":"1h"},
        {"view":"flow","flow":"nope"}
    ]}"#,
    );
    let b = load_board(&proj(dir.path()), "b").ok().unwrap();
    assert_eq!(b.title, "보드");
    assert!(b.err.is_empty());
    let errs: Vec<&str> = b.panels.iter().map(|p| p.err.as_str()).collect();
    assert_eq!(errs[0], "");
    assert!(errs[1].starts_with("query is not read-only: "), "{}", errs[1]);
    assert_eq!(errs[2], "query must be a single statement");
    assert_eq!(errs[3], "보드에서는 범위를 정한 읽기만: HGETALL");
    assert_eq!(errs[4], "KEYS is not run on boards (it blocks the server): use SCAN");
    assert_eq!(errs[5], "");
    assert_eq!(errs[6], "conn aws-stg: aws connections are not query panels (use view flow)");
    assert_eq!(errs[7], "conn nope: no such connection");
    assert_eq!(errs[8], r#"view must be one of number|table|chart|flow: "pie""#);
    assert_eq!(errs[9], "warn is only for view number");
    assert_eq!(errs[10], "");
    assert_eq!(b.panels[10].since_ns, 7 * 24 * 3_600_000_000_000);
    assert_eq!(errs[11], "since is only for loki·prometheus·grafana");
    assert_eq!(errs[12], "flow nope: no such flow (.dbtui/flows.json)");
    assert_eq!(b.panels[1].title, "#2");
}

#[test]
fn refresh_는_최소_10초이고_깨진_JSON_은_패널_없이_오류다() {
    let dir = tempfile::tempdir().unwrap();
    let p = proj(dir.path());
    write_board(dir.path(), "fast", r#"{"refresh":"5s","panels":[]}"#);
    assert_eq!(load_board(&p, "fast").ok().unwrap().err, "refresh must be at least 10s: 5s");
    write_board(dir.path(), "bad", r#"{"refresh":"soon"}"#);
    assert_eq!(load_board(&p, "bad").ok().unwrap().err, r#"refresh: invalid duration "soon""#);
    write_board(dir.path(), "broken", "{");
    let b = load_board(&p, "broken").ok().unwrap();
    assert!(b.err.starts_with(".dbtui/boards/broken.json: ") && b.panels.is_empty(), "{}", b.err);
    assert!(matches!(load_board(&p, "none"), Err(LoadErr::NotFound)));
    assert!(matches!(load_board(&p, "../x"), Err(LoadErr::Other(_))));
    assert_eq!(list_boards(dir.path()), ["bad", "broken", "fast"]);
}

#[test]
fn Redis_는_컬렉션_전체를_읽는_명령은_받지_않고_범위를_정한_읽기만_받는다() {
    let a = |s: &str| split_args(s);
    for (cmd, ok) in [
        ("GET k", true),
        ("HGETALL h", false),
        ("SMEMBERS s", false),
        ("LRANGE l 0 -1", false),
        ("LRANGE l 0 49", true),
        ("LRANGE l 0 50", false),
        ("ZRANGE z 0 9 WITHSCORES", true),
        ("LRANGE l 0 9 WITHSCORES", false),
        ("ZREVRANGE z 5 4", false),
        ("SCAN 0 COUNT 50", true),
        ("SCAN 0 COUNT 51", false),
        ("SCAN 0 MATCH a*", true),
        ("SCAN 0 MATCH", false),
    ] {
        assert_eq!(board_redis_bounded(&a(cmd)), ok, "{cmd}");
    }
}

#[test]
fn 경고_조건의_비교와_숫자가_아닌_칸() {
    let w = parse_warn(">= 1,000").unwrap();
    assert!(w.hit("1,000") && w.hit(" 2000 ") && !w.hit("999") && !w.hit("n/a"));
    assert!(parse_warn("!=0").unwrap().hit("1"));
    assert!(parse_warn("=0").unwrap().hit("0"));
    assert!(parse_warn("< -1.5").unwrap().hit("-2"));
    for bad in ["0", ">", ">=x", "> 1.", "~1"] {
        assert!(parse_warn(bad).is_err(), "{bad}");
    }
}

#[test]
fn 같은_접속은_한_묶음이고_설정_오류는_뺀다() {
    let p = |key: &str, err: &str| Panel {
        key: key.into(),
        err: err.into(),
        ..Default::default()
    };
    let panels = [p("a", ""), p("b", ""), p("a", ""), p("c", "bad"), p("b", "")];
    assert_eq!(board_groups(&panels), [vec![0, 2], vec![1, 4]]);
}
