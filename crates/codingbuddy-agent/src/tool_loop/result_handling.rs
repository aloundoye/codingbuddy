//! Tool-result shaping before results are added back to the transcript.

use codingbuddy_core::ToolResult;

pub(super) fn truncate_result_output(result: &mut ToolResult, max_chars: usize) {
    if let Some(text) = result.output.as_str()
        && text.len() > max_chars
    {
        let truncated = &text[..text.floor_char_boundary(max_chars)];
        let footer = format!(
            "\n\n[Output truncated. Showing first {} of {} characters. \
             Use more specific queries to narrow results.]",
            max_chars,
            text.len()
        );
        result.output = serde_json::Value::String(format!("{truncated}{footer}"));
    }
}
