#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

mod collect;
mod config;
mod hooks;
mod json;
mod merge;
mod model;
mod resume;
mod ui;

use anyhow::Context;
use clap::{Parser, Subcommand};
use std::io::Read;

#[derive(Parser)]
#[command(
    name = "tracon",
    version,
    about = "Approach control for your agent sessions"
)]
struct Cli {
    /// 1회 스냅샷을 JSON으로 출력하고 종료한다.
    #[arg(long)]
    json: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// 훅 설치와 제거
    Hooks {
        #[command(subcommand)]
        action: HooksAction,
    },
    /// 훅에서 호출되는 1회성 기록 경로
    HookEvent {
        #[arg(long, default_value = "claude")]
        provider: String,
    },
}

#[derive(Subcommand)]
enum HooksAction {
    Install,
    Uninstall,
    /// settings 조각만 출력한다. 설정 저장소에 직접 넣고 싶을 때 쓴다.
    Print,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::HookEvent { provider }) => run_hook_event(&provider),
        Some(Command::Hooks { action }) => run_hooks(action),
        None if cli.json => run_json(),
        None => run_tui(),
    }
}

/// 설치된 훅이 호출하는 경로. stdin을 읽어 이벤트를 기록만 하고,
/// 어떤 경우에도 0으로 종료한다 - 실패가 에이전트를 막으면 안 된다.
///
/// stdin 파싱은 지금 유일하게 알려진 모양(claude 훅 stdin)을 그대로 쓴다 - codex
/// 쪽 훅 설치(Task 15 Step 5)는 이번 버전에서 빠졌으므로 codex가 실제로 이 경로를
/// 호출할 일은 아직 없다. 다만 `--provider`를 무시하고 항상 `Provider::Claude`로
/// 기록하던 것은 Task 9부터 있던 known gap이었으므로, 기록되는 키의 provider만은
/// 호출부가 넘긴 값을 따르게 고친다.
fn run_hook_event(provider: &str) -> anyhow::Result<()> {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return Ok(());
    }
    let Some(mut record) = crate::collect::hooksink::event_from_claude_hook(&input) else {
        return Ok(());
    };
    record.key.provider = parse_provider(provider);
    // 훅 payload에는 pid가 없다. 조상 체인에서 찾아 채워 두면 Collector가 프로세스와
    // 세션을 짐작 대신 사실로 맞출 수 있다.
    record.pid = crate::collect::hooksink::agent_ancestor_pid();
    let dir = crate::collect::hooksink::sink_dir();
    let _ = crate::collect::hooksink::record_event(&dir, &record);
    Ok(())
}

fn parse_provider(s: &str) -> crate::model::Provider {
    match s {
        "codex" => crate::model::Provider::Codex,
        _ => crate::model::Provider::Claude,
    }
}

fn settings_path() -> anyhow::Result<std::path::PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(std::path::PathBuf::from(home)
        .join(".claude")
        .join("settings.local.json"))
}

fn exe_path() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_else(|| "tracon".to_string())
}

fn run_hooks(action: HooksAction) -> anyhow::Result<()> {
    let exe = exe_path();
    match action {
        HooksAction::Install => {
            let path = settings_path()?;
            crate::hooks::install(&path, &exe)
        }
        HooksAction::Uninstall => {
            let path = settings_path()?;
            crate::hooks::uninstall(&path)
        }
        HooksAction::Print => {
            let block = crate::hooks::hook_block(&exe);
            println!("{}", serde_json::to_string_pretty(&block)?);
            Ok(())
        }
    }
}

fn run_json() -> anyhow::Result<()> {
    // cmux가 없거나 구독이 즉시 죽으면 None이고, 그 아래 나머지는 cmux가 존재한
    // 적 없는 것처럼 그대로 동작한다 - 레이어 2는 언제나 선택이다. `--json`은
    // 스냅샷 한 번만 찍고 끝나는 호출(예: statusline 폴링)이라 `--reconnect` 없는
    // `spawn_one_shot()`을 쓴다 - 계속 재연결을 시도하는 구독을 한 번 쓰고
    // 버릴 이유가 없다.
    let cmux = crate::collect::cmux::CmuxSubscriber::spawn_one_shot();
    let mut collector = crate::collect::Collector::new(
        Box::new(crate::collect::proc::SysProcessSource::new()),
        crate::config::Thresholds::default(),
    )
    .with_cmux(cmux);
    let snapshot = collector.snapshot(crate::collect::hooksink::now_ms());
    // 스냅샷을 찍은 즉시 collector를 버려 cmux 구독(있다면)을 명시적으로 끊는다 -
    // 프로세스 종료에 기대지 않는다. 그러지 않으면 이 호출이 statusline처럼
    // 몇 초마다 반복될 때마다 cmux 데몬에 자식이 하나씩 쌓인다.
    drop(collector);
    println!("{}", serde_json::to_string(&snapshot)?);
    Ok(())
}

fn run_tui() -> anyhow::Result<()> {
    crate::ui::run_tui()
}
