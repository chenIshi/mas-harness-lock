# The agent capability model

**Status:** spec for the v1 test harness — not yet implemented
**What this is:** a model of what a real LLM agent *can* do — its capabilities and failure modes,
not a script for what one imagined agent does — used to derive test races for the harness described
in [`handover.md`](handover.md). See handover §8, "modeled agents, not real LLM calls."
**Read first:** [`handover.md`](handover.md) §1 and §3 for the design rule this model exists to
test against; this doc does not restate that argument, only applies it.
**Sources:** [doc/reference/](reference/) — `papers.md`, `field-reports.md`, `lock-primitives.md`.
Every external claim below links to its source there rather than to memory; read the reference docs
for verbatim quotes and full citations.

---

## 0. Glossary

Plain meanings for the shorthand used below. Written out because most of it is borrowed jargon that
reads as obvious once you know it and as noise until then.

| Term | Plain meaning |
|---|---|
| **take / give back the lock** | the two operations that must come in a matching pair around a write ("acquire"/"release") |
| **hold time** | how long the gap between taking the lock and giving it back lasts |
| **span** | *which* work sits inside that gap. **Write-only span** = only the save. **Full span** = the reading and the thinking too |
| **lease** | a lock that expires on its own unless the holder keeps saying "still here." Stops a dead holder locking things forever |
| **ticket number** (fencing token) | a counter the lock service bumps every time the lock changes hands. The harness checks yours is current *at the moment you write*, so a holder who lost the lock gets refused. Called a "fencing token" in the literature |
| **the lock service** (DLM) | the separate server that hands out locks — etcd or ZooKeeper here, not Redis |
| **revoke** | take the lock away from a holder who has not given it back. **Unilateral** here — we change the world and never notify the holder |
| **cooperative revocation** | the *polite* alternative, and what Curator actually offers: the holder's code gets a callback saying "someone wants this lock", and **its own code must choose to release**. Nothing compels it |
| **wedged** | the process is alive and healthy — not crashed, still checking in with the lock service, which sees a perfectly fine client — but its thread of execution is parked forever at a point *before* the release. Real causes (pi#5778): an LLM stream that stopped sending but never closed; a tool call that never returns. **Distinct from dead**, and the distinction is why leases do not help: a dead holder stops checking in and gets reaped, a wedged one checks in forever |
| **nested take** | taking the lock again while already holding it — the R9 bug |
| **reentrant** | a lock that *permits* a nested take, by counting how many times you took it |
| **exposure window** | how long a risky period lasts. For most lock failures this equals hold time |
| **heavy-tailed** | a delay that is usually quick but occasionally *far* slower — so no single timeout value works well |
| **the blind interval** | any stretch where the harness gets no signal at all, so it cannot tell slow from stuck |

## 1. What this model is: capabilities, not content

A capability model states what an agent **can** do, so the harness can be tested against the worst
thing in that range rather than the typical thing. It is deliberately not a claim about what any
particular agent *will* do on a given run — see §1.2 for why that distinction collapses anyway.

### 1.1 Two capability facts that drive the whole design

**Fact 1 — anything an agent's generated code may get wrong, it must be treated as eventually
getting wrong.** If agent-generated code could call lock `acquire`/`release` directly, then across
enough runs, whatever *can* go wrong at any one of those call sites — for example, an agent forgetting
the matching release — *will* go wrong at least once, and that single mistake is enough to break the
guarantee for every other agent, not just the one that made it. This is exactly the reason
[`handover.md`](handover.md)'s §1 gives for why a coordination guarantee cannot live in a prompt or a
role label. §1.2 below applies that argument at this level; it is not re-derived here. The
consequence for design: handover §5's option
(a) removes raw lock primitives from the agent's reachable surface entirely, so there is nothing for
a "may" to eventually become a "would" *of* — see §2, where the event alphabet has no
`acquire`/`release` events for exactly this reason.

**Fact 2 — decode has two distinct capability modes, and the harness's ability to detect a hang
differs between them.** Streaming decode emits tokens one at a time, so each arriving token is a
liveness signal — a stream that goes quiet mid-generation is observable, which is the exact
dropped-connection failure in [pi#5778](reference/field-reports.md). Non-streaming decode is one
blocking call, response ready or not, with no signal in between, so a hang here is indistinguishable
from a slow-but-healthy response until a whole-call timeout fires — and tool execution behaves the
same way *regardless of decode mode*, because a tool call is one blocking operation with no
token-like heartbeat, by definition. That makes tool calls the phase where long unobserved waits
actually concentrate in an agentic loop: a tool call must complete before the agent can continue no
matter how its decode is served, and it is where [pi#5778](reference/field-reports.md)'s *second*
unbounded wait lived.

These two facts are why the model needs a `streaming` flag and a `hang_at` phase marker (§3): "is
this agent alive?" has a different answer depending on which phase it is in, and that difference has
to be a first-class part of the model, not an afterthought bolted on later (§5.2 works through the
consequence in full).

### 1.2 Why the model emits a call trace, not content

Given §1.1, the harness's correctness depends only on **the pattern and timing of
acquire/read/write/release calls**, never on what an agent decides to write — so the model does not
generate content at all. It emits a call trace: a sequence of the events in §2, timed and ordered by
the parameters in §3.

That buys the thing real LLM calls cannot: **the adversarial interleavings become constructible and
replayable on purpose** rather than hoped for. Same principle as Jepsen-style testing. A race that
fires on 5% of real runs is a race you cannot debug; a race you script fires every time.

**What this can and cannot prove, stated so it is not overclaimed later:** this harness can prove the
mechanism gates correctly under a given interleaving. It cannot say how *often* real agents produce
that interleaving — that is a separate, model-*behavior* question (handover §8 defers it), and no
amount of scripted testing answers it.

---

## 2. Event alphabet

The agent model emits only these. Note what is *absent*: under handover §5 design option (a), the
agent has no `acquire`/`release` in its alphabet at all — the harness inserts them around the
mediated action. Which is the whole point; there is nothing for the agent to get wrong.

| Event | Meaning | Harness-observable |
|---|---|---|
| `read(r)` | read resource `r` through the mediated action | yes |
| `decide(d)` | occupy `d` ms producing nothing | **no** — this is the blind interval |
| `write(r, v)` | request a write of `v` to `r` | yes |
| `report_done()` | self-report task completion | yes |
| `exit()` | clean termination | yes |
| `crash()` | process death, no cleanup | only via DLM liveness |
| `hang()` | never returns, process stays alive | **no** — indistinguishable from `decide(∞)` |

`hang()` versus a long `decide()` is what R4 turns on, and **how indistinguishable they are depends on
which capability mode from §1.1 the agent is in**: distinguishable under streaming decode (a token
heartbeat that goes quiet is observable), not distinguishable under non-streaming decode or tool
execution (both are opaque, blocking calls). That is why the model carries both a `streaming` flag
and a `hang_at` phase marker (`stream` vs `tool`) rather than one undifferentiated "hang" event — §5.2
works through what the harness can actually do with each case.

---

## 3. Parameters

Each knob exists to induce a specific failure. The reference column is why we believe the failure
is real rather than invented.

| Parameter | Domain | Failure it induces | Source |
|---|---|---|---|
| `decide_ms` | int \| ∞ | long hold — the thing that turns cheap DB locking expensive | [CoAgent §3.3](reference/papers.md) |
| `streaming` | bool | whether decode emits a token heartbeat — decides if `hang_at: stream` is detectable at all | [pi#5778](reference/field-reports.md), §5.2 |
| `decide_dist` | fixed \| lognormal \| pareto(α) | latency *distribution*, not a constant — a heavy tail is what breaks fixed ceilings (§5.5) | §5.5 |
| `load_spike_at` | none \| ms | inflate all agents' latency at once, to produce correlated revocation | §5.5 |
| `write_ms` | int | slow write; lease expiry *mid-write* | [Kleppmann](handover.md) |
| `hang_at` | none \| stream \| tool | unbounded hold, process alive and healthy | [pi#5778](reference/field-reports.md) |
| `crash_at` | none \| after_read \| after_acquire \| mid_write \| before_release | total process death | handover §5 |
| `write_after_expiry` | bool | zombie writer — writes believing it still holds | [Kleppmann](handover.md) |
| `report_early` | bool | self-report before verification; unlocks dependents on an unverified basis | handover §2, §8 |
| `wrong` | bool | deliberately wrong output, injected later as a "bug" | handover §8 |
| `stale_write` | bool | writes content computed from a value since overwritten | [A1 Stale-Generation](reference/papers.md) |
| `role` | writer \| dependent \| preemptor | control-plane contention for the data-plane lock | [mofa#1022, openclaw#18470](reference/field-reports.md) |
| `reacquire_on_retry` | bool | self-deadlock (non-reentrant) or silent leak (reentrant) — see R9 | [Curator, reentrant-mutex docs](reference/lock-primitives.md) |
| `abandon_after_read` | bool | acquires, decides not to write, exits | — |
| `write_order` | in-order \| permuted | multiple effects externalized out of intended order | [A6](reference/papers.md) |
| `n_resources` | int (v1: 1) | multi-lock deadlock — parameterized now, exercised post-v1 | handover §7.3 |

`role: preemptor` is the parameter the original handover §8 sketch was missing. It is not a writer at all —
it models an operator or supervisor trying to *stop* another agent. Without it, the entire
mofa#1022 family is untestable, because that bug only appears when something needs to intervene.

---

## 4. Named races

Each entry below follows the same four-part shape: setup → what an unprotected baseline does → what
the harness must do → what it proves. R1–R3 formalize races handover §8 already names; R4 and
R6–R11, R13, R14 were added while writing this model. Two numbers are deliberately not full test
races and are marked as such where they appear: **R5** is a real failure but out of the lock's scope
(§5.4 explains why, and it stays in this list only as a documented boundary); **R12** is specified
now but its test is deferred to v2 (§5.4c) because running it needs a harness that can restart,
which v1 does not build.

**R1 — Two writers collide.** Two `writer`s, overlapping `decide`, both `write(r)`. Baseline: last
write wins, first is lost silently. Harness: one write lands, the other is serialized behind it or
rejected. *Proves: basic mutual exclusion.*

**R2 — Holder dies mid-lock.** `crash_at: after_acquire`. Baseline: resource locked forever.
Harness: DLM lease expires, lock frees, a waiter proceeds. *Proves: the §5 lease backstop.*

**R3 — Zombie write after expiry.** `crash_at: none`, `write_after_expiry: true` — holder is paused
long enough for its lease to lapse, a second agent acquires and writes, then the first writes
anyway. Baseline: stale write clobbers the newer one. Harness: rejected on fencing-token mismatch
at write time, regardless of what the holder believes. *Proves: the write-time fencing check, and
the handover §7.4 sole-gatekeeper property it depends on.*

**R4 — Stuck-but-alive holder vs. preemptor.** ⚠️ *The one that changes a design decision — §5 below.*
One `writer` with `hang_at: tool` (or `decide_ms: ∞`); one `preemptor` that must revoke it. The
process never dies, so R2's mechanism never fires. Baseline: the stuck holder keeps the lock
indefinitely and **every other writer is blocked forever**. Harness: the preemptor must succeed
**without ever acquiring the protected lock**. *Proves: revocation is possible without contending for
the resource.*

  > **The mofa#1022 analogy is narrower than it first looks — read this before citing it.**
  > mofa locks **the agent object**; this design locks **the file**. In mofa the stuck thing and the
  > locked thing are the same object, which is exactly why `stop_agent` deadlocks: stopping requires
  > touching the agent, and the agent is locked. Here they are separate — a stuck agent holds the file
  > lock, and killing that agent does not require the file lock. **So the literal "an operator cannot
  > kill it" does not transfer.** R4 here is a *liveness* failure: a stuck holder starves every other
  > writer, and taking the lock back must not require waiting for it. Real, still needs
  > fencing-revocation (§5.3), but not the same story.
  >
  > **What does transfer is a design rule**, and it is the durable lesson: *any administrative
  > operation on the resource must not require acquiring the resource lock.* If v1 ever grows a "reset
  > the file", "drop all locks", or "inspect state" path that takes the lock, mofa#1022 has been
  > rebuilt from scratch. Fencing-revocation is what prevents that, by making revocation a write to
  > the lock service rather than a claim on the resource.
  >
  > Also worth knowing *why* mofa took the lock at all, since "why lock an agent?" is a fair question:
  > an agent holds mutable state (history, memory, tool state) and two concurrent requests would
  > scramble it, so one-request-at-a-time is a legitimate goal. In Rust it was not even a choice —
  > `execute()` takes `&mut self`, so the compiler *requires* exclusive access, which is why it is a
  > write lock. **The bug is the duration, not the existence of the lock**, and the issue's own
  > long-term fix is to change the signature to `&self` so exclusivity stops being required.

**R5 — Self-report races verification.** *Not a race the lock must pass — kept as a documented scope
boundary. See §5.4 for the full argument.* A `writer` with `report_early: true, wrong: true` and a
`dependent` blocked on it: the self-report unlocks the dependent, a bug surfaces later, the
dependent's work is discarded. This is a real failure, but it is filed under the wrong mechanism — it
is a task-*dependency* question (does the dependent wait for verification?), not a mutual-exclusion
question (can two agents write at once?), and v1 is a lock and nothing else. The `report_early` and
`wrong` knobs stay in §3 so the scenario can still be *demonstrated* in the conformance demo (§6),
but the harness is not expected to prevent it, and no test asserts that it does.

**R11 — False revocation under a heavy tail.** `decide_dist: pareto`, ceiling set below the tail: a
slow-but-perfectly-healthy holder overruns, gets revoked, then finishes and tries to write. Harness:
the write is **rejected cleanly** — no corruption, no interleaving, no partial application. Then the
correlated variant, `load_spike_at`: many holders overrun simultaneously and all retry at once.
*Proves the property the whole timeout story rests on — that a wrong deadline costs wasted work and
never costs correctness (§5.5) — plus that mass revocation degrades rather than breaks.*

**R6 — Stale content under a valid lock.** `stale_write: true`, no lock violation anywhere. Every
acquire/release is correct and the result is still wrong. Harness (write-only span): **must fail
without a freshness check** — this is handover §7.1 with no mitigation, and the test should assert
the failure rather than pretend otherwise. Harness (full span): passes for free, per §5.2.
*Proves: mutual exclusion is not sufficient for correctness — the sharpest single argument for the
full-span option, and exactly the anomaly [A1 Stale-Generation](reference/papers.md) formalizes.*

  > **Split the two halves; only one is in scope.** *Mechanical:* the bytes read are no longer the
  > current bytes — caught by a version number / compare-and-swap, no understanding of the code
  > required. *Semantic:* the bytes are current and the edit is still wrong for the codebase — no
  > concurrency mechanism touches this, and it is correctly out of scope. **R6 is the mechanical
  > one.** Locking cannot prevent bad code and does not claim to.
  >
  > **This is textbook, under three names** — worth knowing so we cite rather than reinvent:
  > *lost update* (classical); *antidependency cycle* in serializability theory (A's read→B's write
  > edge plus B's write→A's write edge closes a loop in the precedence graph, so the schedule is not
  > conflict-serializable — [CoAgent](reference/papers.md) lists this by name); *A1 Stale-Generation*
  > in the [concurrency-anomalies catalog](reference/papers.md) (arXiv:2606.17182).
  >
  > The classical remedies map one-to-one onto §5.2's open question: hold read locks to
  > end-of-transaction (**strict 2PL** = full span) versus validate at write time (**OCC** = version
  > check). §5.2 is therefore not a novel dilemma — it is 2PL-vs-OCC, and what makes it hard again
  > is only that both textbook answers assume short transactions.

**R7 — Lock service violates its contract.** *Named for what it actually tests — not "split-brain
DLM," a name that would be misleading here (see below).* Force the fake to grant the same lock to two
holders. Baseline: concurrent writes. Harness: at most one write lands, because tickets are monotonic
and the loser's is stale. *Proves: correctness survives the lock service breaking its promise, not
just agents failing. This is [A4 Split-View](reference/papers.md) from the concurrency-anomalies
catalog.*

  > **Why this isn't called "split-brain": etcd cannot split-brain.** Under partition the majority side serves
  > and the minority side rejects all writes — Raft quorum means a minority partition cannot elect a
  > leader. No correct etcd ever hands one lock to two holders. So this is **defence in depth against a
  > scenario a correct backend does not produce**, not a realistic partition, and it must be reported
  > that way. Consequence for the fake: a faithful multi-node simulation would *prove R7 unreachable*
  > rather than test it, so deliberately breaking the contract is the mechanism, not a shortcut. See
  > [`lock-interface.md`](lock-interface.md) §7 D4.
  >
  > The realistic partition worth simulating is a different one — between **the harness and the lock
  > service**, so keepalives are lost and the lease lapses while the harness still believes it holds.
  > That is R3/R11, and it is what `turmoil` is for.

**R8 — Abandoned acquire.** `abandon_after_read: true` — acquires, reads, exits cleanly without
writing. Harness: lock released on the clean exit path, not left to lease expiry. *Proves: the
decorator/context-manager release runs on all exits, including the boring one. Cheap test, common
bug.*

**R9 — Retry lock leak / self-deadlock.** `reacquire_on_retry: true` with a write that fails once,
so something retries it while the lock is still held. Harness: no double-hold, no waiting on itself,
no lock left stuck. *Proves re-entrancy handling in the
harness's own wrapper — the one piece of lock code agents cannot be blamed for.*

  > **This race is documented, real behavior, not a hypothetical.**
  > See [`reference/lock-primitives.md`](reference/lock-primitives.md). It fails in **both**
  > directions, which is the useful part: a *non-reentrant* lock re-acquired by a retry "would block
  > indefinitely" (self-deadlock, one agent, zero contention), while a *reentrant* one counts
  > acquisitions and "is only released ... once the owning thread has unlocked it the same number of
  > times it was acquired" — so a retry that acquires twice and releases once **leaks the lock
  > silently** until lease expiry. Apache Curator's `InterProcessMutex` documents exactly this
  > balanced-call requirement. **So "just make it reentrant" is not a fix** — it trades a loud
  > deadlock for a silent leak, which is worse here.
  >
  > **Where it can actually arise — and this differs by span, which matters:**
  > - *Write-only span:* **the agent cannot cause this.** The agent calls the write tool; that call
  >   finishes — and the harness gives the lock back on its way out — before the agent even sees the
  >   error and decides to retry. So an agent retry always starts from a clean state. What remains is
  >   a harness bug: the harness's own retry logic sitting *inside* its own lock-handling code, or one
  >   internal helper taking the lock and then calling another that takes it again.
  > - *Full span:* **the agent can cause it.** Here the harness keeps the lock across several agent
  >   turns — taken when the agent reads, given back after it writes. A second read or write from the
  >   agent therefore arrives while the lock is still held, and the agent has no way to know. Add
  >   this to §5.5's tally of full-span costs.
  >
  > **Resolution — on failure, start over from taking the lock; and treat a nested take as a bug.**
  > Retry the whole take-write-give-back sequence, never just the write in the middle of it, so a
  > nested take cannot happen at all. This is not merely tidier: for the most likely failure cause — a write rejected on
  > fencing-token mismatch — retrying the write alone is *wrong*, because the rejection means the
  > lock is gone and must be taken again. It also fits the rest: starting over means reading again
  > first, which is exactly what the R6 freshness check wants. Refusing a nested take then turns any
  > remaining harness bug into a loud error instead of a hang or a silent leak.

**R12 — Harness death and restart.** *(Specified now, deferred to v2 — §5.4c.)* Kill the harness at
each phase: after taking the lock, between the write landing and the give-back, and mid-write. Then
restart it. Harness: no write is lost or duplicated; a pre-crash holder is never able to write again
after the restart; the resource is never assumed clean on restart. *Proves the ticket numbers really
do come from the lock service rather than harness memory — the §5.4c v1 rule — and that fail-closed
holds while the harness is down.* The v1 deliverable is the rule; running this race needs harness
failover and is v2.

**R13 — Crash between prepare and commit.** Inject an abort at each step of the write path: after the
temp file is written, before `rename`, during `rename`. Harness: the resource content is **either the
old value or the new one, never a mixture**, and no temp file is left visible as the resource.
*Asserts P3, P4.* **Deterministic** — we choose the abort point, so this is not a race at all, just an
enumeration of injection sites. Settles §5.4b's crash-safety claim by test rather than by assertion.

**R14 — Revoked between the ticket check and the commit.** The one-syscall gap from §5.4b, made
observable: a test hook between "ticket checked, matches" and `rename` yields control, the preemptor
revokes there, then the rename proceeds. Harness: the write must **not** land. *Asserts P2.*
**Deterministic**, via the hook. Two things to be honest about:

  > The hook *widens* the gap to make it observable, so a pass means "the gap is handled" and a failure
  > means "the gap is real and exploitable in principle" — neither says anything about how *likely* the
  > interleaving is in production. That is the correct claim to make and no stronger one.
  >
  > **Expected result differs by resource storage, which is the point of the test.** With the resource
  > as a file plus rename: **expected failure** — the ticket was valid when checked and the rename has
  > no way to re-check, so a stale write lands. With the resource inside the lock service: **pass**,
  > because compare-and-commit is one transaction. So R14 is the test that settles the parked
  > resource-storage decision empirically instead of by argument.

**R10 — Out-of-order effects.** `write_order: permuted`, multiple writes in one held span.
Harness: externalized in issue order, or refused. *Proves: [A6 Tool-Effect
Reordering](reference/papers.md). Lower priority for v1's single-file resource; include the knob,
defer the race.*

---

## 5. What R4 forces on the span decision

Handover §5.2 records full-span locking (holding across read-decide-write, not just the write) as an
open alternative, attractive because it absorbs content staleness for free (R6). **R4 shows that
attraction is not the whole story**, for a reason specific to this design rather than a general
objection to full-span locking:

The argument, in three steps:

1. A full-span lock is held across `decide_ms`, i.e. across inference — minutes, per
   [CoAgent §3.3](reference/papers.md).
2. To survive that span, the lease must either be very long or be renewed. Renewal is
   liveness-based: it proves the holder's *process* is alive.
3. **A hung-but-alive holder renews successfully forever.** [pi#5778](reference/field-reports.md)
   establishes this state arises from ordinary causes — a dropped stream, an unresolved promise —
   and that the process "silently dies in the background" while staying up. So the renewal machinery
   that makes full-span viable is precisely what defeats the §5 lease backstop.

Handover §5's backstop covers **process death**. It does not cover **liveness without progress** —
a hung-but-alive holder is neither dead nor healthy in the sense the lease checks for. R4 is the
test that makes this gap visible.

It is tempting to conclude a progress-based lease is simply impossible, on the grounds that "there
is no signal" for whether a holder is converging. That is too strong: a signal *does* exist during
streaming decode (§1.1) and is genuinely absent only during tool execution, so the right answer is
per-phase rather than a single global verdict — worked out in §5.2.

**The trilemma — and none of the three is free:**

| Option | Buys | Costs |
|---|---|---|
| Full span, unbounded hold | staleness free (R6) | mofa#1022 by construction (R4) |
| Full span, per-phase watchdog + hold ceiling | staleness free, R4 detectable during streaming decode | discards work on overrun; tool phases still need blind deadlines |
| Write-only span + freshness check | no long holds, no ceiling needed | two mechanisms, and R6 fails without the second one |

This does not settle §5.2, but it does move it: **the hold-ceiling value and the per-phase timeout
budgets are v1 design parameters, not implementation details.**

### 5.2 What the signal actually is, per phase

"Is this agent alive?" is sometimes unanswerable (§1.1). **"Has this phase exceeded its budget?" is
always answerable.** So the enforceable mechanism asks the second question, not the first: a
per-phase watchdog, sized to each phase's genuinely different observability (§2):

| Phase | Signal available | Enforceable check |
|---|---|---|
| Decode, streaming | one heartbeat per token/chunk | **inter-token timeout** — fine-grained |
| Decode, non-streaming | none | whole-call timeout — coarse |
| Tool execution | none | per-tool deadline — coarse |

This decomposition is not invented for this project — it is precisely the fix proposed in
[pi#5778](reference/field-reports.md): `streamTimeoutMs` (chunk wait), `toolTimeoutMs` (overall),
per-tool `timeoutMs` override. Three knobs because three phases, arrived at there from the bug
itself rather than from theory, and reused here for the same reason.

Two caveats to keep honest:

- A token heartbeat proves the **model is emitting**, not that the agent is converging. A model can
  loop, repeat, or emit filler indefinitely and look perfectly healthy. So this separates *hung*
  from *alive*, which is what R4 needs, and says nothing about whether the work will ever finish.
  The hold ceiling is still required as the outer bound.
- Non-streaming decode does not lose the *mechanism*, only the *resolution* — a coarse whole-call
  timeout instead of a per-chunk one. Worth noting because it means serving mode changes how tightly
  the ceiling can be set, not whether R4 is addressable at all.

**Where this leaves the span decision:** the reason to avoid holding across thinking was R4's
unkillable holder. If inter-token timeouts make decode-phase hangs detectable, holding across decode
is less dangerous than §5's three-step argument alone suggests, and full-span keeps R6 for free. The
residual risk concentrates in tool-execution phases, where no signal exists and only blind deadlines
apply. **This tilts the decision toward full-span without settling it** — and it makes serving mode
(streaming or not) an input to that decision, which handover §5.2 does not currently mention.

### 5.3 Revocation via fencing token — R4's constructive answer

R4 demands a preemptor that succeeds without acquiring the protected lock. The mechanism already
exists in handover §5 and needs no new machinery:

Revocation is a write to the **lock service**, not to the protected resource. Bump the fencing
token (or delete the lock key so the next acquirer gets a higher one). The stuck holder's eventual
write is then rejected by the write-time token check of §5 — the same check R3 exercises. The
preemptor never touches the resource lock, so **it cannot block on it.**

This closes mofa#1022 by construction rather than by avoidance, which is the distinction worth
drawing in any writeup: the thread's own proposals (`try_write()` for fail-fast) only make the
*stop* call fail quickly — the stuck agent still holds the lock and still cannot be stopped.
Fencing revocation actually takes the lock away.

**Why polite revocation cannot work here, stated as the composition of two facts.** Cooperative
revocation requires the holder to *act* — its callback fires and its own code decides to release.
*Wedged* (§0) means the holder will never execute another line of its own code. So the notification
lands in a mailbox nobody will ever open. The two together are fatal, and no amount of asking
improves it: an agent that is stuck will not comply however politely it is asked.

**When revocation is safe, and when it is not.** Under full span the lock covers read → think →
write:

- **Revoking during *think* is entirely safe.** Nothing has been written, so nothing is half-done;
  the holder's eventual write is simply refused. This is where R4's hang lives, so the case we
  actually care about falls wholly inside the safe window.
- **Revoking during the *write* is the hazardous case** — and see §5.4b, because it is not the hazard
  it first appears to be.

Think is minutes, a write is milliseconds, so the hazardous window is smaller by orders of magnitude.
Not zero, which is what §5.4b addresses.

**One limit worth naming:** revocation guarantees the holder's *write* cannot land. It does not undo
side effects outside the resource — a tool that sent an email stays sent. Out of scope for v1's
single file, but it is a real boundary of what "taking the lock back" means.

**Not reinventing an existing recipe — checking that first.** Apache Curator, the mature ZooKeeper
recipe library, does offer lock revocation, but its docs are explicit that "Revocation is
**cooperative**" — a listener is notified that someone wants the lock, and the holder must
voluntarily comply. A hung holder never complies, which is mofa#1022 restated in the API semantics of
the best-in-class library. **The standard tool stops at "ask nicely."** Fencing-token revocation is
the unilateral version and is what R4 actually requires. See
[`reference/lock-primitives.md`](reference/lock-primitives.md).

Two honest limits:

- Revocation guarantees the stuck agent's **write can never land**. It does not reclaim the
  inference work already in flight — that is the handover §7.7 / Continuum question, and it stays
  out of v1 scope.
- This works *only* because the harness is the sole gatekeeper (handover §7.4). It is the same
  single assumption the zombie-writer fix rests on, now carrying a second guarantee. Worth noting
  that handover §7.4 is load-bearing in more places than that doc currently admits.

---

### 5.4 Why recoverability is not a v1 property

Recorded because it is the kind of thing that looks like an omission later.

Classical concurrency control distinguishes *serializable* schedules from *recoverable* ones. A
schedule is **cascadeless** iff transactions read only committed data; without that, one agent can
read a write that is later retracted and end up built on an aborted basis (a **dirty read**).

**This design gets cascadelessness for free, and needs no mechanism for it.** Dirty reads require
state that is visible and can still be taken back. Here a write is either refused at the write-time
fencing check or it lands — refusal happens *before* visibility, so a rejected write was never
readable by anyone. Revocation (§5.3) discards a *pending* write, not a landed one. **The harness
refuses writes; it never undoes them.** So there is no uncommitted-but-readable state, no dirty read
is constructible, and the property holds vacuously.

It would stop being free the moment writes became speculative — write now, undo later if wrong,
which is MTPO's saga-style rollback. That is the design this project explicitly does not adopt
(handover §6), so the exposure never arises. **Corollary: "no speculative writes" is load-bearing,
not merely a simplification.** If v1 ever adds optimistic execution, recoverability stops being
vacuous and R5 comes back as a real race.

Deliberately **not** in v1: any notion of verified-vs-unverified state, test/CI gating, or two-tier
visibility for dependents. Handover §7.1 floats that as a layered option and it stays an option.
The v1 mechanism is a lock and nothing else — one property, mutual exclusion on writes, enforced
structurally. Whether a write is *good* is not a concurrency property and v1 does not ask.

**Cost of that choice, stated honestly:** handover §8's headline demo was the Agent Teams contrast
— self-reported "done" unlocking a dependent, an injected bug forcing rework. Without a
verification layer that contrast cannot be shown, because the lock genuinely does not prevent it.
**R4 is the better headline anyway:** one stuck agent freezes every other writer indefinitely, and
the harness takes the lock back without ever waiting on it. Real (the mofa#1022 family, with the
caveat in R4's note that they locked the agent while we lock the file), and *purely* a locking
property — no semantics, no verification, nothing to assume about the model.

### 5.4b Guaranteed give-back, and the one place atomicity is load-bearing

**Automatic cleanup covers code paths, not dead processes.** Wrapping take-save-give-back so the
give-back always runs (try/except/finally, i.e. handover §5's decorator/context-manager) closes R8
and every "the code took a path I forgot about" case. It does **not** cover `kill -9`, the OOM
killer, a segfault, or power loss — `finally` needs a live process to run it, and there is none.
That is the whole reason the expiring lease exists: cleanup handles unexpected code paths, the lease
handles the absence of code. Two mechanisms, two different failures, neither substituting for the
other.

On R9 specifically, automatic cleanup is **necessary but not sufficient**: if the retry sits inside
the protected section, the give-back has not run yet, so the lock is still taken twice. Cleanup
guarantees each take is eventually matched; it does not prevent the double take. What prevents it is
treating take-save-give-back as **one indivisible unit**, so that "retry" can only mean retrying the
whole unit — the double take becomes inexpressible rather than merely discouraged.

**Write-then-rename: what it buys and what it does not — OPEN, needs verifying not assuming.**
The proposal is to never write the resource in place: build the new content in a temporary file, then
`rename()` it over the target. It buys two genuinely distinct benefits, worth keeping separate rather
than treating as one:

1. **Crash safety (solid).** `rename()` is atomic with respect to readers — a reader sees the old file
   or the new one, never a mixture. So a harness that dies mid-write leaves no partial file. This one
   is genuinely settled, with the standard caveats: same filesystem only (a cross-filesystem rename is
   a copy, not atomic), and durability needs `fsync` of the temp file and then of the directory —
   atomicity is not durability.
2. **Shrinking the check-to-commit window (partial).** The intuitive failure mode — "revocation
   interrupts our write mid-flight" — is not actually what happens: the harness is still running and
   nothing stops it mid-syscall. The real hazard is subtler: the write **completes successfully even
   though the lock was revoked partway through**, producing a landed write with a stale ticket — a P2
   violation, not a corrupt file.
   Write-then-rename helps because it splits the work into *prepare* (no effect on the resource) and
   *commit* (one instantaneous `rename`). Check the ticket immediately before the rename and the gap
   shrinks from "however long it takes to write the whole file" to "one syscall."

**But shrinking is not closing, and this should not be recorded as solved.** A single-threaded harness
plus sole-gatekeeper makes the remaining one-syscall gap unreachable in practice — nothing else can
interleave — but that is an argument from *our* design, not a property the filesystem gives us. To
close it properly the filesystem would have to offer "rename only if ticket == N", and it does not.

**A third option closes it while keeping the file on disk** — put the resource's *version number* (not
its content) in the lock service, make the commit decision one transaction ("if my ticket is current,
bump the version"), and rename only after it succeeds. Checking and deciding become the same atomic
operation, so the gap disappears. It requires defining P2 at the moment of *authorization* rather than
physical application, which is how databases define a commit point rather than a weakening. Full detail
and costs in [`lock-interface.md`](lock-interface.md) §4.2b.

**Open question to settle, not to assume:** is the one-syscall gap acceptable given single-threaded
serialisation, or should the commit decision move into the lock service per §4.2b there? This is an input to the parked resource-storage
decision, not an independent question.

**Where atomicity is actually load-bearing.** "Check the ticket number, then write" contains a gap
between the check and the write, and that gap is a classic check-then-act race — the token can stop
being current inside it. The requirement is therefore that **checking and writing are a single
operation**: a conditional write that the storage layer itself refuses unless the token is current
(compare-and-set), not two steps performed in order. This is the only place in the design where true
atomicity is required; everywhere else "guaranteed cleanup" is enough. Worth stating explicitly
because handover §5 currently says the harness "re-checks the token at the moment it performs the
write," which reads as two steps.

### 5.4c If the harness itself goes down

Everything above assumes the harness is running. It will not always be.

**Fail-closed is correct and free.** With no path to the resource except the harness (handover §7.4),
a dead harness means nothing can write at all. No writes, no inconsistency: availability is lost,
correctness is kept — exactly §4's stated priority ordering, so this needs no mechanism.

**One v1 rule follows, and it is not optional.** If a restarted harness issues ticket numbers from
its own memory, the numbers reset, and a holder from before the crash can look current again — the
write-time check silently stops working, taking R3, R4 and R11 down with it. Therefore: **ticket
numbers must be generated by the lock service, never from harness-local state** — etcd's revision
number or ZooKeeper's zxid, which survive harness restarts and are monotonic by construction. Cheap
to honour, catastrophic to get wrong, and invisible unless the question is asked.

**The rest is v2.** A single harness process is a single point of failure — an availability problem,
not a correctness one. Replicating it turns "sole gatekeeper" into "sole *logical* gatekeeper" across
instances that must agree, making the harness itself a distributed system. That is a workstream, not
a patch, and it is deliberately deferred. See R12, which is specified now but not run in v1.

### 5.5 Time-varying latency: why no ceiling is the "right" one

The §5.2 watchdogs and the hold ceiling both assume a threshold can be chosen. Under a real remote
LLM call it cannot, and this is a theorem rather than a tuning gap.

**"Slow or dead?" is undecidable in an asynchronous system** — the unreliable-failure-detector
result (Chandra & Toueg; Dwork–Lynch–Stockmeyer on partial synchrony). Any constant is at once too
tight for some legitimate slow call and too loose for some genuinely stuck one. Do not look for a
better number.

**The move that makes this survivable: the deadline is not a safety mechanism.** Ask what a *wrong*
firing costs. The lock is revoked, the token bumped, and the slow agent's eventual write is rejected
at the write-time fencing check — rejected, not corrupted, not interleaved. **A wrong deadline costs
wasted work, never incorrectness.** Safety rests on the fencing comparison, an integer check with no
timing judgment in it; timing affects only liveness and throughput. This is exactly
[Kleppmann](handover.md)'s argument for fencing tokens (handover §6), and it is what licenses being
sloppy about the threshold. The
requirements reduce to: eventually fires, usually generous. R11 is the test of it.

**Where it still bites: heavy tails.** LLM latency is heavy-tailed — p99 many multiples of p50, plus
throttling and provider retries. Holding the lock across that call gives a dilemma with no interior
solution:

- A ceiling generous enough not to kill legitimate p99 work is *by definition* long enough that a
  genuinely stuck agent holds the lock for that same long time.
- A ceiling tight enough to catch stuck agents promptly revokes legitimate slow work constantly,
  discarding minutes each time — OCC's measured-bad failure mode.

**No estimator escapes this**, because it is a property of the distribution rather than of the
estimator. The heavier the tail, the wider the gap.

**Consequence: keep the time-varying component outside the critical section.** Under the write-only
span the remote call happens before the lock is taken, so hold time is a local write — tight, thin-
tailed, low variance — and a tight ceiling is genuinely safe because the thing being timed is
actually predictable. Under full span the lock inherits the full remote-latency distribution.

If holding across a remote call is unavoidable, do not use a constant: estimate from observed
latency and widen on every false revocation — TCP's RTO estimator (smoothed mean + deviation), or an
eventually-perfect failure detector that relaxes its timeout each time it is caught being wrong.
[Continuum](reference/papers.md) is precedent for treating the variable part as variable: its TTL is
derived from queueing delay, not fixed. Watch for **correlated revocation** — a load spike overruns
many holders at once and they all retry together, an abort storm (the `load_spike_at` knob, R11's
second half).

**Where this leaves §5.2 — no longer balanced.** Full span buys exactly one thing, R6 staleness for
free, and costs: four exposure windows inflated from milliseconds to minutes (§6.2), R10 reactivated,
R9 promoted from a harness-only bug to one the agent can trigger (the lock spans several turns), and
this unresolvable ceiling-vs-tail tension. Write-only span has exactly one gap, R6, closable by
a version/CAS check at write time — a comparison, so it inherits none of the latency variance.

The "two mechanisms instead of one" objection to write-only was fair, but the second mechanism turns
out to be the cheap, timing-free one, while full span's single mechanism has to absorb all the
variance. **Current lean: write-only span plus a write-time version check.** Not declared settled —
see §6.6.

---

## 6. The demo: a conformance table

The demo handover §8 asks for is **a conformance run, not a narrative**: no injected bugs, no story
about wrong code, no rework parable. Run every race against two implementations — the harness, and
an unprotected baseline — and print the table. The mechanism either gates correctly or it does not,
and the table says which.

### 6.1 The finding that should lead the demo

Count where the races come from. **R1, R6 and R10 are failures that exist without any lock at all.
R2, R3, R4, R7, R8 and R9 exist *only because a lock was introduced.***

Six of ten are self-inflicted. Adding a lock fixes one thing (concurrent writes) and creates six new
ways to fail: dead holders, stale holders, stuck holders, split-brain lock services, leaked locks,
and self-deadlock. That is the honest case for harness enforcement rather than an argument against
it — those six all have to be handled *somewhere*, and the harness is the only place a guarantee can
cover every agent at once. But the demo should say it plainly instead of implying the lock is free.

### 6.2 Hold time is the exposure window

Sharper statement of §5.2 than "trilemma": four of the six lock-induced races (R2 dead holder, R3
stale holder, R4 stuck holder, R7 contract violation) have an exposure window **proportional to how long
the lock is held.** R8 and R9 are code-correctness bugs in the harness wrapper, present or absent
regardless of duration.

So the span choice is not a preference between two tastes:

- **Write-only span** — hold time is a write, milliseconds. All four windows shrink to that. R4 in
  particular becomes *nearly vacuous*: an agent that hangs while thinking is holding nothing, so
  there is no lock to revoke and no preemptor to unblock. What remains is the narrow case of a hang
  *inside the write itself*, which needs only a write-phase deadline.
- **Full span** — hold time is the whole transaction, minutes. All four windows inflate to minutes,
  R4 becomes the central risk requiring the whole §5.2/§5.3 apparatus, and R10 stops being vacuous
  because a single held span can now issue several writes that need ordering.

**R6-for-free is therefore bought by widening four other windows and reactivating a fifth race.**
That is the actual trade, and it is less symmetric than §5.2 currently reads.

### 6.3 Expected results

`pass` = the harness prevents it. `n/a` = the failure cannot arise in that configuration.

| Race | No lock | Write-only span | Full span |
|---|---|---|---|
| R1 two writers collide | **fail** | pass | pass |
| R2 holder dies | n/a | pass — lease, ms window | pass — lease, minutes window |
| R3 zombie write | n/a | pass — write-time fencing | pass — write-time fencing |
| R4 stuck but alive | n/a | near-vacuous; needs write-phase deadline | pass *only with* per-phase watchdog + hold ceiling + §5.3 revocation |
| R5 self-report | fail | out of scope (§5.4) | out of scope (§5.4) |
| R6 stale content | **fail** | **pass** — the version check was implemented, so this is no longer the gap | pass, free |
| R7 lock service breaks contract | n/a | pass — monotonic tickets | pass — monotonic tickets |
| R8 abandoned acquire | n/a | pass — harness-owned release | pass |
| R9 retry leak / self-deadlock | n/a | pass — start over on failure; agent cannot cause it | pass — the API offers no way to express a nested take |
| R10 out-of-order effects | fail | n/a — one write per span | live concern |
| R11 false revocation (heavy tail) | n/a | pass — tight ceiling on a thin-tailed local write | pass, but ceiling cannot be both safe and prompt (§5.5) |
| R12 harness death + restart | n/a | **deferred to v2** — v1 ships the token-source rule only | **deferred to v2** |
| R13 crash between prepare and commit | **fail** (partial file) | pass — temp file + rename | pass |
| R14 revoked between check and commit | n/a | **pass** — `apply` guards on the version and discards a superseded write | **pass**, same guard |

### 6.4 What the table admits

This section compares what §6.3 predicted against what the v1 implementation actually does. Three of
the predicted failures did not survive contact with the code, in different ways:

- **R6 now passes on the write-only span.** The predicted failure was conditional on *not* building
  the freshness check; the implementation builds it (the content version lives in the lock service,
  §7 D5 there), so the write-only path is no longer gapped. This closes the write-only side of the
  §5.2 question in practice rather than by argument.
- **R9's full-span variant cannot occur.** The prediction was that a multi-turn lock makes a nested
  take agent-reachable. In the implementation the only mediated write is a whole take-write-give-back
  unit, so a nested take is not expressible at all — made impossible by construction rather than
  handled at runtime, which was the intent.
- **R14 passes, and its mechanism was mispredicted.** The doc expected "a write lands with a stale
  ticket." What actually happens is subtler: revocation does not interrupt the write, so the hazard
  is that *another holder completes an entire cycle inside the gap* and then the first holder's older
  content lands on top of theirs while the lock service records theirs — file and service
  disagreeing. The fix is a version guard in the apply phase: if the current version is no longer the
  one our authorization produced, discard rather than apply. A residual one-syscall gap between that
  guard and the rename remains, and is still recorded as open.

Two entries are still not passes, and the demo prints them as-is rather than quietly omitting them:

1. **R12 harness death and restart is deferred**, not passing. v1 ships only the rule that makes a
   restart safe — tickets come from the lock service, never harness memory — and does not exercise a
   restart, which needs the process boundary.
2. **R4 under full span** passes only conditionally, and one condition (tool-phase hangs) has no
   signal — only a blind deadline. It is not a clean pass and should not be printed as one.

Everything else is a clean pass, which is the useful result: **8 clean passes, 1 declared out of
scope, 1 genuine gap whose resolution is the §5.2 decision** (plus R4/R11 conditional under full
span).

### 6.5 Span is a configuration flag, not a fork

§5.2 is not settled on paper and does not need to be. The two spans differ only in *when* the harness takes
and gives back the lock relative to the agent's thinking; everything else — single
mediated write action, DLM lease, write-time fencing check, per-phase watchdogs — is common. So
**build span as a config flag and run the conformance table under both**, which is what §6.3's two
columns already are.

Two implementation notes, so "just a flag" is not overclaimed: full span additionally needs lease
renewal across the held decide phase (§5), and write-only additionally needs the version/CAS check
to close R6. Each variant carries one extra piece, and the table is what decides whether either
extra piece earns its keep. §5.5's lean toward write-only is a prediction the table can falsify.

### 6.6 Why an unprotected baseline column earns its place

Not for drama. Half the table is `n/a` for the baseline, and that is the point: it shows the lock's
value is narrow and specific (R1, and R6/R10 partially) while its cost is six new failure modes.
A demo that only showed the harness column would look like the lock is free.

---

## 7. Out of scope for v1

Unchanged from handover §8: multi-lock/deadlock policy (handover §7.3 — `n_resources` exists as a
knob, the race is deferred), static verification of lock discipline in generated code (handover §5.1
and §7.5), throughput optimization (handover §7.6). Add to that list: [A2
Phantom-Tool](reference/papers.md), which needs a mutable tool registry that v1's single-file
resource does not have; R10, whose knob ships without its race; and
**R5 plus everything in §5.4** — verification gating, two-tier visibility, recoverability
machinery. v1 is a lock and nothing else. Also **R12 / harness failover** (§5.4c): v1 ships the
token-source rule that makes a restart safe, but does not test restart itself.
