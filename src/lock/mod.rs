//! The lock-service interface — the one abstraction the fake and both real backends satisfy.
//!
//! Specified in `doc/lock-interface.md`. Two things it exists to hide: the ticket number
//! (etcd `mod_revision` vs ZooKeeper `czxid`, both cluster-global and monotonic) and how the
//! service decides a holder is gone (etcd: a lease per lock; ZooKeeper: a session per client —
//! §4.1, and §7 D7 for why that divergence cannot surface in v1).
//!
//! There is deliberately **no** `conditional_write` over the resource itself: both backends offer
//! compare-and-swap only for data stored *in them*, and the resource is a file on disk (§4.2).
//! What [`LockService::commit`] guards is the *version*, which does live in the service (§7 D5).

pub mod fake;

use crate::types::*;

pub trait LockService {
    /// Take the lock. Fails with [`LockError::Contended`] if someone else holds it.
    async fn acquire(&self, resource: &ResourceId, holder: &HolderId) -> Result<Holding, LockError>;

    /// Normal completion. Idempotent: releasing a lock already lost is not an error, because the
    /// caller may be learning about the loss for the first time.
    async fn release(&self, holding: &Holding) -> Result<(), LockError>;

    /// The ticket in force *right now*, or `None` if unheld.
    async fn current_ticket(&self, resource: &ResourceId) -> Result<Option<Ticket>, LockError>;

    /// Keep a long hold alive. The caller must never renew past the hold ceiling — the ceiling is
    /// enforced by the harness, not here (§7 D6).
    async fn keepalive(&self, holding: &Holding) -> Result<(), LockError>;

    /// Take the lock away from a holder that has not given it back.
    ///
    /// **Named `force_release` and not `revoke` on purpose.** "Revoke" is Curator's word for the
    /// *cooperative* mechanism, where the holder is notified and must choose to comply
    /// (`doc/reference/lock-primitives.md`). This does not notify the holder at all: the holder
    /// discovers it only when its next write is refused. A wedged holder never runs its own code
    /// again, so asking politely could never work.
    ///
    /// **Must not require holding the lock** — that is the whole point (R4). The preemptor cannot
    /// block on the thing it is trying to reclaim.
    async fn force_release(&self, resource: &ResourceId) -> Result<Ticket, LockError>;

    /// Advisory only, and documented as such: **never** treat this as permission to write.
    /// Doing so reintroduces exactly the check-then-act race that checking the ticket at commit
    /// time exists to eliminate.
    async fn is_still_held(&self, holding: &Holding) -> Result<bool, LockError>;

    /// The resource's current content version.
    async fn current_version(&self, resource: &ResourceId) -> Result<Version, LockError>;

    /// **The atomic step.** Compare the holder's ticket against the current one *and* the expected
    /// version against the current one, and if both match, bump the version and store the content
    /// hash — indivisibly (§4.2b).
    ///
    /// This is the authorization point. Ticket validity is defined here, at the moment a write is
    /// authorized, rather than at the moment bytes reach the disk — which is how a commit point is
    /// normally defined, not a weakening. The file write afterwards applies an already-committed
    /// decision.
    async fn commit(
        &self,
        holding: &Holding,
        expected_version: Version,
        content_hash: u64,
    ) -> Result<Version, Refusal>;
}
