//! cmux 워크스페이스 점프. `cmux workspace list --id-format both`로
//! workspace UUID -> short ref(`workspace:3`) 맵을 만들고, 실제로 옮겨갈 때는
//! `cmux workspace select`와 `cmux focus-window`를 부른다.
//!
//! 명령 실행을 [`CommandRunner`]로 감싼 이유는 이 파일의 테스트가 진짜 cmux를
//! 절대 실행하면 안 되기 때문이다 - 사용자의 살아 있는 작업 환경(워크스페이스,
//! 창 포커스)을 테스트가 흔들면 안 된다. 운영 코드는 [`RealRunner`]를 쓰고,
//! 테스트는 가짜 러너로 호출 인자만 검증한다.

use super::JumpTarget;
use std::collections::HashMap;
use std::process::{Command, Output};

pub trait CommandRunner {
    fn run(&self, args: &[&str]) -> std::io::Result<Output>;
}

struct RealRunner;

impl CommandRunner for RealRunner {
    fn run(&self, args: &[&str]) -> std::io::Result<Output> {
        Command::new("cmux").args(args).output()
    }
}

#[derive(Default)]
pub struct CmuxJumper {
    /// workspace uuid -> short ref (예: "workspace:3")
    workspaces: HashMap<String, String>,
}

impl CmuxJumper {
    pub fn new() -> Self {
        Self::default()
    }

    /// tick마다 한 번, `cmux workspace list --id-format both`로 캐시를 채운다.
    /// cmux가 없거나 명령이 실패하면(비영 0 종료 포함) 빈 맵을 남긴다 - 에러가
    /// 아니라 "지금은 workspace를 모른다"는 평범한 상태다.
    pub fn refresh(&mut self) {
        self.workspaces = list_workspaces(&RealRunner);
    }

    /// 순수 조회. workspace uuid를 모르면(아직 refresh 전이거나 알려지지 않은
    /// workspace) `None`.
    pub fn resolve(&self, workspace_id: &str) -> Option<JumpTarget> {
        self.workspaces
            .get(workspace_id)
            .cloned()
            .map(JumpTarget::Cmux)
    }

    /// `target`(short ref, 예: "workspace:3")로 실제로 옮겨간다. workspace를
    /// 활성화한 뒤 창을 앞으로 가져온다. workspace 활성화가 실패하면 에러를
    /// 돌려준다 - 호출부가 resume 명령 복사로 폴백한다. focus-window 실패는
    /// best-effort라 무시한다(예: 이미 그 창이 보이는 경우).
    pub fn jump(target: &str) -> anyhow::Result<()> {
        jump_with(&RealRunner, target)
    }
}

fn list_workspaces(runner: &dyn CommandRunner) -> HashMap<String, String> {
    let Ok(out) = runner.run(&["workspace", "list", "--id-format", "both"]) else {
        return HashMap::new();
    };
    if !out.status.success() {
        return HashMap::new();
    }
    parse_workspace_list(&String::from_utf8_lossy(&out.stdout))
}

fn jump_with(runner: &dyn CommandRunner, target: &str) -> anyhow::Result<()> {
    let select = runner
        .run(&["workspace", "select", target])
        .map_err(|e| anyhow::anyhow!("cmux workspace select 실행 실패: {e}"))?;
    if !select.status.success() {
        anyhow::bail!("cmux workspace select가 실패 종료했습니다: {target}");
    }
    let _ = runner.run(&["focus-window", "--window", target]);
    Ok(())
}

