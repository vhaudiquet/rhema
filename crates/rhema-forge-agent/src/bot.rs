//! The dispatch glue: admitted keys become handler invocations.
//!
//! The [`ForgeBot`] closes the loop between the dispatcher and a forge:
//! every [`rhema_rsd::Dispatcher::admit`] key is acquired, the thread's
//! current state is fetched, and the embedder's [`Handler`] runs with a
//! [`ThreadContext`] — the snapshot, the resumable
//! [`rhema_rsd::Session`], and the forge capabilities (comment, refresh,
//! poll the live inbox). When the handler returns, the session is
//! checkpointed and the lease released with the handler's outcome.
//!
//! Two rules keep a run inside the lease:
//!
//! - **Poll to live.** [`ThreadContext::poll_inbox`] renews the lease
//!   before draining — every poll is the spec §4 heartbeat. A handler
//!   that never polls and outruns the TTL loses its lease; its checkpoint
//!   and release are revision-rejected, and the key is swept for a fresh
//!   dispatch.
//! - **One run per key, ever.** A lost race (two admits offering the same
//!   key) or a full admission queue is not a run: the loser simply skips.

use std::sync::Arc;

use rhema_rsd::{DispatchError, Dispatcher, InboxItem, Lease, Outcome, ResourceKey, Session};

use crate::async_trait;
use crate::error::ForgeError;
use crate::event::{ForgeEvent, RepoRef, Snapshot};
use crate::forge::Forge;
use crate::watcher::parse_key;

/// The embedder's answer to one dispatch: given the thread context, do the
/// work and report the outcome. Idempotence is the handler's business —
/// delivery is at-least-once.
#[async_trait]
pub trait Handler: Send + Sync {
    /// Runs one dispatch on a thread.
    async fn handle(&self, ctx: &mut ThreadContext) -> Outcome;
}

/// Everything one handler run can touch: the thread's coordinates, its
/// current state on the forge, the resumable session, and the capabilities
/// the lease authorizes.
pub struct ThreadContext {
    /// The resource key this run holds the lease for.
    pub key: ResourceKey,
    /// Where the thread lives.
    pub repo: RepoRef,
    /// The thread number (GitLab: the iid).
    pub number: u64,
    /// The thread's state as of dispatch, straight from the forge.
    pub snapshot: Snapshot,
    /// The persistent per-key session. Mutate freely; it is checkpointed
    /// when the run ends (and at every [`Self::checkpoint`]).
    pub session: Session,

    dispatcher: Arc<Dispatcher>,
    forge: Arc<dyn Forge>,
    lease: Lease,
}

impl ThreadContext {
    /// Renews the lease (the heartbeat) and drains the inbox: every event
    /// that arrived since the last poll, oldest first — including the ones
    /// that triggered this dispatch, on the first call.
    pub async fn poll_inbox(&mut self) -> Result<Vec<InboxItem>, DispatchError> {
        self.lease = self.dispatcher.renew(&self.lease)?;
        self.dispatcher.poll_inbox(&self.lease)
    }

    /// Persists the session mid-run. The end-of-run checkpoint happens
    /// regardless; this one matters before a long, interruption-prone
    /// stretch of work.
    pub fn checkpoint(&self) -> Result<(), DispatchError> {
        self.dispatcher
            .checkpoint(&self.lease, self.session.clone())
    }

    /// Posts a comment on the thread this run is leased for.
    pub async fn comment(&self, body: &str) -> Result<(), ForgeError> {
        self.forge.comment(&self.repo, self.number, body).await
    }

    /// Refetches the thread's state — recent enough to answer "what is
    /// happening" without re-deriving it, stale enough to need this.
    pub async fn refresh_snapshot(&self) -> Result<Snapshot, ForgeError> {
        self.forge.snapshot(&self.repo, self.number).await
    }
}

/// Decodes a delivered inbox item back into the forge event the watcher
/// fed. A payload this crate did not build is a loud parse error, never a
/// silently skipped event.
pub fn decode(item: &InboxItem) -> Result<ForgeEvent, ForgeError> {
    serde_json::from_value(item.event.payload.clone())
        .map_err(|err| ForgeError::Parse(format!("inbox payload is not a forge event: {err}")))
}

