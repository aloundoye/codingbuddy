//! Streaming markdown-to-ANSI renderer for the non-interactive (readline) chat path.
//!
//! `StreamingMdRenderer` buffers streaming LLM tokens and renders complete lines
//! with ANSI formatting as they arrive, providing real-time formatted output that
//! matches the TUI's `render_assistant_markdown` quality.

use std::io::{self, Write};

use unicode_width::UnicodeWidthChar;

// ── ANSI escape helpers ─────────────────────────────────────────────────

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const ITALIC: &str = "\x1b[3m";
const UNDERLINE: &str = "\x1b[4m";
const CYAN: &str = "\x1b[36m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const WHITE: &str = "\x1b[37m";
const GRAY: &str = "\x1b[90m";
const RED: &str = "\x1b[31m";
const MAGENTA: &str = "\x1b[35m";
const BG_GRAY: &str = "\x1b[48;5;236m";

// ── StreamingMdRenderer ─────────────────────────────────────────────────

/// Incremental markdown-to-ANSI renderer.
///
/// Feed streaming tokens via [`Self::push`]. Complete lines are rendered immediately
/// with ANSI formatting. Call [`Self::finish`] after streaming ends to emit
/// any trailing partial line.
pub struct StreamingMdRenderer {
    buffer: String,
    in_code_block: bool,
    code_lang: String,
    preview_visible: bool,
}

impl StreamingMdRenderer {
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
            in_code_block: false,
            code_lang: String::new(),
            preview_visible: false,
        }
    }

    /// Commit complete lines and redraw the unfinished line on a terminal.
    pub fn push(&mut self, text: &str) {
        let out = io::stdout();
        let mut writer = out.lock();
        let preview_width = crossterm::terminal::size()
            .ok()
            .map(|(width, _)| usize::from(width));
        self.push_to(&mut writer, text, preview_width);
    }

    fn push_to(&mut self, writer: &mut impl Write, text: &str, preview_width: Option<usize>) {
        if text.is_empty() {
            return;
        }
        self.clear_preview(writer);
        for chunk in text.split_inclusive('\n') {
            let line = chunk.strip_suffix('\n');
            self.buffer.extend(
                line.unwrap_or(chunk)
                    .chars()
                    .filter(|ch| !ch.is_control() || *ch == '\t'),
            );
            if line.is_some() {
                let complete = std::mem::take(&mut self.buffer);
                self.render_line(&mut *writer, &complete);
            }
        }
        if let Some(width) = preview_width {
            self.show_preview(writer, width);
        }
        let _ = writer.flush();
    }

    /// Finish the current response block and reset Markdown state for the next one.
    pub fn finish(&mut self) {
        let out = io::stdout();
        let mut writer = out.lock();
        self.finish_to(&mut writer, false);
    }

    pub fn clear(&mut self) {
        let out = io::stdout();
        let mut writer = out.lock();
        self.finish_to(&mut writer, true);
    }

    fn finish_to(&mut self, writer: &mut impl Write, discard: bool) {
        self.clear_preview(writer);
        let line = std::mem::take(&mut self.buffer);
        if !discard && !line.is_empty() {
            self.render_line(&mut *writer, &line);
        }
        if self.in_code_block {
            let _ = writeln!(writer, "  {DIM}{GRAY}└────────{RESET}");
            self.in_code_block = false;
            self.code_lang.clear();
        }
        let _ = writer.flush();
    }

    fn clear_preview(&mut self, writer: &mut impl Write) {
        if self.preview_visible {
            let _ = write!(writer, "\x1b[J");
            self.preview_visible = false;
        }
    }

    fn show_preview(&mut self, writer: &mut impl Write, columns: usize) {
        if self.buffer.is_empty() || columns <= 3 {
            return;
        }
        // Show the most recent text and leave the last column clear to prevent wrapping.
        let budget = columns - 3;
        let mut start = self.buffer.len();
        let mut used = 0;
        for (index, ch) in self.buffer.char_indices().rev() {
            let width = ch.width().unwrap_or(0);
            if used + width > budget {
                break;
            }
            start = index;
            used += width;
        }
        let clipped = start != 0;
        if clipped && used == budget {
            for ch in self.buffer[start..].chars() {
                start += ch.len_utf8();
                if ch.width().unwrap_or(0) > 0 {
                    break;
                }
            }
        }
        let _ = write!(writer, "  {}", if clipped { "…" } else { "" });
        for ch in self.buffer[start..].chars().filter(|ch| !ch.is_control()) {
            let _ = write!(writer, "{ch}");
        }
        // Leave the cursor at the preview's start so a resize can reflow the
        // preview without moving the start of the area that will be erased.
        let _ = write!(writer, "\r");
        self.preview_visible = true;
    }

    fn render_line(&mut self, mut w: impl Write, line: &str) {
        // ── Code fence toggle ───────────────────────────────────────
        if line.starts_with("```") {
            if !self.in_code_block {
                self.in_code_block = true;
                self.code_lang = line.trim_start_matches('`').trim().to_string();
                let lang_label = if self.code_lang.is_empty() {
                    String::new()
                } else {
                    format!(" {}", self.code_lang)
                };
                let _ = writeln!(w, "  {DIM}{GRAY}┌──{lang_label}──{RESET}");
            } else {
                self.in_code_block = false;
                self.code_lang.clear();
                let _ = writeln!(w, "  {DIM}{GRAY}└────────{RESET}");
            }
            return;
        }

        // ── Inside code block — render with dim gray + line prefix ──
        if self.in_code_block {
            let _ = writeln!(w, "  {DIM}{GRAY}│{RESET} {DIM}{WHITE}{line}{RESET}");
            return;
        }

        // ── Horizontal rule ─────────────────────────────────────────
        if line == "---" || line == "***" || line == "___" {
            let _ = writeln!(
                w,
                "  {GRAY}─────────────────────────────────────────{RESET}"
            );
            return;
        }

        // ── Headings ────────────────────────────────────────────────
        if line.starts_with("# ") || (line.starts_with('#') && !line.starts_with("#!")) {
            let trimmed = line.trim_start_matches('#');
            let level = line.len() - trimmed.len();
            let heading = trimmed.strip_prefix(' ').unwrap_or(trimmed);
            if !heading.is_empty() {
                let styled = render_inline_markdown(heading);
                match level {
                    1 => {
                        let _ = writeln!(w);
                        let _ = writeln!(w, "  {BOLD}{CYAN}{UNDERLINE}{styled}{RESET}");
                        let _ = writeln!(w);
                    }
                    2 => {
                        let _ = writeln!(w);
                        let _ = writeln!(w, "  {BOLD}{CYAN}{styled}{RESET}");
                    }
                    3 => {
                        let _ = writeln!(w, "  {CYAN}{styled}{RESET}");
                    }
                    _ => {
                        let _ = writeln!(w, "  {BOLD}{WHITE}{styled}{RESET}");
                    }
                }
                return;
            }
        }

        // ── Blockquote ──────────────────────────────────────────────
        if let Some(quote) = line.strip_prefix("> ") {
            let styled = render_inline_markdown(quote);
            let _ = writeln!(w, "  {GRAY}│{RESET} {DIM}{styled}{RESET}");
            return;
        }

        // ── Task list ───────────────────────────────────────────────
        if let Some(rest) = line
            .strip_prefix("- [ ] ")
            .or_else(|| line.strip_prefix("* [ ] "))
        {
            let styled = render_inline_markdown(rest);
            let _ = writeln!(w, "  {GRAY}☐{RESET} {styled}");
            return;
        }
        if let Some(rest) = line
            .strip_prefix("- [x] ")
            .or_else(|| line.strip_prefix("* [x] "))
            .or_else(|| line.strip_prefix("- [X] "))
            .or_else(|| line.strip_prefix("* [X] "))
        {
            let styled = render_inline_markdown(rest);
            let _ = writeln!(w, "  {GREEN}☑{RESET} {DIM}{styled}{RESET}");
            return;
        }

        // ── Unordered list ──────────────────────────────────────────
        if let Some(item) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
            let styled = render_inline_markdown(item);
            let _ = writeln!(w, "  {CYAN}•{RESET} {styled}");
            return;
        }

        // ── Indented sub-list (2-4 spaces + bullet) ─────────────────
        let stripped = line.trim_start();
        let indent_len = line.len() - stripped.len();
        if indent_len >= 2
            && let Some(item) = stripped
                .strip_prefix("- ")
                .or_else(|| stripped.strip_prefix("* "))
        {
            let indent = " ".repeat(indent_len);
            let styled = render_inline_markdown(item);
            let _ = writeln!(w, "  {indent}{CYAN}◦{RESET} {styled}");
            return;
        }

        // ── Numbered list ───────────────────────────────────────────
        if let Some(dot_pos) = line.find(". ")
            && dot_pos <= 4
            && line[..dot_pos].chars().all(|c| c.is_ascii_digit())
        {
            let num = &line[..dot_pos + 2];
            let item = &line[dot_pos + 2..];
            let styled = render_inline_markdown(item);
            let _ = writeln!(w, "  {CYAN}{num}{RESET}{styled}");
            return;
        }

        // ── Table rows ──────────────────────────────────────────────
        if line.len() >= 2 && line.starts_with('|') && line.ends_with('|') {
            let inner = &line[1..line.len() - 1];
            // Separator row
            if inner
                .chars()
                .all(|c| c == '-' || c == '|' || c == ':' || c == ' ')
            {
                let _ = writeln!(w, "  {GRAY}{line}{RESET}");
                return;
            }
            // Data row — highlight separators
            let cells: Vec<&str> = inner.split('|').collect();
            let _ = write!(w, "  {GRAY}│{RESET}");
            for cell in cells {
                let styled = render_inline_markdown(cell.trim());
                let _ = write!(w, " {styled} {GRAY}│{RESET}");
            }
            let _ = writeln!(w);
            return;
        }

        // ── Blank line ──────────────────────────────────────────────
        if line.trim().is_empty() {
            let _ = writeln!(w);
            return;
        }

        // ── Regular paragraph text with inline markdown ─────────────
        let styled = render_inline_markdown(line);
        let _ = writeln!(w, "  {styled}");
    }
}

