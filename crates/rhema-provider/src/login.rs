//! The interactive login flow seam: how an adapter walks a user through
//! an OAuth-style handshake (device codes, browser round-trips) without
//! any frontend knowing the protocol.
//!
//! An adapter declares a flow by implementing [`InteractiveLogin`]; the
//! frontend renders the [`LoginUpdate`]s it reports and persists the
//! resulting credential. Everything protocol-specific — codes, polling
//! intervals, expiry — stays inside the flow; the credential comes out
//! as one opaque string, the same shape a static API key takes, so it
//! stores in a profile field like any other key. A flow needing a
//! richer credential than one string is a later decision, not an
//! extension here.

/// One thing the user must do outside the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginInstruction {
    /// The instruction, rendered as-is: what to do, where.
    pub text: String,
    /// The URL to open, when the flow has one.
    pub url: Option<String>,
    /// The short code to enter there, when the flow uses one.
    pub code: Option<String>,
}

/// One step of a running login flow. `Completed` and `Failed` are the
/// terminal updates: a well-formed flow emits exactly one before its
/// [`InteractiveLogin::run`] returns, and nothing after.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginUpdate {
    /// Show the user an [`LoginInstruction`].
    Instruct(LoginInstruction),
    /// Still waiting on the user or the remote; the text renders as a
    /// progress note.
    Waiting(String),
    /// The flow succeeded; the string is the credential to persist.
    Completed(String),
    /// The flow gave up; the text is why, rendered inline for a retry.
    Failed(String),
}

/// Where a flow reports its updates. The frontend implements this over
/// its event channel; the flow calls it from [`InteractiveLogin::run`].
pub trait LoginSink: Send {
    /// Reports one step.
    fn update(&mut self, update: LoginUpdate);
}

/// A login flow one adapter declares. Cancellation is dropping the task
/// `run` runs in — the flow needs no cancel API, and a dropped flow
/// leaves no state behind that the next attempt would trip over.
#[async_trait::async_trait]
pub trait InteractiveLogin: Send + Sync {
    /// Runs the flow to its terminal update, reporting each step to
    /// `sink` as it happens.
    async fn run(&self, sink: &mut dyn LoginSink);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sink that records the update order a flow reports.
    #[derive(Default)]
    struct Recording {
        updates: Vec<LoginUpdate>,
    }

    impl LoginSink for Recording {
        fn update(&mut self, update: LoginUpdate) {
            self.updates.push(update);
        }
    }

    /// A flow scripted step by step: the terminal update is whatever the
    /// script ends with, and the contract under test is that the sink
    /// saw exactly that sequence.
    struct Scripted {
        steps: Vec<LoginUpdate>,
    }

    #[async_trait::async_trait]
    impl InteractiveLogin for Scripted {
        async fn run(&self, sink: &mut dyn LoginSink) {
            for step in &self.steps {
                sink.update(step.clone());
            }
        }
    }

    #[tokio::test]
    async fn the_sink_sees_the_steps_in_the_order_the_flow_reports_them() {
        let flow = Scripted {
            steps: vec![
                LoginUpdate::Instruct(LoginInstruction {
                    text: "open the page and enter the code".to_owned(),
                    url: Some("https://example.com/device".to_owned()),
                    code: Some("ABCD-1234".to_owned()),
                }),
                LoginUpdate::Waiting("waiting for authorization".to_owned()),
                LoginUpdate::Completed("the credential".to_owned()),
            ],
        };
        let mut sink = Recording::default();
        flow.run(&mut sink).await;
        assert_eq!(
            sink.updates,
            vec![
                LoginUpdate::Instruct(LoginInstruction {
                    text: "open the page and enter the code".to_owned(),
                    url: Some("https://example.com/device".to_owned()),
                    code: Some("ABCD-1234".to_owned()),
                }),
                LoginUpdate::Waiting("waiting for authorization".to_owned()),
                LoginUpdate::Completed("the credential".to_owned()),
            ]
        );
    }

    #[tokio::test]
    async fn a_failed_flow_reports_why_through_the_same_sink() {
        let flow = Scripted {
            steps: vec![LoginUpdate::Failed("the device code expired".to_owned())],
        };
        let mut sink = Recording::default();
        flow.run(&mut sink).await;
        assert_eq!(
            sink.updates,
            vec![LoginUpdate::Failed("the device code expired".to_owned())]
        );
    }
}
