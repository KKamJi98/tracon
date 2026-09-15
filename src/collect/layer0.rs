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
        // 사용자가 끊었으면 에이전트는 멈췄고 세션은 다음 입력을 기다린다. CPU가
        // 0인 것은 당연하지 - 그걸 "판단 불가"로 읽으면 화면에서 제일 중요한 행이
        // Unknown이 된다.
        EntryKind::UserInterrupted => (State::WaitingInput, Confidence::Medium),
        // 여기까지 왔으면 마지막 엔트리가 누구 차례였는지로 정한다. CPU는 근거가
        // 되지 못한다 - claude는 모델 응답을 기다리는 동안 네트워크에 묶여 CPU를
        // 거의 쓰지 않아서, 낮은 CPU를 "안 돌고 있다"로 읽으면 한창 일하는 세션이
        // 판단 불가로 떨어진다. transcript가 있는 한 차례는 언제나 알 수 있으므로
        // 이 경로에서 Unknown은 나오지 않는다 - 정말 모르는 것은 transcript가
        // 아예 없을 때뿐이고, 그건 위에서 이미 걸렀다.
        //
        // assistant가 말을 했는데 미완결 tool_use가 남았다: 도구를 돌리는 중이다.
        // 한 응답 안에서 text 블록이 tool_use보다 앞에 오면 이 모양이 된다.
        EntryKind::AssistantText => (State::RunningTool, Confidence::Low),
        // 나머지는 전부 에이전트가 다음 출력을 빚진 상태다. 오래 걸리면 demote가
        // Idle로, 하루가 지나면 Stale로 내려 준다.
        EntryKind::AssistantToolUse | EntryKind::UserToolResult | EntryKind::UserText => {
            (State::RunningInference, Confidence::Low)
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
            title: None,
            entrypoint: None,
        }
    }

    /// 마지막 엔트리가 사용자 메시지라는 것은 에이전트가 방금 일을 받았다는 뜻이다.
    /// 사용자를 기다리는 상태가 아니라 응답을 만드는 중으로 읽어야 한다.
    /// 인터럽트 직후 CPU는 0이다. 그걸 "판단 불가"로 읽으면, 사용자가 방금 끊어서
    /// 입력을 기다리는 세션이 Unknown으로 보인다 - 화면에서 제일 중요한 행인데.
    /// claude는 모델 응답을 기다리는 동안 네트워크에 묶여 CPU를 거의 쓰지 않는다.
    /// 낮은 CPU를 "안 돌고 있다"로 읽으면, tool_result를 받아 한창 생각 중인 세션이
    /// 60초 grace를 넘긴 순간 판단 불가로 떨어진다 - 실측에서 3분 48초째 Unknown이었다.
    /// 턴이 끝난 직후는 정말 내 답을 기다린다. 하지만 한참 지난 뒤에도 빨갛게 남으면
    /// "지금 나를 부르는 세션"이라는 빨간색의 의미가 닳는다.
    #[test]
    fn a_finished_turn_stops_being_red_once_it_is_stale() {
        let cfg = Thresholds::default();
        let fresh = tail(EntryKind::AssistantText, 0, 60_000);
        assert_eq!(
            infer(Some(&fresh), true, 0.0, NOW, &cfg).0,
            State::WaitingInput
        );

        let old = tail(EntryKind::AssistantText, 0, cfg.idle_after_ms + 1_000);
        let (state, _) = infer(Some(&old), true, 0.0, NOW, &cfg);
        assert_eq!(state, State::Idle);
        assert!(!state.is_waiting(), "오래된 턴 종료가 계속 빨갛다");
    }

    #[test]
    fn a_long_think_after_a_tool_result_is_still_running_not_unknown() {
        let cfg = Thresholds::default();
        let t = tail(EntryKind::UserToolResult, 0, 228_000);
        let (state, conf) = infer(Some(&t), true, 0.0, NOW, &cfg);
        assert_eq!(
            state,
            State::RunningInference,
            "일하는 세션을 Unknown으로 읽었다"
        );
        assert_eq!(conf, Confidence::Low);
    }

    /// assistant가 말을 했는데 미완결 tool_use가 남아 있으면 도구를 돌리는 중이다.
    /// 한 응답 안에서 text 블록이 tool_use 블록보다 앞에 오면 이 모양이 된다.
    #[test]
    fn assistant_text_with_a_pending_tool_is_running_a_tool() {
        let cfg = Thresholds::default();
        let t = tail(EntryKind::AssistantText, 1, 5 * 60 * 1000);
        assert_eq!(infer(Some(&t), true, 0.0, NOW, &cfg).0, State::RunningTool);
    }

    /// transcript가 아예 없을 때만 모른다고 한다. 있으면 누구 차례인지는 늘 안다.
    #[test]
    fn unknown_is_reserved_for_having_no_transcript_at_all() {
        let cfg = Thresholds::default();
        assert_eq!(infer(None, true, 0.0, NOW, &cfg).0, State::Unknown);
    }

    #[test]
    fn an_interrupted_turn_is_waiting_for_input_not_unknown() {
        let cfg = Thresholds::default();
        let t = tail(EntryKind::UserInterrupted, 0, 90_000);
        let (state, conf) = infer(Some(&t), true, 0.0, NOW, &cfg);
        assert_eq!(state, State::WaitingInput);
        assert_eq!(conf, Confidence::Medium);

        // 시간이 지나면 다른 대기와 똑같이 Idle로 내려간다 - Unknown으로는 절대 안 간다.
        let old = tail(EntryKind::UserInterrupted, 0, cfg.idle_after_ms + 1_000);
        assert_eq!(infer(Some(&old), true, 0.0, NOW, &cfg).0, State::Idle);
    }

    #[test]
    fn user_text_reads_as_running_not_waiting() {
        let t = tail(EntryKind::UserText, 0, 5_000);
        let (s, c) = infer(Some(&t), true, 0.0, NOW, &Thresholds::default());
        assert_eq!(s, State::RunningInference);
        assert_eq!(c, Confidence::Medium);
    }

    /// 사용자 메시지 뒤로 grace 창을 넘기도록 오래 멈춰 있어도 사용자를 기다리는
    /// 것은 아니다 - 다음 출력은 에이전트가 빚지고 있다. 빨간 대기로 오인하지
    /// 않는다는 원래 보장은 그대로고, 확신만 Low로 낮춘다.
    #[test]
    fn stale_user_text_is_still_the_agents_turn_not_the_users() {
        let cfg = Thresholds::default();
        let t = tail(EntryKind::UserText, 0, cfg.running_grace_ms + 1_000);
        let (s, c) = infer(Some(&t), true, 0.0, NOW, &cfg);
        assert_eq!(s, State::RunningInference);
        assert_eq!(c, Confidence::Low);
        assert!(!s.is_waiting(), "빨간 대기로 오인하면 안 된다");
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

    /// CPU가 0이어도 tool_result 뒤는 에이전트 차례다. claude가 모델 응답을
    /// 기다리는 시간은 대부분 CPU 0이라, 침묵을 곧 정지로 읽으면 안 된다.
    #[test]
    fn long_silence_without_cpu_is_still_the_agents_turn() {
        let t = tail(EntryKind::UserToolResult, 0, 400_000);
        let (s, c) = infer(Some(&t), true, 0.0, NOW, &Thresholds::default());
        assert_eq!(s, State::RunningInference);
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
