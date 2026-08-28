# mas-harness-lock

Harness-enforced locking for multi-agent coordination — a correctness-first baseline that prevents
unsafe concurrent writes by construction, not by prompting agents to behave.

A coordination guarantee that must hold for *every* agent at once cannot live in a prompt, a role
label, or a convention: it only takes one agent, once, not respecting it to break the guarantee for
everyone downstream. So it lives in the one place every agent is structurally forced to go through —
the harness. Agents are never handed a lock, and never see a ticket number, so there is nothing about
lock discipline for them to get wrong and nothing for them to forge.

**Status: v1 prototype, implemented and green.** 12 named races pass, plus 6000 randomized runs.
Agents are scripted stand-ins, not real LLM calls — v1 validates the *mechanism*, whose correctness
depends only on the pattern and timing of acquire/read/write/release, never on what an agent decides
to write.

## Quickstart

The Rust toolchain lives at `~/.cargo/bin` but is **not on `PATH`** (installed with
`--no-modify-path`):

```sh
export PATH="$HOME/.cargo/bin:$PATH"

cargo test                    # 12 named races + 300 randomized soak runs. ~1s, hermetic
cargo run --example demo      # prints histories and property reports
```

```sh
cargo test --test conformance             # just the named races
cargo test --test soak                    # just the randomized soak
cargo test --test conformance r4_wedged   # one race, by name substring
```

## What it does

One shared resource — a file on disk. One harness that is the sole gatekeeper to it. Agents get
exactly two mediated actions, `read` and `write`; no raw lock primitives are reachable from agent
code at all.

Underneath: a distributed lock service (etcd or ZooKeeper — deliberately not Redis, per Kleppmann's
clock-skew critique) holds the lock, a monotonic **ticket** saying whose turn it is, and a **version**
saying which revision of the content this is. The ticket is re-checked at the moment of committing,
not once at acquire time, so a holder whose lease lapsed is refused however firmly it believes
otherwise.

Three things that are easy to assume and are not true here:

- **A lease detects death, not wedging.** A holder that is alive, healthy, still checking in, and
  will never run another line of its own code renews forever. This is a filed bug in three shipping
  agent frameworks, not a hypothetical. Taking the lock back is therefore *unilateral* — the holder
  is never asked and never notified.
- **Mutual exclusion is not enough.** An agent can hold the lock correctly at every moment and still
  write content decided from a version that no longer exists. That needs a second mechanism.
- **Timeouts are liveness mechanisms, never safety ones.** Slow and stuck are indistinguishable in an
  asynchronous system, so no threshold is correct — and none needs to be. A wrongly revoked holder is
  refused at commit, so a bad threshold costs wasted work, never correctness.

## Reading the code

Start with **[`doc/code-structure.md`](doc/code-structure.md)** — module map, the lifecycle of a
write, and where the tricky parts are.

The short version: the harness emits a machine-readable **history** of everything it does, and the
tests assert eight named properties over that history rather than over the file. Inspecting the file
alone cannot distinguish "correct by construction" from "correct by luck on this run".

## Design docs

The code says *what*; these say *why*, including which decisions are deliberately still open and
which "obvious" simplifications are already-published negative results.

| Doc | Contents |
|---|---|
| [`doc/handover.md`](doc/handover.md) | The authoritative spec. Premise, scope decisions, open questions (§7), safety boundary and trust ledger (§10), stack (§11) |
| [`doc/agent-model.md`](doc/agent-model.md) | The scripted "error-prone agent": glossary (§0), event alphabet, parameters, and named races R1–R14 |
| [`doc/test-design.md`](doc/test-design.md) | Three test tiers, the history-as-oracle requirement, the eight properties, and what the soak actually caught (§7b) |
| [`doc/lock-interface.md`](doc/lock-interface.md) | The abstraction the fake and both real backends satisfy, plus the decision log (§7) |
| [`doc/reference/`](doc/reference/) | Verified sources with verbatim quotes. The field reports are worth more than the papers for test design — they are failures that actually happened, with the offending code named |

## Not done

- **Tier 3** — the same race scripts against live etcd and ZooKeeper, to check the fake did not lie.
- **R12** — harness death and restart. v1 ships the rule that makes a restart safe (ticket numbers
  come from the lock service, never from harness memory) but does not exercise a restart, which
  needs a process boundary.
- **Multi-resource** — deadlock policy, lock granularity, and a canonicalization rule for resource
  names are all out of v1 scope. Several simplifications here are load-bearing only while there is
  exactly one resource.
- **Real LLM agents** — a separate question about model *behaviour*, deliberately not mixed into a
  test of the mechanism.

Known gaps in the threat model, named rather than solved: the harness is assumed never to fail, and
v1 defends against faults, not adversaries. Starvation is unaddressed. See handover §10.
