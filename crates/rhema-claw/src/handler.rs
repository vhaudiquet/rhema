//! The handler: one dispatch of a forge thread becomes one coding-agent
//! turn, and the turn's final text becomes the thread's comment reply.
//!
//! The deterministic part stays ahead of the model (spec §1, §8): the
//! watch's forge client is looked up by key, the snapshot is fetched, the
//! inbox is drained and decoded, the allowlist is resolved — v1 is
//! permissive, the spec's starting point, pending a real
//! profile-to-permissions mapping — and only then does the agent run.
//! One turn per dispatch; the final assistant text posts verbatim.
//!
//! Session continuity rides the dispatcher, not the filesystem: the
//! agent's session JSON is stashed into `ctx.session.core.driver_state`
//! before the run returns and restored with [`CodingAgent::from_session`]
//! on the next dispatch of the same key, so a resumed thread keeps its
//! conversation. Commit-before-execute rides it too: the run installs the
//! context's commit hook, so the intent is in the store before the first
//! tool runs, and a crash-left dangling call revives at turn entry.
//! Delivery is at-least-once: a re-delivered drained event
//! re-dispatches, and this handler treats a dispatch as work to do — the
//! dedup boundary is the dispatcher's, not re-derived here.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use crate::forge::{WatchCredential, record_client, resolve_watch_credential};
use crate::watch_store::ClawStore;
use rhema_agent_tools::AllowAll;
use rhema_autonomous_agent::{Handler, SessionContext, async_trait};
use rhema_coding_agent::{CodingAgent, CodingAgentConfig};
use rhema_core::{Content, Event as ModelEvent, EventSink, Message, Session};
use rhema_forge_integration::ForgeKind;
use rhema_forge_integration::triage::Role;
use rhema_forge_integration::{
    EventKind, Forge, ForgeError, ForgeEvent, Notification, RepoRef, Snapshot, ThreadState, decode,
    parse_key,
};
use rhema_identity::IdentityStore;
use rhema_rsd::{InboxItem, Outcome, ResourceKey};

use crate::backends::BackendRegistry;

/// The handler's tool permissions, resolved deterministically before the
/// agent's first step (spec §8) — never by the model, never mid-run. V1
/// is permissive, the spec's starting point; a profile-to-permissions
/// mapping lands here as a per-deployment policy — the triage's
/// [`Role`] is the seam it hangs on, a reviewer dispatch deserving a
/// narrower policy than an implementer's — and a policy that refuses the
/// shell refuses the run (see [`ClawHandler::dispatch`]).
fn allowlist() -> rhema_autonomous_agent::Allowlist {
    rhema_autonomous_agent::Allowlist::permissive()
}

/// The coding agent over one watch: a provider, a model, a workspace
/// root, and the watch table the dispatch keys resolve through — live,
/// so a watch added at runtime is answerable without a restart.
pub struct ClawHandler {
    /// The deployment's providers and the active pair, read per dispatch.
    backends: Arc<BackendRegistry>,
    /// Whether the sessions run the bounded request view (the config's
    /// `transcript`, on unless set false).
    transcript: bool,
    workspace_root: PathBuf,
    watches: Arc<ClawStore>,
    /// The account store a blank watch credential resolves through: the
    /// reply client must authenticate as the identity the poll did, or
    /// the forge refuses the very post the poll was reading.
    accounts: Arc<dyn IdentityStore>,
    /// The client tools' environment, layered onto every dispatch's
    /// agent shell: the daemon URL, the scoped agent token, the tools'
    /// bin dir on PATH. Empty when the config issues no agent token.
    tool_env: std::collections::BTreeMap<String, String>,
    clients: Mutex<HashMap<String, Arc<dyn Forge>>>,
}

impl ClawHandler {
    /// A handler whose per-thread workspaces live under `workspace_root`
    /// and whose dispatch keys resolve through `watches` at dispatch time.
    pub fn new(
        backends: Arc<BackendRegistry>,
        workspace_root: impl Into<PathBuf>,
        watches: Arc<ClawStore>,
        accounts: Arc<dyn IdentityStore>,
        transcript: bool,
        tool_env: std::collections::BTreeMap<String, String>,
    ) -> Self {
        Self {
            backends,
            transcript,
            workspace_root: workspace_root.into(),
            watches,
            accounts,
            tool_env,
            clients: Mutex::new(HashMap::new()),
        }
    }

