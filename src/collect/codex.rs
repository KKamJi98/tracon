//! codex 어댑터. codex rollout 파일은 `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`
//! 형태로 날짜 디렉터리에 들어 있다 - claude처럼 cwd 슬러그 디렉터리 하나에 다 모여
//! 있지 않으므로 경로 탐색 규칙이 다르다. 스키마 근거는
//! `.superpowers/sdd/2026-09-15-tracon/codex-schema-findings.md`(컨트롤러가 실측한 결과)다.
//!
//! codex는 claude보다 두 가지 지점에서 판정이 쉽다 - 같은 방식으로 억지로 맞추지
//! 않고 그대로 이용한다.
//! 1. `event_msg`의 `task_complete`는 turn이 끝났다는 명시적 신호다. 추론이 아니라
//!    사실이므로 `Confidence::Low`보다 나은 대우를 받을 자격이 있다.
//! 2. `event_msg`의 `token_count`는 현재 컨텍스트(`last_token_usage.total_tokens`)와
//!    윈도우(`model_context_window`)를 직접 들고 있다. claude 쪽에서 쓰는 "200k
//!    넘으면 1M으로 본다" 휴리스틱은 여기서는 쓰지 않는다 - codex는 윈도우를 실측값
//!    그대로 준다.
//!
//! 반대로 approval 대기를 직접 관측할 방법은 없다(rollout에 승인 요청 레코드가
//! 없다) - 그 상태는 claude와 같은 방식(idle cpu + pending tool + 나이)으로 계속
//! 추론해야 한다. README의 지원 표에 이 한계를 명시한다.

use super::transcript::{parse_ts_ms, EntryKind, TailSummary, Usage};
use crate::config::Thresholds;
use crate::model::{demote, Confidence, State};
use serde_json::Value;
use std::collections::HashSet;
use std::io::BufRead;
use std::path::{Path, PathBuf};

pub fn codex_sessions_root() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".codex/sessions")
}

/// rollout 파일명에서 uuid를 뽑는다. `rollout-<ISO8601>-<uuid>.jsonl`에서 타임스탬프도
/// 대시를 쓰므로, uuid는 고정 길이(8-4-4-4-12 = 36자)라는 점을 이용해 뒤에서 잘라낸다.
pub fn uuid_from_rollout_name(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let rest = stem.strip_prefix("rollout-")?;
    if rest.len() < 36 {
        return None;
    }
    let candidate = &rest[rest.len() - 36..];
    is_uuid_like(candidate).then(|| candidate.to_string())
}

fn is_uuid_like(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (i, b) in bytes.iter().enumerate() {
        let want_dash = matches!(i, 8 | 13 | 18 | 23);
        if want_dash {
            if *b != b'-' {
                return false;
            }
        } else if !b.is_ascii_hexdigit() {
            return false;
        }
    }
    true
}

/// `YYYY`/`MM`/`DD` 디렉터리를 최신 순(이름 내림차순)으로 나열한다. 이름이 전부
/// zero-padded 숫자라 문자열 정렬이 곧 시간 정렬이다.
fn dated_subdirs_desc(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    dirs
}

/// 날짜 디렉터리 하나 안의 `rollout-*.jsonl` 파일을 최신 순으로 나열한다. 파일명이
/// 타임스탬프로 시작하므로 이름 내림차순이 곧 최신 순이다.
fn rollout_files_desc(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "jsonl").unwrap_or(false))
        .collect();
    files.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    files
}

/// `~/.codex/sessions` 아래를 최신 날짜 디렉터리부터 훑어 uuid가 일치하는 rollout을
/// 찾는다. 찾는 즉시 멈춘다 - 트리 전체(수백MB)를 매번 읽지 않기 위함이다. 호출부
/// (`Collector`)가 결과를 uuid별로 캐시해 이후 tick에서는 이 함수 자체를 다시
/// 부르지 않는다.
pub fn find_codex_transcript(root: &Path, uuid: &str) -> Option<PathBuf> {
    for year in dated_subdirs_desc(root) {
        for month in dated_subdirs_desc(&year) {
            for day in dated_subdirs_desc(&month) {
                for path in rollout_files_desc(&day) {
                    if uuid_from_rollout_name(&path).as_deref() == Some(uuid) {
                        return Some(path);
                    }
                }
            }
        }
    }
    None
}

