//! The in-memory fake lock service — tier 1 of `doc/test-design.md` §2, and the suite that gates
//! commits.
//!
//! **Not a compromise.** For several races the fake is strictly better than a real backend, because
//! it can construct faults a correct backend will not produce on demand: expiring a lease at an
//! exact instant (R2, R3, R11), swallowing one holder's keepalives while it stays otherwise healthy
//! (R4), and granting one lock twice (R7).
//!
//! That last one deserves care. **etcd cannot split-brain** — under partition the majority side
//! serves and the minority rejects all writes, because Raft quorum means a minority cannot elect a
//! leader. So R7 is not "what happens when the lock service partitions" but "what happens if the
//! lock service breaks its promise": defence in depth against something a correct backend does not
//! do. A faithful multi-node simulation would *prove R7 unreachable* rather than test it, which is
//! why deliberately violating the contract is the mechanism and not a shortcut
//! (`doc/lock-interface.md` §7 D4).

use super::LockService;
use crate::history::{Event, History};
use crate::types::*;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Mutex;
use tokio::time::{Duration, Instant};

/// Faults the fake can be told to inject. Everything here exists to construct a specific named
/// race; nothing is speculative (`doc/lock-interface.md` §5).
#[derive(Debug, Default, Clone)]
pub struct Faults {
    /// Silently discard these holders' keepalives while they stay otherwise live. Produces a
    /// *wedged* holder that the lock service still believes is healthy — R4.
    pub swallow_keepalive: HashSet<HolderId>,
    /// Grant these resources to a second holder while the first still holds. A deliberate contract
    /// violation — R7. See the module note on why this is the only way to construct it.
    pub grant_twice: HashSet<ResourceId>,
    /// Issue the *previous* ticket value again instead of a higher one.
    ///
    /// The only fault whose purpose is to make a test **fail**: it verifies the checker actually
    /// detects a violated assumption rather than trusting it. Without it, P2 passes vacuously
    /// forever — and would pass identically if the checker were entirely broken.
    ///
    /// Two real causes it stands in for (`doc/lock-interface.md` §5.1): a harness that generates
    /// its own tickets and restarts, which the §7.4b rule forbids; and etcd revisions genuinely
    /// moving backwards after a cluster is restored from an older snapshot, which is why etcd
    /// ships a revision-bump option on restore.
    pub non_monotonic_tickets: bool,
    /// Ignore these holders' `release` calls, so the lock is freed only by lease expiry — R8, R9.
    pub drop_release: HashSet<HolderId>,
}

#[derive(Debug)]
struct HeldBy {
    holder: HolderId,
    ticket: Ticket,
    deadline: Instant,
}

#[derive(Debug, Default)]
struct ResState {
    /// Normally zero or one entry. Two only under `grant_twice`, which is the contract violation.
    held: Vec<HeldBy>,
    version: u64,
    #[allow(dead_code)]
    content_hash: u64,
}

struct State {
    /// Monotonic and service-global, standing in for etcd's `mod_revision` / ZooKeeper's `czxid`.
    /// Starts at 1 so `Ticket(0)` is never valid.
    next_ticket: u64,
    last_issued: u64,
    resources: HashMap<ResourceId, ResState>,
    faults: Faults,
}

pub struct FakeLockService {
    state: Mutex<State>,
    history: History,
    start: Instant,
    lease: Duration,
}

impl FakeLockService {
    /// `lease` is the liveness timeout — seconds scale, because it detects whether a *process* is
    /// alive. Not to be confused with the hold ceiling, which is minutes scale because it bounds
    /// how long *work* may take. Conflating the two clocks is the mistake `doc/lock-interface.md`
    /// §7 D6 warns about.
    pub fn new(history: History, lease: Duration) -> Self {
        Self {
            state: Mutex::new(State {
                next_ticket: 1,
                last_issued: 0,
                resources: HashMap::new(),
                faults: Faults::default(),
            }),
            history,
            start: Instant::now(),
            lease,
        }
    }

    pub fn with_faults(self, faults: Faults) -> Self {
        self.state.lock().unwrap().faults = faults;
        self
    }