    /// The forge client a dispatch key answers to:
    /// `"github:issue:acme/widgets#7"` resolves its watch record from the
    /// table at dispatch time — the table is the source of truth, not a
    /// boot-time snapshot — and the client is cached per
    /// `(forge, host, credential)` so one poll pool serves every thread of
    /// a watch.
    ///
    /// A thread an account watch delivered may have no repo watch of its
    /// own: the account's notification feed covers the whole instance, so
    /// the row that fed the dispatch is the account watch, and the reply
    /// has to post as that account. `as_account` names it; `None` — the
    /// caller does not know, or there is one account per forge — takes the
    /// kind's first account watch.
    pub fn watch_forge(&self, key: &ResourceKey) -> Result<Arc<dyn Forge>, String> {
        self.watch_forge_as(key, None)
    }

    /// [`Self::watch_forge`] with the account the dispatch was delivered
    /// as, when the caller knows it: an account notification carries the
    /// id it arrived on (see [`crate::forge::ForgeSource`]'s poll).
    pub fn watch_forge_as(
        &self,
        key: &ResourceKey,
        as_account: Option<&str>,
    ) -> Result<Arc<dyn Forge>, String> {
        let watch = watch_key(key)
            .ok_or_else(|| format!("resource key {key} is not a forge thread key"))?;
        let (forge, repo) = watch
            .split_once(':')
            .ok_or_else(|| format!("watch entry {watch} has no forge kind"))?;
        let records = self
            .watches
            .list()
            .map_err(|err| format!("watch lookup for {key} failed: {err}"))?;
        if let Some(record) = records
            .iter()
            .find(|record| record.forge == forge && record.repo == repo)
        {
            let kind = ForgeKind::parse(&record.forge)
                .ok_or_else(|| format!("watch entry {watch} names an unknown forge"))?;
            // The same chain the poll resolved: row token, env var, then
            // the matching account identity — keyed on the resolved
            // value, so a credential rotation (or a saved account) is a
            // new client on the next dispatch. The login rides along: an
            // instance that refuses the value in the token header takes it
            // as Basic auth (`GiteaForge::with_login`).
            let credential = resolve_watch_credential(
                self.accounts.as_ref(),
                kind,
                record.host.as_deref(),
                &record.token,
            );
            return Ok(self.client(kind, record.host.as_deref(), &credential));
        }
        // No repo watch: the account watch is what fed this thread, so its
        // identity is what the reply posts as.
        let account_watches = self
            .watches
            .list_account_watches()
            .map_err(|err| format!("watch lookup for {key} failed: {err}"))?;
        let account_watch = account_watches
            .iter()
            .filter(|watch| watch.forge == forge)
            .find(|watch| as_account.is_none_or(|id| id == watch.account_id))
            .ok_or_else(|| format!("no watch configured for {key}"))?;
        let kind = ForgeKind::parse(&account_watch.forge)
            .ok_or_else(|| format!("watch entry {watch} names an unknown forge"))?;
        let secret = self
            .accounts
            .secret(&account_watch.account_id)
            .map_err(|err| format!("account lookup for {key} failed: {err}"))?
            .ok_or_else(|| {
                format!(
                    "the account watch for {key} names account {}, which is gone",
                    account_watch.account_id
                )
            })?;
        let login = self
            .accounts
            .get(&account_watch.account_id)
            .map_err(|err| format!("account lookup for {key} failed: {err}"))?
            .and_then(|account| account.username);
        let credential = WatchCredential {
            token: secret.value,
            login,
        };
        Ok(self.client(kind, account_watch.host.as_deref(), &credential))
    }

    /// The cached client for one credential: the dispatch path and the
    /// poll path build the same key, so a thread's replies ride the poll's
    /// pool.
    fn client(
        &self,
        kind: ForgeKind,
        host: Option<&str>,
        credential: &WatchCredential,
    ) -> Arc<dyn Forge> {
        // Everything that changes the client: the kind, its host, and the
        // credential — the login included, since one value sent two ways
        // is two credentials.
        let cache_key = format!(
            "{}|{:?}|{}|{:?}",
            kind.name(),
            host,
            credential.token,
            credential.login
        );
        let mut clients = self.clients.lock().unwrap_or_else(PoisonError::into_inner);
        clients
            .entry(cache_key)
            .or_insert_with(|| record_client(kind, host, credential))
            .clone()
    }

