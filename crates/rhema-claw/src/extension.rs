//! The extension seam: one trait for everything the resident runs
//! beyond its core, and the standard set the daemon assembles from it.
//!
//! The claw's core is small: the shell, memory, the bootstrap prompt,
//! the chat surface, and the dispatch lanes. Everything else — the
//! watch table, the task scheduler, the coding jobs, the notification
//! queue, the consent bridge, attachments, the phone, the accounts
//! layer, the presentation identity, the calendar reminders — joins
//! the daemon through [`ClawExtension`]: tool resolvers for the
//! resident's chain, sources for the tick's detect passes, and a boot
//! hook where a previous process's wreckage gets made honest. The
//! daemon's assembly loops over the set; it does not name features.
//!
//! The seam is compile-time, not dynamic. An extension is a struct in
//! this crate holding its own store handles; installing one is a
//! constructor line in [`Extensions::standard`], and each struct is
//! the boundary a future extraction would lift wholesale — the glue
//! next to its logic in an integration crate, or, if a second
//! deployment shape ever appears, an out-of-process plugin speaking
//! for the struct. The HTTP routes are deliberately not part of the
//! trait: the surface is one API contract the web UI and the phone
//! client both compile against, and it stays whole in [`crate::http`]
//! until a second consumer exists.
//!
//! Order is contract. The resolver chain is the extensions'
//! [`ClawExtension::tools`] in [`Extensions::standard`]'s order, and
//! the tick detects through [`ClawExtension::sources`] in the same
//! order — due tasks before calendar reminders, the toolsets the model
//! addresses most first. Reordering the set changes what the model
//! sees and what fires first.

use std::path::PathBuf;
use std::sync::Arc;

use rhema_autonomous_agent::{Allowlist, Source, ToolRegistry};
use rhema_core::ToolResolver;
use rhema_rsd::{Dispatcher, ResourceKey, SystemClock};

use crate::config::{CalendarWatch, ForgeKind};
use crate::consents::WaitBridge;
use crate::daemon::ClawError;
use crate::forge::ForgeSource;
use crate::hub::EventHub;
use rhema_calendar_integration::CalendarSource;

/// The store handles the standard set is assembled over. One struct,
/// so a new extension is a field plus a constructor line, not another
/// positional argument down a sixteen-deep call chain.
pub struct ExtensionDeps {
    /// The watch table: the forge extension's tools manage it, its
    /// sources poll it, and the accounts tools hang account watches
    /// off it.
    pub watches: Arc<crate::watch_store::ClawStore>,
    /// The accounts layer the accounts tools manage and the forge
    /// sources poll through.
    pub accounts: Arc<dyn rhema_identity::IdentityStore>,
    /// The notification queue: the `notify` tool writes it, devices
    /// poll it, and non-chat replies auto-notify through it.
    pub notifications: Arc<crate::notifications::NotificationStore>,
    /// The task table the task tools manage and the scheduler fires.
    pub tasks: Arc<crate::task_store::TaskStore>,
    /// The coding-job tables the `coding_agent` tool enqueues into.
    pub jobs: Arc<crate::job_store::CodingJobStore>,
    /// The consent table the `ask_secret` tool asks through.
    pub consents: Arc<crate::consents::ConsentStore>,
    /// The wake bridge between the consent tools and the routes that
    /// answer them.
    pub bridge: Arc<WaitBridge>,
    /// The attachment store the `attach_file` tool copies into.
    pub attachments: Arc<crate::attachments::AttachmentStore>,
    /// The phone tables the `phone` tool reads and queues through.
    pub phone: Arc<crate::phone::PhoneStore>,
    /// The presentation-identity store the `identity_set` tool writes.
    pub identity: Arc<crate::identity::IdentityStore>,
    /// The event hub: the key-scoped tools (consents, attachments,
    /// phone) emit their cards and asks onto the turn it streams.
    pub hub: Arc<EventHub>,
    /// The interactive workspace the account exec and the identity
    /// registry resolve against.
    pub workspace: PathBuf,
    /// The calendar feeds the reminders source polls.
    pub calendars: Vec<CalendarWatch>,
}

