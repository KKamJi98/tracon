use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    AssistantText,
    AssistantToolUse,
    UserToolResult,
    UserText,
    /// 사용자가 Esc로 응답을 끊었다. 형태는 user 엔트리지만 새 지시가 아니라
    /// "에이전트가 멈췄다"는 기록이라, 다음 입력을 기다리는 상태로 읽어야 한다.
    UserInterrupted,
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
    /// 세션에 붙은 이름. claude가 지어 준 것(`ai-title`)이거나 사용자가 직접 지은
    /// 것(`custom-title`)이다. 둘 다 없으면 `None` - 대화 내용으로 이름을 지어내지
    /// 않는다.
    pub title: Option<String>,
    /// 이 세션을 누가 몰고 있는지. 사람이 터미널에서 친 세션은 `cli`, SDK가 띄운
    /// 세션(보안 리뷰 같은 자동 서브에이전트)은 `sdk-py` 같은 값이다. 아직 못 본
    /// 세션은 `None` - 모르면 사람이 쓰는 세션으로 취급한다.
    pub entrypoint: Option<String>,
}

#[derive(Deserialize)]
struct RawEntry {
    #[serde(rename = "type")]
    kind: String,
    timestamp: Option<String>,
    cwd: Option<String>,
    entrypoint: Option<String>,
    #[serde(default)]
    #[serde(rename = "isSidechain")]
    is_sidechain: bool,
    /// claude가 스스로 끼워 넣는 알림(스킬 경로, 이미지 첨부, 세션 이름 공지)에
    /// 붙는 표시. 형태는 user 엔트리지만 사용자가 말한 것이 아니다.
    #[serde(default)]
    #[serde(rename = "isMeta")]
    is_meta: bool,
    /// claude가 모델을 부르지 않고 직접 적은 알림(세션 한도, 네트워크 끊김 등)에
    /// 붙는 표시. 모델은 `<synthetic>`이고 usage는 전부 0이다.
    #[serde(default)]
    #[serde(rename = "isApiErrorMessage")]
    is_api_error: bool,
    message: Option<RawMessage>,
}

#[derive(Deserialize)]
struct RawMessage {
    model: Option<String>,
    #[serde(default)]
    content: RawContent,
    usage: Option<RawUsage>,
}

/// `message.content`는 블록 배열일 때도 있고 그냥 문자열일 때도 있다. 한쪽 모양만
/// 받으면 반대쪽 엔트리는 줄 전체가 역직렬화에 실패해 통째로 버려진다 - 실측
/// transcript에서 문자열 형태의 `user` 엔트리가 상당수였다.
#[derive(Deserialize)]
#[serde(untagged)]
enum RawContent {
    Blocks(Vec<RawBlock>),
    /// 본문은 인터럽트 표식을 알아보는 데만 쓰고 화면에는 내보내지 않는다 -
    /// transcript 내용을 노출하지 않는 것이 설계 비목표다.
    Text(String),
}

impl Default for RawContent {
    fn default() -> Self {
        RawContent::Blocks(Vec::new())
    }
}

impl RawContent {
    /// 문자열 content에는 tool_use/tool_result 블록이 있을 수 없으므로 빈 슬라이스다.
    fn blocks(&self) -> &[RawBlock] {
        match self {
            RawContent::Blocks(blocks) => blocks,
            RawContent::Text(_) => &[],
        }
    }

    /// 첫 본문 조각. 인터럽트 표식 판정에만 쓴다.
    fn leading_text(&self) -> Option<&str> {
        match self {
            RawContent::Text(s) => Some(s.as_str()),
            RawContent::Blocks(blocks) => blocks.first()?.text.as_deref(),
        }
    }
}

/// claude가 중단된 응답 자리에 남기는 표식.
const INTERRUPT_MARKER: &str = "[Request interrupted by user";

/// 모델을 부르지 않고 만들어진 엔트리에 붙는 모델 이름.
const SYNTHETIC_MODEL: &str = "<synthetic>";

#[derive(Deserialize)]
struct RawBlock {
    #[serde(rename = "type")]
    kind: String,
    id: Option<String>,
    tool_use_id: Option<String>,
    /// 인터럽트 표식을 알아보는 데만 쓴다. 화면에 내보내지 않는다 - transcript
    /// 내용을 노출하지 않는 것이 이 도구의 비목표다.
    text: Option<String>,
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

#[derive(Deserialize)]
struct RawTitle {
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "customTitle")]
    custom_title: Option<String>,
    #[serde(rename = "aiTitle")]
    ai_title: Option<String>,
}