    /// One dispatch, failed with a reason on any step. The session stash
    /// lands before the comment posts, so a failed post still leaves the
    /// conversation resumable.
    async fn dispatch(&self, ctx: &mut SessionContext) -> Result<Outcome, String> {
        // Owned: the key names the run through every step, while `ctx`
        // itself goes mutable for the inbox drain.
        let key = ctx.key.as_str().to_owned();
        let (repo, thread_kind, number) = parse_key(&ctx.key)
            .ok_or_else(|| format!("resource key {key} is not a forge thread key"))?;
        // Drained before the client resolves: an account notification
        // names the account that delivered it, and the reply must post as
        // that account — a thread the account's feed delivered may have no
        // repo watch row at all.
        let news = self.drain_news(ctx, &key).await?;
        let forge = self.watch_forge_as(&ctx.key, delivery_account(&news))?;

        let snapshot = forge
            .snapshot(&repo, thread_kind, number)
            .await
            .map_err(|err| format!("snapshot of {key} failed: {err}"))?;
        let role = dominant_role(&news);

        // The permission check is load-bearing: this handler's one tool
        // is the agent's shell, so a policy without it is a run that
        // could only flail — fail it loudly instead.
        let permissions = allowlist();
        if !permissions.allows("bash") {
            return Err(format!(
                "tool policy for {key} does not permit the shell; nothing to run"
            ));
        }

        let workspace = self.workspace_for(&key, &repo, number)?;
        let mut agent = self.resume(ctx, &workspace, &key)?;
        // Commit-before-execute: the intent is in the store before the
        // first tool runs; a lease that cannot be committed under fails
        // the step before any side effect.
        agent.set_commit_hook(Some(ctx.commit_hook()));

        let prompt = build_prompt(&key, &snapshot, &news, workspace.as_path(), role);
        let mut sink = TurnSink;
        let output = agent
            .turn(prompt, &mut sink, Arc::new(AllowAll))
            .await
            .map_err(|err| format!("turn for {key} failed: {err}"))?;

        // Stash before posting: the turn's work survives a failed comment.
        ctx.session.core.driver_state = serde_json::to_value(agent.session())
            .map_err(|err| format!("session for {key} does not serialize: {err}"))?;

        let reply = final_text(&output.message)
            .ok_or_else(|| format!("the model returned no text to post for {key}"))?;
        forge
            .comment(&repo, thread_kind, number, &reply)
            .await
            .map_err(|err| format!("comment on {key} failed: {err}"))?;
        Ok(Outcome::Handled)
    }

    /// Renews the lease and drains the inbox, decoding every item into
    /// the news it carries: a repo-watch forge event, or a triaged
    /// account notification with its role. A payload that decodes to
    /// neither is loud: a shape break must surface, not silently shrink
    /// the prompt.
    async fn drain_news(
        &self,
        ctx: &mut SessionContext,
        key: &str,
    ) -> Result<Vec<InboxNews>, String> {
        let items = ctx
            .poll_inbox()
            .await
            .map_err(|err| format!("inbox poll for {key} failed: {err}"))?;
        items
            .iter()
            .map(decode_news)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| format!("an inbox item for {key} does not decode: {err}"))
    }

    /// The agent for this dispatch: the stashed session resumed when the
    /// key has history, a fresh one otherwise. A stash that does not parse
    /// fails the run loudly — silently resetting would re-orient the agent
    /// mid-conversation without anyone knowing.
    fn resume(
        &self,
        ctx: &SessionContext,
        workspace: &Path,
        key: &str,
    ) -> Result<CodingAgent, String> {
        let (provider, model) = self.backends.current();
        let config = self.agent_config(&model, workspace);
        let agent = match &ctx.session.core.driver_state {
            // A never-dispatched key carries no stash: start clean.
            serde_json::Value::Null => CodingAgent::new(provider, config),
            stashed => {
                let session: Session = serde_json::from_value(stashed.clone())
                    .map_err(|err| format!("stashed session for {key} does not parse: {err}"))?;
                CodingAgent::from_session(provider, config, session)
            }
        };
        agent.map_err(|err| format!("cannot start the coding agent for {key}: {err}"))
    }

    /// The config one dispatch's agent runs on: the default coding
    /// agent's, with the bounded request view when the config has it on.
    /// A forge thread answers for as long as the thread lives; without
    /// the view its history would ride whole on every step.
    fn agent_config(&self, model: &str, workspace: &Path) -> CodingAgentConfig {
        let mut config = CodingAgentConfig::new(model, workspace);
        // Write the resolved value, not an override: the coding agent's
        // own default is on now, and the config's `false` must win.
        config.transcript = self
            .transcript
            .then(rhema_transcript::TranscriptSettings::default);
        config.shell_env = self.tool_env.clone();
        crate::vision::apply(&mut config);
        crate::mcp::apply(&mut config);
        config
    }

    /// The per-thread workspace: one directory under the root, created on
    /// demand. The name mapping is not injective (separators collapse);
    /// thread identity lives in the dispatch key — the directory is only
    /// scratch space for the agent's tools.
    fn workspace_for(&self, key: &str, repo: &RepoRef, number: u64) -> Result<PathBuf, String> {
        let dir = self.workspace_root.join(workspace_name(key, repo, number));
        std::fs::create_dir_all(&dir)
            .map_err(|err| format!("cannot create the workspace {dir:?} for {key}: {err}"))?;
        Ok(dir)
    }
}

