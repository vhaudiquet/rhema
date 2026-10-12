//! The identity tools: `identity_save`, `identity_list`, and
//! `identity_forget`, the model's surface over the [`IdentityStore`].
//!
//! These are the one place the use-never-read contract could break, so
//! the answers are built to keep it: a save answers with the redacted
//! [`Account`] and the watch verdict — never the secret, which nothing
//! here re-renders; a list answers with the store's own redacted
//! records; a forget answers with what went. The store's `secret()` is
//! *not* reachable from any tool: it exists for the forge clients, and
//! this module never calls it.
//!
//! These tools join the interactive chain only. A forge dispatch — whose
//! prompt is text a stranger wrote — never resolves them, so an injected
//! comment cannot ask its way to the accounts.

use std::sync::Arc;

use rhema_autonomous_agent::{Tool, ToolRegistry, async_trait};
use rhema_core::ToolSpec;
use rhema_identity::{Account, AccountDraft, IdentityStore, Secret, SecretKind};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::ForgeKind;
use crate::watch_store::ClawStore;

/// `identity_save`: store an account (label, service, url, username, and
/// the secret — required) and optionally register its forge watch.
struct IdentitySave {
    accounts: Arc<dyn IdentityStore>,
    watches: Arc<ClawStore>,
}

/// The save's arguments. `secret_kind` defaults to a token; a password
/// is stored but is never model-accessible or pollable in v1.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SaveArgs {
    label: String,
    service: String,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    username: Option<String>,
    secret: String,
    #[serde(default)]
    secret_kind: Option<String>,
    #[serde(default)]
    watch: bool,
}

/// What saving an account earns: the redacted account, whether the
/// service resolved to a forge the daemon watches, and whether the watch
/// got registered.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SavedAccount {
    /// The stored account, redacted.
    pub account: Account,
    /// Whether `service`/`url` resolved to a watchable forge.
    pub forge_known: bool,
    /// Whether the forge watch got registered (asked for, forge known,
    /// and not already watched).
    pub watch_added: bool,
}

/// The forge an account's `service` (then its `url` host) names, when it
/// is one the daemon watches. A self-hosted instance is only recognizable
/// through an explicit service name — a URL alone cannot distinguish a
/// Gitea from a GitLab.
pub fn forge_of(service: &str, url: Option<&str>) -> Option<ForgeKind> {
    // Case-insensitive on the service: a user says "GitHub", the store
    // normalizes to "github", and both should name the forge.
    if let Some(kind) = ForgeKind::parse(service.trim().to_lowercase().as_str()) {
        return Some(kind);
    }
    let url = url?;
    let host = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(url)
        .split('/')
        .next()?;
    match host.trim_end_matches('/') {
        "github.com" => Some(ForgeKind::GitHub),
        "gitlab.com" => Some(ForgeKind::GitLab),
        _ => None,
    }
}

/// Parses the optional `secret_kind` spelling; `None` (absent) means a
/// token. An unknown spelling is an in-band error. Shared with the HTTP
/// route, which validates the same shape.
pub fn parse_kind(kind: Option<&str>) -> Result<SecretKind, String> {
    match kind.map(str::trim).filter(|name| !name.is_empty()) {
        None => Ok(SecretKind::Token),
        Some("token") => Ok(SecretKind::Token),
        Some("password") => Ok(SecretKind::Password),
        Some(other) => Err(format!(
            "unknown secret_kind {other:?}: a kind is 'token' or 'password'"
        )),
    }
}

/// Stores an account and, when asked and the service names a watchable
/// forge, registers its account watch — the explicit-at-creation sync:
/// no background reconciliation, so this is the one moment a watch
/// appears. The store's error and the watch verdict both land in-band.
pub fn save_account(
    accounts: &dyn IdentityStore,
    watches: &ClawStore,
    draft: AccountDraft,
    watch: bool,
) -> Result<SavedAccount, String> {
    let account = accounts.put(draft).map_err(|err| err.to_string())?;
    let forge = forge_of(&account.service, account.url.as_deref());
    let mut saved = SavedAccount {
        account,
        forge_known: forge.is_some(),
        watch_added: false,
    };
    if watch {
        if let Some(kind) = forge {
            let host = saved.account.url.as_deref().filter(|url| !url.is_empty());
            saved.watch_added = watches
                .add_account_watch(kind.name(), host, &saved.account.id)
                .map_err(|err| err.to_string())?;
        }
        // Asked to watch a service the daemon cannot watch: the save
        // stands, the watch verdict says why not.
    }
    Ok(saved)
}

