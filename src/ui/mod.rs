//! TUI 렌더링. 위쪽 Overview 블록과 아래쪽 세션 테이블을 그린다.
//! 색상 판단은 [`theme`]에만 두고, 여기와 하위 모듈은 그 결과만 쓴다.

mod overview;
mod table;
pub(crate) mod theme;

use crate::json::Snapshot;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::widgets::Paragraph;
use ratatui::{Frame, Terminal};
use std::io::{stdout, Stdout};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

/// 화면 맨 아래 한 줄에 띄우는 키 안내. 실제로 동작하는 키만 적는다 - `x`(kill)와
/// `s`(sort)는 아직 아무 일도 하지 않으므로 광고하지 않는다.
pub(crate) const KEY_HINTS: &str = "enter/r copy resume   j/k move   q quit";

/// 24시간 넘게 아무 일도 없었던 세션. "지금 누가 나를 기다리는가"를 보는 화면에서
/// 자리만 차지한다.
fn is_dormant(state: crate::model::State) -> bool {
    matches!(
        state,
        crate::model::State::Stale | crate::model::State::Dead
    )
}

/// 사람이 아니라 프로그램이 몰고 있는 세션. 보안 리뷰 서브에이전트처럼 SDK가 띄운
/// 것들이다. 누구도 그 앞에 앉아 있지 않으므로 나를 기다릴 수가 없다. entrypoint를
/// 아직 못 본 세션은 접지 않는다 - 모르면 사람 것으로 본다.
fn is_headless(entrypoint: Option<&str>) -> bool {
    entrypoint.is_some_and(|e| e != "cli")
}

/// 기본 목록에서 접는 행. 숨긴다고 없는 셈 치지는 않는다 - 오버뷰 카운터에는
/// 그대로 세고, 푸터가 몇 개를 접었는지 말해 주며, `a`로 편다.
fn is_folded(session: &crate::model::Session) -> bool {
    is_dormant(session.state) || is_headless(session.entrypoint.as_deref())
}

/// 이번에 실제로 그릴 행. `show_all`이면 전부.
pub fn visible_rows(snap: &Snapshot, show_all: bool) -> Vec<&crate::model::Session> {
    snap.sessions
        .iter()
        .filter(|s| show_all || !is_folded(s))
        .collect()
}

/// 푸터 한 줄. 접힌 행이 있을 때만 `a`를 광고한다 - 할 일이 없는 키는 안내하지 않는다.
fn footer(hidden: usize, show_all: bool) -> String {
    if show_all {
        format!("{KEY_HINTS}   a fold back")
    } else if hidden > 0 {
        format!("{KEY_HINTS}   a show {hidden} folded")
    } else {
        KEY_HINTS.to_string()
    }
}

#[allow(dead_code)]
pub fn render(frame: &mut Frame, snap: &Snapshot, selected: usize, show_all: bool) {
    let area = frame.area();
    // 푸터는 고정 1줄, 테이블이 남는 높이를 받는다. 화면이 아주 낮으면 테이블 쪽이
    // 0줄로 줄어들 뿐 렌더는 계속 성립한다.
    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(area);
    let rows = visible_rows(snap, show_all);
    let hidden = snap.sessions.len() - rows.len();
    // 오버뷰는 언제나 스냅샷 전체를 센다 - 접힌 행도 존재는 한다.
    overview::render(frame, chunks[0], snap);
    table::render(frame, chunks[1], &rows, snap.generated_at_ms, selected);
    frame.render_widget(Paragraph::new(footer(hidden, show_all)), chunks[2]);
}

/// 밀리초를 `2s`, `4m12s`, `5h`, `5d02h` 형태로 압축한다.
#[allow(dead_code)]
pub fn format_age(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    if s < 60 {
        return format!("{s}s");
    }
    if s < 3_600 {
        return format!("{}m{:02}s", s / 60, s % 60);
    }
    if s < 86_400 {
        return format!("{}h", s / 3_600);
    }
    format!("{}d{:02}h", s / 86_400, (s % 86_400) / 3_600)
}

/// 키 입력이 요청하는 동작. `on_key`는 순수하게 동작을 반환만 하고, 실제 실행은
/// `handle_action`이 한다 - 헤드리스로 테스트하기 위해서다. `Kill`은 프로세스를
/// 죽이는 범위가 이 task 밖이라 여전히 inert 하다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    Quit,
    CopyResume(usize),
    Kill(usize),
    ToggleSort,
}

/// TUI의 순수 상태. 렌더링·IO와 분리해 두어야 `on_key`/`apply`를 헤드리스로
/// 테스트할 수 있다.
pub struct App {
    pub snapshot: Snapshot,
    pub selected: usize,
    /// 상태줄에 한 줄 띄우는 알림과 그 만료 시각. 복사 확인은 읽고 나면 볼 일이
    /// 없으므로 스스로 사라진다 - 사라지지 않으면 키 안내를 계속 가린다.
    pub message: Option<Notice>,
    /// 잠든(stale/dead) 세션까지 전부 보여줄지. `a`로 토글한다.
    pub show_all: bool,
}

/// 상태줄 알림 한 건. `until_ms`가 지나면 스스로 사라진다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub text: String,
    pub until_ms: i64,
}

/// 복사 확인이 화면에 머무는 시간. 한 번 읽기에 충분하고, 다음 행으로 넘어가기
/// 전에는 사라질 만큼 짧다.
pub const NOTICE_MS: i64 = 2_500;

impl App {
    pub fn new(snapshot: Snapshot) -> Self {
        Self {
            snapshot,
            selected: 0,
            message: None,
            show_all: false,
        }
    }

