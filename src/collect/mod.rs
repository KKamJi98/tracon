pub mod hooksink;
pub mod layer0;
pub mod proc;
pub mod tail;
pub mod transcript;

use crate::collect::proc::{ProcInfo, ProcessSource};
use crate::config::Thresholds;
use crate::json::{sort_sessions, Snapshot};
use crate::merge::winner;
use crate::model::{Observation, Session, SessionKey, Source};
use std::path::{Path, PathBuf};

pub struct Collector {
    procs: Box<dyn ProcessSource>,
    cfg: Thresholds,
    tailer: tail::Tailer,
    projects_root: PathBuf,
    sink_dir: PathBuf,
}

/// `/home/dev/code/app` -> `-home-dev-code-app`
pub fn slug_for_cwd(cwd: &Path) -> String {
    cwd.to_string_lossy().replace(['/', '.', '_'], "-")
}

/// cwd 슬러그 디렉터리에서 uuid에 해당하는 transcript 경로를 만든다.
pub fn transcript_path_for(projects_root: &Path, cwd: &Path, uuid: &str) -> PathBuf {
    projects_root
        .join(slug_for_cwd(cwd))
        .join(format!("{uuid}.jsonl"))
}

/// session_id를 모르는 프로세스를 위해 cwd 디렉터리에서 가장 최근 jsonl을 고른다.
fn newest_transcript(projects_root: &Path, cwd: &Path) -> Option<(String, PathBuf)> {
    let dir = projects_root.join(slug_for_cwd(cwd));
    let mut best: Option<(std::time::SystemTime, String, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()?.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().map(|e| e != "jsonl").unwrap_or(true) {
            continue;
        }
        let Ok(mtime) = entry.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        let stem = path.file_stem()?.to_string_lossy().into_owned();
        if best.as_ref().map(|(t, _, _)| mtime > *t).unwrap_or(true) {
            best = Some((mtime, stem, path));
        }
    }
    best.map(|(_, id, p)| (id, p))
}

impl Collector {
    pub fn new(procs: Box<dyn ProcessSource>, cfg: Thresholds) -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        Self {
            procs,
            cfg,
            tailer: tail::Tailer::new(),
            projects_root: home.join(".claude/projects"),
            sink_dir: hooksink::sink_dir(),
        }
    }

    #[allow(dead_code)]
    pub fn with_projects_root(mut self, root: PathBuf) -> Self {
        self.projects_root = root;
        self
    }

    #[allow(dead_code)]
    pub fn with_sink_dir(mut self, dir: PathBuf) -> Self {
        self.sink_dir = dir;
        self
    }

    pub fn snapshot(&mut self, now_ms: i64) -> Snapshot {
        let live: Vec<ProcInfo> = self.procs.list_agents();
        let sink_records = hooksink::read_all(&self.sink_dir);
        let hooks_installed = !sink_records.is_empty();

        // 같은 session_id를 여러 프로세스(부모/워커)가 공유할 수 있다. transcript 경로
        // 기준으로 세션당 하나만 남긴다 - Tailer가 같은 파일을 두 번 읽으면 두 번째
        // 호출은 offset이 이미 끝까지 이동해 빈 조각을 받는다.
        let mut by_key: std::collections::HashMap<SessionKey, (ProcInfo, PathBuf)> =
            std::collections::HashMap::new();
        for p in &live {
            let Some((uuid, path)) = self.resolve_transcript(p) else {
                continue;
            };
            let key = SessionKey {
                provider: p.provider,
                uuid,
            };
            by_key
                .entry(key)
                .and_modify(|(cur, _)| {
                    if p.cpu > cur.cpu {
                        *cur = p.clone();
                    }
                })
                .or_insert_with(|| (p.clone(), path));
        }

        let mut sessions = Vec::new();
        for (key, (p, path)) in &by_key {
            let chunk = self.tailer.read_new(path).unwrap_or_default();
            let summary = transcript::parse_tail(&chunk);

            let (state0, conf0) = layer0::infer(summary.as_ref(), true, p.cpu, now_ms, &self.cfg);
            let mut observations = vec![Observation {
                key: key.clone(),
                state: state0,
                source: Source::Layer0Inferred,
                confidence: conf0,
                observed_at: now_ms,
            }];
            observations.extend(
                sink_records
                    .iter()
                    .filter(|r| &r.key == key)
                    .filter_map(|r| r.to_observation()),
            );

            let Some(win) = winner(&observations) else {
                continue;
            };
            let last_change = summary.as_ref().map(|s| s.last_ts_ms).unwrap_or(now_ms);
            let ctx_tokens = summary.as_ref().and_then(|s| s.usage.map(|u| u.total()));
            let model = summary.as_ref().and_then(|s| s.model.clone());

            sessions.push(Session {
                key: key.clone(),
                state: crate::model::demote(win.state, now_ms - last_change, &self.cfg),
                source: win.source,
                confidence: win.confidence,
                last_change_ms: last_change,
                started_at_ms: Some(p.started_at_ms),
                cwd: p.cwd.as_ref().map(|c| c.to_string_lossy().into_owned()),
                ctx_window: ctx_tokens
                    .map(|t| transcript::window_for(model.as_deref().unwrap_or(""), t)),
                ctx_tokens,
                model,
                cpu: Some(p.cpu),
                pid: Some(p.pid),
                jump: None,
            });
        }

        sort_sessions(&mut sessions);
        Snapshot {
            sessions,
            hooks_installed,
            cmux_linked: false,
            generated_at_ms: now_ms,
        }
    }

    fn resolve_transcript(&self, p: &ProcInfo) -> Option<(String, PathBuf)> {
        let cwd = p.cwd.as_ref()?;
        if let Some(id) = &p.session_id {
            return Some((
                id.clone(),
                transcript_path_for(&self.projects_root, cwd, id),
            ));
        }
        newest_transcript(&self.projects_root, cwd)
    }
}

