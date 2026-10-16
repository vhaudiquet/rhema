//! Why a calendar call failed.
//!
//! One error type for the whole crate: every adapter maps its transport,
//! HTTP, and decoding failures onto it, so callers match one shape no
//! matter which feed answered.

/// Everything that can go wrong between a [`crate::Calendar`] call and
/// its answer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CalendarError {
    /// The feed answered with a non-2xx status. `body` is a bounded
    /// snippet of the response body, usually the server's error page or
    /// JSON.
    #[error("calendar api error (HTTP {status}): {body}")]
    Api {
        /// The HTTP status the feed answered with.
        status: u16,
        /// A bounded snippet of the response body.
        body: String,
    },
    /// A response body could not be decoded, or a wire row is missing a
    /// field the normalized vocabulary requires. Loud by design: feeds
    /// are machine-generated, and a shape break must surface, not
    /// silently shrink the calendar.
    #[error("malformed calendar response: {0}")]
    Parse(String),
    /// The server kept offering full pages, or offered a page cursor
    /// that leads nowhere. Polling stopped rather than looping forever.
    #[error("pagination failed: {0}")]
    Pagination(String),
    /// The request never got an answer (DNS, connect, timeout).
    #[error("network error: {0}")]
    Network(String),
}

/// The longest body snippet an [`CalendarError::Api`] keeps: enough for
/// any server's error JSON, small enough that error lines stay bounded.
const BODY_SNIPPET_CAP: usize = 512;

impl CalendarError {
    /// Builds [`CalendarError::Api`] from a non-2xx status and the raw
    /// body, truncating the body to [`BODY_SNIPPET_CAP`] characters.
    pub(crate) fn api(status: reqwest::StatusCode, raw: &str) -> Self {
        let body: String = raw.chars().take(BODY_SNIPPET_CAP).collect();
        CalendarError::Api {
            status: status.as_u16(),
            body,
        }
    }

    /// Maps a transport failure onto [`CalendarError::Network`];
    /// reqwest's error is neither `Clone` nor `Eq`, so only its display
    /// text travels.
    pub(crate) fn network(err: reqwest::Error) -> Self {
        CalendarError::Network(err.to_string())
    }
}

/// A wire field the normalized vocabulary cannot do without. A missing
/// one is a loud [`CalendarError::Parse`], not a silently skipped row.
pub(crate) fn required<T>(field: Option<T>, what: &str) -> Result<T, CalendarError> {
    field.ok_or_else(|| CalendarError::Parse(format!("calendar row without {what}")))
}

/// Passes a 2xx response through; turns anything else into
/// [`CalendarError::Api`] with a bounded body snippet.
pub(crate) async fn ensure_success(
    response: reqwest::Response,
) -> Result<reqwest::Response, CalendarError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    Err(CalendarError::api(status, &body))
}
