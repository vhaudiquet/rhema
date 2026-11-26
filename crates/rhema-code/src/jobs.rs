//! The session's view of its background jobs: the strip's running
//! lines, and the finish feed that announces an end to the model.
//!
//! The jobs themselves are the agent shell's: `job run` starts one
//! through the backend, `job ls` lists this session's. The frontend
//! holds one subscription to the backend's event stream (`gob events
//! --all`, filtered on the session tag the shim writes into every job's
//! description) and splits what arrives in two. A `job_added` or
//! `job_started` feeds the status strip. A `job_stopped` or
//! `job_removed` for a job this subscription tracked running is a
//! finish, minted once into a feed with two readers: the transcript's
//! notice, and the driver's steering, the subagent registry's
//! delivery shape over an event stream instead of a registry, so a
//! job the model started reaches it without a poll of its own.
//!
//! Events are the primary truth, and no timer runs anywhere: the
//! listing is read only twice in a subscription's life, never on a
//! tick. Once at the seed, to adopt what already runs (its later end
//! still announces) and to leave what already ended to the past: a
//! job that started and ended before the subscription opened has no
//! events to deliver. Once at every reconnect, because the stream
//! ends when the daemon does, and the listing is the only way to
//! catch a finish that landed inside the gap. Between the two, a job
//! that starts and ends in milliseconds cannot hide: its two events
//! arrive in order on one pipe.
//!
//! One end is no finish at all: a job the session stopped through its
//! own shim (`job stop`, `job rm`, `job shutdown`) mints nothing. The
//! model ordered that end, the stop's own output was the confirmation,
//! and a notice would spend a turn telling the model what it just did.
//! The shim's ending verbs
//! record the ids they stopped; the subscription reads the session's
//! record before minting, so an ordered stop leaves the strip silently
//! while a stop from anywhere else (the user's terminal, another
//! session, the kernel) still announces. The suppression keeps the
//! notice, not the settle: the end still retires the ledger record,
//! because a record left Running is the one a later resume recovers
//! into the very announcement the model never deserved. The record
//! and the event
//! race now that latency is the daemon's emit: `gob stop` returns
//! before the shim's append lands, so an end re-reads the record once,
//! a moment later, before minting.
//!
//! The tag is the conversation's checkpoint id, not a per-process name:
//! a resumed process lists the jobs its previous one started, and
//! `/resume` restarts this subscription under the loaded tag.
//!
//! One delivery per finish, like a subagent's: steering mid-turn or
//! the idle prompt, never both. The registry earns that with its
//! fetch, the model fetching the report retires everything, so
//! here the delivery itself retires: the drain consumes the line,
//! because the exit code was the whole message. The divergence is
//! the point where the mirror ends, and it is the registry's own
//! rule telling the truth: a query-only drain protects a fetch, and
//! a job has none.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rhema_bash::jobs::{EXIT_NO_BACKEND, EventKind, JobEvent, JobRow};
use rhema_host_agent::Steering;
use rhema_sandbox::{OutputOnly, RunRequest, Sandbox};

/// How long the reconnect waits before reopening the stream, and how
/// long a stop-record re-read gives the shim's append to land. Short:
/// the first is a daemon restart, the second a file write that
/// normally wins the race outright.
const RECONNECT_BACKOFF: Duration = Duration::from_secs(2);

/// The granularity every wait is sliced to, so a dropped handle ends
/// the wait promptly instead of after the whole slice.
const SLICE: Duration = Duration::from_millis(100);

/// The grace a stop-record re-read gives the shim's append. The
/// append follows `gob stop`'s exit within milliseconds; the event
/// can arrive first, because the daemon emits as it kills.
const STOP_RECORD_GRACE: Duration = Duration::from_millis(300);

/// How long the remote poller waits between two listings of a
/// sandbox's gob. A sandbox has no event stream this process can hold:
/// the gob daemon is behind SSH or HTTP, so the listing is the only
/// observation point. Short enough that a finished job reaches the
/// model on the next step, long enough that the exec does not compete
/// with the agent's own shell.
const REMOTE_POLL_INTERVAL: Duration = Duration::from_secs(3);

/// The budget one remote listing gets. A busy or booting sandbox can
/// take seconds; a poll that hangs must not outlive the interval by
/// much, and the next tick retries.
const REMOTE_LIST_TIMEOUT: Duration = Duration::from_secs(15);

/// The line the remote listing script prints between its JSON rows and
/// the self-stop record. The script is shell, so the marker is its only
/// framing.
const REMOTE_STOPPED_MARKER: &str = "__RHEMA_JOBS_STOPPED__";

/// The line the script prints instead of a listing when the helper
/// cannot answer. A failed poll is not an empty one: treating it as
/// empty would mint a vanish-finish for every tracked job.
const REMOTE_FAILED_MARKER: &str = "__RHEMA_JOBS_FAILED__";

/// One job as the strip draws it: the shim's row plus the age this
/// process has watched it, which is the only clock available without
/// parsing the backend's timestamps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobEntry {
    /// What the shim reported.
    pub row: JobRow,
    /// Time since this process first saw the job.
    pub age: Duration,
}

/// The subscription's shared snapshot: one (row, first-seen) pair per
/// running job. Ages are computed at read time, so a quiet job's badge
/// keeps counting between events instead of freezing at the last one.
type Snapshot = Vec<(JobRow, Instant)>;

/// One finished job, as the finish feed delivers it: the transcript
/// notice a frontend pushes the moment a job ends, and, in `line`,
/// the same announcement the driver steers with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobFinished {
    /// The backend's job id: what `job logs` and `job stop` take.
    pub id: String,
    /// The shell line as the model wrote it.
    pub command: String,
    /// The process's exit code, when the backend reported one. `None`
    /// means a stop that came with no code, a signal, or a job gone
    /// from the listing under us.
    pub exit_code: Option<i32>,
    /// The notification line: the notice's text and the steering's.
    pub line: String,
}

/// One finish between minting and its two takes. The latches mirror
/// the subagent registry's: `noticed` retires the transcript notice,
/// `announced` retires the notification line here and in the steering
/// drain. A finish survives until both land, so a notice never costs
/// the model its steering line, and a taken prompt never repeats.
struct Pending {
    finished: JobFinished,
    noticed: bool,
    announced: bool,
}

/// The finish feed: minted transitions, oldest first. The subscription
/// mints, the frontend takes, and the driver reads through
/// [`JobsHandle::steering`].
#[derive(Default)]
struct Feed {
    pending: Vec<Pending>,
    /// Running rows this subscription saw first: the frontend writes
    /// them into the ledger when it next owns the agent. Unbounded
    /// would be a leak in a headless run that never drains; a missed
    /// start is adopted at the next load.
    started: Vec<JobStarted>,
    /// Ids whose end the subscription retired: the frontend settles the
    /// ledger's record for each, without asking the daemon again. A
    /// minted finish lands here with its notice; a self-stop lands here
    /// alone, so its record cannot resurface as a recovered finish at
    /// the next load.
    finished_ids: Vec<String>,
}

