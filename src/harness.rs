//! The harness: the sole gatekeeper to the protected resource.
//!
//! Exposes exactly two mediated actions to agents — [`Harness::read`] and [`Harness::write`].
//! Agents never receive `acquire`/`release`, and never see a ticket (handover §5 option (a),
//! `doc/lock-interface.md` §4.3). There is therefore nothing about lock discipline for an agent to
//! get wrong, and nothing for it to forge.
//!
//! Every guarantee here rests on there being no other route to the resource (handover §7.4). That
//! assumption is currently unenforced — see handover §10.3 on why it is the weakest point in the
//! design rather than a minor to-do.

use crate::history::{Event, History};
use crate::lock::LockService;
use crate::store::{FileStore, content_hash};
use crate::types::*;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use tokio::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct Config {
    pub span: Span,
    /// Liveness timeout — *seconds* scale, because it detects whether a process is alive across a
    /// network. Default from Kubernetes leader election, which solves the same problem.
    pub lease: Duration,
    /// Policy limit on how long work may take — *minutes* scale, because inference is minutes.
    /// No prior art exists (no existing lock service holds locks this long); the default comes from
    /// openclaw#18470's observed 10-minute agent-run timeout. See `doc/lock-interface.md` §7 D6b.
    pub hold_ceiling: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            span: Span::WriteOnly,
            lease: Duration::from_secs(15),
            hold_ceiling: Duration::from_secs(600),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteError {
    Refused(Refusal),
    Lock(LockError),
    /// The hold ceiling was reached, so renewal stopped and the lock was lost. Slow and stuck are
    /// indistinguishable from outside (`doc/agent-model.md` §5.5), so this fires for both.
    CeilingExceeded,
    /// Authorized, then superseded by another holder before this write could be applied. Fails
    /// safe: nothing visible is lost, because the write was never applied.
    Superseded { authorized: Version, current: Version },
    Io(String),
}

/// What an agent gets back from a read. Carries the version it read at, so staleness is checkable
/// at commit time. The agent cannot inspect or fabricate it meaningfully — it is opaque to it.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub content: String,
    pub version: Version,
}

/// A write that the lock service has authorized but which has not yet been applied to disk.
///
/// Exposed so tests can drive the two phases separately (R14). In normal operation
/// [`Harness::write`] does both and this is never seen.
pub struct Authorized {
    holding: Holding,
    tmp: PathBuf,
    new_version: Version,
    content: String,
}

struct Open {
    holding: Holding,
    taken_at: Instant,
}

pub struct Harness<L: LockService> {
    lock: L,
    store: FileStore,
    history: History,
    cfg: Config,
    resource: ResourceId,
    start: Instant,
    /// Locks held across an agent's thinking, under [`Span::Full`] only.
    open: Mutex<HashMap<HolderId, Open>>,
}

impl<L: LockService> Harness<L> {
    pub fn new(lock: L, store: FileStore, history: History, cfg: Config) -> Self {
        let resource = store.path().to_string_lossy().to_string();
        Self {
            lock,
            store,
            history,
            cfg,
            resource,
            start: Instant::now(),
            open: Mutex::new(HashMap::new()),
        }
    }

    pub fn history(&self) -> &History {
        &self.history
    }

    pub fn lock_service(&self) -> &L {
        &self.lock
    }

    pub fn resource(&self) -> &ResourceId {
        &self.resource
    }

    fn now_ms(&self) -> u64 {
        Instant::now().saturating_duration_since(self.start).as_millis() as u64
    }

    /// Read the resource.
    ///
    /// Under [`Span::WriteOnly`] this takes no lock at all — which is why the version in the
    /// returned [`Snapshot`] is load-bearing: another holder may write before this agent commits
    /// (R6). Under [`Span::Full`] this acquires the lock and keeps it until the write, so nothing
    /// can change underneath and the version check is redundant.
    pub async fn read(&self, holder: &HolderId) -> Result<Snapshot, WriteError> {
        if self.cfg.span == Span::Full {
            let holding = self
                .lock
                .acquire(&self.resource, holder)
                .await
                .map_err(WriteError::Lock)?;
            self.open
                .lock()
                .unwrap()
                .insert(holder.clone(), Open { holding, taken_at: Instant::now() });
        }
        let version = self
            .lock
            .current_version(&self.resource)
            .await
            .map_err(WriteError::Lock)?;
        let content = self.store.read().map_err(|e| WriteError::Io(e.to_string()))?;
        self.history.record(
            self.now_ms(),
            Event::Read { holder: holder.clone(), resource: self.resource.clone(), version },
        );
        Ok(Snapshot { content, version })
    }

    /// Report observed progress, keeping the lease alive.
    ///
    /// **Progress-driven renewal, not a background timer.** Each arriving token counts as "still
    /// here", so a wedged decode stops renewing by itself and the lock frees with no watchdog
    /// (`doc/lock-interface.md` §7 D6). Renewal never continues past the hold ceiling — that is the
    /// one rule covering every case, and it is why an unbounded hold cannot happen even when the
    /// agent is alive and healthy (R4).
    ///
    /// v1 simplification, recorded rather than hidden: non-streaming decode and tool execution have
    /// no progress signal at all, so they simply do not call this and the lease lapses on its own.
    /// A real implementation would use a *bounded* background timer for those, capped at the same
    /// ceiling.
    pub async fn progress(&self, holder: &HolderId) -> Result<(), WriteError> {
        let (holding, taken_at) = {
            let g = self.open.lock().unwrap();
            match g.get(holder) {
                Some(o) => (o.holding.clone(), o.taken_at),
                None => return Ok(()),
            }
        };
        if Instant::now().duration_since(taken_at) >= self.cfg.hold_ceiling {
            self.history.record(
                self.now_ms(),
                Event::CeilingReached {
                    holder: holder.clone(),
                    resource: self.resource.clone(),
                },
            );
            return Err(WriteError::CeilingExceeded);
        }
        self.lock.keepalive(&holding).await.map_err(WriteError::Lock)
    }

