//! The turn's life: begin, track, cancel, end — and the UI events
//! and ticks that carry it. Effects that touch the runtime come back
//! to the runner from here.

use super::*;

impl App {
    /// Handles one event from the turn/fetch tasks.
    pub fn on_event(&mut self, event: UiEvent) -> Vec<Effect> {
        match event {
            UiEvent::Engine(owned) => {
                self.track_phase(&owned);
                self.transcript.apply(&owned);
                Vec::new()
            }
            UiEvent::ModelsLoaded { why, result } => self.on_models_loaded(why, result),
            UiEvent::LoginUpdate(update) => self.on_login_update(update),
            UiEvent::ApprovalRequested { call } => {
                if self.turn.decline_all {
                    // Esc declined the rest of the turn: answer without a
                    // modal so the blocked turn task keeps going.
                    self.turn.answer(false);
                } else {
                    self.turn.pending_approval = Some(call);
                }
                Vec::new()
            }
            UiEvent::SessionResumed { path, result } => {
                self.apply_resume(&path, result);
                Vec::new()
            }
            UiEvent::UsageLoaded { provider, result } => {
                // Err or a switched-away backend both degrade to a
                // smaller badge, never to a stale figure.
                self.subscription = result.ok().map(|allowance| (provider, allowance));
                Vec::new()
            }
            UiEvent::SandboxAssembled { choice, result } => {
                self.assembling = None;
                match result {
                    Ok(assembled) => {
                        self.replace_sandboxed(Some(assembled));
                        self.rebuild_for_sandbox(&format!(
                            "bash runs on {} now",
                            choice.stored_name()
                        ));
                    }
                    Err(err) => self.transcript.push_notice(&format!(
                        "the {} sandbox failed: {err}; bash stays on {}",
                        choice.stored_name(),
                        self.active_sandbox().stored_name()
                    )),
                }
                Vec::new()
            }
            UiEvent::TurnFinished {
                agent,
                result,
                steer,
            } => {
                // The turn just moved the account's counters; the badge
                // is only as good as its last fetch. A steered message
                // that raced the finish rides along as the next turn.
                let mut effects = self.end_turn(agent, result, steer);
                effects.push(Effect::FetchUsage);
                effects
            }
            UiEvent::TurnCancelled { agent, steer } => {
                self.cancel_finished(agent, steer);
                vec![Effect::FetchUsage]
            }
        }
    }

    /// The redraw loop's per-tick work: subagent finishes reach the
    /// transcript, and a finished job's notification reaches the model.
    /// Both deliveries split on the turn state — the steering path and
    /// the regular-prompt path:
    ///
    /// - a finish lands as a transcript notice right away, whatever
    ///   the turn state — the job's pinned line left the strip at the
    ///   same moment, so the notice is the record;
    /// - while a turn runs, the driver steers: it appends the pending
    ///   notification lines at the next step boundary, and the tick
    ///   touches nothing but the notice;
    /// - with no turn running, the tick takes the pending lines and
    ///   runs them as the next turn's prompt — the same delivery a
    ///   user submit would give them. The take retires the lines, so
    ///   the driver's turn-start drain does not repeat what the prompt
    ///   already said, and the prompt's user line is the transcript's
    ///   record (no separate notice for the same text).
    pub fn on_tick(&mut self) -> Vec<Effect> {
        let Some(handle) = self.subagents.as_ref() else {
            return Vec::new();
        };
        let finishes = handle.take_finished();
        let promptable = !self.turn.running
            && !self.confirm_quit
            && self.agent.is_some()
            && self.assembling.is_none();
        if !promptable {
            for job in finishes {
                self.transcript.push_notice(&job.line);
            }
            return Vec::new();
        }
        let lines = handle.take_notifications();
        if lines.is_empty() {
            return Vec::new();
        }
        let prompt = lines.join("\n");
        self.transcript.push_user(&prompt, Vec::new());
        vec![Effect::StartTurn {
            text: prompt,
            images: Vec::new(),
        }]
    }

