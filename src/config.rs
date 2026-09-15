#[derive(Debug, Clone, Copy)]
pub struct Thresholds {
    /// Waiting이 이 시간을 넘기면 Idle로 강등한다.
    ///
    /// 턴이 끝난 직후의 세션은 정말로 내 답을 기다린다. 하지만 그 상태로 한참
    /// 지나면 나는 이미 보고 지나간 것이고, 그런 행이 빨갛게 남아 있으면 "지금
    /// 나를 부르는 세션"이라는 빨간색의 의미가 닳는다. 실측 기준: 사용자가 "기다리는
    /// 상태가 아니다"라고 지적한 세션이 각각 8분 35초, 24분 45초 경과였다.
    pub idle_after_ms: i64,
    /// 이 시간을 넘기면 Stale로 강등한다.
    pub stale_after_ms: i64,
    /// tool_result 이후 이 시간 안이면 Running으로 본다.
    pub running_grace_ms: i64,
    /// 미완결 tool_use가 이 시간 넘게 지속되고 CPU가 낮으면 승인 대기로 의심한다.
    pub approval_suspect_ms: i64,
    /// 이 값 이하를 CPU 유휴로 본다.
    pub cpu_idle_pct: f32,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            idle_after_ms: 5 * 60 * 1000,
            stale_after_ms: 24 * 60 * 60 * 1000,
            running_grace_ms: 60 * 1000,
            approval_suspect_ms: 30 * 1000,
            cpu_idle_pct: 5.0,
        }
    }
}
