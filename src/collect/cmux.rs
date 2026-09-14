//! cmux 어댑터 (레이어 2, 선택). cmux는 hook 이벤트를 Unix 소켓으로 중개하는
//! 세션 매니저다 - 떠 있으면 세션 상태가 추론이 아니라 사실이 되고, 세션마다
//! 점프 대상(workspace)까지 딸려온다. 없으면 이 모듈 전체가 비활성일 뿐, 나머지
//! 계층(0/1)은 그대로 동작한다 - 터미널 중립성이 이 프로젝트의 제약이기 때문이다.

use crate::collect::hooksink::{hook_event_from_name, SinkRecord};
use crate::collect::transcript::parse_ts_ms;
use crate::model::{Confidence, Observation, Provider, SessionKey, Source};
use serde::Deserialize;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
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

/// `cmux events`를 자식 프로세스로 띄워 stdout을 줄 단위로 읽는 구독. 이 구조체가
/// `Child`를 들고 있다가 [`Drop`]에서 죽이는 이유는, 그렇게 하지 않으면 구독을
/// 그만 쓰는 모든 호출부가 "자식을 죽여야 한다"는 걸 따로 기억해야 하기 때문이다 -
/// 정상 반환이든 에러 경로든 값이 스코프를 벗어나는 순간 자동으로 정리된다.
pub struct CmuxSubscriber {
    pub(crate) rx: Receiver<CmuxEvent>,
    child: Arc<Mutex<Child>>,
}

impl CmuxSubscriber {
    /// 오래 사는 구독(TUI). 연결이 끊기면 `--reconnect`가 알아서 재연결한다.
    pub fn spawn() -> Option<Self> {
        Self::spawn_inner(true)
    }

    /// 스냅샷 한 번만 찍고 끝나는 호출(`--json`)용. `--reconnect`를 붙이지 않는다 -
    /// 한 번 쓰고 버릴 구독을 재연결까지 시도하게 둘 이유가 없고, 호출부가 스냅샷을
    /// 찍자마자 이 값을 버려(drop) 자식을 죽여야 한다 - 그러지 않으면 폴링할 때마다
    /// cmux 데몬에 고아 프로세스가 하나씩 쌓인다.
    pub fn spawn_one_shot() -> Option<Self> {
        Self::spawn_inner(false)
    }

    fn spawn_inner(reconnect: bool) -> Option<Self> {
        let state_dir = crate::collect::hooksink::sink_dir();
        let _ = std::fs::create_dir_all(&state_dir);
        spawn_with("cmux", &events_args(reconnect, &state_dir))
    }
}

/// `cmux events`에 넘길 인자를 만든다.
///
/// 커서 파일은 오래 사는 구독(TUI)만 쓴다. `--json`의 1회성 자식과 커서를 공유하면,
/// 먼저 끝나는 쪽이 커서를 TUI가 아직 읽지 못한 지점 너머로 밀어 이벤트를 삼킨다 -
/// README가 권하는 "TUI를 띄운 채 statusline에서 --json 폴링"이 바로 그 조합이다.
/// 1회성 호출은 애초에 이어받을 구간이 없으므로 커서 자체가 필요 없다.
fn events_args(reconnect: bool, state_dir: &Path) -> Vec<String> {
    let mut args = vec![
        "events".to_string(),
        "--category".to_string(),
        "agent".to_string(),
    ];
    if reconnect {
        args.push("--cursor-file".to_string());
        args.push(state_dir.join("cmux.cursor").to_string_lossy().into_owned());
        args.push("--reconnect".to_string());
    }
    args
}

