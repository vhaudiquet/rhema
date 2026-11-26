//! Session state: resume, clear, export, the checkpoint the exit
//! save rides, and the gauges and palette snapshots read from it.

use super::*;

impl App {
    /// Backs the outgoing conversation up, then drops it: a fresh
    /// transcript and a fresh session. The file binding drops with it,
    /// what is typed after /clear is a new conversation, not a
    /// continuation of the backed-up file, so the next turn allocates a
    /// file of its own instead of overwriting that history (see
    /// [`Self::ensure_checkpoint_target`]).
    pub fn clear_session(&mut self) {
        // The backup lands before the history clears: a clear is a
        // conversation boundary the user asked for, not a discard.
        let backup = self.back_up_session();
        if let Some(agent) = self.agent.as_mut() {
            agent.session_mut().history.clear();
        }
        // The App's own copy clears with the conversation too: with the
        // agent out, a keyless switch, a removed login, this copy is
        // what a switch-back rebuilds on and what the exit save exports.
        // Left full, the cleared conversation came back, and the exit
        // save (its file binding dropped below) wrote it to a fresh
        // session file: one conversation, two picker entries.
        self.session.history.clear();
        self.transcript = Transcript::new();
        self.usage = None;
        self.session_cost = 0.0;
        self.error = None;
        self.retryable = false;
        self.session_file = None;
        // The benchmark describes one conversation: cleared with it,
        // in the fallback snapshot and in the agent's mirror alike.
        self.transcript_stats = None;
        if let Some(agent) = self.agent.as_mut() {
            agent.reset_transcript_stats();
        }
        match backup {
            Some(Err(err)) => self.transcript.push_notice(&format!(
                "conversation cleared; the old one was not saved: {err}"
            )),
            Some(Ok(_)) => self
                .transcript
                .push_notice("conversation saved; starting fresh"),
            None => self.transcript.push_notice("conversation cleared"),
        }
    }

    /// Writes the conversation out before [`Self::clear_session`] drops
    /// it: into this conversation's own file, the one it was resumed
    /// from or the runner allocated, so the run's `--save` path stays
    /// free for the conversation that follows. A conversation with no
    /// file (a run whose startup allocation failed) gets a fresh one in
    /// the sessions cache. `None` when the history is empty, so there is
    /// nothing to back up; `Some(Err)` when the write failed, the clear
    /// proceeds either way and says so.
    fn back_up_session(&self) -> Option<std::io::Result<PathBuf>> {
        let (session, _) = self.export_session()?;
        let saved = match self.session_file.clone() {
            Some(path) => crate::sessions::write_session(&path, &session).map(|()| path),
            None => self
                .sessions_base()
                .and_then(|base| self.save_new_session(&base, &session)),
        };
        Some(saved)
    }

    /// Allocates a file under `base` and writes `session` there. The
    /// same shape [`crate::sessions::save_session`] takes, but the path
    /// comes back: the caller just proved the history is not empty.
    fn save_new_session(&self, base: &Path, session: &Session) -> std::io::Result<PathBuf> {
        let workspace = crate::args::canonical(&self.options.workspace);
        let path = crate::sessions::next_session_path(base, &workspace)?;
        crate::sessions::write_session(&path, session)?;
        Ok(path)
    }

    /// The sessions cache: the setup's own base (the test isolation
    /// seam), else the environment's.
    fn sessions_base(&self) -> std::io::Result<PathBuf> {
        match &self.sessions_base {
            Some(base) => Ok(base.clone()),
            None => crate::sessions::base_dir(),
        }
    }

    /// A fresh file for the next conversation, named from the sessions
    /// cache. `None` when the cache is unreachable, the turn then runs
    /// uncheckpointed rather than failing.
    fn next_session_file(&self) -> Option<PathBuf> {
        let base = self.sessions_base().ok()?;
        let workspace = crate::args::canonical(&self.options.workspace);
        crate::sessions::next_session_path(&base, &workspace).ok()
    }

    /// The file a turn checkpoints into and the exit save updates:
    /// `--save`'s explicit path wins; otherwise this conversation's own
    /// file. `None` when neither exists, the exit save then falls back
    /// to allocating at exit, and a turn simply cannot checkpoint.
    pub fn checkpoint_target(&self) -> Option<PathBuf> {
        self.options
            .save
            .clone()
            .or_else(|| self.session_file.clone())
    }

    /// The checkpoint target a turn takes: [`Self::checkpoint_target`],
    /// allocated first when the conversation has none. `/clear` drops
    /// the binding so the next conversation does not overwrite the
    /// backed-up file; its first turn then gets a fresh name (unless
    /// `--save` names the run's file) and streams to disk from then on.
    pub fn ensure_checkpoint_target(&mut self) -> Option<PathBuf> {
        if self.checkpoint_target().is_none() {
            self.session_file = self.next_session_file();
            // A cleared conversation's file is a new one, so its jobs
            // and its subagent children are a new set: the shells, the
            // subscription, and the children's store move with it.
            if let Some(path) = self.session_file.clone() {
                self.adopt_conversation(&path);
            }
        }
        self.checkpoint_target()
    }

