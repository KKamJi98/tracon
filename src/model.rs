use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Provider {
    Claude,
    Codex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum State {
    WaitingApproval,
    WaitingInput,
    RunningInference,
    RunningTool,
    Idle,
    Unknown,
    Stale,
    Dead,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    Red,
    Green,
    Plain,
    Dim,
}

impl State {
    pub fn color(self) -> Color {
        match self {
            State::WaitingApproval | State::WaitingInput => Color::Red,
            State::RunningInference | State::RunningTool => Color::Green,
            State::Idle | State::Unknown => Color::Plain,
            State::Stale | State::Dead => Color::Dim,
        }
    }

    pub fn is_waiting(self) -> bool {
        matches!(self, State::WaitingApproval | State::WaitingInput)
    }

    /// 정렬 우선순위. 작을수록 위로 온다.
    pub fn rank(self) -> u8 {
        match self.color() {
            Color::Red => 0,
            Color::Green => 1,
            Color::Plain => 2,
            Color::Dim => 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Source {
    Layer0Inferred,
    Layer1Hook,
    Layer2Cmux,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Confidence {
    Low,
    Medium,
    Fact,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionKey {
    pub provider: Provider,
    pub uuid: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    pub key: SessionKey,
    pub state: State,
    pub source: Source,
    pub confidence: Confidence,
    /// UTC epoch milliseconds
    pub observed_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HookEvent {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    PermissionRequest,
    Notification,
    Stop,
    SubagentStop,
    SessionEnd,
}

/// 훅 이벤트 하나를 상태로 옮긴다. `None`은 이전 상태 유지를 뜻한다.
#[allow(dead_code)]
pub fn transition(_prev: Option<State>, event: HookEvent) -> Option<State> {
    match event {
        HookEvent::SessionStart => Some(State::Idle),
        HookEvent::UserPromptSubmit | HookEvent::PostToolUse => Some(State::RunningInference),
        HookEvent::PreToolUse => Some(State::RunningTool),
        HookEvent::PermissionRequest => Some(State::WaitingApproval),
        HookEvent::Notification | HookEvent::Stop => Some(State::WaitingInput),
        HookEvent::SubagentStop => None,
        HookEvent::SessionEnd => Some(State::Dead),
    }
}

/// 같은 상태로 오래 머문 세션을 강등한다.
pub fn demote(state: State, since_change_ms: i64, cfg: &crate::config::Thresholds) -> State {
    if since_change_ms > cfg.stale_after_ms {
        return match state {
            State::Dead => State::Dead,
            _ => State::Stale,
        };
    }
    if state.is_waiting() && since_change_ms > cfg.idle_after_ms {
        return State::Idle;
    }
    state
}

/// 화면 한 줄에 대응하는, 병합이 끝난 세션 스냅샷 행.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub key: SessionKey,
    pub state: State,
    pub source: Source,
    pub confidence: Confidence,
    /// 마지막 상태 전이 시각. UTC epoch ms
    pub last_change_ms: i64,
    pub started_at_ms: Option<i64>,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub ctx_tokens: Option<u64>,
    pub ctx_window: Option<u64>,
    pub cpu: Option<f32>,
    pub pid: Option<i32>,
    pub jump: Option<String>,
}

impl Session {
    #[allow(dead_code)]
    pub fn ctx_pct(&self) -> Option<u32> {
        let (t, w) = (self.ctx_tokens?, self.ctx_window?);
        if w == 0 {
            return None;
        }
        Some(((t as f64 / w as f64) * 100.0).round() as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_request_maps_to_waiting_approval() {
        assert_eq!(
            transition(Some(State::RunningTool), HookEvent::PermissionRequest),
            Some(State::WaitingApproval)
        );
    }

    #[test]
    fn subagent_stop_keeps_parent_state() {
        assert_eq!(
            transition(Some(State::RunningTool), HookEvent::SubagentStop),
            None
        );
    }

    #[test]
    fn waiting_demotes_to_idle_after_threshold() {
        let cfg = crate::config::Thresholds::default();
        let since = cfg.idle_after_ms + 1;
        assert_eq!(demote(State::WaitingInput, since, &cfg), State::Idle);
    }

    #[test]
    fn waiting_stays_waiting_before_threshold() {
        let cfg = crate::config::Thresholds::default();
        assert_eq!(
            demote(State::WaitingInput, 1_000, &cfg),
            State::WaitingInput
        );
    }

    #[test]
    fn idle_becomes_stale_after_a_day() {
        let cfg = crate::config::Thresholds::default();
        assert_eq!(
            demote(State::Idle, cfg.stale_after_ms + 1, &cfg),
            State::Stale
        );
    }

    #[test]
    fn colors_fold_seven_states_into_three() {
        assert_eq!(State::WaitingApproval.color(), Color::Red);
        assert_eq!(State::WaitingInput.color(), Color::Red);
        assert_eq!(State::RunningInference.color(), Color::Green);
        assert_eq!(State::RunningTool.color(), Color::Green);
        assert_eq!(State::Idle.color(), Color::Plain);
        assert_eq!(State::Unknown.color(), Color::Plain);
        assert_eq!(State::Stale.color(), Color::Dim);
        assert_eq!(State::Dead.color(), Color::Dim);
    }
}
