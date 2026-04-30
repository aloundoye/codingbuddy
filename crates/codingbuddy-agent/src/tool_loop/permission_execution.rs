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