#[derive(Deserialize)]
struct RawModelAttachment {
    attachment: Option<RawAttachment>,
}

#[derive(Deserialize)]
struct RawAttachment {
    #[serde(rename = "type")]
    kind: String,
    identity: Option<RawIdentity>,
}

#[derive(Deserialize)]
struct RawIdentity {
    #[serde(rename = "modelId")]
    model_id: Option<String>,
}

#[derive(Deserialize)]
struct RawEntrypoint {
    entrypoint: Option<String>,
}

/// 이 세션을 몬 주체. 사람이 터미널에서 친 세션은 `cli`, SDK가 프로그램으로 띄운
/// 세션(보안 리뷰 같은 자동 서브에이전트)은 `sdk-py` 같은 값이 온다.
fn parse_entrypoint(line: &str) -> Option<String> {
    serde_json::from_str::<RawEntrypoint>(line).ok()?.entrypoint
}

/// 모델 신원 줄이면 `modelId`를 돌려준다. claude는 세션을 시작할 때와 `/model`로
/// 모델을 바꿀 때만 이 줄을 적는다.
fn parse_model_line(line: &str) -> Option<String> {
    let raw: RawModelAttachment = serde_json::from_str(line).ok()?;
    let attachment = raw.attachment?;
    if attachment.kind != "model" {
        return None;
    }
    attachment.identity?.model_id
}

/// `modelId`가 밝히는 컨텍스트 윈도우. `[1m]` 변종만 1M이고 나머지는 200k다.
/// 관측 토큰 수로 짐작하는 [`window_for`]와 달리 이건 사실이다.
pub fn window_for_model_id(model_id: &str) -> u64 {
    if model_id.ends_with("[1m]") {
        1_000_000
    } else {
        200_000
    }
}

/// 세션 이름 줄이면 `(사용자가 직접 지었는가, 이름)`을 돌려준다. claude는 `ai-title`을
/// 몇 턴마다 다시 적어 주므로, 64KB tail 안에서 거의 항상 최신 이름을 만난다.
fn parse_title_line(line: &str) -> Option<(bool, String)> {
    let raw: RawTitle = serde_json::from_str(line).ok()?;
    let (custom, title) = match raw.kind.as_str() {
        "custom-title" => (true, raw.custom_title?),
        "ai-title" => (false, raw.ai_title?),
        _ => return None,
    };
    let title = title.trim();
    if title.is_empty() {
        return None;
    }
    Some((custom, title.to_string()))
}

/// 사용자가 직접 지은 이름은 claude가 지어 준 이름에 밀리지 않는다.
fn keep_title(slot: &mut Option<(bool, String)>, found: (bool, String)) {
    let outranked = matches!(slot, Some((true, _))) && !found.0;
    if !outranked {
        *slot = Some(found);
    }
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
    entrypoint: Option<String>,
    /// 모델을 부르지 않고 만들어진 엔트리. 대화의 일부이긴 하지만 컨텍스트
    /// 측정값이 아니므로 마지막 실측을 덮으면 안 된다.
    synthetic: bool,
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

    for block in message.content.blocks() {
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

    // sidechain(서브에이전트)과 claude가 끼워 넣은 알림은 대화 차례가 아니다.
    // tool_use/tool_result 집계는 위에서 이미 끝났으므로 여기서 걸러도 안전하다.
    if entry.is_sidechain || entry.is_meta {
        return None;
    }

    let kind = match (
        entry.kind.as_str(),
        message.content.blocks().first().map(|b| b.kind.as_str()),
    ) {
        ("assistant", Some("tool_use")) => EntryKind::AssistantToolUse,
        ("assistant", _) => EntryKind::AssistantText,
        ("user", Some("tool_result")) => EntryKind::UserToolResult,
        ("user", _)
            if message
                .content
                .leading_text()
                .is_some_and(|s| s.starts_with(INTERRUPT_MARKER)) =>
        {
            EntryKind::UserInterrupted
        }
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
        synthetic: entry.is_api_error || message.model.as_deref() == Some(SYNTHETIC_MODEL),
        model: message.model,
        cwd: entry.cwd,
        entrypoint: entry.entrypoint,
    })
}

