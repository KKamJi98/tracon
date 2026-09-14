use crate::model::Provider;
use std::collections::HashMap;
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
    pub tty: Option<String>,
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
        let mut infos: Vec<ProcInfo> = self
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
                    tty: None,
                })
            })
            .collect();

        // 세션마다 `ps`를 새로 띄우면 프로세스 수만큼 spawn이 늘어난다. pid를
        // 한 번에 모아 단일 호출로 조회해 tick당 spawn 수를 세션 수와 무관하게 만든다.
        let pids: Vec<i32> = infos.iter().map(|i| i.pid).collect();
        let tty_map = tty_for_pids(&pids);
        for info in &mut infos {
            info.tty = tty_map.get(&info.pid).cloned();
        }
        infos
    }
}

/// `ps -o pid=,tty= -p <pid1,pid2,...>` 한 번으로 여러 pid의 tty를 조회한다.
/// pid 목록이 비어 있으면 아예 spawn하지 않는다. `ps`가 없거나 실패해도 빈 맵을
/// 돌려줄 뿐 에러로 취급하지 않는다 - tty를 못 찾으면 그냥 JUMP 열이 `-`로 남는다.
fn tty_for_pids(pids: &[i32]) -> HashMap<i32, String> {
    if pids.is_empty() {
        return HashMap::new();
    }
    let pid_list = pids
        .iter()
        .map(i32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let Ok(out) = std::process::Command::new("ps")
        .args(["-o", "pid=,tty=", "-p", &pid_list])
        .output()
    else {
        return HashMap::new();
    };
    if !out.status.success() {
        return HashMap::new();
    }
    parse_ps_tty_output(&String::from_utf8_lossy(&out.stdout))
}

/// `ps -o pid=,tty=` 출력(줄마다 `<pid> <tty>`)을 파싱해 tty를 `/dev/`로 정규화한다.
fn parse_ps_tty_output(text: &str) -> HashMap<i32, String> {
    text.lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let pid: i32 = parts.next()?.parse().ok()?;
            let tty = normalize_tty(parts.next()?)?;
            Some((pid, tty))
        })
        .collect()
}

/// ps가 보여주는 tty 표기(`ttys004`, `pts/0`, `?`/`??`은 tty 없음)를
/// `tmux list-panes`가 보고하는 `/dev/` 접두 경로 형태로 맞춘다.
fn normalize_tty(raw: &str) -> Option<String> {
    if raw.is_empty() || raw == "?" || raw == "??" {
        return None;
    }
    if let Some(stripped) = raw.strip_prefix("/dev/") {
        return Some(format!("/dev/{stripped}"));
    }
    Some(format!("/dev/{raw}"))
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

    #[test]
    fn normalizes_bare_tty_name_to_dev_path() {
        assert_eq!(normalize_tty("ttys004").as_deref(), Some("/dev/ttys004"));
        assert_eq!(normalize_tty("pts/0").as_deref(), Some("/dev/pts/0"));
    }

    #[test]
    fn already_prefixed_tty_is_kept_as_is() {
        assert_eq!(
            normalize_tty("/dev/ttys004").as_deref(),
            Some("/dev/ttys004")
        );
    }

    #[test]
    fn no_tty_markers_normalize_to_none() {
        assert_eq!(normalize_tty("?"), None);
        assert_eq!(normalize_tty("??"), None);
        assert_eq!(normalize_tty(""), None);
    }

    #[test]
    fn parses_multi_pid_ps_output_and_skips_processes_without_a_tty() {
        let out = "  100 ttys004\n  200 ??\n  300 pts/1\n";
        let m = parse_ps_tty_output(out);
        assert_eq!(m.get(&100).map(String::as_str), Some("/dev/ttys004"));
        assert_eq!(m.get(&200), None);
        assert_eq!(m.get(&300).map(String::as_str), Some("/dev/pts/1"));
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn tty_lookup_spawns_nothing_for_an_empty_pid_list() {
        // pid가 없으면 ps를 아예 띄우지 않는다 - 살아 있는 에이전트가 없는 tick에서
        // 불필요한 spawn을 만들지 않기 위함이다. 빈 맵이면 spawn을 건너뛴 것이다.
        assert!(tty_for_pids(&[]).is_empty());
    }
}
