# The lock-service interface

**Status:** design only. §7 is the decision log; **all decisions now resolved or set as revisable
defaults.** Ready to implement against.
**Depends on:** [`test-design.md`](test-design.md), [`agent-model.md`](agent-model.md) for races and
glossary, [`handover.md`](handover.md) §11 for the stack.
**Why it comes first:** it is the one abstraction the fake, etcd, and ZooKeeper must all satisfy, and
both the precise property definitions and the test-case data format depend on it.

---

## 1. Two things the interface exists to hide

1. **The ticket number.** etcd's is a key's `mod_revision`; ZooKeeper's is a created znode's `czxid`.
   Both are cluster-global and monotonically increasing, which is what the §7.4b rule needs.
2. **How the service decides a holder is gone.** These differ structurally, not cosmetically — see
   §4.1. This is the hard part of the interface, not the ticket.

---

## 2. Operations the races require

Derived from R1–R11; nothing here is speculative surface.

| Operation | Why it is needed | Races |
|---|---|---|
| `acquire(resource) -> (Holding, Ticket)` | Basic mutual exclusion | R1 |
| `release(Holding)` | Normal completion | R1, R8 |
| `current_ticket(resource) -> Ticket` | Checked at write time, not acquire time | R3, R7, R11 |
| `keepalive(Holding)` | Keep a long hold from expiring | R4, full span |
| `force_release(resource) -> Ticket` | Take the lock from a holder that has not given it back — must **not** require holding it. Named *not* `revoke`: see §7 D2 | R4 |
| `is_still_held(Holding) -> bool` | A holder learning it lost the lock; advisory only, never the safety check | R3, R11 |

Two properties of this list matter more than its contents:

- **`revoke` must be implementable without acquiring.** Both backends can delete another client's
  lock node (etcd: revoke the lease or delete the key; ZooKeeper: delete the znode), so the next
  acquirer necessarily gets a higher ticket. This is what makes R4's preemptor non-blocking, and it
  is the unilateral counterpart to Curator's *cooperative* revocation
  ([`reference/lock-primitives.md`](reference/lock-primitives.md)).
- **`is_still_held` is advisory and must be documented as such.** Any code that treats it as
  permission to write has reintroduced the check-then-act race that write-time ticket checking
  exists to eliminate.

---

## 3. Backend mapping

| Concept | etcd | ZooKeeper | Fake |
|---|---|---|---|
| Lock held | a key with a lease attached | ephemeral sequential znode, lowest sequence wins | entry in a map |
| Ticket number | `mod_revision` of the lock key | `czxid` of the created znode | monotonic counter, cluster-global |
| Liveness | lease TTL + `KeepAlive` | session timeout; ephemeral znode dies with the session | virtual-time deadline |
| Conditional update | `Txn` with `Compare(mod_revision)` → `Then`/`Else` | `setData` with expected `version` | direct check |
| Revoke by third party | revoke the lease, or delete the key | delete the znode | drop the map entry, bump counter |
| Who owns liveness | **per lock** (each lease is separate) | **per client session** (see §4.1) | configurable |

---

## 4. Two real mismatches the interface must confront

### 4.1 Lease-per-lock versus session-per-client

etcd attaches a lease to each lock, so one lock's lease can expire while the same client's other
locks survive. ZooKeeper ties liveness to the **client session**: when a session expires, *every*
ephemeral znode that session created disappears at once.

This is not an implementation detail to paper over. It changes observable behaviour in R2 and R4: in
etcd a single stuck hold can be expired in isolation; in ZooKeeper expiring it via session timeout
takes all that client's locks with it. With one resource in v1 the difference is invisible, and it
becomes load-bearing the moment `n_resources > 1` (handover §7.3).

The interface therefore cannot expose "expire this lock's lease" as a portable primitive. What is
portable is "this holder no longer holds the lock," reached by deletion in both backends. **Decision
D7 in §7 determines how far this needs to be modelled in v1.**

### 4.2 The resource is a file, so the backends' atomic write does not reach it

Both backends offer genuine compare-and-swap — but only over data *stored in them*. etcd's `Lock`
returns a key usable in a `Txn` so that updates to **etcd** happen only while holding the lock; the
resource here is a file on disk, and no etcd transaction can guard a filesystem write.

This is already recorded (handover §7.4b, §11): atomicity of check-then-write comes from the harness
serialising every write, not from the storage layer. Restated here because the interface is where
someone would reasonably expect a `conditional_write` operation, and **there deliberately is not
one.** The interface's job ends at the ticket; the harness owns the ordering.

Consequence for testing: any property asserting atomic check-and-write is testing *harness
serialisation*, so it must be checked against the history (`test-design.md` §3), which is the only
place that ordering is observable.

