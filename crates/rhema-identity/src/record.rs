//! The account data: records, drafts, secrets — and the small shared
//! pieces every store composes (validation, service normalization, id
//! assignment).
//!
//! Timestamps are milliseconds since the Unix epoch, the framework's
//! convention (`rhema-rsd`'s `Clock`).
//!
//! The id borrows `rhema-memory`'s shape — `{UTC second}-{counter:08x}` —
//! because both stores read the same way: creation order is id order.

use std::fmt;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::store::IdentityError;

/// A secret value is cut at this many bytes: room for any token or
/// credential blob, small enough that an accidental echo stays bounded.
pub const SECRET_LIMIT: usize = 4 * 1024;

/// A label is cut at this many bytes.
pub const LABEL_LIMIT: usize = 128;

/// A url or username is cut at this many bytes.
pub const FIELD_LIMIT: usize = 256;

/// What kind of credential the account holds. Drives policy at the
/// consumers: a [`SecretKind::Token`] is working material a tool may act
/// through; a [`SecretKind::Password`] is stored for explicit,
/// user-approved login actions only — never model-accessible in v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SecretKind {
    /// An API token (a forge token, a CLI tool's credential).
    Token,
    /// A password. Stored, gated, and never handed to a model.
    Password,
}

impl SecretKind {
    /// The name the table's `secret_kind` column stores.
    pub fn name(self) -> &'static str {
        match self {
            SecretKind::Token => "token",
            SecretKind::Password => "password",
        }
    }

    /// Parses the column spelling. Private: rows only enter through
    /// [`crate::IdentityStore::put`], and this guards only hand-edited
    /// tables.
    pub(crate) fn parse(name: &str) -> Option<Self> {
        match name {
            "token" => Some(SecretKind::Token),
            "password" => Some(SecretKind::Password),
            _ => None,
        }
    }
}

/// A stored credential. [`fmt::Debug`] is manual and value-free, so a
/// debug dump of anything holding a secret prints `[…]`, never the
/// value; the type carries no `Serialize` impl, so it cannot reach a
/// wire shape by accident.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret {
    /// What the value authenticates as.
    pub kind: SecretKind,
    /// The credential itself. Handle with care: this is the one place
    /// the value exists outside the backend.
    pub value: String,
}

impl Secret {
    /// A secret of `kind`.
    pub fn new(kind: SecretKind, value: impl Into<String>) -> Self {
        Self {
            kind,
            value: value.into(),
        }
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Secret")
            .field("kind", &self.kind)
            .field("value", &"[…]")
            .finish()
    }
}

/// One identity the deployment holds on one service, as the store hands
/// it out: **redacted by construction** — the struct has no secret field,
/// so no caller can leak what it cannot reach. The credential is reached
/// only through [`crate::IdentityStore::secret`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Account {
    /// Store-assigned: `{UTC second}-{counter:08x}`.
    pub id: String,
    /// The display name the user gave it (`"acme bot"`, `"work email"`).
    pub label: String,
    /// The service, normalized lowercase (`"github"`, `"smtp"`). Neutral
    /// vocabulary: consumers map services they know to their own kinds.
    pub service: String,
    /// The service's base URL, when the service has one worth naming
    /// (self-hosted instances especially).
    pub url: Option<String>,
    /// The login name on the service, when there is one.
    pub username: Option<String>,
    /// What kind of secret the account holds. The secret itself never
    /// rides this struct.
    pub secret_kind: SecretKind,
    /// Millis since the Unix epoch; assigned once at creation.
    pub created_at: u64,
    /// Millis since the Unix epoch; refreshed on every write.
    pub updated_at: u64,
}

/// What a caller supplies to [`crate::IdentityStore::put`]. `id: None`
/// creates; `Some` replaces that account's metadata *and* secret —
/// `created_at` preserved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountDraft {
    /// `None` to create; `Some` an existing account to replace.
    pub id: Option<String>,
    /// The display name.
    pub label: String,
    /// The service, stored normalized (trimmed, lowercased).
    pub service: String,
    /// The service's base URL, if any.
    pub url: Option<String>,
    /// The login name, if any.
    pub username: Option<String>,
    /// The credential to store. Required: an account without a secret is
    /// a placeholder, and the store does not hold placeholders.
    pub secret: Secret,
}

impl AccountDraft {
    /// A draft creating an account.
    pub fn new(label: impl Into<String>, service: impl Into<String>, secret: Secret) -> Self {
        Self {
            id: None,
            label: label.into(),
            service: service.into(),
            url: None,
            username: None,
            secret,
        }
    }

