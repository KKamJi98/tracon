use crate::collect::transcript::{EntryKind, TailSummary};
use crate::config::Thresholds;
use crate::model::{demote, Confidence, State};

/// 훅도 cmux도 없는 세션의 상태를 추론한다.
pub fn infer(
    tail: Option<&TailSummary>,
    alive: bool,
    cpu: f32,
    now_ms: i64,
    cfg: &Thresholds,
) -> (State, Confidence) {
    if !alive {
        return (State::Dead, Confidence::Fact);
    }
    let Some(t) = tail else {
        return (State::Unknown, Confidence::Low);
    };

    let age = now_ms - t.last_ts_ms;
    let idle_cpu = cpu <= cfg.cpu_idle_pct;

    let (raw, confidence) = match t.last_kind {
        EntryKind::AssistantText if t.pending_tool_use == 0 => {
            (State::WaitingInput, Confidence::Medium)
        }
        EntryKind::AssistantToolUse if t.pending_tool_use > 0 => {
            if !idle_cpu {
                (State::RunningTool, Confidence::Medium)
            } else if age > cfg.approval_suspect_ms {
                (State::WaitingApproval, Confidence::Low)
            } else {
                (State::RunningTool, Confidence::Low)
            }
        }
        // 사용자 메시지가 마지막이면 에이전트는 방금 일을 받은 것이다 - 사용자를
        // 기다리는 게 아니라 응답을 만드는 중이다. tool_result와 같은 grace 창을
        // 쓰고, 창을 넘기면 아래 기본 갈래에서 CPU로 다시 판단한다.
        EntryKind::UserToolResult | EntryKind::UserText if age <= cfg.running_grace_ms => {
            (State::RunningInference, Confidence::Medium)
        }
        _ => {
            if !idle_cpu {
                (State::RunningInference, Confidence::Low)
            } else {
                (State::Unknown, Confidence::Low)
            }
        }
    };

    (demote(raw, age, cfg), confidence)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect::transcript::{EntryKind, TailSummary, Usage};
    use crate::config::Thresholds;
    use crate::model::{Confidence, State};

    const NOW: i64 = 1_800_000_000_000;

    fn tail(kind: EntryKind, pending: usize, age_ms: i64) -> TailSummary {
        TailSummary {
            last_kind: kind,
            last_ts_ms: NOW - age_ms,
            pending_tool_use: pending,
            usage: Some(Usage {
                input: 1,
                cache_read: 2,
                cache_creation: 3,
            }),
            model: Some("claude-opus-5".into()),
            cwd: Some("/home/dev/project-a".into()),
        }
    }

    /// 마지막 엔트리가 사용자 메시지라는 것은 에이전트가 방금 일을 받았다는 뜻이다.
    /// 사용자를 기다리는 상태가 아니라 응답을 만드는 중으로 읽어야 한다.
    #[test]
    fn user_text_reads_as_running_not_waiting() {
        let t = tail(EntryKind::UserText, 0, 5_000);
        let (s, c) = infer(Some(&t), true, 0.0, NOW, &Thresholds::default());
        assert_eq!(s, State::RunningInference);
        assert_eq!(c, Confidence::Medium);
    }

    /// 사용자 메시지 뒤로 grace 창을 넘기도록 오래 멈춰 있으면 아직도 "돌고 있다"고
    /// 단정할 수 없다. 빨간 대기로 오인하지 않고 Unknown으로 떨어진다.
    #[test]
    fn stale_user_text_falls_through_to_unknown_not_waiting() {
        let cfg = Thresholds::default();
        let t = tail(EntryKind::UserText, 0, cfg.running_grace_ms + 1_000);
        let (s, _) = infer(Some(&t), true, 0.0, NOW, &cfg);
        assert_eq!(s, State::Unknown);
    }

    #[test]
    fn dead_when_process_is_gone() {
        let t = tail(EntryKind::AssistantText, 0, 1_000);
        let (s, c) = infer(Some(&t), false, 0.0, NOW, &Thresholds::default());
        assert_eq!(s, State::Dead);
        assert_eq!(c, Confidence::Fact);
    }

    #[test]
    fn assistant_text_without_pending_is_waiting_input() {
        let t = tail(EntryKind::AssistantText, 0, 5_000);
        let (s, c) = infer(Some(&t), true, 0.0, NOW, &Thresholds::default());
        assert_eq!(s, State::WaitingInput);
        assert_eq!(c, Confidence::Medium);
    }

    #[test]
    fn recent_tool_result_is_running() {
        let t = tail(EntryKind::UserToolResult, 0, 10_000);
        let (s, _) = infer(Some(&t), true, 0.0, NOW, &Thresholds::default());
        assert_eq!(s, State::RunningInference);
    }

    #[test]
    fn pending_tool_with_cpu_is_running_tool() {
        let t = tail(EntryKind::AssistantToolUse, 1, 40_000);
        let (s, _) = infer(Some(&t), true, 22.5, NOW, &Thresholds::default());
        assert_eq!(s, State::RunningTool);
    }

    #[test]
    fn pending_tool_without_cpu_is_suspected_approval() {
        let t = tail(EntryKind::AssistantToolUse, 1, 40_000);
        let (s, c) = infer(Some(&t), true, 0.0, NOW, &Thresholds::default());
        assert_eq!(s, State::WaitingApproval);
        assert_eq!(c, Confidence::Low);
    }

    #[test]
    fn long_silence_without_cpu_is_unknown() {
        let t = tail(EntryKind::UserToolResult, 0, 400_000);
        let (s, c) = infer(Some(&t), true, 0.0, NOW, &Thresholds::default());
        assert_eq!(s, State::Unknown);
        assert_eq!(c, Confidence::Low);
    }

    #[test]
    fn day_old_session_is_stale() {
        let t = tail(EntryKind::AssistantText, 0, 25 * 60 * 60 * 1000);
        let (s, _) = infer(Some(&t), true, 0.0, NOW, &Thresholds::default());
        assert_eq!(s, State::Stale);
    }

    #[test]
    fn waiting_past_idle_threshold_becomes_idle() {
        let t = tail(EntryKind::AssistantText, 0, 31 * 60 * 1000);
        let (s, _) = infer(Some(&t), true, 0.0, NOW, &Thresholds::default());
        assert_eq!(s, State::Idle);
    }

    #[test]
    fn no_transcript_yields_unknown() {
        let (s, c) = infer(None, true, 0.0, NOW, &Thresholds::default());
        assert_eq!(s, State::Unknown);
        assert_eq!(c, Confidence::Low);
    }
}
