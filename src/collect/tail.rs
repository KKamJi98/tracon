use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct Tailer {
    offsets: HashMap<PathBuf, u64>,
}

impl Tailer {
    /// 최초 읽기에서 파일 끝에서 읽어들일 최대 바이트.
    pub const TAIL_BYTES: usize = 64 * 1024;

    pub fn new() -> Self {
        Self::default()
    }

    /// 지난 호출 이후 늘어난 부분만 읽는다. 최초 호출은 끝에서 TAIL_BYTES까지만 읽는다.
    pub fn read_new(&mut self, path: &Path) -> std::io::Result<String> {
        let mut file = std::fs::File::open(path)?;
        let len = file.metadata()?.len();
        let prev = self.offsets.get(path).copied();

        let start = match prev {
            Some(off) if off <= len => off,
            Some(_) => len.saturating_sub(Self::TAIL_BYTES as u64),
            None => len.saturating_sub(Self::TAIL_BYTES as u64),
        };

        self.offsets.insert(path.to_path_buf(), len);
        if start >= len {
            return Ok(String::new());
        }

        file.seek(SeekFrom::Start(start))?;
        let mut buf = Vec::with_capacity((len - start) as usize);
        file.take(len - start).read_to_end(&mut buf)?;
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }

    /// `live`에 없는 경로의 offset을 잊는다. 더 이상 어떤 프로세스도 가리키지 않는
    /// transcript를 위해 오프셋을 무한정 들고 있지 않기 위함이다 - 장시간 폴링하는
    /// TUI에서 누적되는 세션마다 이 맵이 하나씩 늘어나면 메모리 누수가 된다.
    pub fn retain(&mut self, live: &HashSet<PathBuf>) {
        self.offsets.retain(|path, _| live.contains(path));
    }

    #[cfg(test)]
    pub(crate) fn tracked_count(&self) -> usize {
        self.offsets.len()
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
