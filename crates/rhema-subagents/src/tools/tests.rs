use super::*;

use rhema_core::{ModelOutput, ProviderError};

#[test]
fn a_oversized_report_keeps_its_tail() {
    let head = "h".repeat(200);
    let tail = "t".repeat(REPORT_LIMIT);
    let out = capped(format!("{head}{tail}"));
    assert!(out.starts_with("[...truncated...]\n"));
    assert!(out.ends_with(&tail));
    assert!(out.len() < head.len() + tail.len());
}

#[test]
fn a_within_limit_report_passes_through() {
    assert_eq!(capped("short".to_owned()), "short");
}

#[test]
fn the_spec_carries_the_catalog_and_the_protocol() {
    let defs = vec![
        SubagentDef {
            name: "reviewer".into(),
            description: "reviews diffs".into(),
            model: None,
            max_steps: None,
            body: "Review.".into(),
        },
        SubagentDef {
            name: "scout".into(),
            description: "explores the tree".into(),
            model: None,
            max_steps: None,
            body: "Explore.".into(),
        },
    ];
    let spec = SubagentTools::spec(&defs, &[]);
    assert_eq!(spec.name, "subagent");
    assert!(spec.description.contains("reviewer: reviews diffs"));
    assert!(spec.description.contains("scout: explores the tree"));
    // The protocol: start, notification, fetch, cache.
    assert!(
        spec.description.contains("background"),
        "{}",
        spec.description
    );
    assert!(spec.description.contains("notification"));
    assert!(spec.description.contains("job id"));
    assert!(spec.description.contains("never a re-run"));
    // The schema carries the optional id beside name and prompt.
    let properties = spec.parameters["properties"].as_object().unwrap();
    assert!(properties.contains_key("name"));
    assert!(properties.contains_key("prompt"));
    assert!(properties.contains_key("id"));
    // The catalog also rides the schema as an enum, so a provider
    // that validates enums refuses a typo before the call.
    assert_eq!(
        spec.parameters["properties"]["name"]["enum"],
        json!(["reviewer", "scout"])
    );
    // The two shapes are exclusive; the schema subset cannot say
    // `oneOf`, so the id field says it.
    assert!(
        spec.parameters["properties"]["id"]["description"]
            .as_str()
            .unwrap()
            .contains("never beside"),
        "{}",
        spec.parameters["properties"]["id"]
    );
}

/// The spec teaches where a run rides: the provider list rides the
/// description and the schema, an empty list reading honestly.
#[test]
fn the_spec_teaches_the_model_argument() {
    let defs = vec![SubagentDef {
        name: "scout".into(),
        description: "explores".into(),
        model: None,
        max_steps: None,
        body: "Explore.".into(),
    }];
    let spec = SubagentTools::spec(&defs, &["google", "anthropic"]);
    assert!(
        spec.description
            .contains("available here: google, anthropic"),
        "{}",
        spec.description
    );
    assert!(spec.description.contains("<provider>/<model>"));
    let properties = spec.parameters["properties"].as_object().unwrap();
    assert!(properties.contains_key("model"));

    // No factory, no providers named: the session's own is the
    // only one, and the spec says so instead of staying silent.
    let spec = SubagentTools::spec(&defs, &[]);
    assert!(
        spec.description
            .contains("No other provider is available here"),
        "{}",
        spec.description
    );
    assert!(!spec.description.contains("available here:"));
}