/// A job this subscription saw start: the record's payload before a
/// record exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobStarted {
    /// The backend's short id.
    pub id: String,
    /// The shell line as the listing names it.
    pub command: String,
}

impl Feed {
    /// Queues a start for the ledger, bounded: a headless run that
    /// never drains must not grow with the session's jobs. A dropped
    /// start is the record `adopt` rebuilds at the next load.
    fn note_start(&mut self, row: &JobRow) {
        if self.started.len() < 32 {
            self.started.push(JobStarted {
                id: row.id.clone(),
                command: row.command.clone(),
            });
        }
    }

    /// Notes an id the frontend should settle without a notice: a
    /// self-stop retires its ledger record here, the one half of
    /// [`Self::mint`] its suppression keeps.
    fn note_settle(&mut self, id: &str) {
        self.finished_ids.push(id.to_owned());
    }

    /// Mints one finish: the row as it was first seen, plus the exit
    /// code the end reported, `None` when it reported none.
    fn mint(&mut self, seen: JobRow, exit_code: Option<i32>) {
        let line = announcement(&seen.id, &seen.command, exit_code);
        self.finished_ids.push(seen.id.clone());
        self.pending.push(Pending {
            finished: JobFinished {
                id: seen.id,
                command: seen.command,
                exit_code,
                line,
            },
            noticed: false,
            announced: false,
        });
    }

    /// The pending notification lines, oldest first, a pure query:
    /// the same lines come back until a take retires them.
    fn lines(&self) -> Vec<String> {
        self.pending
            .iter()
            .filter(|pending| !pending.announced)
            .map(|pending| pending.finished.line.clone())
            .collect()
    }
}

/// The notification line for one finished job, the driver's steering
/// vocabulary and the finish notice's text, in the registry's shape:
/// what ended, and how.
pub(crate) fn announcement(id: &str, command: &str, exit_code: Option<i32>) -> String {
    match exit_code {
        Some(0) => format!("Background job {id} (\"{command}\") has finished (exit 0)."),
        Some(code) => {
            format!("Background job {id} (\"{command}\") has finished with an error (exit {code}).")
        }
        None => format!("Background job {id} (\"{command}\") was stopped."),
    }
}

/// What the subscription tracks between the seed, the events, and the
/// reconnects. Deliberately less than the poll it replaced: the event
/// stream is the primary truth, so there is no tick to bound, no id
/// set to tell a fresh row from history, because the seed's rule
/// covers that in one place, and nothing to decide per listing.
#[derive(Default)]
struct Watch {
    /// Jobs currently running, in first-seen order, with the row as it
    /// was first seen: a finish announces the command it started with.
    running: Vec<(String, JobRow)>,
    /// Ids whose end this subscription already minted. One end's
    /// event tail (`job_stopped`, then the `job_removed` a later
    /// `job rm` fires) stays one finish; a restart's `job_started`
    /// clears the id, because a new run deserves its own finish.
    finished: HashSet<String>,
}

impl Watch {
    /// Whether `id` is tracked running.
    fn is_tracked(&self, id: &str) -> bool {
        self.running.iter().any(|(tracked, _)| tracked == id)
    }

    /// Drops `id` from the running set.
    fn untrack(&mut self, id: &str) {
        self.running.retain(|(tracked, _)| tracked != id);
    }
}

/// The transition pass over one listing, run at the seed (nothing is
/// tracked yet, so it only adopts) and at every reconnect (the
/// listing is the only way to catch a finish that landed while the
/// stream was down). Every tracked job the listing no longer reports
/// as running has finished: a stopped row gives the exit code, a
/// vanished row finishes with none. An id in `stopped`, the
/// session's self-stop record, leaves the running set without
/// minting but still settles its record, so a later load cannot
/// recover the stop into the finish the model never deserved: the
/// end was the model's own order. The running set keeps
/// the first-seen order, so finishes mint oldest first.
///
/// A running row this subscription never tracked is adopted, with the
/// start its ledger record needs. A stopped row it never tracked is
/// history (it ended before the seed, or inside a gap before this
/// listing) and mints nothing.
fn reconcile(watch: &mut Watch, feed: &mut Feed, rows: &[JobRow], stopped: &HashSet<String>) {
    let mut alive: Vec<(String, JobRow)> = Vec::new();
    for (id, last) in watch.running.drain(..) {
        match rows.iter().find(|row| row.id == id) {
            Some(row) if row.status.starts_with("running") => alive.push((id, row.clone())),
            _ if stopped.contains(&id) => feed.note_settle(&id),
            Some(row) => feed.mint(last, row.exit_code),
            None => feed.mint(last, None),
        }
    }
    for row in rows.iter().filter(|row| row.status.starts_with("running")) {
        if !alive.iter().any(|(id, _)| *id == row.id) {
            feed.note_start(row);
            alive.push((row.id.clone(), row.clone()));
        }
    }
    watch.running = alive;
}

/// One daemon event's transition. Events are the primary truth: a job
/// this subscription saw added or started is tracked (and its ledger
/// start queued), and its end mints the finish the whole feed exists
/// for. An end for a job never tracked is history, it began before
/// the subscription opened, and mints nothing, which is what keeps
/// the seed listing's already-ended rows from announcing twice.
fn on_event(watch: &mut Watch, feed: &mut Feed, stopped: &HashSet<String>, event: &JobEvent) {
    let id = event.row.id.as_str();
    match event.kind {
        EventKind::Added | EventKind::Started => {
            watch.finished.remove(id);
            if !watch.is_tracked(id) {
                feed.note_start(&event.row);
                watch
                    .running
                    .push((event.row.id.clone(), event.row.clone()));
            }
        }
        EventKind::Stopped | EventKind::Removed => {
            if !watch.is_tracked(id) {
                return;
            }
            let last = watch
                .running
                .iter()
                .find(|(tracked, _)| tracked == id)
                .map(|(_, row)| row.clone());
            watch.untrack(id);
            watch.finished.insert(event.row.id.clone());
            if stopped.contains(id) {
                // The suppression keeps the notice, not the settle:
                // the record must not outlive the turn as Running,
                // or the next load recovers it into the very finish
                // the order never deserved.
                feed.note_settle(id);
                return;
            }
            // A removal without a stop reports no code, and neither
            // does a stop the backend saw but did not measure.
            let exit_code = match event.kind {
                EventKind::Stopped => event.row.exit_code,
                EventKind::Removed | EventKind::Added | EventKind::Started => None,
            };
            if let Some(last) = last {
                feed.mint(last, exit_code);
            }
        }
    }
}

