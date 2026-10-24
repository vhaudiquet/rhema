//! The deployment's sandbox backend selection: which backend a turn's
//! `bash` runs in, decoded from the deployment environment — a
//! deployment decision, never `SessionConfig` (a per-session
//! client-chosen URL would be an SSRF on a server that executes
//! commands). Plain data and pure functions: `main` reads the process
//! env once at startup and hands the values to [`select_backend`], so
//! the choice matrix is testable without mutating process env.
#![forbid(unsafe_code)]

/// The env var naming the sandbox backend.
pub const BACKEND_VAR: &str = "SANDBOX_BACKEND";

/// The env var carrying the `http` backend's daemon base URL.
pub const URL_VAR: &str = "SANDBOX_URL";

/// The env var carrying the `http` backend's shared secret.
pub const TOKEN_VAR: &str = "SANDBOX_TOKEN";

/// The sandbox backend a deployment selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxBackend {
    /// The platform's own sandbox: a named Vercel Sandbox per session,
    /// through the REST API. The default when [`BACKEND_VAR`] is unset.
    Vercel,
    /// The sandbox protocol over plain HTTPS to a daemon at a base URL
    /// (`https://box.example:7600`, no trailing slash); the shared
    /// secret rides every route's token header, and the Vercel REST
    /// API is never called.
    Http {
        /// The daemon's base URL.
        url: String,
        /// The shared secret the daemon checks on every route.
        token: String,
    },
}

/// Decodes the deployment's backend choice from the raw env values.
/// Unknown kinds and a missing URL under `http` are config errors, not
/// silent fallbacks — a deployment that misspells `http` must not
/// quietly get the REST sandbox. The token is the daemon's problem to
/// refuse: an absent one only fails at the first route, the way a
/// wrong one does.
pub fn select_backend(
    backend: Option<&str>,
    url: Option<&str>,
    token: Option<&str>,
) -> Result<SandboxBackend, String> {
    match backend.unwrap_or("vercel") {
        "vercel" => Ok(SandboxBackend::Vercel),
        "http" => {
            let url = url.filter(|url| !url.trim().is_empty()).ok_or_else(|| {
                format!(
                    "{URL_VAR} is not set: {BACKEND_VAR}=http runs bash on the sandbox \
                         daemon at that URL (e.g. https://box.example:7600)"
                )
            })?;
            Ok(SandboxBackend::Http {
                url: url.to_owned(),
                token: token.unwrap_or_default().to_owned(),
            })
        }
        other => Err(format!(
            "unknown {BACKEND_VAR} `{other}` (want `vercel` or `http`)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_backend_is_the_vercel_rest_sandbox() {
        assert_eq!(
            select_backend(None, None, None).ok(),
            Some(SandboxBackend::Vercel)
        );
    }

    #[test]
    fn vercel_names_the_rest_sandbox_explicitly() {
        let env = (
            Some("vercel"),
            Some("https://ignored.example"),
            Some("sekrit"),
        );
        assert_eq!(
            select_backend(env.0, env.1, env.2).ok(),
            Some(SandboxBackend::Vercel)
        );
    }

    #[test]
    fn http_yields_the_daemon_url_and_token() {
        let backend = select_backend(
            Some("http"),
            Some("https://box.example:7600"),
            Some("sekrit"),
        )
        .expect("the http choice decodes");
        assert_eq!(
            backend,
            SandboxBackend::Http {
                url: "https://box.example:7600".into(),
                token: "sekrit".into(),
            }
        );
    }

    #[test]
    fn http_without_a_url_is_a_config_error() {
        for (url, token) in [(None, Some("sekrit")), (Some(String::new()), None)] {
            let message = select_backend(Some("http"), url.as_deref(), token)
                .expect_err("the missing URL surfaces");
            assert!(
                message.contains(URL_VAR) && message.contains(BACKEND_VAR),
                "names the var and the fix: {message}"
            );
        }
    }

    #[test]
    fn an_unknown_backend_is_a_config_error() {
        let message =
            select_backend(Some("ssh"), None, None).expect_err("the unknown kind surfaces");
        assert!(
            message.contains(BACKEND_VAR) && message.contains("ssh"),
            "names the var and the value: {message}"
        );
    }
}