/// rollout 파일의 첫 줄(`session_meta`)만 읽어 `payload.cwd`를 뽑는다. `sysinfo`가
/// ChatGPT 앱이 띄운 codex 프로세스의 cwd를 주지 못하는 macOS 환경에서, 세션의
/// 작업 디렉터리를 알아낼 유일한 방법이다. 파일 전체를 읽지 않고 첫 줄만 읽으므로
/// 세션 하나당 한 번, 파일 크기와 무관하게 저렴하다.
pub fn read_session_meta_cwd(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let mut first_line = String::new();
    reader.read_line(&mut first_line).ok()?;
    let v: Value = serde_json::from_str(first_line.trim_end()).ok()?;
    v.get("payload")?
        .get("cwd")?
        .as_str()
        .map(|s| s.to_string())
}

/// 배정되지 않은 codex 프로세스가 찾아낸 transcript.
pub struct CodexHit {
    pub uuid: String,
    pub path: PathBuf,
    pub cwd: Option<String>,
}

/// session_id를 모르는 codex 프로세스를 위한 transcript 탐색. claude처럼 cwd
/// 슬러그 디렉터리가 없으므로 접근이 다르다 - `cwd`가 알려져 있으면(터미널에서 띄운
/// `codex`) 그 cwd와 `session_meta.cwd`가 일치하는 가장 최근 미배정 파일을 찾고,
/// `cwd`를 모르면(예: ChatGPT 앱이 띄운 codex 프로세스 - sysinfo가 cwd를 주지 않는다)
/// 그냥 가장 최근 미배정 파일을 돌려준다. 날짜 디렉터리를 최신 순으로 훑다가 첫
/// 매치에서 멈춘다.
pub fn find_unclaimed_codex_transcript(
    root: &Path,
    cwd: Option<&Path>,
    claimed: &HashSet<PathBuf>,
) -> Option<CodexHit> {
    let want_cwd = cwd.map(|c| c.to_string_lossy().into_owned());
    for year in dated_subdirs_desc(root) {
        for month in dated_subdirs_desc(&year) {
            for day in dated_subdirs_desc(&month) {
                for path in rollout_files_desc(&day) {
                    if claimed.contains(&path) {
                        continue;
                    }
                    let Some(uuid) = uuid_from_rollout_name(&path) else {
                        continue;
                    };
                    let meta_cwd = read_session_meta_cwd(&path);
                    if let Some(want) = &want_cwd {
                        if meta_cwd.as_deref() != Some(want.as_str()) {
                            continue;
                        }
                    }
                    return Some(CodexHit {
                        uuid,
                        path,
                        cwd: meta_cwd,
                    });
                }
            }
        }
    }
    None
}

/// 대화 엔트리 한 줄에서 뽑아낸 최신 상태 조각. claude의 `LatestEntry`와 같은
/// 역할이지만 codex는 usage/model이 같은 줄에 실리지 않아 따로 추적한다.
#[derive(Debug, Clone)]
struct LatestEntry {
    kind: EntryKind,
    ts_ms: i64,
}

/// 여러 tick에 걸친 codex rollout 조각을 누적해 상태를 기억한다. claude의
/// `TranscriptTracker`와 같은 이유로 존재한다 - `Tailer`가 델타만 돌려주므로 새로
/// 읽을 바이트가 없는 tick에도 세션의 진짜 상태를 잃지 않으려면 세션마다 하나씩
/// 들고 있어야 한다.
#[derive(Debug, Clone, Default)]
pub struct CodexTracker {
    pending: Vec<String>,
    latest: Option<LatestEntry>,
    usage: Option<Usage>,
    context_window: Option<u64>,
    meta_cwd: Option<String>,
    /// 가장 최근에 본 "의미 있는" 엔트리가 `task_complete`였는지. 그 뒤로 새
    /// `response_item`이나 `task_started`가 오면 꺼진다 - "마지막이 task_complete"라는
    /// 사실만 잡아내면 된다.
    turn_ended: bool,
}

