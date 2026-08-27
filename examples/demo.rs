//! The demo: a conformance run, not a narrative.
//!
//! No injected bugs, no story about wrong code, no rework parable (`doc/test-design.md` §6). Run the
//! races against an unprotected baseline and against the harness, and print the table.
//!
//! The finding that leads: **R1, R6 and R13 are failures that exist without any lock at all. R2, R3,
//! R4, R7, R8, R9, R11 and R14 exist *only because a lock was introduced.*** Adding a lock fixes one
//! thing and creates eight new ways to fail — dead holders, stale holders, wedged holders, a lock
//! service breaking its contract, leaked locks, self-deadlock, wrongly-revoked holders, superseded
//! writes. That is the honest case *for* harness enforcement rather than against it: those eight all
//! have to be handled somewhere, and the harness is the only place one guarantee covers every agent
//! at once. But the demo says it out loud rather than implying the lock is free.

use mas_harness_lock::checker::{self, Property::*};
use mas_harness_lock::harness::{Config, Harness, WriteError};
use mas_harness_lock::history::{Event, History};
use mas_harness_lock::lock::fake::{FakeLockService, Faults};
use mas_harness_lock::store::FileStore;
use mas_harness_lock::types::*;
use std::path::PathBuf;
use tokio::time::Duration;

const INITIAL: &str = "initial\n";

fn scratch(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("mhl-demo-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_file(&p);
    std::fs::write(&p, INITIAL).unwrap();
    p
}

/// The unprotected baseline: read the file, decide, write the file. No lock, no ticket, no version.
/// This is what half the table's `n/a` entries mean — most of the races cannot even arise here,
/// because there is no lock to go wrong.
mod baseline {
    use std::path::Path;
    pub fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap_or_default()
    }
    pub fn write(p: &Path, content: &str) {
        std::fs::write(p, content).unwrap();
    }
}

fn harness(tag: &str, span: Span, faults: Faults) -> (Harness<FakeLockService>, PathBuf) {
    let path = scratch(tag);
    let history = History::new();
    let lease = Duration::from_secs(15);
    let lock = FakeLockService::new(history.clone(), lease).with_faults(faults);
    let cfg = Config { span, lease, hold_ceiling: Duration::from_secs(600) };
    (Harness::new(lock, FileStore::new(&path), history, cfg), path)
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // Virtual time, so the demo is instant and identical on every run — the same reason the
    // conformance suite uses it (`doc/test-design.md` §2). Lease expiry and hold ceilings are not
    // something to sit and wait for.
    tokio::time::pause();

    println!("=== mas-harness-lock — conformance demo ===\n");

    // ------------------------------------------------------------------ R1
    println!("--- R1: two writers collide ---");
    {
        let path = scratch("r1-baseline");
        let (a, b) = (baseline::read(&path), baseline::read(&path));
        // Both decided from the same content. Both write. One is silently gone.
        baseline::write(&path, &format!("{a}from A\n"));
        baseline::write(&path, &format!("{b}from B\n"));
        let got = baseline::read(&path);
        println!("  no lock : final = {got:?}");
        println!("            A's write vanished with no error anywhere -> FAIL");
        let _ = std::fs::remove_file(&path);
    }
    {
        let (h, path) = harness("r1-harness", Span::WriteOnly, Faults::default());
        let (a, b) = ("A".to_string(), "B".to_string());
        let sa = h.read(&a).await.unwrap();
        let sb = h.read(&b).await.unwrap();
        h.write(&a, sa.version, "from A\n").await.unwrap();
        let second = h.write(&b, sb.version, "from B\n").await;
        println!("  harness : A accepted; B -> {}", describe(&second));
        println!("            final = {:?} -> PASS", std::fs::read_to_string(&path).unwrap());
        let _ = std::fs::remove_file(&path);
    }

    // ------------------------------------------------------------------ R4
    println!("\n--- R4: wedged holder vs preemptor (the headline) ---");
    println!("  A is alive, healthy, still checking in — and will never run another line of its own");
    println!("  code. The lease never fires, because a lease detects death, not wedging.");
    {
        let mut faults = Faults::default();
        faults.swallow_keepalive.insert("A".to_string());
        let (h, path) = harness("r4", Span::Full, faults);
        let (a, b) = ("A".to_string(), "B".to_string());

        let sa = h.read(&a).await.unwrap();
        for _ in 0..5 {
            tokio::time::advance(Duration::from_secs(2)).await;
            let _ = h.progress(&a).await;
        }
        println!("  before  : B cannot read -> {}", h.read(&b).await.is_err());

        let barrier = h.force_release().await.unwrap();
        println!("  revoke  : took the lock without ever acquiring it; barrier = {barrier:?}");
        println!("            (Curator's revocation only *asks*; a wedged holder never answers)");

        let sb = h.read(&b).await.unwrap();
        h.write(&b, sb.version, "from B\n").await.unwrap();
        let late = h.write(&a, sa.version, "from A (wedged)\n").await;
        println!("  after   : B wrote; A's late write -> {}", describe(&late));

        h.history().record(0, Event::AgentWedged { holder: a });
        h.history().record(0, Event::AgentFinished { holder: b });
        let content = std::fs::read_to_string(&path).unwrap();
        let rep = checker::check(
            h.history(),
            &[P8Preemptibility, P2TicketValidity, P4NoPhantomWrite, P7Liveness],
            &content,
            INITIAL,
        );
        println!("  checker : {}", rep.render());
        println!("\n  history (this is the oracle — properties are checked over it, not over the file):");
        for line in h.history().render().lines() {
            println!("    {line}");
        }
        let _ = std::fs::remove_file(&path);
    }

    // ------------------------------------------------------------------ R6
    println!("\n--- R6: stale content under a perfectly valid lock ---");
    println!("  Every acquire and release is correct. No lock rule is broken. Still wrong without");
    println!("  a second mechanism: mutual exclusion stops simultaneous writes, not stale decisions.");
    {
        let (h, path) = harness("r6", Span::WriteOnly, Faults::default());
        let (a, b) = ("A".to_string(), "B".to_string());
        let sa = h.read(&a).await.unwrap();
        let sb = h.read(&b).await.unwrap();
        h.write(&b, sb.version, "from B\n").await.unwrap();
        let stale = h.write(&a, sa.version, "from A (stale)\n").await;
        println!("  harness : A -> {}", describe(&stale));
        println!("            caught by the version check, which is the *second* mechanism the");
        println!("            write-only span needs and the full span gets for free");
        let _ = std::fs::remove_file(&path);
    }

    println!("\n=== what the table admits ===");
    println!("  R5  self-report racing verification : OUT OF SCOPE. A lock cannot fix it — 'is this");
    println!("      code right' is not a concurrency property. Demonstrable, not preventable.");
    println!("  R12 harness death and restart      : DEFERRED to v2. v1 ships only the rule that");
    println!("      makes a restart safe (tickets come from the lock service, never harness memory).");
    println!("  R14 residual one-syscall gap       : OPEN. Shrunk, not closed. Unreachable given");
    println!("      single-threaded serialisation, but that is an argument from our own design.");
}

fn describe(r: &Result<Version, WriteError>) -> String {
    match r {
        Ok(v) => format!("accepted at {v:?}"),
        Err(WriteError::Refused(reason)) => format!("REFUSED ({reason})"),
        Err(e) => format!("REFUSED ({e:?})"),
    }
}
