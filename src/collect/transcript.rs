use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    AssistantText,
    AssistantToolUse,
    UserToolResult,
    UserText,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input: u64,
    pub cache_read: u64,
    pub cache_creation: u64,
}

impl Usage {
    pub fn total(&self) -> u64 {
        self.input + self.cache_read + self.cache_creation
    }
}

#[derive(Debug, Clone)]
pub struct TailSummary {
    pub last_kind: EntryKind,
    pub last_ts_ms: i64,
    pub pending_tool_use: usize,
    pub usage: Option<Usage>,
    pub model: Option<String>,
    #[allow(dead_code)]
    pub cwd: Option<String>,
}

#[derive(Deserialize)]
struct RawEntry {
    #[serde(rename = "type")]
    kind: String,
    timestamp: Option<String>,
    cwd: Option<String>,
    #[serde(default)]
    #[serde(rename = "isSidechain")]
    is_sidechain: bool,
    message: Option<RawMessage>,
}

#[derive(Deserialize)]
struct RawMessage {
    model: Option<String>,
    #[serde(default)]
    content: Vec<RawBlock>,
    usage: Option<RawUsage>,
}

#[derive(Deserialize)]
struct RawBlock {
    #[serde(rename = "type")]
    kind: String,
    id: Option<String>,
    tool_use_id: Option<String>,
}

#[derive(Deserialize)]
struct RawUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
}

pub(crate) fn parse_ts_ms(ts: &str) -> Option<i64> {
    // 형식: 2026-09-15T00:00:03.000Z. 외부 시간 크레이트 없이 고정 폭으로 파싱한다.
    let bytes = ts.as_bytes();
    if bytes.len() < 20 || bytes[4] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let num = |a: usize, b: usize| ts.get(a..b)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, s) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    let ms = ts
        .get(20..23)
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0);
    let days = days_from_civil(y, mo, d);
    Some(((days * 86_400 + h * 3_600 + mi * 60 + s) * 1000) + ms)
}

/// Howard Hinnant의 days_from_civil 알고리즘.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// 대화 엔트리 한 줄에서 뽑아낸, sidechain이 아닌 최신 상태 조각.
#[derive(Debug, Clone)]
struct LatestEntry {
    kind: EntryKind,
    ts_ms: i64,
    usage: Option<Usage>,
    model: Option<String>,
    cwd: Option<String>,
}

/// 한 줄을 파싱해 `pending`(미완결 tool_use id 집합)을 갱신하고, sidechain이 아닌
/// user/assistant 대화 엔트리면 그 내용을 반환한다. 메타 엔트리·깨진 줄·sidechain
/// 엔트리는 `None`을 반환하되 `pending`은 계속 갱신될 수 있다(tool_use/tool_result는
/// sidechain 여부와 무관하게 집계한다).
fn process_line(line: &str, pending: &mut Vec<String>) -> Option<LatestEntry> {
    let entry: RawEntry = serde_json::from_str(line).ok()?;
    if entry.kind != "user" && entry.kind != "assistant" {
        return None;
    }
    let message = entry.message?;

    for block in &message.content {
        match block.kind.as_str() {
            "tool_use" => {
                if let Some(id) = &block.id {
                    pending.push(id.clone());
                }
            }
            "tool_result" => {
                if let Some(id) = &block.tool_use_id {
                    pending.retain(|p| p != id);
                }
            }
            _ => {}
        }
    }

    if entry.is_sidechain {
        return None;
    }

    let kind = match (
        entry.kind.as_str(),
        message.content.first().map(|b| b.kind.as_str()),
    ) {
        ("assistant", Some("tool_use")) => EntryKind::AssistantToolUse,
        ("assistant", _) => EntryKind::AssistantText,
        ("user", Some("tool_result")) => EntryKind::UserToolResult,
        _ => EntryKind::UserText,
    };

    Some(LatestEntry {
        kind,
        ts_ms: entry
            .timestamp
            .as_deref()
            .and_then(parse_ts_ms)
            .unwrap_or(0),
        usage: message.usage.map(|u| Usage {
            input: u.input_tokens,
            cache_read: u.cache_read_input_tokens,
            cache_creation: u.cache_creation_input_tokens,
        }),
        model: message.model,
        cwd: entry.cwd,
    })
}

/// jsonl 조각에서 마지막 대화 엔트리 요약을 만든다.
/// 메타 엔트리와 sidechain 엔트리는 last_kind 판정에서 제외한다.
///
/// 한 조각짜리 스냅샷에만 쓰는 순수 함수다. tick을 이어가며 상태를 기억해야 하면
/// `TranscriptTracker`를 쓴다 - `Collector`가 실제로 쓰는 것은 그쪽이다.
#[allow(dead_code)]
pub fn parse_tail(chunk: &str) -> Option<TailSummary> {
    let mut pending: Vec<String> = Vec::new();
    let mut latest: Option<LatestEntry> = None;

    for line in chunk.lines() {
        if let Some(entry) = process_line(line, &mut pending) {
            latest = Some(entry);
        }
    }

    latest.map(|l| TailSummary {
        last_kind: l.kind,
        last_ts_ms: l.ts_ms,
        pending_tool_use: pending.len(),
        usage: l.usage,
        model: l.model,
        cwd: l.cwd,
    })
}

