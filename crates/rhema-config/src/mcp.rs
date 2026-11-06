//! MCP server configuration: `<config dir>/mcp.yaml`, user-authored.
//!
//! One entry per configured server, stdio (`command:`) or Streamable
//! HTTP (`url:`), never both. Loading is per-entry and forgiving of
//! the file, strict of the entry: a malformed entry is skipped and
//! its reason lands in [`McpConfig::notices`], a whole-file failure
//! yields an empty config with one notice, and nothing here fails
//! agent construction. Duplicate server names never reach the fold:
//! serde_yaml rejects duplicate keys at parse time, so a name said
//! twice skips the whole file with the parser's message.
//!
//! The manager consumes [`McpServer`], the validated shape; the raw
//! yaml never leaks past this module.
//!
//! ```yaml
//! # <config dir>/mcp.yaml
//! servers:
//!   atlassian:
//!     description: Jira and Confluence
//!     command: ["npx", "-y", "@modelcontextprotocol/server-atlassian"]
//!     env: { JIRA_TOKEN: "…" }
//!   blender:
//!     description: Blender scene operations
//!     url: "http://127.0.0.1:9721/mcp"
//!     headers: { Authorization: "Bearer …" }
//! ```

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::ConfigError;

/// The handshake budget for stdio servers: `npx` cold starts are slow,
/// so the default is generous.
pub const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(60);

/// The per-call budget, aligned with the shell tool's own cap: a call
/// configured longer is moot while the invoking shell dies first, so
/// config may lower this but never raise it (the manager's rule).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// The `<config dir>/mcp.yaml` path.
pub fn config_path() -> Result<PathBuf, ConfigError> {
    Ok(crate::Home::discover()?.config.join("mcp.yaml"))
}

/// The validated MCP configuration: the servers to expose and why any
/// others were left out.
///
/// Construction is infallible by design; loading is too
/// ([`McpConfig::load`]).
/// Absence of a server shows up either as a skipped entry's notice or
/// as an absent file, never as an error a caller must handle.
#[derive(Debug, Clone, Default)]
pub struct McpConfig {
    /// The servers that validated, in file order.
    pub servers: Vec<McpServer>,
    /// Why entries (or the whole file) were skipped, one line each,
    /// in file order. Debug-notice material; safe to show the user —
    /// secrets never appear in it.
    pub notices: Vec<String>,
}

impl McpConfig {
    /// Loads `<config dir>/mcp.yaml`; a missing file is an empty
    /// config, every other failure a notice.
    pub fn load() -> McpConfig {
        let path = match config_path() {
            Ok(path) => path,
            // No config dir, no MCP file: nothing to load, nothing to
            // report.
            Err(ConfigError::NoHome) => return McpConfig::default(),
            Err(err) => {
                return McpConfig {
                    notices: vec![format!("mcp.yaml unreadable: {err}")],
                    ..McpConfig::default()
                };
            }
        };
        Self::load_from(&path)
    }

    /// [`McpConfig::load`] against an explicit file, the test seam.
    pub fn load_from(path: &impl AsRef<Path>) -> McpConfig {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return McpConfig::default();
            }
            Err(err) => {
                return McpConfig {
                    notices: vec![format!("mcp.yaml unreadable: {err}")],
                    ..McpConfig::default()
                };
            }
        };
        let doc: serde_yaml::Mapping = match serde_yaml::from_str(&raw) {
            Ok(doc) => doc,
            Err(err) => {
                return McpConfig {
                    notices: vec![format!("mcp.yaml skipped: {err}")],
                    ..McpConfig::default()
                };
            }
        };
        Self::from_mapping(&doc)
    }

    /// Builds from a profile's `mcp_servers` mapping: the same fold
    /// rules as the file — per-entry validation, a notice per skipped
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

    /// Folds the `servers` mapping in file order, skipping entries
    /// that fail validation with a notice each. Duplicate names
    /// cannot occur here: serde_yaml rejects duplicate keys before
    /// this runs.
    fn from_mapping(doc: &serde_yaml::Mapping) -> McpConfig {
        let mut config = McpConfig::default();
        let Some(servers) = doc.get("servers") else {
            config
                .notices
                .push("mcp.yaml has no servers mapping".to_owned());
            return config;
        };
        let Ok(entries) = serde_yaml::from_value::<serde_yaml::Mapping>(servers.clone()) else {
            config
                .notices
                .push("mcp.yaml servers is not a mapping".to_owned());
            return config;
        };
        for (name, entry) in entries {
            let Some(name) = name.as_str().map(str::to_owned) else {
                config
                    .notices
                    .push("mcp.yaml has a non-string server name; skipped".to_owned());
                continue;
            };
            let raw: McpServerEntry = match serde_yaml::from_value(entry) {
                Ok(raw) => raw,
                Err(err) => {
                    config
                        .notices
                        .push(format!("mcp server `{name}` skipped: {err}"));
                    continue;
                }
            };
            match raw.resolve(&name) {
                Ok(server) => config.servers.push(server),
                Err(reason) => {
                    config
                        .notices
                        .push(format!("mcp server `{name}` skipped: {reason}"));
                }
            }
        }
        config
    }
}

