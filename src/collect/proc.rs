use crate::model::Provider;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct ProcInfo {
    pub pid: i32,
    pub provider: Provider,
    pub session_id: Option<String>,
    pub cwd: Option<PathBuf>,
    pub cpu: f32,
    /// UTC epoch milliseconds
    pub started_at_ms: i64,
}

/// `Send`를 요구하는 이유: Collector가 TUI의 수집 스레드로 통째로 옮겨지므로
/// (`src/ui/mod.rs`의 `run_tui`), 그 안의 `Box<dyn ProcessSource>`도 스레드 경계를
/// 넘을 수 있어야 한다.
pub trait ProcessSource: Send {
    fn list_agents(&mut self) -> Vec<ProcInfo>;
}

pub fn session_id_from_argv(argv: &[String]) -> Option<String> {
    let mut resumed: Option<String> = None;
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        if let Some(v) = a.strip_prefix("--session-id=") {
            return Some(v.to_string());
        }
        if a == "--session-id" {
            return it.next().cloned();
        }
        // `--resume`는 uuid도 세션 이름("Jenkins Upgrade")도 받는다. uuid일 때만
        // 신원이다 - 이어 쓰는 transcript의 파일명이 그 uuid이기 때문이다(실측:
        // 905fb4b6 세션은 3주에 걸쳐 여러 번 resume됐지만 파일도 sessionId도 하나다).
        // 이름은 파일명이 되지 못하므로 버리고, 아래 mtime 추측 경로로 내려보낸다.
        //
        // `--session-id`가 뒤에 또 나올 수 있으므로 여기서 바로 돌려주지 않고 들고만 간다.
        let candidate = match a.strip_prefix("--resume=") {
            Some(v) => Some(v.to_string()),
            None if a == "--resume" => it.next().cloned(),
            None => None,
        };
        if let Some(v) = candidate {
            if is_uuid(&v) {
                resumed = Some(v);
            }
        }
    }
    resumed
}

/// 8-4-4-4-12 hex. claude가 transcript 파일명으로 쓰는 형태다.
fn is_uuid(s: &str) -> bool {
    let groups = [8, 4, 4, 4, 12];
    let mut parts = s.split('-');
    for want in groups {
        let Some(part) = parts.next() else {
            return false;
        };
        if part.len() != want || !part.bytes().all(|b| b.is_ascii_hexdigit()) {
            return false;
        }
    }
    parts.next().is_none()
}

/// argv[0]의 파일명만 본다. 래퍼가 인자로 에이전트 이름을 넘기는 경우는 제외한다.
pub fn provider_from_argv(argv: &[String]) -> Option<Provider> {
    let exe = argv.first()?;
    let name = std::path::Path::new(exe).file_name()?.to_str()?;
    match name {
        "claude" => Some(Provider::Claude),
        "codex" => Some(Provider::Codex),
        "agy" => Some(Provider::Antigravity),
        _ => None,
    }
}

pub struct SysProcessSource {
    system: sysinfo::System,
}

impl SysProcessSource {
    pub fn new() -> Self {
        Self {
            system: sysinfo::System::new(),
        }
    }
}

impl Default for SysProcessSource {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessSource for SysProcessSource {
    fn list_agents(&mut self) -> Vec<ProcInfo> {
        use sysinfo::{ProcessRefreshKind, ProcessesToUpdate};
        // `remove_dead_processes = true`가 이 호출의 핵심이다. 편해 보이는
        // `refresh_specifics`는 내부적으로 이 값을 false로 넘기고, 그러면 끝난
        // 프로세스가 `System` 안에 영원히 남는다. `--json`은 매번 새 프로세스라
        // 증상이 안 보이지만, `System` 하나를 계속 쓰는 TUI에서는 닫은 세션이
        // 화면에서 사라지지 않는다.
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::everything(),
        );
        let infos: Vec<ProcInfo> = self
            .system
            .processes()
            .values()
            .filter_map(|p| {
                let argv: Vec<String> = p
                    .cmd()
                    .iter()
                    .map(|s| s.to_string_lossy().into_owned())
                    .collect();
                let provider = provider_from_argv(&argv)?;
                Some(ProcInfo {
                    pid: p.pid().as_u32() as i32,
                    provider,
                    session_id: session_id_from_argv(&argv),
                    cwd: p.cwd().map(|c| c.to_path_buf()),
                    cpu: p.cpu_usage(),
                    started_at_ms: (p.start_time() as i64) * 1000,
                })
            })
            .collect();
        infos
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Provider;

