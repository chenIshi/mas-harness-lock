# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

Rust toolchain is installed at `~/.cargo/bin` but **not on `PATH`** (installed with
`--no-modify-path`). Prefix commands, or source `~/.cargo/env` first:

```sh
export PATH="$HOME/.cargo/bin:$PATH"

cargo test                      # tier 1 (12 named races) + tier 2 (soak, 300 runs). ~1s, hermetic
cargo test --test conformance   # just the named races
cargo test --test soak          # just the randomized soak
cargo test --test conformance r4_wedged   # one race by name substring
cargo run --example demo        # the conformance demo — prints histories and property reports
cargo clippy --all-targets      # `clippy::await_holding_lock` is the relevant lint here
```

Tier 3 (`tests/real_backend/`, against live etcd/ZooKeeper) is **not written yet**; it must be
`#[ignore]`d by default so `cargo test` stays hermetic.

To widen the soak while debugging, edit the seed range in `tests/soak.rs` and run `--release`
(3000 seeds × 2 spans takes ~7s). A failing seed replays exactly — promote it into
`tests/conformance.rs` as a named race rather than just fixing it.

## Repository state

**v1 is implemented and green:** 12 named races pass, plus 6000 randomized soak runs across both
spans. Layout: `src/types.rs` (Ticket/Version/Span/Refusal), `src/history.rs`, `src/lock/`
(trait + fake with fault injection), `src/store.rs` (temp-file + rename), `src/harness.rs`,
`src/checker.rs` (the eight properties), `tests/`, `examples/demo.rs`.

Design docs remain authoritative for *why*: `doc/handover.md`, `doc/agent-model.md`,
`doc/test-design.md`, `doc/lock-interface.md`, `doc/reference/`. `doc/milestone/` and `doc/tmp/`
are empty placeholders.

`doc/handover.md` is the authoritative source: read it before proposing or writing anything. It
records not just decisions but which decisions are deliberately still open (§7), and several
"obvious" simplifications are already-rejected or already-published negative results. When
implementation begins, whatever build/test tooling gets chosen should be documented here.

- `doc/agent-model.md` — spec for the scripted "error-prone agent" that stands in for an LLM agent
  in v1: event alphabet (§2), parameters (§3), and named races R1–R12 (§4). Its §6.3 table is the
  single source of truth for which races pass — don't restate counts elsewhere. Read alongside §8.
- `doc/reference/` — verified sources with verbatim quotes (`papers.md`, `field-reports.md`). Cite
  from here rather than from memory; the field reports are worth more than the papers for test
  design because they are failure modes that actually happened, with the offending code named.

## What this project is

Harness-enforced locking for multi-agent coding: mutual exclusion on writes to a shared codebase
enforced at the harness layer, so no agent's prompt, role label, or good behavior is load-bearing.

The governing design rule (handover §3): a coordination property belongs in the harness exactly
when a single agent's deviation breaks the guarantee for *every* other agent, not just degrades
that one agent's own output. Two sub-cases stay distinct throughout:

- **Structural / eligibility** properties (who may act, when, on what) — decidable before the
  action, from information the harness already has, so the disallowed action is simply never
  offered. Locking is in this category.
- **Behavioral / correctness** properties (is the output right) — undecidable until something
  external (a test, CI) evaluates it. The harness can't prevent these; it can only gate whether
  other agents may *depend* on an unverified result.

## Positioning constraints (do not relitigate)

These are settled framing choices; work that contradicts them is wrong for this project even if
technically reasonable:

- **v1 is a lock and nothing else.** One property: mutual exclusion on writes, enforced
  structurally. No verified-vs-unverified state, no two-tier visibility, no test/CI gating —
  handover §7.1 floats that as a layered option and it stays an option. Whether a write is *good*
  is not a concurrency property. Recoverability needs no mechanism because the harness refuses
  writes rather than undoing them, so no dirty read is constructible; that holds only while writes
  are never speculative. See `doc/agent-model.md` §5.4.
- **Correctness over throughput, unconditionally.** Performing close to serial execution under
  contention is an accepted property of this baseline, not a bug to fix in v1 (§4, §7.6).
- **Not competing with CoAgent/MTPO on speed or token cost.** CoAgent (arXiv:2606.15376) already
  published 2PL and OCC as *bad* baselines (2PL: 1.04× speedup, 0.81 deadlocks/trial). A naive
  "just add locking" pitch is a known negative result — never present it as new. The claim here is
  a guarantee that holds unconditionally, versus MTPO's proof being conditional on its A3
  self-healing assumption (measured to fail 5/100 trials, with no runtime check). Best-effort is a
  different *category* from a guarantee, not a weaker point on the same scale.
- **Lock the write, not the thinking.** Hold time is bounded by write latency, not LLM decode time
  (§8). The strict-2PL alternative that locks the whole read-decide-write span is recorded in §5.2
  as an explicit, still-open v1 candidate — pick between them deliberately, don't drift. It is a
  three-way tradeoff, not two: full-span/unbounded reproduces mofa#1022, full-span/hold-ceiling
  discards work like OCC, write-only needs a second freshness mechanism. None is free.
