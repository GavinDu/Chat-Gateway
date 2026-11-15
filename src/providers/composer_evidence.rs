//! Pure evidence ordering for browser-controlled composers.
//!
//! Event provenance (first-party DOM, framework render, and turn identity) must
//! be verified by the caller. This reducer never reads a browser or stores a
//! prompt and cannot itself prove authentication or provider completion.

use std::fmt;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposerPhase {
    AwaitingEditor,
    EditorReady,
    DraftVisible,
    SendReady,
    Dispatched,
    UserTurnAccepted,
    PartialResponse,
    Complete,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposerEvidenceError {
    StaleRequest,
    WrongComposer,
    AmbiguousOrInactiveEditor,
    WrongPhase,
    DraftNotVisible,
    SendUnavailable,
    TurnNotAcknowledged,
    ContentNotCurrent,
    TerminalNotVerified,
    GenerationExhausted,
}

impl fmt::Display for ComposerEvidenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("provider composer evidence not sufficient for requested transition")
    }
}

impl std::error::Error for ComposerEvidenceError {}

/// One request owns a single resolved editor, not an arbitrary selector match.
/// Neither the draft, editor identity, nor the observed reply is logged.
/// A request-scoped capability, not merely a reusable numeric generation.
/// Holding old identities alive prevents address reuse across registry resets.
#[derive(Clone)]
pub struct RequestIdentity {
    owner: Arc<()>,
    epoch: u64,
}

impl PartialEq for RequestIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.epoch == other.epoch && Arc::ptr_eq(&self.owner, &other.owner)
    }
}

impl Eq for RequestIdentity {}

impl fmt::Debug for RequestIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RequestIdentity(<redacted>)")
    }
}

pub struct ComposerEvidence {
    owner: Arc<()>,
    epoch: u64,
    editor_id: Option<u64>,
    phase: ComposerPhase,
}

impl fmt::Debug for ComposerEvidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ComposerEvidence")
            .field("phase", &self.phase)
            .finish_non_exhaustive()
    }
}

impl Default for ComposerEvidence {
    fn default() -> Self {
        Self::new()
    }
}

impl ComposerEvidence {
    pub fn new() -> Self {
        Self {
            owner: Arc::new(()),
            epoch: 1,
            editor_id: None,
            phase: ComposerPhase::AwaitingEditor,
        }
    }

    pub fn request_identity(&self) -> RequestIdentity {
        RequestIdentity {
            owner: Arc::clone(&self.owner),
            epoch: self.epoch,
        }
    }

    pub fn phase(&self) -> ComposerPhase {
        self.phase
    }

    fn guard(
        &self,
        request: &RequestIdentity,
        editor_id: u64,
    ) -> Result<(), ComposerEvidenceError> {
        if request.epoch != self.epoch
            || !Arc::ptr_eq(&request.owner, &self.owner)
            || self.phase == ComposerPhase::Cancelled
        {
            return Err(ComposerEvidenceError::StaleRequest);
        }
        if self.editor_id != Some(editor_id) {
            return Err(ComposerEvidenceError::WrongComposer);
        }
        Ok(())
    }

    /// Only an independently verified unique, actionable composer can be
    /// selected. A search, login or hidden duplicate cannot authorize Send.
    pub fn resolve_editor(
        &mut self,
        request: &RequestIdentity,
        editor_id: u64,
        actionable_count: usize,
    ) -> Result<(), ComposerEvidenceError> {
        if request.epoch != self.epoch
            || !Arc::ptr_eq(&request.owner, &self.owner)
            || self.phase == ComposerPhase::Cancelled
        {
            return Err(ComposerEvidenceError::StaleRequest);
        }
        if self.phase != ComposerPhase::AwaitingEditor {
            return Err(ComposerEvidenceError::WrongPhase);
        }
        if actionable_count != 1 {
            return Err(ComposerEvidenceError::AmbiguousOrInactiveEditor);
        }
        self.editor_id = Some(editor_id);
        self.phase = ComposerPhase::EditorReady;
        Ok(())
    }

    /// Compare exactly the *visible framework-rendered* draft, not a JS shim's
    /// synthetic `.value` or input-dispatch success. Strings are never retained.
    pub fn observe_visible_draft(
        &mut self,
        request: &RequestIdentity,
        editor_id: u64,
        expected_prompt: &str,
        visible_draft: &str,
    ) -> Result<(), ComposerEvidenceError> {
        self.guard(request, editor_id)?;
        if !matches!(
            self.phase,
            ComposerPhase::EditorReady | ComposerPhase::DraftVisible | ComposerPhase::SendReady
        ) {
            return Err(ComposerEvidenceError::WrongPhase);
        }
        if visible_draft != expected_prompt {
            // A framework rerender can invalidate a previously enabled Send.
            self.phase = ComposerPhase::EditorReady;
            return Err(ComposerEvidenceError::DraftNotVisible);
        }
        self.phase = ComposerPhase::DraftVisible;
        Ok(())
    }

