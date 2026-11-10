//! The credential a ChatGPT sign-in produces.
//!
//! The login flow returns one opaque string, the [`InteractiveLogin`]
//! seam's shape. Here that string is a small JSON object: the
//! OAuth tokens, plus the account id, email, plan, and expiry the ID
//! token's claims name. A bare string is accepted too, as an access
//! token, so an operator can point the adapter at a token they already
//! hold without running the flow.
//!
//! The tokens are read from the issuer's JWT payloads without
//! verifying the signature: they arrived over TLS from the issuer that
//! minted them, and the provider only uses the claims to fill headers
//! and pick models. Signature verification would need the issuer's
//! JWKS, which nothing here consumes.

use base64::Engine;
use serde::{Deserialize, Serialize};

/// The ChatGPT sign-in credential: the OAuth tokens plus the account
/// facts the ID token carries.
///
/// Serialized by the login flow's `Completed` string, persisted by a
/// frontend like any API key, and re-parsed by
/// [`crate::ChatGptProvider::new`].
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatGptCredential {
    /// The bearer token the backend calls carry.
    pub access_token: String,
    /// The refresh token, when the flow granted `offline_access`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// The ChatGPT workspace/account id the `chatgpt-account-id`
    /// header carries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// The account's email, for display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// The subscription plan the ID token names (`plus`, `pro`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// Unix seconds the access token expires at, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
}

impl std::fmt::Debug for ChatGptCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The tokens authenticate; they never print.
        f.debug_struct("ChatGptCredential")
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("account_id", &self.account_id)
            .field("email", &self.email)
            .field("plan", &self.plan)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl ChatGptCredential {
    /// Parses a stored credential: the login flow's JSON, or a bare
    /// access token.
    ///
    /// A JSON object without a non-empty `access_token` is not a
    /// credential; the raw string is then kept as the access token,
    /// which is what an operator passing a lone token means.
    pub fn parse(raw: &str) -> Self {
        let raw = raw.trim();
        if let Ok(mut parsed) = serde_json::from_str::<Self>(raw)
            && !parsed.access_token.is_empty()
        {
            if parsed.refresh_token.as_deref() == Some("") {
                parsed.refresh_token = None;
            }
            // A hand-written JSON may omit what the login derives;
            // fill it from the access token's claims.
            if (parsed.account_id.is_none() || parsed.expires_at.is_none())
                && let Some(claims) = claims_from_jwt(&parsed.access_token)
            {
                parsed.account_id = parsed.account_id.or(claims.account_id);
                parsed.email = parsed.email.or(claims.email);
                parsed.plan = parsed.plan.or(claims.plan);
                parsed.expires_at = parsed.expires_at.or(claims.exp);
            }
            return parsed;
        }
        let mut credential = Self {
            access_token: raw.to_owned(),
            ..Self::default()
        };
        // A bare OAuth access token is a JWT that already names the
        // account and its expiry; read them so the provider can send
        // the account header without a JSON wrapper.
        if let Some(claims) = claims_from_jwt(raw) {
            credential.account_id = claims.account_id;
            credential.email = claims.email;
            credential.plan = claims.plan;
            credential.expires_at = claims.exp;
        }
        credential
    }

    /// The JSON the login flow reports as its `Completed` credential.
    /// Serialization cannot fail for this field set; the fallback keeps
    /// the access token rather than an empty string.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| self.access_token.clone())
    }

    /// Whether the credential is close enough to expiry to refresh now:
    /// `true` within [`EXPIRY_MARGIN`] seconds, `false` when it carries
    /// no expiry (the server stays the authority).
    pub fn needs_refresh(&self, now: u64) -> bool {
        self.expires_at
            .is_some_and(|expires_at| now.saturating_add(EXPIRY_MARGIN) >= expires_at)
    }

    /// Builds the credential from a token endpoint's answer.
    pub(crate) fn from_tokens(
        access_token: String,
        refresh_token: Option<String>,
        id_token: Option<String>,
        expires_in: Option<u64>,
    ) -> Self {
        let id_claims = id_token.as_deref().and_then(claims_from_jwt);
        let access_claims = claims_from_jwt(&access_token);
        // The ID token names the account, but a deployment may omit a
        // claim there that the access token carries; take each field
        // from the first token that has it.
        let claim = |pick: fn(&Claims) -> Option<String>| {
            id_claims
                .as_ref()
                .and_then(pick)
                .or_else(|| access_claims.as_ref().and_then(pick))
        };
        let expires_at = expires_in
            .map(|seconds| now_epoch().saturating_add(seconds))
            .or_else(|| {
                access_claims
                    .as_ref()
                    .and_then(|claims| claims.exp)
                    .or_else(|| id_claims.as_ref().and_then(|claims| claims.exp))
            })
            .or_else(|| Some(now_epoch().saturating_add(DEFAULT_LIFETIME)));
        Self {
            access_token,
            refresh_token: refresh_token.filter(|token| !token.is_empty()),
            account_id: claim(|claims| claims.account_id.clone()),
            email: claim(|claims| claims.email.clone()),
            plan: claim(|claims| claims.plan.clone()),
            expires_at,
        }
    }
}