/// jsonl 조각에서 마지막 대화 엔트리 요약을 만든다.
/// 메타 엔트리와 sidechain 엔트리는 last_kind 판정에서 제외한다.
///
/// 한 조각짜리 스냅샷에만 쓰는 순수 함수다. tick을 이어가며 상태를 기억해야 하면
/// `TranscriptTracker`를 쓴다 - `Collector`가 실제로 쓰는 것은 그쪽이다.
#[allow(dead_code)]
pub fn parse_tail(chunk: &str) -> Option<TailSummary> {
    let mut tracker = TranscriptTracker::new();
    tracker.apply(chunk);
    tracker.summary()
}

/// 여러 tick에 걸친 tail 조각을 누적해 상태를 기억한다. `Tailer`는 매 호출마다
/// 새로 늘어난 바이트만 돌려주므로, 조각이 비어 있던 tick에도 세션의 진짜 상태와
/// 마지막 변경 시각을 잃지 않으려면 파싱 결과를 세션당 하나씩 들고 있어야 한다.
/// 미완결 tool_use id 집합도 마찬가지로 tick 경계를 넘어 유지한다.
#[derive(Debug, Clone, Default)]
pub struct TranscriptTracker {
    pending: Vec<String>,
    latest: Option<LatestEntry>,
    /// 한 번 본 이름은 계속 들고 있는다. 이름 줄은 매 턴 나오지 않으므로, tick마다
    /// 새 델타만 보는 이 tracker가 기억하지 않으면 이름이 깜빡인다.
    title: Option<(bool, String)>,
    /// 마지막으로 본 모델 신원. 컨텍스트 윈도우 크기를 짐작 대신 사실로 아는
    /// 유일한 근거다.
    model_id: Option<String>,
    /// 이 세션을 몬 주체. 엔트리마다 실려 오지만 세션 내내 같은 값이다.
    entrypoint: Option<String>,
    /// 마지막으로 **측정된** 사용량과 모델. 사용자가 메시지를 보내면 그 user 엔트리가
    /// 가장 최근 엔트리가 되는데, user 엔트리에는 usage도 model도 없다. 최신 엔트리만
    /// 보면 그 순간 컨텍스트 사용률과 모델이 통째로 `-`가 된다 - 답이 오기 전까지.
    /// 컨텍스트는 사용자가 타이핑했다고 해서 미지수가 되지 않으므로 마지막 측정값을 쥔다.
    usage: Option<Usage>,
    model: Option<String>,
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
            if self.take_meta(line) {
                continue;
            }
            if let Some(entry) = process_line(line, &mut self.pending) {
                if self.entrypoint.is_none() {
                    self.entrypoint = entry.entrypoint.clone();
                }
                // 세션 한도 알림 같은 합성 엔트리는 대화에는 남지만 컨텍스트
                // 측정값이 아니다. usage가 전부 0이고 모델이 `<synthetic>`이라,
                // 그대로 받으면 CTX%가 0%로, MODEL이 `<synthetic>`으로 덮인다.
                if !entry.synthetic {
                    if entry.usage.is_some() {
                        self.usage = entry.usage;
                    }
                    if entry.model.is_some() {
                        self.model = entry.model.clone();
                    }
                }
                self.latest = Some(entry);
            }
        }
    }

    /// 대화 상태는 그대로 두고 메타 줄(이름, 모델 신원)만 접어 넣는다. transcript
    /// 앞부분을 한 번 읽을 때 쓴다 - 거기 있는 대화 엔트리는 이미 지나간 것이라,
    /// 미완결 tool_use 집합이나 "마지막 엔트리" 판정에 섞이면 상태가 망가진다.
    pub fn apply_meta(&mut self, chunk: &str) {
        for line in chunk.lines() {
            self.take_meta(line);
        }
    }

    /// 메타 줄이면 받아 두고 true. 대화 엔트리면 false.
    fn take_meta(&mut self, line: &str) -> bool {
        // entrypoint 는 메타 줄에도 대화 줄에도 실려 온다. 어느 쪽이든 한 번만
        // 보면 되고, 세션 내내 바뀌지 않는다.
        if self.entrypoint.is_none() {
            if let Some(ep) = parse_entrypoint(line) {
                self.entrypoint = Some(ep);
            }
        }
        if let Some(found) = parse_title_line(line) {
            keep_title(&mut self.title, found);
            return true;
        }
        if let Some(model_id) = parse_model_line(line) {
            self.model_id = Some(model_id);
            return true;
        }
        false
    }

    /// 모델 신원을 실제로 본 세션만 윈도우 크기를 사실로 돌려준다. 못 봤으면
    /// `None`이고, 호출부가 관측 토큰 수로 짐작하는 폴백으로 내려간다.
    pub fn context_window(&self) -> Option<u64> {
        self.model_id.as_deref().map(window_for_model_id)
    }

    /// 지금까지 누적된 상태로 요약을 만든다. 대화 엔트리를 한 번도 못 봤으면 `None`.
    pub fn summary(&self) -> Option<TailSummary> {
        let latest = self.latest.as_ref()?;
        Some(TailSummary {
            last_kind: latest.kind,
            last_ts_ms: latest.ts_ms,
            pending_tool_use: self.pending.len(),
            usage: self.usage,
            model: self.model.clone(),
            cwd: latest.cwd.clone(),
            title: self.title.as_ref().map(|(_, name)| name.clone()),
            entrypoint: self.entrypoint.clone(),
        })
    }
}

