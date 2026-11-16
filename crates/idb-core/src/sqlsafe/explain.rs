//! EXPLAIN 문장 만들기와 계획 JSON 읽기다. Go `internal/db/explain.go` 에서 접속 없는 쪽과 같다.

use serde::Deserialize;

use super::{first_sql_word, go_upper, trim_statement};

const EXPLAIN_FORMATS: &[&str] = &["TEXT", "JSON", "YAML", "XML"];

/// format 이 비었거나 TEXT·JSON·YAML·XML 이 아니면 TEXT 로 본다.
#[derive(Clone, Debug, Default)]
pub struct ExplainOpt {
    pub analyze: bool,
    pub buffers: bool,
    pub format: String,
}

/// 문장 하나를 EXPLAIN (옵션) 으로 감싼다. 끝 ';' 는 뗀다.
/// 이미 EXPLAIN 으로 시작하면 사용자가 쓴 옵션을 그대로 두려고 감싸지 않는다.
/// ANALYZE 는 문장을 실제로 실행하므로 확인은 호출하는 쪽이 `is_read_only_sql` 로 한다.
pub fn explain_sql(stmt: &str, opt: &ExplainOpt) -> String {
    let stmt = trim_statement(stmt);
    if stmt.is_empty() || first_sql_word(stmt) == "explain" {
        return stmt.to_string();
    }
    let mut opts = Vec::new();
    if opt.analyze {
        opts.push("ANALYZE".to_string());
    }
    if opt.buffers {
        opts.push("BUFFERS".to_string());
    }
    // format 을 그대로 이어 붙이면 SQL 조각이 끼어들 수 있어 허용 목록만 받는다
    let mut format = go_upper(opt.format.trim());
    if !EXPLAIN_FORMATS.contains(&format.as_str()) {
        format = "TEXT".into();
    }
    opts.push(format!("FORMAT {format}"));
    // 문장이 줄 주석으로 끝나도 EXPLAIN 이 앞에 붙으므로 괜찮다
    format!("EXPLAIN ({}) {stmt}", opts.join(", "))
}

