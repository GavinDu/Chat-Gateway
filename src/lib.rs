//! Independently testable, pure provider protocol contracts.
//!
//! Keep transport- and browser-agnostic state reducers out of the gateway
//! binary until first-party authenticated event sources are verified.

#[path = "providers/manus/agent_lifecycle.rs"]
pub mod manus_agent_lifecycle;
