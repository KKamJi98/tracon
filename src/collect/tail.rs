use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// 한 파일에 대해 다음에 어디서부터 읽어야 하는지.
#[derive(Debug, Clone, Copy)]
struct Cursor {
    /// 다음 읽기의 시작 바이트. 항상 "완결된 줄의 경계"이거나, `mid_line`이
    /// true면 "버리는 중인 줄의 한복판"이다.
    offset: u64,
    /// `offset`이 줄 경계가 아님을 뜻한다. 최초 읽기는 파일 끝에서 TAIL_BYTES만
    /// 거슬러 올라가므로 첫 줄은 앞이 잘려 있고, 그 줄은 복구할 수 없어 버린다.
    mid_line: bool,
}

#[derive(Default)]
pub struct Tailer {
    cursors: HashMap<PathBuf, Cursor>,
}

impl Tailer {
    /// 최초 읽기에서 파일 끝에서 읽어들일 최대 바이트.
    pub const TAIL_BYTES: usize = 64 * 1024;

    pub fn new() -> Self {
        Self::default()
    }

    /// 지난 호출 이후 늘어난 부분 중 **완결된 줄만** 돌려준다. 최초 호출은 끝에서
    /// TAIL_BYTES까지만 읽는다.
    ///
    /// 완결되지 않은 꼬리(마지막 개행 뒤에 남은 바이트)는 넘기지 않고 offset을 그
    /// 줄의 시작으로만 옮긴다. 다음 호출에서 그 줄이 완결되면 통째로 한 번 넘어간다.
    /// 파일 크기는 줄 경계와 무관하므로 - 실측 transcript에는 131KB짜리 단일 라인이
    /// 있어 write 하나에 담기지 않는다 - 크기에서 그냥 자르면 찢어진 줄이 생기고,
    /// 소비자의 `lines()`가 그 줄을 통째로 버린다. 버려진 줄이 tool_result면
    /// 미완결 tool_use 집합이 영영 비지 않아 세션이 Unknown에 갇힌다.
    ///
    /// 개행 경계에서만 자르므로 멀티바이트 문자가 잘릴 일도 없다.
    pub fn read_new(&mut self, path: &Path) -> std::io::Result<String> {
        let mut file = std::fs::File::open(path)?;
        let len = file.metadata()?.len();

        let mut cursor = match self.cursors.get(path).copied() {
            Some(c) if c.offset <= len => c,
            // 처음 보는 파일이거나 파일이 줄어들었다(로테이션/재생성). 끝에서
            // TAIL_BYTES만 거슬러 올라가고, 그 지점이 파일 처음이 아니면 첫 줄은
            // 앞이 잘린 것으로 본다.
            _ => {
                let start = len.saturating_sub(Self::TAIL_BYTES as u64);
                Cursor {
                    offset: start,
                    mid_line: start > 0,
                }
            }
        };

        if cursor.offset >= len {
            self.cursors.insert(path.to_path_buf(), cursor);
            return Ok(String::new());
        }

        file.seek(SeekFrom::Start(cursor.offset))?;
        let mut buf = Vec::with_capacity((len - cursor.offset) as usize);
        file.take(len - cursor.offset).read_to_end(&mut buf)?;

        // 앞이 잘린 줄은 첫 개행까지 버린다. 이번 조각에 개행이 하나도 없으면 그
        // 줄이 아직 이어지는 중이므로 다음 호출에서도 계속 버린다.
        let body_start = if cursor.mid_line {
            match buf.iter().position(|b| *b == b'\n') {
                Some(i) => {
                    cursor.mid_line = false;
                    i + 1
                }
                None => buf.len(),
            }
        } else {
            0
        };

        // 마지막 개행 뒤는 아직 완결되지 않은 줄이다. 넘기지 않고, 다음 호출이
        // 그 줄을 처음부터 다시 읽도록 offset을 그 시작으로만 옮긴다.
        let complete_end = match buf[body_start..].iter().rposition(|b| *b == b'\n') {
            Some(i) => body_start + i + 1,
            None => body_start,
        };

        cursor.offset += complete_end as u64;
        self.cursors.insert(path.to_path_buf(), cursor);
        Ok(String::from_utf8_lossy(&buf[body_start..complete_end]).into_owned())
    }

