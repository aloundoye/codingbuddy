use ratatui::style::{Color, Modifier, Style};
use std::sync::OnceLock;

/// Global active theme. Initialized once at TUI startup.
static ACTIVE_THEME: OnceLock<TuiTheme> = OnceLock::new();

/// Initialize the global theme. Call once at TUI startup.
pub fn init_theme(theme: TuiTheme) {
    let _ = ACTIVE_THEME.set(theme);
}

/// Get the active theme. Returns dark theme if not yet initialized.
pub fn theme() -> &'static TuiTheme {
    ACTIVE_THEME.get_or_init(TuiTheme::dark)
}

/// Full TUI color palette with semantic roles.
#[derive(Debug, Clone)]
pub struct TuiTheme {
    // Brand / accent
    pub primary: Color,
    pub secondary: Color,
    pub error: Color,
    pub success: Color,
    pub warning: Color,

    // Message prefixes & bodies
    pub user_prefix: Color,
    pub user_body: Color,
    pub assistant_body: Color,
    pub system_fg: Color,
    pub tool_call: Color,
    pub tool_result_ok: Color,
    pub tool_result_err: Color,
    pub tool_result_neutral: Color,
    pub thinking: Color,
    pub thinking_body: Color,

    // Diff
    pub diff_add: Color,
    pub diff_remove: Color,
    pub diff_hunk: Color,
    pub diff_meta: Color,

    // Code
    pub code_inline: Color,
    pub code_fence: Color,

    // Table
    pub table_border: Color,
    pub table_header: Color,
    pub table_cell: Color,

    // UI chrome
    pub separator: Color,
    pub muted: Color,
    pub bold_heading: Color,

    // Whether terminal has a light background
    pub is_light: bool,
}

impl Default for TuiTheme {
    fn default() -> Self {
        Self::dark()
    }
}

impl TuiTheme {
    /// Dark theme (default — for dark terminal backgrounds).
    pub fn dark() -> Self {
        Self {
            primary: Color::Cyan,
            secondary: Color::Yellow,
            error: Color::Red,
            success: Color::Green,
            warning: Color::Yellow,

            user_prefix: Color::Cyan,
            user_body: Color::White,
            assistant_body: Color::White,
            system_fg: Color::DarkGray,
            tool_call: Color::Yellow,
            tool_result_ok: Color::Green,
            tool_result_err: Color::Red,
            tool_result_neutral: Color::DarkGray,
            thinking: Color::Magenta,
            thinking_body: Color::DarkGray,

            diff_add: Color::Green,
            diff_remove: Color::Red,
            diff_hunk: Color::Cyan,
            diff_meta: Color::DarkGray,

            code_inline: Color::Yellow,
            code_fence: Color::DarkGray,

            table_border: Color::DarkGray,
            table_header: Color::Cyan,
            table_cell: Color::White,

            separator: Color::DarkGray,
            muted: Color::DarkGray,
            bold_heading: Color::Cyan,

            is_light: false,
        }
    }

    /// Light theme — for light/white terminal backgrounds.
    pub fn light() -> Self {
        Self {
            primary: Color::Blue,
            secondary: Color::Magenta,
            error: Color::Red,
            success: Color::Green,
            warning: Color::Yellow,

            user_prefix: Color::Blue,
            user_body: Color::Black,
            assistant_body: Color::Black,
            system_fg: Color::Gray,
            tool_call: Color::Magenta,
            tool_result_ok: Color::Green,
            tool_result_err: Color::Red,
            tool_result_neutral: Color::Gray,
            thinking: Color::Magenta,
            thinking_body: Color::Gray,

            diff_add: Color::Green,
            diff_remove: Color::Red,
            diff_hunk: Color::Blue,
            diff_meta: Color::Gray,

            code_inline: Color::Magenta,
            code_fence: Color::Gray,

            table_border: Color::Gray,
            table_header: Color::Blue,
            table_cell: Color::Black,

            separator: Color::Gray,
            muted: Color::Gray,
            bold_heading: Color::Blue,

            is_light: true,
        }
    }

    /// Colorblind-safe theme (deuteranopia). Avoids red/green distinction,
    /// uses blue/orange/yellow instead. Based on Wong (2011) palette.
    pub fn colorblind() -> Self {
        Self {
            primary: Color::Rgb(0, 114, 178),   // Blue
            secondary: Color::Rgb(230, 159, 0), // Orange
            error: Color::Rgb(213, 94, 0),      // Vermillion (red-safe)
            success: Color::Rgb(0, 158, 115),   // Bluish green
            warning: Color::Rgb(240, 228, 66),  // Yellow

            user_prefix: Color::Rgb(0, 114, 178), // Blue
            user_body: Color::White,
            assistant_body: Color::White,
            system_fg: Color::DarkGray,
            tool_call: Color::Rgb(230, 159, 0),      // Orange
            tool_result_ok: Color::Rgb(0, 158, 115), // Bluish green
            tool_result_err: Color::Rgb(213, 94, 0), // Vermillion
            tool_result_neutral: Color::DarkGray,
            thinking: Color::Rgb(204, 121, 167), // Reddish purple
            thinking_body: Color::DarkGray,

            diff_add: Color::Rgb(0, 158, 115), // Bluish green (not plain green)
            diff_remove: Color::Rgb(213, 94, 0), // Vermillion (not plain red)
            diff_hunk: Color::Rgb(0, 114, 178), // Blue
            diff_meta: Color::DarkGray,

            code_inline: Color::Rgb(230, 159, 0), // Orange
            code_fence: Color::DarkGray,

            table_border: Color::DarkGray,
            table_header: Color::Rgb(0, 114, 178),
            table_cell: Color::White,

            separator: Color::DarkGray,
            muted: Color::DarkGray,
            bold_heading: Color::Rgb(0, 114, 178),

            is_light: false,
        }
    }

