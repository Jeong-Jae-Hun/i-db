//! 대시보드(R64)·흐름(R69)·보드(R70) 화면 규칙이다. 확인은 엔진이 하고 여기서는 카드·상태 기호만 정한다.

use std::collections::HashMap;

use idb_engine::{ConnSummary, FlowBox, FlowResult, HealthInfo, PanelOut};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Ok,
    Warn,
    Fail,
    Pending,
}

impl State {
    pub fn symbol(self) -> &'static str {
        match self {
            State::Ok => "●",
            State::Warn => "▲",
            State::Fail => "✕",
            State::Pending => "…",
        }
    }
}

/// 흐름 그림의 자리다(R69.4). 상자 목록은 한 줄 사슬이 아니다: 머리(아웃박스 → SNS)에서 큐마다 갈래가 나뉘고,
/// 아웃박스 종류 줄(R69.6)은 서로 이어지지 않는 아웃박스의 내역이다. 값은 상자 자리(index)다.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FlowLayout {
    pub head: Vec<usize>,
    pub branches: Vec<Branch>,
    pub types: Vec<usize>,
}

/// 큐 하나의 갈래다. lambdas 는 큐 뒤에 잇고 dlq 는 큐 밑에 붙인다.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Branch {
    pub queue: usize,
    pub dlq: Vec<usize>,
    pub lambdas: Vec<usize>,
}

/// 접속 한 줄의 상태다. 수치 하나라도 경고면 경고다.
pub fn health_state(h: Option<&Result<HealthInfo, String>>) -> State {
    match h {
        None => State::Pending,
        Some(Err(_)) => State::Fail,
        Some(Ok(h)) if !h.ok || !h.err.is_empty() => State::Fail,
        Some(Ok(h)) if h.metrics.iter().any(|m| m.warn) => State::Warn,
        Some(Ok(_)) => State::Ok,
    }
}

/// 맨 위 요약 카드(R64.1): (제목, 값, 경고)
pub fn summary(conns: &[ConnSummary], health: &HashMap<String, Result<HealthInfo, String>>, exhausted: i64) -> Vec<(String, String, bool)> {
    let count = |f: &dyn Fn(&ConnSummary) -> bool| conns.iter().filter(|c| f(c)).count();
    let mut kinds: Vec<(String, usize)> = Vec::new();
    for c in conns {
        match kinds.iter_mut().find(|(k, _)| *k == c.kind) {
            Some((_, n)) => *n += 1,
            None => kinds.push((c.kind.clone(), 1)),
        }
    }
    let states: Vec<State> = conns.iter().map(|c| health_state(health.get(&c.name))).collect();
    let fail = states.iter().filter(|s| **s == State::Fail).count();
    let ok = states.iter().filter(|s| matches!(s, State::Ok | State::Warn)).count();
    let mut out = vec![
        (
            "접속".to_string(),
            format!(
                "{} ({})",
                conns.len(),
                kinds.iter().map(|(k, n)| format!("{k} {n}")).collect::<Vec<_>>().join(" · ")
            ),
            false,
        ),
        (
            "환경".to_string(),
            format!(
                "prod {} · stg {} · local {}",
                count(&|c| c.env == "prod"),
                count(&|c| c.env == "stg"),
                count(&|c| c.env != "prod" && c.env != "stg")
            ),
            false,
        ),
        ("연결".to_string(), format!("됨 {ok} · 실패 {fail}"), fail > 0),
    ];
    if exhausted > 0 {
        out.push(("아웃박스 한도 소진".to_string(), exhausted.to_string(), true));
    }
    out
}

/// 흐름 하나의 상태다(R69.4).
pub fn flow_state(f: Option<&FlowResult>) -> State {
    match f {
        None => State::Pending,
        Some(f) if f.boxes.iter().any(|b| !b.err.is_empty()) => State::Fail,
        Some(f) if f.boxes.iter().any(|b| b.warn) => State::Warn,
        Some(_) => State::Ok,
    }
}

/// 엔진 상자 순서(아웃박스 → SNS → 큐(→ DLQ·Lambda) → 종류 줄)를 그림 자리로 나눈다.
pub fn flow_layout(bs: &[FlowBox]) -> FlowLayout {
    let mut l = FlowLayout::default();
    for (i, b) in bs.iter().enumerate() {
        match (b.stage.as_str(), l.branches.last_mut()) {
            ("outbox-type", _) => l.types.push(i),
            ("sqs", _) => l.branches.push(Branch {
                queue: i,
                ..Default::default()
            }),
            ("dlq", Some(br)) => br.dlq.push(i),
            ("lambda", Some(br)) => br.lambdas.push(i),
            _ => l.head.push(i),
        }
    }
    l
}