/// The dispatch lanes a source can feed, handed to
/// [`ClawExtension::sources`] once the daemon has built its
/// dispatchers. Which lane an extension's sources feed is fixed by
/// what the extension reads here — the forge sources feed the forge
/// lane, the task scheduler and the calendar reminders the resident
/// lane, the coding jobs their own.
pub struct Lanes {
    /// The forge lane: watch polls and account notifications.
    pub forge: Arc<Dispatcher>,
    /// The resident lane: chat, tasks, reminders, ingest.
    pub interactive: Arc<Dispatcher>,
    /// The coding-job lane: one leased turn per job.
    pub jobs: Arc<Dispatcher>,
}

/// One resident feature beyond the core: the tools it lends the
/// resolver chain, the sources it feeds the tick, and the boot hook
/// that recovers what a crash left behind.
///
/// Every method has a default, so an extension contributes only what
/// it has; `tools` is called per dispatch and `sources` once at
/// assembly.
pub trait ClawExtension: Send + Sync {
    /// The extension's name, as logs and the boot line spell it.
    fn name(&self) -> &'static str;

    /// The tool resolvers this extension lends the resident's chain,
    /// in the order they join it. Each is cut permissive — the surface
    /// is the trust boundary — and rebuilt per dispatch, because the
    /// key-scoped toolsets (consents, attachments, phone) must bind
    /// their asks and cards to the session that asked.
    fn tools(&self, _key: &ResourceKey) -> Vec<Arc<dyn ToolResolver>> {
        Vec::new()
    }

    /// The sources this extension feeds the tick with, each already
    /// registered on the lane it feeds. Built once, at assembly; a
    /// source over an empty table simply detects nothing.
    fn sources(&self, _lanes: &Lanes) -> Result<Vec<Box<dyn Source>>, ClawError> {
        Ok(Vec::new())
    }

    /// The boot pass: whatever a previous process's crash left
    /// inconsistent in this extension's tables, made honest before the
    /// first tick. Runs before the dispatchers recover their leases.
    fn boot(&self) -> Result<(), ClawError> {
        Ok(())
    }
}

/// The assembled set: the extension list the daemon iterates.
pub struct Extensions {
    items: Vec<Box<dyn ClawExtension>>,
}

impl Extensions {
    /// The standard set, in the order the resolver chain and the tick
    /// inherit: notifications, tasks, jobs, forge watches, accounts,
    /// identity, consents, attachments, phone, calendar reminders.
    /// Tasks precede the calendar on purpose — a due task is the
    /// user-facing clock, and the reminder that fires for it should
    /// find the work already detected.
    pub fn standard(deps: ExtensionDeps) -> Self {
        Self {
            items: vec![
                Box::new(NotifyExtension {
                    notifications: deps.notifications.clone(),
                }),
                Box::new(TasksExtension {
                    tasks: deps.tasks.clone(),
                }),
                Box::new(JobsExtension {
                    jobs: deps.jobs.clone(),
                }),
                Box::new(ForgeExtension {
                    watches: deps.watches.clone(),
                    accounts: deps.accounts.clone(),
                    notifications: deps.notifications.clone(),
                }),
                Box::new(AccountsExtension {
                    accounts: deps.accounts.clone(),
                    watches: deps.watches.clone(),
                    workspace: deps.workspace.clone(),
                }),
                Box::new(IdentityExtension {
                    identity: deps.identity.clone(),
                    workspace: deps.workspace.clone(),
                }),
                Box::new(ConsentsExtension {
                    consents: deps.consents.clone(),
                    bridge: deps.bridge.clone(),
                    hub: deps.hub.clone(),
                }),
                Box::new(AttachmentsExtension {
                    store: deps.attachments.clone(),
                    workspace: deps.workspace.clone(),
                    hub: deps.hub.clone(),
                }),
                Box::new(PhoneExtension {
                    store: deps.phone.clone(),
                    consents: deps.consents.clone(),
                    bridge: deps.bridge.clone(),
                    hub: deps.hub.clone(),
                }),
                Box::new(CalendarExtension {
                    calendars: deps.calendars,
                }),
            ],
        }
    }

    /// The full resolver chain one dispatch runs under: every
    /// extension's tools, in set order, key-scoped toolsets bound to
    /// `key`.
    pub fn tools(&self, key: &ResourceKey) -> Vec<Arc<dyn ToolResolver>> {
        self.items
            .iter()
            .flat_map(|extension| extension.tools(key))
            .collect()
    }

    /// Every extension's sources, in set order, each registered on the
    /// lane it feeds. Built once; the daemon holds the result for the
    /// tick's detect passes.
    pub fn sources(&self, lanes: &Lanes) -> Result<Vec<Box<dyn Source>>, ClawError> {
        let mut sources = Vec::new();
        for extension in &self.items {
            sources.extend(extension.sources(lanes)?);
        }
        Ok(sources)
    }