/// Forgets an account and cascades its derived account watches — the
/// other half of the explicit sync. Returns whether the account existed
/// and how many watches went with it.
pub fn forget_account(
    accounts: &dyn IdentityStore,
    watches: &ClawStore,
    id: &str,
) -> Result<(bool, usize), String> {
    let forgot = accounts.forget(id).map_err(|err| err.to_string())?;
    let removed = if forgot {
        watches
            .remove_account_watches_for(id)
            .map_err(|err| err.to_string())?
    } else {
        0
    };
    Ok((forgot, removed))
}

#[async_trait]
impl Tool for IdentitySave {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "identity_save",
            "Store an account the daemon holds on a service: a label, the service's name (e.g. \
             'github'), optionally its URL and the username, and the secret (an API token, or a \
             password — passwords are stored but never usable by you). With watch=true and a \
             forge the daemon watches, its notifications for this account are followed.",
            json!({
                "type": "object",
                "properties": {
                    "label": { "type": "string",
                        "description": "Display name, e.g. 'acme releases'." },
                    "service": { "type": "string",
                        "description": "The service, lowercase: github, gitlab, gitea, or any name." },
                    "url": { "type": "string",
                        "description": "Base URL, for self-hosted instances." },
                    "username": { "type": "string",
                        "description": "The login name on the service." },
                    "secret": { "type": "string",
                        "description": "The credential to store. Never echoed back." },
                    "secret_kind": { "type": "string",
                        "enum": ["token", "password"],
                        "description": "What the secret is; default token." },
                    "watch": { "type": "boolean",
                        "description": "Follow this forge identity's notifications; default false." }
                },
                "required": ["label", "service", "secret"]
            }),
        )
    }

    async fn invoke(&self, args: Value) -> Result<String, String> {
        let args: SaveArgs =
            serde_json::from_value(args).map_err(|err| format!("bad arguments: {err}"))?;
        let kind = parse_kind(args.secret_kind.as_deref())?;
        let mut draft = AccountDraft::new(args.label, args.service, Secret::new(kind, args.secret));
        if let Some(url) = args.url {
            draft = draft.with_url(url);
        }
        if let Some(username) = args.username {
            draft = draft.with_username(username);
        }
        let saved = save_account(
            self.accounts.as_ref(),
            self.watches.as_ref(),
            draft,
            args.watch,
        )?;
        Ok(json!({
            "saved": true,
            "id": saved.account.id,
            "label": saved.account.label,
            "service": saved.account.service,
            "secret_kind": saved.account.secret_kind,
            "watched": saved.watch_added,
            "watchable_forge": saved.forge_known,
        })
        .to_string())
    }
}

/// `identity_list`: every account, redacted — the store's own records,
/// which have no secret field to leak.
struct IdentityList {
    accounts: Arc<dyn IdentityStore>,
}

#[async_trait]
impl Tool for IdentityList {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "identity_list",
            "List the accounts the daemon holds. Secrets are never shown — only each account's \
             label, service, url, username, and what kind of secret it keeps.",
            json!({ "type": "object", "properties": {} }),
        )
    }

    async fn invoke(&self, _args: Value) -> Result<String, String> {
        let accounts = self.accounts.list().map_err(|err| err.to_string())?;
        Ok(json!({ "accounts": accounts }).to_string())
    }
}

/// `identity_forget`: remove an account, its secret with it, and the
/// account watches derived from it.
struct IdentityForget {
    accounts: Arc<dyn IdentityStore>,
    watches: Arc<ClawStore>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ForgetArgs {
    id: String,
}

#[async_trait]
impl Tool for IdentityForget {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "identity_forget",
            "Forget a stored account by id: the secret goes with it, and any forge watch that \
             followed this identity is removed too.",
            json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "The account id (identity_list's ids)." }
                },
                "required": ["id"]
            }),
        )
    }

    async fn invoke(&self, args: Value) -> Result<String, String> {
        let args: ForgetArgs =
            serde_json::from_value(args).map_err(|err| format!("bad arguments: {err}"))?;
        let (forgot, watches_removed) =
            forget_account(self.accounts.as_ref(), self.watches.as_ref(), &args.id)?;
        Ok(json!({
            "forgot": forgot,
            "watches_removed": watches_removed,
        })
        .to_string())
    }
}

