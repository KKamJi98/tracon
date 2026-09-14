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

/// jsonl 조각에서 마지막 대화 엔트리 요약을 만든다.
/// 메타 엔트리와 sidechain 엔트리는 last_kind 판정에서 제외한다.
pub fn parse_tail(chunk: &str) -> Option<TailSummary> {
    let mut summary: Option<TailSummary> = None;
    let mut pending: Vec<String> = Vec::new();

    for line in chunk.lines() {
        let Ok(entry) = serde_json::from_str::<RawEntry>(line) else {
            continue;
        };
        if entry.kind != "user" && entry.kind != "assistant" {
            continue;
        }
        let Some(message) = entry.message else {
            continue;
        };

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
            continue;
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

        summary = Some(TailSummary {
            last_kind: kind,
            last_ts_ms: entry
                .timestamp
                .as_deref()
                .and_then(parse_ts_ms)
                .unwrap_or(0),
            pending_tool_use: 0,
            usage: message.usage.map(|u| Usage {
                input: u.input_tokens,
                cache_read: u.cache_read_input_tokens,
                cache_creation: u.cache_creation_input_tokens,
            }),
            model: message.model,
            cwd: entry.cwd,
        });
    }

    summary.map(|mut s| {
        s.pending_tool_use = pending.len();
        s
    })
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
}
