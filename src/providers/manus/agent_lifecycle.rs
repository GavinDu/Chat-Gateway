//! Pure Manus Agent task-state contract, independent of unverified site markup.
//!
//! A Chat Mode response is not evidence that an Agent task has completed.
//! First-party event decoding and browser integration remain separately gated
//! on sanitized signed-in task traces (#1041); this module does not issue
//! requests, persist credentials, or infer the current authenticated session.

use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentPhase {
    Queued,
    Accepted,
    Running,
    Terminal,
    Failed,
    Cancelled,
    VerificationRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentEvent {
    Accepted,
    Running,
    Terminal,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    Applied,
    Stale,
    Invalid,
}

/// An unforgeable in-process owner generation held by each async event.
#[derive(Clone)]
pub struct AgentRequestIdentity {
    owner: Arc<()>,
    epoch: u64,
}

impl std::fmt::Debug for AgentRequestIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AgentRequestIdentity(<redacted>)")
    }
}

/// The opaque task ID is retained only for comparison; Debug never prints it.
/// Epochs are supplied by the owning worker, not derived from provider markup.
pub struct AgentTaskLifecycle {
    owner: Arc<()>,
    epoch: u64,
    task_id: Option<String>,
    phase: AgentPhase,
}

impl std::fmt::Debug for AgentTaskLifecycle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentTaskLifecycle")
            .field("epoch", &self.epoch)
            .field("phase", &self.phase)
            .field("has_task_id", &self.task_id.is_some())
            .finish()
    }
}

