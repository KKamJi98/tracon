pub mod antigravity;
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
    // 레이어 2(cmux). None이면 cmux가 없거나 아직 연결을 시도하지 않은 것 -
    // 이 경우 나머지 전부는 cmux가 존재한 적 없는 것처럼 그대로 동작해야 한다.
    cmux_subscriber: Option<cmux::CmuxSubscriber>,
    cmux_linked: bool,
    /// Antigravity는 transcript가 없어 위의 배정 기계를 타지 않는다. 실행 중인
    /// `agy`가 열어 둔 `brain/<uuid>`가 대화를 직접 알려주므로 짐작이 필요 없다.
    /// `lsof`는 0.15초쯤 걸리니 pid마다 한 번만 부르고 여기 담아 둔다.
    agy_pid_uuid: std::collections::HashMap<i32, String>,
    /// 대화별 마지막 읽기 결과와 그때의 활동 시각. SQLite를 tick마다 새로 열 이유가
    /// 없다 - 파일이 움직였을 때만 다시 읽는다.
    agy_cache: std::collections::HashMap<String, (i64, antigravity::ConversationInfo)>,
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
            cmux_subscriber: None,
            cmux_linked: false,
            agy_pid_uuid: std::collections::HashMap::new(),
            agy_cache: std::collections::HashMap::new(),
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

    pub fn snapshot(&mut self, now_ms: i64) -> Snapshot {
        let live: Vec<ProcInfo> = self.procs.list_agents();
        let sink_records = hooksink::read_all(&self.sink_dir);
        let hooks_installed = !sink_records.is_empty();

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

        // 훅이 적어 둔 pid -> uuid. 훅 payload에는 pid가 없어서 훅이 자기 조상 체인을
        // 거슬러 올라가 찾아 넣은 값이다. argv의 `--session-id`와 같은 급의 사실이므로
        // 2단계의 cwd+mtime 짐작보다 먼저 쓴다. 같은 pid에 기록이 여러 건이면 최신 것.
        let mut hook_pid_uuid: std::collections::HashMap<i32, (i64, String)> =
            std::collections::HashMap::new();
        for r in &sink_records {
            let Some(pid) = r.pid else { continue };
            let slot = hook_pid_uuid
                .entry(pid)
                .or_insert((i64::MIN, String::new()));
            if r.occurred_at >= slot.0 {
                *slot = (r.occurred_at, r.key.uuid.clone());
            }
        }

        // 1단계: 세션이 사실로 밝혀진 프로세스는 자기 transcript를 그대로 차지한다.
        // argv의 `--session-id`나 훅 기록 둘 중 하나면 된다. 이 배정은 2단계보다 먼저
        // 끝나므로 live 벡터 안에서의 순서와 무관하게 우선한다.
        let mut pinned: std::collections::HashSet<i32> = std::collections::HashSet::new();
        for p in &live {
            let known = p.session_id.clone().or_else(|| {
                hook_pid_uuid
                    .get(&p.pid)
                    .filter(|_| p.provider == Provider::Claude)
                    .map(|(_, uuid)| uuid.clone())
            });
            let Some(id) = known.as_ref() else {
                continue;
            };
            pinned.insert(p.pid);
            let path = match p.provider {
                // Antigravity는 아래 전용 경로에서 따로 모은다.
                Provider::Antigravity => continue,
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
        let mut unclaimed_procs: Vec<&ProcInfo> = live
            .iter()
            .filter(|p| p.session_id.is_none() && !pinned.contains(&p.pid))
            .collect();
        unclaimed_procs.sort_by_key(|p| p.pid);
        for p in unclaimed_procs {
            let (key, path) = match p.provider {
                Provider::Antigravity => continue,
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
            // 모델 신원에서 읽어낸 컨텍스트 윈도우. 관측 토큰 수로 짐작하는
            // 폴백보다 우선한다.
            let mut window_fact: Option<u64> = None;
            let summary = match path {
                Some(path) => {
                    let chunk = self.tailer.read_new(path).unwrap_or_default();
                    match key.provider {
                        // 위에서 걸러져 여기까지 오지 않는다.
                        Provider::Antigravity => None,
                        Provider::Claude => {
                            let tracker = match self.trackers.entry(path.clone()) {
                                std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                                // 처음 보는 transcript다. 모델 신원은 파일 첫머리에
                                // 한 번만 적히고 다시 나오지 않으므로, 끝만 보는
                                // Tailer로는 영영 못 만난다 - 여기서 앞부분을 딱 한 번
                                // 읽는다. 대화 엔트리는 일부러 건드리지 않는다.
                                std::collections::hash_map::Entry::Vacant(e) => {
                                    let mut fresh = transcript::TranscriptTracker::new();
                                    if let Ok(head) = tail::read_head(path) {
                                        fresh.apply_meta(&head);
                                    }
                                    e.insert(fresh)
                                }
                            };
                            tracker.apply(&chunk);
                            window_fact = tracker.context_window();
                            tracker.summary()
                        }
                        Provider::Codex => {
                            let tracker = self.codex_trackers.entry(path.clone()).or_default();
                            tracker.apply(&chunk);
                            turn_ended = tracker.turn_ended();
                            window_fact = tracker.context_window();
                            tracker.summary()
                        }
                    }
                }
                None => None,
            };

            let (state0, conf0) = match key.provider {
                Provider::Antigravity => {
                    (crate::model::State::Unknown, crate::model::Confidence::Low)
                }
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
            let title = summary.as_ref().and_then(|s| s.title.clone());
            let entrypoint = summary.as_ref().and_then(|s| s.entrypoint.clone());
            let ctx_window = match key.provider {
                Provider::Antigravity => None,
                // 모델 신원을 봤으면 그게 사실이다. 못 본 세션만 관측값으로 짐작한다.
                Provider::Claude => window_fact.or_else(|| {
                    ctx_tokens.map(|t| transcript::window_for(model.as_deref().unwrap_or(""), t))
                }),
                // codex는 token_count 이벤트가 윈도우를 직접 알려주므로 짐작하지 않는다.
                Provider::Codex => window_fact,
            };
            let cwd = match key.provider {
                Provider::Antigravity => None,
                // codex는 session_meta.cwd(rollout 파일 자체가 기록한 값)를 우선한다 -
                // ChatGPT 앱이 띄운 codex 프로세스는 sysinfo가 cwd를 못 준다.
                Provider::Codex => self
                    .codex_paths
                    .get(&key.uuid)
                    .and_then(|(_, cwd)| cwd.clone())
                    .or_else(|| p.cwd.as_ref().map(|c| c.to_string_lossy().into_owned())),
                Provider::Claude => p.cwd.as_ref().map(|c| c.to_string_lossy().into_owned()),
            };
            sessions.push(Session {
                key: key.clone(),
                state: crate::model::demote(win.state, now_ms - last_change, &self.cfg),
                source: win.source,
                confidence: win.confidence,
                last_change_ms: last_change,
                started_at_ms: Some(p.started_at_ms),
                cwd,
                title,
                entrypoint,
                ctx_window,
                ctx_tokens,
                model,
                cpu: Some(p.cpu),
                pid: Some(p.pid),
            });
        }

        // Antigravity는 transcript 대신 대화 SQLite를 쓴다. 프로세스가 열어 둔
        // brain 디렉터리가 대화를 사실로 알려주므로, 위의 cwd+mtime 짐작을 거치지
        // 않고 여기서 곧장 행을 만든다.
        let agy_root = antigravity::root();
        let agy_procs: Vec<&ProcInfo> = live
            .iter()
            .filter(|p| p.provider == Provider::Antigravity)
            .collect();
        if !agy_procs.is_empty() {
            let titles = antigravity::titles(&agy_root);
            let mut seen: std::collections::HashSet<i32> = std::collections::HashSet::new();
            for p in agy_procs {
                seen.insert(p.pid);
                let uuid = match self.agy_pid_uuid.get(&p.pid) {
                    Some(u) => u.clone(),
                    None => match antigravity::conversation_for_pid(p.pid) {
                        Some(u) => {
                            self.agy_pid_uuid.insert(p.pid, u.clone());
                            u
                        }
                        // 아직 대화를 열지 않은 프로세스다. 다음 tick에 다시 본다.
                        None => continue,
                    },
                };
                // 진행 중에도 갱신되는 평문 transcript가 있으면 그 마지막 단계가
                // 파일 mtime보다 정확하다 - 언제 움직였는지가 아니라 누구 차례인지를
                // 알려 준다.
                let step = antigravity::last_step(&agy_root, &uuid);
                let db_ms = antigravity::last_activity_ms(&agy_root, &uuid);
                let last_ms = step
                    .as_ref()
                    .map(|s| s.ts_ms)
                    .or(db_ms)
                    .unwrap_or(p.started_at_ms);
                // 대화 SQLite는 파일이 움직였을 때만 다시 읽는다 - tick마다 열 이유가 없다.
                let cache_key = db_ms.unwrap_or(last_ms);
                let info = match self.agy_cache.get(&uuid) {
                    Some((seen_at, info)) if *seen_at == cache_key => info.clone(),
                    _ => {
                        let fresh =
                            antigravity::read_conversation(&agy_root, &uuid).unwrap_or_default();
                        self.agy_cache
                            .insert(uuid.clone(), (cache_key, fresh.clone()));
                        fresh
                    }
                };
                let (state, confidence) =
                    antigravity::infer(step.as_ref(), now_ms - last_ms, &self.cfg);
                sessions.push(Session {
                    key: SessionKey {
                        provider: Provider::Antigravity,
                        uuid: uuid.clone(),
                    },
                    state,
                    source: Source::Layer0Inferred,
                    confidence,
                    last_change_ms: last_ms,
                    started_at_ms: Some(p.started_at_ms),
                    cwd: p.cwd.as_ref().map(|c| c.to_string_lossy().into_owned()),
                    title: titles.get(&uuid).cloned(),
                    // agy 세션은 사람이 터미널에서 띄운다. SDK 판별 대상이 아니다.
                    entrypoint: Some("cli".to_string()),
                    ctx_window: info.ctx_window,
                    ctx_tokens: info.ctx_tokens,
                    model: info.model.clone(),
                    cpu: Some(p.cpu),
                    pid: Some(p.pid),
                });
            }
            // 사라진 프로세스의 캐시는 버린다.
            self.agy_pid_uuid.retain(|pid, _| seen.contains(pid));
            let live_uuids: std::collections::HashSet<String> =
                self.agy_pid_uuid.values().cloned().collect();
            self.agy_cache.retain(|uuid, _| live_uuids.contains(uuid));
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

    /// 세션을 닫으면 그 transcript는 그 cwd에서 가장 최근 것이 된다. `--session-id`
    /// 없이 떠 있는 다른 프로세스가 짐작으로 그걸 집어가고, 닫은 세션의 이름이
    /// 살아 있는 행에 계속 붙는다. 닫힐 때 훅이 남긴 `SessionEnd`가 그 행을 Dead로
    /// 만들어 기본 목록에서 접히게 한다.
    #[test]
    fn a_closed_session_reads_as_dead_even_if_another_process_grabs_its_transcript() {
        let dir = tempfile::tempdir().expect("dir");
        let proj = dir.path().join("-home-dev-app");
        std::fs::create_dir_all(&proj).expect("mkdir");
        std::fs::write(proj.join("closed.jsonl"), "").expect("write");

        let sink = tempfile::tempdir().expect("sink");
        crate::collect::hooksink::record_event(
            sink.path(),
            &crate::collect::hooksink::SinkRecord {
                key: crate::model::SessionKey {
                    provider: crate::model::Provider::Claude,
                    uuid: "closed".into(),
                },
                event: crate::model::HookEvent::SessionEnd,
                occurred_at: NOW - 1_000,
                cwd: Some("/home/dev/app".into()),
                pid: None,
            },
        )
        .expect("record");

        let mut c = super::Collector::new(
            Box::new(FakeProcs(vec![proc_with(300, None, "/home/dev/app", 0.0)])),
            Thresholds::default(),
        )
        .with_projects_root(dir.path().to_path_buf())
        .with_sink_dir(sink.path().to_path_buf());

        let snap = c.snapshot(NOW);
        let row = snap
            .sessions
            .iter()
            .find(|s| s.key.uuid == "closed")
            .expect("closed 세션");
        assert_eq!(
            row.state,
            State::Dead,
            "닫힌 세션이 살아 있는 행으로 계속 보인다"
        );
        assert_eq!(row.source, crate::model::Source::Layer1Hook);
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

    /// `--session-id` 없이 뜬 프로세스는 cwd + mtime 짐작으로 transcript를 배정받는다.
    /// 같은 cwd에 세션이 여러 개면 그 짐작이 틀려서, 닫힌 세션의 이름이 살아 있는
    /// 다른 행에 붙는다. 훅이 조상 체인에서 찾아 적어 둔 pid가 그 짐작을 끊는다.
    #[test]
    fn a_hook_record_pins_the_session_to_its_real_process() {
        let dir = tempfile::tempdir().expect("dir");
        let proj = dir.path().join("-home-dev-app");
        std::fs::create_dir_all(&proj).expect("mkdir");
        // mtime 순서상 짐작은 bbb를 먼저 집는다. 훅은 200번이 aaa라고 말한다.
        std::fs::write(proj.join("aaa.jsonl"), "").expect("write aaa");
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(proj.join("bbb.jsonl"), "").expect("write bbb");

        let sink = tempfile::tempdir().expect("sink");
        let rec = crate::collect::hooksink::SinkRecord {
            key: crate::model::SessionKey {
                provider: crate::model::Provider::Claude,
                uuid: "aaa".into(),
            },
            event: crate::model::HookEvent::Stop,
            occurred_at: NOW - 1_000,
            cwd: Some("/home/dev/app".into()),
            pid: Some(200),
        };
        crate::collect::hooksink::record_event(sink.path(), &rec).expect("record");

        let mut c = super::Collector::new(
            Box::new(FakeProcs(vec![
                proc_with(200, None, "/home/dev/app", 0.0),
                proc_with(201, None, "/home/dev/app", 0.0),
            ])),
            Thresholds::default(),
        )
        .with_projects_root(dir.path().to_path_buf())
        .with_sink_dir(sink.path().to_path_buf());

        let snap = c.snapshot(NOW);
        let pinned = snap
            .sessions
            .iter()
            .find(|s| s.pid == Some(200))
            .expect("200번 세션");
        assert_eq!(
            pinned.key.uuid, "aaa",
            "훅이 말해 준 세션 대신 mtime 짐작을 따랐다"
        );
        let other = snap
            .sessions
            .iter()
            .find(|s| s.pid == Some(201))
            .expect("201번 세션");
        assert_eq!(
            other.key.uuid, "bbb",
            "남은 프로세스가 남은 transcript를 가져야 한다"
        );
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
