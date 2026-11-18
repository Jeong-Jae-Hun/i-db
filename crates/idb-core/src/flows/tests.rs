use super::*;

fn conns() -> Vec<Conn> {
    let c = |name: &str, kind: Option<Kind>| Conn {
        name: name.into(),
        kind,
        ..Default::default()
    };
    vec![
        c("shop-stg", Some(Kind::Pg)),
        c("aws-stg", Some(Kind::Aws)),
        c("cache", Some(Kind::Redis)),
        Conn {
            uuid: "gone".into(),
            ..Default::default()
        },
    ]
}

fn write_flows(body: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".dbtui")).unwrap();
    fs::write(dir.path().join(FILE), body).unwrap();
    dir
}

#[test]
fn 파일이_없으면_빈_목록이고_깨진_JSON_은_오류다() {
    assert!(load(tempfile::tempdir().unwrap().path(), &conns()).unwrap().is_empty());
    let err = load(write_flows(r#"[{"name":"#).path(), &conns()).unwrap_err();
    assert!(err.contains(".dbtui/flows.json"), "{err}");
}

#[test]
fn 틀린_항목만_까닭을_적고_나머지는_그대로_쓴다() {
    let dir = write_flows(
        r#"[
		{"name":"ok","outbox":{"conn":"shop-stg","table":"public.t_outbox","type":"event_type","retry":"retry_count","due":"check_needed_at","where":"event_type like 'DATA_%'"},"aws":"aws-stg","topic":"stg-hub","queues":[{"name":"q1","dlq":"q1-dlq"},{"name":"arn:aws:sqs:ap-northeast-2:1:q2"}]},
		{"name":"topic-only","topic":"arn:aws:sns:ap-northeast-2:1:t"},
		{"name":"redis","outbox":{"conn":"cache","table":"t","type":"x"},"topic":"t"},
		{"name":"pg-as-aws","aws":"shop-stg","queues":[{"name":"q"}]},
		{"name":"no-conn","aws":"nope","queues":[{"name":"q"}]},
		{"name":"no-topic","aws":"aws-stg"},
		{"name":"two-stmt","outbox":{"conn":"shop-stg","table":"t","type":"x","where":"1=1; delete from t"},"topic":"t"},
		{"name":"no-type","outbox":{"conn":"shop-stg","table":"t"},"topic":"t"},
		{"name":"deep","outbox":{"conn":"shop-stg","table":"a.b.c","type":"x"},"topic":"t"},
		{"name":"ok","topic":"t"},
		{"topic":"t"}
	]"#,
    );
    let fs = load(dir.path(), &conns()).unwrap();
    let want = [
        "",
        "",
        "outbox.conn cache: want kind postgresql, got redis",
        "aws shop-stg: want kind aws, got postgresql",
        "aws nope: no such connection",
        "outbox, topic or queues is required",
        "outbox.where must be a single condition without ';'",
        "outbox.type is empty",
        r#"outbox.table must be table or schema.table: "a.b.c""#,
        "duplicate name ok",
        "name is empty",
    ];
    assert_eq!(fs.len(), want.len());
    for (f, w) in fs.iter().zip(want) {
        assert_eq!(f.err, w, "{}", f.name);
    }
    assert_eq!(fs[0].outbox.as_ref().unwrap().due, "check_needed_at");
    assert!(fs[1].outbox.is_none());
    assert_eq!(
        fs[0].queues,
        [
            Queue {
                name: "q1".into(),
                dlq: "q1-dlq".into()
            },
            Queue {
                name: "arn:aws:sqs:ap-northeast-2:1:q2".into(),
                dlq: String::new()
            }
        ]
    );
    assert_eq!(fs[10].name, "#11");
}

#[test]
fn where_의_문자열_주석_안_세미콜론은_받는다() {
    for (w, ok) in [
        ("", true),
        ("event_type like 'a;b'", true),
        ("x = 1 -- 끝;", true),
        ("x = 1;", false),
        ("x = 1; select 1", false),
        ("x = 1 /* ; */ and y = $$;$$", true),
        ("x = 1;\n-- trailing comment\n", false),
    ] {
        assert_eq!(check_where(w).is_ok(), ok, "{w}");
    }
}

#[test]
fn 토픽_큐_없는_아웃박스_흐름과_aws_큐_한도_설정을_검사한다() {
    let dir = write_flows(
        r#"[
		{"name":"outbox-only","outbox":{"conn":"shop-stg","table":"public.t_outbox","type":"event_type","retry":"retry_count","due":"check_needed_at","limit":5,
			"types":{"CREATE_PAYOUT":{"limit":20,"consumer":"consume-outbox-create-payout"},"CLIP":{"consumer":"consume-outbox-clip"}}}},
		{"name":"aws-no-queues","outbox":{"conn":"shop-stg","table":"t","type":"x"},"aws":"aws-stg","topic":"t"},
		{"name":"queues-no-aws","topic":"t","queues":[{"name":"q"}]},
		{"name":"queue-no-name","aws":"aws-stg","queues":[{"name":"q"},{"dlq":"d"}]},
		{"name":"nothing"},
		{"name":"queues-only","aws":"aws-stg","queues":[{"name":"q"}]},
		{"name":"zero-limit","outbox":{"conn":"shop-stg","table":"t","type":"x","retry":"r","limit":0}},
		{"name":"type-zero","outbox":{"conn":"shop-stg","table":"t","type":"x","retry":"r","types":{"A":{"limit":0}}}},
		{"name":"empty-type","outbox":{"conn":"shop-stg","table":"t","type":"x","retry":"r","types":{" ":{"consumer":"c"}}}},
		{"name":"limit-no-retry","outbox":{"conn":"shop-stg","table":"t","type":"x","limit":3}},
		{"name":"types-no-retry","outbox":{"conn":"shop-stg","table":"t","type":"x","types":{"A":{"consumer":"c"}}}},
		{"name":"type-stale-zero","outbox":{"conn":"shop-stg","table":"t","type":"x","retry":"r","due":"d","types":{"A":{"stale":0}}}},
		{"name":"type-stale-no-due","outbox":{"conn":"shop-stg","table":"t","type":"x","retry":"r","types":{"A":{"stale":70}}}}
	]"#,
    );
    let fs = load(dir.path(), &conns()).unwrap();
    let want = [
        "",
        "aws requires queues",
        "aws is empty",
        "queues[1].name is empty",
        "outbox, topic or queues is required",
        "",
        "outbox.limit must be >= 1",
        "outbox.types.A.limit must be >= 1",
        "outbox.types has an empty type",
        "outbox.limit requires outbox.retry",
        "outbox.types requires outbox.retry",
        "outbox.types.A.stale must be >= 1",
        "outbox.types.A.stale requires outbox.due",
    ];
    for (f, w) in fs.iter().zip(want) {
        assert_eq!(f.err, w, "{}", f.name);
    }
    let o = fs[0].outbox.as_ref().unwrap();
    assert_eq!(o.limit(), 5);
    assert_eq!(o.types["CREATE_PAYOUT"].limit, Some(20));
    assert_eq!(o.types["CLIP"].limit, None);
    assert_eq!(o.types["CLIP"].consumer, "consume-outbox-clip");
    assert_eq!(o.types["CLIP"].stale, None);
    assert_eq!(Outbox::default().limit(), DEFAULT_LIMIT);
}