#[async_trait]
impl Handler for ClawHandler {
    async fn handle(&self, ctx: &mut SessionContext) -> Outcome {
        match self.dispatch(ctx).await {
            Ok(outcome) => outcome,
            Err(reason) => Outcome::Failed(reason),
        }
    }
}

/// The watch-table key of a resource key:
/// `"github:issue:acme/widgets#7"` builds `"github:acme/widgets"` — the
/// thread kind between source and path is key identity, not watch
/// identity. `None` for a key that is not a forge thread key at all.
pub fn watch_key(key: &ResourceKey) -> Option<String> {
    let (head, _) = key.as_str().rsplit_once('#')?;
    let (source, rest) = head.split_once(':')?;
    let (_, path) = rest.split_once(':')?;
    Some(format!("{source}:{path}"))
}

/// The watch-table entry for a repo: `{forge}:{owner}/{name}` — the value
/// [`watch_key`] parses back out of a dispatch key. The pair is the
/// key-parse-to-watch-lookup seam.
pub fn watch_entry(kind: &str, repo: &RepoRef) -> String {
    format!("{kind}:{}", repo.path())
}

/// The workspace directory name for a thread: kind, repo path, and number
/// flattened to filesystem-safe characters.
pub fn workspace_name(key: &str, repo: &RepoRef, number: u64) -> String {
    let kind = key.split_once(':').map(|(kind, _)| kind).unwrap_or("forge");
    format!("{kind}-{}-{number}", repo.path())
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// One drained inbox item: a plain repo-watch forge event, or a triaged
/// account notification carrying the role the triage assigned it.
#[derive(Debug, Clone)]
enum InboxNews {
    Event(ForgeEvent),
    Notified {
        role: Role,
        notification: Notification,
        /// The account watch the notification arrived on, when the payload
        /// names it: what the reply posts as.
        account_id: Option<String>,
    },
}

/// The triaged payload [`crate::forge::ForgeSource`] ingests: `repo` and
/// `number` ride at the top level for the key function, the rest is the
/// notification, the role its triage earned, and the account whose feed
/// delivered it.
#[derive(serde::Deserialize)]
struct TriagedPayload {
    role: Role,
    notification: Notification,
    /// The identity store id of the account watch that polled this. Absent
    /// on payloads ingested before it was recorded.
    #[serde(default)]
    account_id: Option<String>,
}

/// Decodes one inbox item into its news. A triaged notification is tried
/// first — a plain forge event has none of its fields — and the plain
/// event second; anything else is a loud parse error.
fn decode_news(item: &InboxItem) -> Result<InboxNews, ForgeError> {
    if let Ok(triaged) = serde_json::from_value::<TriagedPayload>(item.event.payload.clone()) {
        return Ok(InboxNews::Notified {
            role: triaged.role,
            notification: triaged.notification,
            account_id: triaged.account_id,
        });
    }
    decode(item).map(InboxNews::Event)
}

/// The account a batch's notifications were delivered as, when they name
/// one: the first is enough, since a thread fed by two accounts' feeds is
/// two identities on one conversation and the first is as good as the
/// second. Plain events name no account.
fn delivery_account(news: &[InboxNews]) -> Option<&str> {
    news.iter().find_map(|news| match news {
        InboxNews::Notified { account_id, .. } => account_id.as_deref(),
        InboxNews::Event(_) => None,
    })
}

/// The batch's dominant role, for the prompt's persona framing:
/// implementing subsumes reviewing — a batch that asks for both is a
/// dispatch where the work is the point. Plain events imply no persona:
/// the default framing runs.
fn dominant_role(news: &[InboxNews]) -> Option<Role> {
    if news.iter().any(|news| {
        matches!(
            news,
            InboxNews::Notified {
                role: Role::Implementer,
                ..
            }
        )
    }) {
        Some(Role::Implementer)
    } else if news.iter().any(|news| {
        matches!(
            news,
            InboxNews::Notified {
                role: Role::Reviewer,
                ..
            }
        )
    }) {
        Some(Role::Reviewer)
    } else {
        None
    }
}

/// The persona framing a dispatched role opens the prompt with. The key
/// stays the thread, so a role change across dispatches is the session's
/// continuity, not a new conversation.
fn persona_text(role: Role) -> &'static str {
    match role {
        Role::Reviewer => {
            "You are acting as this pull request's reviewer. Check the change \
out in the workspace when you can, read the diff, and judge it: correctness, \
tests, edge cases, fit with the surrounding code. Your final reply is the \
review — findings first, ordered by severity, concrete and actionable; if \
there is nothing to fix, say so plainly and say what you checked."
        }
        Role::Implementer => {
            "You are acting as this thread's implementer. Do the work the new \
activity asks for: address the review feedback or the assigned issue by \
changing the code in the workspace — build and test what you change. Your \
final reply is a short report of what you changed and why, or, when the \
whole change does not fit one turn, what landed now and the concrete plan \
for the rest."
        }
    }
}

