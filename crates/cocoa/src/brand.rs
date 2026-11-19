//! Text-only terminal welcome screen.

use ratatui::{Frame, layout::Rect, style::Style, widgets::Paragraph};

use crate::theme::Theme;

const TAGLINE: &str = "Got an idea? Let's make something sweet.";

pub(crate) fn welcome(frame: &mut Frame<'_>, area: Rect, theme: Theme) {
    let palette = theme.palette();
    let color = palette.accent;
    if area.width < 9 || area.height < 2 {
        return;
    }
    let show_tagline = area.width >= 40 && area.height >= 4;
    let width = if show_tagline {
        TAGLINE.len() as u16
    } else {
        5
    };
    let height = if show_tagline { 3 } else { 1 };
    let left = area.x + area.width.saturating_sub(width) / 2;
    let top = area.y + area.height.saturating_sub(height) / 2;
    frame.render_widget(
        Paragraph::new("cocoa").style(Style::new().fg(color)),
        Rect::new(area.x + area.width.saturating_sub(5) / 2, top, 5, 1),
    );
    if show_tagline {
        frame.render_widget(
            Paragraph::new(TAGLINE).style(Style::new().fg(palette.muted)),
            Rect::new(left, top + 2, width, 1),
        );
    }
}
