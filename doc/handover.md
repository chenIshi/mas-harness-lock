# MAS coordination via harness-enforced locking — handover doc

**Status:** early proposal, not yet implemented — this doc is the spec for a first mock prototype
**Owner:** Yixi Chen, SANDS Lab, KAUST
**Date:** 2026-08-26
**Companion docs:** [`agent-model.md`](agent-model.md) — the scripted-agent test model for §8,
and the failure modes it must reproduce. [`reference/`](reference/) — verified sources and quotes
for everything cited in §6.
**Purpose:** capture a harness-level coordination mechanism for multi-agent coding, scoped down to
a correctness-first locking baseline, so a fresh session or a GitHub collaborator can implement a
mock prototype without re-deriving this conversation.

---

## 1. Core premise

A coordination guarantee that has to hold for *every* agent at once cannot live in a prompt, a
role label, or a convention — because it only takes one agent, one time, not respecting it to
break the guarantee for everyone downstream. It has to be enforced somewhere every agent is
structurally forced to go through. That "somewhere" is the harness.

This project applies that rule to real-time coordination among agents working the same codebase or
issue: preventing unsafe concurrent writes via harness-enforced locking, rather than relying on any
agent's own judgment or a role it was merely told to respect.

---

## 2. Motivating evidence (why prompt-level coordination doesn't hold)

- Role-prompting alone is weak: Anthropic's "Patterns and problems in emerging multiagent systems"
  (Aug 2026) found that telling agents their roles via prompt "did not make much difference" to
  coordination outcomes in a multi-agent coding swarm, and separately showed unconstrained agents
  sharing a codebase with no coordination mechanism escalating to active sabotage.
- Real systems already coordinate via a shared task list with declared dependencies (e.g., Claude
  Code Agent Teams: backend/frontend/test roles, a blocked task unlocks once the owner marks it
  done) — but the unlock signal is self-report, not verification. Anchor example for the prototype
  (§8): a dependent agent builds on work that later turns out wrong, and has to redo it.

---

## 3. Core design rule

> A coordination property belongs in the harness exactly when a single agent's deviation breaks
> the guarantee for every other agent — not just degrades that one agent's own output.

Two sub-cases, worth keeping distinct:

- **Structural / eligibility properties** (who may act, when, on what) — decidable from the action
  itself, before it happens, from information the harness already has. The harness can enforce
  these natively via an allowlist/capability restriction: the disallowed action is never offered,
  so there's nothing to negotiate or forget. True prevention.
- **Behavioral / correctness properties** (is the output actually right) — not decidable until the
  action executes and something (a test, CI) evaluates it. The harness can't prevent these at
  decision time because the fact doesn't exist yet. What it *can* do is gate other agents' ability
  to depend on the outcome until that outcome is verified — computation stays external, but the
  harness owns whether anyone is allowed to build on an unverified result (avoids "dirty reads").

Locking, the subject of this doc, is a structural/eligibility mechanism: mutual exclusion on who
may write to a resource, when.

---

## 4. Scope decision: start with locking, not the full phase/capability system

Reasons to start here rather than with a broader phase-gated capability system for the whole
agent/task lifecycle:

- Doesn't require deciding up front what a "data item" is in a codebase, or what "commit" means —
  both are genuinely open, hard modeling questions best deferred.
- Small, well-understood, independently buildable and testable.
- Explicit choice: **prioritize correctness over performance for this baseline.** If it performs
  close to serial execution under contention, that's an accepted property of the baseline, not a
  discovered flaw — see §6 on why an unconditional-correctness floor is valuable even if slow.

---

## 5. Mechanism, as currently scoped

**Release safety, within one continuous program run — open, two competing directions.** Not yet
decided between:

- **(a) Harness owns the lock entirely.** The agent never gets raw `acquire()` / `release()` as
  callable tools at all — it only ever calls a harness-mediated action (e.g., a single write tool
  call), and the harness wraps that action with acquire-before / release-after as a structural
  guarantee (decorator / context-manager pattern). Release always runs, success or error, because
  the agent's own generated code never touches the lock and so has nothing to get wrong. Simpler,
  but constrains the agent to only ever act through harness-defined tools — no raw code that
  itself manages a lock.
- **(b) Let agent-generated code touch locks directly, verify it's safe.** More expressive
  (agent can write real lock-using code, not just call fixed tools), but then acquire/release
  pairing has to be checked somehow before that code runs — see §5.1 for what this would take and
  why it's a much harder problem than (a).

These aren't necessarily exclusive (could default to (a) and only reach for (b) where an agent
genuinely needs to write lock-aware application code, not just call a harness tool), but which one
is the actual design for v1 is still open.

