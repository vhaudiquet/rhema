//! The coding jobs path: the source that ingests queued rows, and the
//! handler that turns one into a coding-agent turn off the chat.
//!
//! **The key:** `job:<instance>#<id>` — per job, like a forge thread is
//! per thread, so the dispatcher's dedup and lease machinery applies
//! per job while the *conversation* carries in the instance row's
//! session blob (`crate::job_store`), keyed by instance. A run the
//! restart cut is failed interrupted by the store's recover pass; its
//! next `run` resumes from the last saved session.
//!
//! **The handler** is the forge handler's sibling with a different
//! driver: the deterministic part stays ahead of the model — the
//! instance and job resolve through the store, the workspace is the
//! instance's path, the session blob (or a fresh agent) is loaded, the
//! allowlist is permissive exactly as the forge path's — and then one
//! `CodingAgent` turn runs. The final text lands in the job row and
//! the user's notifications; the turn itself streams nowhere, its
//! observability is the stderr tool log.
//!
//! The permission check is the forge handler's own argument: this
//! handler's one tool is the agent's shell, so a policy without it
//! fails before any model call. The trust boundary is unchanged from
//! the forge path — the bearer-gated surface and the instance list.

use std::sync::Arc;

use rhema_autonomous_agent::{Handler, SessionContext, Source, SourceError, async_trait};
use rhema_coding_agent::{AllowAll, CodingAgent, CodingAgentConfig};
use rhema_core::{Event as ModelEvent, EventSink};
use rhema_rsd::{Dispatcher, Event, KeyFn, Outcome, ResourceKey};

use crate::backends::BackendRegistry;
use crate::handler::final_text;
use crate::interactive::passthrough_key_fn;
use crate::job_store::{CodingJobStore, JobStatus};
use crate::notifications::{DEFAULT_DEVICE, NotificationStore};

/// The source name the coding jobs ingest under.
pub const JOBS_SOURCE: &str = "jobs";

/// One job's dispatch key: `job:<instance>#<id>`.
pub fn job_key(instance: &str, id: i64) -> String {
    format!("job:{instance}#{id}")
}

/// Whether `key` belongs to the coding-jobs context — the check the
/// drivers' ownership split keys on, alongside the resident prefixes
/// and the forge keys.
pub fn is_job_key(key: &str) -> bool {
    key.starts_with("job:")
}

/// Parses the dispatch key back into instance and job id.
pub fn parse_job_key(key: &ResourceKey) -> Option<(String, i64)> {
    let raw = key.as_str().strip_prefix("job:")?;
    let (instance, id) = raw.rsplit_once('#')?;
    if instance.is_empty() || id.is_empty() {
        return None;
    }
    Some((instance.to_owned(), id.parse().ok()?))
}

/// The detectable stream of queued coding jobs: the job table feeding
/// the jobs dispatcher — the task scheduler's shape, one source,
/// registered once, before the first detect.
pub struct JobsSource {
    store: Arc<CodingJobStore>,
    dispatcher: Arc<Dispatcher>,
    key_fn: Arc<KeyFn>,
}

impl JobsSource {
    /// Registers the `jobs` source's key function on `dispatcher` and
    /// returns the source feeding it.
    pub fn new(dispatcher: &Arc<Dispatcher>, store: Arc<CodingJobStore>) -> Self {
        let key_fn = passthrough_key_fn();
        dispatcher.register_source(JOBS_SOURCE, key_fn.clone());
        Self {
            store,
            dispatcher: dispatcher.clone(),
            key_fn,
        }
    }

    /// The event one queued row becomes. The dedup id carries the
    /// row's `updated_at_millis`, so a row the recover pass re-queued
    /// after a restart ingests fresh instead of drowning in the
    /// previous process's consumed set.
    fn event(job: &crate::job_store::JobRecord, now: u64) -> Event {
        Event {
            id: format!("job-{}@{}", job.id, job.updated_at_millis),
            source: JOBS_SOURCE.to_owned(),
            payload: json_key(&job.instance, job.id),
            received_at: now,
        }
    }
}

fn json_key(instance: &str, id: i64) -> serde_json::Value {
    serde_json::json!({ "key": job_key(instance, id) })
}

#[async_trait]
impl Source for JobsSource {
    fn name(&self) -> &str {
        JOBS_SOURCE
    }

    fn key_fn(&self) -> Arc<KeyFn> {
        self.key_fn.clone()
    }

