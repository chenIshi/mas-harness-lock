//! Tier 1: the scripted conformance suite (`doc/test-design.md` §2).
//!
//! Every race runs against the in-memory fake lock service under virtual time, so each one fires
//! on every run rather than 5% of runs. `start_paused = true` plus a single-threaded runtime is what
//! makes lease expiry and hold ceilings deterministic — a real backend's clock is not ours to
//! advance, which is why tier 3 exists separately and asserts loosely.
//!
//! Assertions are made over the **history**, not over the file: inspecting the file alone cannot
//! distinguish "correct by construction" from "correct by luck on this run".

use mas_harness_lock::checker::{self, Property::*};
use mas_harness_lock::harness::{Config, Harness, WriteError};
use mas_harness_lock::history::{Event, History};

use mas_harness_lock::lock::fake::{FakeLockService, Faults};
use mas_harness_lock::store::{CrashPoint, FileStore};
use mas_harness_lock::types::*;
use std::path::PathBuf;
use tokio::time::Duration;

const INITIAL: &str = "initial\n";

fn scratch(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("mhl-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_file(&p);
    std::fs::write(&p, INITIAL).unwrap();
    p
}

struct Rig {
    h: Harness<FakeLockService>,
    path: PathBuf,
}

fn rig(name: &str, span: Span) -> Rig {
    rig_with(name, span, Faults::default(), None)
}

fn rig_with(name: &str, span: Span, faults: Faults, crash: Option<CrashPoint>) -> Rig {
    let path = scratch(name);
    let history = History::new();
    let lease = Duration::from_secs(15);
    let lock = FakeLockService::new(history.clone(), lease).with_faults(faults);
    let mut store = FileStore::new(&path);
    if let Some(c) = crash {
        store = store.with_crash_at(c);
    }
    let cfg = Config { span, lease, hold_ceiling: Duration::from_secs(600) };
    Rig { h: Harness::new(lock, store, history, cfg), path }
}

impl Rig {
    fn content(&self) -> String {
        std::fs::read_to_string(&self.path).unwrap_or_default()
    }
    fn report(&self, props: &[checker::Property]) -> checker::Report {
        checker::check(self.h.history(), props, &self.content(), INITIAL)
    }
    fn finish(&self, who: &str) {
        self.h.history().record(0, Event::AgentFinished { holder: who.to_string() });
    }
    fn dump(&self) -> String {
        self.h.history().render()
    }
}

// ---------------------------------------------------------------- R1

/// R1 — two writers collide. The failure everyone imagines when they reach for a lock, and the one
/// locking genuinely fixes.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn r1_two_writers_collide() {
    let r = rig("r1", Span::WriteOnly);
    let (a, b) = ("A".to_string(), "B".to_string());

    // Both read the same version, then both try to write — the classic lost update.
    let sa = r.h.read(&a).await.unwrap();
    let sb = r.h.read(&b).await.unwrap();
    assert_eq!(sa.version, sb.version);

    r.h.write(&a, sa.version, "from A\n").await.unwrap();
    let second = r.h.write(&b, sb.version, "from B\n").await;

    // B is refused: it decided from a version that no longer exists. Not lost silently — refused.
    assert!(matches!(second, Err(WriteError::Refused(Refusal::StaleVersion { .. }))), "{second:?}");
    assert_eq!(r.content(), "from A\n");

    r.finish(&a);
    r.finish(&b);
    let rep = r.report(&[P1MutualExclusion, P3NoLostWrite, P4NoPhantomWrite, P5NoLeakedLock]);
    assert!(rep.passed(), "{}\n{}", rep.render(), r.dump());
}

// ---------------------------------------------------------------- R2