    /// Retry a failed model fetch.
    pub(crate) fn retry_fetch(&mut self) -> Vec<Effect> {
        match &self.screen {
            Screen::Picker(_) => vec![Effect::FetchModels(FetchWhy::Refresh)],
            _ => Vec::new(),
        }
    }

    /// The runner accepted a turn: a fresh lifecycle state with the
    /// cancel flag and the approval answer channel wired, starting in
    /// the thinking phase.
    pub fn begin_turn(
        &mut self,
        cancel: tokio::sync::watch::Sender<bool>,
        steer: tokio::sync::mpsc::Sender<String>,
        answers: Option<ApprovalAnswerer>,
    ) {
        self.turn = TurnState::running(cancel, steer, answers);
        self.remember_profile();
    }

    /// Tracks which phase a running turn is in, from the engine's own
    /// events: streaming is thinking, a tool call is processing. A result
    /// leaves the phase alone — the next call or step moves it, so the
    /// label never flickers between back-to-back commands. Reasoning
    /// deltas fill the preview buffer; real output or a tool call clears
    /// it — the thinking block vanishes once the answer starts. A
    /// running command's lines fill the tool preview; its result or the
    /// next call clears that. The provider's running usage tally lands
    /// in `usage` as it arrives: its prompt count already covers the
    /// whole conversation, so the footer's gauge moves with the stream
    /// instead of waiting for `StepEnd`.
    pub(crate) fn track_phase(&mut self, owned: &OwnedEvent) {
        // Any event ends a retry wait: the next attempt is speaking.
        // The announcement itself is what sets one.
        if !matches!(owned, OwnedEvent::RetryWait { .. }) {
            self.turn.retry_wait = None;
        }
        match owned {
            OwnedEvent::StepStart { .. } | OwnedEvent::TextDelta(_) => {
                self.turn.processing = false;
                self.turn.thinking.clear();
            }
            OwnedEvent::ThinkingDelta(delta) => self.turn.push_thinking(delta),
            // A tool call means a command runs. Its arguments often never
            // stream as deltas, so the label stays phase-only; the last
            // command's preview goes with it.
            OwnedEvent::ToolCallStart { .. } => {
                self.turn.processing = true;
                self.turn.thinking.clear();
                self.turn.clear_tool_output();
            }
            OwnedEvent::ToolOutputDelta { chunk, .. } => self.turn.push_tool_output(chunk),
            // A background subagent job reports through the registry
            // (the wire's handle), not through the call's own status
            // stream: the start call answers with its job id at once.
            OwnedEvent::ToolStatus { .. } => {}
            OwnedEvent::ToolResult { .. } => self.turn.clear_tool_output(),
            // A zero tally is a placeholder frame, not a measurement:
            // keeping the stale figure beats dropping the gauge to 0.
            OwnedEvent::Usage(usage) if context_tokens(usage) > 0 => self.usage = Some(*usage),
            OwnedEvent::RetryWait {
                error,
                delay,
                attempt,
                total,
            } => {
                self.turn.retry_wait = Some(TurnRetryWait {
                    error: error.clone(),
                    deadline: std::time::Instant::now() + *delay,
                    attempt: *attempt,
                    total: *total,
                });
            }
            _ => {}
        }
    }

    /// Cancels the running turn from the keyboard. The approval bridge
    /// blocks its thread on the answer, so a pending modal is released
    /// first; upcoming approvals are declined; then the flag flips and
    /// the turn task drops the future at its current await. The agent
    /// comes back in a `TurnCancelled`.
    pub(crate) fn cancel_turn(&mut self) {
        self.turn.answer(false);
        self.turn.decline_all = true;
        if let Some(cancel) = &self.turn.cancel {
            let _ = cancel.send(true);
        }
    }

