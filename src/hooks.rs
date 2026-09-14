use anyhow::Context;
use serde_json::{json, Value};
use std::path::Path;

const EVENTS: [&str; 9] = [
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PermissionRequest",
    "Notification",
    "Stop",
    "SubagentStop",
    "SessionEnd",
];

const MARKER: &str = "hook-event --provider";

pub fn hook_block(exe: &str) -> Value {
    let mut map = serde_json::Map::new();
    for ev in EVENTS {
        map.insert(
            ev.to_string(),
            json!([{ "hooks": [{ "type": "command", "command": format!("{exe} hook-event --provider claude") }] }]),
        );
    }
    Value::Object(map)
}

fn load(path: &Path) -> anyhow::Result<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(json!({}));
    }
    Ok(serde_json::from_str(&raw)?)
}

const BACKUP_INFIX: &str = ".bak-";

fn backup(path: &Path) -> anyhow::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let stamp = crate::collect::hooksink::now_ms();
    let dest = path.with_extension(format!("json{BACKUP_INFIX}{stamp}"));
    std::fs::copy(path, &dest)?;
    prune_backups(path, &dest);
    Ok(())
}

/// 방금 만든 백업 하나만 남기고 이전 백업을 지운다. 설치와 제거 때마다 하나씩
/// 쌓이면 사용자의 `~/.claude`가 아무도 치우지 않는 백업으로 덮인다.
fn prune_backups(path: &Path, keep: &Path) {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str())) else {
        return;
    };
    let prefix = format!("{name}{BACKUP_INFIX}");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        if entry.path() == keep {
            continue;
        }
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// 임시 파일에 쓴 뒤 rename한다. 같은 디렉터리 안의 rename은 원자적이라, 쓰는
/// 도중에 죽어도 사용자의 settings가 반쯤 쓰인 채로 남지 않는다.
fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "settings.local.json".to_string());
    let tmp = dir.join(format!("{name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename into {}", path.display()))?;
    Ok(())
}

/// `cargo run`으로 실행하면 `current_exe()`가 `target/debug/tracon`을 가리킨다.
/// 그 경로를 settings에 박아 두면 `cargo clean` 한 번에 모든 claude 세션의 훅이
/// 실패한다. 조용히 망가지느니 설치를 거부한다.
fn is_build_artifact(exe: &str) -> bool {
    Path::new(exe)
        .components()
        .any(|c| c.as_os_str() == "target")
}

fn is_ours(entry: &Value) -> bool {
    serde_json::to_string(entry)
        .map(|s| s.contains(MARKER))
        .unwrap_or(false)
}

pub fn install(path: &Path, exe: &str) -> anyhow::Result<()> {
    if is_build_artifact(exe) {
        anyhow::bail!(
            "빌드 산출물 경로라 설치하지 않습니다: {exe}\n\
             이 경로는 `cargo clean` 한 번에 사라지고, 그 뒤로는 모든 claude 세션에서 \
             훅이 실패합니다. 바이너리를 먼저 설치한 뒤(cargo install --git \
             https://github.com/KKamJi98/tracon) 설치된 tracon으로 다시 실행하세요."
        );
    }
    backup(path)?;
    let mut settings = load(path)?;
    let block = hook_block(exe);
    let hooks = settings
        .as_object_mut()
        .context("settings root is not an object")?
        .entry("hooks")
        .or_insert_with(|| json!({}));
    let hooks = hooks.as_object_mut().context("hooks is not an object")?;

    for ev in EVENTS {
        let list = hooks.entry(ev.to_string()).or_insert_with(|| json!([]));
        let arr = list.as_array_mut().context("hook list is not an array")?;
        arr.retain(|e| !is_ours(e));
        if let Some(ours) = block[ev].as_array().and_then(|a| a.first()) {
            arr.push(ours.clone());
        }
    }
    write_atomic(path, &serde_json::to_vec_pretty(&settings)?)?;
    Ok(())
}