// ── Inline markdown rendering ───────────────────────────────────────────

/// Render inline markdown elements (bold, italic, code, strikethrough) to ANSI.
fn render_inline_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 64);
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        // Bold+italic: ***text***
        if i + 2 < len
            && chars[i] == '*'
            && chars[i + 1] == '*'
            && chars[i + 2] == '*'
            && let Some(end) = find_closing(&chars, i + 3, &['*', '*', '*'])
        {
            let inner: String = chars[i + 3..end].iter().collect();
            out.push_str(&format!("{BOLD}{ITALIC}{inner}{RESET}"));
            i = end + 3;
            continue;
        }

        // Bold: **text**
        if i + 1 < len
            && chars[i] == '*'
            && chars[i + 1] == '*'
            && let Some(end) = find_closing(&chars, i + 2, &['*', '*'])
        {
            let inner: String = chars[i + 2..end].iter().collect();
            out.push_str(&format!("{BOLD}{inner}{RESET}"));
            i = end + 2;
            continue;
        }

        // Bold (underscores): __text__
        if i + 1 < len
            && chars[i] == '_'
            && chars[i + 1] == '_'
            && let Some(end) = find_closing(&chars, i + 2, &['_', '_'])
        {
            let inner: String = chars[i + 2..end].iter().collect();
            out.push_str(&format!("{BOLD}{inner}{RESET}"));
            i = end + 2;
            continue;
        }

        // Italic: *text* (single asterisk, not followed by another)
        if chars[i] == '*'
            && (i + 1 >= len || chars[i + 1] != '*')
            && let Some(end) = find_closing(&chars, i + 1, &['*'])
        {
            let inner: String = chars[i + 1..end].iter().collect();
            out.push_str(&format!("{ITALIC}{inner}{RESET}"));
            i = end + 1;
            continue;
        }

        // Italic (underscore): _text_
        if chars[i] == '_'
            && (i + 1 >= len || chars[i + 1] != '_')
            && let Some(end) = find_closing(&chars, i + 1, &['_'])
        {
            // Avoid matching snake_case identifiers
            if end > i + 1 {
                let inner: String = chars[i + 1..end].iter().collect();
                if !inner.contains(' ') && inner.chars().any(|c| c == '_') {
                    // Likely snake_case — don't treat as italic
                    out.push(chars[i]);
                    i += 1;
                    continue;
                }
                out.push_str(&format!("{ITALIC}{inner}{RESET}"));
                i = end + 1;
                continue;
            }
        }

        // Strikethrough: ~~text~~
        if i + 1 < len
            && chars[i] == '~'
            && chars[i + 1] == '~'
            && let Some(end) = find_closing(&chars, i + 2, &['~', '~'])
        {
            let inner: String = chars[i + 2..end].iter().collect();
            out.push_str(&format!("\x1b[9m{inner}{RESET}"));
            i = end + 2;
            continue;
        }

        // Inline code: `text`
        if chars[i] == '`'
            && let Some(end) = find_single_closing(&chars, i + 1, '`')
        {
            let inner: String = chars[i + 1..end].iter().collect();
            out.push_str(&format!("{BG_GRAY}{YELLOW} {inner} {RESET}"));
            i = end + 1;
            continue;
        }

        out.push(chars[i]);
        i += 1;
    }

    out
}