    /// 만료된 상태줄 알림을 치운다. 시각을 인자로 받아 헤드리스로 테스트한다.
    pub fn expire_message(&mut self, now_ms: i64) {
        if self.message.as_ref().is_some_and(|n| now_ms >= n.until_ms) {
            self.message = None;
        }
    }

    /// 이번에 화면에 오른 행들. `selected`는 이 목록의 인덱스다 - 스냅샷 전체가
    /// 아니라. 접힌 행을 세면 커서가 보이지 않는 줄을 가리키게 된다.
    pub fn visible(&self) -> Vec<&crate::model::Session> {
        visible_rows(&self.snapshot, self.show_all)
    }

    pub fn selected_key(&self) -> Option<&crate::model::SessionKey> {
        let rows = self.visible();
        rows.get(self.selected).map(|s| &s.key)
    }

    /// 새 스냅샷을 받아도 사용자가 보던 세션에 선택을 유지한다.
    pub fn apply(&mut self, snapshot: Snapshot) {
        let prev = self.selected_key().cloned();
        self.snapshot = snapshot;
        self.selected = prev
            .and_then(|k| self.visible().iter().position(|s| s.key == k))
            .unwrap_or(0);
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        // raw mode에서는 터미널이 ISIG를 끄므로 ctrl-c가 SIGINT로 오지 않고 평범한
        // 키 이벤트로 온다. 여기서 받아 주지 않으면 사용자가 아는 유일한 탈출구가
        // 아무 반응도 하지 않는다 - 목록이 비었든 아니든 먼저 본다.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d'))
        {
            return Action::Quit;
        }
        if key.code == KeyCode::Char('a') {
            self.show_all = !self.show_all;
            // 펼치거나 접으면 목록 길이가 바뀐다 - 커서를 범위 안으로 되돌린다.
            self.selected = self.selected.min(self.visible().len().saturating_sub(1));
            return Action::None;
        }
        let len = self.visible().len();
        if len == 0 {
            return match key.code {
                KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
                _ => Action::None,
            };
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
            KeyCode::Char('j') | KeyCode::Down => {
                self.selected = (self.selected + 1).min(len - 1);
                Action::None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                Action::None
            }
            KeyCode::Enter | KeyCode::Char('r') => Action::CopyResume(self.selected),
            KeyCode::Char('x') => Action::Kill(self.selected),
            KeyCode::Char('s') => Action::ToggleSort,
            _ => Action::None,
        }
    }
}

/// resume 명령을 클립보드에 복사하고, 실패하면 그 명령 문자열 자체를 상태줄에
/// 보여줄 메시지로 돌려준다.
fn copy_or_show(cmd: String) -> String {
    if crate::resume::clipboard::copy(&cmd) {
        format!("복사됨: {cmd}")
    } else {
        cmd
    }
}

/// `Action::CopyResume`를 실제로 실행한다. `Action::Kill`과
/// `Action::ToggleSort`/`Action::None`/`Action::Quit`은 이 task 범위 밖이거나
/// 이미 `event_loop`에서 처리되어 여기서는 아무것도 하지 않는다.
fn handle_action(app: &mut App, action: Action, now_ms: i64) {
    match action {
        Action::CopyResume(i) => {
            let rows = app.visible();
            let Some(session) = rows.get(i) else {
                return;
            };
            let resume_cmd = crate::resume::resume_command(session);
            app.message = Some(Notice {
                text: copy_or_show(resume_cmd),
                until_ms: now_ms + NOTICE_MS,
            });
        }
        Action::Kill(_) | Action::ToggleSort | Action::None | Action::Quit => {}
    }
}

/// 패닉 훅을 설치한다. 기본 훅이 메시지를 찍기 전에 터미널을 raw mode/alternate
/// screen에서 먼저 복구해, 어떤 경로로 죽어도 사용자의 터미널이 먹통이 되지 않게 한다.
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal();
        default_hook(info);
    }));
}

fn init_terminal() -> anyhow::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen)?;
    Ok(Terminal::new(CrosstermBackend::new(out))?)
}

/// 실제 복구 작업이 이미 끝났는지 표시한다. 패닉 훅(수집 스레드에서 죽을 수도
/// 있다)과 `run_tui`의 정상 종료 경로가 둘 다 `restore_terminal`을 부를 수
/// 있으므로, 두 번째 호출이 raw mode/alternate screen을 다시 건드리지 않게 한다.
static TERMINAL_RESTORED: AtomicBool = AtomicBool::new(false);

/// alternate screen을 떠나고 raw mode를 해제한다. 정상 종료, 에러 종료, 패닉
/// 세 경로 모두 이 함수 하나로 복구한다. 두 번 불러도 안전하다 - 실제 복구는
/// 프로세스당 한 번만 수행된다.
fn restore_terminal() -> anyhow::Result<()> {
    if TERMINAL_RESTORED.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    disable_raw_mode()?;
    execute!(stdout(), LeaveAlternateScreen)?;
    Ok(())
}

