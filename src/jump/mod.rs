//! 점프 대상 탐색과 실행. tmux는 어느 터미널에서나 쓰이는 표준이고, cmux는
//! 선택적으로 붙는 세션 매니저 어댑터(레이어 2)다.
//!
//! 스냅샷 한 번에 Jumper::refresh를 한 번씩만 호출해 tmux list-panes 같은 비용이
//! 드는 조회를 tick당 한 번으로 묶는다 - resolve_tty는 그 캐시를 읽기만 하는
//! 순수 조회라 세션 수만큼 불러도 새 프로세스를 띄우지 않는다. cmux는 workspace_id로
//! 찾아야 해서 tty 기반인 이 trait을 그대로 쓰지 않는다 - `Collector`가 별도로 다룬다.

pub mod clipboard;
pub mod cmux;
pub mod tmux;

use crate::model::{Provider, Session};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JumpTarget {
    Tmux(String),
    Cmux(String),
}

impl JumpTarget {
    pub fn label(&self) -> String {
        match self {
            JumpTarget::Tmux(t) => format!("tmux:{t}"),
            JumpTarget::Cmux(w) => format!("cmux:{w}"),
        }
    }
}

/// 세션이 어디에 붙어 있는지 찾아내는 소스 하나. Collector가 스냅샷마다 들고 있는
/// 목록을 순회하며 tty가 알려진 프로세스에 대해 조회한다.
///
/// 실제로 그 대상으로 "옮겨가는" 동작은 이 trait에 없다 - Collector와 그 Jumper는
/// 수집 스레드 안에 살고, 키 입력을 받는 메인 스레드는 거기에 접근할 수 없다.
/// 대신 스냅샷이 이미 구운 `Session.jump` 라벨을 [`jump_to`]가 해석해서 실행한다.
pub trait Jumper: Send {
    /// tick마다 한 번 호출된다. 외부 프로세스 실행 등 비용이 드는 조회는 여기서
    /// 끝내고 내부에 캐시해 둔다.
    fn refresh(&mut self);
    /// 순수 조회. tty를 모르면 애초에 호출하지 않는다.
    fn resolve_tty(&self, tty: &str) -> Option<JumpTarget>;
}

pub fn resume_command(session: &Session) -> String {
    let bin = match session.key.provider {
        Provider::Claude => "claude",
        Provider::Codex => "codex",
    };
    format!("{bin} --resume {}", session.key.uuid)
}

/// `Session.jump`에 저장된 라벨(`tmux:<target>` 등)을 해석해 실제로 그 대상으로
/// 옮겨간다. 알려진 prefix가 없으면 에러로 돌려주고, 호출부가 resume 명령 복사로
/// 폴백한다.
pub fn jump_to(label: &str) -> anyhow::Result<()> {
    if let Some(target) = label.strip_prefix("tmux:") {
        return tmux::switch_to(target);
    }
    if let Some(target) = label.strip_prefix("cmux:") {
        return cmux::CmuxJumper::jump(target);
    }
    anyhow::bail!("알 수 없는 점프 대상: {label}")
}