The partial mitigation — build content in a temp file, check the ticket, then `rename()` — reduces the
check-to-commit gap to one syscall and gives crash safety outright, but does not close the gap the way
a transaction would. See `agent-model.md` §5.4b, where it is recorded as open rather than solved.

### 4.2b Third option: file on disk, commit decision in the lock service

**Owner's preference (2026-08-27): the resource stays a file on disk.** This subsection records the
option that makes that work without conceding the atomicity gap — a third choice, better than either
of the two originally offered.

Note first that the lock was always "virtual" in this sense: the lock entry lives in the lock service
under a name the harness maps to the file path. Keeping content on disk never changed that.

**The move: put the resource's *version number* in the lock service too — the number, not the
content.** Committing a write then becomes a single transaction: *if my ticket is still current, bump
the version.* Only once that succeeds is the file renamed into place.

**Why this closes the §5.4b / R14 gap.** The gap was: ticket valid at check time, revoked, rename lands
anyway. Now checking and deciding are **the same operation**, executed atomically by the lock service —
a revoked holder's transaction fails outright. There is no window between the two steps because there
are no longer two steps.

**What it requires conceding, and why the concession is principled.** Property P2 must be defined at
the moment a write is *authorized*, not the moment bytes reach the disk. That is not a weakening
dressed up as a definition — it is how databases define a commit point. Serializability is stated over
commits, never over physical application; the rename becomes the application of an already-committed
decision.

**What it costs.** A harness death between authorizing and renaming leaves the lock service at version
N+1 while the file still holds version N's content. This **fails safe** — a later agent's freshness
check finds a mismatch and is refused, so no wrong write can land — but it can wedge: nobody can make
progress until something reconciles the two. The complete fix is replay-on-restart, which is
write-ahead logging. That is v2 work (handover §10, R12), and it does not arise in v1, where the
harness is assumed alive.

**Consequences for the parked decisions:** this is a candidate answer to D1 and D5 (the version is a
separate number, and it lives in the lock service) that preserves the file-on-disk choice, and it would
turn R14 from an expected failure into a pass. It does not resolve D1/D5 by itself — the owner still
picks — but it removes the need to trade one against the other.

### 4.3 Ticket numbers are the harness's, never the agent's

Stated explicitly because it is easy to misread, and because it determines what the fault injections in
§5 are actually simulating.

The agent never sees, holds, supplies, or can name a ticket number. The harness obtains it from the
lock service and checks it. **An agent therefore cannot lie about a ticket, because it never touches
one** — which is why forgery is absent from the threat model (handover §10.4) rather than merely
forbidden.

Normal flow: the harness acquires the lock on F and is told "yours, ticket 41"; the agent thinks; the
harness asks the service for F's current ticket immediately before committing, gets 41, and proceeds.
Zombie flow (R3): the harness holds 41, the holder stalls, the lease lapses, another holder acquires
and is issued 42; the first holder's write is then compared 41-against-42 and refused.

So the non-monotonic-ticket injection in §5 simulates **a bad ticket source, not a dishonest agent** —
see §5.1 for the two real ways that happens.

---

## 5. Fault-injection surface the fake must expose

Derived from the races; this is what tier 1 needs and what real backends cannot provide on demand
(`test-design.md` §2).

| Injection | Races |
|---|---|
| Expire a holder at an exact virtual time | R2, R3, R11 |
| Silently discard a holder's `keepalive` while it stays otherwise live | R4 |
| Grant the same lock to two holders simultaneously | R7 |
| Bump the ticket with no real handover | R4 revocation path |
| Delay or drop a `release` | R8, R9 |
| Emit a **non-monotonic** ticket | negative test for the §7.4b rule — the checker must catch it (§5.1) |

That last one is the only injection whose purpose is to make a test *fail*: it verifies the history
checker actually detects a violated assumption rather than trusting it. Without it, P2 could pass
vacuously forever — and would pass identically if the checker were entirely broken.

### 5.1 Non-monotonic tickets: two real causes, not one hypothetical

Worth separating, because an earlier explanation ran them together.

**(a) The harness generates its own tickets and restarts.** Our bug, and precisely what the handover
§7.4b rule forbids. A counter in harness memory resets on restart, so a pre-restart holder can look
current again. The rule ("tickets come from the lock service") exists to make this impossible — note
that when the rule *is* followed, a harness restart has no effect on ticket numbers at all, because
the lock service did not restart.

**(b) The lock service's own numbers go backwards.** Assumed hypothetical in an earlier draft; it is
not. **etcd revisions can move backwards after restoring a cluster from a snapshot** — restore an older
backup and the revision resumes from that older point. etcd ships a revision-bump option on its restore
command specifically because watchers and caches break otherwise.

**Operational requirement that follows:** if this system's etcd is ever restored from a backup, it must
be restored **with revision bumping**. Otherwise a correct harness, following the §7.4b rule properly,
can still see a stale holder appear current. This is a deployment rule with no code to enforce it,
which puts it in the silent-failure class of handover §10.3.