    /// Attaches the conversation-scoped plumbing to `path`: the tag the
    /// shells write, the subscription's listing, and the parent the subagent
    /// children are filed under all name the same session. Stays put
    /// when the path names no session id, because a tag the next resume
    /// cannot reproduce would only orphan the work it started.
    pub(crate) fn adopt_conversation(&mut self, path: &Path) {
        let Some(tag) = crate::jobs::tag_from_path(path) else {
            return;
        };
        if tag == self.jobs_tag {
            return;
        }
        self.jobs_tag = tag.clone();
        // A crash leaves the self-stop record behind; its entries are
        // backend ids, which the daemon reuses, so a stale one must not
        // silence a new job that happens to get the same id.
        rhema_bash::jobs::forget_stopped(&tag);
        // The shells wired so far keep the env map they were handed,
        // so the tag file is what moves them: without this, `job run`
        // would keep tagging the conversation just left.
        if let Some(Err(err)) = rhema_bash::jobs::retag(&tag) {
            self.transcript
                .push_notice(&format!("the job tag was not updated: {err}"));
        }
        // The subscription restarts before the agent builds, so the rebuilt
        // steering chain reads the new feed.
        if self.sandboxed.is_none() {
            self.jobs = crate::jobs::JobsHandle::spawn(tag.clone());
        }
        // The children's store is keyed by the same session: a
        // conversation switched under a live agent (a clear, whose
        // children are the next conversation's) must not keep filing
        // them under the one just left.
        if let Some(subagents) = &self.subagents {
            let base = self.sessions_base().ok();
            let workspace = crate::args::canonical(&self.options.workspace);
            Self::attach_subagents(subagents, base.as_deref(), &workspace, &tag);
        }
    }

    /// The session to save for /resume, and the file it belongs to: the
    /// live session when its history holds anything, writing back to the
    /// origin when this session was resumed. A session of notices alone
    /// (a switch, a decline) has nothing resumable and saves nothing.
    pub fn export_session(&self) -> Option<(Session, Option<PathBuf>)> {
        let mut session = match &self.agent {
            Some(agent) => agent.session().clone(),
            None => self.session.clone(),
        };
        // The accounting rides with the save: a resumed session's
        // footer continues where this one ended, and its Usage page
        // keeps this run's numbers.
        session.cost_usd = self.session_cost;
        crate::sessions::UsageStats {
            transcript: self
                .agent
                .as_ref()
                .and_then(HostAgent::transcript_stats)
                .or(self.transcript_stats),
            // The compaction rides with the counters: the resumed
            // session adopts the sections instead of re-summarizing.
            transcript_sections: self
                .agent
                .as_ref()
                .map(HostAgent::transcript_sections)
                .filter(|sections| !sections.is_empty()),
            last_turn: self.usage,
        }
        .write_into(&mut session);
        if session.history.is_empty() {
            None
        } else {
            Some((session, self.session_file.clone()))
        }
    }

    /// Opens the resume picker over the sessions saved for this
    /// workspace. An empty history of saves is a notice, not a screen.
    pub fn open_resume(&mut self) -> Vec<Effect> {
        match crate::sessions::base_dir() {
            Ok(base) => self.open_resume_from(&base),
            Err(err) => {
                self.push_notice(&format!("cannot locate the session cache: {err}"));
                Vec::new()
            }
        }
    }

    /// [`App::open_resume`] against an explicit base, so tests drive real
    /// directories instead of the environment.
    pub fn open_resume_from(&mut self, base: &Path) -> Vec<Effect> {
        if self.turn.running {
            self.transcript
                .push_notice("a turn is running; /resume waits for it to finish");
            return Vec::new();
        }
        let workspace = crate::args::canonical(&self.options.workspace);
        match crate::sessions::list_sessions(base, &workspace) {
            Ok(entries) if entries.is_empty() => {
                self.transcript
                    .push_notice("no saved sessions for this directory");
            }
            Ok(entries) => {
                self.screen = Screen::Resume(crate::screens::ResumeScreen::new(
                    entries,
                    crate::sessions::now_millis(),
                ));
            }
            Err(err) => self.error = Some(format!("cannot list saved sessions: {err}")),
        }
        Vec::new()
    }