/// Where a run rides: call model over frontmatter over the
/// session's own, the `<provider>/<model>` split, and the honest
/// failure when the kind has no credentials.
#[test]
fn model_resolution_walks_the_precedence_ladder() {
    let def_with_model = SubagentDef {
        name: "reviewer".into(),
        description: "reviews".into(),
        model: Some("haiku-on-parent".into()),
        max_steps: None,
        body: "Review.".into(),
    };
    let plain = SubagentDef {
        name: "scout".into(),
        description: "explores".into(),
        model: None,
        max_steps: None,
        body: "Explore.".into(),
    };
    let parent = Recording::new();
    let other = Recording::new();
    let factory: SubagentProviders = {
        let other = other.clone();
        Arc::new(move |kind: &str| (kind == "anthropic").then(|| other.clone() as _))
    };
    let tools = SubagentTools::new(
        parent.clone() as Arc<dyn ModelProvider>,
        vec![plain.clone(), def_with_model.clone()],
        Arc::new(ShellTools::new("/workspace")),
        "parent-model",
    )
    .with_providers(factory);

    // No argument, no frontmatter: the parent's.
    let ride = tools.inner.resolve_model(None, &plain).unwrap();
    assert_eq!(ride.model, "parent-model");
    assert!(Arc::ptr_eq(
        &(ride.provider as Arc<dyn ModelProvider>),
        &(parent.clone() as Arc<dyn ModelProvider>)
    ));

    // Frontmatter only: the definition's model, on the parent.
    let ride = tools.inner.resolve_model(None, &def_with_model).unwrap();
    assert_eq!(ride.model, "haiku-on-parent");
    assert!(Arc::ptr_eq(
        &(ride.provider as Arc<dyn ModelProvider>),
        &(parent.clone() as Arc<dyn ModelProvider>)
    ));

    // The call's argument wins over the frontmatter.
    let ride = tools
        .inner
        .resolve_model(Some("sonnet-on-parent"), &def_with_model)
        .unwrap();
    assert_eq!(ride.model, "sonnet-on-parent");

    // A kind the factory resolves moves the provider, and only the
    // tail after the first slash is the model id.
    let ride = tools
        .inner
        .resolve_model(Some("anthropic/claude-haiku"), &plain)
        .unwrap();
    assert_eq!(ride.model, "claude-haiku");
    assert!(Arc::ptr_eq(
        &(ride.provider as Arc<dyn ModelProvider>),
        &(other.clone() as Arc<dyn ModelProvider>)
    ));

    // A first component naming no kind stays whole: an
    // OpenRouter-style id rides the parent's provider.
    let ride = tools
        .inner
        .resolve_model(Some("vendor/model-name"), &plain)
        .unwrap();
    assert_eq!(ride.model, "vendor/model-name");

    // A known kind without credentials fails the call, naming what
    // is available, never a silent ride on the session's own.
    let why = expect_resolution_error(&tools.inner, Some("openrouter/mistral-large"), &plain);
    assert!(why.contains("openrouter"), "{why}");
    assert!(why.contains("anthropic"), "{why}");

    // A kind spelled with no model after the slash names nothing.
    let why = expect_resolution_error(&tools.inner, Some("anthropic/"), &plain);
    assert!(why.contains("no model"), "{why}");

    // Without a factory installed, the same spellings fail the
    // same way.
    let bare = SubagentTools::new(
        parent.clone() as Arc<dyn ModelProvider>,
        vec![plain],
        Arc::new(ShellTools::new("/workspace")),
        "parent-model",
    );
    let why = expect_resolution_error(&bare.inner, Some("anthropic/claude-haiku"), &def_with_model);
    assert!(why.contains("no usable credentials"), "{why}");
}

/// The failure a resolution must carry, asserted to be a failure
/// first, `Result`'s `Ok` half holds no `Debug` (a dyn provider),
/// so `unwrap_err` cannot.
fn expect_resolution_error(inner: &Inner, model: Option<&str>, def: &SubagentDef) -> String {
    match inner.resolve_model(model, def) {
        Ok(_) => panic!("expected the resolution to fail"),
        Err(why) => why,
    }
}

/// A recording provider: the model id each request carried, for
/// asserting where a run actually rode.
struct Recording {
    models: std::sync::Mutex<Vec<String>>,
    /// The conversation keys the run's requests carried, in call
    /// order; every entry but the first is a tool-call step.
    session_keys: std::sync::Mutex<Vec<Option<String>>>,
}

