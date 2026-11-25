use anyhow::{Result, anyhow};
use codingbuddy_core::{
    AppConfig, ChatMessage, ChatRequest, LlmResponse, StreamCallback, ToolChoice,
};
use codingbuddy_llm::LlmClient;
use std::path::Path;

use crate::gather_context;
use crate::{ChatMode, ChatOptions};

pub fn analyze(
    llm: &(dyn LlmClient + Send + Sync),
    cfg: &AppConfig,
    workspace: &Path,
    prompt: &str,
    options: &ChatOptions,
    stream_cb: Option<StreamCallback>,
) -> Result<LlmResponse> {
    let workspace = options.repo_root_override.as_deref().unwrap_or(workspace);
    let bootstrap = gather_context::gather_for_prompt(
        workspace,
        cfg,
        prompt,
        options.mode,
        &options.additional_dirs,
        options.repo_root_override.as_deref(),
    );

    if bootstrap.repoish && bootstrap.repo_root.is_none() {
        return Err(anyhow!(
            "{}",
            bootstrap
                .unavailable_reason
                .as_deref()
                .unwrap_or("No repository detected. Run from project root or pass --repo <path>.")
        ));
    }

    if gather_context::debug_context_enabled(options.debug_context) {
        eprintln!("{}", bootstrap.debug_digest("InspectRepo", options.mode));
    }

    let system_prompt =
        build_system_prompt(options, bootstrap.repoish, bootstrap.vague_codebase_prompt);
    let user_prompt = build_user_prompt(prompt, &bootstrap);

    let mut messages = vec![ChatMessage::System {
        content: system_prompt,
    }];
    messages.extend(options.chat_history.iter().cloned());
    messages.push(ChatMessage::User {
        content: user_prompt,
    });

    let (model, is_reasoner) = if options.force_max_think {
        let m = cfg.llm.active_reasoner_model();
        let r = m.to_ascii_lowercase().contains("reasoner");
        (m, r)
    } else {
        let m = cfg.llm.active_base_model();
        let r = m.to_ascii_lowercase().contains("reasoner");
        (m, r)
    };

    let execution = codingbuddy_core::execution::ExecutionContext {
        cancellation: options.cancellation.clone().unwrap_or_default(),
        deadline: options.deadline,
        ..Default::default()
    };
    execution.check()?;
    codingbuddy_core::strip_prior_reasoning_content(&mut messages);
    let request = ChatRequest {
        model: model.clone(),
        messages,
        tools: vec![],
        tool_choice: ToolChoice::none(),
        max_tokens: codingbuddy_core::max_output_tokens_for_model(
            cfg.llm
                .active_provider_kind()
                .unwrap_or(codingbuddy_core::ProviderKind::Deepseek),
            &model,
            false,
        ),
        // Reasoner rejects temperature; deepseek-chat needs 0.0 for determinism
        temperature: if is_reasoner { None } else { Some(0.0) },
        top_p: None,
        presence_penalty: None,
        frequency_penalty: None,
        logprobs: None,
        top_logprobs: None,
        // Reasoner thinks natively — ThinkingConfig is only for deepseek-chat
        thinking: if options.force_max_think && !is_reasoner {
            Some(codingbuddy_core::ThinkingConfig::enabled(16_384))
        } else {
            None
        },
        images: options.images.clone(),
        provider_options: Default::default(),
        response_format: None,
    };

    let response = if let Some(cb) = stream_cb {
        llm.complete_chat_streaming(&request, cb)?
    } else {
        llm.complete_chat(&request)?
    };
    execution.check()?;
    Ok(response)
}

fn build_system_prompt(
    options: &ChatOptions,
    repoish_bootstrap: bool,
    vague_codebase_prompt: bool,
) -> String {
    if let Some(ref override_prompt) = options.system_prompt_override {
        return override_prompt.clone();
    }

    let mut prompt = if matches!(options.mode, ChatMode::Ask | ChatMode::Context)
        && repoish_bootstrap
    {
        let role = if matches!(options.mode, ChatMode::Context) {
            "You are a codebase analysis specialist. Focus exclusively on understanding architecture, dependencies, patterns, and structure. Do not suggest code changes or generate code."
        } else {
            "You are a precise software engineering assistant."
        };
        let mut base = vec![
            role,
            "Assume you are operating inside the current repository unless explicitly told otherwise.",
            "Use the project context below to inform your answer. Answer the question directly.",
            "Choose the structure that fits the question; do not add boilerplate section headings.",
            "Ask at most 2 targeted follow-up questions, and only when they help.",
            "Do not ask broad requests for more context before answering.",
        ];
        if vague_codebase_prompt {
            base.push(
                "For vague codebase audit prompts, summarize the baseline audit findings first, then ask at most 1 focused follow-up about the highest-risk area.",
            );
        }
        base.join("\n")
    } else {
        "You are a precise software engineering assistant.".to_string()
    };

    if let Some(ref append) = options.system_prompt_append {
        prompt.push('\n');
        prompt.push_str(append);
    }

    prompt
}

fn build_user_prompt(prompt: &str, bootstrap: &gather_context::AutoContextBootstrap) -> String {
    if !bootstrap.enabled || !bootstrap.repoish || bootstrap.packet.trim().is_empty() {
        return prompt.to_string();
    }

    format!("{prompt}\n\n---\n{}", bootstrap.packet.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_prompt_does_not_force_section_headings() {
        let options = ChatOptions {
            mode: ChatMode::Ask,
            ..Default::default()
        };
        for vague in [false, true] {
            let prompt = build_system_prompt(&options, true, vague);
            assert!(!prompt.contains("Initial Analysis"));
            assert!(!prompt.contains("Key Findings"));
        }
    }

    #[test]
    fn context_mode_system_prompt_excludes_code_gen() {
        let options = ChatOptions {
            mode: ChatMode::Context,
            ..Default::default()
        };
        let prompt = build_system_prompt(&options, true, false);
        assert!(
            prompt.contains("codebase analysis specialist"),
            "Context mode should use analysis-specialist role"
        );
        assert!(
            prompt.contains("Do not suggest code changes"),
            "Context mode should prohibit code generation"
        );
        assert!(
            !prompt.contains("precise software engineering assistant"),
            "Context mode should not use generic assistant role"
        );
    }

    #[test]
    fn ask_mode_system_prompt_uses_generic_role() {
        let options = ChatOptions {
            mode: ChatMode::Ask,
            ..Default::default()
        };
        let prompt = build_system_prompt(&options, true, false);
        assert!(
            prompt.contains("precise software engineering assistant"),
            "Ask mode should use generic assistant role"
        );
        assert!(
            !prompt.contains("codebase analysis specialist"),
            "Ask mode should not use context-specialist role"
        );
    }
}