/// The session's job view: a shared snapshot the subscription replaces
/// and the renderer reads without touching the filesystem or a
/// process, plus the finish feed the delivery paths read.
pub struct JobsHandle {
    entries: Arc<Mutex<Snapshot>>,
    feed: Arc<Mutex<Feed>>,
    /// The stream's child process, so a dropped handle kills the
    /// subscription instead of orphaning a pipe nothing reads.
    child: Arc<Mutex<Option<Child>>>,
    stop: Arc<AtomicBool>,
    /// This session's tag: the self-stop record's name, removed when
    /// the handle drops, the session's suppressions history with it.
    session: String,
    /// Whether this handle reads a sandbox listing. The startup
    /// reconciler has no async path to that listing, so it registers
    /// the job kind only for a local gob.
    remote: bool,
}

impl Drop for JobsHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Killing the child EOFs the reader, and the thread exits at
        // its next stop check; no lock is held across a read.
        if let Ok(mut slot) = self.child.lock()
            && let Some(mut child) = slot.take()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
        rhema_bash::jobs::forget_stopped(&self.session);
    }
}

impl JobsHandle {
    /// Starts the event subscription over `session`'s tag, or `None`
    /// when there is nothing to subscribe to: no backend installed, or
    /// no binary to run the shim from.
    pub fn spawn(session: String) -> Option<Self> {
        if !backend_ready() {
            return None;
        }
        let bin = std::env::current_exe().ok()?;
        // A copy for the thread: the handle keeps its own, and the
        // record's cleanup reads the handle's after both.
        let thread_session = session.clone();
        let entries = Arc::new(Mutex::new(Snapshot::new()));
        let feed = Arc::new(Mutex::new(Feed::default()));
        let child = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_entries = Arc::clone(&entries);
        let thread_feed = Arc::clone(&feed);
        let thread_child = Arc::clone(&child);
        let thread_stop = Arc::clone(&stop);
        std::thread::Builder::new()
            .name("rhema-jobs".to_owned())
            .spawn(move || {
                subscribe(
                    bin,
                    thread_session,
                    thread_entries,
                    thread_feed,
                    thread_child,
                    thread_stop,
                )
            })
            .ok()?;
        Some(Self {
            entries,
            feed,
            child,
            stop,
            session,
            remote: false,
        })
    }

    /// Starts the listing poller over a sandbox's own gob, or `None`
    /// when there is no sandbox to poll.
    ///
    /// The local [`Self::spawn`] holds `gob events` open. A sandboxed
    /// session has no such stream: the daemon lives behind SSH or
    /// HTTP. This poller runs the jobs shim inside the sandbox on a
    /// timer, reconciles the listing exactly like a reconnect, and
    /// mints the same finish feed. The trade is latency, not semantics.
    pub fn spawn_remote(
        sandbox: Arc<dyn Sandbox>,
        workspace: String,
        session: String,
        init: String,
        session_file: String,
    ) -> Option<Self> {
        if session.is_empty() {
            return None;
        }
        let entries = Arc::new(Mutex::new(Snapshot::new()));
        let feed = Arc::new(Mutex::new(Feed::default()));
        let child = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_entries = Arc::clone(&entries);
        let thread_feed = Arc::clone(&feed);
        let thread_stop = Arc::clone(&stop);
        let thread_session = session.clone();
        std::thread::Builder::new()
            .name("rhema-jobs-remote".to_owned())
            .spawn(move || {
                poll_remote(
                    RemoteJobs {
                        sandbox,
                        workspace,
                        session: thread_session,
                        init,
                        session_file,
                    },
                    thread_entries,
                    thread_feed,
                    thread_stop,
                )
            })
            .ok()?;
        Some(Self {
            entries,
            feed,
            child,
            stop,
            session,
            remote: true,
        })
    }

    /// The tag this subscription reads: the conversation's checkpoint
    /// id, so a resumed process lists the same jobs it started.
    pub fn tag(&self) -> &str {
        &self.session
    }

    /// Whether this feed reads a sandbox's listing rather than this
    /// host's `gob events`.
    pub fn is_remote(&self) -> bool {
        self.remote
    }

    /// Adds a finish the subscription did not observe, the
    /// recovered-record path: a job that ended while no process ran
    /// still reaches the transcript and the model, once, in the live
    /// feed's own wording.
    pub fn note_finished(&self, finished: JobFinished) {
        if let Ok(mut feed) = self.feed.lock() {
            feed.finished_ids.push(finished.id.clone());
            feed.pending.push(Pending {
                finished,
                noticed: false,
                announced: false,
            });
        }
    }

    /// Takes the backend ids the subscription retired: the frontend
    /// settles the ledger's record for each. A minted finish and a
    /// suppressed self-stop both land here; only the former carries a
    /// notice.
    pub fn take_finished_ids(&self) -> Vec<String> {
        self.feed
            .lock()
            .map(|mut feed| std::mem::take(&mut feed.finished_ids))
            .unwrap_or_default()
    }

    /// Takes the starts the subscription observed, oldest first: the
    /// frontend writes a `job` record for each, unless the ledger
    /// already names the row.
    pub fn take_started(&self) -> Vec<JobStarted> {
        self.feed
            .lock()
            .map(|mut feed| std::mem::take(&mut feed.started))
            .unwrap_or_default()
    }

    /// Notes a start for the tests: no subscription thread, just the
    /// queue.
    #[cfg(test)]
    pub fn note_started(&self, started: JobStarted) {
        if let Ok(mut feed) = self.feed.lock() {
            feed.started.push(started);
        }
    }

    /// The snapshot the last event left, oldest first. Ages are
    /// computed here, at read time, so the strip keeps counting
    /// between events.
    pub fn entries(&self) -> Vec<JobEntry> {
        let now = Instant::now();
        self.entries
            .lock()
            .map(|snapshot| {
                let mut entries: Vec<JobEntry> = snapshot
                    .iter()
                    .map(|(row, since)| JobEntry {
                        row: row.clone(),
                        age: now.saturating_duration_since(*since),
                    })
                    .collect();
                entries.sort_by_key(|entry| entry.age);
                entries
            })
            .unwrap_or_default()
    }

