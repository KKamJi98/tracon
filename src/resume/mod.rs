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
    };
    format!("{bin} --resume {}", session.key.uuid)
}
