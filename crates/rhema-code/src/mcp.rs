//! The MCP entry shape the settings dialog edits and validates.
//!
//! `mcp-cli` owns the file and its accepted fields. This module mirrors
//! those fields so the page can show and edit them: stdio (`command`)
//! or Streamable HTTP (`url`), never both, plus the env and header maps.
//! Nothing here belongs in the host profile any more.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

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
    /// The config key: `[a-z0-9_-]+`.
    pub name: String,
    /// The user-written description feeding the prompt disclosure.
    pub description: String,
    /// How to reach it.
    pub transport: McpTransport,
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
///
/// The fields are the ones `mcp-cli` reads. `command` accepts an argv
/// array or a bare program name; `args` carries the rest of a bare
/// program's argv, the shape most MCP client configs use.
#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct McpServerEntry {
    /// The user-written description feeding the prompt disclosure.
    /// Empty is legal, the fold substitutes a line that names the
    /// discovery command.
    #[serde(default)]
    pub description: String,
    /// The program and its arguments, argv style; stdio servers.
    #[serde(
        default,
        deserialize_with = "command_from_json",
        skip_serializing_if = "Option::is_none"
    )]
    pub command: Option<Vec<String>>,
    /// Extra stdio arguments when `command` is a bare program name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
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
}

/// The two spellings `mcp-cli` accepts for `command`.
#[derive(Deserialize)]
#[serde(untagged)]
enum RawCommand {
    /// The whole argv.
    Line(Vec<String>),
    /// The program; `args` carries the rest.
    Program(String),
}

fn command_from_json<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(
        Option::<RawCommand>::deserialize(deserializer)?.map(|command| match command {
            RawCommand::Line(argv) => argv,
            RawCommand::Program(program) => vec![program],
        }),
    )
}

impl fmt::Debug for McpServerEntry {
    /// `env` and `headers` values are secrets: keys print, values
    /// redact, matching the crate's rule everywhere else.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpServerEntry")
            .field("description", &self.description)
            .field("command", &self.command)
            .field("args", &self.args)
            .field("url", &self.url)
            .field("env", &SecretMap(&self.env))
            .field("headers", &SecretMap(&self.headers))
            .finish()
    }
}

impl McpServerEntry {
    /// Validates into the manager-facing shape: transport XOR, name
    /// charset, and command/URL placement.
    pub fn resolve(&self, name: &str) -> Result<McpServer, String> {
        if !server_name_ok(name) {
            return Err(format!(
                "`{name}` does not match [a-z0-9_-]+; the name is the server key"
            ));
        }
        // An absent description is a fold-time default, not a parse
        // failure: the disclosure line happens before any connection,
        // so the server cannot be asked.
        let description = if self.description.is_empty() {
            format!("no description provided; mcp {name} --list lists its tools")
        } else {
            self.description.clone()
        };
        let transport = match (self.command.clone(), self.url.clone()) {
            (Some(mut command), None) => {
                command.extend(self.args.clone());
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
                if !self.args.is_empty() {
                    return Err("`args` belong to `command` servers".to_owned());
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
        Ok(McpServer {
            name: name.to_owned(),
            description,
            transport,
        })
    }
}

/// Whether a server name can ride a command line unquoted.
fn server_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}
