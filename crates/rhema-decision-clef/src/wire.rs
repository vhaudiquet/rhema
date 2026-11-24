//! The route, the request's shaping, and the envelope's unwrapping.
//! Always compiled: a host without sockets maps its own HTTP through
//! these.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use rhema_decision::{DecisionError, DecisionRequest, DecisionResponse};

/// Which Clef serves: the model is the route on Workers AI, so the
/// choice is an enum, not a free string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClefModel {
    /// `@cf/cloudflare/clef`, the 27B model.
    #[default]
    Clef,
    /// `@cf/cloudflare/clef-flash`, the 9B model, for latency-bound
    /// decisions.
    ClefFlash,
}

impl ClefModel {
    /// The spelling the config and the body's `model` field use:
    /// `clef` or `clef-flash`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Clef => "clef",
            Self::ClefFlash => "clef-flash",
        }
    }

    /// The model for a config spelling: `clef`, `clef-flash`, or the
    /// full Workers AI id `@cf/cloudflare/clef[-flash]`. Case and
    /// surrounding whitespace are forgiven; anything else is `None`,
    /// and the registry skips the entry.
    pub fn parse(spelling: &str) -> Option<Self> {
        match spelling.trim().to_ascii_lowercase().as_str() {
            "clef" | "@cf/cloudflare/clef" => Some(Self::Clef),
            "clef-flash" | "clef_flash" | "@cf/cloudflare/clef-flash" => Some(Self::ClefFlash),
            _ => None,
        }
    }
}

/// The route under the API root for one account and model, the
/// Workers AI REST shape.
pub fn route_path(account_id: &str, model: ClefModel) -> String {
    format!(
        "/accounts/{account_id}/ai/run/@cf/cloudflare/{}",
        model.as_str()
    )
}

/// The request body: the model field the route requires, then the
/// seam's request, whose serialization is the System One wire shape.
pub fn request_body(request: &DecisionRequest, model: ClefModel) -> Value {
    json!({
        "model": model.as_str(),
        "state": request.state,
        "questions": request.questions,
    })
}

/// Unwraps Workers AI's envelope: `result` when `success` is true, the
/// System One response as it stands. A `success` of false, a missing
/// `result`, or a `result` of null — success that names nothing — is
/// the failure the status says it is, with the `errors[].message`
/// lines as the text. `status` is the HTTP status the envelope arrived
/// with, so the mapping is the seam's own.
pub fn unwrap_envelope(body: &str, status: u16) -> Result<DecisionResponse, DecisionError> {
    let value: Value = serde_json::from_str(body)
        .map_err(|err| DecisionError::Parse(format!("not the Workers AI envelope: {err}")))?;
    let success = value.get("success").and_then(Value::as_bool) == Some(true);
    let Some(result) = value
        .get("result")
        .filter(|result| success && !result.is_null())
    else {
        // The failure's reasons are the detail the seam's status
        // mapping reads out of `errors[].message`.
        return Err(DecisionError::from_status(status, body));
    };
    DecisionResponse::from_value(result.clone()).map_err(|err| {
        DecisionError::Parse(format!("the result is not a System One response: {err}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhema_decision::Question;

    #[test]
    fn the_route_names_the_account_and_the_model() {
        assert_eq!(
            route_path("acc123", ClefModel::Clef),
            "/accounts/acc123/ai/run/@cf/cloudflare/clef"
        );
        assert_eq!(
            route_path("acc123", ClefModel::ClefFlash),
            "/accounts/acc123/ai/run/@cf/cloudflare/clef-flash"
        );
    }

    #[test]
    fn the_model_spellings_parse_and_the_default_is_clef() {
        assert_eq!(ClefModel::parse("clef"), Some(ClefModel::Clef));
        assert_eq!(ClefModel::parse(" Clef-Flash "), Some(ClefModel::ClefFlash));
        assert_eq!(
            ClefModel::parse("@cf/cloudflare/clef-flash"),
            Some(ClefModel::ClefFlash)
        );
        assert_eq!(ClefModel::default(), ClefModel::Clef);
        assert_eq!(ClefModel::parse("jev"), None);
        assert_eq!(ClefModel::ClefFlash.as_str(), "clef-flash");
    }

    #[test]
    fn the_body_repeats_the_model_the_route_took() {
        let request = DecisionRequest::new("s").with_question("q", Question::noul("x?"));
        let body = request_body(&request, ClefModel::ClefFlash);
        assert_eq!(body["model"], json!("clef-flash"));
        assert_eq!(body["questions"]["q"]["type"], json!("noul"));
    }

    #[test]
    fn the_envelope_yields_the_result() {
        let body = r#"{
            "result": {
                "model": "clef",
                "answers": {"urgent": {"type": "noul", "noul": 0.9}},
                "usage": {"input_tokens": 120, "output_tokens": 0}
            },
            "success": true,
            "errors": [],
            "messages": []
        }"#;
        let response = unwrap_envelope(body, 200).unwrap();
        assert_eq!(response.model, "clef");
        assert_eq!(response.usage.input_tokens, 120);
    }

    #[test]
    fn a_failed_envelope_is_its_status_with_its_messages() {
        let body = r#"{"result": null, "success": false,
                        "errors": [{"code": 1002, "message": "bad token"}], "messages": []}"#;
        assert_eq!(unwrap_envelope(body, 401).unwrap_err(), DecisionError::Auth);
        let body = r#"{"success": false, "errors": [{"code": 1001, "message": "questions: at least 1 item required"}]}"#;
        assert_eq!(
            unwrap_envelope(body, 400).unwrap_err(),
            DecisionError::Invalid("questions: at least 1 item required".into())
        );
        let body = r#"{"success": false, "errors": [{"code": 5, "message": "busy"}]}"#;
        assert_eq!(
            unwrap_envelope(body, 503).unwrap_err(),
            DecisionError::Http {
                status: 503,
                body: "busy".into()
            }
        );
    }

    #[test]
    fn a_success_envelope_without_a_result_is_a_parse_error() {
        let err = unwrap_envelope(r#"{"success": true}"#, 200).unwrap_err();
        assert!(
            matches!(err, DecisionError::Http { status: 200, .. }),
            "{err:?}"
        );
        // A null result is success that names nothing: the same
        // failure as a missing one, not a later parse of null.
        let err = unwrap_envelope(r#"{"success": true, "result": null}"#, 200).unwrap_err();
        assert!(
            matches!(err, DecisionError::Http { status: 200, .. }),
            "{err:?}"
        );
        let err =
            unwrap_envelope(r#"{"success": true, "result": {"model": "clef"}}"#, 200).unwrap_err();
        assert!(matches!(err, DecisionError::Parse(_)), "{err:?}");
        let err = unwrap_envelope("not json", 200).unwrap_err();
        assert!(matches!(err, DecisionError::Parse(_)), "{err:?}");
    }
}