    fn argv(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    /// 죽은 프로세스는 다음 tick에 사라져야 한다.
    ///
    /// sysinfo의 `refresh_specifics`는 내부적으로 `remove_dead_processes = false`로
    /// 돈다. 그래서 오래 사는 `System` 하나를 계속 쓰는 TUI에서는 끝난 세션이 목록에
    /// 영원히 남는다. `--json`은 매번 새 프로세스라 이 증상이 안 보인다 - 그쪽으로만
    /// 확인하면 통과하는 것처럼 보이므로, 이 테스트는 한 source를 두 번 부른다.
    #[test]
    fn a_dead_agent_disappears_from_the_next_refresh() {
        // basename만 보고 에이전트를 가리므로, sleep에 claude 이름을 붙여 띄운다.
        let dir = tempfile::tempdir().expect("dir");
        let fake = dir.path().join("claude");
        std::os::unix::fs::symlink("/bin/sleep", &fake).expect("symlink");

        let mut child = std::process::Command::new(&fake)
            .arg("30")
            .spawn()
            .expect("spawn");
        let pid = child.id() as i32;

        let mut source = SysProcessSource::new();
        let listed = |s: &mut SysProcessSource| s.list_agents().iter().any(|p| p.pid == pid);
        assert!(listed(&mut source), "살아 있는 에이전트를 못 봤다");

        child.kill().expect("kill");
        child.wait().expect("wait");

        assert!(
            !listed(&mut source),
            "죽은 프로세스가 같은 source의 다음 조회에 그대로 남았다"
        );
    }

    #[test]
    fn extracts_session_id_flag() {
        let a = argv(&["/home/dev/.local/bin/claude", "--session-id", "abc-123"]);
        assert_eq!(session_id_from_argv(&a).as_deref(), Some("abc-123"));
    }

    #[test]
    fn extracts_session_id_with_equals_form() {
        let a = argv(&["claude", "--session-id=abc-456"]);
        assert_eq!(session_id_from_argv(&a).as_deref(), Some("abc-456"));
    }

    #[test]
    fn resume_with_uuid_is_the_session_id() {
        let a = argv(&["claude", "--resume", "09775af0-3d97-411e-885b-6b9e1caec6e2"]);
        assert_eq!(
            session_id_from_argv(&a).as_deref(),
            Some("09775af0-3d97-411e-885b-6b9e1caec6e2")
        );
    }

    #[test]
    fn resume_with_uuid_equals_form_is_the_session_id() {
        let a = argv(&["claude", "--resume=905fb4b6-80f0-4c2d-8cdf-d42c981d45ff"]);
        assert_eq!(
            session_id_from_argv(&a).as_deref(),
            Some("905fb4b6-80f0-4c2d-8cdf-d42c981d45ff")
        );
    }

    /// `--session-id`가 이기는 규칙은 "먼저 만나면 즉시 돌려준다"는 구조에 기대고
    /// 있다. 나중에 `--resume`처럼 끝까지 훑어 마지막에 정하는 형태로 정리하면
    /// 조용히 뒤집히므로, 두 순서를 모두 고정해 둔다.
    #[test]
    fn session_id_outranks_resume_whatever_the_order() {
        let want = Some("b1c8feb3-6baf-4aa3-8e07-eb1dc2779b29");
        let resume_first = argv(&[
            "claude",
            "--resume",
            "09775af0-3d97-411e-885b-6b9e1caec6e2",
            "--session-id",
            "b1c8feb3-6baf-4aa3-8e07-eb1dc2779b29",
        ]);
        assert_eq!(session_id_from_argv(&resume_first).as_deref(), want);

        let session_id_first = argv(&[
            "claude",
            "--session-id",
            "b1c8feb3-6baf-4aa3-8e07-eb1dc2779b29",
            "--resume",
            "09775af0-3d97-411e-885b-6b9e1caec6e2",
        ]);
        assert_eq!(session_id_from_argv(&session_id_first).as_deref(), want);
    }

    /// `--resume`는 세션 이름도 받는다. 이름은 uuid가 아니므로 transcript 파일명이
    /// 되지 못한다 - 신원으로 쓰면 없는 경로를 가리킨다.
    #[test]
    fn resume_with_session_name_is_not_a_session_id() {
        let a = argv(&["claude", "--resume", "Jenkins Upgrade"]);
        assert_eq!(session_id_from_argv(&a), None);
    }

    #[test]
    fn absent_flag_yields_none() {
        let a = argv(&["claude", "--settings", "{}"]);
        assert_eq!(session_id_from_argv(&a), None);
    }

    #[test]
    fn detects_provider_from_binary_name() {
        assert_eq!(
            provider_from_argv(&argv(&["/x/bin/claude"])),
            Some(Provider::Claude)
        );
        assert_eq!(
            provider_from_argv(&argv(&["/x/bin/codex", "exec"])),
            Some(Provider::Codex)
        );
        assert_eq!(provider_from_argv(&argv(&["/x/bin/vim"])), None);
    }

    #[test]
    fn wrapper_process_is_not_mistaken_for_the_agent() {
        // agentctx _exec claude 같은 래퍼는 에이전트 본체가 아니다.
        assert_eq!(
            provider_from_argv(&argv(&["agentctx", "_exec", "claude"])),
            None
        );
    }
}
