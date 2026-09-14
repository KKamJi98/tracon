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

fn backup(path: &Path) -> anyhow::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let stamp = crate::collect::hooksink::now_ms();
    let dest = path.with_extension(format!("json.bak-{stamp}"));
    std::fs::copy(path, dest)?;
    Ok(())
}

fn is_ours(entry: &Value) -> bool {
    serde_json::to_string(entry)
        .map(|s| s.contains(MARKER))
        .unwrap_or(false)
}

pub fn install(path: &Path, exe: &str) -> anyhow::Result<()> {
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
    std::fs::write(path, serde_json::to_vec_pretty(&settings)?)?;
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
    std::fs::write(path, serde_json::to_vec_pretty(&settings)?)?;
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
