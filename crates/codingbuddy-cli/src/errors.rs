//! Enhanced error handling and user guidance for CodingBuddy

use anyhow::Error;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Enhanced error with user-friendly message and recovery suggestions
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnhancedError {
    pub title: String,
    pub message: String,
    pub suggestions: Vec<String>,
    pub error_type: ErrorType,
    pub context: Option<String>,
}

/// Types of errors for better categorization
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ErrorType {
    /// Configuration errors (missing API key, invalid settings)
    Configuration,
    /// Network errors (timeout, connection issues)
    Network,
    /// Permission errors (file access, tool restrictions)
    Permission,
    /// Runtime errors (tool failures, parsing errors)
    Runtime,
    /// Validation errors (invalid input, constraints)
    Validation,
    /// Resource errors (memory, disk space)
    Resource,
    /// Unknown or uncategorized errors
    Unknown,
}

impl EnhancedError {
    /// Create a new enhanced error
    pub fn new(
        title: impl Into<String>,
        message: impl Into<String>,
        error_type: ErrorType,
    ) -> Self {
        Self {
            title: title.into(),
            message: message.into(),
            suggestions: Vec::new(),
            error_type,
            context: None,
        }
    }

    /// Add a recovery suggestion
    pub fn with_suggestion(mut self, suggestion: impl Into<String>) -> Self {
        self.suggestions.push(suggestion.into());
        self
    }

    /// Add multiple recovery suggestions
    pub fn with_suggestions(mut self, suggestions: Vec<String>) -> Self {
        self.suggestions.extend(suggestions);
        self
    }

    /// Add context information
    pub fn with_context(mut self, context: impl Into<String>) -> Self {
        self.context = Some(context.into());
        self
    }

    /// Convert to anyhow::Error
    pub fn into_error(self) -> Error {
        Error::new(self)
    }

    /// Format error for display
    pub fn format(&self, verbose: bool) -> String {
        let mut output = String::new();

        // Title
        output.push_str(&format!("{}: {}\n", self.error_type.emoji(), self.title));

        // Message
        output.push_str(&format!("  {}\n", self.message));

        // Context (if verbose)
        if verbose && let Some(context) = &self.context {
            output.push_str(&format!("\n  Context: {}\n", context));
        }

        // Suggestions
        if !self.suggestions.is_empty() {
            output.push_str("\n  Suggestions:\n");
            for (i, suggestion) in self.suggestions.iter().enumerate() {
                output.push_str(&format!("    {}. {}\n", i + 1, suggestion));
            }
        }

        output
    }
}

impl ErrorType {
    /// Get emoji for error type
    pub fn emoji(&self) -> &'static str {
        match self {
            ErrorType::Configuration => "🔧",
            ErrorType::Network => "🌐",
            ErrorType::Permission => "🔒",
            ErrorType::Runtime => "⚡",
            ErrorType::Validation => "📋",
            ErrorType::Resource => "💾",
            ErrorType::Unknown => "❓",
        }
    }

    /// Get color code for error type
    pub fn color(&self) -> &'static str {
        match self {
            ErrorType::Configuration => "yellow",
            ErrorType::Network => "blue",
            ErrorType::Permission => "red",
            ErrorType::Runtime => "magenta",
            ErrorType::Validation => "cyan",
            ErrorType::Resource => "yellow",
            ErrorType::Unknown => "gray",
        }
    }
}

impl fmt::Display for EnhancedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.format(false))
    }
}

impl std::error::Error for EnhancedError {}

/// Error handler for providing user-friendly error messages
pub struct ErrorHandler {
    verbose: bool,
    show_suggestions: bool,
}

impl Default for ErrorHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl ErrorHandler {
    /// Create a new error handler
    pub fn new() -> Self {
        Self {
            verbose: false,
            show_suggestions: true,
        }
    }

    /// Set verbose mode
    pub fn verbose(mut self, verbose: bool) -> Self {
        self.verbose = verbose;
        self
    }

    /// Set whether to show suggestions
    pub fn show_suggestions(mut self, show: bool) -> Self {
        self.show_suggestions = show;
        self
    }

