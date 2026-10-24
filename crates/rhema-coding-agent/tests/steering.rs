//! The steered turn loop: notifications appended at the two drain
//! points, deduplicated within a turn, and the no-steering shape
//! untouched.

#![cfg(feature = "mock")]

use std::sync::Arc;
use std::sync::Mutex;

use rhema_coding_agent::{AllowAll, CodingAgent, CodingAgentConfig, Steering};
use rhema_core::{
    Content, Event, EventSink, Message, ModelOutput, ModelProvider, ModelRequest, ProviderError,
    Role, StopReason, ToolCall, Usage, async_trait,
};
use serde_json::json;

struct NullSink;

impl EventSink for NullSink {
    fn emit(&mut self, _event: Event<'_>) {}
}

fn tool_call(id: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "bash".into(),
        args,
        provider_data: serde_json::Value::Null,
    }
}

fn tool_use_output(call: ToolCall) -> ModelOutput {
    ModelOutput {
        message: Message {
            role: Role::Assistant,
            content: vec![Content::ToolCall(call)],
        },
        stop_reason: StopReason::ToolUse,
        usage: Usage::default(),
    }
}

fn end_turn_output(text: &str) -> ModelOutput {
    ModelOutput {
        message: Message::text(Role::Assistant, text),
        stop_reason: StopReason::EndTurn,
        usage: Usage::default(),
    }
}

/// A steering source serving queued batches in order; an exhausted
/// queue answers empty. Records every drain for the assertions.
struct Scripted(Mutex<Vec<Vec<String>>>);

impl Scripted {
    fn new(batches: Vec<Vec<String>>) -> Arc<Self> {
        Arc::new(Self(Mutex::new(batches)))
    }

    fn drained(&self) -> usize {
        self.0.lock().unwrap().len()
    }
}

impl Steering for Scripted {
    fn drain(&self) -> Vec<String> {
        let mut batches = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if batches.is_empty() {
            Vec::new()
        } else {
            batches.remove(0)
        }
    }
}

/// A steering source that answers the same line forever — a job the
/// model never fetched, which the driver must still announce only once
/// per turn.
struct Stubborn(String);

impl Steering for Stubborn {
    fn drain(&self) -> Vec<String> {
        vec![self.0.clone()]
    }
}

/// Records every request's message list, then ends the turn (or calls
/// the tool `rounds` times first).
struct Recorder {
    rounds: usize,
    requests: Mutex<Vec<Vec<Message>>>,
}

impl Recorder {
    fn new(rounds: usize) -> Arc<Self> {
        Arc::new(Self {
            rounds,
            requests: Mutex::new(Vec::new()),
        })
    }

    fn requests(&self) -> Vec<Vec<Message>> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl ModelProvider for Recorder {
    async fn complete(
        &self,
        req: ModelRequest,
        _sink: &mut dyn EventSink,
    ) -> Result<ModelOutput, ProviderError> {
        let mut requests = self
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let round = requests.len();
        requests.push(req.messages);
        if round < self.rounds {
            Ok(tool_use_output(tool_call(
                &format!("c{round}"),
                json!({ "command": "true" }),
            )))
        } else {
            Ok(end_turn_output("done"))
        }
    }
}

/// The text of every User-role message in `messages`.
fn user_texts(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter(|m| m.role == Role::User)
        .flat_map(|m| {
            m.content.iter().filter_map(|c| match c {
                Content::Text(text) => Some(text.clone()),
                _ => None,
            })
        })
        .collect()
}

#[tokio::test]
async fn a_turn_start_announcement_lands_before_the_user_input() {
    let recorder = Recorder::new(0);
    let steering = Scripted::new(vec![vec!["Subagent scout (job-3) has finished.".into()]]);
    let config = CodingAgentConfig {
        steering: Some(steering.clone()),
        ..CodingAgentConfig::new("m", std::env::temp_dir())
    };
    let mut agent = CodingAgent::new(recorder.clone(), config).unwrap();
    agent
        .turn("run it", &mut NullSink, Arc::new(AllowAll))
        .await
        .unwrap();

    let requests = recorder.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        user_texts(&requests[0]),
        vec![
            "Subagent scout (job-3) has finished.".to_owned(),
            "run it".to_owned(),
        ],
        "the notification precedes the user's input; consecutive User messages are valid"
    );
}

#[tokio::test]
async fn a_mid_turn_announcement_lands_after_the_tool_round() {
    let recorder = Recorder::new(1);
    let notification = "Subagent scout (job-3) has finished.";
    let steering = Scripted::new(vec![vec![], vec![notification.to_owned()], vec![], vec![]]);
    let config = CodingAgentConfig {
        steering: Some(steering.clone()),
        ..CodingAgentConfig::new("m", std::env::temp_dir())
    };
    let mut agent = CodingAgent::new(recorder.clone(), config).unwrap();
    agent
        .turn("run it", &mut NullSink, Arc::new(AllowAll))
        .await
        .unwrap();

    let requests = recorder.requests();
    assert_eq!(requests.len(), 2);
    // Turn start: nothing to announce, only the input.
    assert_eq!(user_texts(&requests[0]), vec!["run it".to_owned()]);
    // After the tool round: the notification, then the model decides.
    let second = user_texts(&requests[1]);
    assert_eq!(second.first().map(String::as_str), Some("run it"));
    assert_eq!(second.last().map(String::as_str), Some(notification));
    assert_eq!(second.len(), 2, "one announcement message: {:?}", second);
    // The drain ran at exactly the two points that consumed a batch:
    // turn start and after the tool round — the turn's last step is an
    // end-turn, which drains no further.
    assert_eq!(steering.drained(), 2);
}

