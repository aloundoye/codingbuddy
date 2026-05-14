use anyhow::Result;
use codingbuddy_core::{
    RuntimeToolMetadata, TaskPhase, ToolCall, ToolDefinition, ToolPermissionMatcher, ToolResult,
    ToolResultSizePolicy,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

pub type ToolValidator =
    Arc<dyn Fn(&serde_json::Value) -> std::result::Result<(), String> + Send + Sync>;
pub type ToolExecutor = Arc<dyn Fn(ToolCall) -> Result<ToolResult> + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPermissionTarget {
    pub matcher: ToolPermissionMatcher,
    pub label: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolTruncationPolicy {
    pub result_size_policy: ToolResultSizePolicy,
    pub max_result_chars: usize,
}

#[derive(Clone)]
pub struct RegisteredTool {
    pub definition: ToolDefinition,
    pub metadata: RuntimeToolMetadata,
    pub validator: Option<ToolValidator>,
    pub executor: Option<ToolExecutor>,
    pub permission_targets: Vec<ToolPermissionTarget>,
    pub truncation: ToolTruncationPolicy,
}

impl fmt::Debug for RegisteredTool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegisteredTool")
            .field("name", &self.definition.function.name)
            .field("metadata", &self.metadata)
            .field("has_validator", &self.validator.is_some())
            .field("has_executor", &self.executor.is_some())
            .field("permission_targets", &self.permission_targets)
            .field("truncation", &self.truncation)
            .finish()
    }
}

impl RegisteredTool {
    #[must_use]
    pub fn from_definition(definition: ToolDefinition) -> Self {
        let metadata = RuntimeToolMetadata::for_api_name(&definition.function.name);
        Self::new(definition, metadata)
    }

    #[must_use]
    pub fn new(definition: ToolDefinition, metadata: RuntimeToolMetadata) -> Self {
        let truncation = ToolTruncationPolicy {
            result_size_policy: metadata.result_size_policy,
            max_result_chars: metadata.max_result_chars(),
        };
        let permission_targets = vec![ToolPermissionTarget {
            matcher: metadata.permission_matcher,
            label: Some(definition.function.name.clone()),
        }];
        Self {
            definition,
            metadata,
            validator: None,
            executor: None,
            permission_targets,
            truncation,
        }
    }

    #[must_use]
    pub fn api_name(&self) -> &str {
        &self.definition.function.name
    }

    #[must_use]
    pub fn is_read_only(&self) -> bool {
        self.metadata.read_only
    }

    #[must_use]
    pub fn is_allowed_in_phase(&self, phase: TaskPhase) -> bool {
        self.metadata.is_allowed_in_phase(phase)
    }

    #[must_use]
    pub fn with_validator(mut self, validator: ToolValidator) -> Self {
        self.validator = Some(validator);
        self
    }

    #[must_use]
    pub fn with_executor(mut self, executor: ToolExecutor) -> Self {
        self.executor = Some(executor);
        self
    }

    pub fn validate_args(&self, args: &serde_json::Value) -> std::result::Result<(), String> {
        validate_schema(&self.definition, args)?;
        if let Some(validator) = &self.validator {
            validator(args)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, RegisteredTool>,
    disabled_tools: BTreeSet<String>,
}

impl ToolRegistry {
    #[must_use]
    pub fn from_definitions(definitions: Vec<ToolDefinition>) -> Self {
        let mut registry = Self::default();
        for definition in definitions {
            registry.insert(RegisteredTool::from_definition(definition));
        }
        registry
    }

    pub fn insert(&mut self, tool: RegisteredTool) {
        self.tools.insert(tool.api_name().to_string(), tool);
    }

    pub fn disable(&mut self, tool_name: impl Into<String>) {
        self.disabled_tools.insert(tool_name.into());
    }

    pub fn enable(&mut self, tool_name: &str) {
        self.disabled_tools.remove(tool_name);
    }

    #[must_use]
    pub fn get(&self, tool_name: &str) -> Option<&RegisteredTool> {
        self.tools
            .get(tool_name)
            .filter(|_| !self.disabled_tools.contains(tool_name))
    }

    #[must_use]
    pub fn all(&self) -> Vec<&RegisteredTool> {
        self.tools
            .iter()
            .filter(|(name, _)| !self.disabled_tools.contains(*name))
            .map(|(_, tool)| tool)
            .collect()
    }

    #[must_use]
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.all()
            .into_iter()
            .map(|tool| tool.definition.clone())
            .collect()
    }

    #[must_use]
    pub fn filter_phase(&self, phase: TaskPhase) -> Vec<&RegisteredTool> {
        self.all()
            .into_iter()
            .filter(|tool| tool.is_allowed_in_phase(phase))
            .collect()
    }

    #[must_use]
    pub fn filter_read_only(&self) -> Vec<&RegisteredTool> {
        self.all()
            .into_iter()
            .filter(|tool| tool.is_read_only())
            .collect()
    }
}

fn validate_schema(
    definition: &ToolDefinition,
    args: &serde_json::Value,
) -> std::result::Result<(), String> {
    let schema = &definition.function.parameters;
    if schema.is_null() || schema.as_object().is_some_and(|o| o.is_empty()) {
        return Ok(());
    }
    let validator = match jsonschema::validator_for(schema) {
        Ok(validator) => validator,
        Err(_) => return Ok(()),
    };
    let errors: Vec<String> = validator
        .iter_errors(args)
        .map(|error| {
            let path = error.instance_path.to_string();
            if path.is_empty() {
                error.to_string()
            } else {
                format!("{path}: {error}")
            }
        })
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Invalid arguments for tool '{}': {}",
            definition.function.name,
            errors.join("; ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_definitions;
    use codingbuddy_core::TaskPhase;

    #[test]
    fn registry_serves_definitions_and_filters() {
        let registry = ToolRegistry::from_definitions(tool_definitions());
        assert!(registry.get("fs_read").is_some());
        assert!(
            registry
                .filter_read_only()
                .iter()
                .any(|tool| tool.api_name() == "fs_read")
        );
        assert!(
            registry
                .filter_phase(TaskPhase::Explore)
                .iter()
                .any(|tool| tool.api_name() == "fs_read")
        );
    }

    #[test]
    fn registry_disabled_tools_are_hidden_from_consumers() {
        let mut registry = ToolRegistry::from_definitions(tool_definitions());
        registry.disable("fs_read");
        assert!(registry.get("fs_read").is_none());
        assert!(
            !registry
                .definitions()
                .iter()
                .any(|tool| tool.function.name == "fs_read")
        );
        registry.enable("fs_read");
        assert!(registry.get("fs_read").is_some());
    }

    #[test]
    fn registered_tool_schema_validation_reports_field_errors() {
        let registry = ToolRegistry::from_definitions(tool_definitions());
        let tool = registry.get("fs_read").expect("fs_read registered");
        let error = tool
            .validate_args(&serde_json::json!({}))
            .expect_err("missing path rejected");
        assert!(error.contains("fs_read"));
    }
}