/// Find closing delimiter sequence starting at `start`.
fn find_closing(chars: &[char], start: usize, delim: &[char]) -> Option<usize> {
    let dlen = delim.len();
    if chars.len() < start + dlen {
        return None;
    }
    for i in start..=(chars.len() - dlen) {
        if chars[i..i + dlen] == *delim {
            return Some(i);
        }
    }
    None
}

/// Find single closing character.
fn find_single_closing(chars: &[char], start: usize, ch: char) -> Option<usize> {
    (start..chars.len()).find(|&i| chars[i] == ch)
}

// ── Role headers ────────────────────────────────────────────────────────

/// Print a styled role header before an assistant response.
pub fn print_role_header(role: &str, model: &str) {
    let out = io::stdout();
    let mut w = out.lock();
    let _ = writeln!(w);
    let label = if model.is_empty() {
        format!(" {role} ")
    } else {
        format!(" {role} ({model}) ")
    };
    let _ = writeln!(w, "{CYAN}{BOLD}╭─{label}─╮{RESET}");
}

/// Print a styled separator after an assistant response.
pub fn print_role_footer() {
    let out = io::stdout();
    let mut w = out.lock();
    let _ = writeln!(w, "{CYAN}╰──────────────────╯{RESET}");
    let _ = writeln!(w);
}