/// 종류 줄 수치 `대기 0 · 재시도 0 · 한도 소진 0 · → consumer` 를 수치와 소비 큐로 나눈다.
pub fn split_type_metrics(m: &str) -> (&str, &str) {
    m.split_once(" · → ").unwrap_or((m, ""))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelKind {
    Error,
    Number,
    Table,
    Flow,
    Note,
}

pub fn panel_kind(p: &PanelOut) -> PanelKind {
    if !p.error.is_empty() {
        PanelKind::Error
    } else if !p.boxes.is_empty() {
        PanelKind::Flow
    } else if p.value.is_some() {
        PanelKind::Number
    } else if p.result.is_some() {
        PanelKind::Table
    } else {
        PanelKind::Note
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(ok: bool, warn: bool) -> HealthInfo {
        serde_json::from_value(serde_json::json!({
            "ok": ok, "latency_ms": 3, "version": "16", "summary": "", "err": if ok { "" } else { "boom" },
            "metrics": [{"name": "sessions", "value": "3", "warn": warn}]
        }))
        .unwrap()
    }

    fn conn(name: &str, kind: &str, env: &str) -> ConnSummary {
        ConnSummary {
            name: name.into(),
            kind: kind.into(),
            env: env.into(),
            ..Default::default()
        }
    }

    #[test]
    fn 상태는_실패_경고_정상_대기_순으로_가른다() {
        assert_eq!(health_state(None), State::Pending);
        assert_eq!(health_state(Some(&Err("x".into()))), State::Fail);
        assert_eq!(health_state(Some(&Ok(h(false, false)))), State::Fail);
        assert_eq!(health_state(Some(&Ok(h(true, true)))), State::Warn);
        assert_eq!(health_state(Some(&Ok(h(true, false)))), State::Ok);
        assert_eq!(State::Pending.symbol(), "…");
    }

    #[test]
    fn 요약_카드는_종류_환경_연결_수를_센다() {
        let conns = vec![
            conn("a", "postgresql", "prod"),
            conn("b", "postgresql", "local"),
            conn("c", "redis", "stg"),
        ];
        let health = HashMap::from([("a".to_string(), Ok(h(true, false))), ("c".to_string(), Err("x".to_string()))]);
        let s = summary(&conns, &health, 0);
        assert_eq!(s[0].1, "3 (postgresql 2 · redis 1)");
        assert_eq!(s[1].1, "prod 1 · stg 1 · local 1");
        assert_eq!(s[2], ("연결".to_string(), "됨 1 · 실패 1".to_string(), true));
        assert_eq!(s.len(), 3);
        let s = summary(&conns, &health, 7);
        assert_eq!(s[3], ("아웃박스 한도 소진".to_string(), "7".to_string(), true));
    }

    fn fb(stage: &str, name: &str) -> FlowBox {
        FlowBox {
            stage: stage.into(),
            name: name.into(),
            ..Default::default()
        }
    }

    #[test]
    fn 흐름_그림은_종류_줄을_사슬에서_뺀다() {
        let bs = [
            fb("outbox", "o"),
            fb("sns", "t"),
            fb("sqs", "q1"),
            fb("dlq", "q1-dlq"),
            fb("lambda", "w1"),
            fb("sqs", "q2"),
            fb("outbox-type", "CLIP"),
            fb("outbox-type", "PAY"),
        ];
        let l = flow_layout(&bs);
        assert_eq!(l.head, vec![0, 1]);
        assert_eq!(
            l.branches,
            vec![
                Branch {
                    queue: 2,
                    dlq: vec![3],
                    lambdas: vec![4]
                },
                Branch {
                    queue: 5,
                    ..Default::default()
                }
            ]
        );
        assert_eq!(l.types, vec![6, 7]);
        // 토픽·큐 없는 아웃박스 흐름은 머리 하나와 종류 줄뿐이다
        let l = flow_layout(&[fb("outbox", "o"), fb("outbox-type", "CLIP")]);
        assert_eq!((l.head, l.branches.len(), l.types), (vec![0], 0, vec![1]));
    }

    #[test]
    fn 종류_수치와_소비_큐를_나눈다() {
        assert_eq!(
            split_type_metrics("대기 3 · 한도 소진 1 · → consume-clip"),
            ("대기 3 · 한도 소진 1", "consume-clip")
        );
        assert_eq!(split_type_metrics("대기 3"), ("대기 3", ""));
    }

    #[test]
    fn 흐름과_패널_종류() {
        let f: FlowResult = serde_json::from_value(serde_json::json!({"flow": "f", "boxes": [
            {"stage": "outbox", "name": "t", "metrics": "대기 1", "warn": true, "idle": false, "err": "", "note": "", "conn": "a"}
        ]}))
        .unwrap();
        assert_eq!(flow_state(Some(&f)), State::Warn);
        assert_eq!(flow_state(None), State::Pending);
        let p: PanelOut = serde_json::from_value(
            serde_json::json!({"title": "t", "conn": "a", "flow": "", "view": "number", "prod": false, "warn": false, "value": "3"}),
        )
        .unwrap();
        assert_eq!(panel_kind(&p), PanelKind::Number);
        let p: PanelOut = serde_json::from_value(serde_json::json!({"title": "t", "conn": "a", "flow": "", "view": "table", "prod": false, "warn": false, "error": "설정 오류: x"})).unwrap();
        assert_eq!(panel_kind(&p), PanelKind::Error);
    }
}
