//! Automatic retries for transient model failures.
//!
//! [`Retrying`] wraps any [`ModelProvider`]: a request that failed with a
//! retryable error — a transport timeout, a network failure, or a
//! server-side status (see [`ProviderError::is_retryable`]) — is
//! re-issued with backoff from a bounded ladder. Every turn provider the
//! frontends assemble rides one, so a provider hiccup costs a pause
//! instead of the whole turn.
//!
//! The ladder follows the failure. A dead link
//! ([`ProviderError::Network`]) is the local machine's problem, and
//! waiting long can fix it: the ladder runs from a second to five
//! minutes. A struggling server fixes itself in seconds or not at all
//! inside a turn, and every timeout attempt also burns the request's
//! own timeout, so that ladder stays short. Each wait is announced to
//! the sink as an [`Event::RetryWait`] before it runs, so a UI can
//! count it down.
//!
//! Two guards keep a retry honest:
//!
//! - A request whose stream already emitted content never retries. The
//!   deltas reached the caller's sink, and a second attempt would stream
//!   them again next to the first. A usage tally alone is not content —
//!   the adapters report the prompt ahead of the first delta, and a
//!   stream that dies between them is invisible enough to redo.
//! - A request that "succeeds" without producing anything — a provider
//!   that closed the stream before its first chunk — counts as dead like
//!   any error and retries. If every attempt comes back empty, the
//!   exchange fails naming that fact, rather than completing a turn
//!   nobody saw.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rhema_core::{Event, EventSink, ModelOutput, ModelProvider, ModelRequest, ProviderError};

/// The waits before each retry of a dead network link: two quick tries
/// catch an instant recovery nearly free, and the tail — six and a half
/// minutes all told, six attempts — rides out a reassociation or a real
/// outage. The exchange fails when the ladder runs out.
const DEFAULT_NETWORK_LADDER: &[Duration] = &[
    Duration::from_secs(1),
    Duration::from_secs(3),
    Duration::from_secs(30),
    Duration::from_secs(60),
    Duration::from_secs(300),
];

/// The waits before each retry of a struggling server: a transport
/// timeout, a 408 or 5xx status, an exchange that closed before any
/// content. Three attempts, three seconds all told.
const DEFAULT_SERVER_LADDER: &[Duration] = &[Duration::from_secs(1), Duration::from_secs(2)];

/// Re-issues failed model requests with backoff from a per-failure
/// ladder.
///
/// No jitter: one process, one user, nothing to de-synchronize.
pub struct Retrying {
    inner: Arc<dyn ModelProvider>,
    /// Waits before each retry of a dead link; the first entry follows
    /// the second attempt. Its length sets the attempt count.
    network_ladder: Vec<Duration>,
    /// Waits before each retry of a struggling server, and of an
    /// exchange that came back empty. Its length sets the attempt
    /// count.
    server_ladder: Vec<Duration>,
    /// The pause between attempts, injected so runtimes without tokio's
    /// timers (the wasm hosts) supply their own.
    sleeper: Arc<dyn Sleeper>,
}

impl std::fmt::Debug for Retrying {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The inner provider and sleeper are dyn seams: they print as
        // placeholders, the policy prints in full.
        f.debug_struct("Retrying")
            .field("network_ladder", &self.network_ladder)
            .field("server_ladder", &self.server_ladder)
            .finish_non_exhaustive()
    }
}

impl Retrying {
    /// The default policy: the network ladder rides out a real outage,
    /// the server ladder spends three quick tries.
    pub fn new(inner: Arc<dyn ModelProvider>) -> Self {
        Self::with_ladders(
            inner,
            DEFAULT_NETWORK_LADDER.to_vec(),
            DEFAULT_SERVER_LADDER.to_vec(),
        )
    }

    /// An explicit policy: `network_ladder` holds the waits between the
    /// retries of a dead link, `server_ladder` those of a struggling
    /// server and of an exchange that came back empty. Each ladder's
    /// length sets its attempt count — the exchange always runs once,
    /// and an empty ladder means no retries — so the policy can never
    /// silently do nothing.
    pub fn with_ladders(
        inner: Arc<dyn ModelProvider>,
        network_ladder: Vec<Duration>,
        server_ladder: Vec<Duration>,
    ) -> Self {
        Self {
            inner,
            network_ladder,
            server_ladder,
            sleeper: Arc::new(TokioSleeper),
        }
    }

