# Test design

**Status:** design only — no implementation. Written before any code, deliberately.
**Depends on:** [`agent-model.md`](agent-model.md) for the races and the glossary (§0),
[`handover.md`](handover.md) §11 for the stack.
**Stack:** Rust, tokio (single-threaded runtime), two lock-service backends behind one interface.

---

## 1. What is being tested, and what is not

Being tested: **the harness's gating logic.** Does it allow exactly the writes it should and refuse
exactly the writes it should, under adversarial timing?

Not being tested: whether etcd or ZooKeeper are correct (assume they are), whether real agents behave
like the scripted ones (handover §8 defers this), or whether written code is any *good* (not a
concurrency property — `agent-model.md` §5.4).

---

## 2. The determinism problem, and the three-tier answer

The whole value of scripted agents is that a race fires **every time** rather than 5% of the time.
But most races turn on *timing owned by someone else*: a lease expires on the lock service's clock,
not ours. `tokio::time::pause()` gives deterministic control of time inside our process and has no
effect on a real etcd server. So determinism and realism cannot come from the same run.

**Therefore two tiers, running the same race scripts:**

| Tier | Backend | Time | Role |
|---|---|---|---|
| **1 — scripted** | in-memory fake lock service | virtual (`tokio::time::pause`/`advance`) | Every named race, every run, deterministic. Gates commits |
| **2 — soak** | in-memory fake lock service, `turmoil` network | virtual, randomized under a recorded seed | Many iterations of unscripted interleavings, including harness↔lock-service partitions. Finds races nobody thought of |
| **3 — faithfulness** | real etcd, then real ZooKeeper | real | Same scripts, loose assertions. Confirms the fake did not lie |

**The fake is not a compromise — for several races it is strictly better.** R7 requires the lock
service to hand the lock to two holders at once; real etcd will not do that on demand, and a fake
does it in one line. Same for forcing a lease to expire at an exact instant (R3, R11) and for
freezing a holder's renewals while keeping it otherwise alive (R4). **Tier 1 can construct faults
that tier 2 cannot.** Tier 2's job is narrower: catch places where the fake's semantics diverge from
a real backend — especially ticket-number monotonicity and the two different liveness models.

**Tier 2 exists because tier 1 has a blind spot: it only tests failures we thought of.** R1–R11 encode
*predicted* problems, so a purely scripted suite can never surface an interleaving nobody imagined.
Tier 2 randomizes agent timing and the fake's fault schedule, records the seed, and runs many
iterations, checking the same eight properties from §4 — so a violation is caught even when its shape
was not anticipated. A failing seed must replay exactly, which converts a random find into a new named
race. This is 6.824's approach and for the same reason: its Raft/KV labs run repeatedly under an
unreliable simulated network because these races are probabilistic, not reproducible on demand.

Tier 3 assertions must be **loose** (eventually, within a generous bound) because real timing is not
reproducible. A tier-2 failure means the fake is wrong, or the real backend surprised us; it is a
signal to investigate, not a red build.

---

## 3. Design output: the harness must emit a history

This is the main thing test-first buys, and it is a *harness* requirement discovered by designing
tests rather than an afterthought.

The harness must emit a machine-readable, totally-ordered **event log** — a history. Every entry
carries: logical timestamp, agent id, event kind, resource, ticket number in force, and outcome.
Minimum event kinds: `take_requested`, `take_granted`, `read`, `write_attempted`, `write_accepted`,
`write_refused(reason)`, `give_back`, `lease_expired`, `revoked`, `phase_timeout`.

Tests then assert **properties over the history**, not over the file. Checking the file alone cannot
distinguish "correct by construction" from "correct by luck this run" — the history can. Same
approach Jepsen uses: record a history, check it against a model.

Two consequences worth stating: the history is part of the harness's public surface (it must not be
debug-only logging that gets stripped), and it is what the demo prints.

**Precedent — this is the standard approach, not a novel one.** MIT 6.824's Lab 3 ships exactly this
split: `labrpc`, a simulated network the test controls (drop messages, partition, restart nodes), and
`porcupine`, a checker that consumes a recorded history. Its test names read *"unreliable net,
restarts, partitions, snapshots, many clients, linearizability checks."* The correspondence is direct
— our fake lock service is their `labrpc`; our history-plus-properties is their `porcupine`.

