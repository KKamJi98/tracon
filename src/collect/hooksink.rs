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
    let tmp = temp_path(dir, &rec.key.uuid, std::process::id());
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

/// `live`에 없는 세션의 기록 파일을 지운다. `Collector`의 tracker/offset 정리와
/// 같은 이유다 - 아무 프로세스도 가리키지 않게 된 세션의 기록을 영원히 들고 있으면
/// 초 단위로 폴링하는 TUI에서 tick마다 읽는 파일 수가 무한정 늘고, `hooks on`
/// 배지도 기록이 한 번 생긴 뒤로는 영원히 켜진 채로 남는다.
pub fn prune(dir: &Path, live: &std::collections::HashSet<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let live_names: std::collections::HashSet<String> = live.iter().map(|u| sanitize(u)).collect();
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        // `.tmp` 조각과 cmux 커서 파일은 확장자가 달라 여기서 걸러진다.
        if path.extension().map(|x| x != "json").unwrap_or(true) {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if !live_names.contains(stem) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

fn sanitize(uuid: &str) -> String {
    uuid.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect()
}

#[allow(dead_code)]
/// 훅 프로세스에서 조상을 거슬러 올라가 이 이벤트를 낸 에이전트 프로세스를 찾는다.
///
/// 훅 payload에는 pid가 없다. 그런데 pid가 없으면 `Collector`는 살아 있는 프로세스와
/// 세션 uuid를 cwd와 transcript mtime으로 짐작해 맞출 수밖에 없고, 같은 cwd에 세션이
/// 여러 개면 닫힌 세션의 transcript를 살아 있는 다른 프로세스가 집어간다. 훅은 에이전트의
/// 자손으로 실행되므로, 조상 체인이 그 연결을 사실로 만들어 준다.
///
/// 실측 체인은 `tracon -> bash(래퍼) -> zsh -> claude`로 3~4홉이다. 8홉에서 끊어
/// 어떤 경우에도 순환이나 긴 탐색으로 훅을 지연시키지 않는다. 못 찾으면 `None`이고,
/// 호출부는 pid 없는 기록을 그대로 남긴다 - 짐작 경로로 돌아갈 뿐 나빠지지 않는다.
pub fn agent_ancestor_pid() -> Option<i32> {
    let table = ps_table()?;
    let mut pid = std::process::id() as i32;
    for _ in 0..8 {
        let (ppid, comm) = table.get(&pid)?;
        if is_agent_comm(comm) {
            return Some(pid);
        }
        pid = *ppid;
    }
    None
}

/// `ps -eo pid=,ppid=,comm=` 한 번으로 전체 표를 읽는다. 홉마다 `ps`를 새로 띄우면
/// 훅 이벤트 하나에 spawn이 서너 개씩 붙는다 - 훅은 도구 호출마다 돈다.
fn ps_table() -> Option<std::collections::HashMap<i32, (i32, String)>> {
    let out = std::process::Command::new("ps")
        .args(["-eo", "pid=,ppid=,comm="])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(parse_ps_table(&String::from_utf8_lossy(&out.stdout)))
}

fn parse_ps_table(text: &str) -> std::collections::HashMap<i32, (i32, String)> {
    text.lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let pid: i32 = parts.next()?.parse().ok()?;
            let ppid: i32 = parts.next()?.parse().ok()?;
            let comm = parts.next()?.to_string();
            Some((pid, (ppid, comm)))
        })
        .collect()
}

/// `comm`은 실행 파일 경로일 수도, 이름만일 수도 있다. 마지막 경로 요소로 본다.
fn is_agent_comm(comm: &str) -> bool {
    let name = comm.rsplit('/').next().unwrap_or(comm);
    name == "claude" || name == "codex"
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

/// 훅 이벤트 이름 문자열을 `HookEvent`로 옮긴다. cmux 어댑터(Task 14)도 같은
/// 매핑을 쓰므로 이 함수 하나로 모아 둔다 - 두 곳에서 따로 유지하면 표가 갈라진다.
pub(crate) fn hook_event_from_name(name: &str) -> Option<HookEvent> {
    Some(match name {
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
    })
}

pub fn event_from_claude_hook(stdin: &str) -> Option<SinkRecord> {
    let raw: ClaudeHookStdin = serde_json::from_str(stdin).ok()?;
    let event = hook_event_from_name(&raw.hook_event_name)?;
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

    /// `ps -eo pid=,ppid=,comm=` 출력에서 부모 관계를 읽는다. comm에 경로가 붙어
    /// 나오는 환경이 있어 마지막 경로 요소로 판정한다.
    #[test]
    fn ps_table_parses_parent_links() {
        let out = "  100     1 /bin/login\n  200   100 -zsh\n  300   200 /usr/bin/claude\n";
        let m = super::parse_ps_table(out);
        assert_eq!(m.get(&300).map(|(p, _)| *p), Some(200));
        assert_eq!(m.get(&200).map(|(p, _)| *p), Some(100));
        assert!(super::is_agent_comm("/usr/bin/claude"));
        assert!(super::is_agent_comm("claude"));
        assert!(super::is_agent_comm("codex"));
        assert!(!super::is_agent_comm("-zsh"));
        assert!(!super::is_agent_comm("/bin/login"));
    }

    /// 조상에 에이전트가 정말 있으면 찾아낸다. 이 테스트 프로세스는 claude가 띄운
    /// `cargo test`의 자손이 아닐 수도 있으므로, 순수 파싱 경로만 검증한다.
    #[test]
    fn a_chain_without_an_agent_yields_nothing() {
        let out = "  100     1 /bin/login\n  200   100 -zsh\n";
        let m = super::parse_ps_table(out);
        assert!(!m.values().any(|(_, c)| super::is_agent_comm(c)));
    }

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
    fn prune_removes_only_files_outside_the_live_set() {
        let dir = tempfile::tempdir().expect("dir");
        for uuid in ["alive", "gone"] {
            record_event(
                dir.path(),
                &SinkRecord {
                    key: crate::model::SessionKey {
                        provider: Provider::Claude,
                        uuid: uuid.into(),
                    },
                    event: HookEvent::Stop,
                    occurred_at: 1,
                    cwd: None,
                    pid: None,
                },
            )
            .expect("write");
        }
        let cursor = dir.path().join("cmux.cursor");
        std::fs::write(&cursor, "42").expect("cursor");

        let live: std::collections::HashSet<String> = ["alive".to_string()].into();
        prune(dir.path(), &live);

        let all = read_all(dir.path());
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].key.uuid, "alive");
        assert!(cursor.exists(), "json이 아닌 파일은 건드리지 않는다");
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
