//! Tests covering features added in the v2 overhaul: agent profiles,
//! provider normalization, MCP description capping, compaction template.

use codingbuddy_agent::agent_profiles;
use codingbuddy_core::{ProviderKind, is_mcp_tool, normalize_provider_kind};

// ── Agent profiles ──

#[test]
fn profile_by_name_case_insensitive() {
    assert!(agent_profiles::profile_by_name("BUILD").is_some());
    assert!(agent_profiles::profile_by_name("Build").is_some());
    assert!(agent_profiles::profile_by_name("build").is_some());
    assert!(agent_profiles::profile_by_name("nonexistent").is_none());
}

#[test]
fn available_profiles_complete() {
    let names = agent_profiles::available_profile_names();
    assert!(names.contains(&"build"));
    assert!(names.contains(&"explore"));
    assert!(names.contains(&"plan"));
    assert!(names.contains(&"bash"));
    assert!(names.contains(&"general"));
    assert_eq!(names.len(), 5);
}

#[test]
fn handle_agent_command_valid_profile() {
    let (text, profile) = agent_profiles::handle_agent_command(Some("explore"));
    assert!(text.contains("Switched"));
    assert_eq!(profile, Some("explore".to_string()));
}

#[test]
fn handle_agent_command_invalid_profile() {
    let (text, profile) = agent_profiles::handle_agent_command(Some("nonexistent"));
    assert!(text.contains("Unknown"));
    assert!(text.contains("Available"));
    assert!(profile.is_none());
}

#[test]
fn handle_agent_command_no_arg_lists_profiles() {
    let (text, profile) = agent_profiles::handle_agent_command(None);
    assert!(text.contains("Available"));
    assert!(text.contains("build"));
    assert!(profile.is_none());
}

// ── Provider normalization ──

#[test]
fn normalize_provider_kind_covers_all_providers() {
    assert_eq!(
        normalize_provider_kind("deepseek"),
        Some(ProviderKind::Deepseek)
    );
    assert_eq!(
        normalize_provider_kind("openai"),
        Some(ProviderKind::OpenAiCompatible)
    );
    assert_eq!(
        normalize_provider_kind("anthropic"),
        Some(ProviderKind::Anthropic)
    );
    assert_eq!(
        normalize_provider_kind("claude"),
        Some(ProviderKind::Anthropic)
    );
    assert_eq!(
        normalize_provider_kind("google"),
        Some(ProviderKind::Google)
    );
    assert_eq!(
        normalize_provider_kind("gemini"),
        Some(ProviderKind::Google)
    );
    assert_eq!(normalize_provider_kind("groq"), Some(ProviderKind::Groq));
    assert_eq!(
        normalize_provider_kind("openrouter"),
        Some(ProviderKind::OpenRouter)
    );
    assert_eq!(
        normalize_provider_kind("ollama"),
        Some(ProviderKind::Ollama)
    );
    assert_eq!(normalize_provider_kind("unknown"), None);
}

#[test]
fn normalize_provider_kind_case_insensitive() {
    assert_eq!(
        normalize_provider_kind("DEEPSEEK"),
        Some(ProviderKind::Deepseek)
    );
    assert_eq!(
        normalize_provider_kind("Anthropic"),
        Some(ProviderKind::Anthropic)
    );
    assert_eq!(
        normalize_provider_kind("  ollama  "),
        Some(ProviderKind::Ollama)
    );
}

// ── MCP tool detection ──

#[test]
fn is_mcp_tool_detection() {
    assert!(is_mcp_tool("mcp__server__tool_name"));
    assert!(is_mcp_tool("mcp__github__create_issue"));
    assert!(!is_mcp_tool("fs_read"));
    assert!(!is_mcp_tool("bash_run"));
    assert!(!is_mcp_tool("mcp_single_underscore"));
}
