use crate::{TaskPhase, ToolName};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolTier {
    Core,
    Contextual,
    Extended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolAgentRole {
    Build,
    Explore,
    Plan,
    Bash,
    General,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolPhaseAccess {
    pub explore: bool,
    pub plan: bool,
    pub execute: bool,
    pub verify: bool,
}

/// Behavior when the user interrupts during tool execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InterruptBehavior {
    /// Abort immediately (default for reads).
    Cancel,
    /// Finish before handling interrupt (default for writes).
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
            | Self::NotebookEdit => ToolMetadata {
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
