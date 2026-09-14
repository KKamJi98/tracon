#![allow(dead_code)]

#[derive(Debug, Clone, Copy)]
pub struct Thresholds {
    /// Waiting이 이 시간을 넘기면 Idle로 강등한다.
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
            idle_after_ms: 30 * 60 * 1000,
            stale_after_ms: 24 * 60 * 60 * 1000,
            running_grace_ms: 60 * 1000,
            approval_suspect_ms: 30 * 1000,
            cpu_idle_pct: 5.0,
        }
    }
}