**Backstop for total process death.** A decorator can't run its cleanup if the process holding the
lock no longer exists to run any code at all. This case is handled by a session/lease-based
external lock service — a DLM (distributed lock manager), e.g. ZooKeeper or etcd — which detects a
dead/unresponsive holder via its own liveness mechanism and frees the lock automatically,
independent of whether the holder's own code ever gets to clean up after itself.

  **Gap identified 2026-08-27: this covers process *death*, not *liveness without progress*.** A
  hung-but-alive holder renews its lease successfully forever, so the DLM never frees the lock —
  the real, filed bug in mofa-org/mofa#1022, where an exclusive lock held across an LLM call means
  an operator cannot kill a stuck agent, because `stop_agent` needs that same lock. Three
  independent frameworks have this bug (see [`reference/field-reports.md`](reference/field-reports.md)).
  The enforceable form is a **per-phase watchdog** plus fencing-token revocation, not a single
  lease: during *streaming* decode each token is a liveness heartbeat, so a hang there is
  detectable by inter-token timeout; during non-streaming decode and during *tool execution* there
  is no signal at all, and only a blind deadline applies. So serving mode is an input to §5.2's
  choice. Independently confirmed by pi#5778's own proposed fix — three timeout knobs for three
  phases. See [`agent-model.md`](agent-model.md) §5.2.

**Closing the zombie-writer gap.** A DLM freeing an expired lease does not, by itself, stop the
original holder from writing anyway if it doesn't know (or ignores) that its lease expired — this
is the standard critique of naive distributed locking (Kleppmann, "How to do distributed
locking"), and it's real *in general*. It is closed in this design specifically because the harness
is the sole gatekeeper to the protected resource — no side door, the agent never gets direct
filesystem/DB access. The harness re-checks the DLM's current fencing token at the literal moment
it performs the write (not just once at acquire time); a write from a holder whose lease already
expired is rejected regardless of what that holder believes. This only holds because there is no
path to the resource that bypasses the harness — see the zero-exception requirement in §7.4.

### 5.1 If (b): verifying AI-generated code respects lock discipline

If agent-generated code touches a lock directly, whether it correctly pairs acquire/release is a
program-verification problem, not a coordination problem, and is a genuinely different kind of
work from the rest of this doc — worth deciding explicitly whether it belongs in this project at
all, or gets treated as a dependency on separate PL/systems work.

Real precedent exists: eBPF's verifier statically proves `bpf_spin_lock`/`bpf_spin_unlock` pairing
before letting code run — but only because eBPF restricts the language enough (bounded loops, no
arbitrary calls) to make whole-program verification tractable. This doesn't generalize to arbitrary
generated code, which is Turing-complete, where the general version of this check is undecidable.
Related family: WebAssembly's capability-restricted execution, software fault isolation (Wahbe et
al. 1993), proof-carrying code (Necula) — none of these verify arbitrary code either; they all buy
their guarantee by restricting what the code is allowed to express.

**Sub-option under (b): constrain the agent to Rust instead of a bespoke restricted language.**
Rust's ownership model + `Drop` trait give a compile-time guarantee — via the existing, general-
purpose compiler, no bespoke verifier needed — that a lock held in-process is released on every
normal or unwinding exit path. This is a real, actively-discussed idea in practice (not just
theoretical), with two caveats worth building into the harness from the start, not discovering
later:
- The guarantee only holds for *safe* Rust — `unsafe` blocks opt back into unchecked behavior, and
  agents are reported to reach for them (e.g., `unsafe impl Send for MyType {}`) to route around a
  check they didn't satisfy properly. The harness would need to forbid or scan for `unsafe` the
  same way it'd need to police any other bypass — a much smaller, more tractable check than general
  verification, but a real added requirement, not automatic from "just use Rust."
