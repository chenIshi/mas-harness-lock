//! The oracle: eight properties checked over the history.
//!
//! Specified in `doc/test-design.md` §4 and `doc/lock-interface.md` §6. Every property is stated
//! over **history order**, not wall-clock, so all of it remains checkable under virtual time.
//!
//! No general-purpose serializability search is needed while there is one resource and one write per
//! transaction: P1–P4 pin the order completely. That stops being true the moment there is more than
//! one resource, at which point something porcupine-shaped becomes necessary (handover §7.3).

use crate::history::{Entry, Event, History};
use crate::types::*;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Property {
    /// No two accepted writes' execution windows overlap.
    P1MutualExclusion,
    /// Every accepted write held the ticket in force at its authorization point; and issued tickets
    /// never move backwards.
    P2TicketValidity,
    /// Every accepted write is in the final content, or superseded by a later accepted write.
    P3NoLostWrite,
    /// Nothing reached the final content that was not an accepted write.
    P4NoPhantomWrite,
    /// Every granted lock was later given back, expired, or revoked.
    P5NoLeakedLock,
    /// Every accepted write was based on the content version in force when it committed.
    P6Freshness,
    /// Every agent that is not wedged reached a terminal event.
    P7Liveness,
    /// Every revocation actually took the lock away.
    P8Preemptibility,
}

#[derive(Debug, Clone)]
pub struct Violation {
    pub property: Property,
    pub at_seq: Option<u64>,
    pub detail: String,
}

#[derive(Debug, Clone, Default)]
pub struct Report {
    pub checked: Vec<Property>,
    pub violations: Vec<Violation>,
}

impl Report {
    pub fn passed(&self) -> bool {
        self.violations.is_empty()
    }

    pub fn violated(&self, p: Property) -> bool {
        self.violations.iter().any(|v| v.property == p)
    }