    /// The actionable Send must be scoped to the same editor/form and enabled.
    pub fn observe_send(
        &mut self,
        request: &RequestIdentity,
        editor_id: u64,
        enabled_and_same_form: bool,
    ) -> Result<(), ComposerEvidenceError> {
        self.guard(request, editor_id)?;
        if self.phase != ComposerPhase::DraftVisible {
            return Err(ComposerEvidenceError::WrongPhase);
        }
        if !enabled_and_same_form {
            return Err(ComposerEvidenceError::SendUnavailable);
        }
        self.phase = ComposerPhase::SendReady;
        Ok(())
    }

    /// Recheck the visible draft at dispatch time, not only during preflight.
    pub fn dispatch_send(
        &mut self,
        request: &RequestIdentity,
        editor_id: u64,
        expected_prompt: &str,
        visible_at_send: &str,
    ) -> Result<(), ComposerEvidenceError> {
        self.guard(request, editor_id)?;
        if self.phase != ComposerPhase::SendReady {
            return Err(ComposerEvidenceError::WrongPhase);
        }
        if expected_prompt != visible_at_send {
            self.phase = ComposerPhase::EditorReady;
            return Err(ComposerEvidenceError::DraftNotVisible);
        }
        self.phase = ComposerPhase::Dispatched;
        Ok(())
    }

    /// A click alone does not prove accepted submission. This flag must
    /// originate from a new, matching first-party user-turn signal.
    pub fn acknowledge_user_turn(
        &mut self,
        request: &RequestIdentity,
        editor_id: u64,
        fresh_matching_user_turn: bool,
    ) -> Result<(), ComposerEvidenceError> {
        self.guard(request, editor_id)?;
        if self.phase != ComposerPhase::Dispatched {
            return Err(ComposerEvidenceError::WrongPhase);
        }
        if !fresh_matching_user_turn {
            return Err(ComposerEvidenceError::TurnNotAcknowledged);
        }
        self.phase = ComposerPhase::UserTurnAccepted;
        Ok(())
    }

    /// Ignore previous answers, hydrated replies, thinking/status cards,
    /// and empty placeholders. Content is distinct from terminal completion.
    pub fn observe_fresh_assistant_content(
        &mut self,
        request: &RequestIdentity,
        editor_id: u64,
        fresh_nonempty_assistant_content: bool,
    ) -> Result<(), ComposerEvidenceError> {
        self.guard(request, editor_id)?;
        if !matches!(
            self.phase,
            ComposerPhase::UserTurnAccepted | ComposerPhase::PartialResponse
        ) {
            return Err(ComposerEvidenceError::WrongPhase);
        }
        if !fresh_nonempty_assistant_content {
            return Err(ComposerEvidenceError::ContentNotCurrent);
        }
        self.phase = ComposerPhase::PartialResponse;
        Ok(())
    }

    /// A provider-specific, current-turn terminal event is required in
    /// addition to content. Copy buttons, idle time and stale answer nodes
    /// are never terminal events.
    pub fn complete(
        &mut self,
        request: &RequestIdentity,
        editor_id: u64,
        verified_current_turn_terminal: bool,
    ) -> Result<(), ComposerEvidenceError> {
        self.guard(request, editor_id)?;
        if self.phase != ComposerPhase::PartialResponse {
            return Err(ComposerEvidenceError::WrongPhase);
        }
        if !verified_current_turn_terminal {
            return Err(ComposerEvidenceError::TerminalNotVerified);
        }
        self.phase = ComposerPhase::Complete;
        Ok(())
    }

    /// Cancellation never certifies a result. A new request needs a new epoch.
    pub fn cancel(&mut self) {
        self.editor_id = None;
        self.phase = ComposerPhase::Cancelled;
    }