    async fn detect(&self) -> Result<usize, SourceError> {
        let queued = self
            .store
            .queued()
            .map_err(|err| SourceError::Backend(format!("job store: {err}")))?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let mut fed = 0;
        for job in queued {
            self.dispatcher
                .ingest(Self::event(&job, now))
                .map_err(|err| SourceError::Backend(format!("job ingest: {err}")))?;
            fed += 1;
        }
        Ok(fed)
    }
}

/// The coding-agent jobs handler: one dispatch of `job:<instance>#<id>`
/// is one coding-agent turn, and the turn's final text is the job's
/// result and the user's notification.
pub struct CodingJobHandler {
    backends: Arc<BackendRegistry>,
    store: Arc<CodingJobStore>,
    notifications: Arc<NotificationStore>,
    max_turn_steps: u32,
}

impl CodingJobHandler {
    /// A handler whose turns run in the instance's workspace, read per
    /// dispatch, with the deployment's active provider pair and the
    /// resident turn's step budget.
    pub fn new(
        backends: Arc<BackendRegistry>,
        store: Arc<CodingJobStore>,
        notifications: Arc<NotificationStore>,
        max_turn_steps: u32,
    ) -> Self {
        Self {
            backends,
            store,
            notifications,
            max_turn_steps,
        }
    }

    /// One dispatch, failed with a reason on any step; every failure
    /// lands in the job row before it lands in the outcome, so the
    /// model's `status` read and the user's notification agree.
    async fn dispatch(&self, ctx: &mut SessionContext) -> Result<Outcome, String> {
        let (instance_name, job_id) = parse_job_key(&ctx.key)
            .ok_or_else(|| format!("resource key {} is not a job key", ctx.key))?;
        let job = self
            .store
            .job(job_id)
            .map_err(|err| format!("job lookup for {job_id} failed: {err}"))?
            .ok_or_else(|| format!("job {job_id} is not in the table"))?;
        if job.status != JobStatus::Queued {
            // A re-delivered event over a row already running or done:
            // the dedup boundary is the dispatcher's; this handler
            // treats the row as the truth.
            return Ok(Outcome::Handled);
        }
        let instance = self
            .store
            .instance(&instance_name)
            .map_err(|err| format!("instance lookup for {instance_name} failed: {err}"))?
            .ok_or_else(|| format!("instance {instance_name:?} is gone; the job cannot run"))?;
        let workspace = std::path::PathBuf::from(&instance.path);
        if !workspace.is_dir() {
            let reason = format!(
                "the workspace {workspace:?} does not exist; add the instance again with a live path"
            );
            self.fail(job_id, &instance_name, &reason);
            return Ok(Outcome::Failed(reason));
        }
        self.store
            .mark_running(job_id)
            .map_err(|err| format!("job {job_id} could not start: {err}"))?;

        let (provider, model) = self.backends.current();
        let mut config = CodingAgentConfig::new(model, &workspace);
        config.max_turn_steps = self.max_turn_steps;
        let stashed = self
            .store
            .session(&instance_name)
            .map_err(|err| format!("the session lookup for {instance_name:?} failed: {err}"))?;
        let mut agent = match stashed {
            Some(blob) => {
                let session: rhema_core::Session = serde_json::from_str(&blob).map_err(|err| {
                    let reason = format!(
                        "the stashed coding session for {instance_name:?} does not parse: {err}"
                    );
                    self.fail(job_id, &instance_name, &reason);
                    reason
                })?;
                CodingAgent::from_session(provider, config, session)
            }
            None => CodingAgent::new(provider, config),
        }
        .map_err(|err| {
            let reason = format!("cannot start the coding agent for {instance_name:?}: {err}");
            self.fail(job_id, &instance_name, &reason);
            reason
        })?;
        // Commit-before-execute rides the dispatcher exactly as the
        // forge handler's does: the intent is in the store before the
        // first tool runs, and a crash-left dangling call revives at
        // turn entry (CodingAgent::turn's own revive, over the resumed
        // session).
        agent.set_commit_hook(Some(ctx.commit_hook()));

        let mut sink = JobSink;
        let output = match agent.turn(&job.prompt, &mut sink, Arc::new(AllowAll)).await {
            Ok(output) => output,
            Err(err) => {
                let reason = format!("the coding turn for {instance_name:?} failed: {err}");
                // The conversation up to the failure is still the
                // instance's: saved, so a follow-up run continues it.
                self.save_session(&instance_name, &agent);
                self.fail(job_id, &instance_name, &reason);
                return Ok(Outcome::Failed(reason));
            }
        };
        self.save_session(&instance_name, &agent);
        let reply = final_text(&output.message).ok_or_else(|| {
            let reason = format!("the model returned no text to record for job {job_id}");
            self.fail(job_id, &instance_name, &reason);
            reason
        })?;
        self.store
            .finish(job_id, &reply)
            .map_err(|err| format!("job {job_id} could not be finished: {err}"))?;
        // The implicit channel, the task keys' rule: the reply reaches
        // the user away from this chat; `status` is the model's way to
        // read it again.
        self.notify(&instance_name, &reply);
        Ok(Outcome::Handled)
    }

