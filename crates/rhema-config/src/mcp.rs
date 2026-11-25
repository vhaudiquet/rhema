//! The MCP entry shape the settings dialog edits and validates.
//!
//! The config itself lives in the sandbox, in the standalone `mcp`
//! client's JSON file. This module owns the per-entry rules both sides
//! share: stdio (`command`) or Streamable HTTP (`url`), never both, and
//! the name and command-name charsets. [`McpConfig::from_entries`]
//! folds a map into the rows the page shows and the notices a skipped
//! entry earns.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The handshake budget for stdio servers: `npx` cold starts are slow,
/// so the default is generous.
pub const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(60);

/// The per-call budget, aligned with the shell tool's own cap: a call
/// configured longer is moot while the invoking shell dies first, so
/// config may lower this but never raise it (the manager's rule).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// The validated MCP entries: the servers the page shows and why any
/// were left out. Construction is infallible: a skipped entry is a
/// notice, never an error the caller must handle.
#[derive(Debug, Clone, Default)]
pub struct McpConfig {
    /// The servers that validated, in file order.
    pub servers: Vec<McpServer>,
    /// Why entries (or the whole file) were skipped, one line each,
    /// in file order. Debug-notice material; safe to show the user,
    /// secrets never appear in it.
    pub notices: Vec<String>,
}

impl McpConfig {
    /// Folds a map into the validated rows, with a notice per skipped
    /// entry, never an error. Order is the map's (sorted by name).
    pub fn from_entries(entries: &BTreeMap<String, McpServerEntry>) -> McpConfig {
        let mut config = McpConfig::default();
        for (name, entry) in entries {
            match entry.resolve(name) {
                Ok(server) => config.servers.push(server),
                Err(reason) => config
                    .notices
                    .push(format!("mcp server `{name}` skipped: {reason}")),
            }
        }
        config
    }
}

/// One entry, validated: the shape the settings page and the sandbox
/// client agree on.
#[derive(Debug, Clone)]
pub struct McpServer {
    /// The config key: `[a-z0-9_-]+`, since it rides URLs and defaults
    /// the command name.
    pub name: String,
    /// The user-written description feeding the prompt disclosure.
    pub description: String,
    /// How to reach it.
    pub transport: McpTransport,
    /// The handshake budget: [`DEFAULT_STARTUP_TIMEOUT`] unless the
    /// entry set `startup_timeout`.
    pub startup_timeout: Duration,
    /// The per-call budget the manager caps at the shell's own limit.
    pub timeout: Duration,
    /// The client's command alias, when the entry sets one.
    pub command_name: String,
}

/// How a server is reached.
///
/// [`Debug`] shows secret values as `[…]`: presence without the value,
/// matching the crate's redaction rule.
#[derive(Clone)]
pub enum McpTransport {
    /// Spawn a local child speaking JSON-RPC over stdio.
    Stdio {
        /// The program and its arguments, argv style.
        command: Vec<String>,
        /// Extra environment layered over the inherited environment.
        env: BTreeMap<String, String>,
    },
    /// POST to a Streamable HTTP endpoint.
    Http {
        /// The endpoint URL.
        url: String,
        /// Headers sent verbatim with every request.
        headers: BTreeMap<String, String>,
    },
}

impl fmt::Debug for McpTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stdio { command, env } => f
                .debug_struct("Stdio")
                .field("command", command)
                .field("env", &SecretMap(env))
                .finish(),
            Self::Http { url, headers } => f
                .debug_struct("Http")
                .field("url", url)
                .field("headers", &SecretMap(headers))
                .finish(),
        }
    }
}

/// A map whose values print as `[…]`, keys shown, values redacted.
struct SecretMap<'a>(&'a BTreeMap<String, String>);

impl fmt::Debug for SecretMap<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let redacted: BTreeMap<&String, &'static str> =
            self.0.keys().map(|key| (key, "[…]")).collect();
        f.debug_map().entries(redacted).finish()
    }
}

