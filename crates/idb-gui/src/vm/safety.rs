//! 엔진의 거절(code 3 reason)을 화면 동작으로 바꾼다. 무엇을 막을지는 엔진이 정하고 여기서는
//! 확인 창을 띄울지, 확인하면 어떤 표시를 붙여 다시 보낼지만 정한다(docs/serve-protocol.md).

use idb_engine::{CODE_REFUSED, EngineError};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ask {
    /// R6: prod 의 읽기 전용이 아닌 문장
    Prod,
    /// R25: 위험 문장(WHERE 없는 UPDATE·DELETE, DROP 등)
    Danger,
    /// 환경과 상관없이 늘 묻는 것(백엔드 신호, 패턴 일괄 삭제)
    Always,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Gate {
    /// 보통 오류. 상태 줄에 보인다
    Fail(String),
    /// R54: prod 가 잠겨 있다. 확인 창 없이 거절하고 ⌘L 로 열라고 알린다
    Locked(String),
    /// 확인 창을 띄우고, 확인하면 `Confirms` 에 붙여 다시 보낸다
    Ask { ask: Ask, question: String },
}

/// 다시 보낼 때 붙이는 확인 표시다.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Confirms {
    pub prod: bool,
    pub danger: bool,
    pub always: bool,
}

impl Confirms {
    fn has(&self, a: Ask) -> bool {
        match a {
            Ask::Prod => self.prod,
            Ask::Danger => self.danger,
            Ask::Always => self.always,
        }
    }

    pub fn grant(&mut self, a: Ask) {
        match a {
            Ask::Prod => self.prod = true,
            Ask::Danger => self.danger = true,
            Ask::Always => self.always = true,
        }
    }

    /// 이번 오류를 어떻게 다룰지 정한다. 이미 확인한 것을 또 물으면 되묻지 않고 오류로 끝내 고리를 막는다.
    pub fn gate(&self, e: &EngineError) -> Gate {
        if e.code != CODE_REFUSED {
            return Gate::Fail(e.error.clone());
        }
        let ask = match e.reason.as_deref() {
            Some("prod_locked") => return Gate::Locked(e.error.clone()),
            Some("confirm_prod") => Ask::Prod,
            Some("confirm_danger") => Ask::Danger,
            Some("confirm") => Ask::Always,
            _ => return Gate::Fail(e.error.clone()),
        };
        if self.has(ask) {
            return Gate::Fail(e.error.clone());
        }
        Gate::Ask {
            ask,
            question: e.error.clone(),
        }
    }
}

impl Ask {
    /// 확인 창 제목이다. 본문은 엔진이 보낸 질문 글을 그대로 쓴다.
    pub fn title(self) -> &'static str {
        match self {
            Ask::Prod => "운영(prod) 접속에 쓰기",
            Ask::Danger => "위험한 문장",
            Ask::Always => "확인",
        }
    }

    pub fn ok_text(self) -> &'static str {
        match self {
            Ask::Prod => "prod 에 실행",
            Ask::Danger => "그래도 실행",
            Ask::Always => "실행",
        }
    }
}

// ─── 환경 배지 ───

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Prod,
    Stg,
    Local,
}

/// 접속 환경의 배지다. prod 는 빨강으로 그린다(R2.2).
pub fn tone(env: &str) -> Tone {
    match env {
        "prod" => Tone::Prod,
        "stg" => Tone::Stg,
        _ => Tone::Local,
    }
}

/// 잠금 상태 줄 글이다(R54). prod 가 아니면 없다.
pub fn lock_label(l: &idb_engine::LockState) -> Option<String> {
    if !l.prod {
        return None;
    }
    Some(if l.locked {
        "prod 잠김 (⌘L 로 열기)".into()
    } else {
        format!("prod 열림 {}:{:02} 남음", l.remaining_secs / 60, l.remaining_secs % 60)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(reason: &str) -> EngineError {
        EngineError::refused(reason, "prod(app-prod)에 제출 1건: y/N")
    }

    #[test]
    fn 잠긴_prod_는_확인_창_없이_거절한다() {
        assert_eq!(
            Confirms::default().gate(&refused("prod_locked")),
            Gate::Locked("prod(app-prod)에 제출 1건: y/N".into())
        );
    }

    #[test]
    fn 확인_사유마다_묻고_확인하면_그_표시를_붙인다() {
        let mut c = Confirms::default();
        for (reason, ask) in [
            ("confirm_prod", Ask::Prod),
            ("confirm_danger", Ask::Danger),
            ("confirm", Ask::Always),
        ] {
            match c.gate(&refused(reason)) {
                Gate::Ask { ask: a, question } => {
                    assert_eq!(a, ask);
                    assert!(question.contains("y/N"));
                    c.grant(a);
                }
                g => panic!("{reason}: {g:?}"),
            }
        }
        assert_eq!(
            c,
            Confirms {
                prod: true,
                danger: true,
                always: true
            }
        );
    }

    #[test]
    fn 이미_확인한_것을_또_물으면_오류로_끝낸다() {
        let mut c = Confirms::default();
        c.grant(Ask::Prod);
        assert!(matches!(c.gate(&refused("confirm_prod")), Gate::Fail(_)));
        // prod 를 확인한 뒤 위험 문장 확인이 따로 올 수 있다
        assert!(matches!(c.gate(&refused("confirm_danger")), Gate::Ask { ask: Ask::Danger, .. }));
    }

    #[test]
    fn 거절이_아니거나_모르는_사유는_보통_오류다() {
        let c = Confirms::default();
        assert_eq!(c.gate(&EngineError::run("boom")), Gate::Fail("boom".into()));
        assert!(matches!(c.gate(&refused("new_reason")), Gate::Fail(_)));
        let mut e = EngineError::run("tx");
        e.reason = Some("in_tx".into());
        assert!(matches!(c.gate(&e), Gate::Fail(_)));
    }

    #[test]
    fn 환경_배지는_prod_를_따로_친다() {
        assert_eq!(tone("prod"), Tone::Prod);
        assert_eq!(tone("stg"), Tone::Stg);
        assert_eq!(tone("local"), Tone::Local);
    }

    #[test]
    fn 잠금_글은_prod_만_있고_남은_시간을_보인다() {
        let mut l = idb_engine::LockState {
            conn: "a".into(),
            prod: false,
            locked: true,
            remaining_secs: 0,
            unlock_min: 10,
        };
        assert_eq!(lock_label(&l), None);
        l.prod = true;
        assert!(lock_label(&l).unwrap().contains("잠김"));
        l.locked = false;
        l.remaining_secs = 125;
        assert!(lock_label(&l).unwrap().contains("2:05"));
    }
}
