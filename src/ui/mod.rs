//! TUI 렌더링. 위쪽 Overview 블록과 아래쪽 세션 테이블을 그린다.
//! 색상 판단은 [`theme`]에만 두고, 여기와 하위 모듈은 그 결과만 쓴다.

mod overview;
mod table;
pub(crate) mod theme;

use crate::json::Snapshot;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
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
pub(crate) const KEY_HINTS: &str = "enter jump   r copy resume   j/k move   q quit";

#[allow(dead_code)]
pub fn render(frame: &mut Frame, snap: &Snapshot, selected: usize) {
    let area = frame.area();
    // 푸터는 고정 1줄, 테이블이 남는 높이를 받는다. 화면이 아주 낮으면 테이블 쪽이
    // 0줄로 줄어들 뿐 렌더는 계속 성립한다.
    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(area);
    overview::render(frame, chunks[0], snap);
    table::render(frame, chunks[1], snap, selected);
    frame.render_widget(Paragraph::new(KEY_HINTS), chunks[2]);
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

/// 7칸 고정폭 컨텍스트 사용률 바.
#[allow(dead_code)]
pub fn ctx_bar(pct: u32) -> String {
    let filled = ((pct.min(100) as f32 / 100.0) * 7.0).round() as usize;
    let mut bar = String::new();
    for i in 0..7 {
        bar.push(if i < filled { '#' } else { '.' });
    }
    bar
}

/// 키 입력이 요청하는 동작. `on_key`는 순수하게 동작을 반환만 하고, 실제 실행은
/// `handle_action`이 한다 - 헤드리스로 테스트하기 위해서다. `Kill`은 프로세스를
/// 죽이는 범위가 이 task 밖이라 여전히 inert 하다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    Quit,
    Jump(usize),
    CopyResume(usize),
    Kill(usize),
    ToggleSort,
}

/// TUI의 순수 상태. 렌더링·IO와 분리해 두어야 `on_key`/`apply`를 헤드리스로
/// 테스트할 수 있다.
pub struct App {
    pub snapshot: Snapshot,
    pub selected: usize,
    /// 클립보드 복사가 전부 실패했을 때 대신 보여줄 명령 문자열. 상태줄에 한 줄만
    /// 띄우고, 다음 Jump/CopyResume 액션이 오면 덮어쓴다.
    pub message: Option<String>,
}

impl App {
    pub fn new(snapshot: Snapshot) -> Self {
        Self {
            snapshot,
            selected: 0,
            message: None,
        }
    }

    pub fn selected_key(&self) -> Option<&crate::model::SessionKey> {
        self.snapshot.sessions.get(self.selected).map(|s| &s.key)
    }

    /// 새 스냅샷을 받아도 사용자가 보던 세션에 선택을 유지한다.
    pub fn apply(&mut self, snapshot: Snapshot) {
        let prev = self.selected_key().cloned();
        self.snapshot = snapshot;
        self.selected = prev
            .and_then(|k| self.snapshot.sessions.iter().position(|s| s.key == k))
            .unwrap_or(0);
    }

    pub fn on_key(&mut self, code: KeyCode) -> Action {
        let len = self.snapshot.sessions.len();
        if len == 0 {
            return match code {
                KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
                _ => Action::None,
            };
        }
        match code {
            KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
            KeyCode::Char('j') | KeyCode::Down => {
                self.selected = (self.selected + 1).min(len - 1);
                Action::None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                Action::None
            }
            KeyCode::Enter => Action::Jump(self.selected),
            KeyCode::Char('r') => Action::CopyResume(self.selected),
            KeyCode::Char('x') => Action::Kill(self.selected),
            KeyCode::Char('s') => Action::ToggleSort,
            _ => Action::None,
        }
    }
}

/// resume 명령을 클립보드에 복사하고, 실패하면 그 명령 문자열 자체를 상태줄에
/// 보여줄 메시지로 돌려준다.
fn copy_or_show(cmd: String) -> String {
    if crate::jump::clipboard::copy(&cmd) {
        format!("복사됨: {cmd}")
    } else {
        cmd
    }
}