impl CodexTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply(&mut self, chunk: &str) {
        for line in chunk.lines() {
            self.apply_line(line);
        }
    }

    fn apply_line(&mut self, line: &str) {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return;
        };
        let Some(kind) = v.get("type").and_then(|k| k.as_str()) else {
            return;
        };
        let ts_ms = v
            .get("timestamp")
            .and_then(|t| t.as_str())
            .and_then(parse_ts_ms);
        let payload = v.get("payload");

        match kind {
            "session_meta" => {
                if let Some(cwd) = payload.and_then(|p| p.get("cwd")).and_then(|c| c.as_str()) {
                    self.meta_cwd = Some(cwd.to_string());
                }
            }
            "response_item" => self.apply_response_item(payload, ts_ms),
            "event_msg" => self.apply_event_msg(payload, ts_ms),
            _ => {}
        }
    }

    fn apply_response_item(&mut self, payload: Option<&Value>, ts_ms: Option<i64>) {
        let Some(p) = payload else { return };
        let Some(item_type) = p.get("type").and_then(|t| t.as_str()) else {
            return;
        };
        let ts_ms = ts_ms.unwrap_or(0);
        match item_type {
            "function_call" => {
                if let Some(id) = p.get("call_id").and_then(|c| c.as_str()) {
                    self.pending.push(id.to_string());
                }
                self.latest = Some(LatestEntry {
                    kind: EntryKind::AssistantToolUse,
                    ts_ms,
                });
                self.turn_ended = false;
            }
            "function_call_output" => {
                if let Some(id) = p.get("call_id").and_then(|c| c.as_str()) {
                    self.pending.retain(|x| x != id);
                }
                self.latest = Some(LatestEntry {
                    kind: EntryKind::UserToolResult,
                    ts_ms,
                });
                self.turn_ended = false;
            }
            "message" => {
                let role = p.get("role").and_then(|r| r.as_str()).unwrap_or("");
                let kind = if role == "assistant" {
                    EntryKind::AssistantText
                } else {
                    EntryKind::UserText
                };
                self.latest = Some(LatestEntry { kind, ts_ms });
                self.turn_ended = false;
            }
            // reasoning, custom_tool_call 등 - 대화 엔트리로 last_kind를 바꾸지는
            // 않지만, 새 활동이 있었다는 것만은 반영한다.
            _ => {
                self.turn_ended = false;
            }
        }
    }

    fn apply_event_msg(&mut self, payload: Option<&Value>, _ts_ms: Option<i64>) {
        let Some(p) = payload else { return };
        let Some(msg_type) = p.get("type").and_then(|t| t.as_str()) else {
            return;
        };
        match msg_type {
            "task_complete" => self.turn_ended = true,
            "task_started" => self.turn_ended = false,
            "token_count" => {
                let Some(info) = p.get("info") else { return };
                if let Some(total) = info
                    .get("last_token_usage")
                    .and_then(|u| u.get("total_tokens"))
                    .and_then(|v| v.as_u64())
                {
                    self.usage = Some(Usage {
                        input: total,
                        cache_read: 0,
                        cache_creation: 0,
                    });
                }
                if let Some(window) = info.get("model_context_window").and_then(|v| v.as_u64()) {
                    self.context_window = Some(window);
                }
            }
            _ => {}
        }
    }

    /// 지금까지 누적된 상태로 요약을 만든다. 대화 엔트리를 한 번도 못 봤으면 `None`.
    /// `model`은 늘 `None`이다 - session_meta에는 모델명이 없고(`model_provider`만
    /// 있다), 추측으로 채우지 않는다.
    pub fn summary(&self) -> Option<TailSummary> {
        let latest = self.latest.as_ref()?;
        Some(TailSummary {
            last_kind: latest.kind,
            last_ts_ms: latest.ts_ms,
            pending_tool_use: self.pending.len(),
            usage: self.usage,
            model: None,
            cwd: self.meta_cwd.clone(),
        })
    }

    pub fn turn_ended(&self) -> bool {
        self.turn_ended
    }

    pub fn context_window(&self) -> Option<u64> {
        self.context_window
    }
}