#[tokio::test]
async fn an_unfetched_finish_is_announced_once_per_turn() {
    let recorder = Recorder::new(2);
    let steering = Arc::new(Stubborn("Subagent scout (job-3) has finished.".into()));
    let config = CodingAgentConfig {
        steering: Some(steering.clone()),
        ..CodingAgentConfig::new("m", std::env::temp_dir())
    };
    let mut agent = CodingAgent::new(recorder.clone(), config).unwrap();
    agent
        .turn("run it", &mut NullSink, Arc::new(AllowAll))
        .await
        .unwrap();

    let requests = recorder.requests();
    assert_eq!(requests.len(), 3);
    for (round, messages) in requests.iter().enumerate() {
        assert_eq!(
            user_texts(messages)
                .iter()
                .filter(|t| t.contains("Subagent"))
                .count(),
            1,
            "round {round}: announced exactly once, whatever the drain repeats"
        );
    }
}

#[tokio::test]
async fn several_finishes_coalesce_into_one_message() {
    let recorder = Recorder::new(0);
    let steering = Scripted::new(vec![vec![
        "Subagent scout (job-3) has finished.".into(),
        "Subagent reviewer (job-4) has finished with an error: hit its step budget of 256.".into(),
    ]]);
    let config = CodingAgentConfig {
        steering: Some(steering.clone()),
        ..CodingAgentConfig::new("m", std::env::temp_dir())
    };
    let mut agent = CodingAgent::new(recorder.clone(), config).unwrap();
    agent
        .turn("run it", &mut NullSink, Arc::new(AllowAll))
        .await
        .unwrap();

    let requests = recorder.requests();
    let users = user_texts(&requests[0]);
    assert_eq!(users.len(), 2, "the input plus one announcement message");
    assert!(users[0].contains("scout (job-3)") && users[0].contains("reviewer (job-4)"));
    assert_eq!(users[0].lines().count(), 2, "one line per finished job");
}

#[tokio::test]
async fn an_empty_drain_appends_nothing() {
    let recorder = Recorder::new(0);
    let steering = Scripted::new(vec![vec![], vec![]]);
    let config = CodingAgentConfig {
        steering: Some(steering.clone()),
        ..CodingAgentConfig::new("m", std::env::temp_dir())
    };
    let mut agent = CodingAgent::new(recorder.clone(), config).unwrap();
    agent
        .turn("run it", &mut NullSink, Arc::new(AllowAll))
        .await
        .unwrap();

    let requests = recorder.requests();
    assert_eq!(
        user_texts(&requests[0]),
        vec!["run it".to_owned()],
        "no empty User message for an empty drain"
    );
}

#[tokio::test]
async fn a_separate_turn_reannounces_the_same_unfetched_line() {
    let recorder = Recorder::new(0);
    let steering = Arc::new(Stubborn("Subagent scout (job-3) has finished.".into()));
    let config = CodingAgentConfig {
        steering: Some(steering.clone()),
        ..CodingAgentConfig::new("m", std::env::temp_dir())
    };
    let mut agent = CodingAgent::new(recorder.clone(), config).unwrap();
    agent
        .turn("first", &mut NullSink, Arc::new(AllowAll))
        .await
        .unwrap();
    agent
        .turn("second", &mut NullSink, Arc::new(AllowAll))
        .await
        .unwrap();

    let requests = recorder.requests();
    assert_eq!(requests.len(), 2);
    // The request carries the whole history, so the tail is what the
    // second turn appended: the re-announcement before its input — the
    // dedup does not carry across turns while the report is unfetched.
    let texts = user_texts(&requests[1]);
    let tail: Vec<String> = texts[texts.len() - 2..].to_vec();
    assert_eq!(
        tail,
        vec![
            "Subagent scout (job-3) has finished.".to_owned(),
            "second".to_owned(),
        ]
    );
}

#[tokio::test]
async fn without_steering_the_history_keeps_its_plain_shape() {
    let recorder = Recorder::new(1);
    let mut agent = CodingAgent::new(
        recorder.clone(),
        CodingAgentConfig::new("m", std::env::temp_dir()),
    )
    .unwrap();
    agent
        .turn("run it", &mut NullSink, Arc::new(AllowAll))
        .await
        .unwrap();

    let requests = recorder.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(user_texts(&requests[0]), vec!["run it".to_owned()]);
    let roles: Vec<_> = agent.session().history.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        [Role::User, Role::Assistant, Role::Tool, Role::Assistant]
    );
}
