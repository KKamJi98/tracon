//! tmux 어댑터. `tmux list-panes`로 tty -> pane 대상 맵을 만들고, 세션의 tty로
//! 조회해 점프 대상을 찾는다. tmux가 없거나 서버가 없으면 빈 맵을 반환할 뿐,
//! 에러로 취급하지 않는다 - 어느 터미널에서나 쓰인다는 것이 이 프로젝트의 제약이다.

use super::{JumpTarget, Jumper};
use std::collections::HashMap;
use std::process::Command;

#[derive(Debug, Default)]
pub struct TmuxJumper {
    /// tty -> "session:window.pane"
    panes: HashMap<String, String>,
}

impl TmuxJumper {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Jumper for TmuxJumper {
    fn refresh(&mut self) {
        self.panes = list_panes();
    }

    fn resolve_tty(&self, tty: &str) -> Option<JumpTarget> {
        self.panes.get(tty).cloned().map(JumpTarget::Tmux)
    }
}

/// `tmux list-panes -a`를 실행해 tty -> pane 대상 맵을 만든다. tmux가 설치되어
/// 있지 않거나 서버가 없으면(비영 0 종료) 빈 맵을 돌려준다 - 이는 에러가 아니라
/// "이 세션은 tmux 밖에 있다"는 평범한 상태다.
fn list_panes() -> HashMap<String, String> {
    let Ok(out) = Command::new("tmux")
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{pane_tty} #{session_name}:#{window_index}.#{pane_index}",
        ])
        .output()
    else {
        return HashMap::new();
    };
    if !out.status.success() {
        return HashMap::new();
    }
    parse_pane_list(&String::from_utf8_lossy(&out.stdout))
}

/// 현재 클라이언트를 그 대상(`session:window.pane`)으로 옮긴다. `switch-client`로
/// 세션/윈도우를 맞추고 `select-pane`으로 정확한 pane까지 선택한다. 어느 하나가
/// 실패해도(예: 클라이언트가 붙어 있지 않은 bare 세션) 패닉하지 않고 에러를
/// 돌려준다 - 호출부가 resume 명령 복사로 폴백한다.
pub fn switch_to(target: &str) -> anyhow::Result<()> {
    let _ = Command::new("tmux")
        .args(["switch-client", "-t", target])
        .status();
    Command::new("tmux")
        .args(["select-pane", "-t", target])
        .status()
        .map_err(|e| anyhow::anyhow!("tmux select-pane 실행 실패: {e}"))
        .and_then(|status| {
            if status.success() {
                Ok(())
            } else {
                anyhow::bail!("tmux select-pane이 실패 종료했습니다: {target}")
            }
        })
}

/// `tmux list-panes -a -F '#{pane_tty} #{session_name}:#{window_index}.#{pane_index}'`
/// 출력을 파싱한다.
pub fn parse_pane_list(out: &str) -> HashMap<String, String> {
    out.lines()
        .filter_map(|l| {
            let mut parts = l.split_whitespace();
            let tty = parts.next()?;
            let target = parts.next()?;
            if !tty.starts_with("/dev/") {
                return None;
            }
            Some((tty.to_string(), target.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::resume_command;
    use super::*;

    const PANES: &str = "/dev/ttys004 main:2.1\n/dev/ttys009 work:0.0\n";

    #[test]
    fn parses_pane_list_into_tty_map() {
        let m = parse_pane_list(PANES);
        assert_eq!(m.get("/dev/ttys004").map(String::as_str), Some("main:2.1"));
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn unknown_tty_has_no_target() {
        let m = parse_pane_list(PANES);
        assert_eq!(m.get("/dev/ttys999"), None);
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let m = parse_pane_list("garbage\n/dev/ttys004 main:2.1\n");
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn resume_command_uses_provider_binary_and_uuid() {
        let s = crate::ui::tests::sample_session();
        assert_eq!(
            resume_command(&s),
            format!("claude --resume {}", s.key.uuid)
        );
    }
}