- **A DLM with sessions/leases (etcd or ZooKeeper), not Redis/Redlock** — per Kleppmann's clock-skew
  critique.
- **Timeouts and hold ceilings are liveness mechanisms, never safety ones.** "Slow or dead?" is
  undecidable in an asynchronous system, so no threshold is correct — and none needs to be: a wrongly
  revoked holder has its write rejected by the write-time fencing check, so a bad threshold costs
  wasted work, never correctness. Don't propose tuning a constant; under heavy-tailed LLM latency no
  ceiling is both safe and prompt. The design answer is to keep the time-varying call *outside* the
  critical section. See `doc/agent-model.md` §5.5.
- **Span (write-only vs. full read-decide-write) is a config flag, not a fork.** Both variants run
  the conformance table; §5.2 is deliberately not settled on paper. Current lean is write-only plus
  a write-time version/CAS check, as a prediction the table can falsify.
- **The resource is a file on disk** (owner's preference, reaffirmed 2026-08-27). `doc/lock-interface.md`
  §4.2b records how to keep that *and* close the check-to-commit gap: put the resource's version number
  (not its content) in the lock service, make committing one transaction — "if my ticket is current,
  bump the version" — and rename the file only after it succeeds. Requires defining ticket-validity at
  the moment of authorization rather than physical write, which is how a commit point is normally
  defined.
- **Never write the resource in place.** Build content in a temp file, check the ticket, `rename()`.
  Gives crash safety; only *shrinks* the check-to-commit gap to one syscall rather than closing it.
  Recorded as open in `doc/agent-model.md` §5.4b — do not treat it as solved.
- **Operational rule with no code behind it:** if etcd is ever restored from a backup, restore it with
  revision bumping. etcd revisions can otherwise move backwards, making a stale holder look current
  and silently disabling the write-time check. See `doc/lock-interface.md` §5.1.
- **Ticket numbers belong to the harness, never the agent.** Agents never see, hold, or supply one, so
  they cannot forge one — that is why forgery is absent from the threat model rather than forbidden.
- **Revocation is unilateral, never cooperative.** We delete the lock entry and never notify the holder.
  Curator's polite version cannot work here: a *wedged* holder (alive, still checking in, but parked
  forever before its release) will never run the callback. See `doc/agent-model.md` §0 and §5.3.
- **No administrative operation on the resource may require acquiring the resource lock.** A "reset
  the file" / "drop all locks" / "inspect state" path that takes the lock rebuilds mofa#1022. Taking
  a lock back is a write to the *lock service* (bump the ticket), never a claim on the resource.
  Related caution: mofa#1022 locks the *agent*, this design locks the *file*, so its literal "operator
  cannot kill the agent" symptom does not transfer — R4 here is starvation of other writers. Don't
  overstate the analogy; see `doc/agent-model.md` R4.
- **Fencing token re-checked at the moment of the write**, not once at acquire time. The
  zombie-writer gap closes *only* because the harness is the sole gatekeeper to the resource (§7.4
  zero-exception requirement). Any code path that reaches a protected resource without a token
  check silently voids the entire guarantee.
- **Lease expiry covers process death, not liveness without progress.** A hung-but-alive holder
  renews forever; a legitimate full-span holder makes no observable calls while deciding, so the
  two are indistinguishable from outside. Don't propose a progress-based lease — there is no
  signal. The enforceable forms are a hard hold ceiling and fencing-token revocation (revoke by
  bumping the token in the lock service, so the preemptor never contends for the resource lock).
  See `doc/agent-model.md` §5.

## Safety boundary (handover §10)

**v1 assumes the harness never fails.** That is a stated scope decision, not an oversight — handover
§10 is the trust ledger. The key habit it asks for: when touching this design, ask whether a broken
assumption would be *noticed*. Six of the eight assumptions fail **silently** — the system keeps
running and reporting success with the guarantee gone. A harness that is merely down is the benign
case, because it is loud.

Two cheap rules that are v1 deliverables, both easy to violate by accident: ticket numbers come from
the lock service, never harness memory; and checking the ticket and writing are one operation, not
two. Also: v1's threat model is **faults, not adversaries** — starvation and hostile agents are named
gaps, not solved problems.

## Stack and test approach (handover §11, doc/test-design.md)

Rust + tokio (single-threaded runtime); both etcd and ZooKeeper behind one lock-service interface;
the resource is a real file with the harness serialising all writes; built as a library with a
process boundary kept open for v2. **No implementation exists yet — test design came first, on
purpose.**

- **`doc/lock-interface.md`** — the abstraction the fake and both real backends must satisfy. Its §7 is
  the decision log, **now complete** — every item resolved or set as a revisable default — do not resolve the open ones
  silently. Settled points worth knowing: the take-it-back operation is named `force_release` (not
  `revoke`, which means the *cooperative* version in Curator); one lazily-created lock namespace per
  resource; one ZooKeeper session in v1 (the etcd/ZK divergence needs multiple simultaneous locks, which
  is deferred).