- Only covers the in-process case (§5's "Release safety, within one continuous program run").
  Says nothing about process death, cross-process locks, deadlock policy, or content staleness —
  the DLM backstop and the rest of §5/§7 are unchanged and still needed on top.
- No rigorous benchmark yet on how reliably an LLM produces valid (safe) Rust on the first attempt
  — reports so far are anecdotal ("usually corrects within 1-2 tries after a compiler error"), not
  measured. Worth treating as a training/tooling cost to pay down, not a proven-solved input.

Given all this, (a) is still the cheaper default: don't expose raw lock primitives to agent-
generated code at all, so there's nothing to verify. (b) — whether via a bespoke eBPF-style
restricted language or via Rust plus an `unsafe`-blocking check — is only worth it if the project
specifically needs agents to write lock-aware application code rather than call fixed harness
tools.

### 5.2 Alternative scope: lock the full read-decide-write span, not just the write

Everything above (and the §8 demo) scopes the lock to wrap only the final write, on purpose, so
hold time is bounded by write latency rather than by however long the agent takes to decide. This
subsection records the opposite choice as an explicit, named alternative — not a new idea, but a
textbook mechanism worth citing precisely rather than reinventing under a new name:

- This is **strict two-phase locking (strict 2PL)**: every lock a transaction needs (read and
  write) is acquired during a growing phase and held, untouched, until the transaction is done,
  at which point everything is released at once. (Not to be confused with **2PC**, two-phase
  *commit* — an unrelated protocol for getting multiple nodes to agree atomically on a commit; the
  "two phase" name is the only thing they share.)
- Framed as a reader/writer lock: other agents can still *read* the resource freely during the
  hold, but only one agent can be in the read-decide-write pipeline as a prospective *writer* at a
  time, for that agent's entire decide time, not just its write.
- **What this buys, for free:** the content-staleness problem (§7.1) disappears as a side effect
  instead of needing its own mechanism. Since nothing else can write to the resource while this
  agent is deciding, whatever it read at acquire time is still guaranteed valid at write time —
  one mechanism instead of two (lock + separate version/CAS check).
- **What this costs:** write-side concurrency drops to fully serial per resource, for the whole
  decide time — worse than CoAgent's already-bad measured 2PL number (1.04× speedup over serial),
  because that number came from locking only around the write; here a second prospective writer
  can't even start deciding until the first one finishes end to end.
- This is in direct tension with the "lock only the write, not the decode time" design recorded in
  §8 — genuinely open which one v1 should build. Given §4's explicit correctness-over-throughput
  stance, this is a legitimate, arguably simpler v1 candidate (one mechanism, not two), not just a
  slower one — the tradeoff is real in both directions and both should probably go on record.
- **Revised 2026-08-27 (see [`agent-model.md`](agent-model.md) §5):** full-span is *not* simply
  "one mechanism instead of two." Holding across inference requires lease renewal, which defeats
  the §5 death backstop for a hung-but-alive holder, so it additionally requires a hard hold
  ceiling — and overrunning that ceiling discards minutes of work, which is OCC's measured-bad
  failure mode (0.93× serial, 1.83× tokens). The choice is a trilemma, not a two-way comparison,
  and the ceiling value is a v1 design parameter rather than an implementation detail.
  **Tilted toward full-span by streaming, then back by latency variance — current lean is
  write-only.** Streaming decode gives a per-token liveness heartbeat, so a hang while thinking is
  detectable and holding across decode looked safer. But no *ceiling* can be right under a
  heavy-tailed remote latency: generous enough to spare legitimate p99 work is by definition long
  enough that a stuck holder keeps the lock that long, and tight enough to catch stuck holders
  discards legitimate work constantly. No estimator escapes it — it is a property of the
  distribution. So the deciding question is **where the variance lives**: write-only keeps the
  remote call outside the critical section, leaving a thin-tailed local write to time. Full span
  inherits the whole distribution. Tally: full span buys R6-staleness-free and costs four inflated
  exposure windows, R10 reactivated, and the ceiling-vs-tail tension; write-only has one gap (R6),
  closable by a timing-free write-time version/CAS check. See [`agent-model.md`](agent-model.md)
  §5.5.
  **Not being settled on paper:** the two spans differ only in *when* the harness takes and gives
  back the lock relative to the agent's thinking, so span ships as a **config flag** and both
  variants run the conformance table (§6.5 there). The lean above is a prediction the table can
  falsify.
  What makes any of this tolerable is that **the deadline is not a safety mechanism**: a wrongly
  revoked holder has its write rejected by the write-time fencing check, so a bad threshold costs
  wasted work and never correctness. Timing affects liveness only.
- Whether this is actually as expensive as it sounds depends on §7.7: if "blocked waiting for the
  lock" means the harness simply withholds the next LLM call (the already-stated v1 default),
  extending the lock's span doesn't add any LLM-serving-side cost to the wait — it only adds
  wall-clock/throughput cost, which §4 and §7.6 already say this baseline is allowed to pay.

---

## 6. Prior art to read/cite before building

- **CoAgent / MTPO** (arXiv:2606.15376) — closest existing work, and it should shape how this
  project is positioned, not be treated as an obstacle to route around silently.
  - Exact quote for the lock-duration problem (§3.3), replacing the earlier paraphrase here:
    *"Locks that held for milliseconds in databases now hold for minutes. While one agent infers
    for several minutes after reading a shared object, every other agent waiting on that object
    blocks."*
  - Already benchmarks plain 2PL and OCC as baselines on multi-agent LLM workloads and finds both
    bad: 2PL deadlocks 0.81 times/trial for only 1.04× speedup over serial; OCC aborts 0.95
    times/trial, runs *slower* than serial (0.93×), 1.83× token cost. **A naive "just add locking"
    pitch is already a published negative result — do not present it as new.**
  - Their protocol (MTPO) replaces blocking/abort with: fixed serialization order at launch,
    optimistic execution, notify-on-conflict, and either mechanical saga-style undo/redo (for
    writes landing in the wrong order — no LLM judgment involved) or LLM self-repair (for real
    semantic conflicts — the agent judges relevance and patches). Proven "notified serializable"
    *conditional on* assumptions A1 (individual success), A2 (well-formedness), A3
    (self-healing: the agent correctly judges relevance and correctly patches).
  - **Load-bearing weakness identified this session:** A3 is unverified and unverifiable at
    runtime — no independent check on whether the LLM's judgment was right, no fallback if it
    can't produce a correct repair (A1 assumes this case away entirely). Their own measured rate
    of A3 failing: 5 of 100 trials, the agent misjudged whether a notification actually mattered.
    Nothing in the system catches the other 5% live — only their own after-the-fact benchmark
    grading does. Compressed framing from this session: *"don't lock, and trust the model can
    merge the conflict."* Compressed critique: the correctness proof is conditional on a good
    assumption the way "if I had a vampire, I'd live forever" is conditionally true — real math,
    useless as an operating plan, because the harness is supposed to be the thing that *doesn't*
    need that assumption.
  - **Positioning for this project:** not "beat MTPO on throughput or token cost." The goal is a
    guarantee that actually holds — never inconsistent, regardless of what the LLM does — not a
    best-effort mechanism that works most of the time. MTPO's proof is conditional on A3; that
    makes it a different kind of thing entirely, not a better point on the same scale. This
    project isn't trying to be the conservative end of a spectrum that has MTPO-style approaches
    as the "smarter" end — best-effort is a different category from a guarantee, not a weaker
    version of one.
  - Also worth noting as a real, still-open gap in CoAgent itself: benchmarked on office-automation
    and live K8s-ops tasks (WorkBench, AIOpsLab), not collaborative source-code editing with
    git/import/test semantics; and its correctness notion is pure write-order consistency — it has
    no concept of external test/CI verification at all, so a `σ`-consistent write can still be
    behaviorally wrong and MTPO would call it a success.
- **"Verified Detection and Prevention of Concurrency Anomalies in Multi-Agent LLM Systems"**
  (arXiv:2606.17182) — a formal TLA+/Verus catalog of MAS concurrency anomalies (A1 Stale-
  Generation, A2 Phantom-Tool, A3 Causal-Cascade, A4 Split-View, A6 Tool-Effect Reordering).
  Directly reusable as a test taxonomy. Note their A5 *LongGeneration* was dropped as "subsumed by
  A1" — this project should **not** copy that, since the mofa#1022 family is a pure liveness
  failure with no staleness component. See [`reference/papers.md`](reference/papers.md).
- **ATM: CID-Brokered Pre-Write Admission for Multi-Agent Code Co-Synthesis** (arXiv:2607.00041) —
  fills the gap identified below in CoAgent: this one *is* scoped to collaborative source-code
  editing with git/import/test semantics, signature changes and call sites. Read before settling
  §7.2 — its answer is file-level lockable objects with character-level conflict detection.
- **Continuum: Multi-Turn LLM Agent Scheduling with KV Cache Time-to-Live** (arXiv:2511.02230) —
  prior art for §7.7, which is less open than that section assumes: TTL derived from reload cost
  and queueing delay, and 8.18× headroom measured on SWE-agent workloads.
- **"Position: Multi-Agent Systems Should Prioritize Concurrency Control"** (arXiv:2608.18092) —
  the general "harness/system-level mechanisms, not prompting" thesis is already a published
  position paper. Cite it; don't re-argue it from scratch.
- **Kleppmann, "How to do distributed locking"** — canonical reference for the lease-expiry race
  and the fencing-token fix; specifically a critique of Redlock's safety assumptions under clock
  skew, arguing for ZooKeeper/etcd's session model instead. Read before picking a DLM.
- **eBPF spin lock verifier** (`bpf_spin_lock`/`bpf_spin_unlock`) — real precedent for statically
  verified lock-pairing; see §5.1 for why it doesn't generalize past a restricted language.
- **Schneider, Enforceable Security Policies** (TISSEC 2000) — carried over from the pawl project;
  still the right frame for why eligibility properties are enforceable by a harness-side monitor
  and correctness properties are not (safety vs. liveness/general correctness).

---

## 7. Open questions — explicitly left open, not resolved this session

**7.1 Content staleness.** Locking prevents unsafe *concurrent* writes; it says nothing about
whether a write's *content* is still valid relative to what changed elsewhere while the agent was
deciding. This needs a separate check (read-freshness / version validation at write time), not
provided by mutual exclusion alone. One option: gate visibility on CI-verified state (a dependent
agent can't build on a write until it's tested, not just written) — layered on top of the lock as
a second, independent guarantee, not a replacement for it.

**7.2 Lock granularity.** What counts as one lockable object in a codebase — whole repo, file,
function, declared interface? Too coarse kills concurrency (nothing can run in parallel); too fine
misses semantic conflicts that span multiple objects (e.g., a signature change and its call
sites, in different files). Unresolved. CoAgent's "latent object schema" (a declared read/write
footprint per tool call, objects built lazily via a tool-building agent) is one existing answer
worth studying before inventing a new one.

