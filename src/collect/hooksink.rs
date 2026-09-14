#![allow(dead_code)]

use crate::model::{Confidence, HookEvent, Observation, Provider, SessionKey, Source};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SinkRecord {
    pub key: SessionKey,
    pub event: HookEvent,
    pub occurred_at: i64,
    pub cwd: Option<String>,
    pub pid: Option<i32>,
}

impl SinkRecord {
    pub fn to_observation(&self) -> Option<Observation> {
        let state = crate::model::transition(None, self.event)?;
        Some(Observation {
            key: self.key.clone(),
            state,
            source: Source::Layer1Hook,
            confidence: Confidence::Fact,
            observed_at: self.occurred_at,
        })
    }
}

pub fn sink_dir() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("tracon/sessions")
}

pub fn record_event(dir: &Path, rec: &SinkRecord) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}.json", sanitize(&rec.key.uuid)));
    let tmp = dir.join(format!(
        "{}.json.{}.tmp",
        sanitize(&rec.key.uuid),
        std::process::id()
    ));
    std::fs::write(&tmp, serde_json::to_vec(rec)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn read_all(dir: &Path) -> Vec<SinkRecord> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|x| x == "json").unwrap_or(false))
        .filter_map(|e| std::fs::read(e.path()).ok())
        .filter_map(|b| serde_json::from_slice::<SinkRecord>(&b).ok())
        .collect()
}

fn sanitize(uuid: &str) -> String {
    uuid.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect()
}

fn temp_path(dir: &Path, uuid: &str, pid: u32) -> PathBuf {
    dir.join(format!("{}.json.{}.tmp", sanitize(uuid), pid))
}

#[derive(Deserialize)]
struct ClaudeHookStdin {
    session_id: String,
    hook_event_name: String,
    cwd: Option<String>,
}

pub fn event_from_claude_hook(stdin: &str) -> Option<SinkRecord> {
    let raw: ClaudeHookStdin = serde_json::from_str(stdin).ok()?;
    let event = match raw.hook_event_name.as_str() {
        "SessionStart" => HookEvent::SessionStart,
        "UserPromptSubmit" => HookEvent::UserPromptSubmit,
        "PreToolUse" => HookEvent::PreToolUse,
        "PostToolUse" => HookEvent::PostToolUse,
        "PermissionRequest" => HookEvent::PermissionRequest,
        "Notification" => HookEvent::Notification,
        "Stop" => HookEvent::Stop,
        "SubagentStop" => HookEvent::SubagentStop,
        "SessionEnd" => HookEvent::SessionEnd,
        _ => return None,
    };
    Some(SinkRecord {
        key: SessionKey {
            provider: Provider::Claude,
            uuid: raw.session_id,
        },
        event,
        occurred_at: now_ms(),
        cwd: raw.cwd,
        pid: None,
    })
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{HookEvent, Provider};

    #[test]
    fn parses_claude_hook_stdin() {
        let stdin =
            r#"{"session_id":"abc-123","hook_event_name":"PermissionRequest","cwd":"/home/dev/p"}"#;
        let rec = event_from_claude_hook(stdin).expect("record");
        assert_eq!(rec.key.uuid, "abc-123");
        assert_eq!(rec.key.provider, Provider::Claude);
        assert_eq!(rec.event, HookEvent::PermissionRequest);
        assert_eq!(rec.cwd.as_deref(), Some("/home/dev/p"));
    }

    #[test]
    fn unknown_event_name_is_rejected() {
        let stdin = r#"{"session_id":"a","hook_event_name":"Nope"}"#;
        assert!(event_from_claude_hook(stdin).is_none());
    }

    #[test]
    fn write_then_read_roundtrip() {
        let dir = tempfile::tempdir().expect("dir");
        let rec = SinkRecord {
            key: crate::model::SessionKey {
                provider: Provider::Claude,
                uuid: "u1".into(),
            },
            event: HookEvent::Stop,
            occurred_at: 1_800_000_000_000,
            cwd: Some("/home/dev/p".into()),
            pid: Some(42),
        };
        record_event(dir.path(), &rec).expect("write");
        let all = read_all(dir.path());
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].event, HookEvent::Stop);
    }

    #[test]
    fn latest_write_replaces_previous_for_same_session() {
        let dir = tempfile::tempdir().expect("dir");
        let base = crate::model::SessionKey {
            provider: Provider::Claude,
            uuid: "u1".into(),
        };
        for (ev, at) in [(HookEvent::PreToolUse, 1), (HookEvent::Stop, 2)] {
            record_event(
                dir.path(),
                &SinkRecord {
                    key: base.clone(),
                    event: ev,
                    occurred_at: at,
                    cwd: None,
                    pid: None,
                },
            )
            .expect("write");
        }
        let all = read_all(dir.path());
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].event, HookEvent::Stop);
    }

    #[test]
    fn corrupt_file_is_skipped() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(dir.path().join("broken.json"), "{oops").expect("write");
        assert_eq!(read_all(dir.path()).len(), 0);
    }

    #[test]
    fn concurrent_writes_to_same_session_use_different_temp_paths() {
        let dir = tempfile::tempdir().expect("dir");
        let uuid = "concurrent-session";
        let pid1 = 1000u32;
        let pid2 = 2000u32;
        let tmp1 = temp_path(dir.path(), uuid, pid1);
        let tmp2 = temp_path(dir.path(), uuid, pid2);
        assert_ne!(tmp1, tmp2);
        assert!(tmp1.to_string_lossy().contains("1000"));
        assert!(tmp2.to_string_lossy().contains("2000"));
    }
}