    pub fn rotate_request(&mut self) -> Result<RequestIdentity, ComposerEvidenceError> {
        let next = self
            .epoch
            .checked_add(1)
            .ok_or(ComposerEvidenceError::GenerationExhausted)?;
        self.epoch = next;
        self.editor_id = None;
        self.phase = ComposerPhase::AwaitingEditor;
        Ok(self.request_identity())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_actionable_and_visible_unicode_are_required() {
        let mut state = ComposerEvidence::new();
        let e = state.request_identity();
        assert_eq!(
            state.resolve_editor(&e, 77, 2),
            Err(ComposerEvidenceError::AmbiguousOrInactiveEditor)
        );
        assert_eq!(state.phase(), ComposerPhase::AwaitingEditor);
        assert_eq!(
            state.resolve_editor(&e, 77, 0),
            Err(ComposerEvidenceError::AmbiguousOrInactiveEditor)
        );
        state.resolve_editor(&e, 77, 1).unwrap();
        let expected = "你好\n\n☃️ café";
        assert_eq!(
            state.observe_visible_draft(&e, 77, expected, "Old draft"),
            Err(ComposerEvidenceError::DraftNotVisible)
        );
        assert_eq!(state.phase(), ComposerPhase::EditorReady);
        assert_eq!(
            state.observe_visible_draft(&e, 77, expected, "你好\n☃️ café"),
            Err(ComposerEvidenceError::DraftNotVisible)
        );
        state.observe_visible_draft(&e, 77, expected, expected).unwrap();
        assert_eq!(
            state.observe_send(&e, 77, false),
            Err(ComposerEvidenceError::SendUnavailable)
        );
        state.observe_send(&e, 77, true).unwrap();
        assert_eq!(
            state.dispatch_send(&e, 77, expected, "synthetic value only"),
            Err(ComposerEvidenceError::DraftNotVisible)
        );
        state.observe_visible_draft(&e, 77, expected, expected).unwrap();
        state.observe_send(&e, 77, true).unwrap();
        state.dispatch_send(&e, 77, expected, expected).unwrap();
        assert_eq!(state.phase(), ComposerPhase::Dispatched);
    }

    #[test]
    fn previous_turn_and_partial_never_complete_current_request() {
        let mut state = ComposerEvidence::new();
        let e = state.request_identity();
        state.resolve_editor(&e, 1, 1).unwrap();
        state.observe_visible_draft(&e, 1, "prompt", "prompt").unwrap();
        state.observe_send(&e, 1, true).unwrap();
        state.dispatch_send(&e, 1, "prompt", "prompt").unwrap();
        assert_eq!(
            state.observe_fresh_assistant_content(&e, 1, true),
            Err(ComposerEvidenceError::WrongPhase)
        );
        assert_eq!(
            state.acknowledge_user_turn(&e, 1, false),
            Err(ComposerEvidenceError::TurnNotAcknowledged)
        );
        state.acknowledge_user_turn(&e, 1, true).unwrap();
        assert_eq!(
            state.complete(&e, 1, true),
            Err(ComposerEvidenceError::WrongPhase)
        );
        assert_eq!(
            state.observe_fresh_assistant_content(&e, 1, false),
            Err(ComposerEvidenceError::ContentNotCurrent)
        );
        state.observe_fresh_assistant_content(&e, 1, true).unwrap();
        assert_eq!(
            state.complete(&e, 1, false),
            Err(ComposerEvidenceError::TerminalNotVerified)
        );
        assert_eq!(state.phase(), ComposerPhase::PartialResponse);
        state.complete(&e, 1, true).unwrap();
        assert_eq!(state.phase(), ComposerPhase::Complete);
        assert_eq!(
            state.complete(&e, 1, true),
            Err(ComposerEvidenceError::WrongPhase)
        );
    }

    #[test]
    fn wrong_editor_cancel_and_request_rotation_fence_old_callbacks() {
        let mut state = ComposerEvidence::new();
        let e = state.request_identity();
        state.resolve_editor(&e, 2, 1).unwrap();
        assert_eq!(
            state.observe_visible_draft(&e, 3, "p", "p"),
            Err(ComposerEvidenceError::WrongComposer)
        );
        state.cancel();
        assert_eq!(state.phase(), ComposerPhase::Cancelled);
        assert_eq!(
            state.observe_visible_draft(&e, 2, "p", "p"),
            Err(ComposerEvidenceError::StaleRequest)
        );
        let next = state.rotate_request().unwrap();
        state.resolve_editor(&next, 2, 1).unwrap();
        assert_eq!(
            state.acknowledge_user_turn(&e, 2, true),
            Err(ComposerEvidenceError::StaleRequest)
        );
        assert_eq!(
            state.complete(&e, 2, true),
            Err(ComposerEvidenceError::StaleRequest)
        );
        assert_eq!(state.phase(), ComposerPhase::EditorReady);
    }

    #[test]
    fn old_registry_identity_cannot_certify_new_instance_with_same_epoch_and_editor() {
        let mut old = ComposerEvidence::new();
        let old_request = old.request_identity();
        old.resolve_editor(&old_request, 15, 1).unwrap();
        let mut fresh = ComposerEvidence::new();
        let new_request = fresh.request_identity();
        fresh.resolve_editor(&new_request, 15, 1).unwrap();
        assert_eq!(
            fresh.observe_visible_draft(&old_request, 15, "draft", "draft"),
            Err(ComposerEvidenceError::StaleRequest)
        );
        assert_eq!(fresh.phase(), ComposerPhase::EditorReady);
        fresh
            .observe_visible_draft(&new_request, 15, "draft", "draft")
            .unwrap();
    }

    #[test]
    fn redacted_debug_errors_and_generation_overflow() {
        let mut state = ComposerEvidence::new();
        let e = state.request_identity();
        state.resolve_editor(&e, 123456789, 1).unwrap();
        let debug = format!("{state:?}");
        assert!(!debug.contains("123456789"));
        assert_eq!(
            ComposerEvidenceError::DraftNotVisible.to_string(),
            "provider composer evidence not sufficient for requested transition"
        );
        state.epoch = u64::MAX;
        assert_eq!(
            state.rotate_request(),
            Err(ComposerEvidenceError::GenerationExhausted)
        );
        assert_eq!(state.phase(), ComposerPhase::EditorReady);
    }
}