/// R2 — the holder dies mid-lock. Without a backstop the resource is locked by a ghost forever.
/// The lease is what frees it, and it works precisely because a dead holder stops checking in.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn r2_holder_dies_lease_frees_lock() {
    let r = rig("r2", Span::Full);
    let (a, b) = ("A".to_string(), "B".to_string());

    let _sa = r.h.read(&a).await.unwrap(); // Full span: A now holds the lock.
    assert!(r.h.read(&b).await.is_err(), "B must not get the lock while A holds it");

    // A's process is gone: it never renews again. Nothing runs its cleanup.
    tokio::time::advance(Duration::from_secs(20)).await;

    let sb = r.h.read(&b).await.expect("lease must have freed the lock");
    r.h.write(&b, sb.version, "from B\n").await.unwrap();
    assert_eq!(r.content(), "from B\n");

    r.h.history().record(0, Event::AgentWedged { holder: a }); // died, never terminated cleanly
    r.finish(&b);
    let rep = r.report(&[P5NoLeakedLock, P7Liveness, P3NoLostWrite]);
    assert!(rep.passed(), "{}\n{}", rep.render(), r.dump());
}

// ---------------------------------------------------------------- R3

/// R3 — the zombie write. A stalls past its lease, B takes over and writes, then A wakes and writes
/// anyway, believing it still holds the lock. Refused on the ticket, at the moment of committing.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn r3_zombie_write_refused_on_ticket() {
    let r = rig("r3", Span::Full);
    let (a, b) = ("A".to_string(), "B".to_string());

    let sa = r.h.read(&a).await.unwrap();
    tokio::time::advance(Duration::from_secs(20)).await; // A's lease lapses while it thinks.

    let sb = r.h.read(&b).await.unwrap();
    r.h.write(&b, sb.version, "from B\n").await.unwrap();

    // A has no idea it lost the lock. Its own belief is irrelevant.
    let zombie = r.h.write(&a, sa.version, "from A (stale)\n").await;
    assert!(zombie.is_err(), "a holder that lost the lock must not write: {zombie:?}");
    assert_eq!(r.content(), "from B\n", "B's write must survive intact");

    r.finish(&a);
    r.finish(&b);
    let rep = r.report(&[P2TicketValidity, P3NoLostWrite, P4NoPhantomWrite]);
    assert!(rep.passed(), "{}\n{}", rep.render(), r.dump());
}

// ---------------------------------------------------------------- R4

/// R4 — stuck but alive, versus a preemptor.
///
/// A is *wedged*: its process is healthy and the lock service sees a perfectly fine client, but it
/// will never execute another line of its own code. So R2's mechanism never fires — that is the
/// whole point, and why a liveness-based lease cannot save you here.
///
/// The preemptor must succeed **without ever acquiring the lock**. Curator's cooperative revocation
/// cannot: it notifies the holder and waits for the holder to comply, and a wedged holder never runs
/// the callback.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn r4_wedged_holder_is_preemptible_without_acquiring() {
    let mut faults = Faults::default();
    faults.swallow_keepalive.insert("A".to_string());
    let r = rig_with("r4", Span::Full, faults, None);
    let (a, b) = ("A".to_string(), "B".to_string());

    let sa = r.h.read(&a).await.unwrap();

    // A keeps "checking in" — and the service keeps believing it. It is alive, healthy, and stuck.
    for _ in 0..5 {
        tokio::time::advance(Duration::from_secs(2)).await;
        let _ = r.h.progress(&a).await;
    }
    r.h.history().record(0, Event::AgentWedged { holder: a.clone() });

    // The preemptor takes the lock away. It never asks A, and never waits on the lock.
    let barrier = r.h.force_release().await.expect("revocation must not block");

    // B can now proceed.
    let sb = r.h.read(&b).await.expect("lock must be reclaimable");
    r.h.write(&b, sb.version, "from B\n").await.unwrap();

    // If A ever wakes, its write is refused — it discovers the loss only now.
    let late = r.h.write(&a, sa.version, "from A (wedged)\n").await;
    assert!(late.is_err(), "wedged holder's write must be refused: {late:?}");
    assert_eq!(r.content(), "from B\n");
    assert!(barrier > sa.version.0.into_ticket(), "barrier must advance past the revoked ticket");

    r.finish(&b);
    let rep = r.report(&[P8Preemptibility, P2TicketValidity, P4NoPhantomWrite, P7Liveness]);
    assert!(rep.passed(), "{}\n{}", rep.render(), r.dump());
}

