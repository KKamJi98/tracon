#![allow(dead_code)]

use std::collections::HashMap;
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
}