    /// Handle an error and print user-friendly message
    pub fn handle(&self, error: &Error) -> String {
        // Try to extract enhanced error
        if let Some(enhanced) = error.downcast_ref::<EnhancedError>() {
            return enhanced.format(self.verbose);
        }

        // Convert generic error to enhanced error
        let error_str = error.to_string();
        let enhanced = self.classify_error(&error_str);
        enhanced.format(self.verbose)
    }

    /// Classify error based on message patterns
    fn classify_error(&self, error_message: &str) -> EnhancedError {
        let lower_error = error_message.to_lowercase();

        // Configuration errors
        if lower_error.contains("api key") || lower_error.contains("configuration") {
            return EnhancedError::new(
                "Configuration Error",
                error_message,
                ErrorType::Configuration,
            )
            .with_suggestions(vec![
                "Check your .codingbuddy/settings.json file".to_string(),
                "Set the API key for your provider (e.g. DEEPSEEK_API_KEY, OPENAI_API_KEY, ANTHROPIC_API_KEY)".to_string(),
                "Run `codingbuddy init` to initialize configuration".to_string(),
            ]);
        }

        // Auth errors (401, unauthorized, invalid key)
        if lower_error.contains("status: 401")
            || lower_error.contains("status 401")
            || lower_error.contains("unauthorized")
            || lower_error.contains("invalid api key")
            || lower_error.contains("invalid_api_key")
        {
            return EnhancedError::new("Authentication Error", error_message, ErrorType::Network)
                .with_suggestions(vec![
                    "Your API key may be expired or invalid".to_string(),
                    "Run `codingbuddy config set api_key` to update it".to_string(),
                    "Check that the correct provider is configured in settings.json".to_string(),
                ]);
        }

        // Rate limit errors (429)
        if lower_error.contains("status: 429")
            || lower_error.contains("status 429")
            || lower_error.contains("rate limit")
            || lower_error.contains("too many requests")
        {
            return EnhancedError::new("Rate Limit Error", error_message, ErrorType::Network)
                .with_suggestions(vec![
                    "You've hit the API rate limit — wait a moment and retry".to_string(),
                    "Consider using a model with higher rate limits".to_string(),
                ]);
        }

        // Network errors
        if lower_error.contains("network")
            || lower_error.contains("timeout")
            || lower_error.contains("connection")
        {
            return EnhancedError::new("Network Error", error_message, ErrorType::Network)
                .with_suggestions(vec![
                    "Check your internet connection".to_string(),
                    "Verify the API endpoint is accessible".to_string(),
                    "Try again in a few moments".to_string(),
                ]);
        }

        // Permission errors
        if lower_error.contains("permission")
            || lower_error.contains("access")
            || lower_error.contains("denied")
        {
            return EnhancedError::new("Permission Error", error_message, ErrorType::Permission)
                .with_suggestions(vec![
                    "Check file permissions".to_string(),
                    "Run with appropriate user privileges".to_string(),
                    "Use --permission-mode flag to adjust permissions".to_string(),
                ]);
        }

        // Default unknown error
        EnhancedError::new("Error", error_message, ErrorType::Unknown)
            .with_suggestion("Check the documentation or report this issue".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    #[test]
    fn test_enhanced_error_formatting() {
        let error = EnhancedError::new("Test Error", "Something went wrong", ErrorType::Runtime)
            .with_suggestion("Try again");
        let formatted = error.format(false);
        assert!(formatted.contains("Test Error"));
        assert!(formatted.contains("Something went wrong"));
    }

    #[test]
    fn test_error_handler() {
        let handler = ErrorHandler::new();
        let err = anyhow!("Connection refused");
        let output = handler.handle(&err);
        assert!(output.contains("Network Error") || output.contains("Error"));
    }

    #[test]
    fn test_enhanced_error_display() {
        let error = EnhancedError::new("API Error", "Rate limited", ErrorType::Network);
        let display = format!("{error}");
        assert!(display.contains("API Error"));
    }
}