#[test]
fn 수_질의는_종류별로_묶고_식별자를_감싸고_where_를_AND_괄호로_붙인다() {
    let o = Outbox {
        table: r#"public.t"x"#.into(),
        kind: "event_type".into(),
        retry: "retry_count".into(),
        due: "check_needed_at".into(),
        filter: "event_type like 'DATA_%' -- 주석".into(),
        ..Default::default()
    };
    let (r, live) = (r#"coalesce("retry_count", 0)"#, r#"coalesce("retry_count", 0) < 5"#);
    let want = format!(
        r#"select "event_type"::text as type, count(*) as rows, count(*) filter (where {live}) as waiting, count(*) filter (where {r} > 0 and {live}) as retrying, count(*) filter (where {r} >= 5) as exhausted, count(*) filter (where "check_needed_at" < now() - interval '5 minutes' and {live}) as stale, min("check_needed_at") filter (where {live}) as oldest_due, "event_type"::text is null as type_null, grouping("event_type"::text) as total from "public"."t""x" where (
event_type like 'DATA_%' -- 주석
) group by grouping sets (("event_type"::text), ()) order by 9 desc, 5 desc nulls last, 3 desc, 1"#
    );
    assert_eq!(count_sql(&o), want);
    let min = r#"select "x"::text as type, count(*) as rows, count(*) as waiting, null::bigint as retrying, null::bigint as exhausted, null::bigint as stale, null::timestamptz as oldest_due, "x"::text is null as type_null, grouping("x"::text) as total from "t_outbox" group by grouping sets (("x"::text), ()) order by 9 desc, 5 desc nulls last, 3 desc, 1"#;
    let o2 = Outbox {
        table: "t_outbox".into(),
        kind: "x".into(),
        ..Default::default()
    };
    assert_eq!(count_sql(&o2), min);
    assert!(crate::sqlsafe::is_read_only_sql(&count_sql(&o)));
    assert!(crate::sqlsafe::is_read_only_sql(&retry_sql(
        &o,
        &TypeStat {
            name: "A".into(),
            ..Default::default()
        }
    )));
}

