//! Native provider implementations for non-OpenAI-compatible APIs.
//!
//! Anthropic and Google have their own API formats that differ from OpenAI's
//! `/chat/completions` shape. These modules build native payloads, parse native
//! responses, and handle provider-specific auth/headers.

pub mod anthropic;
pub mod google;
