//! resume 명령 만들기와 클립보드 복사.
//!
//! 세션이 떠 있는 pane으로 "옮겨가는" 동작은 터미널 멀티플렉서마다 다르고,
//! 어느 쪽도 모든 사용자에게 있지는 않다. tracon은 그 대신 세션을 되살리는
//! 명령을 클립보드에 넣어 주고, 어디에 붙여넣을지는 사용자가 정한다.

pub mod clipboard;

use crate::model::{Provider, Session};

pub fn resume_command(session: &Session) -> String {
    let bin = match session.key.provider {
        Provider::Claude => "claude",
        Provider::Codex => "codex",
        // agy는 `--resume`이 아니라 `--conversation=<id>`로 되살린다.
        Provider::Antigravity => {
            return format!("agy --conversation={}", session.key.uuid);
        }
    };
    format!("{bin} --resume {}", session.key.uuid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;

    fn session(provider: Provider, uuid: &str) -> Session {
        Session {
            key: SessionKey {
                provider,
                uuid: uuid.into(),
            },
            state: State::Idle,
            source: Source::Layer0Inferred,
            confidence: Confidence::Low,
            last_change_ms: 0,
            started_at_ms: None,
            cwd: None,
            title: None,
            entrypoint: None,
            model: None,
            ctx_tokens: None,
            ctx_window: None,
            cpu: None,
            pid: None,
        }
    }

    /// 에이전트마다 세션을 되살리는 플래그가 다르다. 붙여넣었을 때 그대로 도는
    /// 명령이어야 하므로, 모르는 플래그를 생략하고 명령 이름만 내보내면 안 된다.
    #[test]
    fn each_agent_gets_the_flag_it_actually_takes() {
        assert_eq!(
            resume_command(&session(Provider::Claude, "u1")),
            "claude --resume u1"
        );
        assert_eq!(
            resume_command(&session(Provider::Codex, "u2")),
            "codex --resume u2"
        );
        assert_eq!(
            resume_command(&session(Provider::Antigravity, "u3")),
            "agy --conversation=u3"
        );
    }
}
