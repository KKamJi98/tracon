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
    // transcript 경로별 누적 파서 상태. Tailer가 델타만 돌려주므로, 새로 읽을 바이트가
    // 없는 tick에도 세션의 진짜 상태와 마지막 변경 시각을 잃지 않으려면 세션마다
    // 하나씩 들고 있어야 한다.
    trackers: std::collections::HashMap<PathBuf, transcript::TranscriptTracker>,
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

/// session_id를 모르는 프로세스를 위해 cwd 디렉터리의 jsonl 후보를 최신 mtime 순으로 나열한다.
/// 같은 cwd에 세션이 여러 개면 이미 배정된 파일을 걸러내고 남은 것 중에서 고르게 하기 위함이다.
fn transcripts_for_cwd(
    projects_root: &Path,
    cwd: &Path,
) -> Vec<(String, PathBuf, std::time::SystemTime)> {
    let dir = projects_root.join(slug_for_cwd(cwd));
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut items: Vec<(String, PathBuf, std::time::SystemTime)> = entries
        .filter_map(|e| e.ok())
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().map(|e| e != "jsonl").unwrap_or(true) {
                return None;
            }
            let mtime = entry.metadata().and_then(|m| m.modified()).ok()?;
            let stem = path.file_stem()?.to_string_lossy().into_owned();
            Some((stem, path, mtime))
        })
        .collect();
    items.sort_by_key(|item| std::cmp::Reverse(item.2));
    items
}