/// Print a styled phase indicator (architect/editor/verify/lint).
pub fn print_phase(icon: &str, label: &str, detail: &str) {
    let out = io::stdout();
    let mut w = out.lock();
    if detail.is_empty() {
        let _ = writeln!(w, "  {MAGENTA}{icon}{RESET}  {BOLD}{label}{RESET}");
    } else {
        let _ = writeln!(
            w,
            "  {MAGENTA}{icon}{RESET}  {BOLD}{label}{RESET} {GRAY}({detail}){RESET}"
        );
    }
}

/// Print a phase completion indicator.
pub fn print_phase_done(success: bool, label: &str, detail: &str) {
    let out = io::stdout();
    let mut w = out.lock();
    let (icon, color) = if success {
        ("✓", GREEN)
    } else {
        ("✗", RED)
    };
    if detail.is_empty() {
        let _ = writeln!(w, "  {color}{icon}{RESET}  {label}");
    } else {
        let _ = writeln!(w, "  {color}{icon}{RESET}  {label} {GRAY}({detail}){RESET}");
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_bold() {
        let result = render_inline_markdown("hello **world**");
        assert!(result.contains("\x1b[1m"));
        assert!(result.contains("world"));
    }

    #[test]
    fn inline_italic() {
        let result = render_inline_markdown("hello *world*");
        assert!(result.contains("\x1b[3m"));
        assert!(result.contains("world"));
    }

    #[test]
    fn inline_code() {
        let result = render_inline_markdown("use `cargo test`");
        assert!(result.contains("\x1b[33m")); // yellow
        assert!(result.contains("cargo test"));
    }

    #[test]
    fn inline_bold_italic() {
        let result = render_inline_markdown("***emphasis***");
        assert!(result.contains("\x1b[1m\x1b[3m")); // bold+italic
    }

    #[test]
    fn preview_is_replaced_then_committed_as_markdown() {
        let mut renderer = StreamingMdRenderer::new();
        let mut output = Vec::new();
        renderer.push_to(&mut output, "**界", Some(10));
        assert_eq!(String::from_utf8_lossy(&output), "  **界\r");
        output.clear();
        renderer.push_to(&mut output, "**\r", Some(10));
        assert!(String::from_utf8_lossy(&output).starts_with("\x1b[J"));
        output.clear();
        renderer.push_to(&mut output, "\n", Some(10));
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains(&format!("{BOLD}界")));
        assert!(output.ends_with('\n'));
        assert!(renderer.buffer.is_empty());
    }

    #[test]
    fn preview_respects_new_width_and_clear_discards_the_tail() {
        let mut renderer = StreamingMdRenderer::new();
        let mut output = Vec::new();
        renderer.push_to(&mut output, "界界界界", Some(7));
        assert_eq!(String::from_utf8_lossy(&output), "  …界\r");
        output.clear();
        renderer.push_to(&mut output, "suffix", Some(5));
        assert_eq!(String::from_utf8_lossy(&output), "\x1b[J  …x\r");
        output.clear();
        renderer.finish_to(&mut output, true);
        assert_eq!(String::from_utf8_lossy(&output), "\x1b[J");
        assert!(renderer.buffer.is_empty());
    }

    #[test]
    fn finishing_an_open_fence_resets_the_next_response() {
        let mut renderer = StreamingMdRenderer::new();
        let mut output = Vec::new();
        renderer.push_to(&mut output, "```rust\nfn main() {}", None);
        assert!(renderer.in_code_block);
        renderer.finish_to(&mut output, false);
        assert!(!renderer.in_code_block);
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("fn main() {}"));
        assert!(output.contains("└"));
    }
}
