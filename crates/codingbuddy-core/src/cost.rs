//! Model pricing for cost tracking.
//!
//! Provides per-model pricing (USD per million tokens) used by the cost tracker
//! in the tool-use loop to estimate cumulative session cost.

/// Pricing for a model (USD per million tokens).
#[derive(Debug, Clone, Copy)]
pub struct ModelPricing {
    pub input_per_million: f64,
    pub output_per_million: f64,
    /// Discount factor for cache-hit tokens (0.0 = no discount, 0.5 = 50% cost).
    pub cache_discount: f64,
}

/// Look up pricing for a model by name. Falls back to DeepSeek Chat pricing.
pub fn get_pricing(model: &str) -> ModelPricing {
    let lower = model.to_ascii_lowercase();
    let (input, output, cache) = match lower.as_str() {
        // DeepSeek
        m if m.contains("deepseek-chat") || m.contains("deepseek-v3") => (0.27, 1.10, 0.1),
        m if m.contains("deepseek-reasoner") || m.contains("deepseek-r1") => (0.55, 2.19, 0.1),
        // OpenAI
        m if m.contains("gpt-4.1-mini") || m.contains("gpt-4o-mini") => (0.40, 1.60, 0.5),
        m if m.contains("gpt-4.1-nano") => (0.10, 0.40, 0.5),
        m if m.contains("gpt-4.1") => (2.00, 8.00, 0.5),
        m if m.contains("gpt-4o") => (2.50, 10.00, 0.5),
        m if m.contains("o4-mini") => (1.10, 4.40, 0.5),
        m if m.contains("o3-mini") => (1.10, 4.40, 0.5),
        m if m.contains("o3") => (2.00, 8.00, 0.5),
        m if m.contains("o1-mini") => (1.10, 4.40, 0.5),
        m if m.contains("o1") => (15.00, 60.00, 0.5),
        // Anthropic
        m if m.contains("claude-sonnet-4") || m.contains("claude-3-5-sonnet") => (3.00, 15.00, 0.1),
        m if m.contains("claude-haiku-4") || m.contains("claude-3-5-haiku") => (0.80, 4.00, 0.1),
        m if m.contains("claude-opus-4") => (15.00, 75.00, 0.1),
        // Google
        m if m.contains("gemini-2.5-pro") => (1.25, 10.00, 0.25),
        m if m.contains("gemini-2.5-flash") => (0.15, 0.60, 0.25),
        m if m.contains("gemini-2.0") => (0.10, 0.40, 0.25),
        // Qwen
        m if m.contains("qwen") => (0.14, 0.28, 0.1),
        // Groq (hosted inference)
        m if m.contains("llama") || m.contains("mixtral") => (0.05, 0.08, 0.0),
        // Mistral
        m if m.contains("mistral-large") => (2.00, 6.00, 0.0),
        m if m.contains("mistral") || m.contains("codestral") => (0.30, 0.90, 0.0),
        // Default: DeepSeek Chat
        _ => (0.27, 1.10, 0.1),
    };
    ModelPricing {
        input_per_million: input,
        output_per_million: output,
        cache_discount: cache,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_sonnet_pricing() {
        let p = get_pricing("claude-sonnet-4-20250514");
        assert!((p.input_per_million - 3.0).abs() < 0.01);
        assert!((p.output_per_million - 15.0).abs() < 0.01);
    }

    #[test]
    fn gpt4o_pricing() {
        let p = get_pricing("gpt-4o");
        assert!((p.input_per_million - 2.5).abs() < 0.01);
    }

    #[test]
    fn deepseek_default_pricing() {
        let p = get_pricing("deepseek-chat");
        assert!((p.input_per_million - 0.27).abs() < 0.01);
    }

    #[test]
    fn unknown_model_falls_back() {
        let p = get_pricing("some-unknown-model");
        assert!((p.input_per_million - 0.27).abs() < 0.01);
    }

    #[test]
    fn gemini_flash_pricing() {
        let p = get_pricing("gemini-2.5-flash");
        assert!((p.input_per_million - 0.15).abs() < 0.01);
    }
}