trait IntoTicket {
    fn into_ticket(self) -> Ticket;
}
impl IntoTicket for u64 {
    fn into_ticket(self) -> Ticket {
        Ticket(self)
    }
}

// ---------------------------------------------------------------- R6

/// R6 — stale content under a perfectly valid lock.
///
/// Every acquire and release is correct, no lock rule is broken anywhere, and the result would still
/// be wrong without a second mechanism. Mutual exclusion stops two agents writing at the same
/// moment; it does nothing about one agent deciding from information that has since changed.
///
/// This is the mechanical half only — "the bytes I read are no longer the current bytes". Whether
/// current bytes make for *good* code is not a concurrency property and is not asked here.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn r6_stale_content_refused_by_version_check() {
    let r = rig("r6", Span::WriteOnly);
    let (a, b) = ("A".to_string(), "B".to_string());

    let sa = r.h.read(&a).await.unwrap(); // A reads at version N, holding nothing.

    // B does a complete, legitimate cycle while A is deciding.
    let sb = r.h.read(&b).await.unwrap();
    r.h.write(&b, sb.version, "from B\n").await.unwrap();

    // A's lock discipline is flawless. Its content is stale anyway.
    let stale = r.h.write(&a, sa.version, "from A (stale)\n").await;
    assert!(
        matches!(stale, Err(WriteError::Refused(Refusal::StaleVersion { .. }))),
        "the version check is the only thing standing between this and a silent stale write: {stale:?}"
    );
    assert_eq!(r.content(), "from B\n");

    r.finish(&a);
    r.finish(&b);
    let rep = r.report(&[P6Freshness, P3NoLostWrite, P4NoPhantomWrite]);
    assert!(rep.passed(), "{}\n{}", rep.render(), r.dump());
}

// ---------------------------------------------------------------- R7

/// R7 — the lock service violates its contract.
///
/// **Not a realistic partition.** etcd cannot split-brain: under partition the majority side serves
/// and the minority rejects all writes, because Raft quorum means a minority cannot elect a leader.
/// So this is defence in depth against something a correct backend does not do, and a faithful
/// multi-node simulation would *prove it unreachable* rather than test it. Deliberately breaking the
/// contract is the mechanism, not a shortcut (`doc/lock-interface.md` §7 D4).
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn r7_lock_service_grants_twice() {
    let mut faults = Faults::default();
    faults.grant_twice.insert(
        {
            let mut p = std::env::temp_dir();
            p.push(format!("mhl-{}-{}", "r7", std::process::id()));
            p.to_string_lossy().to_string()
        },
    );
    let r = rig_with("r7", Span::Full, faults, None);
    let (a, b) = ("A".to_string(), "B".to_string());

    let sa = r.h.read(&a).await.unwrap();
    let sb = r.h.read(&b).await.expect("the fake is breaking its promise on purpose");

    // Only one may land. The later ticket is in force, so the earlier holder is refused — the
    // monotonic ticket is what saves us when the service itself misbehaves.
    let wa = r.h.write(&a, sa.version, "from A\n").await;
    let wb = r.h.write(&b, sb.version, "from B\n").await;
    assert!(wa.is_err() ^ wb.is_err(), "exactly one write must land: {wa:?} / {wb:?}");

    r.finish(&a);
    r.finish(&b);
    let rep = r.report(&[P1MutualExclusion, P2TicketValidity, P4NoPhantomWrite]);
    assert!(rep.passed(), "{}\n{}", rep.render(), r.dump());
}

// ---------------------------------------------------------------- R8

