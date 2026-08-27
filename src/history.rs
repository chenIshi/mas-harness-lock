//! The history: a totally-ordered log of everything the harness did.
//!
//! This is a **first-class part of the harness**, not debug logging that can be compiled out
//! (`doc/test-design.md` §3). Tests assert properties over the history rather than over the
//! resource, because inspecting the resource alone cannot distinguish "correct by construction"
//! from "correct by luck on this run". Same split as MIT 6.824's `porcupine`.
//!
//! It is also what the demo prints.

use crate::types::*;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A holder asked for the lock. Paired with `TakeGranted` or a `LockError`.
    TakeRequested { holder: HolderId, resource: ResourceId },
    TakeGranted { holder: HolderId, resource: ResourceId, ticket: Ticket },
    /// The lock service issued a ticket. Recorded separately so monotonicity is checkable even
    /// when no write follows — this is what catches a reset ticket source (§7.4b).
    TicketIssued { resource: ResourceId, ticket: Ticket },
    Read { holder: HolderId, resource: ResourceId, version: Version },
    WriteAttempted {
        holder: HolderId,
        resource: ResourceId,
        ticket: Ticket,
        expected_version: Version,
    },
    WriteAccepted {
        holder: HolderId,
        resource: ResourceId,
        ticket: Ticket,
        version: Version,
        content: String,
    },
    WriteRefused { holder: HolderId, resource: ResourceId, reason: Refusal, content: String },
    /// Carries the ticket, not just the holder. Without it the history cannot be replayed into a
    /// lock state: one holder can hold two tickets at once (under `grant_twice`, R7), so a
    /// holder-only event is ambiguous about *which* hold ended. The soak tier found this.
    GiveBack { holder: HolderId, resource: ResourceId, ticket: Ticket },
    LeaseExpired { holder: HolderId, resource: ResourceId, ticket: Ticket },
    /// A unilateral `force_release`. Never a notification to the holder — it is not told
    /// (`doc/agent-model.md` §0, §5.3).
    Revoked { resource: ResourceId, by: HolderId, new_ticket: Option<Ticket> },
    /// A phase exceeded its budget: inter-token silence, a whole-call timeout, or a tool deadline
    /// (`doc/agent-model.md` §5.2).
    PhaseTimeout { holder: HolderId, phase: &'static str },
    /// Renewal stopped because the hold ceiling was reached. Never renews past it, whatever drove
    /// the renewal (`doc/lock-interface.md` §7 D6).
    CeilingReached { holder: HolderId, resource: ResourceId },
    AgentFinished { holder: HolderId },
    /// The agent is alive and will never make progress. Recorded so P7 can exempt it.
    AgentWedged { holder: HolderId },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Position in the total order. Properties are stated over this, not over wall-clock, so they
    /// remain checkable under virtual time.
    pub seq: u64,
    /// Virtual milliseconds since the run began.
    pub at_ms: u64,
    pub event: Event,
}

/// Cheap to clone; all clones share one log.
#[derive(Clone, Default)]
pub struct History {
    inner: Arc<Mutex<Vec<Entry>>>,
}

impl History {
    pub fn new() -> Self {
        Self { inner: Arc::new(Mutex::new(Vec::new())) }
    }

    pub fn record(&self, at_ms: u64, event: Event) {
        let mut g = self.inner.lock().unwrap();
        let seq = g.len() as u64;
        g.push(Entry { seq, at_ms, event });
    }

    pub fn entries(&self) -> Vec<Entry> {
        self.inner.lock().unwrap().clone()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Human-readable dump. This is the demo's output.
    pub fn render(&self) -> String {
        self.entries()
            .iter()
            .map(|e| format!("{:>4} {:>7}ms  {:?}", e.seq, e.at_ms, e.event))
            .collect::<Vec<_>>()
            .join("\n")
    }
}