impl Recording {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            models: std::sync::Mutex::new(Vec::new()),
            session_keys: std::sync::Mutex::new(Vec::new()),
        })
    }
    fn taken(&self) -> Vec<String> {
        self.models
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    fn keys(&self) -> Vec<Option<String>> {
        self.session_keys
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[async_trait::async_trait]
impl ModelProvider for Recording {
    async fn complete(
        &self,
        req: rhema_core::ModelRequest,
        _sink: &mut dyn EventSink,
    ) -> Result<ModelOutput, ProviderError> {
        self.models
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(req.params.model);
        self.session_keys
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(req.params.session_id.clone());
        Ok(ModelOutput {
            message: Message::text(Role::Assistant, "the report"),
            stop_reason: rhema_core::StopReason::EndTurn,
            usage: rhema_core::Usage::default(),
        })
    }
}

/// The argument reaches the wire: a synchronous call with
/// `<provider>/<model>` runs its first request on the other
/// provider with the tail as the model id, and a bare id rides the
/// session's provider.
#[tokio::test]
async fn a_model_argument_moves_where_the_run_rides() {
    let session = Recording::new();
    let other = Recording::new();
    let factory: SubagentProviders = {
        let other = other.clone();
        Arc::new(move |kind: &str| (kind == "anthropic").then(|| other.clone() as _))
    };
    let def = SubagentDef {
        name: "scout".into(),
        description: "explores".into(),
        model: None,
        max_steps: None,
        body: "Explore.".into(),
    };
    let tools = SubagentTools::new(
        session.clone() as Arc<dyn ModelProvider>,
        vec![def],
        Arc::new(ShellTools::new("/workspace")),
        "parent-model",
    )
    .with_providers(factory);

    let sync = SubagentTools::synchronous(tools.inner.clone(), tools.shell.clone());
    let call = ToolCall {
        id: "c1".into(),
        name: "subagent".into(),
        args: serde_json::json!({
            "name": "scout",
            "prompt": "go",
            "model": "anthropic/claude-haiku"
        }),
        provider_data: serde_json::Value::Null,
    };
    let ctx = rhema_core::ToolContext {
        cwd: "/workspace".into(),
        run_id: "turn-1".into(),
        cancelled: Arc::new(AtomicBool::new(false)),
    };
    let result = sync.resolve(call, &ctx).await;
    assert!(
        !result.is_error,
        "the cross-provider call should run: {:?}",
        result
    );
    assert_eq!(other.taken(), vec!["claude-haiku"]);
    assert!(session.taken().is_empty(), "the session provider is idle");
}

#[tokio::test]
async fn a_run_names_its_conversation_key_on_every_request() {
    let session = Recording::new();
    let def = SubagentDef {
        name: "scout".into(),
        description: "explores".into(),
        model: None,
        max_steps: None,
        body: "Explore.".into(),
    };
    let tools = SubagentTools::new(
        session.clone() as Arc<dyn ModelProvider>,
        vec![def],
        Arc::new(ShellTools::new("/workspace")),
        "parent-model",
    );
    let sync = SubagentTools::synchronous(tools.inner.clone(), tools.shell.clone());
    let call = ToolCall {
        id: "c1".into(),
        name: "subagent".into(),
        args: serde_json::json!({ "name": "scout", "prompt": "go" }),
        provider_data: serde_json::Value::Null,
    };
    let ctx = rhema_core::ToolContext {
        cwd: "/workspace".into(),
        run_id: "turn-1".into(),
        cancelled: Arc::new(AtomicBool::new(false)),
    };
    let result = sync.resolve(call, &ctx).await;
    assert!(
        !result.is_error,
        "the synchronous run should complete: {:?}",
        result
    );
    // No child store is wired here, so the key is the run's own
    // shape; a wired run keys on the persisted child id instead.
    // Every request of the run carries the same key, so a dialect
    // that routes or caches by it keeps the whole conversation in
    // one bucket.
    let keys = session.keys();
    assert!(!keys.is_empty(), "the run made at least one call");
    assert!(
        keys.iter()
            .all(|key| key.as_deref() == Some("turn-1/subagent:scout")),
        "{keys:?}"
    );
}

/// A progress channel that keeps everything it is handed.
struct Collector {
    lines: Vec<String>,
    statuses: Vec<rhema_core::ToolStatus>,
}

impl ToolProgress for Collector {
    fn output(&mut self, chunk: &str) {
        self.lines.push(chunk.to_owned());
    }
    fn status(&mut self, status: rhema_core::ToolStatus) {
        self.statuses.push(status);
    }
}

#[test]
fn the_inner_lifecycle_becomes_phase_transitions_and_a_spend() {
    let mut collected = Collector {
        lines: Vec::new(),
        statuses: Vec::new(),
    };
    let mut sink = Forwarder {
        progress: &mut collected,
        model: "parent-model".into(),
        phase: None,
        context: None,
        spent: rhema_core::Usage::default(),
    };
    // Reasoning first, then a tool, its output, the step's spend,
    // then the report being written.
    sink.emit(Event::ThinkingDelta("hmm"));
    sink.emit(Event::ToolCallStart {
        id: "c1",
        name: "bash",
    });
    sink.emit(Event::StepEnd {
        usage: rhema_core::Usage {
            input_tokens: 100,
            output_tokens: 20,
            cached_input_tokens: 80,
        },
        stop_reason: &rhema_core::StopReason::ToolUse,
    });
    sink.emit(Event::TextDelta("the report"));
    drop(sink);

    // The phases arrived as transitions; the step's spend re-sent
    // the running phase with the figures: thinking → running (the
    // tool) → running again (the spend) → writing output (the
    // report).
    let phases: Vec<&str> = collected
        .statuses
        .iter()
        .map(|status| status.phase.as_str())
        .collect();
    assert_eq!(phases, vec![THINKING, RUNNING, RUNNING, WRITING]);
    let last = collected.statuses.last().unwrap();
    assert_eq!(last.model.as_deref(), Some("parent-model"));
    // The step's call is the current context...
    let context = last.context.unwrap();
    assert_eq!(context.input_tokens, 100);
    assert_eq!(context.output_tokens, 20);
    assert_eq!(context.cached_input_tokens, 80);
    // ...and the only spend so far.
    let spent = last.spent.unwrap();
    assert_eq!(spent.input_tokens, 100);
    assert_eq!(spent.output_tokens, 20);
    assert_eq!(spent.cached_input_tokens, 80);

    // The inner output rides no channel: the subagent is its own
    // line, its report the result.
    assert!(collected.lines.is_empty());
}

/// A retry announcement is a phase of its own: a job riding out an
/// outage reads as waiting, with the attempt count, not hung.
#[test]
fn a_retry_wait_becomes_a_retrying_phase() {
    let mut collected = Collector {
        lines: Vec::new(),
        statuses: Vec::new(),
    };
    let mut sink = Forwarder {
        progress: &mut collected,
        model: "m".into(),
        phase: None,
        context: None,
        spent: rhema_core::Usage::default(),
    };
    sink.emit(Event::ThinkingDelta("hmm"));
    sink.emit(Event::RetryWait {
        error: "network error: dead link",
        delay: Duration::from_secs(30),
        attempt: 2,
        total: 6,
    });
    sink.emit(Event::RetryWait {
        error: "network error: dead link",
        delay: Duration::from_secs(60),
        attempt: 3,
        total: 6,
    });
    sink.emit(Event::TextDelta("still here"));
    drop(sink);

    let phases: Vec<&str> = collected
        .statuses
        .iter()
        .map(|status| status.phase.as_str())
        .collect();
    assert_eq!(
        phases,
        vec![
            THINKING,
            "retrying model call (attempt 2/6)",
            "retrying model call (attempt 3/6)",
            WRITING,
        ]
    );
}

#[test]
fn repeated_phases_send_nothing() {
    let mut collected = Collector {
        lines: Vec::new(),
        statuses: Vec::new(),
    };
    let mut sink = Forwarder {
        progress: &mut collected,
        model: "m".into(),
        phase: None,
        context: None,
        spent: rhema_core::Usage::default(),
    };
    sink.emit(Event::ThinkingDelta("a"));
    sink.emit(Event::ThinkingDelta("b"));
    assert_eq!(collected.statuses.len(), 1);
}

/// A running tally is the current call's context, never a spend: a
/// mid-call frame replaces the context without touching the spend,
/// and only the step's end bills into the spend. The context holds
/// past a `StepEnd`, the reply rides into the next request, until
/// the next call's first frame replaces it.
#[test]
fn a_running_tally_is_context_and_only_a_step_end_is_spend() {
    let mut collected = Collector {
        lines: Vec::new(),
        statuses: Vec::new(),
    };
    let mut sink = Forwarder {
        progress: &mut collected,
        model: "m".into(),
        phase: None,
        context: None,
        spent: rhema_core::Usage::default(),
    };
    sink.emit(Event::ThinkingDelta("hmm"));
    sink.emit(Event::Usage(rhema_core::Usage {
        input_tokens: 40_000,
        output_tokens: 10,
        cached_input_tokens: 0,
    }));
    sink.emit(Event::StepEnd {
        usage: rhema_core::Usage {
            input_tokens: 40_000,
            output_tokens: 500,
            cached_input_tokens: 0,
        },
        stop_reason: &rhema_core::StopReason::ToolUse,
    });
    sink.emit(Event::ToolCallStart {
        id: "c2",
        name: "bash",
    });
    sink.emit(Event::Usage(rhema_core::Usage {
        input_tokens: 41_000,
        output_tokens: 20,
        cached_input_tokens: 40_000,
    }));
    drop(sink);

    // The thinking status carried the running frame as its context
    // and nothing billed yet.
    let thinking = &collected.statuses[1];
    assert_eq!(thinking.phase, THINKING);
    assert_eq!(
        thinking.context,
        Some(rhema_core::Usage {
            input_tokens: 40_000,
            output_tokens: 10,
            cached_input_tokens: 0,
        })
    );
    assert_eq!(
        thinking.spent,
        Some(rhema_core::Usage {
            input_tokens: 0,
            output_tokens: 0,
            cached_input_tokens: 0,
        })
    );

    // The step's end billed the spend and held the context; the
    // running status kept both...
    let running = &collected.statuses[3];
    assert_eq!(running.phase, RUNNING);
    assert_eq!(
        running.context,
        Some(rhema_core::Usage {
            input_tokens: 40_000,
            output_tokens: 500,
            cached_input_tokens: 0,
        })
    );
    assert_eq!(
        running.spent,
        Some(rhema_core::Usage {
            input_tokens: 40_000,
            output_tokens: 500,
            cached_input_tokens: 0,
        })
    );

    // ...and the next call's frame replaced the context, the spend
    // still counting every step.
    let next = collected.statuses.last().unwrap();
    assert_eq!(
        next.context,
        Some(rhema_core::Usage {
            input_tokens: 41_000,
            output_tokens: 20,
            cached_input_tokens: 40_000,
        })
    );
    assert_eq!(
        next.spent,
        Some(rhema_core::Usage {
            input_tokens: 40_000,
            output_tokens: 500,
            cached_input_tokens: 0,
        })
    );
}

/// A background job's statuses land in the registry through the
/// status pump: the job's record moves with its inner turn.
#[test]
fn the_status_pump_reports_into_the_registry() {
    let registry = Arc::new(Registry::new(1));
    let minted = registry.mint("scout");
    registry.mark_running(&minted.id, Some("m".into()));
    let mut pump = StatusPump {
        registry: Arc::downgrade(&registry),
        id: minted.id.clone(),
    };
    pump.status(rhema_core::ToolStatus {
        phase: RUNNING.to_owned(),
        context: Some(rhema_core::Usage::default()),
        spent: Some(rhema_core::Usage::default()),
        model: Some("m".into()),
    });
    assert_eq!(registry.status()[0].phase, RUNNING);
}

/// A probe resolver for the continuation's revival: `safe`
/// declares replay-safe (the posture a read-only tool carries),
/// everything else keeps the resolver default, and the executed
/// names are recorded, so an unsafe call that re-ran as a side
/// effect cannot pass unnoticed.
struct ReviveProbe {
    ran: Mutex<Vec<String>>,
}

impl ReviveProbe {
    fn executed(&self) -> Vec<String> {
        self.ran
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl ToolResolver for ReviveProbe {
    fn specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }

    fn replay_safety(&self, name: &str) -> rhema_core::ReplaySafety {
        if name == "safe" {
            rhema_core::ReplaySafety::Safe
        } else {
            rhema_core::ReplaySafety::Unsafe
        }
    }

    async fn resolve(&self, call: ToolCall, _ctx: &ToolContext) -> ToolResult {
        self.ran
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(call.name.clone());
        ToolResult::success(call.id, "re-ran")
    }
}

/// A child as a transient failure left it: its turn settled
/// `Failed { retryable: true }`, its committed step in the
/// history, a dangling call block at the tail, exactly the shape a
/// restart loads off disk.
fn failed_child() -> (Session, rhema_submissions::SubmissionId) {
    let mut session = Session::default();
    let record = rhema_submissions::turn::submit(&mut session, "do the work", 1).unwrap();
    let started = rhema_submissions::turn::start(&mut session, &record.id, "do the work", 2)
        .unwrap()
        .id;
    session
        .history
        .push(Message::text(Role::User, "do the work"));
    session.history.push(Message {
        annotations: Default::default(),
        role: Role::Assistant,
        content: vec![
            Content::ToolCall(ToolCall {
                id: "safe-1".into(),
                name: "safe".into(),
                args: json!({"look": "at this"}),
                ..ToolCall::default()
            }),
            Content::ToolCall(ToolCall {
                id: "bash-1".into(),
                name: "bash".into(),
                args: json!({"command": "echo ran"}),
                ..ToolCall::default()
            }),
        ],
    });
    rhema_submissions::settle(
        &mut session,
        &started,
        rhema_submissions::Outcome::Failed {
            error: "network error: dead link".into(),
            retryable: true,
        },
        3,
    )
    .unwrap();
    (session, started)
}

/// Every tool result in the history, in order, as
/// (id, is_error, content).
fn tool_results(session: &Session) -> Vec<(String, bool, String)> {
    session
        .history
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|content| match content {
            Content::ToolResult(result) => {
                Some((result.id.clone(), result.is_error, result.content.clone()))
            }
            _ => None,
        })
        .collect()
}

/// The re-run of a transient failure re-arms the child's own turn,
/// never a second submit, and revives the failed run's dangling
/// calls: the replay-safe one re-executed and its result landed,
/// the unsafe one answered with the verify-state failure the
/// engine's contract spells, and the prompt is not pushed twice.
#[tokio::test]
async fn a_restart_re_arms_the_failed_turn_and_revives_the_tail() {
    let (mut session, turn) = failed_child();
    let probe = Arc::new(ReviveProbe {
        ran: Mutex::new(Vec::new()),
    });
    let re_armed = prepare_restart(
        &mut session,
        "do the work",
        probe.as_ref(),
        &ToolContext::default(),
    )
    .await
    .unwrap()
    .expect("the failed turn is re-armable");
    assert_eq!(re_armed, turn, "the same record, not a second submit");
    let records = rhema_submissions::snapshot(&session).unwrap();
    assert_eq!(records.len(), 1, "the ledger holds the one turn");
    let record = records.get(&re_armed).unwrap();
    assert_eq!(record.status, rhema_submissions::Status::Running);
    assert!(
        record.outcome.is_none(),
        "the retry dropped the settled outcome"
    );
    assert_eq!(
        session
            .history
            .iter()
            .filter(|message| message.role == Role::User)
            .count(),
        1,
        "the prompt its turn already landed is not pushed again"
    );
    // The safe call re-ran through the resolver, the unsafe one
    // never executed: the probe's log is the proof of separation.
    assert_eq!(probe.executed(), vec!["safe".to_owned()]);
    let results = tool_results(&session);
    assert_eq!(
        results[0],
        ("safe-1".to_owned(), false, "re-ran".to_owned()),
        "the safe call re-ran and its result landed"
    );
    assert_eq!(results[1].0, "bash-1");
    assert!(
        results[1].1 && results[1].2.contains("verify state before retrying"),
        "the unsafe call reads the engine's verify-state failure: {}
{}",
        results[1].2,
        results[1].1
    );
}

/// A loaded child whose ledger has no re-armable turn (a done
/// child, a child reached with another prompt) keeps its history:
/// the settled turn stays settled, a new one is submitted over
/// the same session, and the prompt lands once, once only even
/// when it already sits in the tail.
#[tokio::test]
async fn a_child_without_a_re_armable_turn_gets_a_new_one_over_its_history() {
    let mut session = Session::default();
    let record = rhema_submissions::turn::submit(&mut session, "do the work", 1).unwrap();
    let started =
        rhema_submissions::turn::start(&mut session, &record.id, "do the work", 2).unwrap();
    session
        .history
        .push(Message::text(Role::User, "do the work"));
    session
        .history
        .push(Message::text(Role::Assistant, "the report"));
    rhema_submissions::settle(
        &mut session,
        &started.id,
        rhema_submissions::Outcome::Done,
        3,
    )
    .unwrap();
    let probe = Arc::new(ReviveProbe {
        ran: Mutex::new(Vec::new()),
    });

    // The same prompt over a done child: no arm is taken, the
    // settled record stands, and the prompt already in the tail is
    // never duplicated.
    let first = prepare_restart(
        &mut session,
        "do the work",
        probe.as_ref(),
        &ToolContext::default(),
    )
    .await
    .unwrap()
    .expect("a fresh turn is submitted");
    assert_ne!(first, started.id, "the done turn is untouched");
    let records = rhema_submissions::snapshot(&session).unwrap();
    assert_eq!(
        records.get(&started.id).unwrap().status,
        rhema_submissions::Status::Done
    );
    let fresh = records.get(&first).unwrap();
    assert_eq!(fresh.status, rhema_submissions::Status::Running);
    assert_eq!(session.history.len(), 2, "the history stands as it was");

    // Another prompt over the same child: it is pushed, once, at
    // the tail, and the new turn reads it there.
    let second = prepare_restart(
        &mut session,
        "another task",
        probe.as_ref(),
        &ToolContext::default(),
    )
    .await
    .unwrap()
    .expect("a fresh turn is submitted");
    assert_ne!(second, first);
    assert_eq!(
        session.history.len(),
        3,
        "a new prompt is pushed exactly once"
    );
    assert_eq!(session.history.last().unwrap().role, Role::User);
    let records = rhema_submissions::snapshot(&session).unwrap();
    let payload: rhema_submissions::turn::Payload =
        serde_json::from_value(records.get(&second).unwrap().payload.clone()).unwrap();
    assert_eq!(payload.input, "another task");
    assert_eq!(
        payload.base,
        Some(2),
        "the turn reads where its input landed"
    );
    // Nothing dangled, so nothing re-ran.
    assert!(probe.executed().is_empty());
}