    /// Phase 1: get the lock service to authorize this write, indivisibly.
    ///
    /// The ticket comparison and the version comparison happen together inside
    /// [`LockService::commit`], so there is no gap between checking and deciding — a revoked
    /// holder's transaction simply fails (`doc/lock-interface.md` §4.2b). This is the commit point:
    /// ticket validity is defined here, not when bytes reach the disk.
    pub async fn authorize(
        &self,
        holder: &HolderId,
        read_at: Version,
        content: &str,
    ) -> Result<Authorized, WriteError> {
        let holding = match self.cfg.span {
            Span::Full => {
                let g = self.open.lock().unwrap();
                let o = g.get(holder).ok_or(WriteError::Lock(LockError::Expired))?;
                if Instant::now().duration_since(o.taken_at) >= self.cfg.hold_ceiling {
                    return Err(WriteError::CeilingExceeded);
                }
                o.holding.clone()
            }
            // Hold time is a local write: thin-tailed, so a tight ceiling is actually safe here.
            Span::WriteOnly => self
                .lock
                .acquire(&self.resource, holder)
                .await
                .map_err(WriteError::Lock)?,
        };

        self.history.record(
            self.now_ms(),
            Event::WriteAttempted {
                holder: holder.clone(),
                resource: self.resource.clone(),
                ticket: holding.ticket,
                expected_version: read_at,
            },
        );

        let tmp = self
            .store
            .prepare(content)
            .map_err(|e| WriteError::Io(e.to_string()))?;

        match self.lock.commit(&holding, read_at, content_hash(content)).await {
            Ok(new_version) => Ok(Authorized {
                holding,
                tmp,
                new_version,
                content: content.to_string(),
            }),
            Err(reason) => {
                let _ = std::fs::remove_file(&tmp);
                self.history.record(
                    self.now_ms(),
                    Event::WriteRefused {
                        holder: holder.clone(),
                        resource: self.resource.clone(),
                        reason: reason.clone(),
                        content: content.to_string(),
                    },
                );
                if self.cfg.span == Span::WriteOnly {
                    let _ = self.lock.release(&holding).await;
                }
                Err(WriteError::Refused(reason))
            }
        }
    }

    /// Phase 2: apply an authorized write to disk.
    ///
    /// Guards on the version before renaming. If another holder committed in between, our
    /// authorization has been superseded and we must **discard rather than apply** — otherwise our
    /// older content would land on top of theirs while the lock service records theirs, leaving the
    /// file and the service in disagreement (R14).
    ///
    /// A residual one-syscall gap remains between this check and the rename. Single-threaded
    /// serialisation makes it unreachable in practice, but that is an argument from our own design
    /// rather than a property the filesystem provides. Recorded as open in `doc/agent-model.md`
    /// §5.4b, not solved.
    pub async fn apply(&self, a: Authorized) -> Result<Version, WriteError> {
        let current = self
            .lock
            .current_version(&self.resource)
            .await
            .map_err(WriteError::Lock)?;
        if current != a.new_version {
            let _ = std::fs::remove_file(&a.tmp);
            self.history.record(
                self.now_ms(),
                Event::WriteRefused {
                    holder: a.holding.holder.clone(),
                    resource: self.resource.clone(),
                    reason: Refusal::StaleVersion { read: a.new_version, current },
                    content: a.content.clone(),
                },
            );
            self.finish(&a.holding).await;
            return Err(WriteError::Superseded { authorized: a.new_version, current });
        }

        self.store
            .commit(&a.tmp)
            .map_err(|e| WriteError::Io(e.to_string()))?;

        self.history.record(
            self.now_ms(),
            Event::WriteAccepted {
                holder: a.holding.holder.clone(),
                resource: self.resource.clone(),
                ticket: a.holding.ticket,
                version: a.new_version,
                content: a.content.clone(),
            },
        );
        self.finish(&a.holding).await;
        Ok(a.new_version)
    }

    async fn finish(&self, holding: &Holding) {
        self.open.lock().unwrap().remove(&holding.holder);
        let _ = self.lock.release(holding).await;
    }

    /// The normal path: authorize then apply.
    pub async fn write(
        &self,
        holder: &HolderId,
        read_at: Version,
        content: &str,
    ) -> Result<Version, WriteError> {
        let a = self.authorize(holder, read_at, content).await?;
        self.apply(a).await
    }

    /// Abandon a read without writing, releasing anything held.
    ///
    /// R8's boring, very common bug: a lock leaked because release only ran on the success path.
    pub async fn abandon(&self, holder: &HolderId) {
        let holding = self.open.lock().unwrap().remove(holder);
        if let Some(o) = holding {
            let _ = self.lock.release(&o.holding).await;
        }
    }

    /// Take the lock away from whoever holds it, without acquiring it.
    ///
    /// A preemptor calling this can never block on the lock it is reclaiming — which is what makes
    /// R4 survivable, and what Curator's cooperative revocation cannot do for a wedged holder.
    pub async fn force_release(&self) -> Result<Ticket, LockError> {
        self.lock.force_release(&self.resource).await
    }
}