    fn save_session(&self, instance: &str, agent: &CodingAgent) {
        match serde_json::to_string(agent.session()) {
            Ok(blob) => {
                let _ = self.store.save_session(instance, &blob);
            }
            Err(err) => {
                eprintln!(
                    "[claw] jobs: the coding session for {instance:?} does not serialize: {err}"
                );
            }
        }
    }

    fn fail(&self, job_id: i64, instance: &str, reason: &str) {
        if let Err(err) = self.store.fail(job_id, reason) {
            eprintln!("[claw] jobs: job {job_id} could not be failed: {err}");
        }
        self.notify(instance, &format!("coding job failed: {reason}"));
    }

    fn notify(&self, instance: &str, body: &str) {
        if let Err(err) = self.notifications.push(DEFAULT_DEVICE, instance, body) {
            eprintln!("[claw] jobs: the notification for {instance:?} failed: {err}");
        }
    }
}

#[async_trait]
impl Handler for CodingJobHandler {
    async fn handle(&self, ctx: &mut SessionContext) -> Outcome {
        match self.dispatch(ctx).await {
            Ok(outcome) => outcome,
            Err(reason) => Outcome::Failed(reason),
        }
    }
}

/// The job's stderr log: tool traffic only. Nothing streams to a chat
/// surface — the result is the row and the notification.
struct JobSink;