/// 수집 스레드를 띄우고 렌더/키 루프를 돈다. 인자 없이 실행했을 때의 진입점.
pub fn run_tui() -> anyhow::Result<()> {
    install_panic_hook();
    let mut terminal = init_terminal()?;

    // cmux가 없거나 구독이 즉시 죽으면 None이고, 그 아래 나머지는 cmux가 존재한
    // 적 없는 것처럼 그대로 동작한다 - 레이어 2는 언제나 선택이다. TUI는 오래
    // 사는 프로세스라 `spawn()`(재연결 포함)을 쓴다 - `--json`의 1회성
    // `spawn_one_shot()`과 다르다.
    let cmux = crate::collect::cmux::CmuxSubscriber::spawn();
    let mut collector = crate::collect::Collector::new(
        Box::new(crate::collect::proc::SysProcessSource::new()),
        crate::config::Thresholds::default(),
    )
    .with_cmux(cmux);
    // 첫 프레임은 백그라운드 스레드의 1초 tick을 기다리지 않고 즉시 그린다.
    let mut app = App::new(collector.snapshot(crate::collect::hooksink::now_ms()));

    let (tx, rx) = mpsc::channel::<Snapshot>();
    let stop = Arc::new(AtomicBool::new(false));
    let collector_stop = Arc::clone(&stop);
    std::thread::spawn(move || {
        while !collector_stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_secs(1));
            if collector_stop.load(Ordering::Relaxed) {
                break;
            }
            let snap = collector.snapshot(crate::collect::hooksink::now_ms());
            // 수신자가 이미 사라졌으면(UI가 종료 중) 조용히 스레드를 끝낸다.
            if tx.send(snap).is_err() {
                break;
            }
        }
    });

    let result = event_loop(&mut terminal, &mut app, &rx);
    stop.store(true, Ordering::Relaxed);
    // 수집 스레드는 join하지 않는다 - 최악의 경우도 1초 sleep 중 하나뿐이고,
    // 프로세스가 곧 끝나므로 join으로 종료를 늦출 이유가 없다.
    let restore_result = restore_terminal();

    // alternate screen을 떠난 뒤에만 이 메시지를 찍는다 - 그 전에 찍으면 화면
    // 위에 그대로 파묻힌다. 수집 스레드가 패닉으로 죽었더라도 여기서는 패닉
    // 덤프를 다시 찍지 않고, 무슨 일이 있었는지 한 줄로만 알린다.
    if let Ok(ExitReason::CollectorGone) = &result {
        eprintln!("tracon: collector 스레드가 종료되어 tracon을 종료합니다.");
    }

    result.map(|_| ()).and(restore_result)
}

/// 이번 tick에 채널을 비운 결과. `Continue`는 평범한 유휴 상태이고,
/// `CollectorGone`은 송신자가 사라졌다는 뜻이다 - 수집 스레드가 정상 종료했든
/// 패닉으로 죽었든, 더 이상 새 스냅샷이 올 수 없으므로 루프를 끝내야 한다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PollOutcome {
    Continue,
    CollectorGone,
}

