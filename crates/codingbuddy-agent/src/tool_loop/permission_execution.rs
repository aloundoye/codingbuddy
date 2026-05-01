//! Permission execution helpers.

use codingbuddy_core::{ApprovedToolCall, ToolProposal};

pub(super) fn approved_call_from_proposal(proposal: &ToolProposal) -> Option<ApprovedToolCall> {
    proposal.approved.then(|| ApprovedToolCall {
        invocation_id: proposal.invocation_id,
        call: proposal.call.clone(),
    })
}

pub(super) fn approved_call_after_permission(proposal: ToolProposal) -> ApprovedToolCall {
    ApprovedToolCall {
        invocation_id: proposal.invocation_id,
        call: proposal.call,
    }
}

pub(super) fn denial_guidance_for_tool(tool_name: &str) -> String {
    let reason = if tool_name.starts_with("bash") {
        "shell commands can change files, start processes, or access the network"
    } else if tool_name.starts_with("fs.write") || tool_name.starts_with("fs_write") {
        "file writes can create or overwrite workspace content"
    } else if tool_name.starts_with("fs.edit") || tool_name.starts_with("fs_edit") {
        "file edits can change workspace content"
    } else if tool_name.starts_with("mcp__") {
        "MCP tools come from external servers and may be mutating unless declared trusted read-only"
    } else {
        "this tool requires explicit approval under the current policy"
    };
    format!(
        "Tool call `{tool_name}` was denied by the user. Reason: {reason}. Try a read-only alternative, ask the user for guidance, or adjust the plan without repeating the same denied call."
    )
}