---

## 6. Property definitions that follow

Precise forms for the properties in `test-design.md` §4 that this interface settles. The remainder
depend on open decisions and are marked.

- **P1 mutual exclusion.** For one resource, no two intervals `[write_attempted, write_accepted]`
  overlap in the history.
- **P2 ticket validity.** For every `write_accepted` at history position *i* with ticket *t*, the most
  recent ticket-changing event before *i* also yields *t*. Stated over history order, not wall-clock,
  so it is checkable under virtual time.
- **P3 / P4 no lost or phantom write.** The final content equals the value of the last
  `write_accepted`; every `write_refused` value appears nowhere in it.
- **P5 no leaked lock.** At end of history, for every `take_granted` there is a later `give_back`,
  `lease_expired`, or `revoked` for the same holder.
- **P6 freshness.** *Depends on D1 and D5* — needs the definition of "resource version" before it can
  be stated.
- **P7 liveness.** Every agent without `hang_at` reaches a terminal event within its configured
  ceiling, measured in virtual time.
- **P8 preemptibility.** Every `revoke` is followed by a ticket change within a bounded number of
  history events, whatever the target holder is doing.

---

## 7. Decision log

Four resolved 2026-08-27; three still open.

### Still open

Nothing blocking. Both remaining items were delegated with "pick something relevant, it's tweakable" —
recorded below as chosen defaults with rationale, not as settled truths.

### Resolved

**D5 — CHOSEN (delegated, revisable): the content version lives in the lock service.**

Owner had no preference and asked for a reasoned pick. Four reasons, in order of weight:

1. **It closes the check-to-commit gap** (§4.2b, R14). "If the version is still 5, set it to 6" becomes
   one indivisible operation, so a revoked holder's transaction fails outright rather than sneaking a
   rename through. This is the only one of the three options that closes it; the other two leave a gap
   we would have to argue our way around.
2. **Nothing on disk to keep in sync.** A sidecar file can drift from the content it describes; there is
   no second thing to drift here.
3. **The file stays byte-for-byte clean.** A `# version: 5` header would mean the protected resource is
   no longer real code — which matters for a project whose whole subject is agents editing real code.
4. **We do not have to maintain a counter at all.** A key's own `mod_revision` already increases on
   every write, so touching `version:/<path>` on each commit gives the content version for free. Less
   code than either alternative.

Refinement that falls out of (4): store the **content hash** as the key's value. The revision is the
version; the hash lets a reader check whether the file on disk actually matches what the lock service
believes. That makes §4.2b's crash window (authorized, version bumped, rename never happened)
*detectable* rather than silent — which by handover §10.3's ranking is the property worth having.

**Honest costs, recorded so a future reader does not think they were missed:**
- The version lives apart from the data it describes, so it does not travel if the file is copied
  somewhere outside the harness.
- Answering "what version is this file?" now needs a network round-trip; with a header or sidecar it
  would be a local read. Irrelevant under handover §4's correctness-over-throughput stance, but real.
- If the lock service's data is lost or restored from a backup, the version resets — the same failure
  class already recorded in §5.1, and the same mitigation applies.

**What would justify revisiting:** the version needing to travel with the file (handing the repo to
something outside the harness); reads becoming hot enough that the round-trip matters; or a move to many
files where a directory-level version is wanted instead of one key per file.

**D6b — DEFAULTS SET (revisable), not decisions.** Lease **15s**, renew every **5s**, hold ceiling
**10 minutes**. The first two are Kubernetes' leader-election numbers adopted wholesale, since they
solve exactly the same problem — is this process still alive across a network — and have been tuned in
production far more than we could justify doing ourselves. The ceiling has no prior art to borrow,
because no existing lock service holds locks for minutes; 10 minutes comes from openclaw#18470's
observed agent-run timeout, the only real agent-scale number in our reference set. All three are
configuration, and the soak tier is what should actually set them.

### Resolved

**D1 — RESOLVED: a separate content version, and the reason is span-dependent.** An earlier draft
offered "reuse the lock ticket" without saying why it was ever plausible. It is plausible **under full
span**: nobody else can write while you hold the lock, so a still-current ticket at write time proves
nothing changed — which is precisely why full span gets R6 for free. It breaks **under write-only
span**: you read without holding the lock, so you have no ticket from read time, and another holder can
write and release in between. Since span is a config flag, the separate content version exists for the
write-only path and the full-span path simply does not consult it. Not a tradeoff — a consequence of
the span choice.

**D6 — RESOLVED in structure; numbers are D6b.** One rule covers every case: **renewal never continues
past the hold ceiling, whatever drives it.**
- **Streaming decode:** renew on observed progress — each arriving token counts as "still here." A
  wedged decode stops producing tokens, so renewal stops by itself and the lock frees with no watchdog
  involved. This is the owner's own earlier observation, promoted to mechanism.