/// The forge-watching bot: a dispatcher, a forge client, and what to do
/// about a thread.
pub struct ForgeBot {
    dispatcher: Arc<Dispatcher>,
    forge: Arc<dyn Forge>,
    handler: Arc<dyn Handler>,
    holder: String,
}

impl ForgeBot {
    /// A bot whose leases are held for `"forge-bot"`.
    pub fn new(
        dispatcher: Arc<Dispatcher>,
        forge: Arc<dyn Forge>,
        handler: Arc<dyn Handler>,
    ) -> Self {
        Self {
            dispatcher,
            forge,
            handler,
            holder: "forge-bot".to_owned(),
        }
    }

    /// Names the lease holder (ops visibility on [`rhema_rsd::KeyStatus`]).
    pub fn with_holder(mut self, holder: impl Into<String>) -> Self {
        self.holder = holder.into();
        self
    }

    /// Startup recovery (spec §7): sweeps leases a dead process left
    /// behind. Call once, before the first [`Self::run_once`].
    pub async fn bootstrap(&self) -> Result<(), DispatchError> {
        self.dispatcher.recover()
    }

    /// Admits the keys whose debounce has elapsed and drives each to
    /// completion, concurrently. One result per key that became a run; a
    /// key lost to a race or parked at capacity is simply not in the list.
    /// A handler that panics fails its own run — the lease it held is left
    /// for the TTL sweep, which re-dispatches against current state.
    pub async fn run_once(&self) -> Result<Vec<Outcome>, DispatchError> {
        let keys = self.dispatcher.admit()?;
        let mut set = tokio::task::JoinSet::new();
        for key in keys {
            let bot = ForgeBot {
                dispatcher: self.dispatcher.clone(),
                forge: self.forge.clone(),
                handler: self.handler.clone(),
                holder: self.holder.clone(),
            };
            set.spawn(async move { bot.drive(key).await });
        }
        let mut outcomes = Vec::new();
        while let Some(joined) = set.join_next().await {
            match joined {
                // The handler ran to a reported outcome (or its prelude
                // failed loudly, releasing the lease either way).
                Ok(run) => outcomes.extend(run?),
                // The task died mid-run without releasing: the sweep owns
                // the key now.
                Err(joined) => outcomes.push(Outcome::Failed(joined.to_string())),
            }
        }
        Ok(outcomes)
    }

    /// One key: acquire, orient on the forge, run the handler, persist,
    /// release. `None` when the key never became a run.
    async fn drive(self, key: ResourceKey) -> Result<Option<Outcome>, DispatchError> {
        let (lease, session) = match self.dispatcher.acquire(key.clone(), &self.holder) {
            Ok(acquired) => acquired,
            // Lost the race against a concurrent run, or the caps are full.
            Err(DispatchError::Conflict(_)) | Err(DispatchError::AtCapacity { .. }) => {
                return Ok(None);
            }
            Err(err) => return Err(err),
        };
        let (repo, number) = match parse_key(&key) {
            Some(found) => found,
            None => {
                let outcome = Outcome::Failed(format!("resource key {key} does not parse"));
                self.dispatcher.release(&lease, outcome.clone())?;
                return Ok(Some(outcome));
            }
        };
        let snapshot = match self.forge.snapshot(&repo, number).await {
            Ok(snapshot) => snapshot,
            Err(err) => {
                let outcome = Outcome::Failed(format!("snapshot of {key} failed: {err}"));
                self.dispatcher.release(&lease, outcome.clone())?;
                return Ok(Some(outcome));
            }
        };
        let mut ctx = ThreadContext {
            key: key.clone(),
            repo,
            number,
            snapshot,
            session,
            dispatcher: self.dispatcher.clone(),
            forge: self.forge.clone(),
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
mod tests {
    use super::*;

    #[test]
    fn an_unparsable_key_fails_its_run_without_a_forge_call() {
        assert_eq!(parse_key(&ResourceKey::from("github:nonsense")), None);
    }
}
