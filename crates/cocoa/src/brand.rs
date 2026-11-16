//! Static terminal rendering of the approved Compact core artwork.

use std::sync::LazyLock;

use image::{GrayImage, imageops::FilterType};
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    widgets::{Paragraph, Wrap},
};

use crate::theme::Theme;

const TAGLINE: &str = "Got an idea? Let's make something sweet.";

static MARKS: LazyLock<[Option<GrayImage>; 2]> = LazyLock::new(|| {
    [
        raster(
            include_bytes!("../../../docs/brand/cocoa/png/cocoa-mark-cream-128.png"),
            40,
        ),
        raster(
            include_bytes!("../../../docs/brand/cocoa/png/cocoa-mark-cream-16.png"),
            16,
        ),
    ]
});

pub(crate) fn welcome(frame: &mut Frame<'_>, area: Rect, theme: Theme) {
    let palette = theme.palette();
    let color = palette.accent;
    if area.width < 9 || area.height < 2 {
        return;
    }
    let size = if area.width >= 68 && area.height >= 20 {
        40
    } else if area.width >= 40 && area.height >= 8 {
        16
    } else {
        0
    };
    let text_width = if size == 0 {
        5
    } else {
        area.width
            .saturating_sub(size + 3)
            .min(TAGLINE.len() as u16)
    };
    let width = if size == 0 { 5 } else { size + 3 + text_width };
    let height = if size == 0 { 1 } else { size / 2 };
    let left = area.x + area.width.saturating_sub(width) / 2;
    let top = area.y + area.height.saturating_sub(height) / 2;
    if size > 0
        && let Some(mark) = &MARKS[usize::from(size == 16)]
    {
        for row in 0..size / 2 {
            for column in 0..size {
                let upper = mark.get_pixel(u32::from(column), u32::from(row * 2))[0] >= 128;
                let lower = mark.get_pixel(u32::from(column), u32::from(row * 2 + 1))[0] >= 128;
                let symbol = match (upper, lower) {
                    (true, true) => "█",
                    (true, false) => "▀",
                    (false, true) => "▄",
                    (false, false) => " ",
                };
                frame.buffer_mut()[(left + column, top + row)]
                    .set_symbol(symbol)
                    .set_style(Style::new().fg(color));
            }
        }
    }
    let text_left = left + if size == 0 { 0 } else { size + 3 };
    let text_top = top + height / 2;
    frame.render_widget(
        Paragraph::new("cocoa").style(Style::new().fg(color)),
        Rect::new(text_left, text_top, 5, 1),
    );
    if size > 0 {
        frame.render_widget(
            Paragraph::new(TAGLINE)
                .style(Style::new().fg(palette.muted))
                .wrap(Wrap { trim: true }),
            Rect::new(text_left, text_top + 2, text_width, 2),
        );
    }
}

fn raster(bytes: &[u8], size: u32) -> Option<GrayImage> {
    match image::load_from_memory(bytes) {
        Ok(image) => {
            let image = image.to_rgba8();
            let alpha = GrayImage::from_fn(image.width(), image.height(), |x, y| {
                image::Luma([image.get_pixel(x, y)[3]])
            });
            Some(image::imageops::resize(
                &alpha,
                size,
                size,
                FilterType::Lanczos3,
            ))
        }
        Err(error) => {
            tracing::error!(%error, "cannot decode Cocoa welcome artwork");
            None
        }
    }
}