    /// Every extension's boot pass, in set order.
    pub fn boot(&self) -> Result<(), ClawError> {
        for extension in &self.items {
            extension.boot()?;
        }
        Ok(())
    }

    /// The set's names, the order the chain and the tick run in — the
    /// boot line's inventory.
    pub fn names(&self) -> Vec<&'static str> {
        self.items
            .iter()
            .map(|extension| extension.name())
            .collect()
    }
}

/// Wraps a registry in the permissive allowlist — the resident tier's
/// standing policy: the surface is the trust boundary, and the shell
/// already runs auto-approved.
fn permissive(registry: ToolRegistry) -> Arc<dyn ToolResolver> {
    registry.resolver_for(&Allowlist::permissive())
}

/// The notification queue: the `notify` tool the resident writes
/// through and the rows a device polls and acks. Nothing detects —
/// outbound mail needs no polling.
struct NotifyExtension {
    notifications: Arc<crate::notifications::NotificationStore>,
}

impl ClawExtension for NotifyExtension {
    fn name(&self) -> &'static str {
        "notify"
    }

    fn tools(&self, _key: &ResourceKey) -> Vec<Arc<dyn ToolResolver>> {
        vec![permissive(crate::notify_tools::registry(
            self.notifications.clone(),
        ))]
    }
}

/// The task scheduler: the `schedule_task`/`list_tasks`/`cancel_task`
/// tools, and the source whose detect pass turns a due row into an
/// event on the resident lane — never a timer near the model.
struct TasksExtension {
    tasks: Arc<crate::task_store::TaskStore>,
}

impl ClawExtension for TasksExtension {
    fn name(&self) -> &'static str {
        "tasks"
    }

    fn tools(&self, _key: &ResourceKey) -> Vec<Arc<dyn ToolResolver>> {
        vec![permissive(crate::task_tools::registry(self.tasks.clone()))]
    }

    fn sources(&self, lanes: &Lanes) -> Result<Vec<Box<dyn Source>>, ClawError> {
        Ok(vec![Box::new(crate::task_source::TaskScheduler::new(
            &lanes.interactive,
            Arc::new(SystemClock),
            self.tasks.clone(),
        ))])
    }
}

/// The coding jobs: the `coding_agent` tool, the source that ingests
/// queued rows onto the jobs lane, and the boot pass that fails the
/// turns a crash interrupted.
struct JobsExtension {
    jobs: Arc<crate::job_store::CodingJobStore>,
}

impl ClawExtension for JobsExtension {
    fn name(&self) -> &'static str {
        "jobs"
    }

    fn tools(&self, _key: &ResourceKey) -> Vec<Arc<dyn ToolResolver>> {
        vec![permissive(crate::job_tools::registry(self.jobs.clone()))]
    }

    fn sources(&self, lanes: &Lanes) -> Result<Vec<Box<dyn Source>>, ClawError> {
        Ok(vec![Box::new(crate::jobs::JobsSource::new(
            &lanes.jobs,
            self.jobs.clone(),
        ))])
    }

    fn boot(&self) -> Result<(), ClawError> {
        let touched = self.jobs.recover().map_err(ClawError::Jobs)?;
        if touched > 0 {
            eprintln!("[claw] jobs: {touched} row(s) recovered at boot");
        }
        Ok(())
    }
}

/// The forge watches: the watch-management tools the resident configures
/// on request, and one source per forge kind — repo watches polled
/// through the watch table, account notifications triaged through the
/// accounts layer.
struct ForgeExtension {
    watches: Arc<crate::watch_store::ClawStore>,
    accounts: Arc<dyn rhema_identity::IdentityStore>,
    notifications: Arc<crate::notifications::NotificationStore>,
}

impl ClawExtension for ForgeExtension {
    fn name(&self) -> &'static str {
        "forge"
    }

    fn tools(&self, _key: &ResourceKey) -> Vec<Arc<dyn ToolResolver>> {
        vec![permissive(crate::watch_tools::registry(
            self.watches.clone(),
        ))]
    }

    fn sources(&self, lanes: &Lanes) -> Result<Vec<Box<dyn Source>>, ClawError> {
        Ok(ForgeKind::ALL
            .into_iter()
            .map(|kind| {
                Box::new(ForgeSource::new(
                    &lanes.forge,
                    kind.name(),
                    self.watches.clone(),
                    self.accounts.clone(),
                    self.notifications.clone(),
                )) as Box<dyn Source>
            })
            .collect())
    }
}