/// R8 — the abandoned take. An agent takes the lock, reads, decides no change is needed, and leaves.
/// If release only ran on the success path the lock would leak. Boring, and extremely common.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn r8_abandoned_take_releases_lock() {
    let r = rig("r8", Span::Full);
    let (a, b) = ("A".to_string(), "B".to_string());

    let _ = r.h.read(&a).await.unwrap();
    r.h.abandon(&a).await; // "actually, nothing to change here"

    let sb = r.h.read(&b).await.expect("lock must be free after a clean abandon, not after a lease");
    r.h.write(&b, sb.version, "from B\n").await.unwrap();

    r.finish(&a);
    r.finish(&b);
    let rep = r.report(&[P5NoLeakedLock, P7Liveness]);
    assert!(rep.passed(), "{}\n{}", rep.render(), r.dump());
}

// ---------------------------------------------------------------- R9

/// R9 — retry after a refused write must not leak or self-deadlock.
///
/// "Just make the lock reentrant" is not the fix: a non-reentrant lock re-taken by a retry blocks
/// forever, and a reentrant one counts acquisitions so a retry that takes twice and gives back once
/// leaks the lock silently. The fix is to retry the *whole* take-write-give-back unit, so a nested
/// take cannot be expressed — which is what this exercises.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn r9_retry_starts_over_without_leaking() {
    let r = rig("r9", Span::WriteOnly);
    let (a, b) = ("A".to_string(), "B".to_string());

    let sa = r.h.read(&a).await.unwrap();
    let sb = r.h.read(&b).await.unwrap();
    r.h.write(&b, sb.version, "from B\n").await.unwrap();

    // A's write is refused because it is stale.
    assert!(r.h.write(&a, sa.version, "from A v1\n").await.is_err());

    // Retrying the whole unit: read again, then write. Not "re-run the write while still holding".
    let sa2 = r.h.read(&a).await.unwrap();
    r.h.write(&a, sa2.version, "from A v2\n").await.unwrap();
    assert_eq!(r.content(), "from A v2\n");

    r.finish(&a);
    r.finish(&b);
    let rep = r.report(&[P5NoLeakedLock, P7Liveness, P3NoLostWrite, P6Freshness]);
    assert!(rep.passed(), "{}\n{}", rep.render(), r.dump());
}

// ---------------------------------------------------------------- R11

/// R11 — false revocation under a heavy tail.
///
/// A holder that is merely *slow*, not stuck, overruns and gets revoked. Its write must be rejected
/// **cleanly** — no corruption, no partial application. This is the test of the property the whole
/// timeout story rests on: since slow and stuck are indistinguishable from outside, a wrong deadline
/// must cost only wasted work and never correctness.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn r11_false_revocation_costs_work_not_correctness() {
    let r = rig("r11", Span::Full);
    let (a, b) = ("A".to_string(), "B".to_string());

    let sa = r.h.read(&a).await.unwrap();

    // A is in the tail of the latency distribution: healthy, productive, just slow. We revoke it
    // wrongly, which no estimator can avoid doing sometimes.
    r.h.force_release().await.unwrap();

    let sb = r.h.read(&b).await.unwrap();
    r.h.write(&b, sb.version, "from B\n").await.unwrap();

    let late = r.h.write(&a, sa.version, "from A (slow but fine)\n").await;
    assert!(late.is_err(), "must be refused, not applied: {late:?}");
    assert_eq!(r.content(), "from B\n", "no corruption from the wrongly-revoked holder");

    r.finish(&a);
    r.finish(&b);
    let rep = r.report(&[P2TicketValidity, P4NoPhantomWrite, P8Preemptibility]);
    assert!(rep.passed(), "{}\n{}", rep.render(), r.dump());
}

// ---------------------------------------------------------------- R13

