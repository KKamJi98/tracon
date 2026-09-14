//! 3색 테마: 상태를 빨강/초록/무색/dim 넷으로 접는 단일 매핑 지점.
//! 색상 결정은 여기서만 하고, 테이블/오버뷰 코드는 이 함수들만 호출한다.

use crate::model::{Confidence, State};
use ratatui::style::{Color as TColor, Modifier, Style};

#[allow(dead_code)]
pub const RED: Style = Style::new().fg(TColor::Red);
#[allow(dead_code)]
pub const DIM_RED: Style = Style::new().fg(TColor::Red).add_modifier(Modifier::DIM);
#[allow(dead_code)]
pub const GREEN: Style = Style::new().fg(TColor::Green);
#[allow(dead_code)]
pub const PLAIN: Style = Style::new();
#[allow(dead_code)]
pub const DIM: Style = Style::new().add_modifier(Modifier::DIM);

#[allow(dead_code)]
pub fn color_for(state: State) -> Style {
    match state.color() {
        crate::model::Color::Red => RED,
        crate::model::Color::Green => GREEN,
        crate::model::Color::Plain => PLAIN,
        crate::model::Color::Dim => DIM,
    }
}

/// 추론으로만 얻은 대기 판정은 dim red로 낮춰 확실한 대기와 구분한다.
#[allow(dead_code)]
pub fn color_for_row(state: State, confidence: Confidence) -> Style {
    if state.is_waiting() && confidence == Confidence::Low {
        return DIM_RED;
    }
    color_for(state)
}
