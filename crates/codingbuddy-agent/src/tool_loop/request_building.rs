//! Request construction for the tool-use loop.

use codingbuddy_core::{
    ChatMessage, ChatRequest, ImageContent, ProviderKind, RuntimeToolMetadata, TaskPhase,
    ThinkingConfig, ToolChoice, ToolDefinition, model_capabilities,
};

pub(super) struct RequestRoute {
    pub model: String,
    pub thinking: Option<ThinkingConfig>,
    pub max_tokens: u32,
}

pub(super) fn filter_tools_for_request(
    tools: &[ToolDefinition],
    read_only: bool,
    phase: Option<TaskPhase>,
    mut metadata_for: impl FnMut(&str) -> RuntimeToolMetadata,
) -> Vec<ToolDefinition> {
    tools
        .iter()
        .filter(|tool| {
            let metadata = metadata_for(&tool.function.name);
            (!read_only || metadata.read_only)
                && phase.is_none_or(|phase| metadata.is_allowed_in_phase(phase))
        })
        .cloned()
        .collect()
}

pub(super) fn build_chat_request(
    messages: Vec<ChatMessage>,
    tools: Vec<ToolDefinition>,
    route: RequestRoute,
    provider_kind: ProviderKind,
    configured_temperature: Option<f32>,
    images: Vec<ImageContent>,
) -> ChatRequest {
    let caps = model_capabilities(provider_kind, &route.model);
    let temperature = if caps.supports_reasoning_mode {
        None
    } else {
        configured_temperature
    };
    let tool_choice = ToolChoice::auto();

    ChatRequest {
        model: route.model,
        messages,
        tools,
        tool_choice,
        max_tokens: route.max_tokens,
        temperature,
        top_p: None,
        presence_penalty: None,
        frequency_penalty: None,
        logprobs: None,
        top_logprobs: None,
        thinking: route.thinking,
        images,
        provider_options: Default::default(),
        response_format: None,
    }
}
