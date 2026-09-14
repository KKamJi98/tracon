//! 상단 Overview 블록: 상태별 카운트와 데이터 소스 신뢰도(hooks/cmux)를 보여준다.

use crate::json::Snapshot;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

#[allow(dead_code)]
pub(crate) fn render(frame: &mut Frame, area: Rect, snap: &Snapshot) {
    let counts = snap.counts();
    let hooks = if snap.hooks_installed {
        "hooks on"
    } else {
        "hooks off"
    };
    let cmux = if snap.cmux_linked {
        "cmux linked"
    } else {
        "cmux unavailable"
    };

    let line = Line::from(vec![
        Span::raw(format!(
            "Waiting {}  Running {}  Idle {}  Stale {}",
            counts.waiting, counts.running, counts.idle, counts.stale
        )),
        Span::raw("   "),
        Span::raw(format!(
            "ctx over {}%: {}",
            crate::json::CTX_PRESSURE_PCT,
            counts.ctx_pressure
        )),
        Span::raw("   "),
        Span::raw(hooks),
        Span::raw("  "),
        Span::raw(cmux),
    ]);

    let block = Block::default().borders(Borders::ALL).title("tracon");
    let para = Paragraph::new(line).block(block);
    frame.render_widget(para, area);
}
