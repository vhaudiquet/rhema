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

use rhema_rsd::{DispatchError, Dispatcher, InboxItem, Lease, Outcome, ResourceKey, Session};
use tokio::task::JoinSet;

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
}

/// Turns admitted keys into handler runs.
pub struct Driver {
    dispatcher: Arc<Dispatcher>,
    handler: Arc<dyn Handler>,
    holder: String,
}

impl Driver {
    /// A driver whose leases are held for `"autonomous-agent"`.
    pub fn new(dispatcher: Arc<Dispatcher>, handler: Arc<dyn Handler>) -> Self {
        Self {
            dispatcher,
            handler,
            holder: "autonomous-agent".to_owned(),
        }
    }

    /// Names the lease holder (ops visibility on `rhema_rsd::KeyStatus`).
    pub fn with_holder(mut self, holder: impl Into<String>) -> Self {
        self.holder = holder.into();
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
            let driver = Driver {
                dispatcher: self.dispatcher.clone(),
                handler: self.handler.clone(),
                holder: self.holder.clone(),
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
