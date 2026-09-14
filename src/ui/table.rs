//! 세션 테이블: ST LAST DUR CTX% CPU% MODEL PROJECT JUMP.
//! 행 색상은 [`crate::ui::theme`]에만 위임하고, 여기서는 색을 직접 고르지 않는다.

use crate::json::Snapshot;
use crate::model::Session;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::Modifier;
use ratatui::widgets::{Block, Borders, Cell, Row, Table};
use ratatui::Frame;

#[allow(dead_code)]
pub(crate) fn render(frame: &mut Frame, area: Rect, snap: &Snapshot, selected: usize) {
    let header = Row::new(vec![
        "ST", "LAST", "DUR", "CTX%", "CPU%", "MODEL", "PROJECT", "JUMP",
    ])
    .style(ratatui::style::Style::new().add_modifier(Modifier::BOLD));

    let now = snap.generated_at_ms;
    let rows: Vec<Row> = snap
        .sessions
        .iter()
        .enumerate()
        .map(|(i, s)| row_for(s, now, i == selected))
        .collect();

    // CTX는 7칸 바 + 공백 + `100%`까지 12칸을 그대로 담아야 한다. 더 좁으면
    // 50%와 100%가 똑같이 잘려 보여 컨텍스트 압박을 읽을 수 없다. JUMP도
    // `tmux:main:1.2`(13자)가 들어가야 점프 대상이 서로 구분된다.
    // PROJECT만 남는 폭을 받는다.
    let widths = [
        Constraint::Length(2),
        Constraint::Length(6),
        Constraint::Length(6),
        Constraint::Length(12),
        Constraint::Length(5),
        Constraint::Length(16),
        Constraint::Min(10),
        Constraint::Length(14),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title("sessions"));

    frame.render_widget(table, area);
}

/// PROJECT 열은 `cwd`의 마지막 경로 요소만 쓴다 - 전체 경로는 실 사용자 머신의
/// 민감 정보이고, 열 폭도 넘긴다.
#[allow(dead_code)]
fn project_name(cwd: Option<&str>) -> String {
    cwd.and_then(|p| std::path::Path::new(p).file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("-")
        .to_string()
}

#[allow(dead_code)]
fn short_state(state: crate::model::State) -> &'static str {
    use crate::model::State::*;
    match state {
        WaitingApproval => "WA",
        WaitingInput => "WI",
        RunningInference => "RI",
        RunningTool => "RT",
        Idle => "ID",
        Unknown => "??",
        Stale => "ST",
        Dead => "DE",
    }
}

#[allow(dead_code)]
fn row_for(session: &Session, now_ms: i64, selected: bool) -> Row<'static> {
    let last = crate::ui::format_age(now_ms.saturating_sub(session.last_change_ms));
    let dur = session
        .started_at_ms
        .map(|started| crate::ui::format_age(now_ms.saturating_sub(started)))
        .unwrap_or_else(|| "-".to_string());
    // 바만 그리면 헤더의 `CTX%`가 약속한 수치가 빠진다 - 바는 7단계라 한 칸이
    // 14%p를 덮어서 바만으로는 압박 정도를 읽을 수 없다.
    let pct = session.ctx_pct().unwrap_or(0);
    let ctx = format!("{} {pct:>3}%", crate::ui::ctx_bar(pct));
    let cpu = session
        .cpu
        .map(|c| format!("{c:.1}"))
        .unwrap_or_else(|| "-".to_string());
    let model = session.model.clone().unwrap_or_else(|| "-".to_string());
    let project = project_name(session.cwd.as_deref());
    // JUMP은 Collector가 tmux 같은 Jumper로 대상을 찾아냈을 때만 라벨을 보여주고,
    // 그 외(tmux 밖 세션, tmux 미설치 등)에는 "-"다.
    let jump = session.jump.clone().unwrap_or_else(|| "-".to_string());

    let mut style = crate::ui::theme::color_for_row(session.state, session.confidence);
    if selected {
        style = style.add_modifier(Modifier::REVERSED);
    }

    Row::new(vec![
        Cell::from(short_state(session.state)),
        Cell::from(last),
        Cell::from(dur),
        Cell::from(ctx),
        Cell::from(cpu),
        Cell::from(model),
        Cell::from(project),
        Cell::from(jump),
    ])
    .style(style)
}
