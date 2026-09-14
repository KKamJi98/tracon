//! cmux 어댑터 (레이어 2, 선택). cmux는 hook 이벤트를 Unix 소켓으로 중개하는
//! 세션 매니저다 - 떠 있으면 세션 상태가 추론이 아니라 사실이 되고, 세션마다
//! 점프 대상(workspace)까지 딸려온다. 없으면 이 모듈 전체가 비활성일 뿐, 나머지
//! 계층(0/1)은 그대로 동작한다 - 터미널 중립성이 이 프로젝트의 제약이기 때문이다.

use crate::collect::hooksink::{hook_event_from_name, SinkRecord};
use crate::collect::transcript::parse_ts_ms;
use crate::model::{Confidence, Observation, Provider, SessionKey, Source};
use serde::Deserialize;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

/// cmux 이벤트 스트림에서 뽑아낸 hook 이벤트 한 건. `record`는 Layer 1과 같은
/// 모양(`SinkRecord`)을 재사용해 관측을 만들 때 중복 정의를 피한다.
#[derive(Debug, Clone)]
pub struct CmuxEvent {
    pub record: SinkRecord,
    /// 재연결 시 이어받을 수 있게 원본 시퀀스 번호를 들고 있는다. `cmux events
    /// --cursor-file`이 재연결 재개를 이미 처리하므로 지금은 관측 용도로만 쓴다.
    #[allow(dead_code)]
    pub seq: u64,
    /// 점프 대상을 찾기 위한 workspace UUID. cmux가 항상 채워 주지는 않는다.
    pub workspace_id: Option<String>,
}

impl CmuxEvent {
    /// Layer 2 관측으로 바꾼다. hook 이벤트가 상태 전이를 낳지 않으면(`SubagentStop`
    /// 등) `None` - Layer 1의 `SinkRecord::to_observation`과 같은 규칙을 쓴다.
    pub fn to_observation(&self) -> Option<Observation> {
        let state = crate::model::transition(None, self.record.event)?;
        Some(Observation {
            key: self.record.key.clone(),
            state,
            source: Source::Layer2Cmux,
            confidence: Confidence::Fact,
            observed_at: self.record.occurred_at,
        })
    }
}

#[derive(Deserialize)]
struct RawLine {
    category: String,
    name: String,
    occurred_at: String,
    seq: u64,
    payload: RawPayload,
}

#[derive(Deserialize)]
struct RawPayload {
    session_id: String,
    hook_event_name: Option<String>,
    cwd: Option<String>,
    workspace_id: Option<String>,
}

/// `cmux events` 한 줄(ndjson)을 파싱한다. `category`가 `agent`이고 `name`이
/// `agent.hook.`으로 시작하는 줄만 받는다 - 같은 hook 호출이 `feed.item.received`
/// 같은 다른 category/name으로도 올라오므로, 걸러내지 않으면 이벤트가 중복된다.
pub fn parse_cmux_event(line: &str) -> Option<CmuxEvent> {
    let raw: RawLine = serde_json::from_str(line).ok()?;
    if raw.category != "agent" || !raw.name.starts_with("agent.hook.") {
        return None;
    }
    let event = hook_event_from_name(raw.payload.hook_event_name.as_deref()?)?;
    let (provider, uuid) = split_session_id(&raw.payload.session_id)?;
    let occurred_at = parse_ts_ms(&raw.occurred_at)?;

    Some(CmuxEvent {
        record: SinkRecord {
            key: SessionKey { provider, uuid },
            event,
            occurred_at,
            cwd: raw.payload.cwd,
            pid: None,
        },
        seq: raw.seq,
        workspace_id: raw.payload.workspace_id,
    })
}

/// `<provider>-<uuid>` 형태의 cmux session_id에서 provider 접두사를 뗀다.
fn split_session_id(session_id: &str) -> Option<(Provider, String)> {
    if let Some(uuid) = session_id.strip_prefix("claude-") {
        return Some((Provider::Claude, uuid.to_string()));
    }
    if let Some(uuid) = session_id.strip_prefix("codex-") {
        return Some((Provider::Codex, uuid.to_string()));
    }
    None
}

/// `cmux events`를 자식 프로세스로 띄워 stdout을 줄 단위로 읽는 구독.
pub struct CmuxSubscriber;