impl Drop for CmuxSubscriber {
    /// 죽은 자식을 다시 죽이는 것도(`kill()`이 ESRCH만 돌려줄 뿐 패닉하지 않는다),
    /// wedge된 자식을 기다리는 것도(`kill()` 뒤의 `wait()`라 금방 끝난다) 안전해야
    /// 한다는 요구를 그대로 따른다.
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// 테스트에서 가짜 명령을 주입할 수 있게 `spawn`에서 떼어낸 실제 구현.
fn spawn_with(program: &str, args: &[String]) -> Option<CmuxSubscriber> {
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
    let child = Arc::new(Mutex::new(child));
    let reader_child = Arc::clone(&child);
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
        // 자식이 스스로 끝났을 뿐이라면(우리가 죽인 게 아니라면) 좀비로 남지 않게
        // 여기서 거둔다. `CmuxSubscriber::drop`이 이미 거뒀다면 이 `wait()`는
        // "그런 자식 없음" 에러로 조용히 끝난다 - 둘 다 안전해야 하므로 무시한다.
        let _ = reader_child.lock().map(|mut c| c.wait());
    });
    Some(CmuxSubscriber { rx, child })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{HookEvent, Provider};

    /// TUI 구독과 `--json` 1회 호출이 같은 커서 파일을 쓰면, 짧게 살다 죽는 후자가
    /// 커서를 TUI가 아직 읽지 못한 지점 너머로 밀어버린다. README가 권하는 "TUI를
    /// 띄운 채 statusline에서 --json 폴링"이 정확히 그 조합이다.
    #[test]
    fn the_one_shot_subscription_does_not_share_the_cursor_file() {
        let args = events_args(false, Path::new("/state"));
        assert!(
            !args.iter().any(|a| a.contains("cursor")),
            "1회성 구독은 커서 파일을 쓰지 않는다: {args:?}"
        );
        assert!(!args.iter().any(|a| a == "--reconnect"));
    }

    #[test]
    fn the_long_lived_subscription_keeps_its_cursor_file_and_reconnects() {
        let args = events_args(true, Path::new("/state"));
        assert!(args.iter().any(|a| a == "--cursor-file"));
        assert!(args.iter().any(|a| a == "/state/cmux.cursor"));
        assert!(args.iter().any(|a| a == "--reconnect"));
    }

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
        let sub = spawn_with("cat", &[fixture_path.to_string()]).expect("subscription");

        let first = sub
            .rx
            .recv_timeout(Duration::from_secs(2))
            .expect("first event");
        assert_eq!(
            first.record.key.uuid,
            "11111111-2222-3333-4444-555555555555"
        );
        let second = sub
            .rx
            .recv_timeout(Duration::from_secs(2))
            .expect("second event");
        assert_eq!(second.record.event, HookEvent::PermissionRequest);
        // 세 번째 줄은 category가 agent가 아니라서 걸러진다 - 채널은 곧 끊긴다.
        assert!(sub.rx.recv_timeout(Duration::from_secs(2)).is_err());
    }

    /// 리뷰 지적: `spawn_with`가 자식을 살려 둔 채 반환되면(원래 구현이 그랬다),
    /// `--json` 같은 1회성 호출은 스냅샷을 찍고 끝나도 `cmux events --reconnect`
    /// 자식이 살아남아 cmux 데몬에 고아로 쌓인다. `CmuxSubscriber`를 버리는 순간
    /// (Drop) 진짜로 죽는지 - 진짜 cmux 없이, 오래 사는 `sleep 30`으로 - 검증한다.
    #[test]
    fn dropping_the_subscriber_kills_the_child_process() {
        let sub = spawn_with("sleep", &["30".to_string()]).expect("subscription");
        let pid = sub.child.lock().expect("lock").id();

        // sleep이 실제로 떠 있는지부터 확인한다 - 그래야 아래에서 "사라졌다"는
        // 주장이 "애초에 없었다"가 아니라 진짜 종료를 뜻한다.
        assert!(
            process_is_alive(pid),
            "sleep 30 must be running before drop"
        );

        drop(sub);

        // kill()은 비동기 신호일 뿐이라 커널이 실제로 회수할 때까지 아주 잠깐의
        // 여유를 둔다 - 하지만 몇 초씩 걸리면 안 된다(고아가 "언젠가" 죽는 게
        // 아니라 즉시 죽어야 한다는 게 이 리뷰의 요지다).
        let mut alive = process_is_alive(pid);
        for _ in 0..20 {
            if !alive {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
            alive = process_is_alive(pid);
        }
        assert!(
            !alive,
            "child must be terminated once the subscriber is dropped"
        );
    }

    /// `kill -0 <pid>`로 프로세스 존재 여부만 확인한다 - 실제 cmux 바이너리에
    /// 의존하지 않고, 표준 유닉스 유틸리티(`sleep`, `kill`)만으로 결정적으로 돈다.
    fn process_is_alive(pid: u32) -> bool {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}