- **The content version lives in the lock service**, as a `version:/<path>` key whose own revision *is*
  the version (no counter to maintain) and whose value is the content hash (so a file that disagrees with
  the lock service is detectable, not silent). Chosen because it is the only option that closes the
  check-to-commit gap. Revisable — see `doc/lock-interface.md` §7 D5 for costs and triggers.
- **Defaults, not decisions:** lease 15s, renew 5s (Kubernetes' leader-election numbers, same problem),
  hold ceiling 10 minutes (openclaw's observed agent-run timeout; no prior art exists for a lock held
  this long). The soak tier should set these properly.
- **Renewal never continues past the hold ceiling, whatever drives it.** Streaming decode renews on
  each arriving token, so a wedged decode stops renewing by itself; non-streaming decode and tool calls
  have no signal, so they use a *bounded* background timer that stops at the ceiling. Two clocks, never
  conflated: the lease is process liveness at **seconds** scale (Kubernetes 15s/10s/2s, Chubby 12s), the
  ceiling is a policy limit on work at **minutes** scale (no prior art — nobody held locks this long;
  openclaw's 10-minute agent run timeout is the closest anchor).
- **etcd cannot split-brain** — Raft quorum means a minority partition rejects all writes. So R7 is
  "the lock service violates its contract" (defence in depth), *not* a realistic partition, and must be
  reported that way. A faithful multi-node simulation would prove R7 unreachable rather than test it.
  `turmoil` is used for the realistic partition instead: harness↔lock-service, so keepalives are lost. Key derived facts: `revoke` must work without acquiring the lock; there is
  deliberately **no** `conditional_write` operation, because the resource is a file and no etcd/ZK
  transaction can guard a filesystem write; and etcd ties liveness to a *lease per lock* while
  ZooKeeper ties it to a *session per client*, which diverge once there is more than one resource.
- **The soak tier is not optional polish.** On the first implementation the 12 scripted races all
  passed immediately; the randomized soak then failed 106/300 runs and found two real bugs — the
  history schema was insufficient (`GiveBack`/`LeaseExpired` lacked the ticket, so the history could
  not be replayed into a lock state) and the checker modelled lock state by holder rather than by
  ticket. Scripted races encode what was predicted; randomization found everything actually wrong.
  See `doc/test-design.md` §7b.
- **`doc/test-design.md`** — two-tier testing: an in-memory fake lock service with virtual time
  (`tokio::time::pause`) is the primary deterministic suite, and the same race scripts re-run against
  real etcd/ZooKeeper only to check the fake did not lie. The fake is *better* for several races,
  since it can force split-brain and exact lease expiry that a real backend will not.
- The harness must emit a machine-readable **history** (event log); tests assert eight named
  properties over that history, not over the file. This is a harness requirement, not debug logging.
- Two tests are **expected failures** by design and must be written as such: R6 on the write-only
  span without a version check, and R9's agent-reachable variant on the full span.
- **Caution on `Drop`:** no language guarantees release of a *remote* lock — a dead process runs no
  cleanup code, and the release is a network message that can be lost. The lease is the real backstop;
  `Drop` only makes the common case prompt. mofa#1022 is itself a Rust program, which is the proof.
- **Harness language and agent output are unrelated decisions.** The harness is Rust; agents are not
  constrained to Rust or to anything else, because under option (a) they never touch locks. §5.1's
  Rust idea concerns constraining *agent-generated* code under option (b), which v1 rejects. See §11.1.

## v1 prototype shape (handover §8)

- One shared resource in a toy repo (a single file to start).
- Exactly one harness-mediated write action exposed to agents; **no raw `acquire()`/`release()`
  ever reachable from agent-generated code** (design option (a) in §5 — the cheap default, because
  it leaves nothing to verify).
- Structural acquire-before/release-after wrapping that action (decorator / context manager).
- DLM-backed lease recovery from a dead holder, plus the write-time fencing check above.
- **Modeled agents, not real LLM calls.** v1 validates the mechanism, whose correctness depends
  only on the pattern and timing of acquire/read/write/release calls — so agents are scripted or
  lightly randomized (parameterized: completion latency, when they self-report done, whether this
  run is deliberately wrong), letting specific races be constructed and replayed deterministically
  (Jepsen-style). Real-LLM behavior is a separate, later question.
- Demo scenario: the Agent Teams shape — a "backend" agent marks its task complete, "frontend" and
  "test" agents are dependency-blocked on it. Contrast the baseline (self-reported completion
  unlocks dependents immediately; an injected bug forces rework) against harness-enforced behavior
  (dependents stay blocked until lock discipline, and as a stretch goal CI verification, clears).
- **Out of scope for v1:** multi-lock/deadlock policy (§7.3), static verification of lock
  discipline in generated code (§5.1, §7.5), throughput optimization (§7.6).

## Terminology to keep straight (§9)

**Serializability** = the correctness property being protected. **Distributed locking / mutual
exclusion** = the mechanism protecting it. **Linearizability** = a different, narrower property
(real-time ordering of single ops on one object) that does not apply to a read-then-later-write
pair. Also: 2PL (two-phase *locking*) is unrelated to 2PC (two-phase *commit*) — the name is the
only shared part.