    /// Replaces the pause. The additive knob a wasm frontend turns to
    /// swap tokio's timers for its runtime's sleep.
    pub fn with_sleeper(mut self, sleeper: Arc<dyn Sleeper>) -> Self {
        self.sleeper = sleeper;
        self
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

/// Counts the content events reaching the real sink; the retry guards
/// read the count to know whether an attempt was seen. Content is every
/// event but the usage tally — the adapters emit `Usage` ahead of the
/// first delta, and a caller that watched only a tally watched nothing.
struct CountingSink<'a> {
    inner: &'a mut dyn EventSink,
    content: usize,
}

impl EventSink for CountingSink<'_> {
    fn emit(&mut self, event: Event<'_>) {
        // Usage and the retry announcement are tallies, not generation:
        // neither is content the caller saw, so neither blocks a retry.
        if !matches!(event, Event::Usage(_) | Event::RetryWait { .. }) {
            self.content += 1;
        }
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
        // Attempts spent, the first included. Each ladder indexes by it:
        // entry `spent` is the wait that follows attempt `spent + 1`
        // failing, and a miss means the policy is out of tries — so the
        // ladder's length, not a separate count, bounds the exchange.
        let mut spent = 0u32;
        loop {
            let mut counted = CountingSink {
                inner: &mut *sink,
                content: 0,
            };
            match self.inner.complete(req.clone(), &mut counted).await {
                // A response with content answers; an empty one the
                // caller never saw content from is a dead exchange,
                // retried below.
                Ok(output) if counted.content > 0 || !output.message.content.is_empty() => {
                    return Ok(output);
                }
                // An empty success carries no error to classify, so it
                // rides the server ladder: a provider that closes its
                // own stream early is a server-side oddity, not a dead
                // local link.
                Ok(_) => {
                    if let Some(&delay) = self.server_ladder.get(spent as usize) {
                        sink.emit(Event::RetryWait {
                            error: "the provider closed the stream without content",
                            delay,
                            attempt: spent + 1,
                            total: self.server_ladder.len() as u32 + 1,
                        });
                        self.sleeper.sleep(delay).await;
                        spent += 1;
                        continue;
                    }
                    return Err(ProviderError::Other(
                        "provider closed the stream without content".to_owned(),
                    ));
                }
                Err(err) => {
                    if counted.content == 0 && err.is_retryable() {
                        let ladder = if matches!(err, ProviderError::Network(_)) {
                            &self.network_ladder
                        } else {
                            &self.server_ladder
                        };
                        if let Some(&delay) = ladder.get(spent as usize) {
                            let message = err.to_string();
                            sink.emit(Event::RetryWait {
                                error: &message,
                                delay,
                                attempt: spent + 1,
                                total: ladder.len() as u32 + 1,
                            });
                            self.sleeper.sleep(delay).await;
                            spent += 1;
                            continue;
                        }
                    }
                    return Err(err);
                }
            }
        }
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
        /// The link itself died before any byte.
        Network,
        /// A client error: never retryable.
        Auth,
        /// A quota wall: never retryable, whatever the status family.
        RateLimit,
        /// One text delta, then a timeout mid-stream.
        DeltaThenTimeout,
        /// The prompt's usage tally, then a timeout before any content.
        UsageThenTimeout,
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
                Attempt::Network => Err(ProviderError::Network("dead link".into())),
                Attempt::Auth => Err(ProviderError::Api {
                    message: "anthropic api error (HTTP 401): bad key".into(),
                    status: Some(401),
                }),
                Attempt::RateLimit => Err(ProviderError::Api {
                    message: "anthropic api error (HTTP 429): rate_limit_error".into(),
                    status: Some(429),
                }),
                Attempt::DeltaThenTimeout => {
                    sink.emit(Event::TextDelta("partial "));
                    Err(ProviderError::Timeout)
                }
                Attempt::UsageThenTimeout => {
                    // The adapters report the prompt this early; a
                    // caller that watched only the tally watched no
                    // content.
                    sink.emit(Event::Usage(Usage {
                        input_tokens: 10,
                        output_tokens: 1,
                        cached_input_tokens: 0,
                    }));
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
        Retrying::with_ladders(
            provider,
            vec![Duration::from_millis(10), Duration::from_millis(20)],
            vec![Duration::from_millis(10), Duration::from_millis(20)],
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
    async fn a_usage_tally_alone_does_not_block_the_retry() {
        // The tally the footer gauge reads is not content: a stream
        // that dies right after message_start is redone, because the
        // caller saw no generation from it.
        let scripted = Arc::new(Scripted::new(vec![
            Attempt::UsageThenTimeout,
            Attempt::Succeed("recovered"),
        ]));
        let mut sink = Deltas::default();
        let out = retried(scripted.clone())
            .complete(request(), &mut sink)
            .await
            .expect("the second attempt answers");
        assert_eq!(out.message.content, vec![Content::Text("recovered".into())]);
        assert_eq!(scripted.calls(), 2, "one retry spent");
        assert!(sink.0.is_empty(), "no text was ever streamed");
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
        let retrying = Retrying::with_ladders(
            scripted.clone(),
            vec![Duration::from_millis(40), Duration::from_millis(80)],
            vec![Duration::from_millis(40), Duration::from_millis(80)],
        )
        .with_sleeper(sleeper);
        let mut sink = Deltas::default();
        retrying
            .complete(request(), &mut sink)
            .await
            .expect("the second attempt answers");
        assert_eq!(*waits.lock().unwrap(), vec![Duration::from_millis(40)]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_dead_link_walks_the_whole_network_ladder() {
        let scripted = Arc::new(Scripted::new(vec![
            Attempt::Network,
            Attempt::Network,
            Attempt::Network,
            Attempt::Succeed("back"),
        ]));
        // The waits sum to 70ms — the network ladder ran to its end and
        // the fourth attempt answered. The server ladder here would
        // have given up one attempt earlier.
        let retrying = Retrying::with_ladders(
            scripted.clone(),
            vec![
                Duration::from_millis(10),
                Duration::from_millis(20),
                Duration::from_millis(40),
            ],
            vec![Duration::from_millis(10), Duration::from_millis(20)],
        );
        let mut sink = Deltas::default();
        let started = tokio::time::Instant::now();
        let out = retrying
            .complete(request(), &mut sink)
            .await
            .expect("the last attempt answers");
        assert_eq!(started.elapsed(), Duration::from_millis(70));
        assert_eq!(scripted.calls(), 4);
        assert_eq!(out.message.content, vec![Content::Text("back".into())]);
    }

    #[tokio::test]
    async fn a_struggling_server_gets_the_short_ladder() {
        // Three timeouts burn the server ladder out — two waits, three
        // attempts — where the same failures on a dead link would have
        // had one more try coming.
        let scripted = Arc::new(Scripted::new(vec![Attempt::Timeout; 3]));
        let retrying = Retrying::with_ladders(
            scripted.clone(),
            vec![
                Duration::from_millis(10),
                Duration::from_millis(20),
                Duration::from_millis(40),
            ],
            vec![Duration::from_millis(10), Duration::from_millis(20)],
        );
        let mut sink = Deltas::default();
        let err = retrying
            .complete(request(), &mut sink)
            .await
            .expect_err("the short ladder runs out");
        assert_eq!(err, ProviderError::Timeout);
        assert_eq!(scripted.calls(), 3);
    }

    /// Records the retry announcements as they cross to the caller.
    #[derive(Default)]
    struct Announcements(Vec<(String, Duration, u32, u32)>);

    impl EventSink for Announcements {
        fn emit(&mut self, event: Event<'_>) {
            if let Event::RetryWait {
                error,
                delay,
                attempt,
                total,
            } = event
            {
                self.0.push((error.to_owned(), delay, attempt, total));
            }
        }
    }

    #[tokio::test]
    async fn the_wait_is_announced_before_it_runs() {
        let scripted = Arc::new(Scripted::new(vec![
            Attempt::Network,
            Attempt::Succeed("recovered"),
        ]));
        let sink = Announcements::default();
        let retrying = Retrying::with_ladders(
            scripted.clone(),
            vec![Duration::from_secs(3), Duration::from_secs(30)],
            vec![Duration::from_secs(1)],
        );
        let mut sink = sink;
        retrying
            .complete(request(), &mut sink)
            .await
            .expect("the second attempt answers");
        assert_eq!(
            sink.0,
            vec![(
                "network error: dead link".to_owned(),
                Duration::from_secs(3),
                1,
                3,
            )]
        );
        // Two calls: the announcement passed through the counting sink
        // without counting as content, or the retry never would have
        // run.
        assert_eq!(scripted.calls(), 2);
    }
}
