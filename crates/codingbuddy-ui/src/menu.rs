use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
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
    alternate_submit: Option<String>,
    fixed_choices: bool,
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

    /// A short list of actions that does not accept text input.
    pub fn actions(title: &str, message: &str, choices: Vec<(String, String)>) -> Self {
        Self {
            fixed_choices: true,
            ..Self::choices(title, message, choices)
        }
    }

    /// Add a Ctrl+S action for saving a masked credential.
    pub fn with_save_action(mut self, action: &str) -> Self {
        self.alternate_submit = Some(action.into());
        self
    }

    fn matches(&self) -> impl Iterator<Item = &(String, String)> {
        let query = self.input.to_lowercase();
        self.choices
            .iter()
            .filter(move |(_, label)| label.to_lowercase().contains(&query))
    }

    pub(crate) fn paste(&mut self, value: &str) {
        if self.fixed_choices {
            return;
        }
        self.input
            .extend(value.chars().filter(|ch| !ch.is_control()).take(4096));
        self.selected = 0;
    }

    pub(crate) fn key(&mut self, key: KeyEvent) -> Option<(String, String)> {
        let code = match key.code {
            KeyCode::Char('s')
                if key.modifiers == KeyModifiers::CONTROL && self.alternate_submit.is_some() =>
            {
                KeyCode::Enter
            }
            KeyCode::Char('j' | 'm') if key.modifiers == KeyModifiers::CONTROL => KeyCode::Enter,
            code => code,
        };
        match code {
            KeyCode::Enter => {
                if let Some((action, masked)) = &self.field {
                    if *masked && self.input.trim().is_empty() {
                        self.message = "Paste a non-empty API key, then press Enter.".into();
                        return None;
                    }
                    let action = if key.code == KeyCode::Char('s') {
                        self.alternate_submit.as_ref().unwrap_or(action)
                    } else {
                        action
                    };
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
                if !self.fixed_choices
                    && !key.modifiers.intersects(
                        crossterm::event::KeyModifiers::CONTROL
                            | crossterm::event::KeyModifiers::ALT,
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
        if self.field.is_some() || (!self.fixed_choices && !self.choices.is_empty()) {
            let label = if self.field.is_some() {
                "›"
            } else {
                "Search"
            };
            let width = usize::from(area.width.saturating_sub(5));
            let start = input.chars().count().saturating_sub(width);
            let visible: String = input.chars().skip(start).collect();
            lines.push(Line::raw(format!("{label} {visible}")));
            if self.field.as_ref().is_some_and(|(_, masked)| *masked) && !input.is_empty() {
                lines.push(Line::raw("Key entered · Press Enter to continue"));
            }
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
                    .title_bottom(if self.alternate_submit.is_some() {
                        " Enter connect · Ctrl+S save · Esc cancel "
                    } else if self.field.is_some() {
                        " Enter continue · Esc cancel "
                    } else {
                        " ↑↓ Select · Enter continue · Esc cancel "
                    }),
            ),
            area,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masked_form_stays_open_when_empty_and_accepts_raw_line_feed() {
        let mut form = TerminalMenu::field("API key", "Paste your key", "submit", true);
        let line_feed = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL);
        assert!(form.key(line_feed).is_none());
        assert!(form.message.contains("non-empty"));
        form.paste("  fake-key\r\n");
        assert_eq!(
            form.key(line_feed),
            Some(("submit".into(), "  fake-key".into()))
        );
    }

    #[test]
    fn credential_actions_ignore_paste_and_masked_form_can_save() {
        let mut menu = TerminalMenu::actions(
            "Credential options",
            "Choose an action",
            vec![("use".into(), "Use existing key".into())],
        );
        menu.paste("fake-secret");
        assert_eq!(
            menu.key(KeyCode::Enter.into()),
            Some(("use".into(), String::new()))
        );
        let mut form =
            TerminalMenu::field("API key", "Paste", "connect", true).with_save_action("save");
        form.paste("fake-secret");
        assert_eq!(
            form.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            Some(("save".into(), "fake-secret".into()))
        );
    }

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