/// `Action::Jump`/`Action::CopyResume`를 실제로 실행한다. `Action::Kill`과
/// `Action::ToggleSort`/`Action::None`/`Action::Quit`은 이 task 범위 밖이거나
/// 이미 `event_loop`에서 처리되어 여기서는 아무것도 하지 않는다.
fn handle_action(app: &mut App, action: Action) {
    match action {
        Action::Jump(i) => {
            let Some(session) = app.snapshot.sessions.get(i) else {
                return;
            };
            let jump_label = session.jump.clone();
            let resume_cmd = crate::jump::resume_command(session);
            // tmux 대상이 있고 실제로 그 pane까지 옮겨갔으면 끝 - 아니면(대상이
            // 없거나, 있어도 클라이언트가 안 붙어 있어 실패했으면) resume 명령
            // 복사로 폴백한다. 이게 터미널 중립성을 지키는 지점이다.
            if jump_label
                .as_deref()
                .is_some_and(|label| crate::jump::jump_to(label).is_ok())
            {
                app.message = None;
                return;
            }
            app.message = Some(copy_or_show(resume_cmd));
        }
        Action::CopyResume(i) => {
            let Some(session) = app.snapshot.sessions.get(i) else {
                return;
            };
            let resume_cmd = crate::jump::resume_command(session);
            app.message = Some(copy_or_show(resume_cmd));
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
    let cmux_jumper = cmux.is_some().then(crate::jump::cmux::CmuxJumper::new);
    let mut collector = crate::collect::Collector::new(
        Box::new(crate::collect::proc::SysProcessSource::new()),
        crate::config::Thresholds::default(),
    )
    .with_jumpers(vec![Box::new(crate::jump::tmux::TmuxJumper::new())])
    .with_cmux(cmux)
    .with_cmux_jumper(cmux_jumper);
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
        terminal.draw(|f| {
            render(f, &app.snapshot, app.selected);
            // 클립보드 폴백 메시지는 render()가 그리는 고정 레이아웃과 별개로,
            // 화면 맨 아래 한 줄에 덧그린다 - render()는 테스트가 직접 호출하는
            // 순수 함수라 시그니처를 바꾸고 싶지 않다. 그 자리는 키 안내 푸터라,
            // 메시지가 떠 있는 동안에는 안내 대신 메시지가 보인다.
            if let Some(msg) = &app.message {
                let area = f.area();
                if area.height > 0 {
                    let bar = Rect {
                        x: area.x,
                        y: area.y + area.height - 1,
                        width: area.width,
                        height: 1,
                    };
                    f.render_widget(Paragraph::new(msg.as_str()), bar);
                }
            }
        })?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    let action = app.on_key(key.code);
                    if action == Action::Quit {
                        return Ok(ExitReason::Quit);
                    }
                    handle_action(app, action);
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
            model: Some("claude-opus-5".into()),
            ctx_tokens: Some(63_000),
            ctx_window: Some(200_000),
            cpu: Some(1.5),
            pid: Some(100),
            jump: None,
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
    fn ctx_bar_has_seven_cells() {
        assert_eq!(ctx_bar(0).chars().count(), 7);
        assert_eq!(ctx_bar(100).chars().count(), 7);
        assert!(ctx_bar(100).starts_with('#'));
        assert!(ctx_bar(0).starts_with('.'));
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
        term.draw(|f| render(f, &snap, 0)).expect("draw");
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

    /// CTX 열은 7칸짜리 바에 사용률까지 담아야 하고, JUMP 열은 `tmux:main:1.2`
    /// 같은 13자 라벨을 담아야 한다. 열이 좁으면 50%와 100%가 똑같이 잘려 보이고,
    /// 점프 대상도 전부 `tmux:m`으로 뭉개져 서로 구분되지 않는다.
    #[test]
    fn ctx_and_jump_columns_are_not_truncated() {
        let backend = TestBackend::new(100, 10);
        let mut term = Terminal::new(backend).expect("terminal");
        let mut full = sample_session();
        full.ctx_tokens = Some(200_000);
        full.ctx_window = Some(200_000);
        full.jump = Some("tmux:main:1.2".into());
        let mut half = sample_session();
        half.key.uuid = "u1".into();
        half.ctx_tokens = Some(100_000);
        half.ctx_window = Some(200_000);
        half.jump = Some("cmux:12".into());

        let snap = crate::json::Snapshot {
            sessions: vec![full, half],
            hooks_installed: false,
            cmux_linked: false,
            generated_at_ms: 1_060_000,
        };
        term.draw(|f| render(f, &snap, 0)).expect("draw");
        let text = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();

        assert!(text.contains("####### 100%"), "100% 행이 잘렸다");
        assert!(text.contains("####...  50%"), "50% 행이 잘렸다");
        assert!(text.contains("tmux:main:1.2"), "JUMP 라벨이 잘렸다");
        assert!(text.contains("cmux:12"));
    }

    fn text_of(width: u16, height: u16, snap: &crate::json::Snapshot) -> String {
        let backend = TestBackend::new(width, height);
        let mut term = Terminal::new(backend).expect("terminal");
        term.draw(|f| render(f, snap, 0)).expect("draw");
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
        for hint in ["enter jump", "r copy resume", "j/k move", "q quit"] {
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
        assert!(
            !text.contains("......."),
            "컨텍스트를 모르면 0% 바를 그리지 않는다"
        );
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

    #[test]
    fn degrade_flags_flip_when_sources_are_live() {
        let backend = TestBackend::new(90, 14);
        let mut term = Terminal::new(backend).expect("terminal");
        let mut snap = snap_with(&[State::Idle]);
        snap.hooks_installed = true;
        snap.cmux_linked = true;
        term.draw(|f| render(f, &snap, 0)).expect("draw");
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
    use crossterm::event::KeyCode;

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
        app.on_key(KeyCode::Char('j'));
        assert_eq!(app.selected, 1);
        app.on_key(KeyCode::Char('k'));
        app.on_key(KeyCode::Char('k'));
        assert_eq!(app.selected, 0);
        for _ in 0..10 {
            app.on_key(KeyCode::Char('j'));
        }
        assert_eq!(app.selected, 2);
    }

    #[test]
    fn q_quits() {
        let mut app = app_with(1);
        assert_eq!(app.on_key(KeyCode::Char('q')), Action::Quit);
    }

    #[test]
    fn enter_requests_jump_for_selected_row() {
        let mut app = app_with(2);
        app.on_key(KeyCode::Char('j'));
        assert_eq!(app.on_key(KeyCode::Enter), Action::Jump(1));
    }

    #[test]
    fn keys_on_empty_list_do_not_panic() {
        let mut app = app_with(0);
        assert_eq!(app.on_key(KeyCode::Enter), Action::None);
        assert_eq!(app.on_key(KeyCode::Char('j')), Action::None);
    }

    #[test]
    fn new_snapshot_keeps_selection_on_same_session() {
        let mut app = app_with(3);
        app.on_key(KeyCode::Char('j'));
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
        assert_eq!(app.on_key(KeyCode::Char('r')), Action::CopyResume(0));
        assert_eq!(app.on_key(KeyCode::Char('x')), Action::Kill(0));
        assert_eq!(app.on_key(KeyCode::Char('s')), Action::ToggleSort);
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
            tty: None,
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
        term.draw(|f| render(f, &snap, 0)).expect("draw");
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