/// EXPLAIN (FORMAT JSON) 의 노드에서 영향 행 수를 찾는 데 쓰는 필드만 읽는다.
/// Go json 처럼 null 은 빈 값으로 본다.
#[derive(Deserialize, Default)]
#[serde(default)]
struct PlanNode {
    #[serde(rename = "Node Type")]
    node_type: Option<String>,
    #[serde(rename = "Parent Relationship")]
    relation: Option<String>,
    #[serde(rename = "Plan Rows")]
    rows: Option<f64>,
    #[serde(rename = "Plans")]
    plans: Option<Vec<PlanNode>>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct PlanRoot {
    #[serde(rename = "Plan")]
    plan: Option<PlanNode>,
}

/// UPDATE·DELETE 계획(EXPLAIN FORMAT JSON)에서 맨 위 ModifyTable 노드의 Outer 자식(바꿀 행을 만드는 노드)의 Plan Rows 다(명세 R39).
/// PostgreSQL 13 이하는 상속·파티션 테이블을 바꿀 때 자식마다 Member 자식을 두므로 Outer 가 없으면 Member 의 합이다.
/// ModifyTable 자신의 Plan Rows 는 RETURNING 이 없으면 0 이라 쓰지 않는다. InitPlan·SubPlan 자식은 건너뛴다.
pub fn modify_table_rows(raw: &[u8]) -> Result<i64, String> {
    let plans: Option<Vec<PlanRoot>> = serde_json::from_slice(raw).map_err(|e| format!("parse EXPLAIN JSON: {e}"))?;
    let plans = plans.unwrap_or_default();
    let top = match plans.as_slice() {
        [PlanRoot { plan: Some(p) }] if p.node_type.as_deref() == Some("ModifyTable") => p,
        _ => return Err("EXPLAIN top node is not ModifyTable".into()),
    };
    let (mut members, mut sum) = (0, 0.0);
    for c in top.plans.iter().flatten() {
        match c.relation.as_deref() {
            Some("Outer") => return Ok(c.rows.unwrap_or(0.0).round() as i64),
            Some("Member") => {
                members += 1;
                sum += c.rows.unwrap_or(0.0);
            }
            _ => {}
        }
    }
    if members > 0 {
        return Ok(sum.round() as i64);
    }
    Err("EXPLAIN ModifyTable has no outer plan".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlsafe::is_read_only_sql;

    fn opt(analyze: bool, buffers: bool, format: &str) -> ExplainOpt {
        ExplainOpt {
            analyze,
            buffers,
            format: format.into(),
        }
    }

    #[test]
    fn 옵션을_붙이고_끝_세미콜론을_뗀다() {
        let cases = [
            ("select 1;", opt(true, true, ""), "EXPLAIN (ANALYZE, BUFFERS, FORMAT TEXT) select 1"),
            ("  select 1 ; ;\n", opt(false, false, "json"), "EXPLAIN (FORMAT JSON) select 1"),
            (
                "select 1",
                opt(false, false, "text) delete from t; --"),
                "EXPLAIN (FORMAT TEXT) select 1",
            ),
            ("select 1 -- 끝;", opt(false, false, ""), "EXPLAIN (FORMAT TEXT) select 1 -- 끝"),
            (
                "/* x */ EXPLAIN analyze select 1;",
                opt(false, true, ""),
                "/* x */ EXPLAIN analyze select 1",
            ),
            (" ; ", opt(true, false, ""), ""),
        ];
        for (stmt, o, want) in cases {
            assert_eq!(explain_sql(stmt, &o), want, "{stmt:?} {o:?}");
        }
    }

    #[test]
    fn ANALYZE를_붙인_결과는_읽기_전용이_아니다() {
        assert!(
            !is_read_only_sql(&explain_sql("delete from t", &opt(true, false, ""))),
            "EXPLAIN ANALYZE 는 확인 대상이어야 한다"
        );
        assert!(
            is_read_only_sql(&explain_sql("delete from t", &ExplainOpt::default())),
            "ANALYZE 없는 EXPLAIN 은 읽기 전용이다"
        );
    }

    #[test]
    fn Outer_자식의_Plan_Rows_를_쓰고_InitPlan_은_건너뛴다() {
        let raw = r#"[{"Plan": {"Node Type": "ModifyTable", "Plan Rows": 0, "Plans": [
            {"Node Type": "Result", "Parent Relationship": "InitPlan", "Plan Rows": 1},
            {"Node Type": "Seq Scan", "Parent Relationship": "Outer", "Plan Rows": 1234.6}]}}]"#;
        assert_eq!(modify_table_rows(raw.as_bytes()), Ok(1235));
        for bad in [
            r#"[{"Plan": {"Node Type": "Seq Scan", "Plan Rows": 5}}]"#,
            r#"[{"Plan": {"Node Type": "ModifyTable", "Plan Rows": 0}}]"#,
            "not json",
        ] {
            assert!(modify_table_rows(bad.as_bytes()).is_err(), "오류여야 한다: {bad}");
        }
    }

    #[test]
    fn PG13_이하의_Member_자식은_Plan_Rows_를_더한다() {
        // PostgreSQL 13 의 파티션 테이블 DELETE 계획(ModifyTable 아래 파티션마다 Member)
        let raw = r#"[{"Plan": {"Node Type": "ModifyTable", "Operation": "Delete", "Plan Rows": 0, "Plans": [
            {"Node Type": "Seq Scan", "Parent Relationship": "Member", "Relation Name": "events_1", "Plan Rows": 40.4},
            {"Node Type": "Seq Scan", "Parent Relationship": "Member", "Relation Name": "events_2", "Plan Rows": 9.4},
            {"Node Type": "Result", "Parent Relationship": "InitPlan", "Plan Rows": 1}]}}]"#;
        assert_eq!(modify_table_rows(raw.as_bytes()), Ok(50));
    }

    #[test]
    fn 고정_오류_글은_Go_와_같다() {
        assert_eq!(
            modify_table_rows(br#"[{"Plan": {"Node Type": "Seq Scan"}}]"#),
            Err("EXPLAIN top node is not ModifyTable".into())
        );
        assert_eq!(
            modify_table_rows(br#"[{"Plan": {"Node Type": "ModifyTable"}}]"#),
            Err("EXPLAIN ModifyTable has no outer plan".into())
        );
        assert!(modify_table_rows(b"not json").unwrap_err().starts_with("parse EXPLAIN JSON: "));
    }
}