/// The agent's prompt for one dispatch: the persona framing (when the
/// triage dispatched a role), the thread's standing state, the new news,
/// and the posting contract. The final assistant message is the comment
/// — everything else is orientation for the investigation.
fn build_prompt(
    key: &str,
    snapshot: &Snapshot,
    news: &[InboxNews],
    workspace: &Path,
    role: Option<Role>,
) -> String {
    let mut prompt = String::new();
    if let Some(role) = role {
        prompt.push_str(persona_text(role));
        prompt.push_str("\n\n");
    }
    prompt.push_str(&format!(
        "You are responding to new activity on forge thread {key}.\n\n\
         Thread: {}\nKind: {}; state: {}\nOpened by: {}\nLabels: {}\nOpening body:\n{}\n\n",
        snapshot.title,
        if snapshot.is_pr {
            "pull request"
        } else {
            "issue"
        },
        state_text(snapshot.state),
        snapshot.author.as_deref().unwrap_or("unknown"),
        if snapshot.labels.is_empty() {
            "none".to_owned()
        } else {
            snapshot.labels.join(", ")
        },
        snapshot
            .body
            .as_deref()
            .map(str::trim)
            .filter(|body| !body.is_empty())
            .unwrap_or("(none)"),
    ));
    prompt.push_str("New activity to respond to:\n");
    for news in news {
        match news {
            InboxNews::Event(event) => prompt.push_str(&format!(
                "- {} by {}: {}\n",
                kind_text(event.kind),
                event.actor.as_deref().unwrap_or("unknown"),
                event
                    .body
                    .as_deref()
                    .map(str::trim)
                    .filter(|body| !body.is_empty())
                    .unwrap_or("(no text)"),
            )),
            InboxNews::Notified { notification, .. } => prompt.push_str(&format!(
                "- notification ({}) on {} #{} by {}: {}\n",
                notification.reason.as_str(),
                thread_kind(notification.is_pr),
                notification.number,
                notification.actor.as_deref().unwrap_or("unknown"),
                notification
                    .title
                    .as_deref()
                    .map(str::trim)
                    .filter(|title| !title.is_empty())
                    .unwrap_or("(no title)"),
            )),
        }
    }
    prompt.push_str(&format!(
        "\nThe workspace is at {}: investigate there with your shell tools (read\n\
         code, run tests, reproduce reports) whenever the text alone does not\n\
         settle it. Your FINAL reply — the message you end the turn on, with\n\
         no tool calls after it — is posted to the thread verbatim: make it\n\
         the exact comment text for the thread's participants, with no\n\
         meta-commentary about this prompt.\n",
        workspace.display(),
    ));
    prompt
}

/// "pull request" or "issue", for the notification lines.
fn thread_kind(is_pr: bool) -> &'static str {
    if is_pr { "pull request" } else { "issue" }
}

/// The thread state, in words.
fn state_text(state: ThreadState) -> &'static str {
    match state {
        ThreadState::Open => "open",
        ThreadState::Closed => "closed",
        ThreadState::Merged => "merged",
    }
}

/// The event kind, in words.
fn kind_text(kind: EventKind) -> &'static str {
    match kind {
        EventKind::Opened => "opened",
        EventKind::Reopened => "reopened",
        EventKind::Closed => "closed",
        EventKind::Merged => "merged",
        EventKind::Comment => "comment",
        EventKind::Review => "review",
    }
}

