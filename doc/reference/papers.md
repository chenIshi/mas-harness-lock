# Published prior art

## CoAgent / MTPO — arXiv:2606.15376

*Concurrency Control for Multi-Agent Systems.* Closest existing work; see handover §6 for
positioning. Two things to take from it verbatim rather than in paraphrase.

**The lock-duration claim** (§3.3, Performance Gap) — the direct source for the concern that
handover §5.2 revives:

> "Locks that held for milliseconds in databases now hold for minutes. While one agent infers for
> several minutes after reading a shared object, every other agent waiting on that object blocks."

Framing of why classical CC transfers badly:

> "A single agent transaction spans minutes of inference, read sets are broad and opaque rather
> than statically inferable, and the live state agents act on admits neither fork nor buffer, so
> writes take effect the moment they execute."

> "Locks block long inference intervals; OCC abort-and-retry discards minutes of work on every
> conflict."

**Measured baselines and enumerated failures:**

| Failure | Rate |
|---|---|
| Deadlock (2PL) | 0.81 / trial under contention |
| Abort (OCC) | 0.95 / trial under contention |
| A3 self-healing failure (agent misjudged a notification) | 5 / 100 trials |
| 2PL speedup over serial | 1.04× |
| OCC speedup over serial | 0.93× (slower), 1.83× token cost |

Also enumerated, without per-item rates: stale reads, write-write conflicts, antidependency
cycles (read-write edges closing loops in the precedence graph), and **silent anomalies** — the
"canary case" of §2.2, where agents each complete their task faithfully and the final state
matches no serial order. Neither deadlock nor abort, just a correctness violation nobody notices.
That case is the strongest argument in the paper for this project's thesis, not against it.

**MTPO**, for the record: fixed serialization order at launch, order-filtered reads, speculative
in-place writes, one-way notification asking an affected reader to re-judge and patch, and
saga-style mechanical undo/reorder via an inverse each tool registers in advance. Serializable at
quiescence, conditional on A1–A3 (handover §6 on why the A3 conditionality is the load-bearing
weakness).

---

## Verified Detection and Prevention of Concurrency Anomalies in Multi-Agent LLM Systems — arXiv:2606.17182

A **formal catalog of MAS concurrency anomalies** — the single most directly reusable source for
test design, because it is already an enumeration rather than a mechanism pitch. TLA+ for the
specifications, TLC for explicit counter-examples, TLAPS for consistency, and Verus for
detector-equivalence proofs and runtime safety theorems refined down to deployed Rust.

| ID | Name | Definition |
|---|---|---|
| A1 | **Stale-Generation** | Agent reads a cell, another agent writes it during generation, the first commits on the obsolete value. Detected as `read time < write time < commit time` with value mismatch. |
| A2 | **Phantom-Tool** | Agent reads the tool registry and plans around a tool that is removed before commit. Detected as present in read registry, absent from write registry. |
| A3 | **Causal-Cascade** | A committed operation depends on an *aborted* operation's writes — left grounded on a retracted basis. |
| A4 | **Split-View** | Under replication, two agents simultaneously observe divergent values of the same cell. Informal in the single-store model; proved as a Verus monotone-primary no-split theorem. |
| A6 | **Tool-Effect Reordering** | An operation issues writes in its intended order, the runtime externalizes them in a different one. Matters because tool effects are irreversible. |

**A5 is deliberately absent.** Per the paper: the numbering is non-contiguous because an earlier
formulation had an A5, *LongGeneration*, "which was subsumed by A1 in the current operational
model and dropped from the catalog."

> Worth flagging: that subsumption is exactly the move this project should **not** copy. A1 folds
> long generation into a *content-staleness* problem — was the value I read still valid? But the
> mofa#1022 family in [field-reports.md](field-reports.md) shows long generation also causes a
> pure *liveness* failure with no staleness component at all: a healthy agent holding a lock
> nobody can take back. Collapsing A5 into A1 loses that. This project needs A5 as a first-class
> failure mode, which is what `doc/agent-model.md` R4 restores.

Runtime levels, each strictly containing the last: **L1** prevents A1 (read-set stability across
generation), **L2** adds A3 prevention (causal tracking, aborting dependents of aborted ops),
**L3** adds A6 prevention (ordered externalization via saga compensation).

---

## ATM: CID-Brokered Pre-Write Admission for Multi-Agent Code Co-Synthesis — arXiv:2607.00041

Fills the gap handover §6 identified in CoAgent — that CoAgent is benchmarked on office-automation
and K8s-ops workloads (WorkBench, AIOpsLab), **not** collaborative source-code editing with
git/import/test semantics. This paper is that missing setting, so it should be read before
settling handover §7.2 (lock granularity).

Conflict cases it identifies for code specifically:

- **Signature changes** — one agent alters a function signature while others hold stale references.
- **Call-site updates** — agents editing call sites without coordinating the signature change,
  the multi-file semantic conflict §7.2 worries about.
- **Git/import semantics** — concurrent edits to shared dependencies or import statements.
- **Test semantics** — concurrent test modifications leaving inconsistent validation state.

**Granularity answer:** file-level lockable objects, with character-level identifiers (CID) for
conflict detection — coordination at the file, detection finer. Admission is requested *before*
writing rather than detecting conflicts post-hoc, which is structurally the same "gate the write"
stance as this project.

Positions itself as: plain locking is too coarse and serializes unnecessary work; CoAgent/MTPO
lacks coordinated write governance and lets conflicts propagate.

> Caution on citing this: its critique of locking is a *throughput* critique, which handover §4
> and §7.6 already accept as the price of the correctness floor. It is not evidence against the
> guarantee. Do not concede more than it actually establishes.

---

## Continuum: Multi-Turn LLM Agent Scheduling with KV Cache Time-to-Live — arXiv:2511.02230

Prior art for handover §7.7, which currently reads as an open follow-on workstream. It is less
open than the doc assumes — this is the same tradeoff, already mechanized and measured.

Pins a paused session's KV cache in GPU memory with a **TTL derived from reload cost and expected
queueing delay** — the middle option between §7.7's two poles rather than a choice between them.
The two costs, in the paper's terms: retention means "the pinned KV cache occupies GPU memory
unnecessarily, blocking other requests and reducing overall system throughput"; eviction means
recompute or reload from CPU/DRAM.

Two results that matter for §7.7:

- Even with CPU offloading, "evicted programs still accumulate substantial waiting time across
  turns — comparable to vanilla vLLM despite InferCept's reload savings." So §7.7's hope that
  prefix caching makes eviction cheap is only probabilistically true, as that section suspects.
- Up to **8.18× latency and throughput improvement on SWE-agent workloads** — i.e. the scheduling
  layer's headroom is large, on a coding workload, which is this project's setting.

Bearing on §5.2: §7.7 argues the full-span lock is affordable because v1 withholds the next LLM
call rather than parking a live generation, so blocking costs wall-clock but not GPU. Continuum
supports that being the right v1 default, and quantifies what the *other* choice would cost.