/// One configured MCP server, validated: the shape the manager and
/// provision steps consume.
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
    /// The generated bash function's name: `command_name` when set,
    /// else [`McpServer::name`]. Matches bash's identifier rules.
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

/// A map whose values print as `[…]` — keys shown, values redacted.
struct SecretMap<'a>(&'a BTreeMap<String, String>);

impl fmt::Debug for SecretMap<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let redacted: BTreeMap<&String, &'static str> =
            self.0.keys().map(|key| (key, "[…]")).collect();
        f.debug_map().entries(redacted).finish()
    }
}

/// The yaml shape of one MCP server entry, as both config scopes
/// store it: the global `<config dir>/mcp.yaml` `servers` mapping and
/// a profile's `mcp_servers` mapping. Validation happens at resolve
/// time, never at deserialize time, so a stored entry round-trips even
/// while invalid — the frontend can show and fix it.
#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct McpServerEntry {
    /// The user-written description feeding the prompt disclosure.
    /// Empty is legal — the fold substitutes a line that names the
    /// discovery command — because the field is optional knowledge,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn load(yaml: &str) -> McpConfig {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mcp.yaml");
        std::fs::write(&path, yaml).expect("write");
        McpConfig::load_from(&path)
    }

    #[test]
    fn missing_file_is_an_empty_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mcp.yaml");
        let config = McpConfig::load_from(&path);
        assert!(config.servers.is_empty());
        assert!(config.notices.is_empty());
    }

    #[test]
    fn stdio_and_http_entries_parse_with_defaults() {
        let config = load(
            "servers:\n  \
             atlassian:\n    \
             description: Jira and Confluence\n    \
             command: [\"npx\", \"-y\", \"@modelcontextprotocol/server-atlassian\"]\n    \
             env: { JIRA_TOKEN: \"tok\" }\n  \
             blender:\n    \
             description: Blender scene operations\n    \
             url: \"http://127.0.0.1:9721/mcp\"\n    \
             headers: { Authorization: \"Bearer tok\" }\n    \
             startup_timeout: 5\n    \
             timeout: 30\n    \
             command_name: blend\n",
        );
        assert!(config.notices.is_empty(), "{:?}", config.notices);
        assert_eq!(config.servers.len(), 2);
        let atlassian = &config.servers[0];
        assert_eq!(atlassian.name, "atlassian");
        assert_eq!(atlassian.command_name, "atlassian");
        assert_eq!(atlassian.startup_timeout, DEFAULT_STARTUP_TIMEOUT);
        assert_eq!(atlassian.timeout, DEFAULT_TIMEOUT);
        let McpTransport::Stdio { command, env } = &atlassian.transport else {
            panic!("stdio expected, got {:?}", atlassian.transport);
        };
        assert_eq!(
            command,
            &["npx", "-y", "@modelcontextprotocol/server-atlassian"]
        );
        assert_eq!(env.get("JIRA_TOKEN").map(String::as_str), Some("tok"));
        let blender = &config.servers[1];
        assert_eq!(blender.command_name, "blend");
        assert_eq!(blender.startup_timeout, Duration::from_secs(5));
        assert_eq!(blender.timeout, Duration::from_secs(30));
        let McpTransport::Http { url, headers } = &blender.transport else {
            panic!("http expected, got {:?}", blender.transport);
        };
        assert_eq!(url, "http://127.0.0.1:9721/mcp");
        assert_eq!(
            headers.get("Authorization").map(String::as_str),
            Some("Bearer tok")
        );
    }

    #[test]
    fn debug_redacts_secret_values() {
        let config = load(
            "servers:\n  \
             a:\n    \
             description: d\n    \
             url: \"http://127.0.0.1:9/mcp\"\n    \
             headers: { Authorization: \"Bearer sekret\" }\n",
        );
        let rendered = format!("{:?}", config.servers[0]);
        assert!(!rendered.contains("sekret"), "{rendered}");
        assert!(rendered.contains("Authorization"), "{rendered}");
        assert!(rendered.contains("[…]"), "{rendered}");
    }

    #[test]
    fn command_and_url_are_exclusive() {
        let config = load(
            "servers:\n  \
             both:\n    \
             description: d\n    \
             command: [\"x\"]\n    \
             url: \"http://127.0.0.1:9/mcp\"\n",
        );
        assert!(config.servers.is_empty());
        assert!(
            config.notices.iter().any(|n| n.contains("exclusive")),
            "{:?}",
            config.notices
        );
    }

    #[test]
    fn missing_transport_is_a_notice() {
        let config = load("servers:\n  neither:\n    description: d\n");
        assert!(config.servers.is_empty());
        assert!(
            config
                .notices
                .iter()
                .any(|n| n.contains("one of `command`")),
            "{:?}",
            config.notices
        );
    }

    #[test]
    fn a_missing_description_defaults_instead_of_dropping_the_server() {
        // A forgotten yaml field is not a reason to skip the server:
        // the entry parses with no description and the fold
        // substitutes the --help pointer at disclosure time.
        let config = load("servers:\n  blender:\n    command: [uvx, blender-mcp]\n");
        assert!(config.notices.is_empty(), "{:?}", config.notices);
        let server = config.servers.first().expect("the server survives");
        assert_eq!(
            server.description,
            "no description provided; blender --help lists its tools"
        );
        // And an explicit one rides verbatim.
        let config = load("servers:\n  b:\n    description: scene ops\n    command: [x]\n");
        assert_eq!(config.servers[0].description, "scene ops");
    }

    #[test]
    fn misplaced_env_and_headers_reject() {
        let config = load(
            "servers:\n  \
             a:\n    \
             description: d\n    \
             url: \"http://127.0.0.1:9/mcp\"\n    \
             env: { X: \"y\" }\n",
        );
        assert!(config.servers.is_empty());
        assert!(
            config.notices.iter().any(|n| n.contains("`env` belongs")),
            "{:?}",
            config.notices
        );
    }

    #[test]
    fn bad_names_and_command_names_skip() {
        let config = load(
            "servers:\n  \
             \"My Server\":\n    \
             description: d\n    \
             command: [\"x\"]\n  \
             good-one:\n    \
             description: d\n    \
             command: [\"x\"]\n    \
             command_name: \"2bad\"\n",
        );
        assert_eq!(config.servers.len(), 0);
        assert_eq!(config.notices.len(), 2, "{:?}", config.notices);
    }

    #[test]
    fn duplicate_names_reject_the_whole_file() {
        // serde_yaml's parser rejects duplicate keys outright, so the
        // fold never sees them: the file skips whole, notice carries
        // the parser's message.
        let config = load(
            "servers:\n  \
             a:\n    \
             description: first\n    \
             command: [\"x\"]\n  \
             a:\n    \
             description: second\n    \
             command: [\"y\"]\n",
        );
        assert!(config.servers.is_empty());
        assert!(
            config.notices.iter().any(|n| n.contains("duplicate entry")),
            "{:?}",
            config.notices
        );
    }

    #[test]
    fn a_malformed_entry_leaves_its_neighbors_alive() {
        let config = load(
            "servers:\n  \
             broken:\n    \
             description: d\n    \
             startup_timeout: \"fast\"\n    \
             command: [\"x\"]\n  \
             working:\n    \
             description: d\n    \
             command: [\"x\"]\n",
        );
        assert_eq!(config.servers.len(), 1);
        assert_eq!(config.servers[0].name, "working");
        assert_eq!(config.notices.len(), 1);
    }

    #[test]
    fn unknown_fields_reject_the_entry_not_the_file() {
        let config = load(
            "servers:\n  \
             a:\n    \
             description: d\n    \
             command: [\"x\"]\n    \
             oauth: true\n",
        );
        assert!(config.servers.is_empty());
        assert!(
            config.notices.iter().any(|n| n.contains("unknown field")),
            "{:?}",
            config.notices
        );
    }

    #[test]
    fn unparseable_file_is_one_notice_and_no_servers() {
        let config = load("servers: [oops");
        assert!(config.servers.is_empty());
        assert_eq!(config.notices.len(), 1);
    }

    #[test]
    fn servers_mapping_of_the_wrong_shape_is_a_notice() {
        let config = load("servers: 3\n");
        assert!(config.servers.is_empty());
        assert!(config.notices.iter().any(|n| n.contains("not a mapping")));
    }

    #[test]
    fn missing_servers_mapping_is_a_notice() {
        let config = load("other: true\n");
        assert!(config.servers.is_empty());
        assert!(
            config
                .notices
                .iter()
                .any(|n| n.contains("no servers mapping"))
        );
    }

    #[test]
    fn zero_timeouts_reject() {
        let config = load(
            "servers:\n  \
             a:\n    \
             description: d\n    \
             command: [\"x\"]\n    \
             timeout: 0\n",
        );
        assert!(config.servers.is_empty());
        assert!(
            config
                .notices
                .iter()
                .any(|n| n.contains("at least one second")),
            "{:?}",
            config.notices
        );
    }

    #[test]
    fn the_reserved_name_survives_config() {
        // `mcp` is reserved for the meta surface; provision withholds
        // its generated function, config keeps the server reachable
        // through `mcp call`.
        let config = load(
            "servers:\n  \
             mcp:\n    \
             description: d\n    \
             command: [\"x\"]\n",
        );
        assert!(config.notices.is_empty(), "{:?}", config.notices);
        assert_eq!(config.servers.len(), 1);
    }

    #[test]
    fn the_name_charset_is_lowercase_only() {
        assert!(server_name_ok("atlassian"));
        assert!(server_name_ok("my-server_2"));
        assert!(!server_name_ok(""));
        assert!(!server_name_ok("GitHub"));
        assert!(!server_name_ok("a b"));
        assert!(!server_name_ok("a.b"));
    }

    #[test]
    fn the_command_name_charset_is_a_bash_identifier() {
        assert!(command_name_ok("jira"));
        assert!(command_name_ok("my-server_2"));
        assert!(!command_name_ok("2jira"));
        assert!(!command_name_ok("-jira"));
        assert!(!command_name_ok(""));
        assert!(!command_name_ok("ji.ra"));
    }
}