/// 같은 세션을 가리키는 여러 프로세스(부모/워커) 관측 중 cpu가 더 높은 쪽을 대표로 남긴다.
fn merge_proc(
    by_key: &mut std::collections::HashMap<SessionKey, (ProcInfo, Option<PathBuf>)>,
    key: SessionKey,
    p: ProcInfo,
    path: Option<PathBuf>,
) {
    use std::collections::hash_map::Entry;
    match by_key.entry(key) {
        Entry::Occupied(mut e) => {
            if p.cpu > e.get().0.cpu {
                e.insert((p, path));
            }
        }
        Entry::Vacant(e) => {
            e.insert((p, path));
        }
    }
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
            trackers: std::collections::HashMap::new(),
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

        // transcript 배정을 프로세스 순회보다 먼저 전부 끝낸다. session_id가 같은
        // 프로세스(부모/워커)는 하나의 세션으로 합치고 - Tailer가 같은 파일을 두 번
        // 읽으면 두 번째 호출은 offset이 이미 끝까지 이동해 빈 조각을 받으므로 - session_id를
        // 모르는 프로세스는 같은 cwd 안에서도 서로 다른 transcript를 하나씩 가져가게 해
        // 세션이 뭉개지지 않게 한다.
        let mut claimed: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        let mut by_key: std::collections::HashMap<SessionKey, (ProcInfo, Option<PathBuf>)> =
            std::collections::HashMap::new();

        // 1단계: session_id가 명시된 프로세스는 자기 transcript를 그대로 차지한다.
        // 이 배정은 2단계보다 먼저 끝나므로 live 벡터 안에서의 순서와 무관하게 우선한다.
        for p in &live {
            let (Some(cwd), Some(id)) = (p.cwd.as_ref(), p.session_id.as_ref()) else {
                continue;
            };
            let path = transcript_path_for(&self.projects_root, cwd, id);
            claimed.insert(path.clone());
            let key = SessionKey {
                provider: p.provider,
                uuid: id.clone(),
            };
            merge_proc(&mut by_key, key, p.clone(), Some(path));
        }

        // 2단계: session_id를 모르는 프로세스는 같은 cwd에서 아직 배정되지 않은
        // transcript 중 가장 최근 것을 하나씩 가져간다. 남은 transcript가 없어도
        // 세션을 숨기지 않고 Unknown으로 띄운다. pid로 먼저 정렬해 배정이 live 벡터의
        // 원래 순서(sysinfo의 HashMap 순회 등, 보장되지 않는다)에 좌우되지 않게 한다.
        let mut unclaimed_procs: Vec<&ProcInfo> =
            live.iter().filter(|p| p.session_id.is_none()).collect();
        unclaimed_procs.sort_by_key(|p| p.pid);
        for p in unclaimed_procs {
            let Some(cwd) = p.cwd.as_ref() else {
                continue;
            };
            let pick = transcripts_for_cwd(&self.projects_root, cwd)
                .into_iter()
                .find(|(_, path, _)| !claimed.contains(path));

            let (key, path) = match pick {
                Some((uuid, path, _)) => {
                    claimed.insert(path.clone());
                    (
                        SessionKey {
                            provider: p.provider,
                            uuid,
                        },
                        Some(path),
                    )
                }
                None => (
                    SessionKey {
                        provider: p.provider,
                        uuid: format!("no-transcript-{}", p.pid),
                    },
                    None,
                ),
            };
            merge_proc(&mut by_key, key, p.clone(), path);
        }

        let mut sessions = Vec::new();
        for (key, (p, path)) in &by_key {
            let summary = match path {
                Some(path) => {
                    let chunk = self.tailer.read_new(path).unwrap_or_default();
                    let tracker = self.trackers.entry(path.clone()).or_default();
                    tracker.apply(&chunk);
                    tracker.summary()
                }
                None => None,
            };

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

    fn proc_with(pid: i32, uuid: Option<&str>, cwd: &str, cpu: f32) -> ProcInfo {
        ProcInfo {
            pid,
            provider: Provider::Claude,
            session_id: uuid.map(|u| u.to_string()),
            cwd: Some(PathBuf::from(cwd)),
            cpu,
            started_at_ms: NOW - 3_600_000,
            tty: None,
        }
    }

    fn proc(uuid: Option<&str>, cwd: &str, cpu: f32) -> ProcInfo {
        proc_with(100, uuid, cwd, cpu)
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

    #[test]
    fn processes_without_session_id_in_same_cwd_get_distinct_sessions() {
        let dir = tempfile::tempdir().expect("dir");
        let proj = dir.path().join("-home-dev-app");
        std::fs::create_dir_all(&proj).expect("mkdir");
        std::fs::write(proj.join("aaa.jsonl"), "").expect("write aaa");
        std::fs::write(proj.join("bbb.jsonl"), "").expect("write bbb");

        let mut c = super::Collector::new(
            Box::new(FakeProcs(vec![
                proc_with(200, None, "/home/dev/app", 0.0),
                proc_with(201, None, "/home/dev/app", 0.0),
            ])),
            Thresholds::default(),
        )
        .with_projects_root(dir.path().to_path_buf());
        let snap = c.snapshot(NOW);

        assert_eq!(snap.sessions.len(), 2);
        assert_ne!(snap.sessions[0].key, snap.sessions[1].key);
    }

    #[test]
    fn explicit_session_id_keeps_its_transcript_regardless_of_process_order() {
        let dir = tempfile::tempdir().expect("dir");
        let proj = dir.path().join("-home-dev-app");
        std::fs::create_dir_all(&proj).expect("mkdir");
        std::fs::write(proj.join("zzz.jsonl"), "").expect("write zzz");
        // u1.jsonl gets a strictly later mtime than zzz.jsonl, so it is the transcript
        // a naive "newest wins" fallback would hand to whichever process asks first.
        std::thread::sleep(std::time::Duration::from_millis(10));
        let line = r#"{"type":"assistant","timestamp":"2027-01-15T00:00:00.000Z","cwd":"/home/dev/app","isSidechain":false,"message":{"model":"claude-opus-5","content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#;
        std::fs::write(proj.join("u1.jsonl"), format!("{line}\n")).expect("write u1");

        // The session_id-less process is listed FIRST in the process list. If pass
        // ordering mattered, it would grab u1.jsonl (the newest file) before the
        // explicit-id process ever got a chance to claim it.
        let mut c = super::Collector::new(
            Box::new(FakeProcs(vec![
                proc_with(300, None, "/home/dev/app", 0.0),
                proc_with(301, Some("u1"), "/home/dev/app", 0.0),
            ])),
            Thresholds::default(),
        )
        .with_projects_root(dir.path().to_path_buf());
        let snap = c.snapshot(NOW);

        assert_eq!(snap.sessions.len(), 2);
        let explicit = snap
            .sessions
            .iter()
            .find(|s| s.key.uuid == "u1")
            .expect("explicit session present");
        assert_eq!(explicit.pid, Some(301));

        let other = snap
            .sessions
            .iter()
            .find(|s| s.key.uuid != "u1")
            .expect("second session present");
        assert_eq!(other.pid, Some(300));
        assert_eq!(other.key.uuid, "zzz");
    }

    #[test]
    fn process_without_any_unclaimed_transcript_still_appears_as_unknown() {
        let dir = tempfile::tempdir().expect("dir");
        let mut c = super::Collector::new(
            Box::new(FakeProcs(vec![proc_with(400, None, "/home/dev/app", 0.0)])),
            Thresholds::default(),
        )
        .with_projects_root(dir.path().to_path_buf());
        let snap = c.snapshot(NOW);

        assert_eq!(snap.sessions.len(), 1);
        assert_eq!(snap.sessions[0].state, State::Unknown);
        assert_eq!(snap.sessions[0].pid, Some(400));
    }

    #[test]
    fn snapshot_state_is_stable_across_repeated_polls_with_no_new_bytes() {
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

        let first = c.snapshot(ts + 5_000);
        // No bytes were appended between polls - Tailer hands back an empty delta on
        // this second call. Without the per-transcript tracker this used to reset the
        // session to Unknown and stamp last_change_ms with the current clock.
        let second = c.snapshot(ts + 10_000);

        assert_eq!(first.sessions[0].state, State::WaitingInput);
        assert_eq!(second.sessions[0].state, first.sessions[0].state);
        assert_eq!(
            second.sessions[0].last_change_ms,
            first.sessions[0].last_change_ms
        );
    }

    #[test]
    fn pending_tool_use_resolves_once_the_matching_tool_result_arrives_in_a_later_poll() {
        let dir = tempfile::tempdir().expect("dir");
        let proj = dir.path().join("-home-dev-app");
        std::fs::create_dir_all(&proj).expect("mkdir");

        let tool_use = r#"{"type":"assistant","timestamp":"2027-01-15T00:00:00.000Z","cwd":"/home/dev/app","isSidechain":false,"message":{"model":"claude-opus-5","content":[{"type":"tool_use","id":"t1","name":"Bash"}]}}"#;
        std::fs::write(proj.join("u1.jsonl"), format!("{tool_use}\n")).expect("write first chunk");
        let t1_ms = crate::collect::transcript::parse_tail(tool_use)
            .expect("tail")
            .last_ts_ms;

        let mut c = super::Collector::new(
            Box::new(FakeProcs(vec![proc(Some("u1"), "/home/dev/app", 0.0)])),
            Thresholds::default(),
        )
        .with_projects_root(dir.path().to_path_buf());

        // 40s later, still no cpu and an unmatched tool_use - infer() suspects the
        // session is stuck waiting on an approval prompt.
        let first = c.snapshot(t1_ms + 40_000);
        assert_eq!(first.sessions[0].state, State::WaitingApproval);

        // The matching tool_result arrives in a later delta, appended to the same file.
        let tool_result = r#"{"type":"user","timestamp":"2027-01-15T00:00:50.000Z","isSidechain":false,"message":{"content":[{"type":"tool_result","tool_use_id":"t1"}]}}"#;
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(proj.join("u1.jsonl"))
                .expect("open for append");
            writeln!(f, "{tool_result}").expect("append tool_result");
        }
        let t2_ms =
            crate::collect::transcript::parse_ts_ms("2027-01-15T00:00:50.000Z").expect("parse ts");

        let second = c.snapshot(t2_ms + 5_000);
        // pending dropped to zero across the chunk boundary - the session reads as
        // running again instead of still being stuck on the resolved tool_use.
        assert_ne!(second.sessions[0].state, State::WaitingApproval);
        assert_eq!(second.sessions[0].state, State::RunningInference);
    }
}