/// The accounts layer: the account tools that manage identities over
/// chat, and the one tool that acts with a stored credential — the
/// secret reaches a command's environment and never returns.
struct AccountsExtension {
    accounts: Arc<dyn rhema_identity::IdentityStore>,
    watches: Arc<crate::watch_store::ClawStore>,
    workspace: PathBuf,
}

impl ClawExtension for AccountsExtension {
    fn name(&self) -> &'static str {
        "accounts"
    }

    fn tools(&self, _key: &ResourceKey) -> Vec<Arc<dyn ToolResolver>> {
        vec![
            permissive(crate::accounts_tools::registry(
                self.accounts.clone(),
                self.watches.clone(),
            )),
            Arc::new(crate::accounts_tools::IdentityExec::new(
                self.accounts.clone(),
                self.workspace.clone(),
            )),
        ]
    }
}

/// The presentation identity: the `identity_set` tool, so the model
/// writes its own name, mark, and images the surface serves.
struct IdentityExtension {
    identity: Arc<crate::identity::IdentityStore>,
    workspace: PathBuf,
}

impl ClawExtension for IdentityExtension {
    fn name(&self) -> &'static str {
        "identity"
    }

    fn tools(&self, _key: &ResourceKey) -> Vec<Arc<dyn ToolResolver>> {
        vec![permissive(crate::identity::registry(
            self.identity.clone(),
            self.workspace.clone(),
        ))]
    }
}

/// The consent bridge: the `ask_secret`/`request_permission` tools
/// that park a running turn on a human answer, and the routes that
/// deliver it. Key-scoped: an ask lands on the session that asked.
struct ConsentsExtension {
    consents: Arc<crate::consents::ConsentStore>,
    bridge: Arc<WaitBridge>,
    hub: Arc<EventHub>,
}

impl ClawExtension for ConsentsExtension {
    fn name(&self) -> &'static str {
        "consents"
    }

    fn tools(&self, key: &ResourceKey) -> Vec<Arc<dyn ToolResolver>> {
        vec![permissive(crate::consents::registry(
            self.consents.clone(),
            self.bridge.clone(),
            self.hub.clone(),
            key.clone(),
        ))]
    }
}

/// The attachments: the `attach_file` tool that copies a workspace
/// file into a fetchable card. Key-scoped: a card lands on the session
/// that attached it.
struct AttachmentsExtension {
    store: Arc<crate::attachments::AttachmentStore>,
    workspace: PathBuf,
    hub: Arc<EventHub>,
}

impl ClawExtension for AttachmentsExtension {
    fn name(&self) -> &'static str {
        "attachments"
    }

    fn tools(&self, key: &ResourceKey) -> Vec<Arc<dyn ToolResolver>> {
        vec![permissive(crate::attachments::registry(
            self.store.clone(),
            self.workspace.clone(),
            self.hub.clone(),
            key.clone(),
        ))]
    }
}

/// The phone: the `phone` tool's consent-gated control actions and the
/// status it reads. Key-scoped: a command queued from `phone:desk`
/// belongs to that device's session.
struct PhoneExtension {
    store: Arc<crate::phone::PhoneStore>,
    consents: Arc<crate::consents::ConsentStore>,
    bridge: Arc<WaitBridge>,
    hub: Arc<EventHub>,
}

impl ClawExtension for PhoneExtension {
    fn name(&self) -> &'static str {
        "phone"
    }

    fn tools(&self, key: &ResourceKey) -> Vec<Arc<dyn ToolResolver>> {
        vec![permissive(crate::phone::registry(
            self.store.clone(),
            self.consents.clone(),
            self.bridge.clone(),
            self.hub.clone(),
            key.clone(),
        ))]
    }
}

/// The calendar reminders: a due occurrence is an event on the resident
/// lane, never a timer near the model.
struct CalendarExtension {
    calendars: Vec<CalendarWatch>,
}

impl ClawExtension for CalendarExtension {
    fn name(&self) -> &'static str {
        "calendar"
    }

    fn sources(&self, lanes: &Lanes) -> Result<Vec<Box<dyn Source>>, ClawError> {
        let source =
            CalendarSource::new(&lanes.interactive, Arc::new(SystemClock), &self.calendars)
                .map_err(ClawError::Backend)?;
        Ok(vec![Box::new(source)])
    }
}