/// The OAuth token endpoint's answer, shared by the code exchange and
/// the refresh grant.
#[derive(Default, Deserialize)]
pub(crate) struct TokenResponse {
    /// The bearer access token.
    #[serde(default)]
    pub access_token: String,
    /// A rotated refresh token, when the issuer returns one.
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// A fresh ID token, when the issuer returns one.
    #[serde(default)]
    pub id_token: Option<String>,
    /// The access token's lifetime in seconds.
    #[serde(default)]
    pub expires_in: Option<u64>,
}

/// Refresh this long before the access token's expiry: a margin
/// refresh costs one exchange; a mid-turn 401 costs a failed call plus
/// a retry.
const EXPIRY_MARGIN: u64 = 120;

/// The lifetime assumed when neither the token endpoint nor the JWTs
/// state one.
const DEFAULT_LIFETIME: u64 = 3_600;

/// The useful claims of an OpenAI ID or access token.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Claims {
    /// Standard `exp`, in Unix seconds.
    pub exp: Option<u64>,
    /// The account email.
    pub email: Option<String>,
    /// The ChatGPT workspace/account id.
    pub account_id: Option<String>,
    /// The subscription plan (`plus`, `pro`, `team`, …).
    pub plan: Option<String>,
}

/// Reads the claims out of a JWT payload, `None` when the token is
/// malformed. The signature is not verified; see the module docs.
pub(crate) fn claims_from_jwt(jwt: &str) -> Option<Claims> {
    let mut parts = jwt.split('.');
    let (_header, payload) = (parts.next()?, parts.next()?);
    if payload.is_empty() {
        return None;
    }
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .ok()?;
    let raw: RawClaims = serde_json::from_slice(&decoded).ok()?;
    let auth = raw.auth.unwrap_or_default();
    let profile = raw.profile.unwrap_or_default();
    Some(Claims {
        exp: raw.exp,
        email: raw.email.or(profile.email),
        account_id: auth.chatgpt_account_id,
        plan: auth.chatgpt_plan_type,
    })
}

/// The JSON payload shape the issuer stamps. Unknown fields are
/// ignored on purpose: the token carries much more than this adapter
/// needs.
#[derive(Debug, Default, Deserialize)]
struct RawClaims {
    #[serde(default)]
    exp: Option<u64>,
    #[serde(default)]
    email: Option<String>,
    #[serde(rename = "https://api.openai.com/profile", default)]
    profile: Option<ProfileClaims>,
    #[serde(rename = "https://api.openai.com/auth", default)]
    auth: Option<AuthClaims>,
}

/// The namespaced profile claim.
#[derive(Debug, Default, Deserialize)]
struct ProfileClaims {
    #[serde(default)]
    email: Option<String>,
}

/// The namespaced auth claim.
#[derive(Debug, Default, Deserialize)]
struct AuthClaims {
    #[serde(default)]
    chatgpt_account_id: Option<String>,
    #[serde(default)]
    chatgpt_plan_type: Option<String>,
}