    /// The `/resume` pick: refuses politely under a running turn, and
    /// hands the load to the runner, the session file is a JSON read
    /// plus a parse on the blocking pool, and the result lands as
    /// [`UiEvent::SessionResumed`] for `apply_resume`.
    pub fn request_resume(&mut self, path: &Path) -> Vec<Effect> {
        if self.turn.running {
            self.transcript
                .push_notice("a turn is running; /resume waits for it to finish");
            return Vec::new();
        }
        // The newest pick owns the landing: an older load still in
        // flight lands stale and drops, rather than overwriting the
        // conversation its picker moved on from.
        self.resume_pending = Some(path.to_path_buf());
        vec![Effect::ResumeSession(path.to_path_buf())]
    }

    /// A resume landed from the blocking pool. Only the newest pick
    /// applies: a load superseded mid-flight by a second pick drops,
    /// and a load overtaken by a running turn refuses the same way a
    /// fresh pick would, the pick is easy to make again once the turn
    /// ends. [`Self::apply_resume`] alone never checks — the runner
    /// can hand it states the picker already refused.
    pub(crate) fn land_resume(&mut self, path: &Path, loaded: Result<Box<ResumedLoad>, String>) {
        if self.resume_pending.as_deref() != Some(path) {
            return;
        }
        self.resume_pending = None;
        if self.turn.running {
            self.transcript.push_notice(
                "a turn began before the session finished loading; /resume again once it ends",
            );
            return;
        }
        self.apply_resume(path, loaded);
    }

    /// Loads a saved session and applies it on the spot: the
    /// synchronous path the tests drive and the headless runner keeps.
    /// The interactive app goes through [`Self::request_resume`]
    /// instead, the UI thread never reads the file.
    pub fn resume_session(&mut self, path: &Path) {
        let loaded = Self::load_resume_parts(path).map_err(|err| err.to_string());
        self.apply_resume(path, loaded);
    }

    /// Applies a loaded resume: rebuilds the agent on it, re-derives
    /// the transcript from its history, and lands in the chat. The
    /// current conversation is replaced, it keeps its own saved file
    /// from its own exit.
    pub(crate) fn apply_resume(&mut self, path: &Path, loaded: Result<Box<ResumedLoad>, String>) {
        let loaded = match loaded {
            Ok(loaded) => *loaded,
            Err(err) => {
                self.error = Some(format!("cannot load session: {err}"));
                return;
            }
        };
        let ResumedLoad { session, stats } = loaded;
        let mut session = session;
        // The conversation's tag is its file's id: a resumed process
        // must list the jobs its previous one started, not a fresh
        // process's empty set.
        self.session_file = Some(path.to_path_buf());
        self.adopt_conversation(path);
        // Loading tells the truth and starts nothing: adopt what the
        // ledger missed, settle what the crash left, and save once.
        let changes = self.reconcile_loaded(&mut session);
        let (mut agent, subagents, missing) = match self.build_agent(session.clone()) {
            Ok(built) => built,
            Err(err) => {
                self.error = Some(err.to_string());
                return;
            }
        };
        Self::attach_subagents(
            &subagents,
            self.sessions_base().ok().as_deref(),
            &crate::args::canonical(&self.options.workspace),
            &self.jobs_tag,
        );
        let messages = session.history.len();
        self.session = session;
        // The saved usage accounting came with the load: the last
        // turn's tokens go straight to the gauge, and the transcript's
        // counters fold into the fresh mirror, so /settings's Usage
        // page keeps reading as one run.
        self.usage = stats.as_ref().and_then(|stats| stats.last_turn);
        if let Some(saved) = stats.as_ref().and_then(|stats| stats.transcript.as_ref()) {
            agent.carry_transcript_stats(saved);
        }
        if let Some(sections) = stats
            .as_ref()
            .and_then(|stats| stats.transcript_sections.clone())
        {
            agent.adopt_transcript_sections(sections);
        }
        self.transcript_stats = None;
        self.agent = Some(agent);
        self.subagents = Some(subagents);
        self.missing_tools = missing;
        self.transcript = Transcript::from_history(&self.session.history);
        // The saved session carries what it spent; the footer continues
        // the story instead of quoting a fresh zero.
        self.session_cost = self.session.cost_usd;
        self.retryable = false;
        self.chat.scroll = 0;
        self.screen = Screen::Chat;
        self.transcript
            .push_notice(&format!("resumed {messages} messages"));
        // What the load found: an interrupted subagent is announced and
        // left alone, a job that ended while the process was down
        // becomes the notice and steering line the live feed would have
        // produced.
        self.announce_changes(&changes);
    }

    /// The kinds this build links for one loaded session: the turn
    /// kind always, the subagent kind when a store is reachable, the
    /// job kind when a local subscription is running.
    pub(crate) fn kinds_for(
        sessions_base: Option<&Path>,
        workspace: &Path,
        jobs_tag: &str,
        with_jobs: bool,
    ) -> rhema_submissions::Kinds {
        let mut kinds =
            rhema_submissions::Kinds::new().with(Arc::new(rhema_submissions::turn::Turn));
        if let (Some(base), Ok(parent)) =
            (sessions_base, rhema_session_store::SessionId::new(jobs_tag))
        {
            let store: Arc<dyn rhema_session_store::SessionStore> =
                Arc::new(rhema_session_store_file::FileStore::scoped(base, workspace));
            kinds = kinds.with(Arc::new(rhema_subagents::SubagentKind::new(store, parent)));
        }
        if with_jobs {
            kinds = kinds.with(Arc::new(rhema_bash::jobs::kind::JobKind::new(jobs_tag)));
        }
        kinds
    }

