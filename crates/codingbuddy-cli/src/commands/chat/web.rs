use super::args::truncate_inline;

pub(crate) fn render_web_fetch_markdown(
    url: &str,
    output: &serde_json::Value,
    max_lines: usize,
) -> String {
    let status = output.get("status").and_then(|v| v.as_u64()).unwrap_or(0);
    let content_type = output
        .get("content_type")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let truncated = output
        .get("truncated")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let bytes = output.get("bytes").and_then(|v| v.as_u64()).unwrap_or(0);
    let preview = fetch_preview_text(output, max_lines);

    let mut lines = vec![
        "# Web Fetch".to_string(),
        format!("- URL: {url}"),
        format!("- Status: {status}"),
        format!("- Content-Type: {content_type}"),
        format!("- Bytes: {bytes}"),
        format!("- Truncated: {truncated}"),
        String::new(),
        "## Extract".to_string(),
    ];

    if preview.is_empty() {
        lines.push("(empty)".to_string());
    } else {
        lines.push("```text".to_string());
        lines.push(preview);
        lines.push("```".to_string());
    }

    lines.join("\n")
}

pub(crate) fn render_web_search_markdown(
    query: &str,
    results: &[serde_json::Value],
    top_extract: Option<(String, String)>,
) -> String {
    let mut lines = vec![
        "# Web Search".to_string(),
        format!("- Query: {query}"),
        format!("- Results: {}", results.len()),
        String::new(),
        "## Top Results".to_string(),
    ];

    if results.is_empty() {
        lines.push("(no results)".to_string());
    } else {
        for (idx, row) in results.iter().enumerate() {
            let title = row
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("untitled");
            let url = row.get("url").and_then(|v| v.as_str()).unwrap_or_default();
            let snippet = row
                .get("snippet")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            lines.push(format!("{}. {}", idx + 1, title));
            if !url.is_empty() {
                lines.push(format!("   - {url}"));
            }
            if !snippet.is_empty() {
                let compact = snippet.split_whitespace().collect::<Vec<_>>().join(" ");
                lines.push(format!("   - {}", truncate_inline(&compact, 220)));
            }
        }
    }

    if let Some((url, preview)) = top_extract {
        lines.push(String::new());
        lines.push(format!("## Extract ({url})"));
        lines.push("```text".to_string());
        lines.push(preview);
        lines.push("```".to_string());
    }

    lines.join("\n")
}

pub(crate) fn fetch_preview_text(output: &serde_json::Value, max_lines: usize) -> String {
    let content = output
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    normalize_web_content(content, max_lines)
}

fn normalize_web_content(content: &str, max_lines: usize) -> String {
    let mut out = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let cleaned = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
        if cleaned.is_empty() {
            continue;
        }
        out.push(cleaned);
        if out.len() >= max_lines {
            break;
        }
    }
    let mut joined = out.join("\n");
    if joined.len() > 12000 {
        joined.truncate(joined.floor_char_boundary(12000));
    }
    joined
}
