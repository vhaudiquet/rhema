//! Automatic retries for transient model failures.
//!
//! [`Retrying`] wraps any [`ModelProvider`]: a request that failed with a
//! retryable error — a transport timeout, a network failure, or a
//! server-side status (see [`ProviderError::is_retryable`]) — is
//! re-issued with exponential backoff, a bounded number of times. Every
//! turn provider the frontends assemble rides one, so a provider hiccup
//! costs a pause instead of the whole turn.
//!
//! Two guards keep a retry honest:
//!
//! - A request whose stream already emitted an event never retries. The
//!   deltas reached the caller's sink, and a second attempt would stream
//!   them again next to the first. Only a failure before the first byte
//!   is invisible enough to redo.
//! - A request that "succeeds" without producing anything — a provider
//!   that closed the stream before its first chunk — counts as dead like
//!   any error and retries. If every attempt comes back empty, the
//!   exchange fails naming that fact, rather than completing a turn
//!   nobody saw.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rhema_core::{Event, EventSink, ModelOutput, ModelProvider, ModelRequest, ProviderError};

/// Re-issues failed model requests with exponential backoff.
///
/// The pause before the first retry is one second; each later retry
/// waits twice its predecessor, capped at two. No jitter: one process,
/// one user, nothing to de-synchronize.
pub struct Retrying {
    inner: Arc<dyn ModelProvider>,
    /// Attempts per exchange, the first included.
    attempts: u32,
    /// The pause before the second attempt.
    base_delay: Duration,
    /// The longest pause any retry waits.
    max_delay: Duration,
    /// The pause between attempts, injected so runtimes without tokio's
    /// timers (the wasm hosts) supply their own.
    sleeper: Arc<dyn Sleeper>,
}

impl Retrying {
    /// The default policy: up to three attempts.
    pub fn new(inner: Arc<dyn ModelProvider>) -> Self {
        Self::with_policy(inner, 3, Duration::from_secs(1), Duration::from_secs(2))
    }

    /// An explicit policy, for tests: `attempts` counts the first try;
    /// the pause starts at `base_delay`, doubling per retry up to
    /// `max_delay`.
    pub fn with_policy(
        inner: Arc<dyn ModelProvider>,
        attempts: u32,
        base_delay: Duration,
        max_delay: Duration,
    ) -> Self {
        Self {
            inner,
            attempts,
            base_delay,
            max_delay,
            sleeper: Arc::new(TokioSleeper),
        }
    }

    /// [`with_policy`](Self::with_policy) with the pause supplied — the
    /// constructor for runtimes without tokio's timers. The policy is
    /// unchanged: the sleeper only waits the ladder out.
    pub fn policy_with_sleeper(
        inner: Arc<dyn ModelProvider>,
        attempts: u32,
        base_delay: Duration,
        max_delay: Duration,
        sleeper: Arc<dyn Sleeper>,
    ) -> Self {
        Self {
            inner,
            attempts,
            base_delay,
            max_delay,
            sleeper,
        }
    }

    /// Replaces the pause. The additive knob a wasm frontend turns to
    /// swap tokio's timers for its runtime's sleep.
    pub fn with_sleeper(mut self, sleeper: Arc<dyn Sleeper>) -> Self {
        self.sleeper = sleeper;
        self
    }

    /// The pause before retry number `retry`, zero-based.
    fn delay(&self, retry: u32) -> Duration {
        // The shift is bounded so a large `attempts` cannot overflow the
        // multiplication; the cap makes the exact bound irrelevant.
        let doubling = 1u32 << retry.min(16);
        self.base_delay.saturating_mul(doubling).min(self.max_delay)
    }
}

/// The pause between retries. A seam because runtimes differ: tokio's
/// timers do not exist in every host (workerd gives wasm no Rust-side
/// timers to fire), so a host injects its own — the same ladder, a
/// different sleep.
#[async_trait]
pub trait Sleeper: Send + Sync {
    /// Waits `duration` out.
    async fn sleep(&self, duration: Duration);
}

/// The tokio pause — today's behavior, the default sleeper.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioSleeper;

#[async_trait]
impl Sleeper for TokioSleeper {
    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

/// Counts the events reaching the real sink; the retry guards read the
/// count to know whether an attempt was seen.
struct CountingSink<'a> {
    inner: &'a mut dyn EventSink,
    events: usize,
}

impl EventSink for CountingSink<'_> {
    fn emit(&mut self, event: Event<'_>) {
        self.events += 1;
        self.inner.emit(event);
    }
}