    /// Takes the finish events out, oldest first, one per job,
    /// exactly once. The transcript notice feed: a frontend pushes
    /// each event's `line` the moment the job ends, whatever turn is
    /// running. Taking a notice retires nothing else: the line stays
    /// pending for the driver's steering until
    /// [`Self::take_notifications`].
    pub fn take_finished(&self) -> Vec<JobFinished> {
        let Ok(mut feed) = self.feed.lock() else {
            return Vec::new();
        };
        let taken: Vec<JobFinished> = feed
            .pending
            .iter()
            .filter(|pending| !pending.noticed)
            .map(|pending| pending.finished.clone())
            .collect();
        for pending in feed.pending.iter_mut() {
            pending.noticed = true;
        }
        feed.pending
            .retain(|pending| !(pending.noticed && pending.announced));
        taken
    }

    /// Takes the pending notification lines out, oldest first,
    /// retiring them here and in the steering drain. The idle path: a
    /// frontend with no running turn runs the lines as the next
    /// turn's prompt, and this take keeps the driver's turn-start
    /// drain from repeating what the prompt already said. While a
    /// turn runs, the lines belong to the driver's steering: do not
    /// call this.
    pub fn take_notifications(&self) -> Vec<String> {
        let Ok(mut feed) = self.feed.lock() else {
            return Vec::new();
        };
        let lines = feed.lines();
        for pending in feed.pending.iter_mut() {
            pending.announced = true;
        }
        feed.pending
            .retain(|pending| !(pending.noticed && pending.announced));
        lines
    }

    /// The driver's steering source over this feed: one notification
    /// line per finished-but-undelivered job, and the drain consumes,
    /// a steered line never comes back, so a finish the driver
    /// announced mid-turn does not re-deliver as the idle session's
    /// prompt. A frontend with both feeds chains this with the
    /// subagent registry's through `SteeringChain` into the config's
    /// one `steering` slot; the registry's drain stays a query, its
    /// retirement being the fetch or the idle take.
    pub fn steering(&self) -> Arc<dyn Steering> {
        Arc::new(JobSteering(Arc::clone(&self.feed)))
    }
}

/// The steering source over one feed. Unlike the subagent registry's,
/// the drain retires what it returns: a steered line is delivered,
/// its whole payload, the exit code, rode inside it, and the card
/// entered the transcript when the finish was detected. The registry
/// re-announces because an unfetched report is expensive work that
/// must not be lost; a job has nothing to fetch.
struct JobSteering(Arc<Mutex<Feed>>);

impl Steering for JobSteering {
    fn drain(&self) -> Vec<String> {
        let Ok(mut feed) = self.0.lock() else {
            return Vec::new();
        };
        let lines = feed.lines();
        for pending in feed.pending.iter_mut() {
            pending.announced = true;
        }
        // A finish both noticed and announced is fully delivered. One
        // announced but never noticed (headless: no transcript exists
        // to notice it) stays in the feed, inert, until the handle
        // drops, bounded by the job count, never re-delivered.
        feed.pending
            .retain(|pending| !(pending.noticed && pending.announced));
        lines
    }
}

/// The tag a checkpoint file names: the conversation's store id, which
/// survives a restart and is what a resumed process lists its jobs by.
/// `None` for a path whose stem is not a valid session id.
pub fn tag_from_path(path: &std::path::Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    rhema_session_store::SessionId::new(stem).ok()?;
    Some(stem.to_owned())
}

/// The tag a process without a checkpoint uses: unique per process, and
/// honestly not resumable, so its jobs are nobody's after it exits.
pub fn fallback_tag() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // `s` for readability in the backend's own listing, then the pid
    // and the low nanos: a restarted process lands on a different id
    // even where the pid was reused.
    format!("s{:x}{:06x}", std::process::id(), nanos & 0xff_ffff)
}

#[cfg(test)]
impl JobsHandle {
    /// A handle over a fixed snapshot: the renderer's tests draw the
    /// strip without a thread, a backend, or a clock. The ages given
    /// here are what `entries` reports, backdated to first-seen.
    pub(crate) fn stub(entries: Vec<JobEntry>) -> Self {
        let now = Instant::now();
        Self {
            entries: Arc::new(Mutex::new(
                entries
                    .into_iter()
                    .map(|entry| (entry.row, now.checked_sub(entry.age).unwrap_or(now)))
                    .collect(),
            )),
            feed: Arc::new(Mutex::new(Feed::default())),
            child: Arc::new(Mutex::new(None)),
            stop: Arc::new(AtomicBool::new(false)),
            session: fallback_tag(),
            remote: false,
        }
    }

    /// A handle over a pre-minted finish feed: the lifecycle's tests
    /// deliver notifications without a thread, a backend, or a stream.
    pub(crate) fn stub_with_finished(finished: Vec<JobFinished>) -> Self {
        Self {
            entries: Arc::new(Mutex::new(Snapshot::new())),
            feed: Arc::new(Mutex::new(Feed {
                pending: finished
                    .into_iter()
                    .map(|finished| Pending {
                        finished,
                        noticed: false,
                        announced: false,
                    })
                    .collect(),
                started: Vec::new(),
                finished_ids: Vec::new(),
            })),
            child: Arc::new(Mutex::new(None)),
            stop: Arc::new(AtomicBool::new(false)),
            session: fallback_tag(),
            remote: false,
        }
    }
}

/// What one listing read answered.
enum Listing {
    /// The backend binary is gone: the subscription has nothing left
    /// to watch, and the next read would fail the same way.
    NoBackend,
    /// This read failed. Keep the stream; a skipped reconcile is a
    /// missed gap catch, never a false finish.
    Failed,
    /// The rows, already scoped to this session's tag.
    Rows(Vec<JobRow>),
}

/// One listing, as the seed and each reconnect's reconcile ask for it:
/// the shim's own listing, scoped by the tag.
fn listing(bin: &Path, session: &str) -> Listing {
    let output = std::process::Command::new(bin)
        .arg(rhema_bash::jobs::MODE)
        .arg("ls")
        .arg("--json")
        .arg("--session")
        .arg(session)
        .output();
    let Ok(output) = output else {
        return Listing::Failed;
    };
    if output.status.code() == Some(EXIT_NO_BACKEND) {
        return Listing::NoBackend;
    }
    // A listing that fails to parse is not an empty one: reading it
    // as empty would mint a vanish-finish for every tracked job.
    serde_json::from_slice::<Vec<JobRow>>(&output.stdout)
        .map(Listing::Rows)
        .map_err(|_| ())
        .unwrap_or(Listing::Failed)
}

/// Rebuilds the strip's snapshot from the tracked running set. The
/// first-seen instants ride beside it, so ages are computed at read
/// time and keep counting between events.
fn publish(shared: &Mutex<Snapshot>, seen: &mut HashMap<String, Instant>, watch: &Watch) {
    let now = Instant::now();
    seen.retain(|id, _| watch.is_tracked(id));
    let snapshot: Snapshot = watch
        .running
        .iter()
        .map(|(id, row)| (row.clone(), *seen.entry(id.clone()).or_insert(now)))
        .collect();
    if let Ok(mut shared) = shared.lock() {
        *shared = snapshot;
    }
}

