#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

mod collect;
mod config;
mod hooks;
mod json;
mod merge;
mod model;
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
fn run_hook_event(_provider: &str) -> anyhow::Result<()> {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return Ok(());
    }
    let Some(record) = crate::collect::hooksink::event_from_claude_hook(&input) else {
        return Ok(());
    };
    let dir = crate::collect::hooksink::sink_dir();
    let _ = crate::collect::hooksink::record_event(&dir, &record);
    Ok(())
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
    let mut collector = crate::collect::Collector::new(
        Box::new(crate::collect::proc::SysProcessSource::new()),
        crate::config::Thresholds::default(),
    );
    let snapshot = collector.snapshot(crate::collect::hooksink::now_ms());
    println!("{}", serde_json::to_string(&snapshot)?);
    Ok(())
}

fn run_tui() -> anyhow::Result<()> {
    crate::ui::run_tui()
}