**7.3 Deadlock policy.** If an agent ever needs to hold more than one lock at a time, standard
deadlock risk reappears (A holds file1 wants file2, B holds file2 wants file1). No policy chosen
yet (fixed global acquisition order, detection-and-abort, etc.).

**7.4c Committing the write — new 2026-08-27.** The resource must never be written in place. Build the
new content in a temporary file, check the ticket, then `rename()` it over the target. This gives crash
safety outright (a reader sees the old file or the new one, never a mixture; caveats: same filesystem
only, and durability additionally needs `fsync` of the temp file and the directory). It **shrinks but
does not close** the gap between checking the ticket and committing: revocation does not interrupt a
write in progress, so the real hazard is a write that *succeeds* after its lock was revoked, and
rename reduces that exposure from the duration of a whole file write to one syscall. Recorded as open,
not solved — see [`agent-model.md`](agent-model.md) §5.4b, and note it is an input to the parked
resource-storage decision rather than an independent question.

**7.4b Harness downtime — new 2026-08-27.** Everything here assumes the harness is running.
Fail-closed is correct and free: with no path to the resource except the harness, a dead harness
means nothing writes, so availability is lost and correctness kept (§4's ordering). But one v1 rule
follows and is not optional — **ticket/fencing numbers must be generated by the lock service, never
from harness-local state.** A restarted harness issuing numbers from its own memory resets them, so
a pre-crash holder can look current again and the write-time check silently stops working. Use
etcd's revision or ZooKeeper's zxid. Separately: the write-time check must be a *single* conditional
write (compare-and-set at the storage layer), not "check then write" — the gap between the two is a
check-then-act race. Harness replication is deferred to v2; see [`agent-model.md`](agent-model.md)
§5.4c and R12.

**7.4 Zero-exception enforcement.** The entire correctness guarantee depends on literally no code
path ever writing to a protected resource without going through the harness's token check. One
missed enforcement point — a debug shortcut, a maintenance script, a bug — silently breaks the
whole thing. No mitigation designed yet beyond "don't do that."

**7.5 Within-program lock-safety verification.** Scoped out entirely as a separate PL/systems
research problem — see §5.1. Not to be built as part of this project's first pass; assume it's
either solved elsewhere or sidestepped by never exposing raw lock primitives to generated code.

**7.6 Throughput cost.** Explicitly deprioritized by design choice (§4). This baseline may perform
close to serial execution under contention — cf. CoAgent's own 2PL baseline getting only 1.04×
speedup. That's an accepted property of a correctness-first floor, not a flaw to fix in v1.
Recovering throughput without losing the unconditional guarantee is future work.

**7.7 LLM-serving-side cost of blocking — deferred to v2, not v1.** *(Partly answered by prior
art — see Continuum, arXiv:2511.02230, in §6 and [`reference/papers.md`](reference/papers.md): the
pin-vs-evict tradeoff below is already mechanized as a cost-derived TTL, and eviction's reliance on
prefix caching is measured to leave substantial per-turn waiting.)* Everything above treats
"an agent waits for a lock" as free. It isn't, if the wait happens while the model has an open,
paused generation session rather than between turns. The same "how long do we hold something
expensive while blocked" tension that motivates the lease/timeout discussion for locks (§5)
recurs one layer down the stack, at the level of GPU serving capacity:
- If the harness withholds the next LLM call until the lock is available (no generation in
  flight during the wait), there's no cost here at all — this should be the v1 default.
- If a session is paused mid-generation instead, the serving layer faces the same tradeoff locks
  do: keep the sequence's KV cache pinned in GPU memory for instant resume (ties up serving
  capacity for the whole wait — a mirror image of holding a lock too long, just one layer further
  down), or evict it and rely on prefix caching (vLLM/PagedAttention, SGLang) to cheaply — but only
  probabilistically — resume without full recomputation.