/// The backend the subscription opens its stream against: the one
/// backend name, or the test seam's script when a test has set one.
fn backend_path() -> PathBuf {
    #[cfg(test)]
    if let Some(path) = TEST_BACKEND.lock().expect("the test backend lock").clone() {
        return path;
    }
    PathBuf::from(rhema_bash::jobs::BACKEND)
}

/// Whether a backend answers at all: the test seam's script when a
/// test has set one, otherwise the production gate on the real
/// binary's `help add`. Reads the same seam [`backend_path`] streams
/// against, so a test's spawn and its subscription see one backend.
fn backend_ready() -> bool {
    #[cfg(test)]
    if TEST_BACKEND
        .lock()
        .expect("the test backend lock")
        .is_some()
    {
        return true;
    }
    rhema_bash::jobs::available()
}

/// The test seam behind [`backend_path`]: the end-to-end test points
/// it at a script that speaks the backend's two commands, the way
/// [`rhema_bash::jobs::JobKind::at`] takes an explicit backend for the
/// listing. Compiled out of production, where the name is the name.
#[cfg(test)]
static TEST_BACKEND: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Sets or clears the test backend for this process.
#[cfg(test)]
fn set_test_backend(path: Option<PathBuf>) {
    *TEST_BACKEND.lock().expect("the test backend lock") = path;
}

/// The subscription's watchdog child, killed and reaped when its
/// iteration ends, whichever way it ends: a reconnect must not leave an
/// `sh` behind to be reaped only when the frontend exits.
#[cfg(unix)]
struct Watchdog(std::process::Child);

#[cfg(unix)]
impl Drop for Watchdog {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Sleeps `duration` in slices, so a dropped handle ends the wait
/// promptly. `true` when the stop flag was seen.
fn sleep_slices(stop: &AtomicBool, duration: Duration) -> bool {
    let mut slept = Duration::ZERO;
    while slept < duration {
        if stop.load(Ordering::Relaxed) {
            return true;
        }
        std::thread::sleep(SLICE);
        slept += SLICE;
    }
    false
}

/// The subscription loop: open the stream, seed, read events line by
/// line, and on an EOF (the daemon died or restarted) reconcile the
/// gap and open again. Exits when the handle drops or the backend
/// vanishes.
fn subscribe(
    bin: PathBuf,
    session: String,
    shared: Arc<Mutex<Snapshot>>,
    feed: Arc<Mutex<Feed>>,
    child_slot: Arc<Mutex<Option<Child>>>,
    stop: Arc<AtomicBool>,
) {
    let mut watch = Watch::default();
    let mut seen: HashMap<String, Instant> = HashMap::new();
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let mut child = match rhema_bash::jobs::events_command(&backend_path())
            .stdout(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                // The backend is gone: there is nothing left to
                // subscribe to, and the next attempt would fail the
                // same way.
                return;
            }
            Err(_) => {
                if sleep_slices(&stop, RECONNECT_BACKOFF) {
                    return;
                }
                continue;
            }
        };
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                let _ = child.wait();
                if sleep_slices(&stop, RECONNECT_BACKOFF) {
                    return;
                }
                continue;
            }
        };
        // The subscription's child is the one long-lived process the
        // frontend owns, and a process that exits without dropping
        // its handles (a crash, a signal, a test run's end) leaves
        // the stream orphaned: the events command reads no stdin and
        // exits on nothing but its daemon. The watchdog closes that
        // gap: while this process and the child both live it sleeps,
        // and when either goes the child is killed. A clean drop
        // kills the child directly and the watchdog exits at its
        // next check; a killed process leaves the watchdog one sleep
        // behind, then the child dies too.
        // The handle is held for the iteration so the exit path reaps
        // it: one `sh` per reconnect used to outlive its own loop as a
        // zombie under a long-lived frontend.
        #[cfg(unix)]
        let _watchdog = std::process::Command::new("sh")
            .arg("-c")
            .arg(
                r#"while kill -0 "$1" 2>/dev/null && kill -0 "$2" 2>/dev/null; do sleep 1; done
kill "$2" 2>/dev/null"#,
            )
            .arg("rhema-jobs-watchdog")
            .arg(std::process::id().to_string())
            .arg(child.id().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()
            .map(Watchdog);
        // Poison only means a panic elsewhere; recovering keeps the
        // child where the handle drop finds and kills it, instead of
        // dropping it here and orphaning the event-stream process.
        {
            let mut slot = child_slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *slot = Some(child);
        }
        if stop.load(Ordering::Relaxed) {
            // The handle dropped while this child was being spawned: its
            // drop found the slot empty and killed nothing, so this is
            // the only place that can reap it. Without this the stream
            // outlives its frontend until the process exits.
            if let Ok(mut slot) = child_slot.lock()
                && let Some(mut child) = slot.take()
            {
                let _ = child.kill();
                let _ = child.wait();
            }
            return;
        }
        // The seed, and every reconnect's reconcile: adopt what runs,
        // mint what this subscription tracked and the listing says
        // ended inside the gap.
        match listing(&bin, &session) {
            Listing::NoBackend => return,
            Listing::Rows(rows) => {
                let stopped = rhema_bash::jobs::stopped_ids(&session);
                if let Ok(mut feed) = feed.lock() {
                    reconcile(&mut watch, &mut feed, &rows, &stopped);
                }
                publish(&shared, &mut seen, &watch);
            }
            Listing::Failed => {}
        }
        let mut reader = std::io::BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            if stop.load(Ordering::Relaxed) {
                return;
            }
            let Some(event) = rhema_bash::jobs::parse_event_line(line.trim()) else {
                continue;
            };
            // Scoping is the tag: a job another session started, or a
            // hand-run `gob add`, never announces here.
            if event.row.session.as_deref() != Some(session.as_str()) {
                continue;
            }
            // The shim appends its stop record after `gob stop`
            // returns, and the daemon emits as it kills, so at this
            // latency the record can lose the race. An end of a
            // tracked job re-reads the record once, a moment later,
            // before minting; ends of untracked jobs mint nothing
            // either way.
            let ends = matches!(event.kind, EventKind::Stopped | EventKind::Removed);
            let stopped = if ends && watch.is_tracked(&event.row.id) {
                let mut stopped = rhema_bash::jobs::stopped_ids(&session);
                if !stopped.contains(&event.row.id) && sleep_slices(&stop, STOP_RECORD_GRACE) {
                    return;
                }
                if !stopped.contains(&event.row.id) {
                    stopped = rhema_bash::jobs::stopped_ids(&session);
                }
                stopped
            } else {
                HashSet::new()
            };
            if let Ok(mut feed) = feed.lock() {
                on_event(&mut watch, &mut feed, &stopped, &event);
            }
            publish(&shared, &mut seen, &watch);
        }
        // The stream ended: reap the child, wait out a daemon
        // restart, and the next round's reconcile catches whatever
        // finished inside the gap.
        if let Ok(mut slot) = child_slot.lock()
            && let Some(mut child) = slot.take()
        {
            let _ = child.wait();
        }
        if sleep_slices(&stop, RECONNECT_BACKOFF) {
            return;
        }
    }
}