/// 채널에 쌓인 스냅샷을 전부 반영한다. `terminal.draw`/`event::poll`과 달리
/// 순수 함수라 헤드리스로 테스트할 수 있다.
fn drain_snapshots(app: &mut App, rx: &mpsc::Receiver<Snapshot>) -> PollOutcome {
    loop {
        match rx.try_recv() {
            Ok(snap) => app.apply(snap),
            Err(mpsc::TryRecvError::Empty) => return PollOutcome::Continue,
            Err(mpsc::TryRecvError::Disconnected) => return PollOutcome::CollectorGone,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitReason {
    Quit,
    CollectorGone,
}

/// 100ms마다 키를 폴링하고, 채널에 새 스냅샷이 있으면 반영한 뒤 매 tick 다시 그린다.
/// 수집 스레드가 느려져도 이 루프는 채널을 기다리지 않으므로 키 입력이 막히지 않는다.
fn event_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    rx: &mpsc::Receiver<Snapshot>,
) -> anyhow::Result<ExitReason> {
    loop {
        // 만료된 알림은 그리기 전에 치운다. 100ms 루프라 눈에 보이는 지연은 없다.
        app.expire_message(crate::collect::hooksink::now_ms());
        terminal.draw(|f| {
            render(f, &app.snapshot, app.selected, app.show_all);
            // 클립보드 폴백 메시지는 render()가 그리는 고정 레이아웃과 별개로,
            // 화면 맨 아래 한 줄에 덧그린다 - render()는 테스트가 직접 호출하는
            // 순수 함수라 시그니처를 바꾸고 싶지 않다. 그 자리는 키 안내 푸터라,
            // 메시지가 떠 있는 동안에는 안내 대신 메시지가 보인다.
            if let Some(notice) = &app.message {
                let area = f.area();
                if area.height > 0 {
                    let bar = Rect {
                        x: area.x,
                        y: area.y + area.height - 1,
                        width: area.width,
                        height: 1,
                    };
                    f.render_widget(Paragraph::new(notice.text.as_str()), bar);
                }
            }
        })?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    let action = app.on_key(key);
                    if action == Action::Quit {
                        return Ok(ExitReason::Quit);
                    }
                    handle_action(app, action, crate::collect::hooksink::now_ms());
                }
            }
        }

        if drain_snapshots(app, rx) == PollOutcome::CollectorGone {
            return Ok(ExitReason::CollectorGone);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::model::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// 테이블 렌더 테스트의 공통 베이스 세션. Task 12/13도 이 헬퍼를 그대로 쓴다.
    pub(crate) fn sample_session() -> crate::model::Session {
        Session {
            key: SessionKey {
                provider: Provider::Claude,
                uuid: "u0".into(),
            },
            state: State::Idle,
            source: Source::Layer0Inferred,
            confidence: Confidence::Medium,
            last_change_ms: 1_000_000,
            started_at_ms: Some(0),
            cwd: Some("/home/dev/project-0".into()),
            title: Some("context window budget".into()),
            entrypoint: Some("cli".into()),
            model: Some("claude-opus-5".into()),
            ctx_tokens: Some(63_000),
            ctx_window: Some(200_000),
            cpu: Some(1.5),
            pid: Some(100),
        }
    }

    fn snap_with(states: &[State]) -> crate::json::Snapshot {
        let sessions = states
            .iter()
            .enumerate()
            .map(|(i, st)| {
                let mut s = sample_session();
                s.key.uuid = format!("u{i}");
                s.state = *st;
                s.cwd = Some(format!("/home/dev/project-{i}"));
                s.pid = Some(100 + i as i32);
                s
            })
            .collect();
        crate::json::Snapshot {
            sessions,
            hooks_installed: false,
            cmux_linked: false,
            generated_at_ms: 1_060_000,
        }
    }

    #[test]
    fn age_formats_compactly() {
        assert_eq!(format_age(2_000), "2s");
        assert_eq!(format_age(252_000), "4m12s");
        assert_eq!(format_age(5 * 3_600_000), "5h");
        assert_eq!(format_age(5 * 86_400_000 + 2 * 3_600_000), "5d02h");
    }

    #[test]
    fn waiting_rows_are_red_and_running_rows_are_green() {
        assert_eq!(theme::color_for(State::WaitingApproval), theme::RED);
        assert_eq!(theme::color_for(State::WaitingInput), theme::RED);
        assert_eq!(theme::color_for(State::RunningTool), theme::GREEN);
        assert_eq!(theme::color_for(State::Idle), theme::PLAIN);
        assert_eq!(theme::color_for(State::Stale), theme::DIM);
    }

    #[test]
    fn low_confidence_waiting_uses_dim_red() {
        assert_eq!(
            theme::color_for_row(State::WaitingApproval, Confidence::Low),
            theme::DIM_RED
        );
        assert_eq!(
            theme::color_for_row(State::WaitingApproval, Confidence::Fact),
            theme::RED
        );
    }

    #[test]
    fn render_snapshot_contains_overview_and_rows() {
        let backend = TestBackend::new(90, 14);
        let mut term = Terminal::new(backend).expect("terminal");
        let snap = snap_with(&[State::WaitingApproval, State::RunningTool, State::Idle]);
        term.draw(|f| render(f, &snap, 0, false)).expect("draw");
        let text = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(text.contains("Waiting 1"));
        assert!(text.contains("Running 1"));
        assert!(text.contains("hooks off"));
        assert!(text.contains("project-0"));
    }

    /// CTX 열은 `100% !`까지 6칸을 담아야 한다. 열이 좁으면 50%와 100%가 똑같이
    /// 잘려 보여 컨텍스트 압박을 읽을 수 없다.
    #[test]
    fn ctx_column_is_not_truncated() {
        let backend = TestBackend::new(100, 10);
        let mut term = Terminal::new(backend).expect("terminal");
        let mut full = sample_session();
        full.ctx_tokens = Some(200_000);
        full.ctx_window = Some(200_000);
        let mut half = sample_session();
        half.key.uuid = "u1".into();
        half.ctx_tokens = Some(100_000);
        half.ctx_window = Some(200_000);

        let snap = crate::json::Snapshot {
            sessions: vec![full, half],
            hooks_installed: false,
            cmux_linked: false,
            generated_at_ms: 1_060_000,
        };
        term.draw(|f| render(f, &snap, 0, false)).expect("draw");
        let text = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();

        assert!(text.contains("100% !"), "100% 행이 잘렸다");
        assert!(text.contains(" 50%"), "50% 행이 잘렸다");
    }

    /// 상태 열은 색을 구분하지 않고도 읽혀야 한다 - `WI`/`RI` 같은 약어로는
    /// 처음 보는 사람이 대기와 실행을 구분할 수 없다.
    #[test]
    fn state_column_spells_the_state_out() {
        // stale은 기본 목록에서 접히므로 펼친 화면으로 확인한다.
        let text = text_of_with(
            100,
            12,
            &snap_with(&[
                State::WaitingInput,
                State::WaitingApproval,
                State::RunningInference,
                State::RunningTool,
                State::Stale,
            ]),
            true,
        );
        for label in ["waiting", "approval", "thinking", "tool", "stale"] {
            assert!(text.contains(label), "STATE 열에 {label}이 없다");
        }
    }

    /// 같은 PROJECT 안에 세션이 여러 개일 때 서로를 구분해 주는 것은 이름뿐이다.
    #[test]
    fn name_column_shows_the_session_title() {
        let mut named = sample_session();
        named.title = Some("deploy gate".into());
        let mut unnamed = sample_session();
        unnamed.key.uuid = "u1".into();
        unnamed.title = None;
        let text = text_of(110, 10, &snap_of(vec![named, unnamed]));
        assert!(text.contains("NAME"), "NAME 헤더가 없다");
        assert!(text.contains("deploy gate"), "세션 이름이 잘렸다");
    }

    /// 세션 이름은 사용자가 쓰는 언어로 붙는다 - 한글처럼 두 칸을 차지하는 글자가
    /// 섞이면 셀 폭 계산이 글자 수로 되어 있을 때 표 오른쪽 테두리가 밀린다.
    #[test]
    fn a_wide_character_name_keeps_the_table_frame_intact() {
        let (w, h) = (104u16, 8u16);
        let mut s = sample_session();
        s.title = Some("컨텍스트 윈도우 예산 점검".into());
        let backend = TestBackend::new(w, h);
        let mut term = Terminal::new(backend).expect("terminal");
        term.draw(|f| render(f, &snap_of(vec![s]), 0, false))
            .expect("draw");
        let buf = term.backend().buffer().clone();
        // 헤더와 본문 줄은 마지막 칸이 세로 테두리여야 한다(위아래 테두리 줄은
        // 모서리 문자라 제외). 폭 계산이 틀리면 이 칸이 이름의 마지막 글자로 덮인다.
        for y in 4..h - 2 {
            assert_eq!(
                buf[(w - 1, y)].symbol(),
                "\u{2502}",
                "{y}번째 줄에서 표 오른쪽 테두리가 밀렸다"
            );
        }
    }

    /// worktree 디렉터리 이름은 앞부분이 겹친다 - `docs-...`끼리 나란히 잘리면
    /// 어느 행이 어느 worktree인지 구분이 안 되어 열이 있으나 마나 해진다.
    #[test]
    fn project_column_grows_to_fit_long_worktree_names() {
        let long = "docs-observability-runbook-rewrite";
        let mut a = sample_session();
        a.cwd = Some(format!("/home/dev/repo/{long}"));
        let mut b = sample_session();
        b.key.uuid = "u1".into();
        b.cwd = Some("/home/dev/repo/feat-gateway-timeout-retry".into());

        let text = text_of(160, 10, &snap_of(vec![a, b]));
        assert!(text.contains(long), "긴 worktree 이름이 잘렸다");
        assert!(
            text.contains("feat-gateway-timeout-retry"),
            "두 번째 행도 잘렸다"
        );
    }

    /// 반대쪽 - 유난히 긴 이름 하나가 NAME을 다 먹어 버리면 정작 세션을 구분해 주는
    /// 열이 사라진다. 상한에서 끊는다.
    #[test]
    fn one_absurd_project_name_does_not_eat_the_name_column() {
        let mut s = sample_session();
        s.cwd = Some(format!("/home/dev/{}", "x".repeat(120)));
        s.title = Some("still visible".into());
        let text = text_of(160, 10, &snap_of(vec![s]));
        assert!(text.contains("still visible"), "NAME 열이 밀려났다");
    }

    /// 보안 리뷰 서브에이전트처럼 SDK가 띄운 세션은 누구도 앞에 앉아 있지 않다 -
    /// 나를 기다릴 수가 없으므로 기본 목록에서 접는다.
    #[test]
    fn sdk_driven_sessions_are_folded_away() {
        let mut human = sample_session();
        human.title = Some("checkout flake".into());
        human.entrypoint = Some("cli".into());
        let mut robot = sample_session();
        robot.key.uuid = "u1".into();
        robot.title = Some("adf module security review".into());
        robot.entrypoint = Some("sdk-py".into());
        // entrypoint를 아직 못 본 세션은 접지 않는다 - 모르면 사람 것으로 본다.
        let mut unknown_driver = sample_session();
        unknown_driver.key.uuid = "u2".into();
        unknown_driver.title = Some("codex session".into());
        unknown_driver.entrypoint = None;

        let snap = snap_of(vec![human, robot, unknown_driver]);
        let folded = text_of(110, 12, &snap);
        assert!(folded.contains("checkout flake"), "사람 세션은 남아야 한다");
        assert!(
            folded.contains("codex session"),
            "모르는 세션을 접으면 안 된다"
        );
        assert!(
            !folded.contains("security review"),
            "SDK 세션이 접히지 않았다"
        );
        assert!(folded.contains("a show 1 folded"));

        let opened = text_of_with(110, 12, &snap, true);
        assert!(opened.contains("security review"), "펼치면 보여야 한다");
    }

    /// 어느 에이전트의 세션인지 한눈에 갈라져야 한다. 모델 이름으로 짐작하게 두면
    /// 모델을 아직 못 읽은 행에서는 그 짐작마저 불가능하다.
    #[test]
    fn agent_column_separates_claude_from_codex() {
        let mut claude = sample_session();
        claude.key.provider = Provider::Claude;
        let mut codex = sample_session();
        codex.key.uuid = "u1".into();
        codex.key.provider = Provider::Codex;
        codex.model = None;

        let text = text_of(110, 10, &snap_of(vec![claude, codex]));
        assert!(text.contains("AGENT"), "AGENT 헤더가 없다");
        assert!(text.contains("claude"), "claude 라벨이 없다");
        assert!(text.contains("codex"), "codex 라벨이 없다");
    }

    /// 하루 넘게 조용한 세션은 "지금 누가 나를 기다리는가"를 보는 화면에서 자리만
    /// 차지한다. 기본 목록에서 접되, 몇 개를 접었는지는 푸터가 말해 준다.
    #[test]
    fn dormant_sessions_are_folded_away_but_still_counted() {
        let snap = snap_with(&[State::WaitingInput, State::Stale, State::Stale, State::Dead]);
        let folded = text_of(100, 12, &snap);
        assert!(folded.contains("waiting"), "살아있는 행은 남아야 한다");
        assert!(!folded.contains("stale "), "stale 행이 접히지 않았다");
        assert!(!folded.contains("dead"), "dead 행이 접히지 않았다");
        assert!(
            folded.contains("a show 3 folded"),
            "몇 개를 접었는지 알려야 한다"
        );
        // 오버뷰는 접힌 행도 계속 센다 - 숨긴다고 없는 셈 치지 않는다.
        assert!(
            folded.contains("Stale 3"),
            "오버뷰 카운터에서도 사라지면 안 된다"
        );

        let opened = text_of_with(100, 12, &snap, true);
        assert!(opened.contains("stale"), "펼치면 보여야 한다");
        assert!(opened.contains("a fold back"));
    }

    /// 접을 게 없으면 `a`를 광고하지 않는다 - 할 일 없는 키는 안내하지 않는다.
    #[test]
    fn the_toggle_is_not_advertised_when_nothing_is_folded() {
        let text = text_of(100, 12, &snap_with(&[State::WaitingInput]));
        assert!(!text.contains("folded"));
    }

    /// JUMP 열은 제거됐다 - 터미널로 옮겨가는 대신 resume 명령을 복사한다.
    #[test]
    fn table_has_no_jump_column() {
        let text = text_of(100, 12, &snap_with(&[State::Idle]));
        assert!(!text.contains("JUMP"), "JUMP 열이 아직 남아 있다");
    }

    fn text_of(width: u16, height: u16, snap: &crate::json::Snapshot) -> String {
        text_of_with(width, height, snap, false)
    }

    fn text_of_with(
        width: u16,
        height: u16,
        snap: &crate::json::Snapshot,
        show_all: bool,
    ) -> String {
        let backend = TestBackend::new(width, height);
        let mut term = Terminal::new(backend).expect("terminal");
        term.draw(|f| render(f, snap, 0, show_all)).expect("draw");
        term.backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>()
    }

    fn snap_of(sessions: Vec<Session>) -> crate::json::Snapshot {
        crate::json::Snapshot {
            sessions,
            hooks_installed: false,
            cmux_linked: false,
            generated_at_ms: 1_060_000,
        }
    }

    /// 처음 쓰는 사람이 나가는 법을 화면에서 알 수 있어야 한다. 동작하지 않는
    /// `x`(kill)와 `s`(sort)는 광고하지 않는다.
    #[test]
    fn footer_lists_only_the_keys_that_work() {
        let text = text_of(90, 14, &snap_with(&[State::Idle]));
        for hint in ["enter/r copy resume", "j/k move", "q quit"] {
            assert!(text.contains(hint), "푸터에 {hint}가 없다");
        }
        assert!(
            !text.contains("x kill"),
            "동작하지 않는 x를 광고하면 안 된다"
        );
        assert!(
            !text.contains("s sort"),
            "동작하지 않는 s를 광고하면 안 된다"
        );
    }

    /// 푸터가 생겨도 좁은 터미널에서 패닉하지 않아야 한다.
    #[test]
    fn render_survives_a_tiny_terminal() {
        let snap = snap_with(&[State::WaitingApproval, State::Idle]);
        for (w, h) in [(20, 5), (20, 4), (20, 1), (1, 1), (80, 3)] {
            let _ = text_of(w, h, &snap);
        }
    }

    /// 컨텍스트를 못 읽은 세션은 0%가 아니다. 프로그램이 모르는 수치를 단언하지 않는다.
    #[test]
    fn ctx_column_shows_a_dash_when_context_is_unknown() {
        let mut s = sample_session();
        s.ctx_tokens = None;
        s.ctx_window = None;
        let text = text_of(100, 10, &snap_of(vec![s]));
        assert!(!text.contains(" 0%"), "모르는 값을 0%라고 단언하면 안 된다");
    }

    /// 스펙 6절: 85%를 넘으면 사용률 뒤에 `!`를 붙인다. compaction 임박 세션을
    /// 찾는 것이 이 도구의 2순위 목표다.
    #[test]
    fn ctx_over_85_pct_gets_a_bang_marker() {
        let mut hot = sample_session();
        hot.ctx_tokens = Some(180_000);
        hot.ctx_window = Some(200_000);
        let text = text_of(100, 10, &snap_of(vec![hot]));
        assert!(text.contains("90% !"), "85% 초과 행에 ! 표시가 없다");

        let mut warm = sample_session();
        warm.ctx_tokens = Some(160_000);
        warm.ctx_window = Some(200_000);
        let text = text_of(100, 10, &snap_of(vec![warm]));
        assert!(text.contains("80%"));
        assert!(!text.contains("80% !"), "85% 이하에는 ! 표시가 없다");
    }

    /// 스펙 6절: 오버뷰에 85% 초과 세션 수를 센다.
    #[test]
    fn overview_counts_sessions_over_85_pct() {
        let hot = |tokens: u64, uuid: &str| {
            let mut s = sample_session();
            s.key.uuid = uuid.into();
            s.ctx_tokens = Some(tokens);
            s.ctx_window = Some(200_000);
            s
        };
        let text = text_of(
            100,
            12,
            &snap_of(vec![
                hot(180_000, "a"),
                hot(190_000, "b"),
                hot(20_000, "c"),
                {
                    let mut s = sample_session();
                    s.key.uuid = "d".into();
                    s.ctx_tokens = None;
                    s.ctx_window = None;
                    s
                },
            ]),
        );
        assert!(
            text.contains("ctx over 85%: 2"),
            "오버뷰에 85% 초과 카운터가 없다"
        );
    }

    /// README 첫 화면 블록을 실제 렌더러로 찍어 준다. 손으로 그린 표는 열 폭이
    /// 바뀔 때마다 조용히 거짓이 되므로, README를 고칠 때는 이 테스트의 출력을
    /// 그대로 붙여넣는다. 일반 `cargo test`에서는 돌지 않는다 -
    /// `cargo test -- --ignored --nocapture readme_frame`으로 실행한다.
    #[test]
    #[ignore]
    fn readme_frame() {
        struct Sample {
            state: State,
            last: i64,
            started: i64,
            tokens: u64,
            cpu: f32,
            model: &'static str,
            project: &'static str,
            name: &'static str,
            provider: crate::model::Provider,
        }
        let samples = [
            Sample {
                state: State::WaitingInput,
                last: 3_000,
                started: 2_460_000,
                tokens: 36_000,
                cpu: 0.0,
                model: "claude-sonnet-5",
                project: "kestrel-web",
                name: "checkout flake",
                provider: crate::model::Provider::Claude,
            },
            Sample {
                state: State::WaitingApproval,
                last: 0,
                started: 840_000,
                tokens: 124_000,
                cpu: 0.0,
                model: "claude-opus-5",
                project: "harbor-api",
                name: "rate limit rollout",
                provider: crate::model::Provider::Claude,
            },
            Sample {
                state: State::RunningTool,
                last: 1_000,
                started: 300_000,
                tokens: 88_000,
                cpu: 38.2,
                model: "gpt-6-astra",
                project: "meridian-cli",
                name: "-",
                provider: crate::model::Provider::Codex,
            },
            Sample {
                state: State::Idle,
                last: 1_320_000,
                started: 7_200_000,
                tokens: 176_000,
                cpu: 0.0,
                model: "claude-opus-5",
                project: "driftwood-infra",
                name: "vpc peering audit",
                provider: crate::model::Provider::Claude,
            },
        ];
        let sessions = samples
            .iter()
            .enumerate()
            .map(|(i, sample)| {
                let mut s = sample_session();
                s.key.uuid = format!("u{i}");
                s.key.provider = sample.provider;
                s.state = sample.state;
                s.last_change_ms = 1_060_000 - sample.last;
                s.started_at_ms = Some(1_060_000 - sample.started);
                s.ctx_tokens = Some(sample.tokens);
                s.ctx_window = Some(200_000);
                s.cpu = Some(sample.cpu);
                s.model = Some(sample.model.into());
                s.cwd = Some(format!("/home/dev/{}", sample.project));
                s.title = (sample.name != "-").then(|| sample.name.to_string());
                s
            })
            .collect();
        let snap = crate::json::Snapshot {
            sessions,
            hooks_installed: true,
            cmux_linked: true,
            generated_at_ms: 1_060_000,
        };

        let (w, h) = (104, 11);
        let backend = TestBackend::new(w, h);
        let mut term = Terminal::new(backend).expect("terminal");
        term.draw(|f| render(f, &snap, usize::MAX, false))
            .expect("draw");
        let buf = term.backend().buffer().clone();
        for y in 0..h {
            let line: String = (0..w).map(|x| buf[(x, y)].symbol()).collect();
            println!("{}", line.trim_end());
        }
    }

    #[test]
    fn degrade_flags_flip_when_sources_are_live() {
        let backend = TestBackend::new(90, 14);
        let mut term = Terminal::new(backend).expect("terminal");
        let mut snap = snap_with(&[State::Idle]);
        snap.hooks_installed = true;
        snap.cmux_linked = true;
        term.draw(|f| render(f, &snap, 0, false)).expect("draw");
        let text = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(text.contains("hooks on"));
        assert!(text.contains("cmux linked"));
    }
}

#[cfg(test)]
mod loop_tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn app_with(n: usize) -> App {
        App::new(crate::json::Snapshot {
            sessions: (0..n).map(|_| super::tests::sample_session()).collect(),
            hooks_installed: false,
            cmux_linked: false,
            generated_at_ms: 0,
        })
    }

    #[test]
    fn j_and_k_move_selection_within_bounds() {
        let mut app = app_with(3);
        assert_eq!(app.selected, 0);
        app.on_key(KeyCode::Char('j').into());
        assert_eq!(app.selected, 1);
        app.on_key(KeyCode::Char('k').into());
        app.on_key(KeyCode::Char('k').into());
        assert_eq!(app.selected, 0);
        for _ in 0..10 {
            app.on_key(KeyCode::Char('j').into());
        }
        assert_eq!(app.selected, 2);
    }

    /// raw mode에서 ctrl-c는 SIGINT가 아니라 키 이벤트로 도착한다. 처리하지 않으면
    /// 화면이 그대로 멈춰 있는 것처럼 보이고, 사용자는 터미널을 닫는 수밖에 없다.
    #[test]
    fn ctrl_c_quits_even_with_no_sessions() {
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        for n in [0, 2] {
            let mut app = app_with(n);
            assert_eq!(app.on_key(ctrl('c')), Action::Quit, "세션 {n}개에서 ctrl-c");
            assert_eq!(app.on_key(ctrl('d')), Action::Quit, "세션 {n}개에서 ctrl-d");
        }
    }

    /// ctrl 없이 누른 `c`는 종료가 아니다 - 수식키를 흘려 보면 안 된다.
    #[test]
    fn a_bare_c_is_not_quit() {
        let mut app = app_with(2);
        assert_eq!(app.on_key(KeyCode::Char('c').into()), Action::None);
    }

    /// 복사 확인은 읽고 나면 볼 일이 없다. 사라지지 않으면 그 자리의 키 안내를
    /// 계속 가린 채 남는다.
    #[test]
    fn a_copy_notice_expires_on_its_own() {
        const NOW: i64 = 1_800_000_000_000;
        let mut app = app_with(2);
        super::handle_action(&mut app, Action::CopyResume(0), NOW);
        let notice = app.message.clone().expect("복사 후 알림이 떠야 한다");
        assert!(notice.text.contains("--resume"));
        assert_eq!(notice.until_ms, NOW + super::NOTICE_MS);

        app.expire_message(NOW + super::NOTICE_MS - 1);
        assert!(app.message.is_some(), "만료 전에는 남아 있어야 한다");

        app.expire_message(NOW + super::NOTICE_MS);
        assert!(app.message.is_none(), "만료 후에는 사라져야 한다");
    }

    /// `a`로 펼쳤다 접을 때 커서가 안 보이는 줄에 남으면 안 된다.
    #[test]
    fn toggling_all_keeps_the_cursor_in_range() {
        let mut app = App::new(crate::json::Snapshot {
            sessions: vec![
                {
                    let mut s = super::tests::sample_session();
                    s.key.uuid = "live".into();
                    s
                },
                {
                    let mut s = super::tests::sample_session();
                    s.key.uuid = "old".into();
                    s.state = crate::model::State::Stale;
                    s
                },
            ],
            hooks_installed: false,
            cmux_linked: false,
            generated_at_ms: 0,
        });
        assert_eq!(app.visible().len(), 1, "기본은 잠든 행을 접는다");

        app.on_key(KeyCode::Char('a').into());
        assert_eq!(app.visible().len(), 2);
        app.on_key(KeyCode::Char('j').into());
        assert_eq!(app.selected, 1);

        app.on_key(KeyCode::Char('a').into());
        assert_eq!(app.visible().len(), 1);
        assert_eq!(app.selected, 0, "접었는데 커서가 범위를 벗어났다");
    }

    #[test]
    fn q_quits() {
        let mut app = app_with(1);
        assert_eq!(app.on_key(KeyCode::Char('q').into()), Action::Quit);
    }

    /// enter는 선택한 행의 resume 명령 복사를 요청한다. 세션으로 옮겨가는 대신
    /// 명령을 손에 쥐여 주는 것이 이 도구가 터미널에 중립인 방식이다.
    #[test]
    fn enter_requests_copy_resume_for_selected_row() {
        let mut app = app_with(2);
        app.on_key(KeyCode::Char('j').into());
        assert_eq!(app.on_key(KeyCode::Enter.into()), Action::CopyResume(1));
    }

    #[test]
    fn keys_on_empty_list_do_not_panic() {
        let mut app = app_with(0);
        assert_eq!(app.on_key(KeyCode::Enter.into()), Action::None);
        assert_eq!(app.on_key(KeyCode::Char('j').into()), Action::None);
    }

    #[test]
    fn new_snapshot_keeps_selection_on_same_session() {
        let mut app = app_with(3);
        app.on_key(KeyCode::Char('j').into());
        let selected_uuid = app.selected_key().map(|k| k.uuid.clone());
        let mut snap = app.snapshot.clone();
        snap.sessions.rotate_left(1);
        app.apply(snap);
        assert_eq!(app.selected_key().map(|k| k.uuid.clone()), selected_uuid);
    }

    /// 수집 스레드가 정상 종료했든 패닉으로 죽었든, 송신자가 드롭되면 채널은
    /// Disconnected를 돌려준다. `drain_snapshots`는 이 둘을 구분하지 않고 똑같이
    /// "더 이상 새 데이터가 오지 않는다"로 취급해야 한다 - 이 테스트는 실제
    /// 스레드나 타이밍 없이, 송신자를 직접 drop해서 그 경로만 헤드리스로 확인한다.
    #[test]
    fn drain_snapshots_signals_exit_when_the_sender_is_dropped() {
        let (tx, rx) = mpsc::channel::<crate::json::Snapshot>();
        drop(tx);
        let mut app = app_with(1);
        assert_eq!(
            super::drain_snapshots(&mut app, &rx),
            PollOutcome::CollectorGone
        );
    }

    #[test]
    fn drain_snapshots_continues_when_the_channel_is_merely_empty() {
        let (_tx, rx) = mpsc::channel::<crate::json::Snapshot>();
        let mut app = app_with(1);
        assert_eq!(super::drain_snapshots(&mut app, &rx), PollOutcome::Continue);
    }

    #[test]
    fn r_requests_copy_resume_and_x_requests_kill_but_neither_mutates_state() {
        let mut app = app_with(2);
        assert_eq!(app.on_key(KeyCode::Char('r').into()), Action::CopyResume(0));
        assert_eq!(app.on_key(KeyCode::Char('x').into()), Action::Kill(0));
        assert_eq!(app.on_key(KeyCode::Char('s').into()), Action::ToggleSort);
        // 이 task에서는 세 Action 모두 inert 하다 - 선택이나 스냅샷을 바꾸지 않는다.
        assert_eq!(app.selected, 0);
        assert_eq!(app.snapshot.sessions.len(), 2);
    }

    /// Task 10의 Collector가 실제로 만든 Snapshot이 Task 11의 render를 그대로
    /// 통과하는지 확인한다 - 두 task가 이 task에서 실제로 이어붙는지 증명하는 것이
    /// 목적이라 App을 거치지 않고 render를 직접 호출한다.
    #[test]
    fn a_real_collector_snapshot_renders_through_the_real_pipeline() {
        struct FakeProcs(Vec<crate::collect::proc::ProcInfo>);
        impl crate::collect::proc::ProcessSource for FakeProcs {
            fn list_agents(&mut self) -> Vec<crate::collect::proc::ProcInfo> {
                self.0.clone()
            }
        }

        let dir = tempfile::tempdir().expect("dir");
        let proc = crate::collect::proc::ProcInfo {
            pid: 123,
            provider: crate::model::Provider::Claude,
            session_id: Some("u1".into()),
            cwd: Some(std::path::PathBuf::from("/home/dev/app")),
            cpu: 0.0,
            started_at_ms: 0,
        };
        let mut collector = crate::collect::Collector::new(
            Box::new(FakeProcs(vec![proc])),
            crate::config::Thresholds::default(),
        )
        .with_projects_root(dir.path().to_path_buf());
        let snap = collector.snapshot(1_000);
        assert_eq!(snap.sessions.len(), 1);

        let backend = ratatui::backend::TestBackend::new(90, 14);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| render(f, &snap, 0, false)).expect("draw");
        let text = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(text.contains("app"));
    }
}