/// R13 — crash between preparing content and committing it.
///
/// Deterministic: we choose the abort point, so this enumerates injection sites rather than racing.
/// The resource must be the old content or the new one, never a mixture — which is what
/// write-then-rename buys outright.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn r13_crash_before_commit_leaves_old_content() {
    for point in [CrashPoint::AfterTempWritten, CrashPoint::DuringRename] {
        let r = rig_with("r13", Span::WriteOnly, Faults::default(), Some(point));
        let a = "A".to_string();
        let sa = r.h.read(&a).await.unwrap();
        let res = r.h.write(&a, sa.version, "half written").await;
        assert!(matches!(res, Err(WriteError::Io(_))), "{point:?} -> {res:?}");
        assert_eq!(
            r.content(),
            INITIAL,
            "{point:?}: a reader must never see a partial file"
        );
        r.finish(&a);
        let rep = r.report(&[P3NoLostWrite, P4NoPhantomWrite]);
        assert!(rep.passed(), "{point:?}: {}\n{}", rep.render(), r.dump());
    }
}

// ---------------------------------------------------------------- R14

/// R14 — revoked between authorization and application.
///
/// The two phases are driven separately here, which *widens* the gap to make it observable. So a
/// pass means the gap is handled, and a failure would mean it is real and exploitable in principle
/// — neither says anything about how likely the interleaving is in production. That is the correct
/// claim and no stronger one.
///
/// The hazard is not a corrupt file: revocation does not interrupt our write. It is that another
/// holder completes a whole cycle in the gap, and then the first holder's older content lands on top
/// of theirs while the lock service records theirs — file and service disagreeing. The version guard
/// in `apply` is what refuses it.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn r14_superseded_write_is_discarded_not_applied() {
    let r = rig("r14", Span::WriteOnly);
    let (a, b) = ("A".to_string(), "B".to_string());

    let sa = r.h.read(&a).await.unwrap();
    let authorized = r.h.authorize(&a, sa.version, "from A\n").await.unwrap();

    // In the gap: A is revoked, and B completes an entire legitimate cycle.
    r.h.force_release().await.unwrap();
    let sb = r.h.read(&b).await.unwrap();
    r.h.write(&b, sb.version, "from B\n").await.unwrap();

    // A now applies an authorization that has been overtaken.
    let applied = r.h.apply(authorized).await;
    assert!(
        matches!(applied, Err(WriteError::Superseded { .. })),
        "a superseded authorization must be discarded, never applied: {applied:?}"
    );
    assert_eq!(r.content(), "from B\n", "A's older content must not land on top of B's");

    r.finish(&a);
    r.finish(&b);
    let rep = r.report(&[P2TicketValidity, P3NoLostWrite, P4NoPhantomWrite, P5NoLeakedLock]);
    assert!(rep.passed(), "{}\n{}", rep.render(), r.dump());
}

// ---------------------------------------------------------------- the checker's own test

/// The negative test — the only fault whose purpose is to make a check **fail**.
///
/// Without it, P2 passes forever and would pass identically if the checker were entirely broken.
/// Two real causes it stands in for: a harness that generates its own tickets and restarts, which
/// the §7.4b rule forbids; and etcd revisions genuinely moving backwards after a cluster is restored
/// from an older snapshot, which is why etcd ships a revision-bump option on restore.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn negative_non_monotonic_tickets_are_detected() {
    let faults = Faults { non_monotonic_tickets: true, ..Default::default() };
    let r = rig_with("neg", Span::WriteOnly, faults, None);
    let (a, b) = ("A".to_string(), "B".to_string());

    let sa = r.h.read(&a).await.unwrap();
    r.h.write(&a, sa.version, "from A\n").await.unwrap();
    let sb = r.h.read(&b).await.unwrap();
    let _ = r.h.write(&b, sb.version, "from B\n").await;

    r.finish(&a);
    r.finish(&b);
    let rep = r.report(&[P2TicketValidity]);
    assert!(
        rep.violated(P2TicketValidity),
        "the checker must catch a ticket source that moves backwards, or every other pass is \
         meaningless:\n{}",
        r.dump()
    );
}
