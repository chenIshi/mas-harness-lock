# Reference index

Annotated sources for the harness-enforced locking design. Every entry here was fetched and
checked on 2026-08-27; quotes are verbatim from the cited source, not paraphrase.

## What is actually stored here

Notes and sources are separate on purpose — the notes are ours and belong in git, the sources are
not ours and mostly do not.

```
reference/
  README.md          this index
  papers.md          notes + verbatim quotes from the published work
  lock-primitives.md documented semantics of real lock implementations
  field-reports.md   notes + verbatim quotes from the filed bugs
  snapshots/*.json   full GitHub issue bodies + comment threads  [tracked in git]
  papers/            fetched PDFs and one HTML post              [gitignored]
```

**Why the split is that way round, rather than the obvious one.** arXiv papers are immutable and
version-pinned, so a URL is a permanent citation and a local PDF is only a convenience — those are
gitignored (see `/.gitignore`; ~4.2 MB, and not ours to redistribute). GitHub issues are the
fragile ones: bodies get edited, threads get deleted, and one of these three is already *closed as
not planned*. Those are snapshotted into git as full API JSON, bodies and comments both.

Re-fetch the gitignored PDFs any time from the URLs below:

```sh
mkdir -p doc/reference/papers && cd doc/reference/papers
for p in 2606.15376 2606.17182 2607.00041 2511.02230v5; do curl -sSL "https://arxiv.org/pdf/$p" -o "$p.pdf"; done
curl -sSL https://martin.kleppmann.com/2016/02/08/how-to-do-distributed-locking.html -o kleppmann-how-to-do-distributed-locking.html
```

Snapshot metadata, as fetched 2026-08-27 — matches what `field-reports.md` cites:

| Issue | State | Opened | Comments |
|---|---|---|---|
| mofa-org/mofa#1022 | open | 2026-03-07 | 3 |
| openclaw/openclaw#18470 | closed (not planned) | 2026-02-16 | 7 |
| earendil-works/pi#5778 | closed | 2026-06-15 | 8 |

## The sources

Split into two files by kind, because they carry different evidential weight:

- **[papers.md](papers.md)** — published prior art. Establishes what is already known, already
  measured, and already claimed, so this project doesn't re-derive or re-announce it.
- **[lock-primitives.md](lock-primitives.md)** — documented semantics of real lock implementations
  (Curator, reentrant mutexes). Settles whether R9 and §5.3 are real concerns; both are.
- **[field-reports.md](field-reports.md)** — real bug reports in shipping agent frameworks. These
  are worth more than the papers for test design: they are the failure modes that actually happen,
  filed by people who hit them, with the offending code named.

Consumed by [../agent-model.md](../agent-model.md), which turns these into the parameter set and
the named races for the scripted-agent test harness (handover §8).

| Source | Kind | What it settles |
|---|---|---|
| CoAgent / MTPO (arXiv:2606.15376) | paper | Locks held across inference; 2PL/OCC already measured bad |
| Concurrency anomalies (arXiv:2606.17182) | paper | Formal catalog A1–A6 of MAS concurrency anomalies |
| ATM (arXiv:2607.00041) | paper | Same problem, scoped to *code* co-synthesis; granularity answer |
| Continuum (arXiv:2511.02230) | paper | KV-cache cost of a blocked agent — prior art for handover §7.7 |
| mofa-org/mofa#1022 | field report | Lock held across `.await`; control plane cannot preempt |
| openclaw/openclaw#18470 | field report | Same shape, different framework — commands hang mid-turn |
| earendil-works/pi#5778 | field report | Unbounded hold with the process still alive |
| Apache Curator `InterProcessMutex` | reference | Reentrant-lock counting; revocation is *cooperative* only |
| Wikipedia, *Reentrant mutex* | reference | Non-reentrant double acquire blocks indefinitely |
| Kleppmann, "How to do distributed locking" | reference | Lease expiry race; fencing tokens; not-Redlock |
| eBPF spin lock verifier | reference | Static lock-pairing precedent (handover §5.1) |
| Schneider, Enforceable Security Policies (TISSEC 2000) | reference | Eligibility vs. correctness enforceability |

The last three are carried over from handover §6 and were already load-bearing there; they are
listed for completeness but not re-annotated here.
