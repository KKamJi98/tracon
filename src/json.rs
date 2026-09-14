use crate::model::{Color, Session};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub sessions: Vec<Session>,
    pub hooks_installed: bool,
    pub cmux_linked: bool,
    pub generated_at_ms: i64,
}

#[derive(Debug, Clone, Copy, Default)]
#[allow(dead_code)]
pub struct Counts {
    pub waiting: usize,
    pub running: usize,
    pub idle: usize,
    pub stale: usize,
}

impl Snapshot {
    #[allow(dead_code)]
    pub fn counts(&self) -> Counts {
        let mut c = Counts::default();
        for s in &self.sessions {
            match s.state.color() {
                Color::Red => c.waiting += 1,
                Color::Green => c.running += 1,
                Color::Plain => c.idle += 1,
                Color::Dim => c.stale += 1,
            }
        }
        c
    }
}

/// 빨강 -> 초록 -> 무색 -> dim, 같은 그룹 안에서는 오래 기다린 순.
/// 상태와 마지막 변경 시각까지 같으면(예: 같은 tick에 기록된 두 세션) uuid로 마지막
/// 판가름을 낸다 - HashMap 순회에서 나온 입력이 poll마다 순서를 바꾸지 않도록.
pub fn sort_sessions(sessions: &mut [Session]) {
    sessions.sort_by(|a, b| {
        (a.state.rank(), a.last_change_ms, &a.key.uuid).cmp(&(
            b.state.rank(),
            b.last_change_ms,
            &b.key.uuid,
        ))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;

    fn session_with(uuid: &str, state: State, last_change: i64) -> Session {
        Session {
            key: SessionKey {
                provider: Provider::Claude,
                uuid: uuid.into(),
            },
            state,
            source: Source::Layer0Inferred,
            confidence: Confidence::Medium,
            last_change_ms: last_change,
            started_at_ms: Some(0),
            cwd: None,
            model: None,
            ctx_tokens: None,
            ctx_window: None,
            cpu: None,
            pid: None,
            jump: None,
        }
    }

    fn session(state: State, last_change: i64) -> Session {
        session_with("u", state, last_change)
    }

    #[test]
    fn sorts_red_first_then_oldest_wait() {
        let mut v = vec![
            session(State::RunningTool, 100),
            session(State::WaitingInput, 50),
            session(State::WaitingApproval, 10),
            session(State::Idle, 5),
        ];
        sort_sessions(&mut v);
        assert_eq!(v[0].state, State::WaitingApproval);
        assert_eq!(v[1].state, State::WaitingInput);
        assert_eq!(v[2].state, State::RunningTool);
        assert_eq!(v[3].state, State::Idle);
    }

    #[test]
    fn ties_on_state_and_last_change_break_by_uuid() {
        let mut v = vec![
            session_with("b", State::WaitingInput, 10),
            session_with("a", State::WaitingInput, 10),
        ];
        sort_sessions(&mut v);
        assert_eq!(v[0].key.uuid, "a");
        assert_eq!(v[1].key.uuid, "b");
    }

    #[test]
    fn snapshot_serializes_to_stable_json() {
        let snap = Snapshot {
            sessions: vec![session(State::WaitingApproval, 1)],
            hooks_installed: false,
            cmux_linked: false,
            generated_at_ms: 1_800_000_000_000,
        };
        let s = serde_json::to_string(&snap).expect("json");
        assert!(s.contains("\"WaitingApproval\""));
        assert!(s.contains("\"hooks_installed\":false"));
    }

    #[test]
    fn counts_group_by_color() {
        let snap = Snapshot {
            sessions: vec![
                session(State::WaitingApproval, 1),
                session(State::RunningTool, 1),
                session(State::Idle, 1),
                session(State::Stale, 1),
            ],
            hooks_installed: true,
            cmux_linked: true,
            generated_at_ms: 0,
        };
        let c = snap.counts();
        assert_eq!((c.waiting, c.running, c.idle, c.stale), (1, 1, 1, 1));
    }
}