impl EventSink for JobSink {
    fn emit(&mut self, event: ModelEvent<'_>) {
        match event {
            ModelEvent::ToolCallStart { name, .. } => eprintln!("[claw] job tool call: {name}"),
            ModelEvent::ToolResult {
                content, is_error, ..
            } => {
                let mark = if is_error { "failed" } else { "ok" };
                let line = content.lines().next().unwrap_or("");
                let mut cut = line.chars().take(72).collect::<String>();
                if line.chars().count() > 72 {
                    cut.push('…');
                }
                eprintln!("[claw] job tool {mark}: {cut}");
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::Backend;
    use rhema_autonomous_agent::Driver;
    use rhema_core::{Message as CoreMessage, MockProvider, ModelOutput, Role, StopReason, Usage};
    use rhema_rsd::{DispatcherConfig, SqliteStore, SystemClock};

    /// A registry whose only backend is the scripted mock.
    fn mock_handler(
        replies: Vec<ModelOutput>,
        store: Arc<CodingJobStore>,
        notifications: Arc<NotificationStore>,
    ) -> CodingJobHandler {
        let provider: Arc<dyn rhema_core::ModelProvider> = Arc::new(MockProvider::new(replies));
        let backends = Arc::new(
            BackendRegistry::new(vec![Backend::headless("mock", provider)], None)
                .expect("a single mock backend always constructs"),
        );
        CodingJobHandler::new(backends, store, notifications, 16)
    }

    fn reply(text: &str) -> ModelOutput {
        ModelOutput {
            message: CoreMessage::text(Role::Assistant, text),
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        }
    }

    fn tempdir() -> std::path::PathBuf {
        let dir = tempfile::tempdir().unwrap().keep();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn a_queued_job_runs_to_done_and_notifies() {
        let workspace = tempdir();
        let store = Arc::new(CodingJobStore::in_memory().unwrap());
        store
            .add_instance("rhema", workspace.to_str().unwrap())
            .unwrap();
        let job = store.enqueue("rhema", "fix the lint").unwrap();
        let notifications = Arc::new(NotificationStore::in_memory().unwrap());
        let handler = mock_handler(
            vec![reply("lint fixed, all green")],
            store.clone(),
            notifications.clone(),
        );

        // The whole path: source detects the queued row, the driver
        // runs the handler over its own dispatcher.
        let (source, driver) = harness(store.clone(), handler);

        assert_eq!(source.detect().await.unwrap(), 1);
        let outcomes = driver.run_once().await.unwrap();
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(outcomes[0], Outcome::Handled));

        let done = store.job(job.id).unwrap().unwrap();
        assert_eq!(done.status, JobStatus::Done);
        assert_eq!(done.result.as_deref(), Some("lint fixed, all green"));
        let instance = store.instance("rhema").unwrap().unwrap();
        assert!(instance.session.is_some(), "the session is stashed");
        let pending = notifications.pending(DEFAULT_DEVICE, 0).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].body, "lint fixed, all green");
        // The source finds nothing more to feed.
        assert_eq!(source.detect().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn the_second_job_resumes_the_first_session() {
        let workspace = tempdir();
        let store = Arc::new(CodingJobStore::in_memory().unwrap());
        store
            .add_instance("rhema", workspace.to_str().unwrap())
            .unwrap();
        let notifications = Arc::new(NotificationStore::in_memory().unwrap());
        let handler = mock_handler(
            vec![reply("first done"), reply("second done, history intact")],
            store.clone(),
            notifications.clone(),
        );
        let first = store.enqueue("rhema", "step one").unwrap();
        let second = store.enqueue("rhema", "step two").unwrap_err();
        assert!(
            second.to_string().contains("job in flight"),
            "one live job per instance: {second}"
        );
        let (source, driver) = harness(store.clone(), handler);

        assert_eq!(source.detect().await.unwrap(), 1);
        driver.run_once().await.unwrap();
        assert_eq!(
            store.job(first.id).unwrap().unwrap().status,
            JobStatus::Done
        );

        // With the first job done, the follow-up enqueues and runs on
        // the resumed session: the mock's second reply lands, and the
        // stash the second turn loaded carried the first turn's rounds.
        let follow = store.enqueue("rhema", "step two").unwrap();
        assert_eq!(source.detect().await.unwrap(), 1);
        driver.run_once().await.unwrap();
        let done = store.job(follow.id).unwrap().unwrap();
        assert_eq!(done.result.as_deref(), Some("second done, history intact"));
        let session = store.instance("rhema").unwrap().unwrap().session.unwrap();
        let session: rhema_core::Session = serde_json::from_str(&session).unwrap();
        // Two turns: two user inputs, two assistant replies.
        assert!(session.history.len() >= 4, "{:?}", session.history.len());
    }

    #[tokio::test]
    async fn a_missing_workspace_fails_the_job_in_band() {
        let store = Arc::new(CodingJobStore::in_memory().unwrap());
        store.add_instance("gone", "/nowhere-at-all").unwrap();
        let job = store.enqueue("gone", "work").unwrap();
        let notifications = Arc::new(NotificationStore::in_memory().unwrap());
        let handler = mock_handler(
            vec![reply("never reached")],
            store.clone(),
            notifications.clone(),
        );
        let (source, driver) = harness(store.clone(), handler);

        assert_eq!(source.detect().await.unwrap(), 1);
        let outcomes = driver.run_once().await.unwrap();
        assert!(
            matches!(&outcomes[0], Outcome::Failed(reason) if reason.contains("does not exist")),
            "{outcomes:?}"
        );
        assert_eq!(
            store.job(job.id).unwrap().unwrap().status,
            JobStatus::Failed
        );
        let pending = notifications.pending(DEFAULT_DEVICE, 0).unwrap();
        assert!(pending[0].body.contains("coding job failed"), "{pending:?}");
    }

    /// The source and driver over one in-memory dispatcher, the shape
    /// the daemon wires and these tests drive.
    fn harness(store: Arc<CodingJobStore>, handler: CodingJobHandler) -> (JobsSource, Driver) {
        let rsd_store = Arc::new(SqliteStore::open(tempdir().join("rsd.db")).unwrap());
        // A zero debounce: the queued row is admitted on this pass,
        // the shape the daemon's jobs dispatcher runs under.
        let dispatcher = Arc::new(Dispatcher::new(
            DispatcherConfig {
                debounce_window_millis: 0,
                ..DispatcherConfig::default()
            },
            Arc::new(SystemClock),
            rsd_store,
        ));
        let source = JobsSource::new(&dispatcher, store);
        let driver = Driver::new(dispatcher, Arc::new(handler))
            .with_holder("test-jobs")
            .owning(Arc::new(|key: &ResourceKey| is_job_key(key.as_str())));
        (source, driver)
    }
}
