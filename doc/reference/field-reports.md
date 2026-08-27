# Field reports — lock-held-across-inference in shipping agent frameworks

Real filed bugs, not paper claims. All three are the same underlying failure: **a lock or serial
lane is held across an LLM/tool call, and the control-plane operation that would rescue the
situation needs the very thing that is stuck.** Three independent frameworks, three different
languages/runtimes, within four months of each other — this is a recurring architectural mistake,
not a one-off, which is what makes it worth building into the test harness as a first-class race
(see `doc/agent-model.md` R4) rather than an edge case.

None of these are distributed-lock bugs. They are all *in-process* locks or queues. That matters:
handover §5's DLM lease backstop is aimed at process death, and **none of these processes died.**

---

## mofa-org/mofa#1022 — the canonical instance

*"[Bug] Exclusive Write Lock Held Across Long-Running Agent Execution Causes Control Plane
Deadlocks"* — opened 2026-03-07, **still open**.

The `chat()` endpoint in `mofa-gateway/src/handlers/chat.rs` takes an exclusive Tokio `RwLock`
write guard on the agent and holds it across the entire execution:

> `let mut agent = agent_arc.write().await; agent.execute(input, &ctx).await`

The guard stays live across the `.await`, so it is held for the whole of `execute()` — LLM API
calls and tool execution, seconds to minutes. Both `stop_agent` and `delete_agent` need the
identical write lock:

> `let mut agent = agent_arc.write().await; // ... agent.shutdown().await`

So `stop_agent` blocks indefinitely on an agent that is actively running. Consequences as filed:
control-plane deadlock when stopping an executing agent; **inability to terminate runaway agents,
which is the entire purpose of the endpoint**; resource exhaustion as requests queue on contention.

Proposed fixes in-thread: `try_write()` for fail-fast in the short term; longer term, refactor
`execute()` to take `&self` with interior mutability.

> Note both proposals are *avoidance*, not preemption. `try_write()` makes the stop call fail fast
> instead of hanging — the stuck agent still holds the lock and still cannot be stopped. That is
> the gap this project's fencing-token revocation actually closes; see `doc/agent-model.md`.

The sharpest line in the whole reference set: **the lock outlives the thing it was meant to
protect.** Quote it as a description of *mofa*, not of this project — see below.

**Why they locked the agent at all**, since it is a fair question: an agent holds mutable state
(conversation history, memory, tool state) and two concurrent `chat` requests would scramble it, so
one-request-at-a-time per agent is a legitimate goal. In Rust it was not even a choice — `execute()`
takes `&mut self`, so the compiler *requires* exclusive access. **The bug is the duration, not the
existence of the lock.**

**What transfers to this project, and what does not.** mofa locks *the agent object*; this design
locks *the file*. There, the stuck thing and the locked thing are the same, which is precisely why
`stop_agent` deadlocks. Here they are separate, so killing a stuck agent needs no file lock and the
literal "operator cannot kill it" **does not transfer**. What transfers is the duration lesson and one
design rule: *no administrative operation on the resource may require acquiring the resource lock.*
See [`../agent-model.md`](../agent-model.md) R4.

---

## openclaw/openclaw#18470 — same shape, different framework

*"[Bug]: Gateway Deadlock: Internal Commands Hang When Called During Active Agent Turn"* —
2026-02-16, closed as not planned.

`openclaw sessions --json`, `cron list`, and `cron(action: "list")` all hang when invoked during
an active agent turn. The reporter is explicit that the mechanism is *suspected, not confirmed* —
worth preserving, since honest uncertainty is more useful than a confident wrong cause:

> "Gateway cannot respond - possibly because: It's waiting for the agent turn to complete before
> processing new requests" / "A lock/mutex on session state is held during LLM turns" / "Request
> queue is blocked by the active session"

Diagnostic detail worth reusing as a test assertion:

> "Only happens during active agent turns - Standalone bash scripts calling `openclaw` commands
> work fine"

Hangs resolve at a consistent 10-minute timeout matching the embedded run timeout — i.e. the
internal commands queue behind the active LLM request with **no priority handling**. That is the
same missing property as mofa#1022 stated in scheduling terms rather than lock terms: control-plane
work has no path that outranks data-plane work.

"Closed as not planned" is itself informative for the positioning argument — this class of bug
gets triaged away as an architectural nuisance rather than a correctness defect.

---

## earendil-works/pi#5778 — unbounded hold, process alive

*"Bug: pi-agent-core hangs indefinitely on unresponsive streams or tool execution deadlocks"* —
2026-06-15, closed. Affects 0.74.2.

Two unbounded waits in the agent loop: `for await (const event of response)` has no timeout if the
provider drops the connection without closing the iterator, and `await executeToolCalls()` has no
deadline, so a hung promise blocks forever.

> "If a user's tool hangs, the agent never yields back to the UI and silently dies in the
> background."

Fix as proposed: configurable `streamTimeoutMs` / `toolTimeoutMs` / per-tool `timeoutMs`, enforced
with `Promise.race()` throwing `TimeoutError`.

**Why this one matters even though it is not itself a locking bug.** It is the mechanism that
generates the *duration* the other two bugs are victims of, and it establishes that an unbounded
hold arises from ordinary causes — a dropped stream, an unresolved promise — not from an agent
misbehaving or a process dying. "Silently dies in the background" is precisely the state a
liveness-based lease cannot detect: the process is up, its heartbeat is fine, it is simply never
going to make progress. Any lease renewal in this project must therefore be **progress-based, not
liveness-based**; see `doc/agent-model.md` R4 and the note added to handover §5.
