//! Pure, browser- and transport-agnostic ownership checks for a possible ChatGPT
//! first-party WebSocket bridge. This does NOT open sockets or prove live transport.
//! Any future bridge must enforce this guard at the actual send/close boundary.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

pub const MAX_ACTIVE_SOCKETS: usize = 8;
pub const MAX_COMMAND_BYTES: usize = 64 * 1024;
pub const MAX_TEXT_FRAME_BYTES: usize = 1024 * 1024;
pub const MAX_BINARY_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// Frame class, never a copy of a frame or its private contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameKind {
    Command,
    Text,
    Binary,
}

/// All diagnostics are independent of page identity, socket IDs and payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeaseRejection {
    StaleOrCancelled,
    DuplicateSocket,
    SocketLimit,
    FrameTooLarge,
    GenerationExhausted,
}

impl fmt::Display for LeaseRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ChatGPT WebSocket lease operation rejected")
    }
}

impl std::error::Error for LeaseRejection {}

/// An opaque capability: both generation and serial must match. The serial
/// prevents old async callbacks from claiming a re-used ID within one page.
#[derive(Clone)]
pub struct SocketLease {
    owner: Arc<()>,
    epoch: u64,
    id: u64,
    serial: u64,
}

impl PartialEq for SocketLease {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.owner, &other.owner)
            && self.epoch == other.epoch
            && self.id == other.id
            && self.serial == other.serial
    }
}

impl Eq for SocketLease {}

impl fmt::Debug for SocketLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SocketLease(<redacted>)")
    }
}

/// A logical single-owner guard with no internal synchronization or IO.
/// The embedding browser worker MUST serialize all registry mutations and
/// authenticate the page-generation event before rotating ownership.
pub struct ChatgptWssLeaseRegistry {
    owner: Arc<()>,
    epoch: u64,
    next_serial: u64,
    active: HashMap<u64, u64>,
    cancelled: bool,
}

impl Default for ChatgptWssLeaseRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ChatgptWssLeaseRegistry {
    pub fn new() -> Self {
        Self {
            // A distinct in-process capability survives in old outstanding
            // leases, so rebuilding a registry cannot recreate its identity.
            owner: Arc::new(()),
            epoch: 1,
            next_serial: 0,
            active: HashMap::new(),
            cancelled: false,
        }
    }

    pub fn page_epoch(&self) -> u64 {
        self.epoch
    }

    pub fn active_count(&self) -> usize {
        self.active.len()
    }

    pub fn open(&mut self, epoch: u64, id: u64) -> Result<SocketLease, LeaseRejection> {
        if self.cancelled || epoch != self.epoch {
            return Err(LeaseRejection::StaleOrCancelled);
        }
        if self.active.contains_key(&id) {
            return Err(LeaseRejection::DuplicateSocket);
        }
        if self.active.len() >= MAX_ACTIVE_SOCKETS {
            return Err(LeaseRejection::SocketLimit);
        }
        let serial = self
            .next_serial
            .checked_add(1)
            .ok_or(LeaseRejection::GenerationExhausted)?;
        self.next_serial = serial;
        self.active.insert(id, serial);
        Ok(SocketLease {
            owner: Arc::clone(&self.owner),
            epoch,
            id,
            serial,
        })
    }

    fn is_live(&self, lease: &SocketLease) -> bool {
        !self.cancelled
            && Arc::ptr_eq(&self.owner, &lease.owner)
            && lease.epoch == self.epoch
            && self.active.get(&lease.id) == Some(&lease.serial)
    }

    /// Validate the length BEFORE frame data is copied or dispatched. This is
    /// a per-frame bound, not a rate limiter or substitute for IO backpressure.
    pub fn check_frame(
        &self,
        lease: &SocketLease,
        kind: FrameKind,
        byte_len: usize,
    ) -> Result<(), LeaseRejection> {
        if !self.is_live(lease) {
            return Err(LeaseRejection::StaleOrCancelled);
        }
        let limit = match kind {
            FrameKind::Command => MAX_COMMAND_BYTES,
            FrameKind::Text => MAX_TEXT_FRAME_BYTES,
            FrameKind::Binary => MAX_BINARY_FRAME_BYTES,
        };
        if byte_len > limit {
            return Err(LeaseRejection::FrameTooLarge);
        }
        Ok(())
    }

    /// Late completion of a previous socket must not close a re-used ID.
    pub fn close(&mut self, lease: &SocketLease) -> bool {
        if !self.is_live(lease) {
            return false;
        }
        self.active.remove(&lease.id);
        true
    }

    /// Cancellation is terminal for the current page epoch.
    pub fn cancel_page(&mut self) {
        self.cancelled = true;
        self.active.clear();
    }

