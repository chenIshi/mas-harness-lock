//! Tier 2: the randomized soak (`doc/test-design.md` §2).
//!
//! Tier 1 has a blind spot — it only tests failures we thought of. R1–R14 encode *predicted*
//! problems, so a purely scripted suite can never surface an interleaving nobody imagined. This
//! randomizes agent timing and fault schedules under a recorded seed, runs many iterations, and
//! checks **all eight** properties, so a violation is caught even when its shape was not
//! anticipated.
//!
//! A failing seed replays exactly, which turns a random find into a new named race. Same reason
//! MIT 6.824 runs its labs repeatedly under an unreliable simulated network: these races are
//! probabilistic, not reproducible on demand.
//!
//! The RNG is hand-rolled xorshift rather than a dependency, purely so a seed is the *whole* state
//! and replay is exact.

use mas_harness_lock::checker;
use mas_harness_lock::harness::{Config, Harness};
use mas_harness_lock::history::{Event, History};
use mas_harness_lock::lock::LockService;
use mas_harness_lock::lock::fake::{FakeLockService, Faults};
use mas_harness_lock::store::FileStore;
use mas_harness_lock::types::*;
use tokio::time::Duration;

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(2685821657736338717).max(1))
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn chance(&mut self, one_in: u64) -> bool {
        self.below(one_in) == 0
    }
}

const INITIAL: &str = "initial\n";

/// One randomized run. Returns the report so the caller can attribute a failure to its seed.
async fn run_one(seed: u64, span: Span) -> (checker::Report, String) {
    let mut rng = Rng::new(seed);

    let mut path = std::env::temp_dir();
    path.push(format!("mhl-soak-{}-{}-{}", std::process::id(), seed, span_tag(span)));
    let _ = std::fs::remove_file(&path);
    std::fs::write(&path, INITIAL).unwrap();
    let resource = path.to_string_lossy().to_string();

    let agents: Vec<HolderId> =
        (0..(2 + rng.below(3))).map(|i| format!("A{i}")).collect();

    // Randomized fault schedule. Deliberately excludes `non_monotonic_tickets`: that fault exists
    // to make a check fail, so including it here would report a violation on every seed and drown
    // out real findings.
    let mut faults = Faults::default();
    for a in &agents {
        if rng.chance(6) {
            faults.swallow_keepalive.insert(a.clone());
        }
        if rng.chance(8) {
            faults.drop_release.insert(a.clone());
        }
    }
    if rng.chance(7) {
        faults.grant_twice.insert(resource.clone());
    }

    let history = History::new();
    let lease = Duration::from_secs(15);
    let lock = FakeLockService::new(history.clone(), lease).with_faults(faults);
    let cfg = Config { span, lease, hold_ceiling: Duration::from_secs(600) };
    let h = Harness::new(lock, FileStore::new(&path), history.clone(), cfg);

    let mut wedged: Vec<HolderId> = Vec::new();

    for _round in 0..(3 + rng.below(5)) {
        for a in &agents {
            if wedged.contains(a) {
                continue;
            }
            match rng.below(10) {
                // Read, think for a random spell, then write.
                0..=5 => {
                    if let Ok(snap) = h.read(a).await {
                        let think = rng.below(30);
                        tokio::time::advance(Duration::from_secs(think)).await;
                        if rng.chance(4) {
                            let _ = h.progress(a).await;
                        }
                        let _ = h.write(a, snap.version, &format!("{a} round\n")).await;
                    }
                }
                // Take and abandon without writing.
                6 => {
                    if h.read(a).await.is_ok() {
                        h.abandon(a).await;
                    }
                }
                // Become wedged: alive, still nominally checking in, never finishing.
                7 => {
                    if h.read(a).await.is_ok() {
                        history.record(0, Event::AgentWedged { holder: a.clone() });
                        wedged.push(a.clone());
                    }
                }
                // Something preempts whoever holds it.
                8 => {
                    let _ = h.force_release().await;
                }
                _ => {
                    tokio::time::advance(Duration::from_secs(1 + rng.below(20))).await;
                }
            }
        }
    }

    // Everything not wedged must have terminated, or P7 rightly complains.
    for a in &agents {
        if !wedged.contains(a) {
            h.abandon(a).await;
            history.record(0, Event::AgentFinished { holder: a.clone() });
        }
    }

    // Let outstanding leases lapse before checking P5.
    //
    // The design's guarantee is "released **or** expired", not "released promptly": automatic
    // cleanup covers code paths, and the lease covers the absence of code (`doc/agent-model.md`
    // §5.4b). A wedged holder, or one whose release was dropped by fault injection, is freed by the
    // lease — so a run that stops before the lease lapses reports a leak that is not one. An earlier
    // version of this soak did exactly that and produced 110 false P5 violations.
    tokio::time::advance(lease * 2).await;
    let _ = h.lock_service().current_ticket(&resource).await; // provoke the sweep

    let final_content = std::fs::read_to_string(&path).unwrap_or_default();
    let report = checker::check(&history, checker::ALL, &final_content, INITIAL);
    let _ = std::fs::remove_file(&path);
    (report, history.render())
}

fn span_tag(s: Span) -> &'static str {
    match s {
        Span::WriteOnly => "w",
        Span::Full => "f",
    }
}

/// Both spans, many seeds. The span is a config flag precisely so the comparison can be run rather
/// than argued about (`doc/agent-model.md` §6.5).
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn soak_all_properties_hold_across_seeds() {
    let mut failures = Vec::new();
    for span in [Span::WriteOnly, Span::Full] {
        for seed in 1..=150u64 {
            let (report, dump) = run_one(seed, span).await;
            if !report.passed() {
                failures.push(format!(
                    "seed {seed} span {span:?}:\n{}\n--- history ---\n{dump}",
                    report.render()
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} soak failure(s). Each is reproducible from its seed — promote it into \
         tests/conformance.rs as a named race.\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}
