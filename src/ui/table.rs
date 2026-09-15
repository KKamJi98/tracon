//! 세션 테이블: AGENT STATE LAST DUR CTX% CPU% MODEL PROJECT NAME.
//! 행 색상은 [`crate::ui::theme`]에만 위임하고, 여기서는 색을 직접 고르지 않는다.

use crate::model::Session;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::Modifier;
use ratatui::widgets::{Block, Borders, Cell, Row, Table};
use ratatui::Frame;

#[allow(dead_code)]
pub(crate) fn render(
    frame: &mut Frame,
    area: Rect,
    sessions: &[&Session],
    now_ms: i64,
    selected: usize,
) {
    let header = Row::new(vec![
        "AGENT", "STATE", "LAST", "DUR", "CTX%", "CPU%", "MODEL", "PROJECT", "NAME",
    ])
    .style(ratatui::style::Style::new().add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = sessions
        .iter()
        .enumerate()
        .map(|(i, s)| row_for(s, now_ms, i == selected))
        .collect();

    // STATE는 가장 긴 라벨(`approval`, `thinking`)이 8자다. CTX는 `100% !`까지
    // 6칸을 담아야 한다 - 더 좁으면 50%와 100%가 똑같이 잘려 보여 컨텍스트 압박을
    // 읽을 수 없다. PROJECT는 이번 화면의 실제 값에 맞춘다(아래 참조).
    // NAME만 남는 폭을 받는다 - 세션 이름은 길이가 제각각이고, 같은 PROJECT 안의
    // 세션들을 구분해 주는 유일한 열이라 가장 넓어야 한다.
    let widths = [
        Constraint::Length(6),
        Constraint::Length(8),
        Constraint::Length(6),
        Constraint::Length(6),
        Constraint::Length(6),
        Constraint::Length(5),
        Constraint::Length(16),
        Constraint::Length(project_width(sessions)),
        Constraint::Min(16),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title("sessions"));

    frame.render_widget(table, area);
}

/// PROJECT 열은 고정폭으로 두면 worktree 디렉터리에서 바로 잘린다 -
/// `docs-observability-runbook-rewrite`처럼 앞부분이 겹치는 이름들이라, 잘리면
/// 서로 구분이 안 되어 열이 있으나 마나 해진다. 이번 화면에 실제로 오른 값에
/// 맞춰 폭을 잡는다.
///
/// 상한이 필요한 이유는 반대쪽이다 - 유난히 긴 이름 하나가 NAME을 다 먹어 버리면
/// 정작 세션을 구분해 주는 열이 사라진다. 하한은 헤더 글자 수다.
const PROJECT_MIN: u16 = 7;
const PROJECT_MAX: u16 = 34;

#[allow(dead_code)]
fn project_width(sessions: &[&Session]) -> u16 {
    let widest = sessions
        .iter()
        .map(|s| ratatui::text::Span::raw(project_name(s.cwd.as_deref())).width() as u16)
        .max()
        .unwrap_or(0);
    widest.clamp(PROJECT_MIN, PROJECT_MAX)
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

/// 어느 에이전트의 세션인지. 모델 이름으로도 짐작은 되지만("claude-opus-5" vs
/// "gpt-6-astra"), 짐작하게 두지 않는다 - 모델을 아직 못 읽은 세션은 MODEL이 `-`라
/// 그 짐작마저 불가능하다.
#[allow(dead_code)]
fn agent_label(provider: crate::model::Provider) -> &'static str {
    match provider {
        crate::model::Provider::Claude => "claude",
        crate::model::Provider::Codex => "codex",
    }
}

/// 상태를 그대로 읽히는 단어로 적는다. `WI`/`RI` 같은 2글자 약어는 색을 구분하기
/// 어려운 환경에서도, 처음 보는 사람에게도 아무것도 알려주지 않는다.
#[allow(dead_code)]
fn state_label(state: crate::model::State) -> &'static str {
    use crate::model::State::*;
    match state {
        WaitingApproval => "approval",
        WaitingInput => "waiting",
        RunningInference => "thinking",
        RunningTool => "tool",
        Idle => "idle",
        Unknown => "unknown",
        Stale => "stale",
        Dead => "dead",
    }
}

#[allow(dead_code)]
fn row_for(session: &Session, now_ms: i64, selected: bool) -> Row<'static> {
    let last = crate::ui::format_age(now_ms.saturating_sub(session.last_change_ms));
    let dur = session
        .started_at_ms
        .map(|started| crate::ui::format_age(now_ms.saturating_sub(started)))
        .unwrap_or_else(|| "-".to_string());
    // 컨텍스트를 못 읽은 세션은 0%가 아니라 미지수이므로 수치 대신 `-`를 그린다.
    let ctx = match session.ctx_pct() {
        Some(pct) => format!(
            "{pct:>3}%{}",
            if crate::json::is_ctx_pressure(pct) {
                " !"
            } else {
                ""
            }
        ),
        None => "-".to_string(),
    };
    let cpu = session
        .cpu
        .map(|c| format!("{c:.1}"))
        .unwrap_or_else(|| "-".to_string());
    let model = session.model.clone().unwrap_or_else(|| "-".to_string());
    let project = project_name(session.cwd.as_deref());
    // 이름이 없는 세션(codex, 또는 아직 이름이 안 붙은 claude 세션)은 `-`다.
    // 대화 내용에서 이름을 지어내지 않는다.
    let name = session.title.clone().unwrap_or_else(|| "-".to_string());

    let mut style = crate::ui::theme::color_for_row(session.state, session.confidence);
    if selected {
        style = style.add_modifier(Modifier::REVERSED);
    }

    Row::new(vec![
        Cell::from(agent_label(session.key.provider)),
        Cell::from(state_label(session.state)),
        Cell::from(last),
        Cell::from(dur),
        Cell::from(ctx),
        Cell::from(cpu),
        Cell::from(model),
        Cell::from(project),
        Cell::from(name),
    ])
    .style(style)
}