/// Unix seconds now.
pub(crate) fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A JWT whose payload is `claims`, signed with a placeholder,
    /// the adapter reads the payload only.
    fn jwt(claims: serde_json::Value) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{\"alg\":\"none\"}");
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&claims).unwrap());
        format!("{header}.{payload}.signature")
    }

    #[test]
    fn a_bare_token_is_the_access_token() {
        let credential = ChatGptCredential::parse("  at-123  ");
        assert_eq!(credential.access_token, "at-123");
        assert_eq!(credential.refresh_token, None);
    }

    #[test]
    fn debug_never_prints_the_tokens() {
        let credential = ChatGptCredential {
            access_token: "access-secret".into(),
            refresh_token: Some("refresh-secret".into()),
            account_id: Some("acct".into()),
            ..ChatGptCredential::default()
        };
        let printed = format!("{credential:?}");
        assert!(!printed.contains("access-secret"), "{printed}");
        assert!(!printed.contains("refresh-secret"), "{printed}");
        assert!(printed.contains("<redacted>"), "{printed}");
        assert!(printed.contains("acct"), "{printed}");
    }

    #[test]
    fn a_bare_access_token_jwt_supplies_its_own_claims() {
        let token = jwt(serde_json::json!({
            "exp": 1_900_000_000u64,
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct-7",
                "chatgpt_plan_type": "pro"
            }
        }));
        let credential = ChatGptCredential::parse(&token);
        assert_eq!(credential.access_token, token);
        assert_eq!(credential.account_id.as_deref(), Some("acct-7"));
        assert_eq!(credential.plan.as_deref(), Some("pro"));
        assert_eq!(credential.expires_at, Some(1_900_000_000));
    }

    #[test]
    fn a_hand_written_json_fills_claims_and_drops_an_empty_refresh() {
        let token = jwt(serde_json::json!({
            "exp": 1_800_000_000u64,
            "https://api.openai.com/auth": {"chatgpt_account_id": "acct-8"}
        }));
        let raw = serde_json::json!({
            "access_token": token,
            "refresh_token": ""
        })
        .to_string();
        let credential = ChatGptCredential::parse(&raw);
        assert_eq!(credential.refresh_token, None);
        assert_eq!(credential.account_id.as_deref(), Some("acct-8"));
        assert_eq!(credential.expires_at, Some(1_800_000_000));
    }

    #[test]
    fn the_login_json_round_trips() {
        let credential = ChatGptCredential {
            access_token: "access".into(),
            refresh_token: Some("refresh".into()),
            account_id: Some("account".into()),
            email: Some("a@example.com".into()),
            plan: Some("plus".into()),
            expires_at: Some(42),
        };
        assert_eq!(ChatGptCredential::parse(&credential.to_json()), credential);
    }

    #[test]
    fn an_empty_json_object_falls_back_to_the_raw_string() {
        let credential = ChatGptCredential::parse("{}");
        assert_eq!(credential.access_token, "{}");
    }

    #[test]
    fn the_id_token_claims_name_the_account() {
        let id_token = jwt(serde_json::json!({
            "exp": 2_000_000_000u64,
            "email": "top@example.com",
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct-1",
                "chatgpt_plan_type": "pro"
            }
        }));
        let credential = ChatGptCredential::from_tokens(
            "access".into(),
            Some("refresh".into()),
            Some(id_token),
            None,
        );
        assert_eq!(credential.account_id.as_deref(), Some("acct-1"));
        assert_eq!(credential.email.as_deref(), Some("top@example.com"));
        assert_eq!(credential.plan.as_deref(), Some("pro"));
        assert_eq!(credential.expires_at, Some(2_000_000_000));
    }

    #[test]
    fn the_profile_claim_supplies_the_email_when_the_top_level_omits_it() {
        let token = jwt(serde_json::json!({
            "https://api.openai.com/profile": { "email": "profile@example.com" },
            "https://api.openai.com/auth": { "chatgpt_account_id": "acct-2" }
        }));
        let claims = claims_from_jwt(&token).expect("the payload parses");
        assert_eq!(claims.email.as_deref(), Some("profile@example.com"));
        assert_eq!(claims.account_id.as_deref(), Some("acct-2"));
    }

    #[test]
    fn a_malformed_jwt_is_ignored() {
        assert!(claims_from_jwt("not-a-jwt").is_none());
        assert!(claims_from_jwt("a..c").is_none());
        assert!(claims_from_jwt("a.!!!.c").is_none());
    }

    #[test]
    fn the_expiry_margin_decides_proactive_refresh() {
        let credential = ChatGptCredential {
            access_token: "a".into(),
            expires_at: Some(1_000),
            ..ChatGptCredential::default()
        };
        assert!(credential.needs_refresh(880));
        assert!(!credential.needs_refresh(879));
        assert!(!ChatGptCredential::default().needs_refresh(u64::MAX));
    }
}