/// One server entry as the settings dialog builds it and the sandbox
/// JSON stores it. Validation happens at resolve time, never at
/// deserialize time, so a stored entry round-trips even while invalid,
/// the page can show and fix it.
#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct McpServerEntry {
    /// The user-written description feeding the prompt disclosure.
    /// Empty is legal, the fold substitutes a line that names the
    /// discovery command, because the field is optional knowledge,
    /// not identity: the server's own `--help` is the real
    /// documentation and this is only the reason to run it.
    #[serde(default)]
    pub description: String,
    /// The program and its arguments, argv style; stdio servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    /// The Streamable HTTP endpoint URL; HTTP servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Extra environment for stdio servers, layered over the
    /// inherited environment.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Headers sent verbatim with every HTTP request.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// Handshake seconds, overriding [`DEFAULT_STARTUP_TIMEOUT`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_timeout: Option<u64>,
    /// Per-call seconds, overriding [`DEFAULT_TIMEOUT`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
    /// Override of the generated bash function's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_name: Option<String>,
}

impl fmt::Debug for McpServerEntry {
    /// `env` and `headers` values are secrets: keys print, values
    /// redact, matching the crate's rule everywhere else.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpServerEntry")
            .field("description", &self.description)
            .field("command", &self.command)
            .field("url", &self.url)
            .field("env", &SecretMap(&self.env))
            .field("headers", &SecretMap(&self.headers))
            .field("startup_timeout", &self.startup_timeout)
            .field("timeout", &self.timeout)
            .field("command_name", &self.command_name)
            .finish()
    }
}

impl McpServerEntry {
    /// Validates into the manager-facing shape: transport XOR, name
    /// and command-name charsets, sane timeouts.
    pub fn resolve(&self, name: &str) -> Result<McpServer, String> {
        if !server_name_ok(name) {
            return Err(format!(
                "`{name}` does not match [a-z0-9_-]+; the name rides URLs \
                 and defaults the command name"
            ));
        }
        let command_name = self.command_name.clone().unwrap_or_else(|| name.to_owned());
        // An absent description is a fold-time default, not a parse
        // failure: the disclosure line happens before any connection,
        // so the server cannot be asked, and a forgotten yaml field
        // must not drop the whole server. `--help` is the real
        // documentation; the line's fallback points at it.
        let description = if self.description.is_empty() {
            format!("no description provided; {command_name} --help lists its tools")
        } else {
            self.description.clone()
        };
        if !command_name_ok(&command_name) {
            return Err(format!(
                "`{command_name}` is not a bash identifier: it must match \
                 [a-zA-Z_][a-zA-Z0-9_-]*"
            ));
        }
        let transport = match (self.command.clone(), self.url.clone()) {
            (Some(command), None) => {
                if command.is_empty() {
                    return Err("`command` is empty".to_owned());
                }
                if !self.headers.is_empty() {
                    return Err("`headers` belong to `url` servers".to_owned());
                }
                McpTransport::Stdio {
                    command,
                    env: self.env.clone(),
                }
            }
            (None, Some(url)) => {
                if url.trim().is_empty() {
                    return Err("`url` is empty".to_owned());
                }
                if !self.env.is_empty() {
                    return Err("`env` belongs to `command` servers".to_owned());
                }
                McpTransport::Http {
                    url,
                    headers: self.headers.clone(),
                }
            }
            (Some(_), Some(_)) => {
                return Err("`command` and `url` are exclusive; pick one".to_owned());
            }
            (None, None) => {
                return Err("one of `command` (stdio) or `url` (HTTP) is required".to_owned());
            }
        };
        let startup_timeout = seconds(self.startup_timeout, "startup_timeout")?;
        let timeout = seconds(self.timeout, "timeout")?;
        Ok(McpServer {
            name: name.to_owned(),
            description,
            transport,
            startup_timeout: startup_timeout.unwrap_or(DEFAULT_STARTUP_TIMEOUT),
            timeout: timeout.unwrap_or(DEFAULT_TIMEOUT),
            command_name,
        })
    }
}

/// Seconds to a positive [`Duration`]; zero and overflow reject.
fn seconds(raw: Option<u64>, field: &str) -> Result<Option<Duration>, String> {
    match raw {
        None => Ok(None),
        Some(0) => Err(format!("`{field}` must be at least one second")),
        Some(secs) => Ok(Some(Duration::from_secs(secs))),
    }
}

/// Whether a server name can ride URLs and default function names.
fn server_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// Whether a generated function name is a legal bash identifier.
/// Hyphens verified legal on bash 3.2 and 5.x (spec §2).
/// Whether `name` is a legal bash function name: an ASCII letter or
/// underscore first, then letters, digits, underscores, hyphens
/// (hyphens verified legal in bash 3.2 and 5.x). The config layer
/// rejects entries that fail it; provision re-checks defensively.
pub fn command_name_ok(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}
