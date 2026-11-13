use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    style::{Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Clear, Paragraph},
};

/// A keyboard menu, or an input form when `field` is set.
#[derive(Default)]
pub struct TerminalMenu {
    pub title: String,
    pub message: String,
    pub choices: Vec<(String, String)>,
    pub field: Option<(String, bool)>,
    input: String,
    selected: usize,
}

impl TerminalMenu {
    pub fn choices(title: &str, message: &str, choices: Vec<(String, String)>) -> Self {
        Self {
            title: title.into(),
            message: message.into(),
            choices,
            ..Self::default()
        }
    }

    pub fn field(title: &str, message: &str, action: &str, masked: bool) -> Self {
        Self {
            title: title.into(),
            message: message.into(),
            field: Some((action.into(), masked)),
            ..Self::default()
        }
    }

    fn matches(&self) -> impl Iterator<Item = &(String, String)> {
        let query = self.input.to_lowercase();
        self.choices
            .iter()
            .filter(move |(_, label)| label.to_lowercase().contains(&query))
    }

    pub(crate) fn paste(&mut self, value: &str) {
        self.input
            .extend(value.chars().filter(|ch| !ch.is_control()).take(4096));
        self.selected = 0;
    }

    pub(crate) fn key(&mut self, key: KeyEvent) -> Option<(String, String)> {
        match key.code {
            KeyCode::Enter => {
                if let Some((action, _)) = &self.field {
                    return Some((action.clone(), std::mem::take(&mut self.input)));
                }
                return self
                    .matches()
                    .nth(self.selected)
                    .map(|(action, _)| (action.clone(), String::new()));
            }
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(self.matches().count().saturating_sub(1))
            }
            KeyCode::Backspace => {
                self.input.pop();
                self.selected = 0;
            }
            KeyCode::Char(ch)
                if !key.modifiers.intersects(
                    crossterm::event::KeyModifiers::CONTROL | crossterm::event::KeyModifiers::ALT,
                ) =>
            {
                self.input.push(ch);
                self.selected = 0;
            }
            _ => {}
        }
        None
    }

    pub(crate) fn render(&self, frame: &mut Frame) {
        let area = frame.area();
        frame.render_widget(Clear, area);
        let input = if self.field.as_ref().is_some_and(|(_, masked)| *masked) {
            "*".repeat(self.input.chars().count())
        } else {
            self.input.clone()
        };
        let mut lines = vec![Line::raw(self.message.as_str())];
        if self.field.is_some() || !self.choices.is_empty() {
            let label = if self.field.is_some() {
                "›"
            } else {
                "Search"
            };
            lines.push(Line::raw(format!("{label} {input}")));
        }
        let visible = usize::from(area.height.saturating_sub(5)).max(1);
        let first = (self.selected + 1).saturating_sub(visible);
        lines.extend(self.matches().enumerate().skip(first).take(visible).map(
            |(index, (_, label))| {
                if index == self.selected {
                    Line::styled(
                        format!("› {label}"),
                        Style::default().add_modifier(Modifier::REVERSED),
                    )
                } else {
                    Line::raw(format!("  {label}"))
                }
            },
        ));
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(self.title.as_str())
                    .title_bottom(" Enter select · Esc back "),
            ),
            area,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_searchable_options_and_masks_pasted_keys() {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 8)).unwrap();
        let mut menu = TerminalMenu::choices(
            "Providers",
            "Select a provider",
            vec![
                ("openai".into(), "OpenAI".into()),
                ("ollama".into(), "Ollama".into()),
            ],
        );
        menu.paste("OLL");
        assert_eq!(
            menu.key(KeyCode::Enter.into()),
            Some(("ollama".into(), String::new()))
        );
        let mut secret = TerminalMenu::field("API key", "Paste your key", "key", true);
        secret.paste("sample-secret");
        terminal.draw(|frame| secret.render(frame)).unwrap();
        let display = format!("{:?}", terminal.backend().buffer());
        assert!(!display.contains("sample-secret"));
        assert!(display.contains("*************"));
        assert_eq!(
            secret.key(KeyCode::Enter.into()),
            Some(("key".into(), "sample-secret".into()))
        );
    }
}