/// 여러 tick에 걸친 tail 조각을 누적해 상태를 기억한다. `Tailer`는 매 호출마다
/// 새로 늘어난 바이트만 돌려주므로, 조각이 비어 있던 tick에도 세션의 진짜 상태와
/// 마지막 변경 시각을 잃지 않으려면 파싱 결과를 세션당 하나씩 들고 있어야 한다.
/// 미완결 tool_use id 집합도 마찬가지로 tick 경계를 넘어 유지한다.
#[derive(Debug, Clone, Default)]
pub struct TranscriptTracker {
    pending: Vec<String>,
    latest: Option<LatestEntry>,
}

impl TranscriptTracker {
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self::default()
    }

    /// 새로 읽은 델타를 누적 상태에 접어 넣는다. 빈 조각(`chunk.is_empty()`이거나
    /// 파싱 가능한 대화 엔트리가 없는 조각)은 상태를 바꾸지 않는다.
    pub fn apply(&mut self, chunk: &str) {
        for line in chunk.lines() {
            if let Some(entry) = process_line(line, &mut self.pending) {
                self.latest = Some(entry);
            }
        }
    }

    /// 지금까지 누적된 상태로 요약을 만든다. 대화 엔트리를 한 번도 못 봤으면 `None`.
    pub fn summary(&self) -> Option<TailSummary> {
        let latest = self.latest.as_ref()?;
        Some(TailSummary {
            last_kind: latest.kind,
            last_ts_ms: latest.ts_ms,
            pending_tool_use: self.pending.len(),
            usage: latest.usage,
            model: latest.model.clone(),
            cwd: latest.cwd.clone(),
        })
    }
}

/// 컨텍스트 윈도우 크기를 정한다.
/// `message.model`에 1M 변종 표시가 없다는 것이 실측으로 확인되었으므로,
/// 관측값이 200k를 넘으면 1M 세션으로 보정한다.
pub fn window_for(_model: &str, observed_total: u64) -> u64 {
    if observed_total > 200_000 {
        1_000_000
    } else {
        200_000
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WAITING: &str = include_str!("../../tests/fixtures/claude_tail_waiting.jsonl");
    const PENDING: &str = include_str!("../../tests/fixtures/claude_tail_tool_pending.jsonl");

    #[test]
    fn meta_entries_are_ignored() {
        let s = parse_tail(WAITING).expect("summary");
        assert_eq!(s.last_kind, EntryKind::AssistantText);
    }

    #[test]
    fn usage_sums_three_fields() {
        let s = parse_tail(WAITING).expect("summary");
        assert_eq!(s.usage.expect("usage").total(), 40_510);
    }

    #[test]
    fn unmatched_tool_use_is_counted() {
        let s = parse_tail(PENDING).expect("summary");
        assert_eq!(s.pending_tool_use, 1);
        assert_eq!(s.last_kind, EntryKind::AssistantToolUse);
    }

    #[test]
    fn matched_tool_use_is_not_pending() {
        let s = parse_tail(WAITING).expect("summary");
        assert_eq!(s.pending_tool_use, 0);
    }

    #[test]
    fn cwd_and_model_are_extracted() {
        let s = parse_tail(WAITING).expect("summary");
        assert_eq!(s.cwd.as_deref(), Some("/home/dev/project-a"));
        assert_eq!(s.model.as_deref(), Some("claude-opus-5"));
    }

    #[test]
    fn broken_lines_are_skipped_not_fatal() {
        let input = format!("{{not json\n{WAITING}");
        assert!(parse_tail(&input).is_some());
    }

    #[test]
    fn window_defaults_to_200k_and_widens_on_observation() {
        assert_eq!(window_for("claude-opus-5", 100_000), 200_000);
        assert_eq!(window_for("claude-opus-5", 628_721), 1_000_000);
    }

    #[test]
    fn sidechain_entries_do_not_become_the_last_kind() {
        let side = r#"{"type":"assistant","timestamp":"2026-09-15T00:00:09.000Z","isSidechain":true,"message":{"content":[{"type":"text","text":"sub"}]}}"#;
        let input = format!("{WAITING}\n{side}");
        let s = parse_tail(&input).expect("summary");
        assert_eq!(s.last_kind, EntryKind::AssistantText);
        assert_eq!(s.last_ts_ms, 1_789_430_403_000);
    }

    #[test]
    fn tracker_keeps_state_when_a_later_delta_is_empty() {
        let mut t = TranscriptTracker::new();
        t.apply(WAITING);
        let first = t.summary().expect("summary after first chunk");

        // Tailer가 아무 것도 새로 읽지 못한 tick은 빈 문자열을 넘긴다.
        t.apply("");
        let second = t.summary().expect("summary survives an empty delta");

        assert_eq!(second.last_kind, first.last_kind);
        assert_eq!(second.last_ts_ms, first.last_ts_ms);
        assert_eq!(second.pending_tool_use, first.pending_tool_use);
    }

    #[test]
    fn tracker_pending_tool_use_spans_chunks() {
        let mut t = TranscriptTracker::new();
        t.apply(PENDING);
        assert_eq!(t.summary().expect("summary").pending_tool_use, 1);

        // t9의 tool_result가 나중 delta로 도착해도 pending 집합은 tick 경계를 넘어 갱신된다.
        let result = r#"{"type":"user","timestamp":"2026-09-15T00:00:06.000Z","isSidechain":false,"message":{"content":[{"type":"tool_result","tool_use_id":"t9"}]}}"#;
        t.apply(result);

        assert_eq!(t.summary().expect("summary").pending_tool_use, 0);
    }
}