/// What one remote poll needs: the sandbox, where its shell starts,
/// and the init file that defines the `job` macro. A struct because the
/// loop would otherwise carry five positional strings into every call.
struct RemoteJobs {
    sandbox: Arc<dyn Sandbox>,
    workspace: String,
    session: String,
    init: String,
    session_file: String,
}

/// The remote poll loop: one listing every [`REMOTE_POLL_INTERVAL`],
/// reconciled like a reconnect. The runtime is this thread's own; the
/// sandbox backend's async clients do not need the frontend's.
fn poll_remote(
    jobs: RemoteJobs,
    shared: Arc<Mutex<Snapshot>>,
    feed: Arc<Mutex<Feed>>,
    stop: Arc<AtomicBool>,
) {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };
    let mut watch = Watch::default();
    let mut seen: HashMap<String, Instant> = HashMap::new();
    let mut stopped: HashSet<String> = HashSet::new();
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let listing = runtime.block_on(remote_listing(&jobs, &stop));
        if let Some((rows, recorded)) = listing {
            stopped.extend(recorded);
            if let Ok(mut feed) = feed.lock() {
                reconcile(&mut watch, &mut feed, &rows, &stopped);
            }
            publish(&shared, &mut seen, &watch);
        }
        if sleep_slices(&stop, REMOTE_POLL_INTERVAL) {
            return;
        }
    }
}

/// One remote listing and the session's self-stop record, in one exec.
/// `None` when the poll could not answer: an unreachable sandbox, a
/// helper that failed, or a listing that did not parse. A missed poll
/// is skipped, never read as an empty one.
async fn remote_listing(
    jobs: &RemoteJobs,
    stop: &AtomicBool,
) -> Option<(Vec<JobRow>, HashSet<String>)> {
    let tag = rhema_sandbox::exec_util::shquote(&jobs.session);
    let init = rhema_sandbox::exec_util::shquote(&jobs.init);
    // The init file defines `job` and puts the installed `gob` on PATH.
    // The final `|| true` keeps the exec green when there is no stop
    // record: an absent record is the normal case, not a failed poll.
    let command = format!(
        ". {init}\n\
         tag={tag}\n\
         if out=$(job ls --json --session \"$tag\" 2>/dev/null); then\n\
           printf '%s\\n' \"$out\"\n\
         else\n\
           printf '%s\\n' {failed}\n\
         fi\n\
         printf '%s\\n' {marker}\n\
         f=\"${{TMPDIR:-/tmp}}/rhema-jobs-$tag-stopped\"\n\
         if [ -f \"$f\" ]; then mv \"$f\" \"$f.$$\" 2>/dev/null && {{ cat \"$f.$$\"; rm -f \"$f.$$\"; }}; fi || true\n",
        failed = REMOTE_FAILED_MARKER,
        marker = REMOTE_STOPPED_MARKER,
    );
    let mut env = BTreeMap::new();
    env.insert(
        rhema_bash::jobs::VAR_SESSION.to_owned(),
        jobs.session.clone(),
    );
    env.insert(
        rhema_bash::jobs::VAR_SESSION_FILE.to_owned(),
        jobs.session_file.clone(),
    );
    let mut progress = OutputOnly(|_| {});
    let outcome = jobs
        .sandbox
        .run(
            RunRequest {
                command,
                cwd: PathBuf::from(&jobs.workspace),
                timeout: REMOTE_LIST_TIMEOUT,
                env,
            },
            stop,
            &mut progress,
        )
        .await
        .ok()?;
    parse_remote_listing(&String::from_utf8_lossy(&outcome.stdout))
}