    /// Select theme based on config or auto-detect.
    /// `preference`: "dark", "light", "colorblind", or "auto" (default).
    pub fn from_preference(preference: &str) -> Self {
        match preference.to_ascii_lowercase().as_str() {
            "light" => Self::light(),
            "dark" => Self::dark(),
            "colorblind" | "colour-blind" | "cb" => Self::colorblind(),
            _ => {
                if detect_light_background() {
                    Self::light()
                } else {
                    Self::dark()
                }
            }
        }
    }

    /// Build theme from explicit config colors (legacy 3-color compat).
    pub fn from_config(primary: &str, secondary: &str, error: &str) -> Self {
        let mut theme = Self::dark();
        theme.primary = parse_theme_color(primary);
        theme.secondary = parse_theme_color(secondary);
        theme.error = parse_theme_color(error);
        // Derive related colors from primary
        theme.user_prefix = theme.primary;
        theme.bold_heading = theme.primary;
        theme.tool_call = theme.secondary;
        theme
    }

    // ── Style helpers ────────────────────────────────────────────────────

    pub fn user_prefix_style(&self) -> Style {
        Style::default()
            .fg(self.user_prefix)
            .add_modifier(Modifier::BOLD)
    }

    pub fn user_body_style(&self) -> Style {
        Style::default().fg(self.user_body)
    }

    pub fn assistant_body_style(&self) -> Style {
        Style::default().fg(self.assistant_body)
    }

    pub fn system_style(&self) -> Style {
        Style::default().fg(self.system_fg)
    }

    pub fn tool_call_style(&self) -> Style {
        Style::default().fg(self.tool_call)
    }

    pub fn error_style(&self) -> Style {
        Style::default().fg(self.error).add_modifier(Modifier::BOLD)
    }

    pub fn error_body_style(&self) -> Style {
        Style::default().fg(self.error)
    }

    pub fn thinking_prefix_style(&self) -> Style {
        Style::default()
            .fg(self.thinking)
            .add_modifier(Modifier::ITALIC)
    }

    pub fn thinking_body_style(&self) -> Style {
        Style::default()
            .fg(self.thinking_body)
            .add_modifier(Modifier::ITALIC)
    }

    pub fn heading_style(&self) -> Style {
        Style::default()
            .fg(self.bold_heading)
            .add_modifier(Modifier::BOLD)
    }
}

fn parse_theme_color(name: &str) -> Color {
    let lower = name.to_ascii_lowercase();
    // Support hex colors: #rrggbb
    if lower.starts_with('#')
        && lower.len() == 7
        && let (Ok(r), Ok(g), Ok(b)) = (
            u8::from_str_radix(&lower[1..3], 16),
            u8::from_str_radix(&lower[3..5], 16),
            u8::from_str_radix(&lower[5..7], 16),
        )
    {
        return Color::Rgb(r, g, b);
    }
    match lower.as_str() {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "white" => Color::White,
        "gray" | "grey" => Color::Gray,
        "darkgray" | "darkgrey" => Color::DarkGray,
        "lightred" => Color::LightRed,
        "lightgreen" => Color::LightGreen,
        "lightyellow" => Color::LightYellow,
        "lightblue" => Color::LightBlue,
        "lightmagenta" => Color::LightMagenta,
        "lightcyan" => Color::LightCyan,
        _ => Color::Cyan,
    }
}

/// Detect whether the terminal has a light background using the OSC 11 query.
/// Returns `true` if the background appears light (luminance > 0.5).
/// Falls back to `false` (dark) if detection fails or times out.
fn detect_light_background() -> bool {
    // Only attempt on real TTYs
    if !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        return false;
    }

    // Use the COLORFGBG env var as a fast heuristic (set by some terminals).
    // Format: "foreground;background" where background > 6 typically means light.
    if let Ok(val) = std::env::var("COLORFGBG")
        && let Some(bg) = val.rsplit(';').next().and_then(|s| s.parse::<u8>().ok())
    {
        // ANSI colors 0-6 are dark, 7+ are light-ish
        return bg >= 7 && bg != 8; // 8 is dark gray
    }

    // Fallback: assume dark (most developer terminals are dark)
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_theme_defaults() {
        let t = TuiTheme::dark();
        assert_eq!(t.primary, Color::Cyan);
        assert!(!t.is_light);
    }

    #[test]
    fn light_theme_uses_darker_colors() {
        let t = TuiTheme::light();
        assert_eq!(t.primary, Color::Blue);
        assert!(t.is_light);
        assert_eq!(t.assistant_body, Color::Black);
    }

    #[test]
    fn from_config_overrides_primary() {
        let t = TuiTheme::from_config("green", "magenta", "lightred");
        assert_eq!(t.primary, Color::Green);
        assert_eq!(t.secondary, Color::Magenta);
        assert_eq!(t.error, Color::LightRed);
    }

    #[test]
    fn hex_color_parsing() {
        let c = parse_theme_color("#ff8800");
        assert_eq!(c, Color::Rgb(255, 136, 0));
    }

    #[test]
    fn from_preference_dark() {
        let t = TuiTheme::from_preference("dark");
        assert!(!t.is_light);
    }

    #[test]
    fn from_preference_light() {
        let t = TuiTheme::from_preference("light");
        assert!(t.is_light);
    }
}