    /// `live`에 없는 경로의 offset을 잊는다. 더 이상 어떤 프로세스도 가리키지 않는
    /// transcript를 위해 오프셋을 무한정 들고 있지 않기 위함이다 - 장시간 폴링하는
    /// TUI에서 누적되는 세션마다 이 맵이 하나씩 늘어나면 메모리 누수가 된다.
    pub fn retain(&mut self, live: &HashSet<PathBuf>) {
        self.cursors.retain(|path, _| live.contains(path));
    }

    #[cfg(test)]
    pub(crate) fn tracked_count(&self) -> usize {
        self.cursors.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(path: &std::path::Path, s: &str) {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("open");
        f.write_all(s.as_bytes()).expect("write");
    }

    #[test]
    fn first_read_returns_at_most_tail_bytes() {
        let dir = tempfile::tempdir().expect("dir");
        let p = dir.path().join("big.jsonl");
        let line = format!("{}\n", "x".repeat(999));
        for _ in 0..200 {
            write(&p, &line);
        }
        let mut t = Tailer::new();
        let out = t.read_new(&p).expect("read");
        assert!(out.len() <= Tailer::TAIL_BYTES);
        assert!(!out.is_empty());
    }

    #[test]
    fn second_read_returns_only_the_delta() {
        let dir = tempfile::tempdir().expect("dir");
        let p = dir.path().join("a.jsonl");
        write(&p, "line1\n");
        let mut t = Tailer::new();
        let _ = t.read_new(&p).expect("first");
        write(&p, "line2\n");
        let out = t.read_new(&p).expect("second");
        assert_eq!(out, "line2\n");
    }

    #[test]
    fn no_change_returns_empty() {
        let dir = tempfile::tempdir().expect("dir");
        let p = dir.path().join("b.jsonl");
        write(&p, "line1\n");
        let mut t = Tailer::new();
        let _ = t.read_new(&p).expect("first");
        assert_eq!(t.read_new(&p).expect("second"), "");
    }

    #[test]
    fn shrunk_file_resets_offset() {
        let dir = tempfile::tempdir().expect("dir");
        let p = dir.path().join("c.jsonl");
        write(&p, "aaaa\nbbbb\n");
        let mut t = Tailer::new();
        let _ = t.read_new(&p).expect("first");
        std::fs::write(&p, "z\n").expect("truncate");
        assert_eq!(t.read_new(&p).expect("after"), "z\n");
    }

    #[test]
    fn missing_file_is_an_error_not_a_panic() {
        let mut t = Tailer::new();
        assert!(t
            .read_new(std::path::Path::new("/nonexistent/x.jsonl"))
            .is_err());
    }

    /// 한 줄이 두 번의 write에 걸쳐 도착하면(실측 transcript에는 131KB짜리 단일
    /// 라인이 있어 write syscall 하나에 담기지 않는다) 잘린 앞부분을 그대로 넘겨서는
    /// 안 된다. 소비자(`TranscriptTracker::apply`)는 `lines()`로 자르므로 잘린 줄을
    /// 그냥 버리고, 그 줄이 tool_result면 pending 집합이 영원히 비지 않는다.
    #[test]
    fn a_line_torn_across_two_reads_is_delivered_once_and_intact() {
        let dir = tempfile::tempdir().expect("dir");
        let p = dir.path().join("torn.jsonl");
        write(&p, "seed\n");
        let mut t = Tailer::new();
        assert_eq!(t.read_new(&p).expect("first"), "seed\n");

        let long = "y".repeat(200_000);
        write(&p, &long[..120_000]);
        assert_eq!(
            t.read_new(&p).expect("second"),
            "",
            "완결되지 않은 줄은 넘기지 않는다"
        );

        write(&p, &format!("{}\n", &long[120_000..]));
        assert_eq!(
            t.read_new(&p).expect("third"),
            format!("{long}\n"),
            "줄이 완결되면 온전히 한 번만 넘어온다"
        );

        assert_eq!(t.read_new(&p).expect("fourth"), "");
    }

    #[test]
    fn retain_drops_offsets_for_paths_not_in_the_live_set() {
        let dir = tempfile::tempdir().expect("dir");
        let a = dir.path().join("a.jsonl");
        let b = dir.path().join("b.jsonl");
        write(&a, "line1\n");
        write(&b, "line1\n");
        let mut t = Tailer::new();
        let _ = t.read_new(&a).expect("read a");
        let _ = t.read_new(&b).expect("read b");
        assert_eq!(t.tracked_count(), 2);

        let live: std::collections::HashSet<std::path::PathBuf> = [a.clone()].into();
        t.retain(&live);

        assert_eq!(t.tracked_count(), 1);
    }
}
