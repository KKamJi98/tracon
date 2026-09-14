//! 클립보드 복사. 새 의존성 없이 외부 명령을 파이프로 실행한다.
//! macOS는 pbcopy, Wayland는 wl-copy, X11은 xclip 순으로 시도하고, 전부 없거나
//! 실패하면 false를 돌려준다 - 호출부가 명령 문자열을 화면에 보여주는 폴백을 쓴다.

use std::io::Write;
use std::process::{Command, Stdio};

/// 성공하면 true. 어떤 경로로도 패닉하지 않는다.
pub fn copy(text: &str) -> bool {
    const CANDIDATES: &[(&str, &[&str])] = &[
        ("pbcopy", &[]),
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
    ];
    copy_with(CANDIDATES, text)
}

/// `copy`의 실제 로직. 후보 목록을 인자로 받아 테스트가 실제 pbcopy/wl-copy/xclip
/// 대신 가짜 스크립트를 절대경로로 주입할 수 있게 한다 - 그래야 `cargo test`가
/// 호스트의 진짜 클립보드를 절대 건드리지 않는다.
fn copy_with(candidates: &[(&str, &[&str])], text: &str) -> bool {
    candidates
        .iter()
        .any(|(cmd, args)| try_copy(cmd, args, text))
}

fn try_copy(cmd: &str, args: &[&str], text: &str) -> bool {
    let Ok(mut child) = Command::new(cmd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let Some(mut stdin) = child.stdin.take() else {
        return false;
    };
    if stdin.write_all(text.as_bytes()).is_err() {
        return false;
    }
    drop(stdin);
    child.wait().map(|s| s.success()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    /// stdin을 그대로 `<dir>/captured-<name>`에 받아 적고 종료하는 가짜 클립보드
    /// 도구를 만들어 절대경로 문자열로 돌려준다. `copy_with`에 이 절대경로를 직접
    /// 넘기므로 PATH 조회를 타지 않는다 - 실행 파일 이름은 pbcopy 등과 무관해도
    /// 되고, 진짜 호스트 클립보드 도구는 이 테스트에서 한 번도 실행되지 않는다.
    fn fake_tool(dir: &std::path::Path, name: &str, succeed: bool) -> String {
        let path = dir.join(name);
        let script = if succeed {
            format!("#!/bin/sh\ncat > \"$(dirname \"$0\")/captured-{name}\"\n")
        } else {
            "#!/bin/sh\nexit 1\n".to_string()
        };
        fs::write(&path, script).expect("write fake tool");
        let mut perms = fs::metadata(&path).expect("stat fake tool").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).expect("chmod fake tool");
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn nonexistent_binary_fails_closed_without_panicking() {
        // 존재할 수 없는 이름을 직접 넣어 spawn 실패 경로를 확인한다.
        assert!(!try_copy(
            "tracon-clipboard-binary-that-does-not-exist",
            &[],
            "test"
        ));
    }

    #[test]
    fn first_successful_candidate_wins_and_later_ones_are_not_tried() {
        let dir = tempfile::tempdir().expect("dir");
        let first = fake_tool(dir.path(), "first", true);
        let second = fake_tool(dir.path(), "second", true);
        let candidates: [(&str, &[&str]); 2] = [(&first, &[]), (&second, &[])];

        assert!(copy_with(&candidates, "hello"));
        assert_eq!(
            fs::read_to_string(dir.path().join("captured-first")).expect("read captured"),
            "hello"
        );
        assert!(
            !dir.path().join("captured-second").exists(),
            "fallback must stop at the first success"
        );
    }

    #[test]
    fn a_failing_candidate_falls_through_to_the_next_one() {
        let dir = tempfile::tempdir().expect("dir");
        let broken = fake_tool(dir.path(), "broken", false);
        let ok = fake_tool(dir.path(), "ok", true);
        let candidates: [(&str, &[&str]); 2] = [(&broken, &[]), (&ok, &[])];

        assert!(copy_with(&candidates, "hi"));
        assert!(dir.path().join("captured-ok").exists());
    }

    #[test]
    fn all_candidates_missing_returns_false_without_panicking() {
        let candidates: [(&str, &[&str]); 3] = [
            ("tracon-clipboard-binary-that-does-not-exist-1", &[]),
            ("tracon-clipboard-binary-that-does-not-exist-2", &[]),
            ("tracon-clipboard-binary-that-does-not-exist-3", &[]),
        ];
        assert!(!copy_with(&candidates, "test"));
    }
}