/// 모델 신원을 못 본 세션의 마지막 폴백. `message.model`에는 1M 변종 표시가 없어
/// 관측 토큰 수로 짐작할 수밖에 없고, 관측값이 200k를 넘으면 1M 세션으로 본다.
///
/// 이 짐작은 1M 세션이 아직 200k를 안 넘겼을 때 반드시 틀린다 - 198k를 200k
/// 윈도우의 99%로 읽는다. 그래서 [`window_for_model_id`]가 먼저고, 이건 뒤다.
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

    /// claude는 세션 이름을 `ai-title`로, 사용자가 직접 붙인 이름은 `custom-title`로
    /// 적는다. 둘 다 대화 엔트리가 아니라서 `process_line`이 버리는 줄이다.
    #[test]
    fn ai_title_becomes_the_session_name() {
        let mut tr = TranscriptTracker::new();
        tr.apply(WAITING);
        tr.apply(r#"{"type":"ai-title","aiTitle":"context window budget","sessionId":"u1"}"#);
        assert_eq!(
            tr.summary().expect("summary").title.as_deref(),
            Some("context window budget")
        );
    }

    /// 사용자가 직접 붙인 이름이 claude가 지어 준 이름보다 우선한다 - 순서와 무관하게.
    #[test]
    fn a_custom_title_outranks_an_ai_title() {
        let lines = [
            r#"{"type":"custom-title","customTitle":"deploy gate","sessionId":"u1"}"#,
            r#"{"type":"ai-title","aiTitle":"guessed name","sessionId":"u1"}"#,
        ];
        let mut tr = TranscriptTracker::new();
        tr.apply(WAITING);
        for line in lines {
            tr.apply(line);
        }
        assert_eq!(
            tr.summary().expect("summary").title.as_deref(),
            Some("deploy gate")
        );
    }

    /// 이름 줄은 매 턴 나오지 않는다. 델타만 보는 tracker가 기억하지 않으면
    /// 이름이 한 tick 만에 사라진다.
    #[test]
    fn the_session_name_survives_later_chunks_without_a_title_line() {
        let mut tr = TranscriptTracker::new();
        tr.apply(r#"{"type":"ai-title","aiTitle":"context window budget","sessionId":"u1"}"#);
        tr.apply(WAITING);
        assert_eq!(
            tr.summary().expect("summary").title.as_deref(),
            Some("context window budget")
        );
    }

    /// 이름이 없는 세션에 대화 내용으로 이름을 지어내지 않는다.
    #[test]
    fn a_session_without_a_title_line_has_no_name() {
        assert_eq!(parse_tail(WAITING).expect("summary").title, None);
    }

    /// 빈 이름은 이름이 아니다 - claude가 빈 문자열을 적는 경우가 있다.
    #[test]
    fn an_empty_title_is_not_a_name() {
        let mut tr = TranscriptTracker::new();
        tr.apply(WAITING);
        tr.apply(r#"{"type":"ai-title","aiTitle":"  ","sessionId":"u1"}"#);
        assert_eq!(tr.summary().expect("summary").title, None);
    }

    /// 1M 세션이 아직 200k를 안 넘겼을 때 관측값만으로는 200k 세션과 구분되지
    /// 않는다 - 198k를 200k 윈도우의 99%로 읽는다. 모델 신원이 그걸 끊어 준다.
    #[test]
    fn model_identity_decides_the_window_instead_of_the_token_count() {
        const MODEL_LINE: &str = r#"{"type":"attachment","attachment":{"type":"model","identity":{"modelId":"claude-opus-5[1m]","marketingName":"Opus 5 (1M context)"}}}"#;
        let mut tr = TranscriptTracker::new();
        tr.apply(WAITING);
        assert_eq!(tr.context_window(), None, "못 본 모델을 지어내면 안 된다");

        tr.apply(MODEL_LINE);
        assert_eq!(tr.context_window(), Some(1_000_000));
        // 짐작 폴백은 같은 토큰 수를 200k 윈도우로 읽는다 - 이게 99% 오독의 원인이다.
        assert_eq!(window_for("claude-opus-5", 198_946), 200_000);
    }

    #[test]
    fn a_plain_model_id_means_the_200k_window() {
        assert_eq!(window_for_model_id("claude-opus-5"), 200_000);
        assert_eq!(window_for_model_id("claude-opus-5[1m]"), 1_000_000);
        assert_eq!(window_for_model_id("claude-haiku-4-5-20251001"), 200_000);
    }

    /// `/model`로 모델을 바꾸면 그 줄이 뒤에 다시 적힌다 - 나중 것이 이긴다.
    #[test]
    fn a_later_model_line_replaces_the_earlier_one() {
        let lines = [
            r#"{"type":"attachment","attachment":{"type":"model","identity":{"modelId":"claude-opus-5[1m]"}}}"#,
            r#"{"type":"attachment","attachment":{"type":"model","identity":{"modelId":"claude-opus-5"}}}"#,
        ];
        let mut tr = TranscriptTracker::new();
        for line in lines {
            tr.apply(line);
        }
        assert_eq!(tr.context_window(), Some(200_000));
    }

    /// 파일 앞부분은 모델 신원만 건져 오고 대화 상태는 건드리지 않는다. 거기 있는
    /// 미완결 tool_use를 집계하면 세션이 영영 승인 대기로 굳는다.
    #[test]
    fn apply_meta_takes_the_model_without_touching_conversation_state() {
        let mut tr = TranscriptTracker::new();
        tr.apply_meta(PENDING);
        tr.apply_meta(
            r#"{"type":"attachment","attachment":{"type":"model","identity":{"modelId":"claude-opus-5[1m]"}}}"#,
        );
        assert_eq!(tr.context_window(), Some(1_000_000));
        assert!(
            tr.summary().is_none(),
            "apply_meta는 대화 엔트리를 세면 안 된다"
        );

        // 그 뒤 진짜 tail을 접어 넣으면 대화 상태는 그때부터 정상으로 쌓인다.
        tr.apply(WAITING);
        let s = tr.summary().expect("summary");
        assert_eq!(
            s.pending_tool_use, 0,
            "앞부분의 미완결 tool_use가 새어 들어왔다"
        );
    }

    /// 사용자가 메시지를 보내면 그 user 엔트리가 마지막 엔트리가 된다. user 엔트리에는
    /// usage도 model도 없으니, 최신 엔트리만 보면 답이 오기 전까지 CTX%와 MODEL이
    /// 통째로 `-`가 된다. 컨텍스트는 사용자가 타이핑했다고 미지수가 되지 않는다.
    #[test]
    fn a_trailing_user_turn_does_not_erase_the_measured_context() {
        const USER_TURN: &str = r#"{"type":"user","timestamp":"2026-09-15T06:24:19.711Z","message":{"role":"user","content":"다음 질문"}}"#;
        let mut tr = TranscriptTracker::new();
        tr.apply(WAITING);
        let before = tr.summary().expect("summary");
        let measured = before.usage.expect("usage").total();
        let model = before.model.clone().expect("model");

        tr.apply(USER_TURN);
        let after = tr.summary().expect("summary");
        assert_eq!(
            after.last_kind,
            EntryKind::UserText,
            "마지막 엔트리는 사용자 차례다"
        );
        assert_eq!(
            after.usage.expect("usage").total(),
            measured,
            "마지막 측정값을 잃으면 CTX%가 빈칸이 된다"
        );
        assert_eq!(after.model.as_deref(), Some(model.as_str()));
    }

    /// claude는 자기가 주입하는 알림(스킬 경로, 이미지 첨부, 세션 이름 공지)을
    /// `isMeta: true`인 user 엔트리로 적는다. 사용자가 말한 것이 아니므로 마지막
    /// 차례로 세면 안 된다 - 세면 아무도 안 건드린 세션의 LAST가 방금으로 되돌아가고,
    /// 60초 grace가 지난 뒤 상태가 Unknown으로 떨어진다.
    #[test]
    fn an_is_meta_entry_is_not_a_user_turn() {
        const NOTICE: &str = r#"{"type":"user","isMeta":true,"timestamp":"2026-09-15T06:34:18.734Z","message":{"role":"user","content":"<system-reminder> The user named this session \"x\". </system-reminder>"}}"#;
        let mut tr = TranscriptTracker::new();
        tr.apply(WAITING);
        let before = tr.summary().expect("summary");

        tr.apply(NOTICE);
        let after = tr.summary().expect("summary");
        assert_eq!(
            after.last_kind, before.last_kind,
            "알림이 마지막 차례를 덮었다"
        );
        assert_eq!(
            after.last_ts_ms, before.last_ts_ms,
            "알림이 LAST를 되돌렸다"
        );
    }

    /// Esc로 응답을 끊으면 claude가 `[Request interrupted by user]`를 적는다.
    /// 그 순간 에이전트는 멈췄고 세션은 다음 입력을 기다린다 - 판단 불가가 아니다.
    #[test]
    fn an_interrupt_marker_reads_as_waiting_for_input() {
        const INTERRUPT: &str = r#"{"type":"user","timestamp":"2026-09-15T06:32:37.202Z","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#;
        let mut tr = TranscriptTracker::new();
        tr.apply(WAITING);
        tr.apply(INTERRUPT);
        assert_eq!(
            tr.summary().expect("summary").last_kind,
            EntryKind::UserInterrupted
        );
    }

    /// 세션 한도에 걸리면 claude가 모델 `<synthetic>`, usage 전부 0인 assistant
    /// 엔트리를 적는다. 그걸 실측으로 받으면 MODEL이 `<synthetic>`으로, CTX%가 0%로
    /// 덮인다 - 컨텍스트는 한도에 걸렸다고 비워지지 않는다.
    #[test]
    fn a_rate_limit_notice_does_not_overwrite_the_measured_context() {
        const LIMIT: &str = r#"{"type":"assistant","timestamp":"2026-09-15T19:41:00.000Z","isApiErrorMessage":true,"error":"rate_limit","message":{"role":"assistant","model":"<synthetic>","content":[{"type":"text","text":"You have hit your session limit"}],"usage":{"input_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#;
        let mut tr = TranscriptTracker::new();
        tr.apply(WAITING);
        let before = tr.summary().expect("summary");
        let measured = before.usage.expect("usage").total();
        let model = before.model.clone().expect("model");

        tr.apply(LIMIT);
        let after = tr.summary().expect("summary");
        assert_eq!(
            after.usage.expect("usage").total(),
            measured,
            "한도 알림이 컨텍스트를 0으로 덮었다"
        );
        assert_eq!(
            after.model.as_deref(),
            Some(model.as_str()),
            "한도 알림이 MODEL을 <synthetic>으로 덮었다"
        );
    }

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

    /// 실측 transcript에서 `user` 엔트리의 `content`는 블록 배열일 때도 있고 그냥
    /// 문자열일 때도 있다(12개 파일 표본에서 문자열 형태가 348건). 문자열 모양을
    /// 못 읽으면 사용자 프롬프트가 통째로 사라져 계층 0이 그 세션의 진행을 못 본다.
    #[test]
    fn user_entry_with_plain_string_content_is_parsed() {
        let line = r#"{"type":"user","timestamp":"2026-09-15T00:00:05.000Z","isSidechain":false,"message":{"role":"user","content":"please fix the build"}}"#;
        let s = parse_tail(line).expect("문자열 content를 가진 user 엔트리도 파싱돼야 한다");
        assert_eq!(s.last_kind, EntryKind::UserText);
        assert_eq!(s.last_ts_ms, 1_789_430_405_000);
    }

    #[test]
    fn assistant_entry_with_plain_string_content_is_parsed() {
        let line = r#"{"type":"assistant","timestamp":"2026-09-15T00:00:06.000Z","isSidechain":false,"message":{"model":"claude-opus-5","content":"done"}}"#;
        let s = parse_tail(line).expect("문자열 content를 가진 assistant 엔트리도 파싱돼야 한다");
        assert_eq!(s.last_kind, EntryKind::AssistantText);
    }

    #[test]
    fn block_array_content_still_parses() {
        let s = parse_tail(WAITING).expect("summary");
        assert_eq!(s.last_kind, EntryKind::AssistantText);
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