#[async_trait]
impl ModelProvider for Retrying {
    async fn complete(
        &self,
        req: ModelRequest,
        sink: &mut dyn EventSink,
    ) -> Result<ModelOutput, ProviderError> {
        for attempt in 0..self.attempts {
            if attempt > 0 {
                self.sleeper.sleep(self.delay(attempt - 1)).await;
            }
            let mut counted = CountingSink {
                inner: &mut *sink,
                events: 0,
            };
            match self.inner.complete(req.clone(), &mut counted).await {
                // A response with content answers; an empty one before
                // the first event is a dead exchange, retried below.
                Ok(output) if counted.events > 0 || !output.message.content.is_empty() => {
                    return Ok(output);
                }
                Ok(_) => {}
                Err(err) => {
                    let last_attempt = attempt + 1 == self.attempts;
                    if !last_attempt && counted.events == 0 && err.is_retryable() {
                        continue;
                    }
                    return Err(err);
                }
            }
        }
        Err(ProviderError::Other(
            "provider closed the stream without content".to_owned(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhema_core::{Content, Message, Role, StopReason, Usage};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// One scripted attempt. The provider plays its steps in order and
    /// counts the calls, so a test can assert how many attempts a
    /// policy spent.
    struct Scripted {
        steps: Mutex<Vec<Attempt>>,
        calls: AtomicUsize,
    }

    #[derive(Clone, Copy)]
    enum Attempt {
        /// A transport timeout before any byte.
        Timeout,
        /// A client error: never retryable.
        Auth,
        /// A quota wall: never retryable, whatever the status family.
        RateLimit,
        /// One text delta, then a timeout mid-stream.
        DeltaThenTimeout,
        /// Success; the string is the whole reply ("" = empty).
        Succeed(&'static str),
    }

    impl Scripted {
        fn new(steps: Vec<Attempt>) -> Self {
            Self {
                steps: Mutex::new(steps),
                calls: AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl ModelProvider for Scripted {
        async fn complete(
            &self,
            _req: ModelRequest,
            sink: &mut dyn EventSink,
        ) -> Result<ModelOutput, ProviderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.steps.lock().unwrap().remove(0) {
                Attempt::Timeout => Err(ProviderError::Timeout),
                Attempt::Auth => Err(ProviderError::Api(
                    "anthropic api error (HTTP 401): bad key".into(),
                )),
                Attempt::RateLimit => Err(ProviderError::Api(
                    "anthropic api error (HTTP 429): rate_limit_error".into(),
                )),
                Attempt::DeltaThenTimeout => {
                    sink.emit(Event::TextDelta("partial "));
                    Err(ProviderError::Timeout)
                }
                Attempt::Succeed(text) => Ok(ModelOutput {
                    message: Message {
                        role: Role::Assistant,
                        content: if text.is_empty() {
                            Vec::new()
                        } else {
                            vec![Content::Text(text.to_owned())]
                        },
                    },
                    stop_reason: StopReason::EndTurn,
                    usage: Usage::default(),
                }),
            }
        }
    }

    /// The text deltas one exchange streamed, in order.
    #[derive(Default)]
    struct Deltas(Vec<String>);

    impl EventSink for Deltas {
        fn emit(&mut self, event: Event<'_>) {
            if let Event::TextDelta(text) = event {
                self.0.push(text.to_owned());
            }
        }
    }

    fn request() -> ModelRequest {
        ModelRequest {
            system: None,
            messages: Vec::new(),
            tools: Vec::new(),
            params: rhema_core::ChatParams::default(),
        }
    }

    fn retried(provider: Arc<Scripted>) -> Retrying {
        // No real waiting: the ladder's shape is a separate test.
        Retrying::with_policy(
            provider,
            3,
            Duration::from_millis(10),
            Duration::from_millis(20),
        )
    }

    #[tokio::test]
    async fn a_failure_before_any_byte_retries_and_answers() {
        let scripted = Arc::new(Scripted::new(vec![
            Attempt::Timeout,
            Attempt::Succeed("recovered"),
        ]));
        let mut sink = Deltas::default();
        let out = retried(scripted.clone())
            .complete(request(), &mut sink)
            .await
            .expect("the second attempt answers");
        assert_eq!(out.message.content, vec![Content::Text("recovered".into())]);
        assert_eq!(scripted.calls(), 2, "one retry spent");
        assert!(sink.0.is_empty(), "a retry re-issues, it never replays");
    }

    #[tokio::test]
    async fn a_failure_after_streamed_text_never_retries() {
        let scripted = Arc::new(Scripted::new(vec![Attempt::DeltaThenTimeout]));
        let mut sink = Deltas::default();
        let err = retried(scripted.clone())
            .complete(request(), &mut sink)
            .await
            .expect_err("a seen stream cannot be redone");
        assert_eq!(err, ProviderError::Timeout);
        assert_eq!(scripted.calls(), 1, "one attempt only");
        assert_eq!(sink.0, vec!["partial "], "the caller keeps what it saw");
    }

    #[tokio::test]
    async fn a_client_error_returns_at_once() {
        let scripted = Arc::new(Scripted::new(vec![Attempt::Auth]));
        let mut sink = Deltas::default();
        let result = retried(scripted.clone())
            .complete(request(), &mut sink)
            .await;
        assert!(result.is_err(), "a bad key survives no retry");
        assert_eq!(scripted.calls(), 1, "no attempts wasted");
    }

    #[tokio::test]
    async fn a_rate_limit_returns_at_once() {
        // The ladder waits seconds; a 5-hour usage limit does not clear
        // inside it. Retrying would spend a turn and its tokens for a
        // certain second 429, so the wall surfaces at once.
        let scripted = Arc::new(Scripted::new(vec![Attempt::RateLimit]));
        let mut sink = Deltas::default();
        let result = retried(scripted.clone())
            .complete(request(), &mut sink)
            .await;
        assert!(result.is_err(), "the quota wall surfaces immediately");
        assert_eq!(scripted.calls(), 1, "no attempts wasted on the window");
    }

    #[tokio::test]
    async fn an_empty_success_is_dead_and_retries() {
        let scripted = Arc::new(Scripted::new(vec![
            Attempt::Succeed(""),
            Attempt::Succeed("real answer"),
        ]));
        let mut sink = Deltas::default();
        let out = retried(scripted.clone())
            .complete(request(), &mut sink)
            .await
            .expect("the retry answers");
        assert_eq!(
            out.message.content,
            vec![Content::Text("real answer".into())]
        );
        assert_eq!(scripted.calls(), 2, "the empty exchange was redone");
    }

    #[tokio::test]
    async fn every_attempt_empty_fails_loudly() {
        let scripted = Arc::new(Scripted::new(vec![Attempt::Succeed(""); 3]));
        let mut sink = Deltas::default();
        let err = retried(scripted.clone())
            .complete(request(), &mut sink)
            .await
            .expect_err("an empty exchange cannot pass silently");
        assert!(
            err.to_string().contains("without content"),
            "names the failure: {err}"
        );
        assert_eq!(scripted.calls(), 3, "the policy ran out");
    }

    /// A sleeper that records the waits it was asked for and answers
    /// immediately: proves the ladder is consulted through the seam.
    struct RecordingShared(Arc<Mutex<Vec<Duration>>>);

    #[async_trait]
    impl Sleeper for RecordingShared {
        async fn sleep(&self, duration: Duration) {
            self.0.lock().unwrap().push(duration);
        }
    }

    #[tokio::test]
    async fn the_pause_rides_the_injected_sleeper() {
        let scripted = Arc::new(Scripted::new(vec![
            Attempt::Timeout,
            Attempt::Succeed("through the seam"),
        ]));
        let waits = Arc::new(Mutex::new(Vec::new()));
        let sleeper = Arc::new(RecordingShared(waits.clone()));
        let retrying = Retrying::policy_with_sleeper(
            scripted.clone(),
            3,
            Duration::from_millis(40),
            Duration::from_millis(80),
            sleeper,
        );
        let mut sink = Deltas::default();
        retrying
            .complete(request(), &mut sink)
            .await
            .expect("the second attempt answers");
        assert_eq!(*waits.lock().unwrap(), vec![Duration::from_millis(40)]);
    }

    #[tokio::test(start_paused = true)]
    async fn the_backoff_doubles_and_caps() {
        let scripted = Arc::new(Scripted::new(vec![
            Attempt::Timeout,
            Attempt::Timeout,
            Attempt::Timeout,
            Attempt::Succeed("fourth time"),
        ]));
        // Ladder: 10ms, 20ms, then 20ms again — the doubling would
        // reach 40ms, and the cap holds it.
        let retrying = Retrying::with_policy(
            scripted.clone(),
            4,
            Duration::from_millis(10),
            Duration::from_millis(20),
        );
        let mut sink = Deltas::default();
        let started = tokio::time::Instant::now();
        retrying
            .complete(request(), &mut sink)
            .await
            .expect("the last attempt answers");
        assert_eq!(started.elapsed(), Duration::from_millis(50));
        assert_eq!(scripted.calls(), 4);
    }
}