    /// The turn task was cancelled from the keyboard: take the agent
    /// back, drop the turn's state, and mark the cutoff in the transcript.
    pub(crate) fn cancel_finished(
        &mut self,
        agent: Box<CodingAgent>,
        mut steer: tokio::sync::mpsc::Receiver<String>,
    ) {
        self.transcript_stats = agent.transcript_stats();
        self.agent = Some(*agent);
        self.turn.finished();
        self.retryable = false;
        self.transcript.push_notice("turn cancelled");
        // A steered message that raced the cancel goes back to the input:
        // the user stopped the turn, so the words are not spent.
        let mut restore = String::new();
        while let Ok(text) = steer.try_recv() {
            if !restore.is_empty() {
                restore.push('\n');
            }
            restore.push_str(&text);
        }
        // The queued chain dies with the turn the same way: unspent
        // words go back to the input, in order.
        while let Some(text) = self.pending_prompts.pop_front() {
            if !restore.is_empty() {
                restore.push('\n');
            }
            restore.push_str(&text);
        }
        if !restore.is_empty() {
            self.chat.insert_str(&restore);
        }
    }

    /// The turn task finished: take the agent back, record the usage or
    /// the error, and drop any stale modal (the turn cannot finish while
    /// an approval is unanswered, so a leftover modal means the answer
    /// channel is gone).
    pub(crate) fn end_turn(
        &mut self,
        agent: Box<CodingAgent>,
        result: Result<ModelOutput, String>,
        mut steer: tokio::sync::mpsc::Receiver<String>,
    ) -> Vec<Effect> {
        self.transcript_stats = agent.transcript_stats();
        self.agent = Some(*agent);
        self.turn.finished();
        match result {
            Ok(output) => {
                self.retryable = false;
                if let Some(pricing) = self.current_model_pricing() {
                    let usage = output.usage;
                    self.session_cost += pricing.cost(
                        u64::from(usage.input_tokens),
                        u64::from(usage.output_tokens),
                        u64::from(usage.cached_input_tokens),
                    );
                }
                // A final tally of zeros is a proxy that never wired
                // usage through, not a free turn: keep the previous
                // gauge instead of pricing the turn at nothing.
                if output.usage.input_tokens > 0 || output.usage.output_tokens > 0 {
                    self.usage = Some(output.usage);
                }
                // Silence is a symptom, not an answer: the usual cause is
                // a reasoning-only response — a thinking model spending
                // its whole output budget before any text started — and
                // the turn must not read as completed when nothing was
                // said.
                let said_something = output.message.content.iter().any(|content| {
                    matches!(content, Content::Text(text) if !text.is_empty())
                        || matches!(content, Content::ToolCall(_))
                });
                if !said_something {
                    self.push_notice(&format!(
                        "the model returned no content (stopped: {})",
                        Self::stop_words(&output.stop_reason)
                    ));
                }
            }
            Err(message) => {
                self.error = Some(message);
                self.retryable = true;
            }
        }
        // Messages sent in the finishing race sit in the returned
        // receiver: they run as the next turns, in order. The transcript
        // already carries their user lines from submit time. Only one
        // turn leaves per finish — the dispatch takes the agent once,
        // and a second StartTurn would find it gone — so the rest queue
        // on the app and each finish chains the next one out.
        while let Ok(text) = steer.try_recv() {
            self.pending_prompts.push_back(text);
        }
        let mut effects = Vec::new();
        if let Some(text) = self.pending_prompts.pop_front() {
            self.retryable = false;
            effects.push(Effect::StartTurn {
                text,
                images: Vec::new(),
            });
        }
        effects
    }

    /// The stop reason as the few words a notice names it by.
    pub(crate) fn stop_words(reason: &StopReason) -> String {
        match reason {
            StopReason::EndTurn => "end turn".to_owned(),
            StopReason::ToolUse => "tool use".to_owned(),
            StopReason::MaxTokens => "max tokens".to_owned(),
            StopReason::StopSequence => "stop sequence".to_owned(),
            StopReason::Other(other) => other.clone(),
        }
    }
}
