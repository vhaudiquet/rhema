//! The lease-driven handler loop (spec §3): admitted keys become handler
//! runs.
//!
//! The [`Driver`] closes the dispatcher's guarantees around an embedder's
//! [`Handler`]: every admitted key is acquired, the handler runs against
//! a [`SessionContext`] — the resumable session and the live inbox — and
//! the run ends checkpointed and released with the handler's outcome.
//! One handler per key at a time is the dispatcher's lease, not the
//! driver's word.
//!
//! Two disciplines keep a run inside its lease:
//!
//! - **Poll to live.** [`SessionContext::poll_inbox`] renews the lease
//!   before draining — every poll is the §4 heartbeat of the *lease*, an
//!   entirely different thing from the forbidden model-invoking timer.
//!   The batch comes back whole: fold it into one reasoning step (spec
//!   §3's batching decision), never one step per item.
//! - **The model is never the detector.** The handler's deterministic
//!   setup — classification, allowlist resolution — happens before its
//!   first model call; this loop never reaches the provider itself.

use std::sync::Arc;

use rhema_core::CommitHook;
use rhema_rsd::{DispatchError, Dispatcher, InboxItem, Lease, Outcome, ResourceKey, Session};
use tokio::task::JoinSet;

/// The key-ownership predicate [`Driver::owning`] takes: which keys
/// this driver's handler answers.
pub type OwnsKey = dyn Fn(&ResourceKey) -> bool + Send + Sync;

use crate::async_trait;

/// The embedder's answer to one dispatch: do the work, report the
/// outcome. Deterministic setup first; delivery is at-least-once, so
/// idempotence is the handler's business.
#[async_trait]
pub trait Handler: Send + Sync {
    /// Runs one dispatch on a resource key.
    async fn handle(&self, ctx: &mut SessionContext) -> Outcome;
}

/// Everything one handler run can touch: the key, the resumable session,
/// and the lease-authorized dispatcher operations.
pub struct SessionContext {
    /// The resource key this run holds the lease for.
    pub key: ResourceKey,
    /// The persistent per-key session. Mutate freely; it is checkpointed
    /// when the run ends (and at every [`Self::checkpoint`]).
    pub session: Session,

    dispatcher: Arc<Dispatcher>,
    lease: Lease,
}

impl SessionContext {
    /// Renews the lease and drains the inbox: every undelivered event
    /// since the last poll, oldest first — the trigger included, on the
    /// first call. Fold the returned batch into one reasoning step.
    pub async fn poll_inbox(&mut self) -> Result<Vec<InboxItem>, DispatchError> {
        self.lease = self.dispatcher.renew(&self.lease)?;
        self.dispatcher.poll_inbox(&self.lease)
    }

    /// Persists the session mid-run. The end-of-run checkpoint happens
    /// regardless; this one matters before a long, interruption-prone
    /// stretch of work (spec §3: checkpoint after every tool-call round).
    pub fn checkpoint(&self) -> Result<(), DispatchError> {
        self.dispatcher
            .checkpoint(&self.lease, self.session.clone())
    }

    /// The commit-before-execute hook bound to this run: persists the
    /// engine's session as this key's engine state under the current
    /// lease. A driver installs it on its engine
    /// ([`rhema_core::Agent::set_commit_hook`]) so a crash inside a tool
    /// call leaves the committed intent in the store instead of no record
    /// at all. A checkpoint that fails here — an expired lease above all —
    /// fails the step before any side effect: the engine never runs a
    /// tool against a lease it cannot commit under.
    pub fn commit_hook(&self) -> CommitHook {
        let dispatcher = self.dispatcher.clone();
        let lease = self.lease.clone();
        let base = self.session.clone();
        Box::new(move |core| {
            let mut session = base.clone();
            session.core = core.clone();
            dispatcher
                .checkpoint(&lease, session)
                .map_err(|err| err.to_string())
        })
    }
}

/// Turns admitted keys into handler runs.
pub struct Driver {
    dispatcher: Arc<Dispatcher>,
    handler: Arc<dyn Handler>,
    holder: String,
    /// Which keys this driver answers: `None` — everything — when it is
    /// the only driver over the store; a predicate when two drivers
    /// share one. The split is load-bearing there: admission reads the
    /// shared table, not a per-driver queue, so an unfiltered driver
    /// would run — and fail, and consume — events addressed to the
    /// other driver's handler.
    owns: Option<Arc<OwnsKey>>,
}