pub fn uninstall(path: &Path) -> anyhow::Result<()> {
    backup(path)?;
    let mut settings = load(path)?;
    if let Some(hooks_value) = settings.get_mut("hooks") {
        let hooks = hooks_value
            .as_object_mut()
            .context("hooks is not an object")?;
        for (_, list) in hooks.iter_mut() {
            if let Some(arr) = list.as_array_mut() {
                arr.retain(|e| !is_ours(e));
            }
        }
        hooks.retain(|_, v| v.as_array().map(|a| !a.is_empty()).unwrap_or(true));
    }
    write_atomic(path, &serde_json::to_vec_pretty(&settings)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_preserves_existing_hooks() {
        let dir = tempfile::tempdir().expect("dir");
        let p = dir.path().join("settings.local.json");
        std::fs::write(
            &p,
            r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"other.sh"}]}]}}"#,
        )
        .expect("seed");
        install(&p, "/usr/local/bin/tracon").expect("install");
        let v: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&p).expect("read")).expect("json");
        let pre = v["hooks"]["PreToolUse"].as_array().expect("array");
        let flat = serde_json::to_string(pre).expect("str");
        assert!(flat.contains("other.sh"));
        assert!(flat.contains("tracon"));
    }

    #[test]
    fn install_is_idempotent() {
        let dir = tempfile::tempdir().expect("dir");
        let p = dir.path().join("settings.local.json");
        std::fs::write(&p, "{}").expect("seed");
        install(&p, "/usr/local/bin/tracon").expect("first");
        install(&p, "/usr/local/bin/tracon").expect("second");
        let v: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&p).expect("read")).expect("json");
        let s = serde_json::to_string(&v["hooks"]["Stop"]).expect("str");
        assert_eq!(s.matches("tracon").count(), 1);
    }

    #[test]
    fn install_writes_a_backup() {
        let dir = tempfile::tempdir().expect("dir");
        let p = dir.path().join("settings.local.json");
        std::fs::write(&p, "{}").expect("seed");
        install(&p, "/usr/local/bin/tracon").expect("install");
        let backups: Vec<_> = std::fs::read_dir(dir.path())
            .expect("dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".bak-"))
            .collect();
        assert_eq!(backups.len(), 1);
    }

    fn backup_names(dir: &std::path::Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .expect("dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".bak-"))
            .collect();
        v.sort();
        v
    }

    /// `cargo run`으로 설치하면 `current_exe()`가 `target/debug/tracon`을 가리킨다.
    /// 그 경로가 settings에 박히면 `cargo clean` 한 번에 모든 claude 세션에서 훅이
    /// 실패한다. 조용히 망가지느니 설치를 거부한다.
    #[test]
    fn install_refuses_an_exe_inside_a_target_directory() {
        let dir = tempfile::tempdir().expect("dir");
        let p = dir.path().join("settings.local.json");
        std::fs::write(&p, "{}").expect("seed");

        let err = install(&p, "/home/dev/tracon/target/debug/tracon")
            .expect_err("빌드 산출물 경로는 거부해야 한다");
        assert!(err.to_string().contains("target"), "메시지: {err}");
        assert_eq!(
            std::fs::read_to_string(&p).expect("read"),
            "{}",
            "거부된 설치는 settings를 건드리지 않는다"
        );
        assert!(
            backup_names(dir.path()).is_empty(),
            "거부된 설치는 백업도 남기지 않는다"
        );
    }

    #[test]
    fn install_keeps_only_the_most_recent_backup() {
        let dir = tempfile::tempdir().expect("dir");
        let p = dir.path().join("settings.local.json");
        std::fs::write(&p, "{}").expect("seed");
        std::fs::write(dir.path().join("settings.local.json.bak-1"), "old").expect("old backup 1");
        std::fs::write(dir.path().join("settings.local.json.bak-2"), "old").expect("old backup 2");

        install(&p, "/usr/local/bin/tracon").expect("install");

        let names = backup_names(dir.path());
        assert_eq!(names.len(), 1, "백업은 최신 하나만 남는다: {names:?}");
        assert!(!names[0].ends_with(".bak-1") && !names[0].ends_with(".bak-2"));
    }

    #[test]
    fn uninstall_keeps_only_the_most_recent_backup() {
        let dir = tempfile::tempdir().expect("dir");
        let p = dir.path().join("settings.local.json");
        std::fs::write(&p, "{}").expect("seed");
        std::fs::write(dir.path().join("settings.local.json.bak-1"), "old").expect("old backup");

        uninstall(&p).expect("uninstall");

        assert_eq!(backup_names(dir.path()).len(), 1);
    }

    /// settings 쓰기는 임시 파일 + rename이라, 쓰다 죽어도 반쯤 쓰인 파일이 남지
    /// 않는다. 성공 경로에서도 임시 파일이 남으면 안 된다.
    #[test]
    fn install_leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().expect("dir");
        let p = dir.path().join("settings.local.json");
        std::fs::write(&p, "{}").expect("seed");
        install(&p, "/usr/local/bin/tracon").expect("install");
        uninstall(&p).expect("uninstall");

        let leftovers: Vec<String> = std::fs::read_dir(dir.path())
            .expect("dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "임시 파일이 남았다: {leftovers:?}");
    }

    #[test]
    fn uninstall_removes_only_our_hooks() {
        let dir = tempfile::tempdir().expect("dir");
        let p = dir.path().join("settings.local.json");
        std::fs::write(
            &p,
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"keepme.sh"}]}]}}"#,
        )
        .expect("seed");
        install(&p, "/usr/local/bin/tracon").expect("install");
        uninstall(&p).expect("uninstall");
        let raw = std::fs::read_to_string(&p).expect("read");
        assert!(raw.contains("keepme.sh"));
        assert!(!raw.contains("tracon"));
    }

    #[test]
    fn hook_block_covers_all_nine_events() {
        let v = hook_block("/usr/local/bin/tracon");
        let obj = v.as_object().expect("object");
        assert_eq!(obj.len(), 9);
        for name in [
            "SessionStart",
            "UserPromptSubmit",
            "PreToolUse",
            "PostToolUse",
            "PermissionRequest",
            "Notification",
            "Stop",
            "SubagentStop",
            "SessionEnd",
        ] {
            assert!(obj.contains_key(name), "missing {name}");
        }
    }

    #[test]
    fn hook_block_command_contains_marker() {
        let v = hook_block("/usr/local/bin/tracon");
        let cmd = v["Stop"][0]["hooks"][0]["command"]
            .as_str()
            .expect("command string");
        assert!(
            cmd.contains(MARKER),
            "command {cmd} does not contain MARKER {MARKER}"
        );
    }

    #[test]
    fn uninstall_errors_on_malformed_hooks() {
        let dir = tempfile::tempdir().expect("dir");
        let p = dir.path().join("settings.local.json");
        let original = r#"{"hooks":"not-an-object"}"#;
        std::fs::write(&p, original).expect("seed");
        let result = uninstall(&p);
        assert!(
            result.is_err(),
            "expected uninstall to error on malformed hooks"
        );
        let raw = std::fs::read_to_string(&p).expect("read");
        assert_eq!(raw, original, "file must be untouched on error");
    }

    #[test]
    fn uninstall_succeeds_when_hooks_absent() {
        let dir = tempfile::tempdir().expect("dir");
        let p = dir.path().join("settings.local.json");
        std::fs::write(&p, "{}").expect("seed");
        uninstall(&p).expect("uninstall should be a no-op success when hooks key is absent");
    }

    #[test]
    fn install_and_uninstall_work_with_exe_name_not_containing_tracon() {
        let dir = tempfile::tempdir().expect("dir");
        let p = dir.path().join("settings.local.json");
        std::fs::write(&p, "{}").expect("seed");
        let exe = "/usr/local/bin/tc";
        install(&p, exe).expect("first install");
        install(&p, exe).expect("second install");
        let v: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&p).expect("read")).expect("json");
        for ev in EVENTS {
            let arr = v["hooks"][ev].as_array().expect("array");
            assert_eq!(arr.len(), 1, "expected exactly one entry for {ev}");
        }
        uninstall(&p).expect("uninstall");
        let raw = std::fs::read_to_string(&p).expect("read");
        assert!(
            !raw.contains(exe),
            "uninstall must remove the tc-installed hooks"
        );
    }
}
