/// Terminal image protocol support.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageProtocol {
    /// iTerm2 inline image protocol (also supported by WezTerm, mintty).
    Iterm2,
    /// Kitty graphics protocol.
    Kitty,
    /// No inline image support — use ASCII placeholder.
    None,
}

/// Detect which image protocol the current terminal supports.
pub fn detect_image_protocol() -> ImageProtocol {
    // iTerm2 and WezTerm set TERM_PROGRAM
    if let Ok(program) = std::env::var("TERM_PROGRAM") {
        let lower = program.to_ascii_lowercase();
        if lower.contains("iterm") || lower.contains("wezterm") || lower.contains("mintty") {
            return ImageProtocol::Iterm2;
        }
    }
    // Kitty sets TERM=xterm-kitty or TERM_PROGRAM=kitty
    if let Ok(term) = std::env::var("TERM")
        && term.contains("kitty")
    {
        return ImageProtocol::Kitty;
    }
    if let Ok(program) = std::env::var("TERM_PROGRAM")
        && program.to_ascii_lowercase().contains("kitty")
    {
        return ImageProtocol::Kitty;
    }
    // KITTY_WINDOW_ID is set inside Kitty
    if std::env::var("KITTY_WINDOW_ID").is_ok() {
        return ImageProtocol::Kitty;
    }
    ImageProtocol::None
}

/// Render an image inline in the terminal.
/// `data` is the raw image bytes. Returns the escape sequence to write.
pub fn render_inline_image(data: &[u8], protocol: ImageProtocol) -> Option<String> {
    use base64::Engine;
    let engine = base64::engine::general_purpose::STANDARD;

    match protocol {
        ImageProtocol::Iterm2 => {
            let b64 = engine.encode(data);
            // iTerm2 protocol: ESC ] 1337 ; File=[args] : <base64> BEL
            Some(format!(
                "\x1b]1337;File=inline=1;size={};preserveAspectRatio=1:{}\x07",
                data.len(),
                b64
            ))
        }
        ImageProtocol::Kitty => {
            let b64 = engine.encode(data);
            // Kitty protocol: send in chunks of 4096 bytes
            let mut output = String::new();
            let chunks: Vec<&str> = {
                let mut v = Vec::new();
                let mut i = 0;
                while i < b64.len() {
                    let end = (i + 4096).min(b64.len());
                    v.push(&b64[i..end]);
                    i = end;
                }
                v
            };
            for (idx, chunk) in chunks.iter().enumerate() {
                let more = if idx < chunks.len() - 1 { 1 } else { 0 };
                if idx == 0 {
                    // First chunk: action=transmit+display, format=100 (auto-detect)
                    output.push_str(&format!("\x1b_Ga=T,f=100,m={more};{chunk}\x1b\\"));
                } else {
                    // Continuation chunks
                    output.push_str(&format!("\x1b_Gm={more};{chunk}\x1b\\"));
                }
            }
            Some(output)
        }
        ImageProtocol::None => None,
    }
}

/// Print an image inline to stdout, if the terminal supports it.
/// Returns true if the image was displayed, false if no protocol is available.
pub fn display_image_inline(data: &[u8]) -> bool {
    let protocol = detect_image_protocol();
    if let Some(escape) = render_inline_image(data, protocol) {
        use std::io::Write;
        let mut out = std::io::stdout();
        let _ = out.write_all(escape.as_bytes());
        let _ = writeln!(out);
        let _ = out.flush();
        true
    } else {
        false
    }
}