/// The text a turn ends on: the assistant message's text parts, joined
/// and trimmed. `None` when the model stopped on tool calls and never
/// wrote a reply.
pub(crate) fn final_text(message: &Message) -> Option<String> {
    let joined = message
        .content
        .iter()
        .filter_map(|content| match content {
            Content::Text(text) => Some(text.as_str()),
            Content::ToolCall(_) | Content::ToolResult(_) | Content::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("");
    let trimmed = joined.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// The turn's stderr log: tool traffic only. The reply is not streamed —
/// it is posted to the thread, and a daemon log line per tool call is the
/// observability a resident run gets.
struct TurnSink;

impl EventSink for TurnSink {
    fn emit(&mut self, event: ModelEvent<'_>) {
        match event {
            ModelEvent::ToolCallStart { name, .. } => eprintln!("[claw] tool call: {name}"),
            ModelEvent::ToolResult {
                content, is_error, ..
            } => {
                let mark = if is_error { "failed" } else { "ok" };
                eprintln!("[claw] tool {mark}: {}", first_line(content));
            }
            _ => {}
        }
    }
}

/// The first line of a tool result, cut for the stderr stream.
fn first_line(content: &str) -> String {
    let line = content.lines().next().unwrap_or("");
    let mut cut = line.chars().take(72).collect::<String>();
    if line.chars().count() > 72 {
        cut.push('…');
    }
    cut
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhema_core::ModelProvider;
    use rhema_forge_integration::{ForgeThreadKind, NotificationReason, resource_key};

    fn snapshot() -> Snapshot {
        serde_json::from_value(serde_json::json!({
            "title": "crash on save",
            "body": "steps to reproduce…",
            "state": "open",
            "is_pr": false,
            "author": "alice",
            "labels": ["bug", "cli"],
            "url": "https://github.com/acme/widgets/issues/7",
            "raw": {},
        }))
        .expect("snapshot fixture parses")
    }

    fn event(kind: EventKind, actor: &str, body: Option<&str>) -> ForgeEvent {
        serde_json::from_value(serde_json::json!({
            "id": "github:comment:8001",
            "kind": kind,
            "repo": { "owner": "acme", "name": "widgets" },
            "number": 7,
            "is_pr": false,
            "actor": actor,
            "body": body,
            "url": null,
            "timestamp": "2026-10-03T09:00:00Z",
            "raw": {},
        }))
        .expect("event fixture parses")
    }

    #[test]
    fn the_prompt_carries_the_thread_the_events_and_the_contract() {
        let prompt = build_prompt(
            "github:issue:acme/widgets#7",
            &snapshot(),
            &[InboxNews::Event(event(
                EventKind::Comment,
                "dave",
                Some("any progress?"),
            ))],
            Path::new("/tmp/workspaces/github-acme-widgets-7"),
            None,
        );
        assert!(
            prompt.contains("github:issue:acme/widgets#7"),
            "key: {prompt}"
        );
        assert!(prompt.contains("crash on save"), "title: {prompt}");
        assert!(prompt.contains("state: open"), "state: {prompt}");
        assert!(prompt.contains("Labels: bug, cli"), "labels: {prompt}");
        assert!(
            prompt.contains("- comment by dave: any progress?"),
            "events: {prompt}"
        );
        assert!(
            prompt.contains("posted to the thread verbatim"),
            "contract: {prompt}"
        );
    }

    #[test]
    fn thread_level_events_carry_no_body_and_the_prompt_says_so() {
        let prompt = build_prompt(
            "github:issue:acme/widgets#7",
            &snapshot(),
            &[InboxNews::Event(event(EventKind::Opened, "alice", None))],
            Path::new("/tmp/w"),
            None,
        );
        assert!(
            prompt.contains("- opened by alice: (no text)"),
            "got: {prompt}"
        );
    }

    fn notification(reason: NotificationReason) -> Notification {
        serde_json::from_value(serde_json::json!({
            "id": "github:notification:42188@2026-10-03T09:00:00Z",
            "reason": reason,
            "repo": { "owner": "acme", "name": "widgets" },
            "number": 142,
            "is_pr": true,
            "actor": "alice",
            "title": "Add the widget flange",
            "url": null,
            "timestamp": "2026-10-03T09:00:00Z",
            "raw": {},
        }))
        .expect("notification fixture parses")
    }

    fn triaged_item(role: Role, reason: NotificationReason) -> InboxItem {
        InboxItem {
            event: rhema_rsd::Event {
                id: "github:notification:42188@2026-10-03T09:00:00Z".to_owned(),
                source: "github".to_owned(),
                payload: serde_json::json!({
                    "repo": "acme/widgets",
                    "number": 142,
                    "role": role,
                    "notification": notification(reason),
                }),
                received_at: 0,
            },
            delivered_at: 0,
        }
    }

    #[test]
    fn a_triaged_item_decodes_to_its_role_and_a_plain_event_to_its_event() {
        let news = decode_news(&triaged_item(
            Role::Reviewer,
            NotificationReason::ReviewRequested,
        ))
        .expect("decodes");
        assert!(matches!(
            news,
            InboxNews::Notified {
                role: Role::Reviewer,
                ..
            }
        ));
        let plain = InboxItem {
            event: rhema_rsd::Event {
                id: "github:comment:8001".to_owned(),
                source: "github".to_owned(),
                payload: serde_json::to_value(event(EventKind::Comment, "dave", None))
                    .expect("serializes"),
                received_at: 0,
            },
            delivered_at: 0,
        };
        assert!(matches!(decode_news(&plain), Ok(InboxNews::Event(_))));
        let garbage = InboxItem {
            event: rhema_rsd::Event {
                id: "x".to_owned(),
                source: "github".to_owned(),
                payload: serde_json::json!({"neither": true}),
                received_at: 0,
            },
            delivered_at: 0,
        };
        assert!(decode_news(&garbage).is_err(), "an unknown shape is loud");
    }

    #[test]
    fn the_batchs_dominant_role_subordinates_implementing_to_nothing() {
        assert_eq!(dominant_role(&[]), None);
        assert_eq!(
            dominant_role(&[InboxNews::Event(event(EventKind::Comment, "dave", None))]),
            None,
            "plain events imply no persona"
        );
        let review = InboxNews::Notified {
            role: Role::Reviewer,
            notification: notification(NotificationReason::ReviewRequested),
            account_id: None,
        };
        let work = InboxNews::Notified {
            role: Role::Implementer,
            notification: notification(NotificationReason::Assigned),
            account_id: None,
        };
        assert_eq!(
            dominant_role(std::slice::from_ref(&review)),
            Some(Role::Reviewer)
        );
        assert_eq!(
            dominant_role(std::slice::from_ref(&work)),
            Some(Role::Implementer)
        );
        assert_eq!(
            dominant_role(&[review, work]),
            Some(Role::Implementer),
            "doing the work subsumes judging it"
        );
    }

    #[test]
    fn a_reviewer_dispatch_frames_the_prompt_as_a_review() {
        let prompt = build_prompt(
            "github:issue:acme/widgets#142",
            &snapshot(),
            &[InboxNews::Notified {
                role: Role::Reviewer,
                notification: notification(NotificationReason::ReviewRequested),
                account_id: None,
            }],
            Path::new("/tmp/w"),
            Some(Role::Reviewer),
        );
        assert!(
            prompt.contains("acting as this pull request's reviewer"),
            "persona: {prompt}"
        );
        assert!(
            prompt.contains("notification (review_requested) on pull request #142"),
            "news line: {prompt}"
        );
    }

    #[test]
    fn the_final_text_joins_and_trims_but_not_tool_calls() {
        let message = Message {
            role: rhema_core::Role::Assistant,
            content: vec![
                Content::Text("on it".into()),
                Content::Text(" — closing.".into()),
            ],
        };
        assert_eq!(final_text(&message).as_deref(), Some("on it — closing."));
        let tool_only = Message {
            role: rhema_core::Role::Assistant,
            content: vec![Content::ToolCall(rhema_core::ToolCall {
                id: "call_1".into(),
                name: "bash".into(),
                args: serde_json::json!({}),
                provider_data: serde_json::Value::Null,
            })],
        };
        assert_eq!(final_text(&tool_only), None, "no text, no reply");
    }

    #[test]
    fn a_key_resolves_to_its_watch_entry_live_from_the_table() {
        let watches = Arc::new(ClawStore::in_memory().expect("store opens"));
        watches
            .add("github", "acme/widgets", None, "tok")
            .expect("watch added");
        let registry = Arc::new(
            crate::backends::BackendRegistry::new(
                vec![crate::backends::Backend::headless(
                    "mock",
                    Arc::new(NoopProvider),
                )],
                None,
            )
            .expect("constructs"),
        );
        let handler = ClawHandler::new(
            registry,
            "/tmp/w",
            watches,
            Arc::new(rhema_identity::VolatileIdentityStore::new()),
            true,
            std::collections::BTreeMap::new(),
        );

        // A watch added to the table AFTER the handler was built resolves:
        // the table is the source of truth, not a boot-time snapshot.
        let key = ResourceKey::from("github:issue:acme/widgets#7");
        assert_eq!(watch_key(&key).as_deref(), Some("github:acme/widgets"));
        // The kind segment is key identity, not watch identity: an MR
        // dispatch resolves the same watch row.
        let mr_key = ResourceKey::from("github:pull:acme/widgets#7");
        assert_eq!(watch_key(&mr_key).as_deref(), Some("github:acme/widgets"));
        assert!(handler.watch_forge(&key).is_ok(), "the watch resolves");
        assert!(
            handler
                .watch_forge(&ResourceKey::from("gitea:issue:acme/widgets#7"))
                .is_err(),
            "another forge's key does not"
        );
        assert!(
            handler
                .watch_forge(&ResourceKey::from("github:nonsense"))
                .is_err(),
            "a non-thread key does not"
        );
    }

    /// A thread an account's notification feed delivered has no repo watch
    /// row of its own — the account watch covers the whole instance — and
    /// the dispatch still resolves a client, as that account.
    #[test]
    fn an_account_watch_resolves_the_client_a_repo_watch_would_have() {
        let watches = Arc::new(ClawStore::in_memory().expect("store opens"));
        let accounts = Arc::new(rhema_identity::VolatileIdentityStore::new());
        let account = accounts
            .put(
                rhema_identity::AccountDraft::new(
                    "kosmos",
                    "gitea",
                    rhema_identity::Secret::new(rhema_identity::SecretKind::Token, "tok"),
                )
                .with_url("https://git.example")
                .with_username("kosmos"),
            )
            .expect("account lands");
        // The watch row names the id the store actually assigned: a
        // fabricated id is a row that resolves nothing.
        watches
            .add_account_watch("gitea", Some("https://git.example"), &account.id)
            .expect("account watch lands");
        let registry = Arc::new(
            crate::backends::BackendRegistry::new(
                vec![crate::backends::Backend::headless(
                    "mock",
                    Arc::new(NoopProvider),
                )],
                None,
            )
            .expect("constructs"),
        );
        let handler = ClawHandler::new(
            registry,
            "/tmp/w",
            watches,
            accounts,
            true,
            std::collections::BTreeMap::new(),
        );
        let keys = |handler: &ClawHandler| -> Vec<String> {
            handler
                .clients
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .keys()
                .cloned()
                .collect()
        };

        let key = ResourceKey::from("gitea:issue:acme/widgets#7");
        handler
            .watch_forge(&key)
            .expect("the account watch answers for the thread");
        let cached = keys(&handler);
        assert_eq!(cached.len(), 1);
        assert!(
            cached[0].contains("kosmos"),
            "the account's login rides the client: {cached:?}"
        );

        handler.watch_forge(&key).expect("it resolves again");
        assert_eq!(keys(&handler).len(), 1, "the second dispatch is cached");

        assert!(
            handler.watch_forge_as(&key, Some("acct-2")).is_err(),
            "naming another account refuses: there is no watch to post as it"
        );
        assert!(
            handler
                .watch_forge(&ResourceKey::from("github:issue:acme/widgets#7"))
                .is_err(),
            "a forge with no watch at all still refuses"
        );
    }

    #[test]
    fn the_transcript_setting_reaches_the_dispatch_config() {
        let watches = Arc::new(ClawStore::in_memory().expect("store opens"));
        let registry = Arc::new(
            crate::backends::BackendRegistry::new(
                vec![crate::backends::Backend::headless(
                    "mock",
                    Arc::new(NoopProvider),
                )],
                None,
            )
            .expect("constructs"),
        );
        let accounts = Arc::new(rhema_identity::VolatileIdentityStore::new());
        let on = ClawHandler::new(
            registry.clone(),
            "/tmp/w",
            watches.clone(),
            accounts.clone(),
            true,
            std::collections::BTreeMap::new(),
        );
        let off = ClawHandler::new(
            registry,
            "/tmp/w",
            watches,
            accounts,
            false,
            std::collections::BTreeMap::new(),
        );
        assert!(
            on.agent_config("m", Path::new("/tmp/w"))
                .transcript
                .is_some()
        );
        assert!(
            off.agent_config("m", Path::new("/tmp/w"))
                .transcript
                .is_none()
        );
    }

    #[test]
    fn the_workspace_name_flattens_to_filesystem_safe_characters() {
        assert_eq!(
            workspace_name(
                "github:issue:acme/widgets#7",
                &RepoRef::new("acme", "widgets"),
                7
            ),
            "github-acme-widgets-7"
        );
        assert_eq!(
            workspace_name(
                "gitlab:group/subgroup/widgets#7",
                &RepoRef::new("group/subgroup", "widgets"),
                7
            ),
            "gitlab-group-subgroup-widgets-7"
        );
    }

    #[test]
    fn the_table_entry_round_trips_through_the_dispatch_key() {
        let repo = RepoRef::new("acme", "widgets");
        let entry = watch_entry("github", &repo);
        // Built the way dispatches are: the canonical key builder, with
        // the thread kind in place — which must not reach the lookup.
        let key = resource_key("github", &repo, ForgeThreadKind::PullRequest, 7);
        assert_eq!(key.as_str(), "github:pull:acme/widgets#7");
        assert_eq!(watch_key(&key).as_deref(), Some(entry.as_str()));
        // A pre-kind key is not a thread key at all anymore.
        assert_eq!(watch_key(&ResourceKey::from("github:acme/widgets#7")), None);
    }

    /// Stands in for the provider in pure-lookup tests; nothing calls it.
    struct NoopProvider;
    #[rhema_core::async_trait]
    impl ModelProvider for NoopProvider {
        async fn complete(
            &self,
            _req: rhema_core::ModelRequest,
            _sink: &mut dyn EventSink,
        ) -> Result<rhema_core::ModelOutput, rhema_core::ProviderError> {
            Err(rhema_core::ProviderError::Api {
                message: "never called".to_owned(),
                status: None,
            })
        }
    }

    // `allowlist()` pins the v1 policy: permissive. The assertion keeps
    // the policy a decision someone wrote down, not an accident.
    #[test]
    fn the_v1_allowlist_is_permissive() {
        let allowlist = allowlist();
        assert!(allowlist.allows("bash"));
        assert!(allowlist.allows("anything_at_all"));
    }
}
