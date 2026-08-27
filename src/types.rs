//! Core value types.
//!
//! Two distinct numbers, deliberately not one — see `doc/lock-interface.md` §7 D1.
//! [`Ticket`] changes when the lock changes hands; [`Version`] changes when the resource's
//! content changes. Reusing one for the other is valid only under [`Span::Full`] and wrong
//! under [`Span::WriteOnly`], which is why both exist.

use std::fmt;

/// Whose turn it is. Issued by the lock service, never by the harness and never by an agent
/// (`doc/lock-interface.md` §4.3). Monotonic across the whole service, so a holder that lost
/// the lock always compares stale.
///
/// The harness must never construct one from its own state: a restarted harness would reset the
/// sequence and a pre-restart holder would look current again (handover §7.4b).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ticket(pub u64);

/// Which version of the *content* this is. Increases on every accepted write.
///
/// Lives in the lock service (`doc/lock-interface.md` §7 D5) so that "if the version is still N,
/// set it to N+1" is one indivisible operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version(pub u64);

/// Identifies a protected resource. One lazily-created lock namespace per resource (§7 D3).
///
/// NOTE: canonicalization is an open TODO (§7 D3) — two spellings of one path must not map to two
/// locks, or mutual exclusion breaks silently. v1 has a single resource so it cannot bite yet.
pub type ResourceId = String;

/// Identifies an agent. Agents never see tickets, so this is the only identity they carry.
pub type HolderId = String;

/// Proof that the lock was granted. Held by the *harness*, never handed to an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holding {
    pub resource: ResourceId,
    pub holder: HolderId,
    pub ticket: Ticket,
}

/// Which work sits inside the lock — the still-open §5.2 question, shipped as a config flag so
/// the conformance table can run both (`doc/agent-model.md` §6.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Span {
    /// Lock only the commit. Hold time is a local write: thin-tailed, so a tight ceiling is safe.
    /// Requires the [`Version`] check to catch staleness (R6).
    WriteOnly,
    /// Lock across read → think → commit. Absorbs staleness for free, at the cost of inflating
    /// every hold-time-proportional exposure window (`doc/agent-model.md` §6.2).
    Full,
}

/// Why a write was refused. Every variant is a *refusal*, never a rollback — nothing speculative
/// is ever written, which is what makes recoverability free (`doc/agent-model.md` §5.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The holder's ticket is no longer current: it lost the lock (R3, R11, R14).
    StaleTicket { held: Ticket, current: Option<Ticket> },
    /// The content moved under the holder while it was deciding (R6).
    StaleVersion { read: Version, current: Version },
    /// The holder does not hold the lock at all.
    NotHeld,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::StaleTicket { held, current } => {
                write!(f, "stale ticket: held {:?}, current {:?}", held, current)
            }
            Refusal::StaleVersion { read, current } => {
                write!(f, "stale version: read {:?}, current {:?}", read, current)
            }
            Refusal::NotHeld => write!(f, "not held"),
        }
    }
}

/// Errors from the lock service itself, as opposed to a refused write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockError {
    /// Someone else holds it.
    Contended,
    /// The lease lapsed before this call.
    Expired,
    /// The lock service could not be reached (simulated by `turmoil` in the soak tier).
    Unreachable,
}