impl Driver {
    /// A driver whose leases are held for `"autonomous-agent"`.
    pub fn new(dispatcher: Arc<Dispatcher>, handler: Arc<dyn Handler>) -> Self {
        Self {
            dispatcher,
            handler,
            holder: "autonomous-agent".to_owned(),
            owns: None,
        }
    }

    /// Names the lease holder (ops visibility on `rhema_rsd::KeyStatus`).
    pub fn with_holder(mut self, holder: impl Into<String>) -> Self {
        self.holder = holder.into();
        self
    }

    /// Restricts [`Self::run_once`] to the keys `owns` answers — the
    /// ownership split when two drivers share one store. Admission is a
    /// read, so a key left for the other driver costs this pass nothing:
    /// the next pass of the driver that owns it offers it again.
    pub fn owning(mut self, owns: Arc<OwnsKey>) -> Self {
        self.owns = Some(owns);
        self
    }

    /// Startup recovery (RSD spec §7): sweeps leases a dead process left
    /// behind. Call once, before the first [`Self::run_once`].
    pub async fn bootstrap(&self) -> Result<(), DispatchError> {
        self.dispatcher.recover()
    }

    /// Admits the keys whose debounce has elapsed and drives each to
    /// completion, concurrently. One outcome per key that became a run;
    /// a key lost to a race or parked at capacity is simply not in the
    /// list. A handler that panics fails its own run — its lease is left
    /// for the TTL sweep, which re-dispatches against current state.
    pub async fn run_once(&self) -> Result<Vec<Outcome>, DispatchError> {
        let keys = self.dispatcher.admit()?;
        let mut set = JoinSet::new();
        for key in keys {
            if let Some(owns) = &self.owns {
                if !owns(&key) {
                    continue;
                }
            }
            let driver = Driver {
                dispatcher: self.dispatcher.clone(),
                handler: self.handler.clone(),
                holder: self.holder.clone(),
                owns: self.owns.clone(),
            };
            set.spawn(async move { driver.drive(key).await });
        }
        let mut outcomes = Vec::new();
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok(run) => outcomes.extend(run?),
                Err(joined) => outcomes.push(Outcome::Failed(joined.to_string())),
            }
        }
        Ok(outcomes)
    }

    /// One key: acquire, run, persist, release. `None` when the key never
    /// became a run.
    async fn drive(self, key: ResourceKey) -> Result<Option<Outcome>, DispatchError> {
        let (lease, session) = match self.dispatcher.acquire(key.clone(), &self.holder) {
            Ok(acquired) => acquired,
            Err(DispatchError::Conflict(_)) | Err(DispatchError::AtCapacity { .. }) => {
                return Ok(None);
            }
            Err(err) => return Err(err),
        };
        let mut ctx = SessionContext {
            key: key.clone(),
            session,
            dispatcher: self.dispatcher.clone(),
            lease,
        };
        let outcome = self.handler.handle(&mut ctx).await;
        // The lease may have died under a handler that never polled: the
        // failure surfaces here, loudly, and the key stays for the sweep.
        self.dispatcher
            .checkpoint(&ctx.lease, ctx.session.clone())?;
        self.dispatcher.release(&ctx.lease, outcome.clone())?;
        Ok(Some(outcome))
    }
}

#[cfg(test)]
mod commit_hook_tests {
    use super::*;
    use rhema_core::{Message, Role};
    use rhema_rsd::{DispatcherConfig, FixedClock, VolatileStore};

    #[test]
    fn the_commit_hook_persists_the_engine_state_under_the_run_lease() {
        let dispatcher = Arc::new(Dispatcher::new(
            DispatcherConfig::default(),
            Arc::new(FixedClock::new(1_000)),
            Arc::new(VolatileStore::new()),
        ));
        let key = ResourceKey::new("github:acme/widgets#1");
        let (lease, _) = dispatcher.acquire(key.clone(), "test").unwrap();
        let ctx = SessionContext {
            key: key.clone(),
            session: Session::fresh(key.clone(), 1_000),
            dispatcher: dispatcher.clone(),
            lease,
        };

        let mut core = rhema_core::Session::default();
        core.history
            .push(Message::text(Role::User, "committed mid-run"));
        ctx.commit_hook()(&core).unwrap();

        // A later holder of the same key resumes with the committed
        // intent: the hook wrote through the store, not into thin air.
        dispatcher.release(&ctx.lease, Outcome::Handled).unwrap();
        let (_, resumed) = dispatcher.acquire(key, "next").unwrap();
        assert_eq!(
            resumed.core.history,
            vec![Message::text(Role::User, "committed mid-run")]
        );
    }
}