    /// Points the subagent tool at the store its children are saved
    /// under, and names the parent session they belong to. Without the
    /// attachment a subagent runs in memory and leaves no child.
    pub(crate) fn attach_subagents(
        handle: &rhema_subagents::SubagentHandle,
        sessions_base: Option<&Path>,
        workspace: &Path,
        jobs_tag: &str,
    ) {
        let (Some(base), Ok(parent)) =
            (sessions_base, rhema_session_store::SessionId::new(jobs_tag))
        else {
            return;
        };
        let store: Arc<dyn rhema_session_store::SessionStore> =
            Arc::new(rhema_session_store_file::FileStore::scoped(base, workspace));
        handle.attach_store(store, parent);
    }

    /// Reconciles a freshly loaded session, saving once when anything
    /// changed. Never starts work.
    pub(crate) fn reconcile_loaded(
        &mut self,
        session: &mut Session,
    ) -> Vec<rhema_submissions::Change> {
        let base = self.sessions_base().ok();
        let workspace = crate::args::canonical(&self.options.workspace);
        let kinds = Self::kinds_for(
            base.as_deref(),
            &workspace,
            &self.jobs_tag,
            self.jobs.as_ref().is_some_and(|jobs| !jobs.is_remote()),
        );
        match rhema_submissions::reconcile(session, &kinds, rhema_submissions::now()) {
            Ok(changes) => {
                if !changes.is_empty()
                    && let Some(path) = self.checkpoint_target()
                    && let Err(err) = crate::sessions::write_session(&path, session)
                {
                    self.transcript
                        .push_notice(&format!("the reconciled session was not saved: {err}"));
                }
                changes
            }
            Err(err) => {
                self.transcript
                    .push_notice(&format!("the submission ledger could not be read: {err}"));
                Vec::new()
            }
        }
    }

    /// Records a prompt's turn before the task takes the agent: the
    /// submit, the start that pins its history index, and the save that
    /// makes both durable. The returned id is the record the chain
    /// settles when it ends; `None` when the ledger refused the write,
    /// the turn then runs without a record.
    pub(crate) fn begin_turn_submission(
        &mut self,
        agent: &mut HostAgent,
        input: &str,
    ) -> Option<rhema_submissions::SubmissionId> {
        let at = rhema_submissions::now();
        let record = match rhema_submissions::turn::submit(agent.session_mut(), input, at) {
            Ok(record) => record,
            Err(err) => {
                self.transcript
                    .push_notice(&format!("the turn was not recorded: {err}"));
                return None;
            }
        };
        if let Err(err) = rhema_submissions::turn::start(agent.session_mut(), &record.id, input, at)
        {
            let _ = rhema_submissions::cancel(agent.session_mut(), &record.id, at);
            self.transcript
                .push_notice(&format!("the turn was not recorded: {err}"));
            return None;
        }
        self.ensure_checkpoint_target();
        if let Some(path) = self.checkpoint_target()
            && let Err(err) = crate::sessions::write_session(&path, agent.session())
        {
            self.transcript
                .push_notice(&format!("the turn was not saved: {err}"));
        }
        Some(record.id)
    }

    /// Re-arms the last turn's record for `/retry`: the failed record
    /// becomes `Running` again without a second prompt record, because
    /// the retry re-issues the same request. `None` when there is no
    /// record to re-arm, or the ledger refuses to re-open one another
    /// driver settled as permanent; the retry then runs without one,
    /// because the verb is the user's, never the record's.
    pub(crate) fn retry_turn_submission(
        &mut self,
        agent: &mut HostAgent,
    ) -> Option<rhema_submissions::SubmissionId> {
        let snapshot = rhema_submissions::snapshot(agent.session()).ok()?;
        let record = snapshot.iter().rev().find(|record| {
            record.kind.as_str() == rhema_submissions::turn::NAME
                && record.status == rhema_submissions::Status::Failed
        })?;
        let id = record.id.clone();
        let at = rhema_submissions::now();
        match rhema_submissions::retry(agent.session_mut(), &id, at) {
            Ok(_) => {}
            Err(rhema_submissions::SubmissionError::NotRetryable { .. }) => {
                // A permanent verdict this build did not write (it
                // settles every failed turn re-armable, and `reconcile`
                // never fails a turn): a record an older build or a
                // non-TUI driver settled. The retry still runs; only
                // its record is gone, and that is the whole notice.
                self.transcript.push_notice("the retry was not recorded");
                return None;
            }
            Err(err) => {
                self.transcript
                    .push_notice(&format!("the retry was not recorded: {err}"));
                return None;
            }
        }
        self.ensure_checkpoint_target();
        if let Some(path) = self.checkpoint_target()
            && let Err(err) = crate::sessions::write_session(&path, agent.session())
        {
            self.transcript
                .push_notice(&format!("the retry was not saved: {err}"));
        }
        Some(id)
    }

