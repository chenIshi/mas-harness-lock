# Code structure

A reader's map of the implementation. The design docs say *why*; this says *where*, and flags the
parts most worth a sceptical eye.

**Orientation:** ~2,250 lines of Rust, single-threaded tokio, no dependencies beyond tokio itself.
The whole thing is a library plus a demo; there is no binary and no server. That is deliberate —
handover §11 chose "library now, process boundary kept open for v2", so nothing in here assumes
shared memory that a socket could not later replace.

---

## 1. Module map

Layered bottom-up; nothing below depends on anything above it.

```
types.rs        Ticket, Version, Span, Holding, Refusal, LockError
                  |
history.rs      Event, Entry, History — the append-only log everything writes to
                  |
lock/mod.rs     trait LockService — acquire, release, keepalive, force_release,
                  |                 current_ticket, current_version, commit
lock/fake.rs    FakeLockService + Faults — in-memory, virtual time, fault injection
                  |
store.rs        FileStore — prepare (temp file) / commit (rename); CrashPoint
                  |
harness.rs      Harness<L: LockService> — the sole gatekeeper. read / write /
                  |                       authorize / apply / progress / abandon /
                  |                       force_release
checker.rs      the eight properties, read over a History
```

`Harness` is generic over `L: LockService` rather than using a trait object, because `async fn` in
traits is not dyn-compatible. That is fine here — v1 picks a backend at construction, not at runtime.

| File | Lines | Owns |
|---|---:|---|
| `src/types.rs` | 90 | The two numbers, and why they are two |
| `src/history.rs` | 102 | The event log — a harness feature, not debug logging |
| `src/lock/mod.rs` | 65 | The backend-agnostic interface |
| `src/lock/fake.rs` | 329 | The in-memory service, and every fault the races need |
| `src/store.rs` | 92 | The file, never written in place |
| `src/harness.rs` | 338 | Gatekeeping: the only path to the resource |
| `src/checker.rs` | 421 | The oracle |
| `tests/conformance.rs` | 450 | Tier 1 — 12 named races |
| `tests/soak.rs` | 181 | Tier 2 — randomized, seeded |
| `examples/demo.rs` | 164 | The conformance demo |

---

## 2. The lifecycle of a write

The one flow worth understanding. Everything else is support.

```
agent                harness                      lock service            disk
  |                     |                              |                    |
  |-- read() ---------->|                              |                    |
  |                     |-- (Full span: acquire) ----->|                    |
  |                     |-- current_version ---------->|                    |
  |                     |-- read file ----------------------------------->  |
  |<-- Snapshot --------|   {content, version}         |                    |
  |                     |                              |                    |
 (thinks — minutes. Under WriteOnly the harness holds nothing at all here.)  |
  |                     |                              |                    |
  |-- write(v, text) -->|                              |                    |
  |                     |== authorize ==============================        |
  |                     |-- (WriteOnly: acquire) ----->|          |         |
  |                     |-- prepare temp file ---------------------------> [tmp]
  |                     |-- commit(ticket, version) -->|          |         |
  |                     |     ticket current? version current?    |         |
  |                     |     both, indivisibly -> bump version   |         |
  |                     |<-- new version --------------|          |         |
  |                     |== apply ==================================        |
  |                     |-- current_version ---------->|  (superseded?)     |
  |                     |-- rename tmp over target ----------------------> [file]
  |                     |-- release ------------------>|                    |
  |<-- Version ---------|                              |                    |
```

Three things to notice:

**The commit point is in the lock service, not on disk.** `LockService::commit` compares the ticket
*and* the version and bumps the version, indivisibly. Ticket validity is therefore defined at the
moment a write is **authorized**, not when bytes land. That is how a commit point is normally
defined, not a weakening — the rename afterwards applies an already-committed decision.

**`authorize` and `apply` are separate public methods.** In normal use `write()` calls both and you
never see `Authorized`. They are split so R14 can drive the gap between them directly, which
*widens* it to make it observable.

**`apply` re-checks the version before renaming.** If another holder committed in the gap, this
authorization has been superseded and the staged content is discarded rather than applied — otherwise
older content would land on top of newer while the lock service recorded the newer. A residual
one-syscall gap between that check and the rename remains, and is recorded as open in
`agent-model.md` §5.4b, not solved.

---

## 3. The two spans

`Span` is a config flag, not a fork (`agent-model.md` §6.5), because §5.2 is deliberately unsettled
on paper. In the code the difference is small and entirely inside `harness.rs`:

| | `Span::WriteOnly` | `Span::Full` |
|---|---|---|
| `read()` | takes no lock | acquires, and holds |
| `authorize()` | acquires here | reuses the open holding |
| Hold time | one local write | the whole think |
| Staleness | caught by the version check | impossible by construction |

The open holdings live in `Harness::open`, a `HashMap<HolderId, Open>`, populated only under
`Span::Full`. Under `WriteOnly` that map stays empty and `progress()` is a no-op — there is no lease
to keep alive, because nothing is held while the agent thinks.

---

## 4. The fake lock service

`FakeLockService` is tier 1's whole reason for existing. It is **not a stand-in for a real backend —
for several races it is strictly better**, because it produces faults a correct backend will not
produce on request.