Not part of v1's scope; flagged here as its own follow-on workstream once the harness mechanism
itself (§8) is validated. This is also the load-bearing assumption behind §5.2's full-span-lock
alternative: as long as "blocked" means the harness withholds the next call rather than parking a
live generation, holding a lock across an agent's whole decide time costs wall-clock throughput
(already an accepted cost, §7.6) but not GPU/KV-cache resources — the two costs are separable, and
only the first one is actually being paid in the v1 default.

---

## 8. What the mock prototype should demonstrate (first GitHub implementation)

- One shared resource in a toy repo (start with a single file).
- A harness layer exposing exactly one mediated write action to agents — no raw lock primitives
  ever reachable from agent-generated code.
- That action wrapped with structural acquire/release (decorator/context-manager) around the
  *write itself* — not around the agent's thinking/decoding time. (Confirms the "non-streaming
  decoding, lock only the quick transaction" insight from this session: lock hold time should be
  bounded by write latency, not by LLM inference time.)
- Backed by a real DLM (etcd or ZooKeeper — not Redis/Redlock, per Kleppmann) for session/lease-
  based recovery from a dead holder.
- Harness checks the DLM's current fencing token immediately before performing each write; reject
  on mismatch regardless of what the calling agent believes about its own lock status.
- **Demo scenario, v1 — modeled agents, not real LLM calls.** What's being validated at this stage
  is the harness mechanism (does it correctly gate on lock state and fencing tokens), not agent
  behavior — and the mechanism's correctness doesn't depend on what an agent decides to write,
  only on the pattern and timing of its acquire/read/write/release calls. So v1 should use a
  scripted or lightly-randomized model standing in for each agent (parameterized: completion
  latency, when it self-reports "done," whether this run is deliberately "wrong") rather than real
  LLM calls. This lets specific races get constructed deterministically and replayed — self-report
  racing ahead of verification, two writers colliding, a holder disappearing mid-lock — instead of
  hoping real model calls happen to reproduce them on a given run. (Same principle Jepsen-style
  distributed-systems testing already uses: script the adversarial interleavings on purpose.)
  Concretely: reproduce the Agent Teams shape — a "backend" model marks its task complete on some
  schedule; "frontend"/"test" models are dependency-blocked on it. Show the baseline behavior
  (self-reported "completed" unlocks dependents immediately, a later-injected "bug" forces rework)
  against the harness-enforced behavior (dependents stay blocked until the harness's lock
  discipline — and, as a stretch goal, CI verification per §7.1 — actually clears).