    /// Writes the starts the ledger has not recorded, and settles the
    /// jobs the subscription saw end. A tool cannot write the session it runs
    /// inside, so the subagent registry and the jobs subscription hand over
    /// what they saw, and the frontend writes it at the next point it
    /// owns the agent. The load-time `adopt` covers this window when the
    /// process dies before one.
    ///
    /// The same drain settles the subagent finishes the registry
    /// delivered, a notification line taken or a report fetched: a
    /// record left `Running` reconciles at every future load as one
    /// the process died before announcing, so a resumed session
    /// announces a finish the model already answered, unprompted, as
    /// new. A crash before the delivery still re-announces at load,
    /// which is the durable path.
    pub(crate) fn drain_submission_writes(&mut self) {
        // Nothing to write with while a turn owns the agent: the starts
        // stay in the queues for the next point that does.
        let Some(agent) = self.agent.as_mut() else {
            return;
        };
        let mut changed = false;
        if let Some(handle) = self.subagents.clone() {
            for start in handle.take_starts() {
                let payload = serde_json::to_value(rhema_subagents::Payload {
                    child: start.child,
                    def: start.def,
                    prompt: start.prompt,
                    model: start.model,
                })
                .unwrap_or(serde_json::Value::Null);
                let at = rhema_submissions::now();
                if let Ok(record) = rhema_submissions::submit(
                    agent.session_mut(),
                    rhema_subagents::kind::kind(),
                    payload,
                    at,
                ) && rhema_submissions::start(agent.session_mut(), &record.id, at, None).is_ok()
                {
                    changed = true;
                }
            }
            // The finishes the registry delivered settle their records,
            // oldest retirement first. A restart's second finish finds
            // a record the first one already took to `Failed`: `retry`
            // re-arms a retryable one the way the restart re-ran the
            // work, then the settle lands.
            let settled = handle.take_settled();
            if !settled.is_empty() {
                let running: Vec<(
                    rhema_submissions::SubmissionId,
                    rhema_session_store::SessionId,
                )> = rhema_submissions::snapshot(agent.session())
                    .map(|snapshot| {
                        snapshot
                            .iter()
                            .filter(|record| {
                                record.kind.as_str() == rhema_subagents::kind::NAME
                                    && record.status == rhema_submissions::Status::Running
                            })
                            .filter_map(|record| {
                                serde_json::from_value::<rhema_subagents::Payload>(
                                    record.payload.clone(),
                                )
                                .ok()
                                .map(|payload| (record.id.clone(), payload.child))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                for finish in settled {
                    let Some((id, _)) = running.iter().find(|(_, child)| child == &finish.child)
                    else {
                        continue;
                    };
                    let at = rhema_submissions::now();
                    let mut settled = rhema_submissions::settle(
                        agent.session_mut(),
                        id,
                        finish.outcome.clone(),
                        at,
                    );
                    if settled.is_err() {
                        // The transient failure the restart re-ran: the
                        // re-arm `retry` promises, then the settle.
                        rhema_submissions::retry(agent.session_mut(), id, at).ok();
                        settled = rhema_submissions::settle(
                            agent.session_mut(),
                            id,
                            finish.outcome.clone(),
                            at,
                        );
                    }
                    if settled.is_ok() {
                        changed = true;
                    }
                }
            }
        }
        // The jobs subscription saw its own starts; the same write applies,
        // and a row the ledger already names is skipped: the load's
        // adopt wrote it, and a second record would be a second job. A
        // remote feed has no synchronous kind to recover its records at
        // load, so it drains its queues without writing a record that
        // the next reconcile could not settle.
        if let Some(jobs) = self.jobs.as_ref() {
            if jobs.is_remote() {
                let _ = jobs.take_started();
                let _ = jobs.take_finished_ids();
            } else {
                let known: Vec<String> = rhema_submissions::snapshot(agent.session())
                    .map(|snapshot| {
                        snapshot
                            .iter()
                            .filter(|record| record.kind.as_str() == "job")
                            .filter_map(|record| {
                                serde_json::from_value::<rhema_bash::jobs::kind::Payload>(
                                    record.payload.clone(),
                                )
                                .ok()
                                .map(|payload| payload.id)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                for started in jobs.take_started() {
                    if known.contains(&started.id) {
                        continue;
                    }
                    let payload = serde_json::json!({
                        "tag": jobs.tag(),
                        "id": started.id,
                        "command": started.command,
                    });
                    let at = rhema_submissions::now();
                    if let Ok(record) = rhema_submissions::submit(
                        agent.session_mut(),
                        rhema_submissions::KindName::new(rhema_bash::jobs::kind::NAME)
                            .expect("the job kind name is valid"),
                        payload,
                        at,
                    ) && rhema_submissions::start(agent.session_mut(), &record.id, at, None)
                        .is_ok()
                    {
                        changed = true;
                    }
                }
                // A record settles when the subscription mints its finish,
                // without waiting for a load. The ids come from the feed,
                // so no extra listing is asked.
                let finished = jobs.take_finished_ids();
                if !finished.is_empty() {
                    let running: Vec<(rhema_submissions::SubmissionId, String)> =
                        rhema_submissions::snapshot(agent.session())
                            .map(|snapshot| {
                                snapshot
                                    .iter()
                                    .filter(|record| {
                                        record.kind.as_str() == "job"
                                            && record.status == rhema_submissions::Status::Running
                                    })
                                    .filter_map(|record| {
                                        serde_json::from_value::<rhema_bash::jobs::kind::Payload>(
                                            record.payload.clone(),
                                        )
                                        .ok()
                                        .map(|payload| (record.id.clone(), payload.id))
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                    for (id, backend_id) in running {
                        if finished.contains(&backend_id)
                            && rhema_submissions::settle(
                                agent.session_mut(),
                                &id,
                                rhema_submissions::Outcome::Done,
                                rhema_submissions::now(),
                            )
                            .is_ok()
                        {
                            changed = true;
                        }
                    }
                }
            }
        }
        if changed {
            let session = agent.session().clone();
            if let Some(path) = self.checkpoint_target()
                && let Err(err) = crate::sessions::write_session(&path, &session)
            {
                self.transcript
                    .push_notice(&format!("the start was not saved: {err}"));
            }
        }
    }

    /// Delivers what a reconcile found: unclaimed records name their
    /// missing kind, a recovered job or subagent becomes the finish
    /// notice the live feed would have produced, and an interrupted
    /// subagent is announced with its id so the model can report it.
    pub(crate) fn announce_changes(&mut self, changes: &[rhema_submissions::Change]) {
        if changes.is_empty() {
            return;
        }
        let snapshot = rhema_submissions::snapshot(&self.session).ok();
        for change in changes {
            let (id, kind, to) = match change {
                rhema_submissions::Change::Unclaimed { id, kind } => {
                    self.transcript.push_notice(&format!(
                        "submission {id} belongs to the extension kind {kind}, \
                         which this build does not link"
                    ));
                    continue;
                }
                rhema_submissions::Change::Recovered { id, kind, to } => (id, kind, *to),
            };
            let Some(record) = snapshot.as_ref().and_then(|snapshot| snapshot.get(id)) else {
                continue;
            };
            match kind.as_str() {
                "job" if to == rhema_submissions::Status::Done => {
                    self.announce_recovered_job(&record.payload);
                }
                "subagent" => self.announce_recovered_subagent(&record.payload, to),
                "turn" => {
                    if let Some(snapshot) = snapshot.as_ref() {
                        self.announce_recovered_turn(id, to, snapshot);
                    }
                }
                _ => {}
            }
        }
    }

    /// A job that ended while no process watched it: the listing's row
    /// still names the command and the exit code, so the notice and the
    /// steering line read exactly as the live feed's would.
    fn announce_recovered_job(&mut self, payload: &serde_json::Value) {
        let Ok(payload) =
            serde_json::from_value::<rhema_bash::jobs::kind::Payload>(payload.clone())
        else {
            return;
        };
        let cwd = crate::args::canonical(&self.options.workspace);
        let rows = rhema_bash::jobs::rows_for(
            std::path::Path::new(rhema_bash::jobs::BACKEND),
            &cwd,
            &self.jobs_tag,
        )
        .unwrap_or_default();
        let row = rows.iter().find(|row| row.id == payload.id);
        let command = row.map_or(payload.command.clone(), |row| row.command.clone());
        let exit_code = row.and_then(|row| row.exit_code);
        let line = crate::jobs::announcement(&payload.id, &command, exit_code);
        if let Some(jobs) = &self.jobs {
            jobs.note_finished(crate::jobs::JobFinished {
                id: payload.id,
                command,
                exit_code,
                line,
            });
        } else {
            self.transcript.push_notice(&line);
        }
    }

    /// A subagent's child outlived the parent process: done becomes a
    /// finish whose report the model fetches from the child's file, any
    /// unfinished state is announced with the child's id and started
    /// never.
    fn announce_recovered_subagent(
        &mut self,
        payload: &serde_json::Value,
        to: rhema_submissions::Status,
    ) {
        let Ok(payload) = serde_json::from_value::<rhema_subagents::Payload>(payload.clone())
        else {
            return;
        };
        let name = payload.def;
        let child = payload.child;
        match to {
            rhema_submissions::Status::Done => {
                let line = format!("Subagent {name} ({child}) has finished.");
                if let Some(handle) = &self.subagents {
                    handle.note_finished(rhema_subagents::JobFinished {
                        id: child.to_string(),
                        name,
                        line,
                    });
                } else {
                    self.transcript.push_notice(&line);
                }
            }
            rhema_submissions::Status::Failed => {
                self.transcript.push_notice(&format!(
                    "subagent {name} ({child}) failed; it is not started again"
                ));
            }
            _ => self.transcript.push_notice(&format!(
                "subagent {name} ({child}) is interrupted; it is not started, \
                 and a later request can resume it"
            )),
        }
    }

    /// A turn the crash left unsettled: a committed answer settles
    /// `Done`, and the notice says so with the answer `turn::answer`
    /// derives from the history, so a finished-after-crash turn never
    /// reads as a half one. An unfinished turn is named interrupted and
    /// left alone, the same courtesy a subagent gets.
    fn announce_recovered_turn(
        &mut self,
        id: &rhema_submissions::SubmissionId,
        to: rhema_submissions::Status,
        snapshot: &rhema_submissions::Snapshot,
    ) {
        match to {
            rhema_submissions::Status::Done => {
                match rhema_submissions::turn::answer(&self.session, snapshot, id) {
                    Some(answer) => self.transcript.push_notice(&format!(
                        "the last turn finished while rhema was gone:\n{answer}"
                    )),
                    None => self
                        .transcript
                        .push_notice("the last turn finished while rhema was gone"),
                }
            }
            rhema_submissions::Status::Queued => self.transcript.push_notice(
                "the last turn never started; it stays queued, and loading does not run it",
            ),
            rhema_submissions::Status::Interrupted => self
                .transcript
                .push_notice("the last turn was interrupted; it is not re-run on load"),
            _ => self
                .transcript
                .push_notice("the last turn did not finish cleanly; it is not re-run on load"),
        }
    }

    /// Reads and derives a resume's parts: the session file plus the
    /// usage accounting computed from it, the work the UI thread must
    /// not do. Runs on the blocking pool via
    /// [`crate::turn::spawn_session_resume`].
    pub(crate) fn load_resume_parts(path: &Path) -> std::io::Result<Box<ResumedLoad>> {
        let mut session = HostAgent::load_session_from(&path.to_path_buf())?;
        // A session written before paste spans existed carries the
        // embedded form in its prompts; the provider must never see a
        // mark. One rewrite on the way in (see [`crate::paste`]), so
        // every reader below sees the plain form.
        crate::paste::migrate(&mut session.history);
        let stats = crate::sessions::UsageStats::from_session(&session);
        Ok(Box::new(ResumedLoad { session, stats }))
    }

    /// The settings that apply to this session's profile: field-wise, the
    /// named profile (or the one chosen at startup) over the top-level
    /// section. Computed on demand, it changes mid-session. A `/switch`
    /// pick overrides the stored provider, instance, and model for as
    /// long as the process runs; the config keeps what it stored.
    pub fn effective(&self) -> rhema_config::Profile {
        let mut profile = self.config.effective(self.session_profile.as_deref());
        if let Some(choice) = &self.session_model {
            profile.provider = Some(choice.provider.clone());
            profile.instance = choice.instance.clone();
            profile.model = Some(choice.model.clone());
        }
        profile
    }

    /// The secret store this session resolves stored keys through, as the
    /// resolution tier under the environment; `None` reads literals only.
    pub fn secret_store(&self) -> Option<&dyn rhema_config::secrets::SecretStore> {
        self.secrets.as_deref()
    }

    /// The profiles the input palette completes `/profile` with: the
    /// config registry's rows, the session's own marked.
    pub fn palette_profiles(&self) -> Vec<crate::screens::PaletteProfile> {
        self.config
            .profiles
            .iter()
            .map(|(name, profile)| crate::screens::PaletteProfile {
                name: name.clone(),
                provider: profile.provider.clone(),
                model: profile.model.clone(),
                active: self.session_profile.as_deref() == Some(name.as_str()),
            })
            .collect()
    }

    /// The sandbox instances the input palette completes `/sandbox`
    /// with: the session profile's configured instances, the one the
    /// session currently runs `bash` in marked.
    pub fn palette_sandboxes(&self) -> Vec<crate::screens::PaletteSandbox> {
        let effective = self.config.effective(self.session_profile.as_deref());
        let active_name = effective
            .sandbox
            .as_ref()
            .and_then(|sandbox| sandbox.active.clone());
        let running = self.active_sandbox();
        effective
            .sandbox_instances
            .iter()
            .map(|(name, instance)| crate::screens::PaletteSandbox {
                name: name.clone(),
                kind: instance.kind.clone(),
                target: instance
                    .target
                    .clone()
                    .map(|target| target.trim().to_owned())
                    .filter(|target| !target.is_empty()),
                active: active_name.as_deref() == Some(name.as_str())
                    && SandboxChoice::from_stored(Some(&instance.kind)) == Some(running),
            })
            .collect()
    }

    /// The active profile's name and display color: the `--profile` flag
    /// over the stored active profile, colored by its config (a `#RRGGBB`
    /// string) or by a name-based theme tone.
    pub fn active_profile(&self) -> Option<(String, ratatui::style::Color)> {
        let name = self.session_profile.clone()?;
        let custom = self
            .config
            .profiles
            .get(&name)
            .and_then(|profile| profile.color.as_deref())
            .and_then(crate::palette::parse_hex_color);
        let color =
            custom.unwrap_or_else(|| crate::palette::default_profile_color(&name, &self.theme));
        Some((name, color))
    }

    /// The pricing of the model the session currently runs on, when its
    /// adapter prices it.
    pub(crate) fn current_model_pricing(&self) -> Option<rhema_provider::Pricing> {
        let effective = self.effective();
        let model = effective.model.as_deref()?;
        let active = self.backend.as_ref().map(Backend::config_name);
        self.models
            .iter()
            .find(|entry| entry.model.id == model && Some(entry.provider) == active)
            .and_then(|entry| entry.model.pricing)
    }

    /// Whether the model the session runs on accepts image input, when
    /// the catalog names it: the same row lookup
    /// [`Self::current_model_pricing`] makes. `Some(false)` installs
    /// the agent's image mask; `None` is unknown, not "no", and sends
    /// the images untouched.
    pub(crate) fn current_supports_vision(&self) -> Option<bool> {
        let effective = self.effective();
        let model = effective.model.as_deref()?;
        let active = self.backend.as_ref().map(Backend::config_name);
        self.models
            .iter()
            .find(|entry| entry.model.id == model && Some(entry.provider) == active)
            .and_then(|entry| entry.model.supports_vision)
    }

    /// The background shells this session started, as the last event left
    /// them: running ones only, oldest first.
    pub(crate) fn job_entries(&self) -> Vec<crate::jobs::JobEntry> {
        self.jobs
            .as_ref()
            .map(crate::jobs::JobsHandle::entries)
            .unwrap_or_default()
    }

    /// The background subagent jobs' live state, read off the wire's
    /// registry handle: one entry per working job, queued or running.
    /// A finished job left already; its outcome arrived as a finish
    /// event. Empty without an agent or a wire.
    pub(crate) fn subagent_jobs(&self) -> Vec<rhema_subagents::JobStatus> {
        self.subagents
            .as_ref()
            .map(|handle| handle.status())
            .unwrap_or_default()
    }

    /// The pricing of any catalog model by id, the running subagent's
    /// model may differ from the session's, and its cost prices by its
    /// own card. An unknown id (the catalog never fetched it) prices to
    /// nothing.
    pub(crate) fn model_pricing(&self, model: &str) -> Option<rhema_provider::Pricing> {
        self.models
            .iter()
            .find(|entry| entry.model.id == model)
            .and_then(|entry| entry.model.pricing)
    }

    /// The context window of any catalog model by id, the running
    /// subagent's gauge is sized by its own card. `None` when unknown.
    pub(crate) fn model_window(&self, model: &str) -> Option<u64> {
        self.models
            .iter()
            .find(|entry| entry.model.id == model)
            .and_then(|entry| entry.model.input_token_limit)
    }

    /// The context window of the current model, when known.
    pub fn current_context_window(&self) -> Option<u64> {
        let effective = self.effective();
        let model = effective.model.as_deref()?;
        let active = self.backend.as_ref().map(Backend::config_name);
        self.models
            .iter()
            .find(|entry| entry.model.id == model && Some(entry.provider) == active)
            .and_then(|entry| entry.model.input_token_limit)
    }

    /// The context footprint the footer shows: the latest reported
    /// tally, the last turn's final usage, or while a turn streams the
    /// provider's running count, whose prompt already includes its reply
    ///, otherwise an estimate over the loaded history, which already
    /// includes every message. A resumed session has spent context
    /// before its first turn here, and an empty one estimates to zero.
    pub fn context_used(&self) -> u64 {
        match &self.usage {
            Some(usage) => context_tokens(usage),
            None => estimate_history_tokens(&self.live_session().history),
        }
    }

    /// The session the conversation currently lives in: the agent's when
    /// one is built, the bare one otherwise.
    pub(crate) fn live_session(&self) -> &Session {
        match &self.agent {
            Some(agent) => agent.session(),
            None => &self.session,
        }
    }
}
