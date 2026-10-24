//! The deployment's sandbox backend selection: which backend a turn's
//! `bash` runs in, decoded from the deployment environment — a
//! deployment decision, never `SessionConfig` (a per-session
//! client-chosen URL would be an SSRF on a server that executes
//! commands). The platform side — reading workerd's env and building
//! the client the choice names — lives in the `worker` module; this
//! module is plain data and pure functions, so the choice matrix stays
//! testable off-wasm, where workerd does not exist.
#![forbid(unsafe_code)]

/// The env var naming the sandbox backend.
pub const BACKEND_VAR: &str = "SANDBOX_BACKEND";

/// The env var carrying the `http` backend's daemon base URL.
pub const URL_VAR: &str = "SANDBOX_URL";

/// The sandbox backend a deployment selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxBackend {
    /// The platform's own sandbox: the session's SandboxShim Durable
    /// Object. The default when [`BACKEND_VAR`] is unset.
    Cloudflare,
    /// The sandbox protocol over plain HTTPS to a daemon at a base URL
    /// (`https://box.example:7600`, no trailing slash); the shim's DO
    /// binding is never touched.
    Http(String),
}

/// The deployment env the selection reads, as plain values: the DO
/// reads workerd's env once per connection and hands the values here,
/// so this stays free of the `worker` crate.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxEnv {
    /// [`BACKEND_VAR`]'s value; absent means the default.
    pub backend: Option<String>,
    /// [`URL_VAR`]'s value; required under `http`.
    pub url: Option<String>,
}

/// Decodes the deployment's backend choice. Unknown kinds and a
/// missing URL under `http` are config errors, not silent fallbacks —
/// a deployment that misspells `http` must not quietly get the DO
/// shim.
pub fn select_backend(env: &SandboxEnv) -> Result<SandboxBackend, String> {
    match env.backend.as_deref().unwrap_or("cloudflare") {
        "cloudflare" => Ok(SandboxBackend::Cloudflare),
        "http" => match env.url.as_deref().filter(|url| !url.trim().is_empty()) {
            Some(url) => Ok(SandboxBackend::Http(url.to_owned())),
            None => Err(format!(
                "{URL_VAR} is not set: {BACKEND_VAR}=http runs bash on the sandbox \
                 daemon at that URL (e.g. https://box.example:7600)"
            )),
        },
        other => Err(format!(
            "unknown {BACKEND_VAR} `{other}` (want `cloudflare` or `http`)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_backend_is_the_do_shim() {
        assert_eq!(
            select_backend(&SandboxEnv::default()).ok(),
            Some(SandboxBackend::Cloudflare)
        );
    }

    #[test]
    fn cloudflare_names_the_do_shim_explicitly() {
        let env = SandboxEnv {
            backend: Some("cloudflare".into()),
            url: Some("https://ignored.example".into()),
        };
        assert_eq!(select_backend(&env).ok(), Some(SandboxBackend::Cloudflare));
    }

    #[test]
    fn http_yields_the_daemon_url() {
        let env = SandboxEnv {
            backend: Some("http".into()),
            url: Some("https://box.example:7600".into()),
        };
        assert_eq!(
            select_backend(&env).ok(),
            Some(SandboxBackend::Http("https://box.example:7600".into()))
        );
    }

    #[test]
    fn http_without_a_url_is_a_config_error() {
        let missing = SandboxEnv {
            backend: Some("http".into()),
            url: None,
        };
        let empty = SandboxEnv {
            backend: Some("http".into()),
            url: Some(String::new()),
        };
        for env in [missing, empty] {
            let message = select_backend(&env).expect_err("the missing URL surfaces");
            assert!(message.contains(URL_VAR), "names the var: {message}");
        }
    }

    #[test]
    fn an_unknown_backend_is_a_config_error() {
        let env = SandboxEnv {
            backend: Some("vercel".into()),
            url: None,
        };
        let message = select_backend(&env).expect_err("the unknown kind surfaces");
        assert!(
            message.contains(BACKEND_VAR) && message.contains("vercel"),
            "names the var and the value: {message}"
        );
    }
}