- **Real LLM calls — later, different question.** Whether real agents actually behave the way the
  model assumes (completion-time distribution, how often a self-report is wrong, whether a
  ToolSmith-style footprint declaration is accurate) is a model-*behavior* question, not a
  mechanism question — worth testing once the mechanism itself is validated, not baked into v1.
- **The model itself is specified in [`agent-model.md`](agent-model.md)** — event alphabet,
  parameter set, and named races R1-R12 with their sources. Two additions there that this section
  did not anticipate: a `preemptor` role (an agent whose job is to *stop* another, without which
  the mofa#1022 family is untestable) and R6, stale content under a perfectly valid lock.
- **Demo redefined 2026-08-27: it is a conformance run, not a narrative.** Run every race in
  [`agent-model.md`](agent-model.md) §4 against the harness and against an unprotected baseline,
  print the pass/fail table. **The expected-results table in [`agent-model.md`](agent-model.md) §6.3
  is the single source of truth for pass/fail status — do not restate its counts here, they drift.**
  Its headline as of 2026-08-27: R5 out of scope, R12 deferred to v2, and R6 a genuine fail under the
  write-only span — that last one *is* the §5.2 decision, so the table doubles as the argument for
  settling it.
  Two findings worth leading with: **six of the ten races exist only because a lock was
  introduced** (dead/stale/stuck holders, split-brain, leaked lock, self-deadlock) — the lock fixes
  one thing and creates six, which is the honest case for harness enforcement rather than against
  it; and **hold time is the exposure window** for four of those six, so full-span's free staleness
  handling is paid for by inflating four windows from milliseconds to minutes and reactivating R10.
- **Scope narrowed 2026-08-27: v1 is a lock and nothing else.** No verified-vs-unverified state, no
  two-tier visibility for dependents, no test/CI gating — §7.1's layered option stays an option.
  Consequence for this section's demo: the Agent Teams contrast (self-reported "done" unlocks a
  dependent, an injected bug forces rework) **cannot be the headline**, because a lock genuinely
  does not prevent it — that is a task-dependency property, not a concurrency one. R4 replaces it
  as the headline: one stuck agent freezes every other writer indefinitely, and the harness takes
  the lock back without ever waiting on it. Real (the mofa#1022 family) and purely a locking
  property. Note the analogy's limit — mofa locks the *agent*, this design locks the *file*, so the
  literal "operator cannot kill it" does not transfer; what transfers is the rule that **no
  administrative operation on the resource may require acquiring the resource lock.** See
  [`agent-model.md`](agent-model.md) §5.4 and R4's note.
- Recoverability/cascadelessness needs no mechanism here: the harness *refuses* writes at the
  write-time fencing check rather than undoing landed ones, so no uncommitted-but-readable state
  exists and no dirty read is constructible. This is free only because writes are never
  speculative — "no speculative writes" is therefore load-bearing, not a simplification.
- **Explicitly out of scope for v1:** multi-lock/deadlock handling (§7.3), in-code lock-safety
  verification (§5.1, §7.5), throughput optimization (§7.6).

---

## 9. Reusable framing from this session

- GitHub issue #482 example (`is_valid_email` / new API endpoint) and the Agent Teams
  backend/frontend/test narrative — ready to reuse as the prototype's demo scenario (§8) and in
  any slide.
- Terminology to keep straight in any writeup: **serializability** (the correctness property being
  protected) vs. **distributed locking / distributed mutual exclusion** (the mechanism used to
  protect it) vs. **linearizability** (a different, narrower property — real-time ordering of
  single operations on one object, not applicable to a read-then-later-write pair the way
  serializability is).
- The zombie-writer resolution: a DLM's lease expiry alone doesn't stop a stale writer *in
  general* (Kleppmann's point) — it's closed here specifically because the harness is the sole
  gatekeeper and re-checks the fencing token at write time, not because DLMs solve it by default.

---

## 10. Safety boundary and trust assumptions

**Scope decision, 2026-08-27: v1 assumes the harness never fails.** This section exists so that is a
*stated boundary* rather than an unexamined gap, and so the discussion is on record before anyone
needs it. Nothing here is a v1 deliverable except §10.2's two starred rules, which are cheap and
already required elsewhere.

### 10.1 What v1 guarantees, given its assumptions

- Two agents never write the same resource concurrently.
- An agent that lost its turn cannot write (checked at the moment of writing, not at acquire time).
- A stuck holder's lock can be taken away without the revoker ever waiting on that lock.
- A dead holder's lock frees on its own.
- A lock service that breaks its own contract and grants one lock twice still yields at most one
  successful writer.

### 10.2 What v1 assumes, and what each assumption is holding up

| Assumption | If it is violated | Would we notice? |
|---|---|---|
| The harness is running | Nothing can write at all — safe by construction, availability only | Immediately |
| The harness is *correct* (not partially broken) | It might write without checking, voiding everything | **No — silent** |
| No route to the resource except the harness (§7.4) | All five guarantees in §10.1 void at once | **No — silent** |
| ★ Ticket numbers come from the lock service, never harness memory (§7.4b) | A pre-crash holder looks current again; stale writes accepted | **No — silent** |
| ★ Checking the ticket and writing are one operation, not two (§7.4b) | A narrow race between check and write | **No — silent** |
| ★ etcd, if ever restored from a backup, is restored **with revision bumping** | Ticket numbers move backwards; a stale holder appears current and the write-time check silently stops working | **No — silent**, and it is a deployment rule with no code to enforce it |
| The resource is only ever committed by atomic rename, never written in place | A crash mid-write leaves a partial file | Yes, on the next read |
| Writes are never speculative — refused, never undone (`agent-model.md` §5.4) | Dirty reads reappear; recoverability stops being free | **No — silent** |
| The lock service is available | No locks can be taken, so nothing writes — safe | Immediately |
| Agents are faulty, not hostile (§10.4) | See §10.4 | Depends |

### 10.3 Rank by silence, not by likelihood

The useful ordering is **not** which assumption is most likely to break. It is which one breaks
*without telling you*. Six of the eight above fail silently — the system keeps running, keeps
reporting success, and the guarantee is simply gone. A harness that is merely *down* is the benign
case precisely because it is loud.

This is also why §7.4's current mitigation — "don't do that" — is the weakest point in the design
rather than a minor to-do. It asks every future developer to remember not to add a shortcut, and
gives no signal if one of them forgets. **That is the same failure this project rejects in §1**
(a guarantee cannot live in a convention that one participant can quietly break for everyone), just
aimed at developers rather than at agents. Worth stating plainly rather than leaving as an irony a
reader has to notice for themselves.

Cheapest hardening, whenever it is picked up: run the harness as one OS user and agents as another so
file permissions enforce the boundary; do not mount the resource where agents can see it; record the
resource's expected state after every harness write and verify it on the next read, so a side-door
write is at least *detected* even if it cannot be prevented; and add a conformance case that tries to
write around the harness and expects to fail, which turns "don't do that" into something that breaks
the build the day a shortcut is added.

### 10.4 Fault-tolerant is not sabotage-tolerant

**v1's threat model is faults, not adversaries** — agents that are slow, stuck, buggy, or wrong, not
agents that are trying to win. This is worth naming because §2's motivating evidence cites
unconstrained agents escalating to *active sabotage*, and a design robust to faults is not thereby
robust to malice.

Two things fall out, one reassuring and one not:

- **Agents cannot forge a ticket, by construction.** Under §5's option (a) the agent never receives,
  holds, or supplies a ticket number — the harness assigns and checks it. So the most obvious attack
  is structurally unavailable rather than merely forbidden. That is a genuine strength of option (a)
  beyond the reasons given in §5.
- **Starvation is unaddressed and is not in the race list.** An agent that repeatedly takes the lock
  can keep others out indefinitely. Nothing in §10.1 promises fairness, and §7.6's acceptance of poor
  throughput is about *contention*, not about one participant monopolising the resource. A fair or
  FIFO queue in the lock service is the standard answer (Curator's mutex documents fairness
  guarantees for this reason). Recorded as a known gap, not scheduled.

### 10.5 What a later version would need

Harness replication, so a single harness process stops being a single point of failure — which turns
"the harness is the sole gatekeeper" into "the harness is the sole *logical* gatekeeper" across
several instances that must agree, making the harness itself a distributed system. Detection of
side-door writes. Fairness. Adversarial agents. Each is a workstream; none is a patch. See
[`agent-model.md`](agent-model.md) R12 for the harness-restart races, specified now and deliberately
not run in v1.

---

## 11. Implementation decisions (2026-08-27)

Settled so the first prototype does not re-litigate them. Test design lives in
[`test-design.md`](test-design.md); no implementation has begun.

| Decision | Choice | Note |
|---|---|---|
| Language | **Rust** | See §11.1 — the usual Rust rationale does *not* fully transfer here |
| Lock service | **Both etcd and ZooKeeper, behind one interface** | `etcd-client` (tokio/tonic) and `zookeeper-client` (runtime-agnostic, ships Curator-compatible lock recipes). Common runtime is therefore tokio |
| Resource | **A real file, single-threaded harness** | Matches §8's toy repo. Atomicity of check-then-write comes from the harness serialising all writes, *not* from the storage layer — stated, not hidden (§7.4b) |
| Deployment | **Library now, process boundary kept open** | Transport-agnostic API so v1 runs in-process and v2 can move it behind a socket without rewriting the core |

The interface must abstract two things that genuinely differ between the backends: the ticket number
(etcd `mod_revision` vs ZooKeeper `czxid`) and the liveness model (etcd lease keepalive vs ZooKeeper
ephemeral node plus session).

**The original justification for two backends no longer holds — flagged 2026-08-27, decision reverts to
the owner.** It was "R7 (split-brain) behaves differently between them." Two findings killed that:
etcd cannot split-brain at all (Raft quorum; see [`lock-interface.md`](lock-interface.md) §7 D4), so R7
is a deliberate contract violation tested only against the fake and does not distinguish the backends;
and the liveness-model difference cannot appear in v1, because one resource means the harness never
holds more than one lock at a time (D7).

So **in v1 the two backends may behave identically in every test**, and the second integration buys
little beyond the discipline of writing the interface — which is already written. Remaining reasons to
keep ZooKeeper are real but weaker: it is the session model Kleppmann endorses over Redlock, so it
carries weight in a writeup; and a second backend is an independent check that the fake is faithful.
This is the cheapest available scope cut and the interface work is not wasted either way.

### 11.1 Two separate questions, and why `Drop` does not answer either

**First, untangling something an earlier draft of this section conflated.** There are two independent
decisions here and they have nothing to do with each other:

- **What the harness is written in** — Rust, per the table above. Agents never observe this. They
  call a mediated write action; the language behind it is invisible to them.
- **What language agents are constrained to emit** — unconstrained, and staying that way. Under
  option (a) agents never touch locks, so there is nothing in their output to verify.

§5.1's Rust sub-option is about the **second** question only: force agent-generated code into Rust so
the compiler proves the *agent* paired its lock calls correctly. That is a sub-option of design (b),
which v1 does not adopt. **It is therefore neither an argument for nor against the harness's own
language, and choosing Rust for the harness implies nothing about what agents may write.**

**Second, and this holds regardless of the above: no language can guarantee release of a remote
lock.** §5.1 argues that Rust's ownership model plus `Drop` gives a compile-time guarantee that a
lock is released on every normal or unwinding exit path. That is true for an in-process synchronous
lock and false for the lock this project actually uses — and the reason is not a gap in Rust.

`Drop` cannot be `async`. Releasing a lock held in etcd or ZooKeeper is a network round-trip, so it
cannot happen inside `drop()`. The RAII pattern assumes synchronous cleanup; the automatic cleanup
path therefore cannot perform the async operation that releasing a remote lock requires. The
practical options are all weaker than the guarantee §5.1 describes: `drop()` hands a release request
to a background task (prompt but best-effort), or blocking inside `drop()` (deadlock-prone on a
single-threaded runtime), or relying on lease expiry (correct but slow).

**The limitation is not a missing language feature.** Even if Rust shipped async cleanup tomorrow it
would still not deliver the guarantee, because two things sit permanently outside any language's
reach: a process can stop existing (`kill -9`, OOM killer, power loss — no code is left to run, in
any language), and the release is a network message that can be lost, delayed, or arrive after the
lock has already been reassigned. The protected thing is not in the language's world at all.

**Empirical confirmation is already in the reference folder: mofa#1022 is a Rust program.** The
compiler accepted it and the bug shipped. Rust does not prevent holding a lock across a network call
— there is a clippy lint for that pattern, and a lint is advice, not a guarantee.

This is handover §1's own argument one level down. §1: a guarantee cannot live in a prompt, because
it depends on every participant choosing to honour it. Here: a guarantee cannot live in a language,
because it depends on the participant still being alive to honour it. Either way it has to live in
the thing that can *refuse* you — the harness plus the lock service. What a language does buy is
narrower and still worth having: you cannot forget to write the release call, and you cannot use the
lock after releasing it.

**Consequences, none fatal:**

- Rust remains a fine choice, on ordinary grounds. It is simply not justified by §5.1, which is about
  a different question (see above) and in any case stops at the process boundary.
- The real backstop for release is the **lease**, exactly as §5 already says. `Drop` makes the
  common case prompt; it does not make it guaranteed. So R8 (abandoned take) is a test of the
  Drop-plus-reaper path, and the lease is what makes it safe rather than merely tidy.
- §5.1's separate point stands untouched: this only ever concerned the in-process case, and the DLM
  backstop was always required on top.

**One unexpected benefit of Rust here.** mofa#1022 — the canonical instance of a lock held across an
LLM call, and the source of R4 — *is a Rust/tokio bug*, holding an `RwLockWriteGuard` across an
`.await` while `stop_agent` needs the same lock. Implementing in Rust/tokio means the unprotected
baseline column can reproduce that bug natively and faithfully rather than approximating it.
