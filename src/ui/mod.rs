//! TUI 렌더링. 위쪽 Overview 블록과 아래쪽 세션 테이블을 그린다.
//! 색상 판단은 [`theme`]에만 두고, 여기와 하위 모듈은 그 결과만 쓴다.

mod overview;
mod table;
pub(crate) mod theme;

use crate::json::Snapshot;
use ratatui::layout::{Constraint, Layout};
use ratatui::Frame;

#[allow(dead_code)]
pub fn render(frame: &mut Frame, snap: &Snapshot, selected: usize) {
    let area = frame.area();
    let chunks = Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).split(area);
    overview::render(frame, chunks[0], snap);
    table::render(frame, chunks[1], snap, selected);
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