/// Parses the listing script's output: the JSON rows, the marker, then
/// the self-stop ids. `None` for a failed or malformed poll, which the
/// loop skips instead of reconciling.
fn parse_remote_listing(stdout: &str) -> Option<(Vec<JobRow>, HashSet<String>)> {
    let (rows, stopped) = stdout.split_once(REMOTE_STOPPED_MARKER)?;
    let rows = rows.trim();
    if rows.is_empty() || rows == REMOTE_FAILED_MARKER {
        return None;
    }
    let rows = serde_json::from_str::<Vec<JobRow>>(rows).ok()?;
    let stopped = stopped
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    Some((rows, stopped))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One row as the listings carry it.
    fn row(id: &str, status: &str, exit_code: Option<i32>) -> JobRow {
        JobRow {
            id: id.to_owned(),
            status: status.to_owned(),
            exit_code,
            command: "npm run dev".to_owned(),
            session: Some("s1".to_owned()),
            started_at: None,
        }
    }

    /// One daemon event about `id`, running when the kind begins a
    /// run and stopped when it ends one.
    fn event(kind: EventKind, id: &str, exit_code: Option<i32>) -> JobEvent {
        let status = match kind {
            EventKind::Added | EventKind::Started => "running",
            EventKind::Stopped | EventKind::Removed => "stopped",
        };
        JobEvent {
            kind,
            row: row(id, status, exit_code),
        }
    }

    /// A subscription that has watched `rows` running since its seed:
    /// the state a transition starts from.
    fn watching(rows: &[JobRow]) -> Watch {
        Watch {
            running: rows
                .iter()
                .map(|row| (row.id.clone(), row.clone()))
                .collect(),
            finished: HashSet::new(),
        }
    }

    #[test]
    fn the_seed_adopts_running_rows_and_leaves_ended_ones_to_history() {
        let mut watch = Watch::default();
        let mut feed = Feed::default();
        let none = HashSet::new();
        reconcile(
            &mut watch,
            &mut feed,
            &[row("abc", "stopped", Some(0)), row("def", "running", None)],
            &none,
        );
        assert!(feed.pending.is_empty(), "no watch, no finish");
        // The running one is taken up, with the start its record needs.
        assert_eq!(watch.running.len(), 1);
        let started: Vec<&str> = feed.started.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(started, vec!["def"]);
    }

    #[test]
    fn a_tracked_job_the_reconcile_says_ended_mints_once() {
        let mut watch = watching(&[row("abc", "running", None)]);
        let mut feed = Feed::default();
        let none = HashSet::new();
        reconcile(
            &mut watch,
            &mut feed,
            &[row("abc", "stopped", Some(1))],
            &none,
        );
        assert!(watch.running.is_empty(), "the job left the running set");
        assert_eq!(feed.pending.len(), 1);
        let finished = &feed.pending[0].finished;
        assert_eq!(finished.id, "abc");
        assert_eq!(finished.exit_code, Some(1));
        assert_eq!(
            finished.line,
            "Background job abc (\"npm run dev\") has finished with an error (exit 1)."
        );
    }

    #[test]
    fn a_tracked_job_that_vanishes_in_a_gap_finishes_stopped() {
        let mut watch = watching(&[row("abc", "running", None)]);
        let mut feed = Feed::default();
        let none = HashSet::new();
        reconcile(&mut watch, &mut feed, &[], &none);
        assert_eq!(feed.pending.len(), 1);
        assert_eq!(feed.pending[0].finished.exit_code, None);
        assert_eq!(
            feed.pending[0].finished.line,
            "Background job abc (\"npm run dev\") was stopped."
        );
    }

    #[test]
    fn an_event_stream_mints_a_fast_job_end_to_end() {
        // The race the poll could never close: a job that starts and
        // ends in milliseconds. Its two events arrive in order, and
        // both are processed.
        let mut watch = Watch::default();
        let mut feed = Feed::default();
        let none = HashSet::new();
        on_event(
            &mut watch,
            &mut feed,
            &none,
            &event(EventKind::Added, "abc", None),
        );
        assert_eq!(watch.running.len(), 1, "the add tracks the job");
        assert_eq!(feed.started.len(), 1, "the start its record needs");
        on_event(
            &mut watch,
            &mut feed,
            &none,
            &event(EventKind::Stopped, "abc", Some(0)),
        );
        assert_eq!(feed.pending.len(), 1);
        assert_eq!(feed.pending[0].finished.exit_code, Some(0));
        assert_eq!(
            feed.pending[0].finished.line,
            "Background job abc (\"npm run dev\") has finished (exit 0)."
        );
    }

    #[test]
    fn a_stop_and_its_remove_are_one_finish() {
        let mut watch = Watch::default();
        let mut feed = Feed::default();
        let none = HashSet::new();
        on_event(
            &mut watch,
            &mut feed,
            &none,
            &event(EventKind::Added, "abc", None),
        );
        on_event(
            &mut watch,
            &mut feed,
            &none,
            &event(EventKind::Stopped, "abc", Some(1)),
        );
        on_event(
            &mut watch,
            &mut feed,
            &none,
            &event(EventKind::Removed, "abc", None),
        );
        assert_eq!(feed.pending.len(), 1, "the same end, one finish");
    }

    #[test]
    fn a_restart_mints_its_own_finish() {
        let mut watch = Watch::default();
        let mut feed = Feed::default();
        let none = HashSet::new();
        on_event(
            &mut watch,
            &mut feed,
            &none,
            &event(EventKind::Added, "abc", None),
        );
        on_event(
            &mut watch,
            &mut feed,
            &none,
            &event(EventKind::Stopped, "abc", Some(0)),
        );
        on_event(
            &mut watch,
            &mut feed,
            &none,
            &event(EventKind::Started, "abc", None),
        );
        assert_eq!(watch.running.len(), 1, "the restart tracks again");
        on_event(
            &mut watch,
            &mut feed,
            &none,
            &event(EventKind::Stopped, "abc", Some(2)),
        );
        assert_eq!(feed.pending.len(), 2, "a new run, a new finish");
        assert_eq!(feed.pending[1].finished.exit_code, Some(2));
    }

    #[test]
    fn an_end_the_session_ordered_is_silent() {
        let mut watch = Watch::default();
        let mut feed = Feed::default();
        on_event(
            &mut watch,
            &mut feed,
            &HashSet::new(),
            &event(EventKind::Added, "abc", None),
        );
        let stopped: HashSet<String> = ["abc".to_owned()].into();
        on_event(
            &mut watch,
            &mut feed,
            &stopped,
            &event(EventKind::Stopped, "abc", Some(1)),
        );
        assert!(
            watch.running.is_empty(),
            "the job still left the running set"
        );
        assert!(feed.pending.is_empty(), "no notice for an ordered stop");
        // The suppression keeps the notice, not the settle: the id
        // still retires, or a resume recovers the record Running and
        // announces the model's own stop as news.
        assert_eq!(feed.finished_ids, ["abc".to_owned()]);
        // The `job rm` tail stays silent with it.
        on_event(
            &mut watch,
            &mut feed,
            &stopped,
            &event(EventKind::Removed, "abc", None),
        );
        assert!(feed.pending.is_empty());
        // One the session did not stop still announces.
        let mut watch = Watch::default();
        on_event(
            &mut watch,
            &mut feed,
            &HashSet::new(),
            &event(EventKind::Added, "def", None),
        );
        on_event(
            &mut watch,
            &mut feed,
            &stopped,
            &event(EventKind::Stopped, "def", Some(0)),
        );
        assert_eq!(feed.pending.len(), 1);
    }

    #[test]
    fn an_end_for_a_job_never_tracked_mints_nothing() {
        // The seed listing's already-ended rows, and any end whose
        // begin predates the subscription: history, not news.
        let mut watch = Watch::default();
        let mut feed = Feed::default();
        let none = HashSet::new();
        on_event(
            &mut watch,
            &mut feed,
            &none,
            &event(EventKind::Stopped, "abc", Some(0)),
        );
        on_event(
            &mut watch,
            &mut feed,
            &none,
            &event(EventKind::Removed, "abc", None),
        );
        assert!(feed.pending.is_empty());
    }

    #[test]
    fn a_job_the_session_stopped_in_a_gap_stays_silent_too() {
        // The stop happened while the stream was down; the reconnect's
        // reconcile consults the record the same way an event does.
        let mut watch = watching(&[row("abc", "running", None)]);
        let mut feed = Feed::default();
        let stopped: HashSet<String> = ["abc".to_owned()].into();
        reconcile(
            &mut watch,
            &mut feed,
            &[row("abc", "stopped", Some(1))],
            &stopped,
        );
        assert!(watch.running.is_empty());
        assert!(feed.pending.is_empty(), "the model ordered this end");
        // The gap's stop settles like the live event's does.
        assert_eq!(feed.finished_ids, ["abc".to_owned()]);
    }

    #[test]
    fn finishes_mint_in_event_order() {
        let mut watch = watching(&[row("aaa", "running", None), row("bbb", "running", None)]);
        let mut feed = Feed::default();
        let none = HashSet::new();
        reconcile(
            &mut watch,
            &mut feed,
            &[
                row("aaa", "stopped", Some(0)),
                row("bbb", "stopped", Some(2)),
            ],
            &none,
        );
        let ids: Vec<&str> = feed
            .pending
            .iter()
            .map(|pending| pending.finished.id.as_str())
            .collect();
        assert_eq!(ids, vec!["aaa", "bbb"]);
    }

    #[test]
    fn the_exit_zero_vocabulary_says_finished() {
        assert_eq!(
            announcement("abc", "npm run dev", Some(0)),
            "Background job abc (\"npm run dev\") has finished (exit 0)."
        );
        assert_eq!(
            announcement("abc", "npm run dev", None),
            "Background job abc (\"npm run dev\") was stopped."
        );
    }

    #[test]
    fn the_steering_drain_delivers_exactly_once() {
        let handle = JobsHandle::stub_with_finished(vec![JobFinished {
            id: "abc".to_owned(),
            command: "npm run dev".to_owned(),
            exit_code: Some(0),
            line: "Background job abc (\"npm run dev\") has finished (exit 0).".to_owned(),
        }]);
        let line = "Background job abc (\"npm run dev\") has finished (exit 0).".to_owned();
        // The notice: once.
        assert_eq!(handle.take_finished().len(), 1);
        assert!(handle.take_finished().is_empty());
        // The steering delivers the line, and consumes it. A steered
        // finish never re-delivers as the idle prompt: the exit code
        // rode inside the line, there is nothing left to fetch.
        assert_eq!(handle.steering().drain(), vec![line.clone()]);
        assert!(handle.steering().drain().is_empty());
        assert!(handle.take_notifications().is_empty());
    }

    /// The thread glue, end to end: a fake backend that answers the
    /// gate's probe and streams a real event pair, the subscription
    /// opened against it, and the finish delivered through the public
    /// takes. No daemon, no network, one script.
    #[test]
    #[cfg(unix)]
    fn a_subscription_delivers_a_real_stream_end_to_end() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let backend = dir.path().join("fake-gob");
        std::fs::write(
            &backend,
            r#"#!/bin/sh
case "$1 $2" in
  "help add") echo "  --description DESC   tag the job"; exit 0;;
  "events --all")
    echo '{"type":"job_added","job_id":"e2j","job":{"id":"e2j","status":"running","command":["bash","-c","cargo build"],"description":"[e2s] cargo build"}}'
    echo '{"type":"job_stopped","job_id":"e2j","job":{"id":"e2j","status":"stopped","command":["bash","-c","cargo build"],"description":"[e2s] cargo build","exit_code":3}}'
    sleep 5;;
  *) exit 0;;