#[cfg(test)]
mod tests {
    use crate::collect::proc::{ProcInfo, ProcessSource};
    use crate::config::Thresholds;
    use crate::model::{Provider, State};
    use std::path::PathBuf;

    struct FakeProcs(Vec<ProcInfo>);
    impl ProcessSource for FakeProcs {
        fn list_agents(&mut self) -> Vec<ProcInfo> {
            self.0.clone()
        }
    }

    const NOW: i64 = 1_800_000_000_000;

    fn proc(uuid: Option<&str>, cwd: &str, cpu: f32) -> ProcInfo {
        ProcInfo {
            pid: 100,
            provider: Provider::Claude,
            session_id: uuid.map(|u| u.to_string()),
            cwd: Some(PathBuf::from(cwd)),
            cpu,
            started_at_ms: NOW - 3_600_000,
            tty: None,
        }
    }

    #[test]
    fn slug_matches_claude_projects_convention() {
        assert_eq!(
            super::slug_for_cwd(std::path::Path::new("/home/dev/code/app")),
            "-home-dev-code-app"
        );
    }

    #[test]
    fn session_without_transcript_is_still_listed_as_unknown() {
        let dir = tempfile::tempdir().expect("dir");
        let mut c = super::Collector::new(
            Box::new(FakeProcs(vec![proc(Some("u1"), "/home/dev/app", 0.0)])),
            Thresholds::default(),
        )
        .with_projects_root(dir.path().to_path_buf());
        let snap = c.snapshot(NOW);
        assert_eq!(snap.sessions.len(), 1);
        assert_eq!(snap.sessions[0].state, State::Unknown);
    }

    #[test]
    fn transcript_drives_state_and_context() {
        let dir = tempfile::tempdir().expect("dir");
        let proj = dir.path().join("-home-dev-app");
        std::fs::create_dir_all(&proj).expect("mkdir");
        let line = r#"{"type":"assistant","timestamp":"2027-01-15T00:00:00.000Z","cwd":"/home/dev/app","isSidechain":false,"message":{"model":"claude-opus-5","content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":0,"cache_read_input_tokens":100000,"cache_creation_input_tokens":0}}}"#;
        std::fs::write(proj.join("u1.jsonl"), format!("{line}\n")).expect("write");

        let ts = crate::collect::transcript::parse_tail(line)
            .expect("tail")
            .last_ts_ms;
        let mut c = super::Collector::new(
            Box::new(FakeProcs(vec![proc(Some("u1"), "/home/dev/app", 0.0)])),
            Thresholds::default(),
        )
        .with_projects_root(dir.path().to_path_buf());
        let snap = c.snapshot(ts + 5_000);

        assert_eq!(snap.sessions[0].state, State::WaitingInput);
        assert_eq!(snap.sessions[0].ctx_tokens, Some(100_000));
        assert_eq!(snap.sessions[0].ctx_window, Some(200_000));
        assert_eq!(snap.sessions[0].ctx_pct(), Some(50));
    }

    #[test]
    fn hook_record_overrides_inference() {
        let dir = tempfile::tempdir().expect("dir");
        let sink = dir.path().join("sink");
        crate::collect::hooksink::record_event(
            &sink,
            &crate::collect::hooksink::SinkRecord {
                key: crate::model::SessionKey {
                    provider: Provider::Claude,
                    uuid: "u1".into(),
                },
                event: crate::model::HookEvent::PermissionRequest,
                occurred_at: NOW - 1_000,
                cwd: None,
                pid: None,
            },
        )
        .expect("record");

        let mut c = super::Collector::new(
            Box::new(FakeProcs(vec![proc(Some("u1"), "/home/dev/app", 0.0)])),
            Thresholds::default(),
        )
        .with_projects_root(dir.path().join("projects"))
        .with_sink_dir(sink);
        let snap = c.snapshot(NOW);

        assert_eq!(snap.sessions[0].state, State::WaitingApproval);
        assert_eq!(snap.sessions[0].source, crate::model::Source::Layer1Hook);
        assert!(snap.hooks_installed);
    }

    #[test]
    fn dead_sessions_from_sink_are_dropped_when_process_is_gone() {
        let dir = tempfile::tempdir().expect("dir");
        let sink = dir.path().join("sink");
        crate::collect::hooksink::record_event(
            &sink,
            &crate::collect::hooksink::SinkRecord {
                key: crate::model::SessionKey {
                    provider: Provider::Claude,
                    uuid: "ghost".into(),
                },
                event: crate::model::HookEvent::SessionEnd,
                occurred_at: NOW - 1_000,
                cwd: None,
                pid: None,
            },
        )
        .expect("record");

        let mut c = super::Collector::new(Box::new(FakeProcs(vec![])), Thresholds::default())
            .with_projects_root(dir.path().join("projects"))
            .with_sink_dir(sink);
        let snap = c.snapshot(NOW);
        assert!(snap.sessions.is_empty());
    }
}