- **Non-streaming decode, and all tool execution:** no signal exists, so explicit renewal has nothing
  to hook onto during a single blocking call. A background timer is the only option — but a **bounded**
  one, which stops at the ceiling.
- Either way a wedged holder loses its lock at the ceiling. Slow still cannot be distinguished from
  stuck (§5.5 — that is impossible), but nothing is unbounded.

**Two clocks, not one — do not conflate them.** The lease is a *liveness* check on a process across a
network and belongs at seconds scale; the ceiling is a *policy* limit on how long work may take and
belongs at minutes scale, because inference is minutes. Prior practice for the first: Kubernetes leader
election uses lease 15s / renew deadline 10s / retry 2s, with the constraint lease > renew deadline >
retry × 1.2, and documents 5/3/1 as aggressive and 60/40/10 as relaxed; Chubby defaults to a 12s lease,
raising it toward 60s under master load, with a 45s grace period across failover. There is no prior art
for the second, because nobody previously held locks for minutes — the closest real anchor is
openclaw#18470's observed 10-minute agent run timeout.

**A — RESOLVED: keep both backends.** The original justification collapsed (etcd cannot split-brain, and
the liveness difference cannot surface at one resource — see D4, D7, and handover §11), so this is now
justified on weaker but real grounds: ZooKeeper is the session model Kleppmann endorses over Redlock and
therefore carries weight in a writeup, a second real backend independently checks the fake's
faithfulness, and the interface is already written so the marginal cost is integration only.

### Resolved

**D2 — RESOLVED: a named operation, called `force_release`.** Deliberately not `revoke`, because that
is the word Curator uses for the *cooperative* version and the confusion would be costly.
`force_release` says the lock is released on the holder's behalf without involving it. **Its doc comment
must state explicitly: this does not notify the holder; the holder discovers it only when its next write
is refused.** The deciding argument was not clarity but the history log — the `revoked` event that
property P8 is asserted over has to be emitted somewhere, and a single named function emits it once,
whereas bare deletion means remembering at every call site. Relying on people to remember is the exact
failure mode this project exists to eliminate.

**D3 — RESOLVED: a separate namespace per resource, created lazily.** An earlier draft claimed this
"needs a registry and cannot handle resources appearing at runtime" — **that was wrong.** Create the
namespace on the first lock request for a path and runtime appearance handles itself, no registry
needed. Two items remain, neither blocking v1 and both TODO rather than decided:
- **Canonicalization** — which path strings count as the same resource (`./f.py` vs `f.py`, symlinks,
  case). Required under *either* namespace shape, so it was never a differentiator, and it is where a
  silent mutual-exclusion break would live: two names mapping to two locks for one file.
- **Cleanup** of namespaces for deleted resources. Irrelevant at one file.

**D4 — RESOLVED, but the question was wrong.** Both, for different purposes — and the reason is a fact
about etcd that reframes R7 entirely.

**etcd cannot split-brain.** Under partition the majority side serves and the minority side rejects all
writes; Raft quorum means a minority partition cannot elect a leader. There is no configuration in which
a correct etcd hands the same lock to two holders. **So a faithful multi-node fake would not produce R7 —
it would prove R7 unreachable.** Modelling nodes does not help with the race it was proposed for.

Therefore:
- **"Grant twice" stays, and R7 must be relabelled.** It is not "what happens when the lock service
  partitions" but **"what happens if the lock service violates its contract"** — defence in depth
  against a scenario a correct backend does not produce. Deliberately breaking the contract is the only
  way to construct it, so the shortcut is not a shortcut; it is the mechanism.
- **Use `turmoil` for something more realistic instead:** partitions between *the harness and the lock
  service*. Keepalives fail to arrive and the lease expires while the harness still believes it holds
  the lock. That is R3/R11 territory, it happens routinely in real deployments, and it is what a
  simulated network is actually good for. `turmoil` (Tokio project — mocked time and network, seeded
  RNG, multiple hosts on one thread, plus *barriers* that inject hooks at chosen points, which is
  exactly what R14 needs) fits our runtime; `madsim` is the heavier alternative, replacing the runtime,
  used in production by RisingWave.

**D7 — RESOLVED by scope: one ZooKeeper session for the whole harness in v1.** The divergence is real —
etcd gives each lock its own timer, while ZooKeeper ties every lock to the *connection*, so one dropped
connection drops all of them at once. **But it cannot appear in v1.** One resource means one lock, so
the harness never holds more than one at a time; a dropped session kills exactly one lock, which is what
etcd would do too. The divergence requires multiple simultaneously-held locks, which is deferred with
handover §7.3. Revisit when `n_resources > 1`; not a punt, genuinely out of scope.