#[test]
fn 종류별_한도는_리터럴을_감싼_CASE_다() {
    let tc = |limit: Option<i64>, consumer: &str| TypeConf {
        limit,
        consumer: consumer.into(),
        stale: None,
    };
    let o = Outbox {
        table: "t".into(),
        kind: "event_type".into(),
        retry: "retry_count".into(),
        limit: Some(3),
        types: BTreeMap::from([
            ("O'BRIEN".to_string(), tc(Some(20), "")),
            (r"A\B".to_string(), tc(Some(7), "")),
            ("CLIP".to_string(), tc(None, "c")),
        ]),
        ..Default::default()
    };
    let want = r#"case "event_type"::text when E'A\\B' then 7 when 'O''BRIEN' then 20 else 3 end"#;
    assert_eq!(limit_expr(&o), want);
    assert_eq!(count_sql(&o).matches(want).count(), 3);
    let o2 = Outbox {
        retry: "r".into(),
        types: BTreeMap::from([("CLIP".to_string(), tc(None, "c"))]),
        ..Default::default()
    };
    assert_eq!(limit_expr(&o2), "5");
}

#[test]
fn 재시도_질의는_종류_하나의_행을_where_와_AND_로_묻는다() {
    let o = Outbox {
        table: "public.t_outbox".into(),
        kind: "event_type".into(),
        retry: "retry_count".into(),
        due: "check_needed_at".into(),
        filter: "x = 1".into(),
        ..Default::default()
    };
    let t = |name: &str, null: bool| TypeStat {
        name: name.into(),
        null,
        ..Default::default()
    };
    assert_eq!(
        retry_sql(&o, &t("O'B", false)),
        "select * from \"public\".\"t_outbox\" where \"event_type\"::text = 'O''B' and coalesce(\"retry_count\", 0) > 0 and (\nx = 1\n) order by coalesce(\"retry_count\", 0) desc, \"check_needed_at\" limit 50"
    );
    let min = Outbox {
        table: "t".into(),
        kind: "x".into(),
        ..Default::default()
    };
    assert_eq!(
        retry_sql(&min, &t("A", false)),
        r#"select * from "t" where "x"::text = 'A' limit 50"#
    );
    assert_eq!(
        retry_sql(&min, &t("NULL", true)),
        r#"select * from "t" where "x"::text is null limit 50"#
    );
}

