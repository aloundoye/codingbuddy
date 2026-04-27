use crate::{TaskPhase, ToolName};

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum ToolTier {
    Core,
    Contextual,
    #[default]
    Extended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolAgentRole {
    Build,
    Explore,
    Plan,
    Bash,
    General,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ToolPhaseAccess {
    pub explore: bool,
    pub plan: bool,
    pub execute: bool,
    pub verify: bool,
}

impl Default for ToolPhaseAccess {
    fn default() -> Self {
        Self {
            explore: false,
            plan: false,
            execute: true,
            verify: false,
        }
    }
}

/// Behavior when the user interrupts during tool execution.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum InterruptBehavior {
    /// Abort immediately (default for reads).
    Cancel,
    /// Finish before handling interrupt (default for writes).
    #[default]
    Block,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolMetadata {
    pub read_only: bool,
    pub phase_access: ToolPhaseAccess,
    pub agent_level: bool,
    pub review_blocked: bool,
    pub tier: ToolTier,
    pub allowed_roles: &'static [ToolAgentRole],
    /// Can run in parallel with other concurrency-safe tools.
    pub concurrency_safe: bool,
    /// Tool performs destructive operations (delete, overwrite).
    pub destructive: bool,
    /// Tool is deferred — not sent to LLM by default, discovered via tool_search.
    pub deferred: bool,
    /// Maximum result size in characters before truncation.
    pub max_result_chars: usize,
    /// What to do when user interrupts during execution.
    pub interrupt_behavior: InterruptBehavior,
}

/// Runtime-only metadata for any tool visible to the agent.
///
/// Built-in tools are derived from [`ToolName::metadata`]. Dynamic tools
/// (MCP/plugin/custom/unknown) default to a conservative Execute-only,
/// approval-required profile unless a trusted adapter supplies narrower
/// metadata in the future.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct RuntimeToolMetadata {
    pub read_only: bool,
    pub phase_access: ToolPhaseAccess,
    pub agent_level: bool,
    pub review_blocked: bool,
    pub tier: ToolTier,
    pub allowed_roles: Vec<ToolAgentRole>,
    pub concurrency_safe: bool,
    pub destructive: bool,
    pub deferred: bool,
    pub max_result_chars: usize,
    pub interrupt_behavior: InterruptBehavior,
    pub approval_required: bool,
    pub dynamic: bool,
    pub trust_level: DynamicToolTrust,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DynamicToolTrust {
    BuiltIn,
    TrustedReadOnly,
    #[default]
    Untrusted,
}

impl Default for RuntimeToolMetadata {
    fn default() -> Self {
        Self::restricted_dynamic()
    }
}

impl From<ToolMetadata> for RuntimeToolMetadata {
    fn from(metadata: ToolMetadata) -> Self {
        Self {
            read_only: metadata.read_only,
            phase_access: metadata.phase_access,
            agent_level: metadata.agent_level,
            review_blocked: metadata.review_blocked,
            tier: metadata.tier,
            allowed_roles: metadata.allowed_roles.to_vec(),
            concurrency_safe: metadata.concurrency_safe,
            destructive: metadata.destructive,
            deferred: metadata.deferred,
            max_result_chars: metadata.max_result_chars,
            interrupt_behavior: metadata.interrupt_behavior,
            approval_required: !metadata.read_only || metadata.review_blocked,
            dynamic: false,
            trust_level: DynamicToolTrust::BuiltIn,
        }
    }
}

impl RuntimeToolMetadata {
    #[must_use]
    pub fn restricted_dynamic() -> Self {
        Self {
            read_only: false,
            phase_access: ToolPhaseAccess::default(),
            agent_level: false,
            review_blocked: true,
            tier: ToolTier::Extended,
            allowed_roles: vec![ToolAgentRole::Build, ToolAgentRole::General],
            concurrency_safe: false,
            destructive: true,
            deferred: false,
            max_result_chars: 30_000,
            interrupt_behavior: InterruptBehavior::Block,
            approval_required: true,
            dynamic: true,
            trust_level: DynamicToolTrust::Untrusted,
        }
    }

    #[must_use]
    pub fn trusted_read_only_dynamic() -> Self {
        Self {
            read_only: true,
            phase_access: READ_ALL_PHASES,
            agent_level: false,
            review_blocked: false,
            tier: ToolTier::Extended,
            allowed_roles: ALL_ROLES.to_vec(),
            concurrency_safe: true,
            destructive: false,
            deferred: false,
            max_result_chars: 30_000,
            interrupt_behavior: InterruptBehavior::Cancel,
            approval_required: false,
            dynamic: true,
            trust_level: DynamicToolTrust::TrustedReadOnly,
        }
    }

    #[must_use]
    pub fn for_api_name(name: &str) -> Self {
        ToolName::from_api_name(name)
            .map(|tool| tool.metadata().into())
            .unwrap_or_else(Self::restricted_dynamic)
    }

    #[must_use]
    pub fn is_allowed_for_role(&self, role: ToolAgentRole) -> bool {
        self.allowed_roles.contains(&role)
    }

    #[must_use]
    pub fn is_allowed_in_phase(&self, phase: TaskPhase) -> bool {
        match phase {
            TaskPhase::Explore => self.phase_access.explore,
            TaskPhase::Plan => self.phase_access.plan,
            TaskPhase::Execute => self.phase_access.execute,
            TaskPhase::Verify => self.phase_access.verify,
        }
    }
}

const ALL_ROLES: &[ToolAgentRole] = &[
    ToolAgentRole::Build,
    ToolAgentRole::Explore,
    ToolAgentRole::Plan,
    ToolAgentRole::Bash,
    ToolAgentRole::General,
];
const BUILD_GENERAL: &[ToolAgentRole] = &[ToolAgentRole::Build, ToolAgentRole::General];
const BUILD_PLAN_GENERAL: &[ToolAgentRole] = &[
    ToolAgentRole::Build,
    ToolAgentRole::Plan,
    ToolAgentRole::General,
];
const BUILD_BASH_GENERAL: &[ToolAgentRole] = &[
    ToolAgentRole::Build,
    ToolAgentRole::Bash,
    ToolAgentRole::General,
];
const BUILD_PLAN_BASH_GENERAL: &[ToolAgentRole] = &[
    ToolAgentRole::Build,
    ToolAgentRole::Plan,
    ToolAgentRole::Bash,
    ToolAgentRole::General,
];
const WEB_ROLES: &[ToolAgentRole] = &[
    ToolAgentRole::Explore,
    ToolAgentRole::Plan,
    ToolAgentRole::General,
];
const NOTEBOOK_READ_ROLES: &[ToolAgentRole] = &[
    ToolAgentRole::Build,
    ToolAgentRole::Explore,
    ToolAgentRole::Plan,
    ToolAgentRole::General,
];

const READ_ALL_PHASES: ToolPhaseAccess = ToolPhaseAccess {
    explore: true,
    plan: true,
    execute: true,
    verify: true,
};
const EXECUTE_ONLY: ToolPhaseAccess = ToolPhaseAccess {
    explore: false,
    plan: false,
    execute: true,
    verify: false,
};
const EXECUTE_AND_VERIFY: ToolPhaseAccess = ToolPhaseAccess {
    explore: false,
    plan: false,
    execute: true,
    verify: true,
};
const PLAN_EXECUTE_VERIFY: ToolPhaseAccess = ToolPhaseAccess {
    explore: false,
    plan: true,
    execute: true,
    verify: true,
};
const EXPLORE_EXECUTE_VERIFY: ToolPhaseAccess = ToolPhaseAccess {
    explore: true,
    plan: false,
    execute: true,
    verify: true,
};
const PLAN_ONLY: ToolPhaseAccess = ToolPhaseAccess {
    explore: false,
    plan: true,
    execute: false,
    verify: false,
};

impl ToolName {
    #[must_use]
    pub fn metadata(&self) -> ToolMetadata {
        match self {
            // ── Read-only tools: concurrency_safe, not destructive ──
            Self::FsRead
            | Self::FsList
            | Self::FsGlob
            | Self::FsGrep
            | Self::GitStatus
            | Self::GitDiff
            | Self::GitShow
            | Self::IndexQuery
            | Self::DiagnosticsCheck
            | Self::Batch
            | Self::LspHover
            | Self::LspDefinition
            | Self::LspReferences
            | Self::LspSymbols
            | Self::GithubListIssues
            | Self::GithubViewPr
            | Self::UserQuestion
            | Self::TaskGet
            | Self::TaskList
            | Self::TodoRead
            | Self::TaskOutput
            | Self::ExtendedThinking
            | Self::ToolSearch => ToolMetadata {
                read_only: true,
                phase_access: READ_ALL_PHASES,
                agent_level: matches!(
                    self,
                    Self::UserQuestion
                        | Self::TaskGet
                        | Self::TaskList
                        | Self::TodoRead
                        | Self::TaskOutput
                        | Self::ExtendedThinking
                        | Self::ToolSearch
                ),
                review_blocked: false,
                tier: match self {
                    Self::ExtendedThinking => ToolTier::Contextual,
                    Self::ToolSearch => ToolTier::Core,
                    Self::GitStatus | Self::GitDiff | Self::GitShow => ToolTier::Contextual,
                    Self::IndexQuery | Self::DiagnosticsCheck => ToolTier::Contextual,
                    _ => ToolTier::Core,
                },
                allowed_roles: ALL_ROLES,
                concurrency_safe: true,
                destructive: false,
                deferred: matches!(
                    self,
                    Self::GitShow
                        | Self::IndexQuery
                        | Self::DiagnosticsCheck
                        | Self::ExtendedThinking
                ),
                max_result_chars: match self {
                    Self::FsRead => 100_000,
                    Self::FsGrep => 50_000,
                    Self::FsGlob => 20_000,
                    _ => 50_000,
                },
                interrupt_behavior: InterruptBehavior::Cancel,
            },
            Self::WebFetch | Self::WebSearch => ToolMetadata {
                read_only: true,
                phase_access: READ_ALL_PHASES,
                agent_level: false,
                review_blocked: false,
                tier: ToolTier::Contextual,
                allowed_roles: WEB_ROLES,
                concurrency_safe: true,
                destructive: false,
                deferred: false,
                max_result_chars: 30_000,
                interrupt_behavior: InterruptBehavior::Cancel,
            },
            Self::NotebookRead => ToolMetadata {
                read_only: true,
                phase_access: READ_ALL_PHASES,
                agent_level: false,
                review_blocked: false,
                tier: ToolTier::Extended,
                allowed_roles: NOTEBOOK_READ_ROLES,
                concurrency_safe: true,
                destructive: false,
                deferred: true,
                max_result_chars: 50_000,
                interrupt_behavior: InterruptBehavior::Cancel,
            },
            // ── Write tools: NOT concurrency_safe, block on interrupt ──
            Self::FsWrite
            | Self::FsEdit
            | Self::MultiEdit
            | Self::PatchStage
            | Self::PatchApply
            | Self::PatchDirect
            | Self::NotebookEdit
            | Self::GithubCreatePr => ToolMetadata {
                read_only: false,
                phase_access: EXECUTE_ONLY,
                agent_level: false,
                review_blocked: true,
                tier: match self {
                    Self::FsWrite | Self::FsEdit | Self::MultiEdit => ToolTier::Core,
                    _ => ToolTier::Extended,
                },
                allowed_roles: BUILD_GENERAL,
                concurrency_safe: false,
                destructive: false,
                deferred: matches!(
                    self,
                    Self::PatchStage | Self::PatchApply | Self::PatchDirect | Self::NotebookEdit
                ),
                max_result_chars: 50_000,
                interrupt_behavior: InterruptBehavior::Block,
            },
            Self::BashRun => ToolMetadata {
                read_only: false,
                phase_access: EXECUTE_AND_VERIFY,
                agent_level: false,
                review_blocked: true,
                tier: ToolTier::Core,
                allowed_roles: BUILD_BASH_GENERAL,
                concurrency_safe: false,
                destructive: true,
                deferred: false,
                max_result_chars: 50_000,
                interrupt_behavior: InterruptBehavior::Block,
            },
            // ── Agent-level tools: NOT concurrency_safe ──
            Self::TaskCreate | Self::TaskUpdate | Self::TodoWrite | Self::TaskStop => {
                ToolMetadata {
                    read_only: false,
                    phase_access: PLAN_EXECUTE_VERIFY,
                    agent_level: true,
                    review_blocked: false,
                    tier: ToolTier::Contextual,
                    allowed_roles: BUILD_PLAN_BASH_GENERAL,
                    concurrency_safe: false,
                    destructive: false,
                    deferred: false,
                    max_result_chars: 50_000,
                    interrupt_behavior: InterruptBehavior::Cancel,
                }
            }
            Self::SpawnTask => ToolMetadata {
                read_only: false,
                phase_access: PLAN_EXECUTE_VERIFY,
                agent_level: true,
                review_blocked: false,
                tier: ToolTier::Contextual,
                allowed_roles: BUILD_PLAN_GENERAL,
                concurrency_safe: false,
                destructive: false,
                deferred: false,
                max_result_chars: 50_000,
                interrupt_behavior: InterruptBehavior::Cancel,
            },
            Self::SendMessage => ToolMetadata {
                read_only: false,
                phase_access: PLAN_EXECUTE_VERIFY,
                agent_level: true,
                review_blocked: false,
                tier: ToolTier::Contextual,
                allowed_roles: BUILD_PLAN_GENERAL,
                concurrency_safe: false,
                destructive: false,
                deferred: false,
                max_result_chars: 50_000,
                interrupt_behavior: InterruptBehavior::Cancel,
            },
            Self::EnterPlanMode => ToolMetadata {
                read_only: false,
                phase_access: EXPLORE_EXECUTE_VERIFY,
                agent_level: true,
                review_blocked: false,
                tier: ToolTier::Contextual,
                allowed_roles: BUILD_PLAN_GENERAL,
                concurrency_safe: false,
                destructive: false,
                deferred: false,
                max_result_chars: 50_000,
                interrupt_behavior: InterruptBehavior::Cancel,
            },
            Self::ExitPlanMode => ToolMetadata {
                read_only: false,
                phase_access: PLAN_ONLY,
                agent_level: true,
                review_blocked: false,
                tier: ToolTier::Contextual,
                allowed_roles: BUILD_PLAN_GENERAL,
                concurrency_safe: false,
                destructive: false,
                deferred: false,
                max_result_chars: 50_000,
                interrupt_behavior: InterruptBehavior::Cancel,
            },
            Self::EnterWorktree | Self::ExitWorktree => ToolMetadata {
                read_only: false,
                phase_access: EXECUTE_AND_VERIFY,
                agent_level: true,
                review_blocked: false,
                tier: ToolTier::Extended,
                allowed_roles: BUILD_GENERAL,
                concurrency_safe: false,
                destructive: false,
                deferred: true,
                max_result_chars: 50_000,
                interrupt_behavior: InterruptBehavior::Block,
            },
            Self::Skill => ToolMetadata {
                read_only: false,
                phase_access: EXECUTE_AND_VERIFY,
                agent_level: true,
                review_blocked: false,
                tier: ToolTier::Extended,
                allowed_roles: BUILD_GENERAL,
                concurrency_safe: false,
                destructive: false,
                deferred: true,
                max_result_chars: 50_000,
                interrupt_behavior: InterruptBehavior::Block,
            },
            Self::KillShell => ToolMetadata {
                read_only: false,
                phase_access: EXECUTE_AND_VERIFY,
                agent_level: true,
                review_blocked: false,
                tier: ToolTier::Extended,
                allowed_roles: BUILD_BASH_GENERAL,
                concurrency_safe: false,
                destructive: false,
                deferred: true,
                max_result_chars: 50_000,
                interrupt_behavior: InterruptBehavior::Cancel,
            },
        }
    }

    #[must_use]
    pub fn is_allowed_for_role(&self, role: ToolAgentRole) -> bool {
        self.metadata().allowed_roles.contains(&role)
    }

    #[must_use]
    pub fn is_allowed_in_phase(&self, phase: TaskPhase) -> bool {
        let access = self.metadata().phase_access;
        match phase {
            TaskPhase::Explore => access.explore,
            TaskPhase::Plan => access.plan,
            TaskPhase::Execute => access.execute,
            TaskPhase::Verify => access.verify,
        }
    }

    #[must_use]
    pub fn tier(&self) -> ToolTier {
        self.metadata().tier
    }
}

#[must_use]
pub fn is_api_tool_name_read_only(name: &str) -> bool {
    ToolName::from_api_name(name)
        .map(|tool| tool.is_read_only())
        .unwrap_or(false)
}

#[must_use]
pub fn is_internal_tool_name_read_only(name: &str) -> bool {
    ToolName::from_internal_name(name)
        .map(|tool| tool.is_read_only())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_metadata_restricts_unknown_dynamic_tools() {
        let metadata = RuntimeToolMetadata::for_api_name("mcp__github__create_issue");
        assert!(metadata.dynamic);
        assert!(!metadata.read_only);
        assert!(!metadata.concurrency_safe);
        assert!(metadata.approval_required);
        assert!(!metadata.is_allowed_in_phase(TaskPhase::Explore));
        assert!(metadata.is_allowed_in_phase(TaskPhase::Execute));
    }

    #[test]
    fn runtime_metadata_keeps_builtin_read_only_parallel_safe() {
        let metadata = RuntimeToolMetadata::for_api_name("fs_read");
        assert!(!metadata.dynamic);
        assert!(metadata.read_only);
        assert!(metadata.concurrency_safe);
        assert!(metadata.is_allowed_in_phase(TaskPhase::Explore));
    }
}