`Faults` has four switches, each tied to specific races:

| Switch | Produces | Races |
|---|---|---|
| `swallow_keepalive` | a *wedged* holder: alive, healthy, renewing, never progressing | R4 |
| `grant_twice` | the service granting one lock to two holders | R7 |
| `drop_release` | a release that never happens, so only the lease frees the lock | R8, R9 |
| `non_monotonic_tickets` | ticket numbers that repeat or go backwards | the negative test |

Plus `force_expire(resource)`, which lapses a lease at an exact instant.

Two implementation details that matter for reading it:

**Lease expiry is swept lazily**, at the top of every operation, rather than by a background timer.
That is what keeps it deterministic under `tokio::time::pause()` — expiry is *observed* when someone
looks, and `tokio::time::advance()` moves the clock without any real waiting.

**`non_monotonic_tickets` is the only fault whose purpose is to make a check fail.** Without it, the
ticket-validity property would pass forever — and would pass identically if the checker were entirely
broken. It stands in for two real causes: a harness generating its own tickets and restarting (which
the design forbids), and etcd revisions genuinely moving backwards after a restore from an older
snapshot.

---

## 5. The checker

Eight properties, each a small function over `Vec<Entry>`. Tests name the subset they care about —
no race asserts all eight, and nothing is asserted informally.

| | Property | Catches |
|---|---|---|
| P1 | Mutual exclusion | Two accepted writes overlapping |
| P2 | Ticket validity | A write committing on a ticket that is not in force; a ticket source moving backwards |
| P3 | No lost write | An accepted write missing from the final content |
| P4 | No phantom write | Content that was never an accepted write |
| P5 | No leaked lock | A grant never matched by release, expiry, or revocation |
| P6 | Freshness | A write based on a version that had already moved |
| P7 | Liveness | A non-wedged agent that never terminated |
| P8 | Preemptibility | A write landing on a ticket predating a revocation |

**Every property is stated over history order (`seq`), never wall-clock.** That is what keeps them
checkable under virtual time.

No general-purpose serializability search is needed while there is one resource and one write per
transaction — P1–P4 pin the order completely. That stops being true with multiple resources, at which
point something porcupine-shaped becomes necessary.

**Read this file sceptically.** It is the oracle, so a bug here silently invalidates every passing
test. It has already been wrong once: it modelled lock state by *holder*, so a third agent's lease
lapsing while another legitimately held the lock made it report violations against a correct harness.
Both P2 and P5 now key by ticket. The scripted races use two agents and could never have caught it.

---

## 6. Tests

**`tests/conformance.rs`** — tier 1. One `#[tokio::test(flavor = "current_thread", start_paused = true)]`
per race. `start_paused` plus a single-threaded runtime is what makes lease expiry and hold ceilings
deterministic, so every race fires on every run rather than 5% of runs.

To add a race: build a `Rig` with `rig(name, span)` or `rig_with(name, span, faults, crash)`, drive
the harness, then assert `rig.report(&[...])` passes. The helper prints the whole history on failure,
which is usually enough to diagnose without a debugger.

**`tests/soak.rs`** — tier 2. Randomized agent schedules and fault schedules under a seeded xorshift
RNG (hand-rolled so a seed is the *entire* state and replay is exact), checking all eight properties.
A failing seed reproduces exactly; the right response is to promote it into `conformance.rs` as a
named race, not just to fix the bug.

Two deliberate choices in the soak worth knowing before editing it:

- **`non_monotonic_tickets` is excluded** from the randomized faults. It exists to make a check fail,
  so including it would report a violation on every seed and bury real findings.
- **It advances past the lease before checking P5.** The guarantee is "released *or* expired", not
  "released promptly". An earlier version checked too early and produced 110 false violations.

**Missing: tier 3**, the same scripts against live etcd and ZooKeeper. Must live in
`tests/real_backend/` and be `#[ignore]`d by default so `cargo test` stays hermetic.

---

## 7. Where the bodies are buried

If you have limited review time, spend it here.

1. **`checker.rs`** — the oracle. A bug here makes every green test meaningless. Already wrong once,
   in exactly the way that scripted tests structurally cannot catch.
2. **`harness.rs::apply`** — the version guard that makes R14 pass. The least pre-vetted logic in the
   crate: the design predicted a different failure mechanism than the one that actually occurs, so
   this code was written in response to the implementation rather than to the spec.
3. **`fake.rs::sweep` and `Self::current`** — `current` is the *maximum* ticket among holders, which
   is what makes the loser lose under `grant_twice`. If that is wrong, R7 proves nothing.
4. **`harness.rs::progress`** — renewal is progress-driven and capped by the hold ceiling. v1
   simplifies: non-streaming decode and tool execution have no progress signal, so they simply never
   call it and the lease lapses on its own. A real implementation needs a *bounded* background timer
   for those. Recorded in the doc comment, not hidden.
5. **Anything assuming one resource.** `ResourceId` is a `String` with no canonicalization, so two
   spellings of one path would map to two locks and silently break mutual exclusion. Harmless at one
   file, and a real hazard the moment there are two.