Three deliberate divergences, so the differences are chosen rather than accidental:
1. **Virtual time, not the real clock.** They use real time with generous margins plus repetition;
   we want exact reproducibility. The cost is that virtual time can hide a bug that only appears when
   a real delay lands in a real gap — which is part of why tiers 2 and 3 exist.
2. **No linearizability search.** One resource with one write per turn forces the valid order; a
   solver is only needed when many keys and many concurrent clients open up a large space of legal
   orderings. This changes the moment `n_resources > 1` (handover §7.3).
3. **A different thing is under test.** They test the consensus algorithm itself. We assume etcd and
   ZooKeeper are correct and test our *use* of them — a materially smaller job.

---

## 4. The oracle: what "pass" means

Eight checkable properties. Each race asserts a subset — no race asserts all of them, and nothing is
asserted informally.

| ID | Property | Checked how |
|---|---|---|
| **P1** | Mutual exclusion | No two write executions on one resource overlap in the history |
| **P2** | Ticket validity | Every accepted write held the current ticket *at the instant it landed* |
| **P3** | No lost accepted write | Every write reported accepted is in the final content, or superseded by a later accepted write |
| **P4** | No phantom write | Final content contains nothing that was not an accepted write |
| **P5** | No leaked lock | At end of test nothing is held; every take is matched by a give-back or a lease expiry |
| **P6** | Freshness | Every accepted write's read-version equals the resource version at write time *(write-only span + version check only)* |
| **P7** | Liveness | Every non-hung agent finishes within the configured ceiling |
| **P8** | Preemptibility | A revoke completes within a bounded time regardless of the holder's state |

**No general-purpose serializability checker is needed for v1.** With one resource and one write per
transaction, P1–P4 pin the order completely; a Knossos-style search would be over-engineering. This
stops being true the moment `n_resources > 1`, so the checker question returns with §7.3.

---

## 5. How a race is expressed

A test case is three things, all data rather than code:

1. **Agent scripts** — per agent, the parameter set from `agent-model.md` §3 plus a trace of events
   at virtual times. Example shape: *agent A: read at t=0, decide 500ms, write; agent B: read at
   t=100, decide 100ms, write.*
2. **A fault schedule for the fake lock service** — at virtual time T, expire this lease / grant this
   lock twice / stop accepting renewals from this holder / bump this ticket.
3. **The properties to assert**, plus the expected verdict per property.

Keeping all three as data is what lets the *same* case run against tier 2 with only the fault
schedule dropped and the assertions loosened. Writing them as imperative test code would not survive
that.

---

## 6. Race-by-race test spec

`span` values: **W** = write-only, **F** = full. Cases marked *both* run twice.

| Race | Construction | Asserts | Span |
|---|---|---|---|
| R1 two writers collide | A and B overlapping decide, both write | P1, P3, P4 | both |
| R2 holder dies | A takes, then `crash_at: after_acquire`; advance past lease | P5, P7 (B proceeds) | both |
| R3 zombie write | A paused past lease expiry, B takes and writes, then A writes | P2, P4 | both |
| R4 stuck but alive | A `hang_at: tool`, renewals continue; preemptor revokes | P8, and P7 for the preemptor | both |
| R5 self-report | *out of scope* — demonstrated, not asserted (`agent-model.md` §5.4) | — | — |
| R6 stale content | A reads, B writes and finishes, A writes | P6 | both — **expected fail on W without the version check** |
| R7 contract violation | Fake grants the lock to A and B simultaneously — **not** a realistic partition; etcd cannot split-brain (`lock-interface.md` §7 D4) | P1, P2, P4 | both |
| R8 abandoned take | A takes, reads, `abandon_after_read`, exits cleanly | P5 | both |
| R9 retry leak | Write fails once, retry path re-enters while held | P5, P7 (no self-wait) | both — **agent-reachable only on F** |
| R10 out-of-order | Several writes in one held span, permuted | P3, P4 | F only (n/a on W) |
| R13 crash before commit | Abort injected at each write-path step: temp written, pre-rename, mid-rename | P3, P4 | both — deterministic, an enumeration not a race |
| R14 revoked at the gap | Test hook between ticket-check and rename; revoke there | P2 | both — **expected fail** on file storage; the empirical form of the resource-storage decision |
| R11 false revocation | `decide_dist: pareto`, ceiling below the tail; then `load_spike_at` | P2, P4 (rejected cleanly), P7 | both |
| R12 harness restart | *deferred to v2* — needs the process boundary | — | — |