    pub fn set_faults(&self, faults: Faults) {
        self.state.lock().unwrap().faults = faults;
    }

    fn now_ms(&self) -> u64 {
        Instant::now().saturating_duration_since(self.start).as_millis() as u64
    }

    /// Expire lapsed leases. Called at the top of every operation, so expiry is *observed* rather
    /// than driven by a background timer — which is what keeps it deterministic under virtual time.
    fn sweep(&self, st: &mut State) {
        let now = Instant::now();
        let mut expired: Vec<(ResourceId, HolderId, Ticket)> = Vec::new();
        for (rid, rs) in st.resources.iter_mut() {
            rs.held.retain(|h| {
                if h.deadline <= now {
                    expired.push((rid.clone(), h.holder.clone(), h.ticket));
                    false
                } else {
                    true
                }
            });
        }
        for (resource, holder, ticket) in expired {
            self.history
                .record(self.now_ms(), Event::LeaseExpired { holder, resource, ticket });
        }
    }

    /// Issue the next ticket. Honours `non_monotonic_tickets`, which is the whole point of that
    /// fault existing.
    fn issue(&self, st: &mut State, resource: &ResourceId) -> Ticket {
        let value = if st.faults.non_monotonic_tickets {
            st.last_issued.max(1)
        } else {
            let v = st.next_ticket;
            st.next_ticket += 1;
            v
        };
        st.last_issued = value;
        let t = Ticket(value);
        self.history
            .record(self.now_ms(), Event::TicketIssued { resource: resource.clone(), ticket: t });
        t
    }

    /// The ticket in force: the highest among current holders. Under `grant_twice` the lower holder
    /// therefore compares stale and is refused, which is exactly the behaviour R7 asserts.
    fn current(rs: &ResState) -> Option<Ticket> {
        rs.held.iter().map(|h| h.ticket).max()
    }

    /// Expire a lease at an exact instant, rather than waiting for the clock. Test-only control
    /// that a real backend cannot offer (R2, R3, R11).
    pub fn force_expire(&self, resource: &ResourceId) {
        let mut st = self.state.lock().unwrap();
        let holders: Vec<(HolderId, Ticket)> = st
            .resources
            .get(resource)
            .map(|rs| rs.held.iter().map(|h| (h.holder.clone(), h.ticket)).collect())
            .unwrap_or_default();
        if let Some(rs) = st.resources.get_mut(resource) {
            rs.held.clear();
        }
        drop(st);
        for (holder, ticket) in holders {
            self.history.record(
                self.now_ms(),
                Event::LeaseExpired { holder, resource: resource.clone(), ticket },
            );
        }
    }

    /// Inspect the version without going through the trait, for assertions.
    pub fn peek_version(&self, resource: &ResourceId) -> Version {
        let st = self.state.lock().unwrap();
        Version(st.resources.get(resource).map(|r| r.version).unwrap_or(0))
    }
}

impl LockService for FakeLockService {
    async fn acquire(&self, resource: &ResourceId, holder: &HolderId) -> Result<Holding, LockError> {
        let mut st = self.state.lock().unwrap();
        self.sweep(&mut st);
        self.history.record(
            self.now_ms(),
            Event::TakeRequested { holder: holder.clone(), resource: resource.clone() },
        );

        let allow_second = st.faults.grant_twice.contains(resource);
        let rs = st.resources.entry(resource.clone()).or_default();
        if !rs.held.is_empty() && !allow_second {
            return Err(LockError::Contended);
        }

        let ticket = self.issue(&mut st, resource);
        let deadline = Instant::now() + self.lease;
        let rs = st.resources.entry(resource.clone()).or_default();
        rs.held.push(HeldBy { holder: holder.clone(), ticket, deadline });

        self.history.record(
            self.now_ms(),
            Event::TakeGranted { holder: holder.clone(), resource: resource.clone(), ticket },
        );
        Ok(Holding { resource: resource.clone(), holder: holder.clone(), ticket })
    }

