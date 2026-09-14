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
        EntryKind::AssistantText | EntryKind::UserText if t.pending_tool_use == 0 => {
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
        EntryKind::UserToolResult if age <= cfg.running_grace_ms => {
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