    /// Start only after an independently verified new page ownership event.
    /// Never wrap an epoch or recycle a serial to prevent ABA at the limit.
    pub fn rotate_page(&mut self) -> Result<u64, LeaseRejection> {
        let next_epoch = self
            .epoch
            .checked_add(1)
            .ok_or(LeaseRejection::GenerationExhausted)?;
        self.epoch = next_epoch;
        self.active.clear();
        self.cancelled = false;
        Ok(next_epoch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_allows_only_one_active_id_and_caps_slots() {
        let mut guard = ChatgptWssLeaseRegistry::new();
        let epoch = guard.page_epoch();
        let first = guard.open(epoch, 88).unwrap();
        assert_eq!(guard.open(epoch, 88), Err(LeaseRejection::DuplicateSocket));
        for id in 1..MAX_ACTIVE_SOCKETS as u64 {
            guard.open(epoch, id).unwrap();
        }
        assert_eq!(guard.active_count(), MAX_ACTIVE_SOCKETS);
        assert_eq!(guard.open(epoch, 999), Err(LeaseRejection::SocketLimit));
        assert!(guard.close(&first));
        assert_eq!(guard.active_count(), MAX_ACTIVE_SOCKETS - 1);
        guard.open(epoch, 999).unwrap();
    }

    #[test]
    fn stale_close_and_frame_cannot_claim_reused_socket_id() {
        let mut guard = ChatgptWssLeaseRegistry::new();
        let epoch = guard.page_epoch();
        let old = guard.open(epoch, 123456789).unwrap();
        assert!(guard.close(&old));
        let fresh = guard.open(epoch, 123456789).unwrap();
        assert_ne!(old, fresh);
        assert!(!guard.close(&old));
        assert_eq!(
            guard.check_frame(&old, FrameKind::Command, 1),
            Err(LeaseRejection::StaleOrCancelled)
        );
        assert_eq!(guard.check_frame(&fresh, FrameKind::Command, 1), Ok(()));
        assert_eq!(guard.active_count(), 1);
    }

    #[test]
    fn cancellation_and_page_rotation_fence_old_async_callbacks() {
        let mut guard = ChatgptWssLeaseRegistry::new();
        let old_epoch = guard.page_epoch();
        let old = guard.open(old_epoch, 9).unwrap();
        guard.cancel_page();
        assert_eq!(guard.active_count(), 0);
        assert_eq!(
            guard.open(old_epoch, 9),
            Err(LeaseRejection::StaleOrCancelled)
        );
        assert_eq!(
            guard.check_frame(&old, FrameKind::Binary, 0),
            Err(LeaseRejection::StaleOrCancelled)
        );
        let next_epoch = guard.rotate_page().unwrap();
        let fresh = guard.open(next_epoch, 9).unwrap();
        assert!(!guard.close(&old));
        assert_eq!(guard.check_frame(&fresh, FrameKind::Text, 10), Ok(()));
        assert_ne!(next_epoch, old_epoch);
        assert_eq!(
            guard.open(old_epoch, 10),
            Err(LeaseRejection::StaleOrCancelled)
        );
        for _ in 0..50 {
            guard.rotate_page().unwrap();
            assert_eq!(guard.active_count(), 0);
        }
    }

    #[test]
    fn new_registry_can_never_replay_old_instance_lease() {
        let mut old_owner = ChatgptWssLeaseRegistry::new();
        let old_epoch = old_owner.page_epoch();
        let old = old_owner.open(old_epoch, 45).unwrap();
        let mut new_owner = ChatgptWssLeaseRegistry::new();
        let new_epoch = new_owner.page_epoch();
        let new_lease = new_owner.open(new_epoch, 45).unwrap();
        assert_ne!(old, new_lease);
        assert_eq!(
            new_owner.check_frame(&old, FrameKind::Text, 1),
            Err(LeaseRejection::StaleOrCancelled)
        );
        assert!(!new_owner.close(&old));
        assert_eq!(new_owner.active_count(), 1);
        assert_eq!(
            new_owner.check_frame(&new_lease, FrameKind::Text, 1),
            Ok(())
        );
    }

    #[test]
    fn exact_frame_limits_and_overflow_fail_closed() {
        let mut guard = ChatgptWssLeaseRegistry::new();
        let epoch = guard.page_epoch();
        let lease = guard.open(epoch, 7).unwrap();
        for (kind, limit) in [
            (FrameKind::Command, MAX_COMMAND_BYTES),
            (FrameKind::Text, MAX_TEXT_FRAME_BYTES),
            (FrameKind::Binary, MAX_BINARY_FRAME_BYTES),
        ] {
            assert_eq!(guard.check_frame(&lease, kind, limit), Ok(()));
            assert_eq!(
                guard.check_frame(&lease, kind, limit + 1),
                Err(LeaseRejection::FrameTooLarge)
            );
            assert_eq!(
                guard.check_frame(&lease, kind, usize::MAX),
                Err(LeaseRejection::FrameTooLarge)
            );
        }
    }

    #[test]
    fn generation_and_serial_never_wrap() {
        let mut guard = ChatgptWssLeaseRegistry::new();
        guard.next_serial = u64::MAX;
        let epoch = guard.page_epoch();
        assert_eq!(
            guard.open(epoch, 4),
            Err(LeaseRejection::GenerationExhausted)
        );
        assert_eq!(guard.active_count(), 0);
        guard.epoch = u64::MAX;
        assert_eq!(
            guard.rotate_page(),
            Err(LeaseRejection::GenerationExhausted)
        );
    }

    #[test]
    fn diagnostics_never_echo_socket_identity_or_payload() {
        let mut guard = ChatgptWssLeaseRegistry::new();
        let epoch = guard.page_epoch();
        let lease = guard.open(epoch, 987654321).unwrap();
        let debug = format!("{lease:?}");
        assert!(!debug.contains("987654321"));
        assert!(!debug.contains("epoch"));
        let error = guard
            .check_frame(&lease, FrameKind::Binary, usize::MAX)
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "ChatGPT WebSocket lease operation rejected"
        );
    }
}
