# Lock primitive semantics — documented behaviour of real implementations

Fetched and checked 2026-08-27. These are library/reference docs rather than papers or bug reports,
and they exist to settle whether R9 (retry lock leak) and §5.3 (revocation) are real concerns or
invented ones. Both turn out to be documented, load-bearing properties of mature implementations.

---

## R9 fails in both directions — reentrancy is not the fix

**Non-reentrant lock, double acquire** — [Wikipedia, *Reentrant mutex*](https://en.wikipedia.org/wiki/Reentrant_mutex):

> "While a thread that attempts to lock a standard (non-reentrant) mutex that it already holds would
> block indefinitely, this operation succeeds on a reentrant mutex."

So a retry that re-enters an already-held non-reentrant lock **self-deadlocks**. One agent, zero
contention, blocked forever.

**Reentrant lock, unbalanced release** — same source on the counting semantics:

> "The owning thread can acquire the lock multiple times, incrementing the count each time."

> "The lock is only released for other threads to acquire once the owning thread has unlocked it the
> same number of times it was acquired, bringing the count to zero."

Confirmed in a production implementation —
[Apache Curator `InterProcessMutex`](https://curator.apache.org/apidocs/org/apache/curator/framework/recipes/locks/InterProcessMutex.html),
described as "A re-entrant mutex that works across JVMs" using ZooKeeper:

> "the same thread can call acquire re-entrantly. Each call to acquire must be balanced by a call to
> `release()`"

> "If the thread had made multiple calls to acquire, the mutex will still be held when this method
> returns."

So a retry that acquires twice and releases once **leaks the lock** — held until lease expiry, with
no error raised.

**The conclusion that matters: "just make it reentrant" is not a fix.** It converts a loud deadlock
into a silent leak, which is strictly worse for a project whose guarantee depends on the lock
actually being released. Both halves above are verbatim from primary docs; the retry-specific
framing is our inference from those documented semantics, not a quoted claim.

> Provenance note: a search summary also attributed a client-side-proxy retry scenario and a
> `LockAcquireLimitReachedException` to the Wikipedia page. **That text is not on the page** as
> fetched, so it is deliberately not cited here. The two mechanisms above are sufficient and are
> directly quoted.

---

## Curator's revocation is *cooperative* — which is exactly the gap §5.3 fills

[`InterProcessMutex` / `Revocable`](https://curator.apache.org/apidocs/org/apache/curator/framework/recipes/locks/InterProcessMutex.html):

> "Your listener will get called when another process/thread wants you to release the lock.
> Revocation is **cooperative**."

This is a strong reference point, and arguably the most useful thing in this file. Curator is *the*
mature ZooKeeper recipe library, and the only revocation it offers is a polite request that the
holder must voluntarily honour. A holder that is hung, wedged, or simply not running its listener
never complies — which is mofa#1022 restated in the API semantics of the best-in-class library.

So §5.3's fencing-token revocation is not reinventing something Curator already provides. Curator
provides the *cooperative* version; this project needs the **unilateral** one, and gets it by bumping
the token in the lock service so the stuck holder's write is refused at write time regardless of
whether it ever notices. Worth stating in any writeup: the standard tool stops at "ask nicely."
