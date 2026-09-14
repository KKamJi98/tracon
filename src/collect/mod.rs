pub mod cmux;
pub mod codex;
pub mod hooksink;
pub mod layer0;
pub mod proc;
pub mod tail;
pub mod transcript;

use crate::collect::proc::{ProcInfo, ProcessSource};
use crate::config::Thresholds;
use crate::json::{sort_sessions, Snapshot};
use crate::jump::Jumper;
use crate::merge::winner;
use crate::model::{Observation, Provider, Session, SessionKey, Source};
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
    // codex는 claude와 tracker/경로 캐시를 따로 둔다 - 스키마가 다르고, 경로도
    // cwd 슬러그가 아니라 uuid로 찾아야 해서 캐시 키 자체가 다르기 때문이다.
    codex_sessions_root: PathBuf,
    codex_trackers: std::collections::HashMap<PathBuf, codex::CodexTracker>,
    // uuid -> (찾아낸 rollout 경로, session_meta에서 읽은 cwd). 한 번 찾으면 다음
    // tick부터는 날짜 트리를 다시 훑지 않기 위한 캐시다 - `~/.codex/sessions`는
    // 수백MB까지 자랄 수 있어 매 tick 전체 스캔은 성능 예산을 깬다.
    codex_paths: std::collections::HashMap<String, (PathBuf, Option<String>)>,
    // session_id도 cwd도 안정적으로 주지 않는 codex 프로세스(예: ChatGPT 앱이 띄운
    // 프로세스)를 위한 pid -> uuid 캐시. 이게 없으면 이런 프로세스는 session_id가
    // 끝내 나타나지 않으므로 1단계 캐시(`codex_paths`만으로는 uuid를 어떤 pid에
    // 물려야 하는지 알 길이 없어) 매 tick 날짜 트리를 다시 훑게 된다.
    codex_pid_uuid: std::collections::HashMap<i32, String>,
    // `--session-id` 없이 뜬 claude 프로세스를 위한 pid -> uuid 캐시. codex 쪽과
    // 같은 이유이자 같은 모양이다 - 이게 없으면 배정 기준이 mtime뿐이라, 같은 cwd의
    // 두 세션이 번갈아 쓸 때 tick마다 배정이 맞바뀐다.
    claude_pid_uuid: std::collections::HashMap<i32, String>,
    sink_dir: PathBuf,
    // tmux list-panes 같은 비용이 드는 조회는 tick당 딱 한 번(refresh)만 하고,
    // 세션마다 하는 resolve_tty는 그 캐시를 읽는 순수 조회다.
    jumpers: Vec<Box<dyn Jumper>>,
    // 레이어 2(cmux). None이면 cmux가 없거나 아직 연결을 시도하지 않은 것 -
    // 이 경우 나머지 전부는 cmux가 존재한 적 없는 것처럼 그대로 동작해야 한다.
    cmux_subscriber: Option<cmux::CmuxSubscriber>,
    cmux_linked: bool,
    cmux_jumper: Option<crate::jump::cmux::CmuxJumper>,
    // 세션마다 마지막으로 본 cmux workspace_id. cmux 이벤트가 tick마다 오지는
    // 않으므로(trackers/tailer와 같은 이유로) 점프 대상을 잃지 않으려면 유지해야 한다.
    cmux_workspaces: std::collections::HashMap<SessionKey, String>,
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
            codex_sessions_root: codex::codex_sessions_root(),
            codex_trackers: std::collections::HashMap::new(),
            codex_paths: std::collections::HashMap::new(),
            codex_pid_uuid: std::collections::HashMap::new(),
            claude_pid_uuid: std::collections::HashMap::new(),
            sink_dir: hooksink::sink_dir(),
            jumpers: Vec::new(),
            cmux_subscriber: None,
            cmux_linked: false,
            cmux_jumper: None,
            cmux_workspaces: std::collections::HashMap::new(),
        }
    }

    #[allow(dead_code)]
    pub fn with_projects_root(mut self, root: PathBuf) -> Self {
        self.projects_root = root;
        self
    }

    #[allow(dead_code)]
    pub fn with_codex_sessions_root(mut self, root: PathBuf) -> Self {
        self.codex_sessions_root = root;
        self
    }

    /// codex rollout 경로를 uuid로 찾는다. 캐시에 있으면 그대로 돌려주고(디스크
    /// 접근 없음), 없으면 `find_codex_transcript`로 날짜 트리를 훑어 찾은 뒤
    /// 캐시에 넣는다 - 이후 tick은 다시 훑지 않는다.
    fn resolve_codex_path(&mut self, uuid: &str) -> Option<PathBuf> {
        if let Some((path, _)) = self.codex_paths.get(uuid) {
            return Some(path.clone());
        }
        let path = codex::find_codex_transcript(&self.codex_sessions_root, uuid)?;
        let cwd = codex::read_session_meta_cwd(&path);
        self.codex_paths
            .insert(uuid.to_string(), (path.clone(), cwd));
        Some(path)
    }

    /// session_id를 모르는 codex 프로세스(예: ChatGPT 앱이 띄운 codex - sysinfo가
    /// session_id도 cwd도 안정적으로 주지 않는다)가 가리키는 transcript를 찾는다.
    /// pid -> uuid를 캐시해, 한 번 찾은 뒤로는 이 프로세스가 살아있는 한 다음
    /// tick부터 날짜 트리를 다시 훑지 않는다.
    fn resolve_unclaimed_codex_path(
        &mut self,
        p: &ProcInfo,
        claimed: &std::collections::HashSet<PathBuf>,
    ) -> Option<(String, PathBuf)> {
        if let Some(uuid) = self.codex_pid_uuid.get(&p.pid) {
            if let Some((path, _)) = self.codex_paths.get(uuid) {
                return Some((uuid.clone(), path.clone()));
            }
        }
        let hit = codex::find_unclaimed_codex_transcript(
            &self.codex_sessions_root,
            p.cwd.as_deref(),
            claimed,
        )?;
        self.codex_pid_uuid.insert(p.pid, hit.uuid.clone());
        self.codex_paths
            .insert(hit.uuid.clone(), (hit.path.clone(), hit.cwd));
        Some((hit.uuid, hit.path))
    }

    #[allow(dead_code)]
    pub fn with_sink_dir(mut self, dir: PathBuf) -> Self {
        self.sink_dir = dir;
        self
    }

    /// 점프 대상을 찾아낼 소스를 등록한다. 테스트는 가짜 Jumper를 주입해 tmux를
    /// 실제로 띄우지 않고도 resolve_tty 경로를 검증할 수 있다.
    pub fn with_jumpers(mut self, jumpers: Vec<Box<dyn Jumper>>) -> Self {
        self.jumpers = jumpers;
        self
    }

    /// cmux 이벤트 구독을 연결한다. `CmuxSubscriber::spawn()`/`spawn_one_shot()`이
    /// 돌려준 값이다. `None`이면(cmux 없음) 레이어 2는 계속 비활성으로 남는다.
    /// `Collector`가 이 값을 들고 있다가 버려질 때 - 예를 들어 `--json`처럼
    /// 스냅샷 한 번을 찍고 함수가 끝날 때 - `CmuxSubscriber::drop`이 자식
    /// 프로세스를 죽인다. 호출부가 따로 기억할 필요가 없다.
    #[allow(dead_code)]
    pub fn with_cmux(mut self, subscriber: Option<cmux::CmuxSubscriber>) -> Self {
        self.cmux_linked = subscriber.is_some();
        self.cmux_subscriber = subscriber;
        self
    }

    /// workspace 점프 대상을 찾아낼 cmux 어댑터를 등록한다. `None`이면(cmux 구독이
    /// 없음) 세션은 tty 기반 Jumper로만 점프 대상을 찾는다.
    #[allow(dead_code)]
    pub fn with_cmux_jumper(mut self, jumper: Option<crate::jump::cmux::CmuxJumper>) -> Self {
        self.cmux_jumper = jumper;
        self
    }

    pub fn snapshot(&mut self, now_ms: i64) -> Snapshot {
        let live: Vec<ProcInfo> = self.procs.list_agents();
        let sink_records = hooksink::read_all(&self.sink_dir);
        let hooks_installed = !sink_records.is_empty();

        // tmux list-panes 같은 비용이 드는 조회는 세션 수와 무관하게 tick당 한 번만
        // 한다. 아래에서 세션마다 부르는 resolve_tty는 이 캐시를 읽기만 하는 순수
        // 조회라 20세션이어도 새 프로세스가 늘지 않는다.
        for jumper in &mut self.jumpers {
            jumper.refresh();
        }
        if let Some(jumper) = &mut self.cmux_jumper {
            jumper.refresh();
        }

        // 이번 tick에 새로 들어온 cmux 이벤트를 전부 비운다. `Empty`는 "당장은
        // 없음"이라 링크가 살아있다고 본다. `Disconnected`는 구독 스레드가 죽었다는
        // 뜻이라 - cmux 프로세스가 도중에 사라졌거나 소켓이 끊겼거나 - 그때부터
        // cmux 없이 동작하던 상태로 되돌아간다(터미널 중립성).
        let mut cmux_events: Vec<cmux::CmuxEvent> = Vec::new();
        let mut cmux_disconnected = false;
        if let Some(sub) = self.cmux_subscriber.as_ref() {
            loop {
                match sub.rx.try_recv() {
                    Ok(ev) => cmux_events.push(ev),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        cmux_disconnected = true;
                        break;
                    }
                }
            }
        }
        if cmux_disconnected {
            // 구독 스레드가 죽었다는 뜻이므로 자식도 이미 죽었거나 곧 죽는다 -
            // 그래도 CmuxSubscriber를 버려 Drop이 확실히 거두게 한다(이미 죽은
            // 자식을 또 죽이는 것도 안전해야 한다는 요구를 그대로 만족한다).
            self.cmux_subscriber = None;
            self.cmux_linked = false;
        }

        let mut cmux_by_key: std::collections::HashMap<SessionKey, Vec<Observation>> =
            std::collections::HashMap::new();
        for ev in &cmux_events {
            if let Some(id) = &ev.workspace_id {
                self.cmux_workspaces
                    .insert(ev.record.key.clone(), id.clone());
            }
            if let Some(obs) = ev.to_observation() {
                cmux_by_key.entry(obs.key.clone()).or_default().push(obs);
            }
        }

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
            let Some(id) = p.session_id.as_ref() else {
                continue;
            };
            let path = match p.provider {
                Provider::Claude => {
                    let Some(cwd) = p.cwd.as_ref() else {
                        continue;
                    };
                    transcript_path_for(&self.projects_root, cwd, id)
                }
                Provider::Codex => {
                    let Some(path) = self.resolve_codex_path(id) else {
                        continue;
                    };
                    path
                }
            };
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
            let (key, path) = match p.provider {
                Provider::Claude => {
                    let Some(cwd) = p.cwd.as_ref() else {
                        continue;
                    };
                    // 지난 tick에 이 pid가 가져간 transcript를 먼저 되찾는다.
                    // mtime 순서는 tick 사이에 뒤집히므로, 이 캐시가 없으면 같은
                    // cwd의 두 세션이 배정을 맞바꾼다 - 선택이 튀고, 복사되는
                    // `claude --resume <uuid>`가 엉뚱한 세션을 가리킨다.
                    let cached = self
                        .claude_pid_uuid
                        .get(&p.pid)
                        .map(|uuid| {
                            (
                                uuid.clone(),
                                transcript_path_for(&self.projects_root, cwd, uuid),
                            )
                        })
                        .filter(|(_, path)| !claimed.contains(path) && path.exists());
                    let pick = cached.or_else(|| {
                        transcripts_for_cwd(&self.projects_root, cwd)
                            .into_iter()
                            .find(|(_, path, _)| !claimed.contains(path))
                            .map(|(uuid, path, _)| (uuid, path))
                    });
                    match pick {
                        Some((uuid, path)) => {
                            claimed.insert(path.clone());
                            self.claude_pid_uuid.insert(p.pid, uuid.clone());
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
                    }
                }
                // codex는 cwd 슬러그 디렉터리가 없다. cwd를 아는 프로세스(터미널에서
                // 띄운 codex)는 session_meta.cwd가 일치하는 가장 최근 미배정 파일을,
                // cwd를 모르는 프로세스(예: ChatGPT 앱이 띄운 codex - sysinfo가 cwd를
                // 주지 않는다)는 그냥 가장 최근 미배정 파일을 받는다. `resolve_unclaimed_codex_path`가
                // pid -> uuid를 캐시해 두 번째 tick부터는 날짜 트리를 다시 훑지 않는다.
                Provider::Codex => match self.resolve_unclaimed_codex_path(p, &claimed) {
                    Some((uuid, path)) => {
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
                },
            };
            merge_proc(&mut by_key, key, p.clone(), path);
        }

        let mut sessions = Vec::new();
        for (key, (p, path)) in &by_key {
            let mut turn_ended = false;
            let mut codex_window: Option<u64> = None;
            let summary = match path {
                Some(path) => {
                    let chunk = self.tailer.read_new(path).unwrap_or_default();
                    match key.provider {
                        Provider::Claude => {
                            let tracker = self.trackers.entry(path.clone()).or_default();
                            tracker.apply(&chunk);
                            tracker.summary()
                        }
                        Provider::Codex => {
                            let tracker = self.codex_trackers.entry(path.clone()).or_default();
                            tracker.apply(&chunk);
                            turn_ended = tracker.turn_ended();
                            codex_window = tracker.context_window();
                            tracker.summary()
                        }
                    }
                }
                None => None,
            };

            let (state0, conf0) = match key.provider {
                Provider::Claude => layer0::infer(summary.as_ref(), true, p.cpu, now_ms, &self.cfg),
                // task_complete가 마지막이면 사실이지 추측이 아니다 - `layer0::infer`가
                // 텍스트/도구 사용만 보고 매기는 Medium보다 나은 Confidence::Fact를 준다.
                // 그 신호가 없을 때는 claude와 같은 휴리스틱으로 그대로 떨어진다.
                Provider::Codex => {
                    codex::infer(summary.as_ref(), turn_ended, true, p.cpu, now_ms, &self.cfg)
                }
            };
            let mut observations = vec![Observation {
                key: key.clone(),
                state: state0,
                source: Source::Layer0Inferred,
                confidence: conf0,
                observed_at: now_ms,
            }];
            // transcript가 훅 기록보다 나중에 움직였으면 그 기록은 낡은 것이다.
            // 훅 기록은 파일로 남으므로 - `hooks uninstall` 이후나 SessionEnd 없이
            // 세션이 죽은 뒤에도 - 이 검사가 없으면 살아 있는 세션이 마지막 훅
            // 상태에 영원히 고정된다.
            let transcript_ts = summary.as_ref().map(|s| s.last_ts_ms).unwrap_or(i64::MIN);
            observations.extend(
                sink_records
                    .iter()
                    .filter(|r| &r.key == key)
                    .filter(|r| r.occurred_at >= transcript_ts)
                    .filter_map(|r| r.to_observation()),
            );
            if let Some(cx) = cmux_by_key.get(key) {
                observations.extend(cx.iter().cloned());
            }

            let Some(win) = winner(&observations) else {
                continue;
            };
            let last_change = summary.as_ref().map(|s| s.last_ts_ms).unwrap_or(now_ms);
            let ctx_tokens = summary.as_ref().and_then(|s| s.usage.map(|u| u.total()));
            let model = summary.as_ref().and_then(|s| s.model.clone());
            let ctx_window = match key.provider {
                // claude는 모델이 윈도우 크기를 밝히지 않아 관측값으로 짐작한다.
                Provider::Claude => {
                    ctx_tokens.map(|t| transcript::window_for(model.as_deref().unwrap_or(""), t))
                }
                // codex는 token_count 이벤트가 윈도우를 직접 알려주므로 짐작하지 않는다.
                Provider::Codex => codex_window,
            };
            let cwd = match key.provider {
                // codex는 session_meta.cwd(rollout 파일 자체가 기록한 값)를 우선한다 -
                // ChatGPT 앱이 띄운 codex 프로세스는 sysinfo가 cwd를 못 준다.
                Provider::Codex => self
                    .codex_paths
                    .get(&key.uuid)
                    .and_then(|(_, cwd)| cwd.clone())
                    .or_else(|| p.cwd.as_ref().map(|c| c.to_string_lossy().into_owned())),
                Provider::Claude => p.cwd.as_ref().map(|c| c.to_string_lossy().into_owned()),
            };
            // cmux가 workspace_id를 사실로 주면 그쪽을 우선한다 - tty 기반 Jumper는
            // cmux 없이도 동작해야 하는 폴백이다.
            let cmux_jump = self.cmux_workspaces.get(key).and_then(|wsid| {
                self.cmux_jumper
                    .as_ref()
                    .and_then(|jumper| jumper.resolve(wsid))
            });
            let jump = cmux_jump.or_else(|| {
                p.tty.as_deref().and_then(|tty| {
                    self.jumpers
                        .iter()
                        .find_map(|jumper| jumper.resolve_tty(tty))
                })
            });

            sessions.push(Session {
                key: key.clone(),
                state: crate::model::demote(win.state, now_ms - last_change, &self.cfg),
                source: win.source,
                confidence: win.confidence,
                last_change_ms: last_change,
                started_at_ms: Some(p.started_at_ms),
                cwd,
                ctx_window,
                ctx_tokens,
                model,
                cpu: Some(p.cpu),
                pid: Some(p.pid),
                jump: jump.map(|t| t.label()),
            });
        }

        sort_sessions(&mut sessions);

        // `claimed`는 이번 tick에 살아 있는 프로세스가 실제로 가리킨 transcript
        // 경로만 담는다. 이 tick에서 아무도 가리키지 않은 경로의 tracker/offset은
        // 여기서 버린다 - 한 번 봤던 세션의 흔적을 영원히 들고 있으면, 초 단위로
        // 폴링하는 TUI에서 두 맵이 시간이 지날수록 계속 커지는 메모리 누수가 된다.
        self.trackers.retain(|path, _| claimed.contains(path));
        self.tailer.retain(&claimed);
        self.codex_trackers.retain(|path, _| claimed.contains(path));
        // pid -> uuid 캐시도 같은 이유로 정리한다. codex_paths를 먼저 지우면 살아있는
        // 매핑까지 "경로가 없다"고 오판할 수 있으므로, codex_paths가 아직 이번 tick의
        // claimed 상태를 반영하는 지금 시점에 먼저 정리한다.
        let alive_codex_uuids: std::collections::HashSet<String> = self
            .codex_paths
            .iter()
            .filter(|(_, (path, _))| claimed.contains(path))
            .map(|(uuid, _)| uuid.clone())
            .collect();
        self.codex_pid_uuid
            .retain(|_, uuid| alive_codex_uuids.contains(uuid));
        // claude 쪽 pid 캐시는 프로세스가 사라지면 쓸모가 없다 - 살아 있는 pid만 남긴다.
        let live_pids: std::collections::HashSet<i32> = live.iter().map(|p| p.pid).collect();
        self.claude_pid_uuid
            .retain(|pid, _| live_pids.contains(pid));
        // uuid별 codex 경로 캐시도 같은 이유로 정리한다 - 더는 살아있지 않은
        // 세션의 경로를 무한정 들고 있으면 장시간 폴링에서 메모리가 계속 는다.
        self.codex_paths
            .retain(|_, (path, _)| claimed.contains(path));
        // by_key는 이번 tick에 실제로 화면에 오른 세션 키만 담는다 - trackers/tailer와
        // 같은 이유로, 더는 없는 세션의 workspace_id를 무한정 들고 있지 않는다.
        self.cmux_workspaces
            .retain(|key, _| by_key.contains_key(key));
        // 훅 기록 파일도 같은 이유로 정리한다. 살아 있는 프로세스가 아무도 가리키지
        // 않은 세션의 기록은 다시 쓰일 일이 없다.
        let live_uuids: std::collections::HashSet<String> =
            by_key.keys().map(|k| k.uuid.clone()).collect();
        hooksink::prune(&self.sink_dir, &live_uuids);

        Snapshot {
            sessions,
            hooks_installed,
            cmux_linked: self.cmux_linked,
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

    /// 훅 기록은 파일에 남아 있는 한 영원히 이긴다. `hooks uninstall` 이후나
    /// SessionEnd 없이 훅이 죽은 뒤에도, 살아 있는 세션이 마지막 훅 상태에 고정돼
    /// 실제로는 사용자를 기다리는데 초록색 "running"으로 남는다. transcript가 그
    /// 기록보다 나중에 움직였으면 훅 기록은 낡은 것이다.
    #[test]
    fn hook_record_older_than_the_transcript_is_ignored() {
        let dir = tempfile::tempdir().expect("dir");
        let proj = dir.path().join("-home-dev-app");
        std::fs::create_dir_all(&proj).expect("mkdir");
        let line = r#"{"type":"assistant","timestamp":"2027-01-15T00:00:00.000Z","cwd":"/home/dev/app","isSidechain":false,"message":{"model":"claude-opus-5","content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#;
        std::fs::write(proj.join("u1.jsonl"), format!("{line}\n")).expect("write");
        let ts = crate::collect::transcript::parse_tail(line)
            .expect("tail")
            .last_ts_ms;

        let sink = dir.path().join("sink");
        crate::collect::hooksink::record_event(
            &sink,
            &crate::collect::hooksink::SinkRecord {
                key: crate::model::SessionKey {
                    provider: Provider::Claude,
                    uuid: "u1".into(),
                },
                event: crate::model::HookEvent::PreToolUse,
                occurred_at: ts - 10_000,
                cwd: None,
                pid: None,
            },
        )
        .expect("record");

        let mut c = super::Collector::new(
            Box::new(FakeProcs(vec![proc(Some("u1"), "/home/dev/app", 0.0)])),
            Thresholds::default(),
        )
        .with_projects_root(dir.path().to_path_buf())
        .with_sink_dir(sink);
        let snap = c.snapshot(ts + 5_000);

        assert_eq!(snap.sessions[0].state, State::WaitingInput);
        assert_eq!(
            snap.sessions[0].source,
            crate::model::Source::Layer0Inferred
        );
    }

    /// 살아 있는 프로세스가 아무도 가리키지 않은 세션의 훅 기록 파일은 지운다.
    /// 그러지 않으면 sink 디렉터리가 무한히 자라고(tick마다 전부 읽는다), `hooks on`
    /// 배지도 기록이 한 번이라도 생긴 뒤로는 영원히 켜진 채로 남는다.
    #[test]
    fn sink_files_for_sessions_no_live_process_resolved_to_are_pruned() {
        let dir = tempfile::tempdir().expect("dir");
        let sink = dir.path().join("sink");
        for uuid in ["u1", "ghost"] {
            crate::collect::hooksink::record_event(
                &sink,
                &crate::collect::hooksink::SinkRecord {
                    key: crate::model::SessionKey {
                        provider: Provider::Claude,
                        uuid: uuid.into(),
                    },
                    event: crate::model::HookEvent::Stop,
                    occurred_at: NOW - 1_000,
                    cwd: None,
                    pid: None,
                },
            )
            .expect("record");
        }
        assert_eq!(crate::collect::hooksink::read_all(&sink).len(), 2);

        let mut c = super::Collector::new(
            Box::new(FakeProcs(vec![proc(Some("u1"), "/home/dev/app", 0.0)])),
            Thresholds::default(),
        )
        .with_projects_root(dir.path().join("projects"))
        .with_sink_dir(sink.clone());
        let _ = c.snapshot(NOW);

        let left = crate::collect::hooksink::read_all(&sink);
        assert_eq!(left.len(), 1, "살아 있는 세션의 기록만 남아야 한다");
        assert_eq!(left[0].key.uuid, "u1");
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

    /// `--session-id` 없이 뜬 claude 프로세스는 같은 cwd에서 mtime이 가장 최근인
    /// 미배정 transcript를 가져간다. 두 세션이 같은 cwd에서 번갈아 쓰면 mtime 순서가
    /// tick 사이에 뒤집히고, 캐시가 없으면 배정이 통째로 맞바뀐다 - TUI 선택이 튀고,
    /// 복사되는 `claude --resume <uuid>`가 엉뚱한 세션을 가리킨다.
    #[test]
    fn claude_processes_without_session_id_keep_their_transcript_across_mtime_flips() {
        fn pairs(snap: &crate::json::Snapshot) -> Vec<(Option<i32>, String)> {
            let mut v: Vec<(Option<i32>, String)> = snap
                .sessions
                .iter()
                .map(|s| (s.pid, s.key.uuid.clone()))
                .collect();
            v.sort();
            v
        }

        let dir = tempfile::tempdir().expect("dir");
        let proj = dir.path().join("-home-dev-app");
        std::fs::create_dir_all(&proj).expect("mkdir");
        std::fs::write(proj.join("aaa.jsonl"), "").expect("write aaa");
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(proj.join("bbb.jsonl"), "").expect("write bbb");

        let mut c = super::Collector::new(
            Box::new(FakeProcs(vec![
                proc_with(200, None, "/home/dev/app", 0.0),
                proc_with(201, None, "/home/dev/app", 0.0),
            ])),
            Thresholds::default(),
        )
        .with_projects_root(dir.path().to_path_buf());

        let first = pairs(&c.snapshot(NOW));
        assert_eq!(first.len(), 2);

        // 두 번째 tick 전에 mtime 순서를 뒤집는다.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(proj.join("aaa.jsonl"), "").expect("touch aaa");

        assert_eq!(
            pairs(&c.snapshot(NOW + 1_000)),
            first,
            "mtime이 뒤집혀도 pid별 transcript 배정은 그대로여야 한다"
        );
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

    struct FakeJumper {
        panes: std::collections::HashMap<String, String>,
        refresh_calls: std::sync::Arc<std::sync::atomic::AtomicU32>,
    }

    impl crate::jump::Jumper for FakeJumper {
        fn refresh(&mut self) {
            self.refresh_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }

        fn resolve_tty(&self, tty: &str) -> Option<crate::jump::JumpTarget> {
            self.panes
                .get(tty)
                .cloned()
                .map(crate::jump::JumpTarget::Tmux)
        }
    }

    #[test]
    fn jump_label_is_filled_from_injected_jumper_and_refreshed_once_per_snapshot() {
        let dir = tempfile::tempdir().expect("dir");
        let mut panes = std::collections::HashMap::new();
        panes.insert("/dev/ttys004".to_string(), "main:2.1".to_string());
        let refresh_calls = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let jumper = FakeJumper {
            panes,
            refresh_calls: refresh_calls.clone(),
        };

        let mut p1 = proc_with(100, Some("u1"), "/home/dev/app", 0.0);
        p1.tty = Some("/dev/ttys004".to_string());
        let mut p2 = proc_with(101, Some("u2"), "/home/dev/app2", 0.0);
        p2.tty = Some("/dev/ttys999".to_string());

        let mut c = super::Collector::new(Box::new(FakeProcs(vec![p1, p2])), Thresholds::default())
            .with_projects_root(dir.path().to_path_buf())
            .with_jumpers(vec![Box::new(jumper)]);

        let snap = c.snapshot(NOW);

        let s1 = snap
            .sessions
            .iter()
            .find(|s| s.key.uuid == "u1")
            .expect("u1 present");
        assert_eq!(s1.jump.as_deref(), Some("tmux:main:2.1"));

        let s2 = snap
            .sessions
            .iter()
            .find(|s| s.key.uuid == "u2")
            .expect("u2 present");
        assert_eq!(s2.jump, None, "unknown tty must not get a jump target");

        // refresh는 세션이 몇 개든 tick당 한 번만 불려야 한다 - list-panes를
        // 세션마다 새로 띄우면 20세션에서 spawn이 20배로 늘어 성능 예산을 깬다.
        assert_eq!(refresh_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn tracker_and_offset_entries_are_pruned_once_the_process_is_gone() {
        let dir = tempfile::tempdir().expect("dir");
        let proj = dir.path().join("-home-dev-app");
        std::fs::create_dir_all(&proj).expect("mkdir");
        std::fs::write(proj.join("u1.jsonl"), "").expect("write");

        let mut c = super::Collector::new(
            Box::new(FakeProcs(vec![proc(Some("u1"), "/home/dev/app", 0.0)])),
            Thresholds::default(),
        )
        .with_projects_root(dir.path().to_path_buf());

        let first = c.snapshot(NOW);
        assert_eq!(first.sessions.len(), 1);
        // The transcript was resolved this tick, so both maps hold exactly its entry.
        assert_eq!(c.trackers.len(), 1);
        assert_eq!(c.tailer.tracked_count(), 1);

        // The process behind that session is gone from now on - a TUI polling once a
        // second for hours will keep seeing this on every future tick unless the
        // collector prunes it.
        c.procs = Box::new(FakeProcs(vec![]));
        let second = c.snapshot(NOW + 1_000);

        assert!(second.sessions.is_empty());
        assert_eq!(
            c.trackers.len(),
            0,
            "tracker entry for the dead session must be dropped"
        );
        assert_eq!(
            c.tailer.tracked_count(),
            0,
            "tailer offset for the dead session must be dropped"
        );
    }

    fn codex_proc(pid: i32, uuid: Option<&str>, cwd: Option<&str>, cpu: f32) -> ProcInfo {
        ProcInfo {
            pid,
            provider: Provider::Codex,
            session_id: uuid.map(|u| u.to_string()),
            cwd: cwd.map(PathBuf::from),
            cpu,
            started_at_ms: NOW - 3_600_000,
            tty: None,
        }
    }

    /// codex rollout 트리(`YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`)를 임시 디렉터리
    /// 아래 하나 만든다. `lines`는 그 파일의 내용이다.
    fn write_codex_rollout(root: &std::path::Path, uuid: &str, lines: &str) -> PathBuf {
        let day = root.join("2026/09/15");
        std::fs::create_dir_all(&day).expect("mkdir day");
        let path = day.join(format!("rollout-2026-09-15T00-00-00-{uuid}.jsonl"));
        std::fs::write(&path, lines).expect("write rollout");
        path
    }

    const CODEX_TASK_COMPLETE_TAIL: &str = concat!(
        r#"{"type":"session_meta","timestamp":"2026-09-15T00:00:00.000Z","ordinal":0,"payload":{"cwd":"/home/dev/from-meta"}}"#,
        "\n",
        r#"{"type":"response_item","timestamp":"2026-09-15T00:00:01.000Z","ordinal":1,"payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}}"#,
        "\n",
        r#"{"type":"event_msg","timestamp":"2026-09-15T00:00:01.500Z","ordinal":2,"payload":{"type":"task_complete","completed_at":"2026-09-15T00:00:01.500Z","duration_ms":500,"last_agent_message":"done","time_to_first_token_ms":100,"turn_id":"t1"}}"#,
        "\n",
    );

    #[test]
    fn codex_session_with_explicit_session_id_gets_fact_confidence_from_task_complete() {
        let dir = tempfile::tempdir().expect("dir");
        let uuid = "11111111-1111-1111-1111-111111111111";
        write_codex_rollout(dir.path(), uuid, CODEX_TASK_COMPLETE_TAIL);

        let mut c = super::Collector::new(
            Box::new(FakeProcs(vec![codex_proc(
                500,
                Some(uuid),
                Some("/home/dev/app"),
                0.0,
            )])),
            Thresholds::default(),
        )
        .with_codex_sessions_root(dir.path().to_path_buf());

        // 픽스처의 timestamp는 고정된 과거 날짜라, NOW(다른 테스트가 쓰는 상수)를
        // 그대로 쓰면 나이가 stale 문턱을 넘어 demote()가 상태를 깔아뭉갠다.
        // task_complete 이벤트 시각 바로 뒤로 스냅샷 시각을 맞춘다.
        let ts = crate::collect::transcript::parse_ts_ms("2026-09-15T00:00:01.500Z")
            .expect("parse fixture ts");
        let snap = c.snapshot(ts + 5_000);
        assert_eq!(snap.sessions.len(), 1);
        let s = &snap.sessions[0];
        // task_complete가 마지막이면 사실이지 추측이 아니다 - claude가 같은 모양의
        // AssistantText 꼬리에서 받는 Medium보다 나은 Confidence::Fact를 받는다.
        assert_eq!(s.state, State::WaitingInput);
        assert_eq!(s.confidence, crate::model::Confidence::Fact);
        assert_eq!(s.source, crate::model::Source::Layer0Inferred);
    }

    #[test]
    fn codex_process_without_cwd_becomes_visible_using_rollout_meta_cwd() {
        let dir = tempfile::tempdir().expect("dir");
        let uuid = "22222222-2222-2222-2222-222222222222";
        write_codex_rollout(dir.path(), uuid, CODEX_TASK_COMPLETE_TAIL);

        // session_id도 cwd도 모르는 프로세스 - ChatGPT 앱이 띄운 codex 프로세스를
        // sysinfo가 cwd 없이 보고하는 실제 상황을 흉내낸다. 예전에는 이 프로세스가
        // 2차 배정 루프에서 cwd가 없다는 이유로 통째로 건너뛰어져 목록에서
        // 사라졌었다.
        let mut c = super::Collector::new(
            Box::new(FakeProcs(vec![codex_proc(501, None, None, 0.0)])),
            Thresholds::default(),
        )
        .with_codex_sessions_root(dir.path().to_path_buf());

        let snap = c.snapshot(NOW);
        assert_eq!(
            snap.sessions.len(),
            1,
            "cwd 없는 codex 프로세스도 세션으로 보여야 한다"
        );
        assert_eq!(snap.sessions[0].cwd.as_deref(), Some("/home/dev/from-meta"));
    }

    #[test]
    fn codex_path_cache_is_populated_on_first_tick_and_pruned_when_process_dies() {
        let dir = tempfile::tempdir().expect("dir");
        let uuid = "33333333-3333-3333-3333-333333333333";
        write_codex_rollout(dir.path(), uuid, CODEX_TASK_COMPLETE_TAIL);

        let mut c = super::Collector::new(
            Box::new(FakeProcs(vec![codex_proc(
                502,
                Some(uuid),
                Some("/home/dev/app"),
                0.0,
            )])),
            Thresholds::default(),
        )
        .with_codex_sessions_root(dir.path().to_path_buf());

        let first = c.snapshot(NOW);
        assert_eq!(first.sessions.len(), 1);
        assert_eq!(
            c.codex_paths.len(),
            1,
            "첫 tick에서 찾은 경로가 uuid별로 캐시돼야 한다"
        );
        assert_eq!(c.codex_trackers.len(), 1);

        c.procs = Box::new(FakeProcs(vec![]));
        let second = c.snapshot(NOW + 1_000);
        assert!(second.sessions.is_empty());
        assert_eq!(
            c.codex_paths.len(),
            0,
            "더 이상 없는 codex 세션의 경로 캐시는 버려야 한다"
        );
        assert_eq!(c.codex_trackers.len(), 0);
    }

    #[test]
    fn codex_context_window_comes_from_token_count_not_the_claude_heuristic() {
        let dir = tempfile::tempdir().expect("dir");
        let uuid = "44444444-4444-4444-4444-444444444444";
        // total_tokens는 일부러 200k 미만으로 둔다 - claude 휴리스틱(관측값이 200k를
        // 넘으면 1M으로 짐작)을 그대로 썼다면 이 값에서는 200000이 나와야 한다.
        // codex는 window를 실측으로 직접 주므로 관측값과 무관하게 1000000이 나와야
        // 맞다.
        let lines = concat!(
            r#"{"type":"response_item","timestamp":"2026-09-15T00:00:01.000Z","ordinal":1,"payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"}]}}"#,
            "\n",
            r#"{"type":"event_msg","timestamp":"2026-09-15T00:00:01.200Z","ordinal":2,"payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":50000},"model_context_window":1000000},"rate_limits":{}}}"#,
            "\n",
        );
        write_codex_rollout(dir.path(), uuid, lines);

        let mut c = super::Collector::new(
            Box::new(FakeProcs(vec![codex_proc(
                503,
                Some(uuid),
                Some("/home/dev/app"),
                0.0,
            )])),
            Thresholds::default(),
        )
        .with_codex_sessions_root(dir.path().to_path_buf());

        let snap = c.snapshot(NOW);
        assert_eq!(snap.sessions[0].ctx_tokens, Some(50_000));
        assert_eq!(snap.sessions[0].ctx_window, Some(1_000_000));
    }

    /// 실제 `~/.codex/sessions`(이 개발 머신에서 수백MB) 앞에서 첫 tick과 캐시된
    /// 이후 tick의 비용을 잰다. 실제 홈 디렉터리 상태에 좌우되므로 일반
    /// `cargo test`에서는 돌지 않는다 - `cargo test -- --ignored --nocapture
    /// codex_real_machine_scan_cost`로 수동 실행한다. 경로/uuid를 하드코딩하지
    /// 않는다 - `codex::codex_sessions_root()`가 `$HOME`에서 매번 새로 계산한다.
    #[test]
    #[ignore]
    fn codex_real_machine_scan_cost() {
        let root = super::codex::codex_sessions_root();
        if !root.exists() {
            eprintln!("skip: {} 없음", root.display());
            return;
        }

        // session_id도 cwd도 모르는 codex 프로세스 - 2차 배정 루프가 매번 날짜
        // 트리를 훑어야 하는, 캐시 없이는 가장 비싼 경로를 그대로 재현한다.
        let mut c = super::Collector::new(
            Box::new(FakeProcs(vec![codex_proc(9, None, None, 0.0)])),
            Thresholds::default(),
        );

        let t0 = std::time::Instant::now();
        let first = c.snapshot(NOW);
        let first_cost = t0.elapsed();

        let t1 = std::time::Instant::now();
        let second = c.snapshot(NOW + 1_000);
        let second_cost = t1.elapsed();

        eprintln!(
            "codex 세션 첫 tick {}개 {:?}, 두번째 tick(캐시 히트) {}개 {:?}",
            first.sessions.len(),
            first_cost,
            second.sessions.len(),
            second_cost
        );
        assert!(
            second_cost <= first_cost,
            "캐시된 두번째 tick이 첫 tick보다 느리면 안 된다: {second_cost:?} > {first_cost:?}"
        );
    }
}
