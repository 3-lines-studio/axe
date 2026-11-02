use ratatui::style::{Color, Modifier, Style};
use std::sync::OnceLock;
use std::time::Duration;
use terminal_colorsaurus::{QueryOptions, ThemeMode};
use tui_markdown::{AlertKind, Options, StyleSheet};

#[derive(Clone, Copy)]
pub(super) struct Palette {
    pub strong: Color,
    pub secondary: Color,
    pub tool: Color,
    pub muted: Color,
}

impl Palette {
    const DARK: Palette = Palette {
        strong: Color::Cyan,
        secondary: Color::Magenta,
        tool: Color::Yellow,
        muted: Color::DarkGray,
    };

    const LIGHT: Palette = Palette {
        strong: Color::Blue,
        secondary: Color::Magenta,
        tool: Color::Red,
        muted: Color::DarkGray,
    };
}

#[derive(Clone, Copy)]
pub(super) struct Styles(Palette);

impl StyleSheet for Styles {
    fn heading(&self, level: u8) -> Style {
        let style = Style::new().fg(self.0.strong).add_modifier(Modifier::BOLD);
        if level == 1 {
            style.add_modifier(Modifier::UNDERLINED)
        } else {
            style
        }
    }

    fn code(&self) -> Style {
        Style::new().fg(self.0.muted)
    }

    fn link(&self) -> Style {
        Style::new()
            .fg(self.0.strong)
            .add_modifier(Modifier::UNDERLINED)
    }

    fn blockquote(&self) -> Style {
        Style::new().fg(self.0.muted)
    }

    fn heading_meta(&self) -> Style {
        Style::new().fg(self.0.muted)
    }

    fn metadata_block(&self) -> Style {
        Style::new().fg(self.0.muted)
    }

    fn html(&self) -> Style {
        Style::new().fg(self.0.muted)
    }

    fn math_inline(&self) -> Style {
        Style::new()
            .fg(self.0.secondary)
            .add_modifier(Modifier::ITALIC)
    }

    fn math_display(&self) -> Style {
        Style::new().fg(self.0.secondary)
    }

    fn footnote_ref(&self) -> Style {
        Style::new().fg(self.0.muted).add_modifier(Modifier::ITALIC)
    }

    fn footnote_def(&self) -> Style {
        Style::new().fg(self.0.muted)
    }

    fn definition_term(&self) -> Style {
        Style::new().add_modifier(Modifier::BOLD)
    }

    fn alert(&self, kind: AlertKind) -> Style {
        match kind {
            AlertKind::Note | AlertKind::Tip => Style::new().fg(self.0.strong),
            AlertKind::Important | AlertKind::Warning | AlertKind::Caution => {
                Style::new().fg(self.0.secondary)
            }
        }
    }

    fn table_header(&self) -> Style {
        Style::new().fg(self.0.strong).add_modifier(Modifier::BOLD)
    }

    fn table_border(&self) -> Style {
        Style::new().fg(self.0.muted)
    }

    fn list_marker(&self) -> Style {
        Style::new().fg(self.0.secondary)
    }

    fn image_alt(&self) -> Style {
        Style::new().fg(self.0.muted).add_modifier(Modifier::ITALIC)
    }
}

const TIMEOUT: Duration = Duration::from_millis(250);

static PALETTE: OnceLock<Palette> = OnceLock::new();

pub(super) fn init() {
    let palette = match terminal_colorsaurus::theme_mode(options()) {
        Ok(ThemeMode::Light) => Palette::LIGHT,
        _ => Palette::DARK,
    };
    let _ = PALETTE.set(palette);
}

fn options() -> QueryOptions {
    let mut options = QueryOptions::default();
    options.timeout = TIMEOUT;
    options
}

pub(super) fn palette() -> Palette {
    *PALETTE.get().unwrap_or(&Palette::DARK)
}

pub(super) fn markdown(markdown: &str) -> ratatui::text::Text<'_> {
    let options = Options::new(Styles(palette()));
    tui_markdown::from_str_with_options(markdown, &options)
}
