use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Clone)]
pub struct Theme {
    pub name: String,

    // Colors
    pub bg: Color,
    pub fg: Color,
    pub border: Color,
    pub highlight: Color,
    pub disabled: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    pub info: Color,
    pub new_item: Color,

    // Styles
    pub selected_modifier: Modifier,
}

impl Theme {
    pub fn default_dark() -> Self {
        Self {
            name: "Dark".to_string(),
            bg: Color::Reset,
            fg: Color::White,
            border: Color::Gray,
            highlight: Color::Cyan,
            disabled: Color::DarkGray,
            success: Color::Green,
            warning: Color::Yellow,
            error: Color::Red,
            info: Color::Blue,
            new_item: Color::Magenta,
            selected_modifier: Modifier::BOLD,
        }
    }

    pub fn get_border_style(&self) -> Style {
        Style::default().fg(self.border)
    }

    pub fn get_selected_style(&self) -> Style {
        Style::default()
            .fg(self.highlight)
            .add_modifier(self.selected_modifier)
    }

    pub fn get_disabled_style(&self) -> Style {
        Style::default().fg(self.disabled)
    }

    pub fn get_success_style(&self) -> Style {
        Style::default().fg(self.success)
    }

    pub fn get_warning_style(&self) -> Style {
        Style::default().fg(self.warning)
    }

    pub fn get_error_style(&self) -> Style {
        Style::default().fg(self.error)
    }

    pub fn get_info_style(&self) -> Style {
        Style::default().fg(self.info)
    }

    pub fn get_new_item_style(&self) -> Style {
        Style::default().fg(self.new_item)
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::default_dark()
    }
}
