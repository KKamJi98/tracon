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
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        if let Some(v) = a.strip_prefix("--session-id=") {
            return Some(v.to_string());
        }
        if a == "--session-id" {
            return it.next().cloned();
        }
    }
    None
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
        use sysinfo::{ProcessRefreshKind, RefreshKind};
        self.system
            .refresh_specifics(RefreshKind::new().with_processes(ProcessRefreshKind::everything()));
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