#[test]
fn 수치는_준_열만_보이고_재시도_한도_소진_오래됨이_있으면_경고다() {
    let st = |pending, retry, stale, exhausted| Stat {
        pending,
        retry,
        stale,
        exhausted,
        ..Default::default()
    };
    for (s, want, warn) in [
        (st(120, 3, 1, 2), "대기 120 · 재시도 3 · 한도 소진 2 · 오래됨 1", true),
        (st(5, 0, 0, 0), "대기 5 · 재시도 0 · 한도 소진 0 · 오래됨 0", false),
        (st(5, -1, -1, -1), "대기 5", false),
        (st(5, -1, 2, -1), "대기 5 · 오래됨 2", true),
        (st(5, 0, 0, 1), "대기 5 · 재시도 0 · 한도 소진 1 · 오래됨 0", true),
    ] {
        assert_eq!(s.metrics(), (want.to_string(), warn));
    }
    let ty = |waiting, retrying, exhausted, stale| TypeStat {
        waiting,
        retrying,
        exhausted,
        stale,
        ..Default::default()
    };
    for (t, want, warn) in [
        (ty(3, 1, 2, 0), "대기 3 · 재시도 1 · 한도 소진 2", true),
        (ty(3, 1, 0, 0), "대기 3 · 재시도 1 · 한도 소진 0", false),
        (ty(3, 0, 0, 1), "대기 3 · 재시도 0 · 한도 소진 0 · 오래됨 1", true),
        (ty(3, -1, -1, -1), "대기 3", false),
    ] {
        assert_eq!(t.metrics(), (want.to_string(), warn));
    }
}

#[test]
fn 수_결과의_합계_줄과_NULL_종류를_읽는다() {
    let row = |v: [&str; 9]| v.map(String::from).to_vec();
    let res = QueryResult {
        rows: vec![
            row(["NULL", "10", "8", "1", "2", "NULL", "NULL", "t", "1"]),
            row(["A", "6", "5", "1", "1", "NULL", "", "f", "0"]),
            row(["NULL", "4", "3", "0", "1", "NULL", "", "t", "0"]),
        ],
        truncated: true,
        ..Default::default()
    };
    let s = parse_count(&res).unwrap();
    assert_eq!((s.pending, s.retry, s.exhausted, s.stale, s.truncated), (8, 1, 2, -1, true));
    assert_eq!(s.types.len(), 2);
    assert!(!s.types[0].null && s.types[1].null && s.types[1].label() == "(NULL)");
    assert!(parse_count(&QueryResult::default()).is_err());
}

#[test]
fn 종류별_오래됨_기준은_리터럴을_감싼_CASE_이고_없으면_기본_5분이다() {
    let tc = |stale: Option<i64>| TypeConf {
        stale,
        ..Default::default()
    };
    let o = Outbox {
        table: "t".into(),
        kind: "event_type".into(),
        retry: "retry_count".into(),
        due: "check_needed_at".into(),
        types: BTreeMap::from([
            ("BILLING_KEY_DELETED".to_string(), tc(Some(70))),
            ("O'BRIEN".to_string(), tc(Some(10))),
            ("CLIP".to_string(), tc(None)),
        ]),
        ..Default::default()
    };
    let want = r#""check_needed_at" < now() - case "event_type"::text when 'BILLING_KEY_DELETED' then interval '70 minutes' when 'O''BRIEN' then interval '10 minutes' else interval '5 minutes' end and "#;
    assert_eq!(count_sql(&o).matches(want).count(), 1);
    let o2 = Outbox {
        types: BTreeMap::from([("CLIP".to_string(), tc(None))]),
        ..o
    };
    assert_eq!(stale_expr(&o2), "interval '5 minutes'");
}