/// `cmux workspace list --id-format both` 출력을 파싱한다. 한 줄은
/// `[*] <short-ref> <uuid>  <나머지 메타데이터>` 모양이다 - 선택된 workspace는
/// `*`로 시작하고, 나머지(제목 등)는 공백을 포함할 수 있어 앞의 두 토큰만 쓴다.
fn parse_workspace_list(out: &str) -> HashMap<String, String> {
    out.lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let mut first = parts.next()?;
            if first == "*" {
                first = parts.next()?;
            }
            let short_ref = first;
            let uuid = parts.next()?;
            Some((uuid.to_string(), short_ref.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    const LIST_OUT: &str = "* workspace:1 AAAAAAAA-0000-0000-0000-000000000001  P0  [selected]\n  workspace:3 BBBBBBBB-0000-0000-0000-000000000002  Fake Title\n";

    fn ok(stdout: &str) -> Output {
        Output {
            status: ExitStatus::from_raw(0),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    fn failed() -> Output {
        Output {
            status: ExitStatus::from_raw(1 << 8),
            stdout: Vec::new(),
            stderr: Vec::new(),
        }
    }

    #[test]
    fn parses_selected_and_unselected_workspace_lines() {
        let m = parse_workspace_list(LIST_OUT);
        assert_eq!(
            m.get("AAAAAAAA-0000-0000-0000-000000000001")
                .map(String::as_str),
            Some("workspace:1")
        );
        assert_eq!(
            m.get("BBBBBBBB-0000-0000-0000-000000000002")
                .map(String::as_str),
            Some("workspace:3")
        );
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn unknown_workspace_has_no_target() {
        let m = parse_workspace_list(LIST_OUT);
        assert_eq!(m.get("no-such-uuid"), None);
    }

    #[test]
    fn resolve_wraps_short_ref_as_cmux_jump_target() {
        let mut jumper = CmuxJumper::new();
        jumper.workspaces = parse_workspace_list(LIST_OUT);
        assert_eq!(
            jumper.resolve("BBBBBBBB-0000-0000-0000-000000000002"),
            Some(JumpTarget::Cmux("workspace:3".to_string()))
        );
        assert_eq!(jumper.resolve("no-such-uuid"), None);
    }

    /// 호출된 명령과 인자를 기록만 하고 절대 실행하지 않는 가짜 러너. 모든 호출에
    /// 같은 결과를 돌려주되, `workspace select`만 `select_ok`로 성공/실패를 고른다
    /// - jump 테스트에서 select 실패 시 focus-window가 불리지 않는지 보려면 필요하다.
    struct FakeRunner {
        calls: RefCell<Vec<Vec<String>>>,
        list_output: Output,
        select_ok: bool,
    }

    impl FakeRunner {
        fn listing(out: Output) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                list_output: out,
                select_ok: true,
            }
        }

        fn jumping(select_ok: bool) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                list_output: ok(""),
                select_ok,
            }
        }

        fn clone_output(o: &Output) -> Output {
            Output {
                status: o.status,
                stdout: o.stdout.clone(),
                stderr: o.stderr.clone(),
            }
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, args: &[&str]) -> std::io::Result<Output> {
            self.calls
                .borrow_mut()
                .push(args.iter().map(|s| s.to_string()).collect());
            if args.first() == Some(&"workspace") && args.get(1) == Some(&"select") {
                return Ok(if self.select_ok { ok("") } else { failed() });
            }
            if args.first() == Some(&"workspace") && args.get(1) == Some(&"list") {
                return Ok(Self::clone_output(&self.list_output));
            }
            Ok(ok(""))
        }
    }

    #[test]
    fn list_workspaces_parses_a_successful_command_output() {
        let runner = FakeRunner::listing(ok(LIST_OUT));
        let m = list_workspaces(&runner);
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn list_workspaces_is_empty_when_command_fails() {
        let runner = FakeRunner::listing(failed());
        let m = list_workspaces(&runner);
        assert!(m.is_empty());
    }

    #[test]
    fn jump_selects_workspace_then_focuses_window_without_touching_real_cmux() {
        let runner = FakeRunner::jumping(true);
        jump_with(&runner, "workspace:3").expect("jump succeeds");
        let calls = runner.calls.borrow();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], vec!["workspace", "select", "workspace:3"]);
        assert_eq!(calls[1], vec!["focus-window", "--window", "workspace:3"]);
    }

    #[test]
    fn jump_fails_when_workspace_select_fails_and_never_calls_focus_window() {
        let runner = FakeRunner::jumping(false);
        assert!(jump_with(&runner, "workspace:3").is_err());
        let calls = runner.calls.borrow();
        assert_eq!(
            calls.len(),
            1,
            "focus-window must not run after a failed select"
        );
    }
}