impl AgentTaskLifecycle {
    pub fn queued(epoch: u64) -> Self {
        Self {
            owner: Arc::new(()),
            epoch,
            task_id: None,
            phase: AgentPhase::Queued,
        }
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn phase(&self) -> AgentPhase {
        self.phase
    }

    /// Capture this identity when scheduling an event. A later registry
    /// instance cannot accept the old callback even if epoch/task ID repeat.
    pub fn request_identity(&self) -> AgentRequestIdentity {
        AgentRequestIdentity {
            owner: Arc::clone(&self.owner),
            epoch: self.epoch,
        }
    }

    /// A new owner generation may begin a fresh task. Never reactivate a
    /// terminal/cancelled task by reusing its old epoch (ABA protection).
    pub fn begin_new(&mut self, next_epoch: u64) -> Transition {
        if next_epoch <= self.epoch {
            return Transition::Stale;
        }
        self.epoch = next_epoch;
        self.phase = AgentPhase::Queued;
        self.task_id = None;
        Transition::Applied
    }

    /// A restart invalidates old task evidence and requires manual or
    /// first-party verified recovery. Never synthesize a terminal result.
    pub fn restart(&mut self, next_epoch: u64) -> Transition {
        if next_epoch <= self.epoch {
            return Transition::Stale;
        }
        self.epoch = next_epoch;
        self.phase = AgentPhase::VerificationRequired;
        self.task_id = None;
        Transition::Applied
    }

    /// Local cancellation releases authority to complete this attempt.
    /// Later success events with the same task ID and epoch are stale.
    pub fn cancel_local(&mut self, request: &AgentRequestIdentity) -> Transition {
        if request.epoch != self.epoch || !Arc::ptr_eq(&request.owner, &self.owner) {
            return Transition::Stale;
        }
        match self.phase {
            AgentPhase::Queued | AgentPhase::Accepted | AgentPhase::Running => {
                self.phase = AgentPhase::Cancelled;
                Transition::Applied
            }
            _ => Transition::Stale,
        }
    }

    /// Apply only a task-correlated event in the owning generation.
    /// A terminal response cannot arrive out of thin air: we require a
    /// previously accepted and running task. This errs on the safe side
    /// when a provider omits a running event, pending first-party evidence.
    pub fn apply(
        &mut self,
        request: &AgentRequestIdentity,
        task_id: &str,
        event: AgentEvent,
    ) -> Transition {
        if request.epoch != self.epoch || !Arc::ptr_eq(&request.owner, &self.owner) {
            return Transition::Stale;
        }
        // Never retain an unbounded page-provided identifier in worker memory.
        // Real Manus identifiers have not yet been verified; err closed.
        if task_id.is_empty() || task_id.len() > 1024 {
            return Transition::Invalid;
        }
        if self.phase == AgentPhase::Queued {
            if event != AgentEvent::Accepted {
                return Transition::Invalid;
            }
            self.task_id = Some(task_id.to_owned());
            self.phase = AgentPhase::Accepted;
            return Transition::Applied;
        }
        if self.task_id.as_deref() != Some(task_id) {
            return if self.phase == AgentPhase::VerificationRequired {
                Transition::Invalid
            } else {
                Transition::Stale
            };
        }
        match (self.phase, event) {
            (AgentPhase::Accepted, AgentEvent::Running) => {
                self.phase = AgentPhase::Running;
                Transition::Applied
            }
            (AgentPhase::Accepted | AgentPhase::Running, AgentEvent::Failed) => {
                self.phase = AgentPhase::Failed;
                Transition::Applied
            }
            (AgentPhase::Accepted | AgentPhase::Running, AgentEvent::Cancelled) => {
                self.phase = AgentPhase::Cancelled;
                Transition::Applied
            }
            (AgentPhase::Running, AgentEvent::Terminal) => {
                self.phase = AgentPhase::Terminal;
                Transition::Applied
            }
            (AgentPhase::Terminal | AgentPhase::Cancelled | AgentPhase::Failed, _) => {
                Transition::Stale
            }
            _ => Transition::Invalid,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AgentEvent as Event, AgentPhase as Phase, AgentTaskLifecycle, Transition as T};

    // Test-only fixtures may synthesize a stale numeric generation. The public
    // production API accepts only captured opaque ownership capabilities.
    fn apply_at(state: &mut AgentTaskLifecycle, epoch: u64, task: &str, event: Event) -> T {
        let mut request = state.request_identity();
        request.epoch = epoch;
        apply_at(&mut state,&request, task, event)
    }

    fn cancel_at(state: &mut AgentTaskLifecycle, epoch: u64) -> T {
        let mut request = state.request_identity();
        request.epoch = epoch;
        cancel_at(&mut state,&request)
    }

    #[test]
    fn accepted_running_terminal_requires_exact_task_and_epoch() {
        let mut state = AgentTaskLifecycle::queued(7);
        assert_eq!(apply_at(&mut state,7, "id-a", Event::Terminal), T::Invalid);
        assert_eq!(apply_at(&mut state,7, "", Event::Accepted), T::Invalid);
        assert_eq!(
            apply_at(&mut state,7, &"x".repeat(1025), Event::Accepted),
            T::Invalid
        );
        assert_eq!(apply_at(&mut state,6, "id-a", Event::Accepted), T::Stale);
        assert_eq!(apply_at(&mut state,7, "id-a", Event::Accepted), T::Applied);
        assert_eq!(state.phase(), Phase::Accepted);
        assert_eq!(apply_at(&mut state,7, "id-b", Event::Running), T::Stale);
        assert_eq!(apply_at(&mut state,7, "id-a", Event::Accepted), T::Invalid);
        assert_eq!(apply_at(&mut state,7, "id-a", Event::Terminal), T::Invalid);
        assert_eq!(apply_at(&mut state,7, "id-a", Event::Running), T::Applied);
        assert_eq!(apply_at(&mut state,7, "id-a", Event::Running), T::Invalid);
        assert_eq!(apply_at(&mut state,7, "id-a", Event::Terminal), T::Applied);
        assert_eq!(state.phase(), Phase::Terminal);
        assert_eq!(apply_at(&mut state,7, "id-a", Event::Terminal), T::Stale);
        assert_eq!(cancel_at(&mut state,7), T::Stale);
        assert!(!format!("{state:?}").contains("id-a"));
    }

    #[test]
    fn failing_or_cancelling_task_can_never_later_complete() {
        let mut state = AgentTaskLifecycle::queued(1);
        assert_eq!(apply_at(&mut state,1, "task-1", Event::Accepted), T::Applied);
        assert_eq!(apply_at(&mut state,1, "task-1", Event::Failed), T::Applied);
        assert_eq!(apply_at(&mut state,1, "task-1", Event::Running), T::Stale);
        assert_eq!(state.begin_new(2), T::Applied);
        assert_eq!(apply_at(&mut state,2, "task-2", Event::Accepted), T::Applied);
        assert_eq!(apply_at(&mut state,2, "task-2", Event::Running), T::Applied);
        assert_eq!(cancel_at(&mut state,2), T::Applied);
        assert_eq!(state.phase(), Phase::Cancelled);
        assert_eq!(apply_at(&mut state,2, "task-2", Event::Terminal), T::Stale);
        assert_eq!(apply_at(&mut state,1, "task-1", Event::Terminal), T::Stale);
        assert_eq!(state.begin_new(2), T::Stale);
        assert_eq!(state.begin_new(3), T::Applied);
        assert_eq!(apply_at(&mut state,3, "task-3", Event::Accepted), T::Applied);
        assert_eq!(apply_at(&mut state,3, "task-2", Event::Running), T::Stale);
        assert_eq!(apply_at(&mut state,3, "task-3", Event::Cancelled), T::Applied);
    }

    #[test]
    fn old_owner_events_cannot_replay_across_new_registry_with_same_epoch_task_id() {
        let mut old = AgentTaskLifecycle::queued(1);
        let old_request = old.request_identity();
        assert_eq!(old.apply(&old_request, "same-id", Event::Accepted), T::Applied);

        let mut fresh = AgentTaskLifecycle::queued(1);
        let current = fresh.request_identity();
        assert_eq!(
            fresh.apply(&old_request, "same-id", Event::Accepted),
            T::Stale
        );
        assert_eq!(fresh.cancel_local(&old_request), T::Stale);
        assert_eq!(fresh.phase(), Phase::Queued);
        assert_eq!(fresh.apply(&current, "same-id", Event::Accepted), T::Applied);
        assert_eq!(
            fresh.apply(&old_request, "same-id", Event::Running),
            T::Stale
        );
        assert_eq!(fresh.apply(&current, "same-id", Event::Running), T::Applied);
        assert_eq!(
            fresh.apply(&old_request, "same-id", Event::Terminal),
            T::Stale
        );
        assert_eq!(fresh.apply(&current, "same-id", Event::Terminal), T::Applied);
        assert_eq!(fresh.phase(), Phase::Terminal);
        assert!(!format!("{old_request:?}").contains("same-id"));
    }

    #[test]
    fn restart_requires_verification_not_assumed_completion() {
        let mut state = AgentTaskLifecycle::queued(21);
        assert_eq!(
            apply_at(&mut state,21, "opaque-test-id", Event::Accepted),
            T::Applied
        );
        assert_eq!(
            apply_at(&mut state,21, "opaque-test-id", Event::Running),
            T::Applied
        );
        assert_eq!(state.restart(21), T::Stale);
        assert_eq!(state.restart(22), T::Applied);
        assert_eq!(state.epoch(), 22);
        assert_eq!(state.phase(), Phase::VerificationRequired);
        assert_eq!(apply_at(&mut state,21, "opaque-test-id", Event::Terminal), T::Stale);
        assert_eq!(
            apply_at(&mut state,22, "opaque-test-id", Event::Terminal),
            T::Invalid
        );
        assert_eq!(
            apply_at(&mut state,22, "opaque-test-id", Event::Accepted),
            T::Invalid
        );
        assert_eq!(state.begin_new(23), T::Applied);
        assert_eq!(state.phase(), Phase::Queued);
        assert_eq!(apply_at(&mut state,23, "new-id", Event::Accepted), T::Applied);
        assert!(!format!("{state:?}").contains("new-id"));
    }
}
