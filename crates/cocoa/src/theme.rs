//! Built-in Cocoa palettes. Themes affect presentation, never execution or saved history.

use ratatui::style::{Color, Modifier, Style};
use serde::{Deserialize, Serialize};

/// Dark chocolate is the default. Terminal native inherits the terminal's base colors.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Theme {
    #[default]
    DarkChocolate,
    CacaoGrove,
    MilkChocolate,
    TerminalNative,
}

/// Semantic colors shared by chat, completion, dialogs and Markdown.
#[derive(Clone, Copy)]
pub struct Palette {
    pub background: Color,
    pub foreground: Color,
    pub muted: Color,
    pub accent: Color,
    pub surface: Color,
    pub selected: Color,
    pub border: Color,
    pub success: Color,
    pub error: Color,
}

impl Theme {
    /// Resolve the approved palette without reading terminal state or configuration.
    #[must_use]
    pub const fn palette(self) -> Palette {
        match self {
            Self::DarkChocolate => Palette {
                background: Color::Rgb(16, 12, 10),
                foreground: Color::Rgb(241, 229, 213),
                muted: Color::Rgb(191, 168, 149),
                accent: Color::Rgb(223, 173, 118),
                surface: Color::Rgb(33, 24, 19),
                selected: Color::Rgb(53, 37, 28),
                border: Color::Rgb(128, 105, 87),
                success: Color::Rgb(185, 203, 151),
                error: Color::Rgb(242, 148, 137),
            },
            Self::CacaoGrove => Palette {
                background: Color::Rgb(11, 16, 13),
                foreground: Color::Rgb(229, 234, 220),
                muted: Color::Rgb(171, 189, 167),
                accent: Color::Rgb(217, 188, 131),
                surface: Color::Rgb(23, 32, 25),
                selected: Color::Rgb(38, 52, 36),
                border: Color::Rgb(99, 124, 100),
                success: Color::Rgb(181, 212, 155),
                error: Color::Rgb(244, 158, 140),
            },
            Self::MilkChocolate => Palette {
                background: Color::Rgb(245, 237, 223),
                foreground: Color::Rgb(56, 40, 31),
                muted: Color::Rgb(98, 74, 56),
                accent: Color::Rgb(120, 67, 35),
                surface: Color::Rgb(232, 219, 199),
                selected: Color::Rgb(218, 197, 168),
                border: Color::Rgb(155, 128, 105),
                success: Color::Rgb(54, 84, 44),
                error: Color::Rgb(146, 45, 40),
            },
            Self::TerminalNative => Palette {
                background: Color::Reset,
                foreground: Color::Reset,
                muted: Color::Reset,
                accent: Color::Reset,
                surface: Color::Reset,
                selected: Color::Reset,
                border: Color::Reset,
                success: Color::Green,
                error: Color::Red,
            },
        }
    }

    /// Base colors, including blank cells and cleared overlays.
    #[must_use]
    pub fn style(self) -> Style {
        let palette = self.palette();
        Style::new().fg(palette.foreground).bg(palette.background)
    }

    /// Selection stays readable even when native terminal colors are unknown.
    #[must_use]
    pub fn selection_style(self) -> Style {
        if self == Self::TerminalNative {
            self.style().add_modifier(Modifier::REVERSED)
        } else {
            self.style().bg(self.palette().selected)
        }
    }

    pub(crate) fn syntax_theme(self) -> Option<&'static str> {
        match self {
            Self::MilkChocolate => Some("InspiredGitHub"),
            Self::TerminalNative => None,
            Self::DarkChocolate | Self::CacaoGrove => Some("base16-mocha.dark"),
        }
    }
}