/// The identity tool set over the account store and the watch table.
pub fn registry(accounts: Arc<dyn IdentityStore>, watches: Arc<ClawStore>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(IdentitySave {
        accounts: accounts.clone(),
        watches: watches.clone(),
    }));
    registry.register(Arc::new(IdentityList {
        accounts: accounts.clone(),
    }));
    registry.register(Arc::new(IdentityForget { accounts, watches }));
    registry
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts() -> (Arc<dyn IdentityStore>, Arc<ClawStore>) {
        (
            Arc::new(rhema_identity::VolatileIdentityStore::new()),
            Arc::new(ClawStore::in_memory().expect("store opens")),
        )
    }

    fn call(tool: &dyn Tool, args: Value) -> Result<String, String> {
        futures_block_on(tool.invoke(args))
    }

    /// The tools are async over a sync store: one block-on harness for
    /// the tests.
    fn futures_block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime builds")
            .block_on(future)
    }

    #[test]
    fn save_answers_without_the_secret_and_watch_lands_for_a_known_forge() {
        let (accounts, watches) = parts();
        let tool = IdentitySave {
            accounts: accounts.clone(),
            watches: watches.clone(),
        };
        let answer = call(
            &tool,
            json!({
                "label": "acme bot",
                "service": "github",
                "secret": "sekrit-token",
                "watch": true
            }),
        )
        .expect("saves");
        assert!(!answer.contains("sekrit-token"), "{answer}");
        assert!(answer.contains("\"watched\":true"), "{answer}");

        let listed = call(
            &IdentityList {
                accounts: accounts.clone(),
            },
            json!({}),
        )
        .expect("lists");
        assert!(!listed.contains("sekrit-token"), "{listed}");
        assert!(listed.contains("acme bot"), "{listed}");

        let watched = watches.list_account_watches().expect("watches read");
        assert_eq!(watched.len(), 1);
        assert_eq!(watched[0].forge, "github");
    }

    #[test]
    fn forget_cascades_the_derived_watch() {
        let (accounts, watches) = parts();
        let saved = save_account(
            accounts.as_ref(),
            watches.as_ref(),
            AccountDraft::new("acme", "github", Secret::new(SecretKind::Token, "t")),
            true,
        )
        .expect("saves");
        assert!(saved.watch_added);

        let tool = IdentityForget {
            accounts: accounts.clone(),
            watches: watches.clone(),
        };
        let answer = call(&tool, json!({ "id": saved.account.id })).expect("forgets");
        assert!(answer.contains("\"forgot\":true"), "{answer}");
        assert!(answer.contains("\"watches_removed\":1"), "{answer}");
        assert!(watches.list_account_watches().expect("reads").is_empty());
        assert!(!accounts.secret(&saved.account.id).expect("reads").is_some());
    }

    #[test]
    fn a_watch_request_on_an_unknown_service_saves_without_watching() {
        let (accounts, watches) = parts();
        let saved = save_account(
            accounts.as_ref(),
            watches.as_ref(),
            AccountDraft::new("mail", "smtp", Secret::new(SecretKind::Password, "p"))
                .with_url("https://mail.example.net"),
            true,
        )
        .expect("saves");
        assert!(!saved.watch_added);
        assert!(!saved.forge_known);
        assert!(watches.list_account_watches().expect("reads").is_empty());
    }

    #[test]
    fn forge_of_matches_services_and_production_hosts_only() {
        assert_eq!(forge_of("GitHub", None), Some(ForgeKind::GitHub));
        assert_eq!(
            forge_of("whatever", Some("https://github.com/acme")),
            Some(ForgeKind::GitHub)
        );
        assert_eq!(
            forge_of("whatever", Some("https://gitlab.com/acme/w")),
            Some(ForgeKind::GitLab)
        );
        assert_eq!(
            forge_of("whatever", Some("https://git.example.net/acme/w")),
            None,
            "a self-hosted host is unrecognizable by URL alone"
        );
        assert_eq!(
            forge_of("gitea", Some("https://git.example.net")),
            Some(ForgeKind::Gitea)
        );
    }

    #[test]
    fn an_unknown_secret_kind_is_rejected_in_band() {
        let (accounts, watches) = parts();
        let tool = IdentitySave { accounts, watches };
        let err = call(
            &tool,
            json!({
                "label": "x",
                "service": "github",
                "secret": "t",
                "secret_kind": "pin"
            }),
        )
        .expect_err("rejected");
        assert!(err.contains("unknown secret_kind"), "{err}");
    }
}