**Three entries are expected failures and must be written as such**, not omitted: R6 on the write-only
span without a version check, R9's agent-reachable variant on the full span, and R14 under
file-plus-rename storage. A test suite that
only encodes passes cannot show the §5.2 trade-off, which is the point of running both spans.

---

## 7. What cannot be tested in v1, and why

- **R12 (harness death)** — needs a process boundary; the library build has none. The v1 deliverable
  is the *rule* (tickets from the lock service, never harness memory), and that rule is at least
  partially checkable: assert that no ticket value is ever generated inside the harness.
- **R13 / side-door write** — needs OS-level separation (different users, unmounted resource) to be
  meaningful. In-process, nothing prevents a test from writing the file directly, so the assertion
  would be vacuous.
- **Starvation / fairness** — no property above covers it, and `agent-model.md` has no race for it.
  Named gap (handover §10.4), not scheduled.

---

## 7b. What the soak tier actually caught (2026-08-27, first implementation)

Recorded because it is the justification for tier 2 existing, and it is stronger evidence than the
argument that motivated it.

The scripted races (R1–R14) all passed on the first full run. The soak then failed on **106 of 300**
runs, and the failures were two genuine bugs, neither of which the scripted suite could have found:

**1. The history schema was insufficient — the serious one.** `GiveBack` and `LeaseExpired` carried
only the *holder*, not the ticket. One holder can hold two tickets at once (the fake's `grant_twice`
produces exactly that, and it is the contract violation R7 tests), so a holder-only event is ambiguous
about *which* hold ended. The consequence was that the history **could not be replayed into a lock
state**, which is the one job it exists for. Both events now carry the ticket.

This is worth dwelling on: the decision that the history is "a first-class part of the harness, not
debug logging" (§3) was already made, deliberately, and the schema was *still* wrong. Designing tests
first surfaced the requirement; only running randomized ones surfaced the defect.

**2. The checker modelled lock state by holder.** P2 and P5 tracked at most one holder and cleared
state on *any* `GiveBack`/`LeaseExpired` regardless of whose it was. So a third agent's lease lapsing
while another agent legitimately held the lock made the checker believe nothing was held, and it
reported a violation against a correct harness. The scripted races use two agents; this needs three.
Both properties now key by ticket.

**3. One false positive, which was a property mis-statement.** The soak initially reported 110 P5
violations because it checked before outstanding leases could lapse. The design's guarantee is
"released **or** expired", not "released promptly" — automatic cleanup covers code paths, the lease
covers the absence of code (`agent-model.md` §5.4b). The soak now advances past the lease before
checking. Worth recording as a finding about the *property*, not the code: a property that is subtly
stronger than the guarantee produces noise that buries real failures.

After both fixes: 3000 seeds × 2 spans = **6000 runs, clean**.

The moral for anyone extending this: the scripted races encode what we predicted, and they were all
correct on the first run. Everything actually wrong was found by randomization. Do not treat tier 2
as optional polish.

## 8. Rust test layout

- `tests/conformance/` — one file per race, each building the three data pieces from §5 and running
  them through a shared scenario runner. Tier 1, default `cargo test`.
- `tests/soak/` — tier 2. Randomized agent timing and fault schedules under a recorded seed, many
  iterations, checking the same eight properties. A failing seed must replay deterministically, and
  the response to a soak failure is to promote it into `tests/conformance/` as a named race.
- `tests/real_backend/` — same cases, tier 3, `#[ignore]` by default so `cargo test` stays hermetic;
  run explicitly against a live etcd or ZooKeeper.
- The scenario runner, the fake lock service, and the history checker are library code, not test-only
  helpers — the demo needs all three.
- Single-threaded tokio runtime (`#[tokio::test(flavor = "current_thread")]`) with `time::pause()`.
  Matches the single-threaded harness decision and is what makes virtual time work.
- **`turmoil`** (Tokio project) for simulating the network *between the harness and the lock service* —
  mocked time and network, seeded RNG, several hosts on one thread, and *barriers* that inject hooks at
  chosen points, which is exactly what R14's check-to-commit hook needs. `madsim` is the heavier
  alternative (replaces the runtime; used in production by RisingWave). Chosen per the owner's rule:
  prefer an existing tool over hand-rolling. Note it is **not** for R7 — see `lock-interface.md` §7 D4.

**Deliberate ordering: build the fake lock service and the history checker first, before the real
harness.** They define the interface the harness must satisfy, and they are what makes every
subsequent race cheap to add. Building the harness first would mean retrofitting a history onto it.