    /// Sets the service's base URL.
    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.url = Some(url.into());
        self
    }

    /// Sets the login name.
    pub fn with_username(mut self, username: impl Into<String>) -> Self {
        self.username = Some(username.into());
        self
    }

    /// Targets an existing account for replacement.
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// The checks every store applies before touching its backend.
    /// Returns the normalized fields the store writes (label trimmed,
    /// service trimmed and lowercased, optional fields trimmed to
    /// `None`).
    pub(crate) fn validate(&self) -> Result<Normalized, IdentityError> {
        let label = self.label.trim();
        if label.is_empty() {
            return Err(IdentityError::EmptyLabel);
        }
        if label.len() > LABEL_LIMIT {
            return Err(IdentityError::FieldTooLong {
                field: "label",
                len: label.len(),
                limit: LABEL_LIMIT,
            });
        }
        let service = self.service.trim().to_lowercase();
        if service.is_empty() {
            return Err(IdentityError::EmptyService);
        }
        if service.len() > FIELD_LIMIT {
            return Err(IdentityError::FieldTooLong {
                field: "service",
                len: service.len(),
                limit: FIELD_LIMIT,
            });
        }
        let trim_field = |field: &Option<String>, name: &'static str| match field
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(value) if value.len() <= FIELD_LIMIT => Ok(Some(value.to_owned())),
            Some(value) => Err(IdentityError::FieldTooLong {
                field: name,
                len: value.len(),
                limit: FIELD_LIMIT,
            }),
            None => Ok(None),
        };
        let url = trim_field(&self.url, "url")?;
        let username = trim_field(&self.username, "username")?;
        let value = self.secret.value.trim();
        if value.is_empty() {
            return Err(IdentityError::EmptySecret);
        }
        if value.len() > SECRET_LIMIT {
            return Err(IdentityError::SecretTooLong {
                len: value.len(),
                limit: SECRET_LIMIT,
            });
        }
        if let Some(id) = &self.id {
            check_id(id)?;
        }
        Ok(Normalized {
            label: label.to_owned(),
            service,
            url,
            username,
            secret: Secret {
                kind: self.secret.kind,
                value: value.to_owned(),
            },
        })
    }
}

/// The normalized fields [`AccountDraft::validate`] hands the stores, so
/// both backends write byte-identical rows.
pub(crate) struct Normalized {
    pub label: String,
    pub service: String,
    pub url: Option<String>,
    pub username: Option<String>,
    pub secret: Secret,
}

/// Ids are `{UTC second}-{counter:08x}`; this shape check keeps a
/// caller-supplied id inside the store's namespace.
pub(crate) fn check_id(id: &str) -> Result<(), IdentityError> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    if ok {
        Ok(())
    } else {
        Err(IdentityError::InvalidId { id: id.to_owned() })
    }
}

/// The counter half of store-assigned ids: process-global and monotonic,
/// so fresh ids differ even when the clock's granularity would not.
#[derive(Debug, Default)]
pub(crate) struct IdCounter(AtomicU32);

impl IdCounter {
    /// A fresh id for `now`.
    pub(crate) fn fresh(&self, now: SystemTime) -> String {
        let n = self.0.fetch_add(1, Ordering::Relaxed);
        let millis = now
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        format!("{}-{n:08x}", stamp_compact(millis))
    }
}

/// The id's date half: compact UTC of `unix_millis`, to the second.
fn stamp_compact(unix_millis: u64) -> String {
    let secs = (unix_millis / 1000) as i64;
    let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
    let time = secs.rem_euclid(86_400);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}",
        time / 3600,
        time / 60 % 60,
        time % 60
    )
}

/// Days since the Unix epoch to `(year, month, day)` — Howard Hinnant's
/// `civil_from_days`, proleptic Gregorian.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [1, 12]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_accept_only_the_generated_shape() {
        assert!(check_id("20261008T113456-0000002a").is_ok());
        assert!(check_id("").is_err(), "empty");
        assert!(check_id("../escape").is_err(), "path separators");
        assert!(check_id("has_underscore").is_err(), "underscore");
        assert!(check_id(&"x".repeat(65)).is_err(), "too long");
    }

    #[test]
    fn fresh_ids_fit_the_shape_and_differ() {
        let counter = IdCounter::default();
        let now = UNIX_EPOCH + std::time::Duration::from_millis(1_759_923_296_123);
        let first = counter.fresh(now);
        let second = counter.fresh(now);
        assert_eq!(first, "20251008T113456-00000000");
        assert_eq!(second, "20251008T113456-00000001");
    }

    #[test]
    fn drafts_validate_and_normalize() {
        let draft = AccountDraft::new(
            "  acme bot ",
            " GitHub ",
            Secret::new(SecretKind::Token, " tok "),
        )
        .with_url("  https://github.com/  ")
        .with_username("  octocat ");
        let norm = draft.validate().expect("valid");
        assert_eq!(norm.label, "acme bot");
        assert_eq!(norm.service, "github");
        assert_eq!(norm.url.as_deref(), Some("https://github.com/"));
        assert_eq!(norm.username.as_deref(), Some("octocat"));
        assert_eq!(norm.secret.value, "tok");
    }

    #[test]
    fn drafts_reject_empties_and_oversize() {
        let empty_label = AccountDraft::new("  ", "github", Secret::new(SecretKind::Token, "t"));
        assert!(matches!(
            empty_label.validate(),
            Err(IdentityError::EmptyLabel)
        ));
        let empty_service = AccountDraft::new("a", "  ", Secret::new(SecretKind::Token, "t"));
        assert!(matches!(
            empty_service.validate(),
            Err(IdentityError::EmptyService)
        ));
        let empty_secret = AccountDraft::new("a", "github", Secret::new(SecretKind::Token, "  "));
        assert!(matches!(
            empty_secret.validate(),
            Err(IdentityError::EmptySecret)
        ));
        let long_secret = Secret::new(SecretKind::Token, "x".repeat(SECRET_LIMIT + 1));
        let oversized = AccountDraft::new("a", "github", long_secret);
        assert!(matches!(
            oversized.validate(),
            Err(IdentityError::SecretTooLong {
                limit: SECRET_LIMIT,
                ..
            })
        ));
    }

    #[test]
    fn secrets_debug_without_their_value() {
        let secret = Secret::new(SecretKind::Password, "hunter2");
        let rendered = format!("{secret:?}");
        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert!(rendered.contains("[…]"), "{rendered}");
    }
}