impl CmuxSubscriber {
    /// 구독을 시작한다. `cmux` 실행 파일이 없거나, 있어도 인자를 못 알아듣고
    /// 곧바로 죽으면 `None`을 돌려준다 - 이 경우 호출부는 레이어 2 없이 그대로
    /// 동작해야 한다(터미널 중립성). 성공하면 파싱된 이벤트를 실어 나르는
    /// 채널의 수신 쪽을 돌려준다.
    pub fn spawn() -> Option<Receiver<CmuxEvent>> {
        let state_dir = crate::collect::hooksink::sink_dir();
        let _ = std::fs::create_dir_all(&state_dir);
        let cursor_file = state_dir.join("cmux.cursor").to_string_lossy().into_owned();
        spawn_with(
            "cmux",
            &[
                "events".to_string(),
                "--category".to_string(),
                "agent".to_string(),
                "--reconnect".to_string(),
                "--cursor-file".to_string(),
                cursor_file,
            ],
        )
    }
}

/// 테스트에서 가짜 명령을 주입할 수 있게 `spawn`에서 떼어낸 실제 구현.
fn spawn_with(program: &str, args: &[String]) -> Option<Receiver<CmuxEvent>> {
    let mut child = Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .spawn()
        .ok()?;

    // 실행 파일은 있어도 인자를 몰라 곧바로 비정상 종료하는 구현이 있을 수 있다.
    // 짧게 폴링해서 그런 죽은 구독을 살아있다고 보고하지 않는다. 이미 성공
    // 종료했으면(예: 테스트에서 쓰는 즉시 끝나는 가짜 명령) 그대로 진행한다 -
    // 남은 stdout은 아래 reader 스레드가 마저 읽는다. 아직 실행 중이면(cmux의
    // 실제 `--reconnect` 구독처럼 끝나지 않는 경우) 그대로 진행한다.
    for _ in 0..10 {
        match child.try_wait() {
            Ok(Some(status)) if !status.success() => return None,
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(_) => return None,
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    let stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if let Some(event) = parse_cmux_event(&line) {
                if tx.send(event).is_err() {
                    break;
                }
            }
        }
        let _ = child.wait();
    });
    Some(rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{HookEvent, Provider};

    const NDJSON: &str = include_str!("../../tests/fixtures/cmux_event.ndjson");

    #[test]
    fn strips_provider_prefix_from_session_id() {
        let e = parse_cmux_event(NDJSON.lines().next().expect("line")).expect("event");
        assert_eq!(e.record.key.uuid, "11111111-2222-3333-4444-555555555555");
        assert_eq!(e.record.key.provider, Provider::Claude);
    }

    #[test]
    fn maps_permission_request_event() {
        let line = NDJSON.lines().nth(1).expect("line");
        let e = parse_cmux_event(line).expect("event");
        assert_eq!(e.record.event, HookEvent::PermissionRequest);
    }

    #[test]
    fn ignores_non_agent_categories() {
        let line = NDJSON.lines().nth(2).expect("line");
        assert!(parse_cmux_event(line).is_none());
    }

    #[test]
    fn extracts_workspace_for_jump() {
        let e = parse_cmux_event(NDJSON.lines().next().expect("line")).expect("event");
        assert_eq!(
            e.workspace_id.as_deref(),
            Some("BBBB2222-0000-0000-0000-000000000000")
        );
    }

    #[test]
    fn seq_is_kept_for_cursor_resume() {
        let e = parse_cmux_event(NDJSON.lines().next().expect("line")).expect("event");
        assert_eq!(e.seq, 1001);
    }

    #[test]
    fn garbage_line_yields_none() {
        assert!(parse_cmux_event("not json").is_none());
    }

    #[test]
    fn missing_binary_yields_no_subscription() {
        assert!(spawn_with("tracon-nonexistent-binary-xyz", &[]).is_none());
    }

    #[test]
    fn command_that_exits_nonzero_immediately_yields_no_subscription() {
        assert!(spawn_with("sh", &["-c".to_string(), "exit 1".to_string()]).is_none());
    }

    #[test]
    fn events_from_a_short_lived_command_reach_the_channel() {
        // 사실 확인이 아니라 배선 확인이다 - 진짜 cmux 대신, 픽스처를 그대로
        // 출력하고 성공 종료하는 명령으로 reader 스레드 -> 채널 경로를 검증한다.
        let fixture_path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/cmux_event.ndjson"
        );
        let rx = spawn_with("cat", &[fixture_path.to_string()]).expect("subscription");

        let first = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("first event");
        assert_eq!(
            first.record.key.uuid,
            "11111111-2222-3333-4444-555555555555"
        );
        let second = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("second event");
        assert_eq!(second.record.event, HookEvent::PermissionRequest);
        // 세 번째 줄은 category가 agent가 아니라서 걸러진다 - 채널은 곧 끊긴다.
        assert!(rx.recv_timeout(Duration::from_secs(2)).is_err());
    }
}