esac
"#,
        )
        .expect("the fake backend is written");
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&backend).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&backend, perms).unwrap();
        }
        // The seam is process-wide: this test is its only user, and
        // no other test in this binary spawns a subscription.
        set_test_backend(Some(backend.clone()));
        let handle = JobsHandle::spawn("e2s".to_owned()).expect("the gate passes on the fake");
        // A take consumes what it returns, so the wait keeps the
        // result instead of re-taking an already-noticed finish.
        let mut finished = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while finished.is_empty() && Instant::now() < deadline {
            finished = handle.take_finished();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(finished.len(), 1, "one finish minted: {finished:?}");
        assert_eq!(finished[0].id, "e2j");
        assert_eq!(finished[0].exit_code, Some(3));
        assert_eq!(
            finished[0].line,
            "Background job e2j (\"cargo build\") has finished with an error (exit 3)."
        );
        // The ledger start rode in on the added event.
        let mut started = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        while started.is_empty() && Instant::now() < deadline {
            started = handle.take_started();
            std::thread::sleep(Duration::from_millis(20));
        }
        set_test_backend(None);
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].id, "e2j");
    }

    #[tokio::test]
    async fn a_remote_listing_sources_the_init_and_reads_the_record() {
        use rhema_sandbox::mock::ScriptedSandbox;
        let sandbox = std::sync::Arc::new(ScriptedSandbox::new());
        sandbox.push_run(Ok(rhema_sandbox::RunOutcome {
            stdout: b"[]\n__RHEMA_JOBS_STOPPED__\nzz\n".to_vec(),
            stderr: Vec::new(),
            code: Some(0),
        }));
        let stop = AtomicBool::new(false);
        let jobs = RemoteJobs {
            sandbox: sandbox.clone(),
            workspace: "/workspace".to_owned(),
            session: "s1".to_owned(),
            init: "/runtime/sessions/s1/init.sh".to_owned(),
            session_file: "/runtime/jobs/session".to_owned(),
        };
        let (rows, stopped) = remote_listing(&jobs, &stop).await.expect("a listing");
        assert!(rows.is_empty());
        assert!(stopped.contains("zz"));
        let command = sandbox.executed().remove(0);
        assert!(command.contains("job ls --json --session"));
        assert!(command.contains(". '/runtime/sessions/s1/init.sh'"));
        assert!(command.contains("rhema-jobs-$tag-stopped"));
    }

    #[test]
    fn a_remote_listing_splits_rows_and_stop_ids() {
        let text = concat!(
            r#"[{"id":"a","status":"running","exit_code":null,"command":"npm run dev","session":"s1","started_at":null}]"#,
            "\n__RHEMA_JOBS_STOPPED__\nb\n"
        );
        let (rows, stopped) = parse_remote_listing(text).expect("a listing");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "a");
        assert!(stopped.contains("b"));
    }

    #[test]
    fn a_remote_failed_poll_is_not_an_empty_listing() {
        let text = "__RHEMA_JOBS_FAILED__\n__RHEMA_JOBS_STOPPED__\n";
        assert!(parse_remote_listing(text).is_none());
        assert!(parse_remote_listing("[]\n__RHEMA_JOBS_STOPPED__\n").is_some());
        assert!(parse_remote_listing("not json\n__RHEMA_JOBS_STOPPED__\n").is_none());
    }

    #[test]
    fn entries_ages_are_computed_at_read_time() {
        let handle = JobsHandle::stub(vec![JobEntry {
            row: row("abc", "running", None),
            age: Duration::from_secs(0),
        }]);
        // The stub froze first-seen at construction; a read a moment
        // later reports a nonzero age without any new event.
        std::thread::sleep(Duration::from_millis(20));
        let entries = handle.entries();
        assert_eq!(entries.len(), 1);
        assert!(
            entries[0].age >= Duration::from_millis(15),
            "{:?}",
            entries[0].age
        );
    }
}
