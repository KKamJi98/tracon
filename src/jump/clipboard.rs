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
    CANDIDATES
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

    #[test]
    fn nonexistent_binary_fails_closed_without_panicking() {
        // 존재할 수 없는 이름을 직접 넣어 spawn 실패 경로를 확인한다. 전역 PATH를
        // 건드리지 않으므로 병렬로 도는 다른 테스트에 영향이 없다.
        assert!(!try_copy(
            "tracon-clipboard-binary-that-does-not-exist",
            &[],
            "test"
        ));
    }

    #[test]
    fn copy_never_panics_regardless_of_host_clipboard_availability() {
        // 이 머신에 pbcopy가 실제로 있을 수도 없을 수도 있다 - 반환값을 단정하지
        // 않고, 어느 쪽이든 패닉 없이 끝나는지만 본다.
        let _ = copy("tracon clipboard smoke test");
    }
}