    pub fn render(&self) -> String {
        if self.violations.is_empty() {
            return format!("all {} properties hold", self.checked.len());
        }
        self.violations
            .iter()
            .map(|v| {
                format!(
                    "{:?} violated{}: {}",
                    v.property,
                    v.at_seq.map(|s| format!(" at seq {s}")).unwrap_or_default(),
                    v.detail
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Check a subset of properties. Each named race asserts only the properties it is about
/// (`doc/test-design.md` §6) — nothing is asserted informally, and nothing irrelevant is asserted.
pub fn check(
    history: &History,
    props: &[Property],
    final_content: &str,
    initial_content: &str,
) -> Report {
    let e = history.entries();
    let mut r = Report { checked: props.to_vec(), violations: Vec::new() };
    for p in props {
        match p {
            Property::P1MutualExclusion => p1(&e, &mut r),
            Property::P2TicketValidity => p2(&e, &mut r),
            Property::P3NoLostWrite => p3(&e, &mut r, final_content, initial_content),
            Property::P4NoPhantomWrite => p4(&e, &mut r, final_content, initial_content),
            Property::P5NoLeakedLock => p5(&e, &mut r),
            Property::P6Freshness => p6(&e, &mut r),
            Property::P7Liveness => p7(&e, &mut r),
            Property::P8Preemptibility => p8(&e, &mut r),
        }
    }
    r
}

fn fail(r: &mut Report, property: Property, at_seq: Option<u64>, detail: impl Into<String>) {
    r.violations.push(Violation { property, at_seq, detail: detail.into() });
}

/// P1: windows of accepted writes must not overlap. A window runs from a holder's `WriteAttempted`
/// to its `WriteAccepted`; refused attempts cannot conflict with anything because they never landed.
fn p1(e: &[Entry], r: &mut Report) {
    let mut open: HashMap<HolderId, u64> = HashMap::new();
    let mut windows: Vec<(HolderId, u64, u64)> = Vec::new();
    for en in e {
        match &en.event {
            Event::WriteAttempted { holder, .. } => {
                open.insert(holder.clone(), en.seq);
            }
            Event::WriteAccepted { holder, .. } => {
                if let Some(start) = open.remove(holder) {
                    windows.push((holder.clone(), start, en.seq));
                }
            }
            Event::WriteRefused { holder, .. } => {
                open.remove(holder);
            }
            _ => {}
        }
    }
    for i in 0..windows.len() {
        for j in (i + 1)..windows.len() {
            let (ref hi, si, ei) = windows[i];
            let (ref hj, sj, ej) = windows[j];
            if hi != hj && si <= ej && sj <= ei {
                fail(
                    r,
                    Property::P1MutualExclusion,
                    Some(sj),
                    format!("accepted writes by {hi} [{si}..{ei}] and {hj} [{sj}..{ej}] overlap"),
                );
            }
        }
    }
}

/// P2: two parts. Issued tickets never move backwards — this is what the `non_monotonic_tickets`
/// fault exists to trip, so that the property cannot pass vacuously. And every accepted write held
/// the ticket in force at that point.
///
/// The set of holders, not a single one. An earlier version tracked one "current" ticket and cleared
/// it on *any* `GiveBack`/`LeaseExpired`, which mis-modelled two situations: a third holder's lease
/// lapsing while another legitimately held the lock, and the deliberate two-holder state produced by
/// `grant_twice` (R7). The soak tier found it; the scripted races never had three agents, so they
/// could not. The ticket in force is the **maximum** among current holders, matching the service.
fn p2(e: &[Entry], r: &mut Report) {
    let mut highest: Option<Ticket> = None;
    // Keyed by ticket, not holder: one holder can hold two tickets at once under `grant_twice`.
    let mut held: HashMap<Ticket, HolderId> = HashMap::new();
    let current_of = |held: &HashMap<Ticket, HolderId>| held.keys().copied().max();

    for en in e {
        match &en.event {
            Event::TicketIssued { ticket, .. } => {
                if let Some(h) = highest
                    && *ticket <= h {
                        fail(
                            r,
                            Property::P2TicketValidity,
                            Some(en.seq),
                            format!("ticket {ticket:?} not above previously issued {h:?} — the \
                                     ticket source moved backwards, so every later check is void"),
                        );
                    }
                highest = Some(highest.map_or(*ticket, |h| h.max(*ticket)));
            }
            Event::TakeGranted { holder, ticket, .. } => {
                held.insert(*ticket, holder.clone());
            }
            Event::GiveBack { ticket, .. } | Event::LeaseExpired { ticket, .. } => {
                held.remove(ticket);
            }
            Event::Revoked { new_ticket, .. } => {
                let revoked = current_of(&held);
                held.clear();
                if let (Some(prev), Some(nt)) = (revoked, new_ticket)
                    && *nt <= prev {
                        fail(
                            r,
                            Property::P2TicketValidity,
                            Some(en.seq),
                            format!("revocation barrier {nt:?} does not exceed revoked {prev:?}"),
                        );
                    }
            }
            Event::WriteAccepted { holder, ticket, .. } => {
                let current = current_of(&held);
                if current != Some(*ticket) {
                    fail(
                        r,
                        Property::P2TicketValidity,
                        Some(en.seq),
                        format!(
                            "{holder} committed with ticket {ticket:?} but {current:?} was in force"
                        ),
                    );
                }
            }
            _ => {}
        }
    }
}

fn last_accepted(e: &[Entry]) -> Option<(u64, String)> {
    e.iter().rev().find_map(|en| match &en.event {
        Event::WriteAccepted { content, .. } => Some((en.seq, content.clone())),
        _ => None,
    })
}

/// P3: the final content is the last accepted write. An earlier accepted write being absent is
/// correct — it was superseded, not lost.
fn p3(e: &[Entry], r: &mut Report, final_content: &str, initial: &str) {
    match last_accepted(e) {
        Some((seq, content)) => {
            if content != final_content {
                fail(
                    r,
                    Property::P3NoLostWrite,
                    Some(seq),
                    format!("last accepted write {content:?} is not the final content {final_content:?}"),
                );
            }
        }
        None => {
            if final_content != initial {
                fail(
                    r,
                    Property::P3NoLostWrite,
                    None,
                    format!("no write was accepted, yet content changed to {final_content:?}"),
                );
            }
        }
    }
}

/// P4: nothing refused may appear in the final content. This is what catches a write that landed on
/// disk after losing its authorization (R14).
fn p4(e: &[Entry], r: &mut Report, final_content: &str, initial: &str) {
    let accepted: Vec<String> = e
        .iter()
        .filter_map(|en| match &en.event {
            Event::WriteAccepted { content, .. } => Some(content.clone()),
            _ => None,
        })
        .collect();
    if final_content != initial && !accepted.iter().any(|c| c == final_content) {
        fail(
            r,
            Property::P4NoPhantomWrite,
            None,
            format!("final content {final_content:?} was never an accepted write"),
        );
    }
    for en in e {
        if let Event::WriteRefused { holder, content, reason, .. } = &en.event
            && content == final_content && !accepted.iter().any(|c| c == content) {
                fail(
                    r,
                    Property::P4NoPhantomWrite,
                    Some(en.seq),
                    format!("{holder}'s write was refused ({reason}) yet is the final content"),
                );
            }
    }
}

/// P5: every grant is matched. Catches the boring, common leak where release only ran on success.
fn p5(e: &[Entry], r: &mut Report) {
    // Keyed by ticket as well as holder, for the same reason as P2.
    let mut held: HashMap<(Ticket, ResourceId), (HolderId, u64)> = HashMap::new();
    for en in e {
        match &en.event {
            Event::TakeGranted { holder, resource, ticket } => {
                held.insert((*ticket, resource.clone()), (holder.clone(), en.seq));
            }
            Event::GiveBack { resource, ticket, .. }
            | Event::LeaseExpired { resource, ticket, .. } => {
                held.remove(&(*ticket, resource.clone()));
            }
            Event::Revoked { resource, .. } => {
                held.retain(|(_, res), _| res != resource);
            }
            _ => {}
        }
    }
    for ((_ticket, resource), (holder, seq)) in held {
        fail(
            r,
            Property::P5NoLeakedLock,
            Some(seq),
            format!("{holder} still holds {resource} at end of history"),
        );
    }
}

/// P6: the version a write was based on must be the version in force when it committed. This is the
/// only thing standing between a write-only span and a silently stale write (R6).
fn p6(e: &[Entry], r: &mut Report) {
    let mut version = Version(0);
    let mut expected: HashMap<HolderId, Version> = HashMap::new();
    for en in e {
        match &en.event {
            Event::WriteAttempted { holder, expected_version, .. } => {
                expected.insert(holder.clone(), *expected_version);
            }
            Event::WriteAccepted { holder, version: got, .. } => {
                let based_on = expected.remove(holder);
                if based_on != Some(version) {
                    fail(
                        r,
                        Property::P6Freshness,
                        Some(en.seq),
                        format!(
                            "{holder} committed based on {based_on:?} but {version:?} was in force"
                        ),
                    );
                }
                version = *got;
            }
            _ => {}
        }
    }
}

/// P7: every agent that is not wedged finishes. Measured in virtual time via the ceiling, so it is
/// deterministic.
fn p7(e: &[Entry], r: &mut Report) {
    let mut seen: HashMap<HolderId, u64> = HashMap::new();
    let mut wedged: Vec<HolderId> = Vec::new();
    let mut finished: Vec<HolderId> = Vec::new();
    for en in e {
        if let Some(h) = holder_of(&en.event) {
            seen.entry(h.clone()).or_insert(en.seq);
        }
        match &en.event {
            Event::AgentWedged { holder } => wedged.push(holder.clone()),
            Event::AgentFinished { holder } => finished.push(holder.clone()),
            _ => {}
        }
    }
    for (h, seq) in seen {
        if !wedged.contains(&h) && !finished.contains(&h) {
            fail(
                r,
                Property::P7Liveness,
                Some(seq),
                format!("{h} never reached a terminal event and was not marked wedged"),
            );
        }
    }
}

/// P8: a revocation must actually take the lock away. No write may land afterwards on a ticket
/// issued before it — which is what makes a preemptor's job possible without acquiring the lock.
fn p8(e: &[Entry], r: &mut Report) {
    let mut revoked_at: Option<(u64, Option<Ticket>)> = None;
    for en in e {
        match &en.event {
            Event::Revoked { new_ticket, .. } => revoked_at = Some((en.seq, *new_ticket)),
            Event::WriteAccepted { holder, ticket, .. } => {
                if let Some((rseq, barrier)) = revoked_at
                    && let Some(b) = barrier
                        && *ticket < b {
                            fail(
                                r,
                                Property::P8Preemptibility,
                                Some(en.seq),
                                format!(
                                    "{holder} committed on ticket {ticket:?} after revocation at \
                                     seq {rseq} raised the barrier to {b:?}"
                                ),
                            );
                        }
            }
            _ => {}
        }
    }
}

fn holder_of(ev: &Event) -> Option<&HolderId> {
    match ev {
        Event::TakeRequested { holder, .. }
        | Event::TakeGranted { holder, .. }
        | Event::Read { holder, .. }
        | Event::WriteAttempted { holder, .. }
        | Event::WriteAccepted { holder, .. }
        | Event::WriteRefused { holder, .. }
        | Event::GiveBack { holder, .. }
        | Event::LeaseExpired { holder, .. }
        | Event::PhaseTimeout { holder, .. }
        | Event::CeilingReached { holder, .. }
        | Event::AgentFinished { holder }
        | Event::AgentWedged { holder } => Some(holder),
        Event::TicketIssued { .. } | Event::Revoked { .. } => None,
    }
}

/// Every property. Convenient for the soak tier, where a violation's shape is not predicted in
/// advance and so nothing should be left unchecked.
pub const ALL: &[Property] = &[
    Property::P1MutualExclusion,
    Property::P2TicketValidity,
    Property::P3NoLostWrite,
    Property::P4NoPhantomWrite,
    Property::P5NoLeakedLock,
    Property::P6Freshness,
    Property::P7Liveness,
    Property::P8Preemptibility,
];
