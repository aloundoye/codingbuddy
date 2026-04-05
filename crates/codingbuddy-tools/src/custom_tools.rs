//! User-defined custom tools loaded from `.codingbuddy/tools/*.toml`.
//!
//! Custom tools let users extend codingbuddy with shell-command-based tools
//! without writing a full plugin. Each `.toml` file defines one tool with a
//! name, description, JSON Schema parameters, and a shell command template.

use anyhow::Result;
use codingbuddy_core::{FunctionDefinition, ToolDefinition};
use serde::Deserialize;
use serde_json::json;
use std::path::Path;

/// A custom tool definition loaded from a TOML file.
#[derive(Debug, Clone, Deserialize)]
pub struct CustomToolDef {
    pub name: String,
    pub description: String,
    /// Shell command template. Use `{{param_name}}` for parameter substitution.
    pub command: String,
    /// Whether this tool is read-only (default: false).
    #[serde(default)]
    pub read_only: bool,
    /// Whether this tool can run in parallel (default: false).
    #[serde(default)]
    pub concurrency_safe: bool,
    /// JSON Schema for parameters (optional — defaults to empty object).
    #[serde(default = "default_parameters")]
    pub parameters: serde_json::Value,
}

fn default_parameters() -> serde_json::Value {
    json!({"type": "object", "properties": {}})
}

/// Load all custom tool definitions from `.codingbuddy/tools/*.toml`.
pub fn load_custom_tools(workspace: &Path) -> Vec<CustomToolDef> {
    let tools_dir = workspace.join(".codingbuddy/tools");
    if !tools_dir.is_dir() {
        return Vec::new();
    }

    let mut tools = Vec::new();
    let entries = match std::fs::read_dir(&tools_dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "toml") {
            match load_custom_tool(&path) {
                Ok(tool) => tools.push(tool),
                Err(e) => {
                    eprintln!("[custom-tools] failed to load {}: {e}", path.display());
                }
            }
        }
    }

    tools
}

fn load_custom_tool(path: &Path) -> Result<CustomToolDef> {
    let content = std::fs::read_to_string(path)?;
    let tool: CustomToolDef = toml::from_str(&content)?;
    if tool.name.is_empty() {
        anyhow::bail!("custom tool missing 'name' field");
    }
    if tool.command.is_empty() {
        anyhow::bail!("custom tool '{}' missing 'command' field", tool.name);
    }
    Ok(tool)
}

/// Convert custom tool definitions to standard ToolDefinitions for the catalog.
pub fn custom_tool_definitions(workspace: &Path) -> Vec<ToolDefinition> {
    load_custom_tools(workspace)
        .into_iter()
        .map(|t| {
            let api_name = format!("custom__{}", t.name.replace(['-', ' '], "_"));
            ToolDefinition {
                tool_type: "function".to_string(),
                function: FunctionDefinition {
                    name: api_name,
                    description: format!("[Custom] {}", t.description),
                    parameters: t.parameters,
                    strict: None,
                },
            }
        })
        .collect()
}

/// Render a command template by substituting `{{param}}` with argument values.
pub fn render_command(template: &str, args: &serde_json::Value) -> String {
    let mut cmd = template.to_string();
    if let Some(obj) = args.as_object() {
        for (key, value) in obj {
            let replacement = match value {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            // Shell-escape the value to prevent injection
            let escaped = shell_escape(&replacement);
            cmd = cmd.replace(&format!("{{{{{key}}}}}"), &escaped);
        }
    }
    cmd
}

fn shell_escape(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    if s.chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.' || c == '/')
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_simple_template() {
        let args = json!({"environment": "staging", "version": "1.2.3"});
        let result = render_command("make deploy ENV={{environment}} VER={{version}}", &args);
        assert_eq!(result, "make deploy ENV=staging VER=1.2.3");
    }

    #[test]
    fn render_escapes_special_chars() {
        let args = json!({"msg": "hello world; rm -rf /"});
        let result = render_command("echo {{msg}}", &args);
        assert!(result.contains("'hello world; rm -rf /'"));
    }

    #[test]
    fn render_empty_args() {
        let result = render_command("echo hello", &json!({}));
        assert_eq!(result, "echo hello");
    }

    #[test]
    fn load_from_nonexistent_dir() {
        let tools = load_custom_tools(Path::new("/nonexistent"));
        assert!(tools.is_empty());
    }

    #[test]
    fn parse_toml_definition() {
        let toml_str = r#"
name = "deploy"
description = "Deploy the app"
command = "make deploy ENV={{environment}}"
read_only = false

[parameters.properties.environment]
type = "string"
description = "Target environment"
"#;
        let tool: CustomToolDef = toml::from_str(toml_str).unwrap();
        assert_eq!(tool.name, "deploy");
        assert_eq!(tool.command, "make deploy ENV={{environment}}");
        assert!(!tool.read_only);
    }

    #[test]
    fn custom_tool_definitions_adds_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let tools_dir = dir.path().join(".codingbuddy/tools");
        std::fs::create_dir_all(&tools_dir).unwrap();
        std::fs::write(
            tools_dir.join("greet.toml"),
            r#"
name = "greet"
description = "Say hello"
command = "echo Hello {{name}}"
read_only = true
"#,
        )
        .unwrap();

        let defs = custom_tool_definitions(dir.path());
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].function.name, "custom__greet");
        assert!(defs[0].function.description.contains("[Custom]"));
    }
}