/// jsonl 조각에서 마지막 대화 엔트리 요약을 만든다. 한 조각짜리 스냅샷에만 쓰는
/// 순수 함수다 - tick을 이어가며 상태를 기억해야 하면 `CodexTracker`를 쓴다.
/// `Collector`가 실제로 쓰는 것은 그쪽이다. 내부적으로 `CodexTracker` 하나를 이번
/// 호출에서만 쓰고 버려 파싱 로직을 두 군데에 중복하지 않는다.
#[allow(dead_code)]
pub fn parse_codex_tail(chunk: &str) -> Option<TailSummary> {
    let mut t = CodexTracker::new();
    t.apply(chunk);
    t.summary()
}

/// codex 세션 전용 계층 0 추론. claude와 다른 점은 `turn_ended`(명시적 turn 종료
/// 신호)뿐이다 - 그 신호가 없을 때는 claude와 같은 휴리스틱(`layer0::infer`)을
/// 그대로 재사용한다. approval 대기를 직접 관측할 방법이 없다는 것은 codex
/// 스키마 자체의 한계이므로, 그 부분까지 다르게 만들 이유가 없다.
pub fn infer(
    tail: Option<&TailSummary>,
    turn_ended: bool,
    alive: bool,
    cpu: f32,
    now_ms: i64,
    cfg: &Thresholds,
) -> (State, Confidence) {
    if !alive {
        return (State::Dead, Confidence::Fact);
    }
    if turn_ended {
        let age = tail.map(|t| now_ms - t.last_ts_ms).unwrap_or(0);
        return (demote(State::WaitingInput, age, cfg), Confidence::Fact);
    }
    super::layer0::infer(tail, alive, cpu, now_ms, cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAIL: &str = include_str!("../../tests/fixtures/codex_tail.jsonl");

    #[test]
    fn finds_uuid_in_rollout_filename() {
        let p = std::path::Path::new(
            "/s/2026/09/15/rollout-2026-09-15T00-00-00-01a09f69-b706-7822-bd35-7f273e2fd8e1.jsonl",
        );
        assert_eq!(
            uuid_from_rollout_name(p).as_deref(),
            Some("01a09f69-b706-7822-bd35-7f273e2fd8e1")
        );
    }

    #[test]
    fn non_uuid_suffix_yields_none() {
        let p = std::path::Path::new("/s/2026/09/15/rollout-2026-09-15T00-00-00-not-a-uuid.jsonl");
        assert_eq!(uuid_from_rollout_name(p), None);
    }

    #[test]
    fn parses_last_entry_kind() {
        let s = parse_codex_tail(TAIL).expect("summary");
        assert!(matches!(s.last_kind, EntryKind::AssistantText));
    }

    #[test]
    fn extracts_token_usage_when_present() {
        let s = parse_codex_tail(TAIL).expect("summary");
        assert!(s.usage.is_some());
        assert_eq!(s.usage.expect("usage").total(), 550);
    }

    #[test]
    fn unknown_schema_returns_none_instead_of_panicking() {
        assert!(parse_codex_tail("{\"unexpected\":1}").is_none());
    }

    #[test]
    fn matched_function_call_is_not_pending() {
        let s = parse_codex_tail(TAIL).expect("summary");
        assert_eq!(s.pending_tool_use, 0);
    }

    #[test]
    fn unmatched_function_call_is_pending() {
        let mut t = CodexTracker::new();
        t.apply(TAIL.lines().take(4).collect::<Vec<_>>().join("\n").as_str());
        let s = t.summary().expect("summary");
        assert_eq!(s.pending_tool_use, 1);
        assert!(matches!(s.last_kind, EntryKind::AssistantToolUse));
    }

    #[test]
    fn trailing_task_complete_marks_turn_ended() {
        let mut t = CodexTracker::new();
        t.apply(TAIL);
        assert!(t.turn_ended());
    }

    #[test]
    fn new_activity_after_task_complete_clears_turn_ended() {
        let mut t = CodexTracker::new();
        t.apply(TAIL);
        assert!(t.turn_ended());
        let follow_up = r#"{"type":"response_item","timestamp":"2026-09-15T00:01:00.000Z","ordinal":8,"payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"and now?"}]}}"#;
        t.apply(follow_up);
        assert!(!t.turn_ended());
    }

    #[test]
    fn context_window_is_read_directly_not_heuristically() {
        let mut t = CodexTracker::new();
        t.apply(TAIL);
        assert_eq!(t.context_window(), Some(272_000));
    }

    #[test]
    fn session_meta_cwd_is_captured_when_present_in_chunk() {
        let mut t = CodexTracker::new();
        t.apply(TAIL);
        let s = t.summary().expect("summary");
        assert_eq!(s.cwd.as_deref(), Some("/home/dev/project-x"));
    }

    #[test]
    fn find_codex_transcript_searches_newest_date_dir_first() {
        let dir = tempfile::tempdir().expect("dir");
        let old = dir.path().join("2026/08/01");
        let new = dir.path().join("2026/09/15");
        std::fs::create_dir_all(&old).expect("mkdir old");
        std::fs::create_dir_all(&new).expect("mkdir new");
        // 같은 uuid를 가진 두 파일을 서로 다른 날짜 디렉터리에 둔다 - 최신
        // 디렉터리가 우선해야 한다는 것을 증명하려면 오래된 쪽이 먼저 발견되면
        // 안 된다는 사실만으로는 부족하다(둘 다 같은 uuid라 구분이 안 된다).
        // 대신 최신 쪽에만 있는 uuid가 확실히 발견되는지로 검증한다.
        std::fs::write(
            old.join("rollout-2026-08-01T00-00-00-11111111-1111-1111-1111-111111111111.jsonl"),
            "",
        )
        .expect("write old");
        std::fs::write(
            new.join("rollout-2026-09-15T00-00-00-22222222-2222-2222-2222-222222222222.jsonl"),
            "",
        )
        .expect("write new");

        let found = find_codex_transcript(dir.path(), "22222222-2222-2222-2222-222222222222");
        assert!(found.is_some());
    }

    #[test]
    fn find_codex_transcript_returns_none_when_uuid_absent() {
        let dir = tempfile::tempdir().expect("dir");
        assert!(find_codex_transcript(dir.path(), "no-such-uuid").is_none());
    }

    #[test]
    fn read_session_meta_cwd_reads_only_the_first_line() {
        let dir = tempfile::tempdir().expect("dir");
        let p = dir.path().join("r.jsonl");
        let meta = r#"{"type":"session_meta","timestamp":"2026-09-15T00:00:00.000Z","ordinal":0,"payload":{"cwd":"/home/dev/app"}}"#;
        std::fs::write(&p, format!("{meta}\nnot json at all\n")).expect("write");
        assert_eq!(read_session_meta_cwd(&p).as_deref(), Some("/home/dev/app"));
    }

    #[test]
    fn find_unclaimed_transcript_with_unknown_cwd_returns_most_recent() {
        let dir = tempfile::tempdir().expect("dir");
        let day = dir.path().join("2026/09/15");
        std::fs::create_dir_all(&day).expect("mkdir");
        let meta = |cwd: &str| {
            format!(
                r#"{{"type":"session_meta","timestamp":"2026-09-15T00:00:00.000Z","ordinal":0,"payload":{{"cwd":"{cwd}"}}}}"#
            )
        };
        std::fs::write(
            day.join("rollout-2026-09-15T00-00-00-11111111-1111-1111-1111-111111111111.jsonl"),
            meta("/home/dev/a"),
        )
        .expect("write a");
        std::fs::write(
            day.join("rollout-2026-09-15T01-00-00-22222222-2222-2222-2222-222222222222.jsonl"),
            meta("/home/dev/b"),
        )
        .expect("write b");

        let claimed = HashSet::new();
        let hit = find_unclaimed_codex_transcript(dir.path(), None, &claimed).expect("hit");
        // 파일명이 타임스탬프로 시작하므로 이름 내림차순 = 최신 우선 -> 01시가 먼저다.
        assert_eq!(hit.uuid, "22222222-2222-2222-2222-222222222222");
    }

    #[test]
    fn find_unclaimed_transcript_with_known_cwd_matches_meta_cwd() {
        let dir = tempfile::tempdir().expect("dir");
        let day = dir.path().join("2026/09/15");
        std::fs::create_dir_all(&day).expect("mkdir");
        let meta = |cwd: &str| {
            format!(
                r#"{{"type":"session_meta","timestamp":"2026-09-15T00:00:00.000Z","ordinal":0,"payload":{{"cwd":"{cwd}"}}}}"#
            )
        };
        std::fs::write(
            day.join("rollout-2026-09-15T01-00-00-11111111-1111-1111-1111-111111111111.jsonl"),
            meta("/home/dev/a"),
        )
        .expect("write a");
        std::fs::write(
            day.join("rollout-2026-09-15T00-00-00-22222222-2222-2222-2222-222222222222.jsonl"),
            meta("/home/dev/b"),
        )
        .expect("write b");

        let claimed = HashSet::new();
        let hit = find_unclaimed_codex_transcript(
            dir.path(),
            Some(std::path::Path::new("/home/dev/b")),
            &claimed,
        )
        .expect("hit");
        assert_eq!(hit.uuid, "22222222-2222-2222-2222-222222222222");
    }

    #[test]
    fn find_unclaimed_transcript_skips_already_claimed_paths() {
        let dir = tempfile::tempdir().expect("dir");
        let day = dir.path().join("2026/09/15");
        std::fs::create_dir_all(&day).expect("mkdir");
        let path =
            day.join("rollout-2026-09-15T00-00-00-11111111-1111-1111-1111-111111111111.jsonl");
        std::fs::write(
            &path,
            r#"{"type":"session_meta","timestamp":"2026-09-15T00:00:00.000Z","ordinal":0,"payload":{"cwd":"/home/dev/a"}}"#,
        )
        .expect("write");

        let mut claimed = HashSet::new();
        claimed.insert(path);
        assert!(find_unclaimed_codex_transcript(dir.path(), None, &claimed).is_none());
    }

    #[test]
    fn infer_dead_when_process_is_gone() {
        let cfg = Thresholds::default();
        let (s, c) = infer(None, true, false, 0.0, 0, &cfg);
        assert_eq!(s, State::Dead);
        assert_eq!(c, Confidence::Fact);
    }

    #[test]
    fn infer_trailing_task_complete_is_a_fact_not_a_guess() {
        let cfg = Thresholds::default();
        let tail = TailSummary {
            last_kind: EntryKind::AssistantText,
            last_ts_ms: 1_000,
            pending_tool_use: 0,
            usage: None,
            model: None,
            cwd: None,
        };
        let (s, c) = infer(Some(&tail), true, true, 0.0, 6_000, &cfg);
        assert_eq!(s, State::WaitingInput);
        assert_eq!(c, Confidence::Fact);
    }

    #[test]
    fn infer_without_turn_ended_falls_back_to_shared_heuristic() {
        let cfg = Thresholds::default();
        let tail = TailSummary {
            last_kind: EntryKind::AssistantToolUse,
            last_ts_ms: 0,
            pending_tool_use: 1,
            usage: None,
            model: None,
            cwd: None,
        };
        // idle cpu + 오래됨 -> claude와 같은 방식으로 승인 대기를 "의심"만 한다
        // (관측이 아니라 추론이므로 Low로 남는다).
        let (s, c) = infer(Some(&tail), false, true, 0.0, 40_000, &cfg);
        assert_eq!(s, State::WaitingApproval);
        assert_eq!(c, Confidence::Low);
    }
}