    async fn release(&self, holding: &Holding) -> Result<(), LockError> {
        let mut st = self.state.lock().unwrap();
        self.sweep(&mut st);
        if st.faults.drop_release.contains(&holding.holder) {
            // Freed only by lease expiry from here on.
            return Ok(());
        }
        if let Some(rs) = st.resources.get_mut(&holding.resource) {
            rs.held.retain(|h| h.ticket != holding.ticket);
        }
        drop(st);
        self.history.record(
            self.now_ms(),
            Event::GiveBack {
                holder: holding.holder.clone(),
                resource: holding.resource.clone(),
                ticket: holding.ticket,
            },
        );
        Ok(())
    }

    async fn current_ticket(&self, resource: &ResourceId) -> Result<Option<Ticket>, LockError> {
        let mut st = self.state.lock().unwrap();
        self.sweep(&mut st);
        Ok(st.resources.get(resource).and_then(Self::current))
    }

    async fn keepalive(&self, holding: &Holding) -> Result<(), LockError> {
        let mut st = self.state.lock().unwrap();
        self.sweep(&mut st);
        // A wedged holder's renewals are swallowed, yet the holder stays alive and healthy — the
        // state a liveness-based lease cannot detect (R4).
        if st.faults.swallow_keepalive.contains(&holding.holder) {
            return Ok(());
        }
        let lease = self.lease;
        let rs = st.resources.get_mut(&holding.resource).ok_or(LockError::Expired)?;
        let h = rs
            .held
            .iter_mut()
            .find(|h| h.ticket == holding.ticket)
            .ok_or(LockError::Expired)?;
        h.deadline = Instant::now() + lease;
        Ok(())
    }

    async fn force_release(&self, resource: &ResourceId) -> Result<Ticket, LockError> {
        let mut st = self.state.lock().unwrap();
        self.sweep(&mut st);
        // Note what is absent: no lock is acquired to do this, and the holder is never notified.
        // Both are the point (R4, §5.3).
        if let Some(rs) = st.resources.get_mut(resource) {
            rs.held.clear();
        }
        st.next_ticket += 1;
        let barrier = Ticket(st.next_ticket);
        drop(st);
        self.history.record(
            self.now_ms(),
            Event::Revoked {
                resource: resource.clone(),
                by: "preemptor".to_string(),
                new_ticket: Some(barrier),
            },
        );
        Ok(barrier)
    }

    async fn is_still_held(&self, holding: &Holding) -> Result<bool, LockError> {
        let mut st = self.state.lock().unwrap();
        self.sweep(&mut st);
        Ok(st
            .resources
            .get(&holding.resource)
            .map(|rs| rs.held.iter().any(|h| h.ticket == holding.ticket))
            .unwrap_or(false))
    }

    async fn current_version(&self, resource: &ResourceId) -> Result<Version, LockError> {
        let mut st = self.state.lock().unwrap();
        self.sweep(&mut st);
        Ok(Version(st.resources.get(resource).map(|r| r.version).unwrap_or(0)))
    }

    async fn commit(
        &self,
        holding: &Holding,
        expected_version: Version,
        content_hash: u64,
    ) -> Result<Version, Refusal> {
        let mut st = self.state.lock().unwrap();
        self.sweep(&mut st);

        let rs = st.resources.get(&holding.resource);
        let current = rs.and_then(Self::current);
        let still_holds =
            rs.map(|r| r.held.iter().any(|h| h.ticket == holding.ticket)).unwrap_or(false);

        // Both comparisons happen here, indivisibly, and this is the authorization point: ticket
        // validity is defined at the moment a write is authorized, not when bytes reach the disk
        // (`doc/lock-interface.md` §4.2b).
        if !still_holds {
            return Err(Refusal::StaleTicket { held: holding.ticket, current });
        }
        if current != Some(holding.ticket) {
            return Err(Refusal::StaleTicket { held: holding.ticket, current });
        }
        let rs = st.resources.get_mut(&holding.resource).unwrap();
        if Version(rs.version) != expected_version {
            return Err(Refusal::StaleVersion { read: expected_version, current: Version(rs.version) });
        }
        rs.version += 1;
        rs.content_hash = content_hash;
        Ok(Version(rs.version))
    }
}
