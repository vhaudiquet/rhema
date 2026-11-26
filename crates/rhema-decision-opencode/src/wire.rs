//! The request's shaping and the response's parsing. Always compiled:
//! a host without sockets maps its own HTTP through these.

use serde_json::{Value, json};

use rhema_decision::{DecisionError, DecisionRequest, DecisionResponse};

/// The model the provider asks for when the instance names none:
/// Go's default decision model, the one the gateway documents.
pub const DEFAULT_MODEL: &str = "jev-1.14";

/// The request body: the seam's request plus the model field, the
/// System One shape the gateway documents. The seam's own
/// serialization is that shape, so nothing is translated, only
/// prefixed.
pub fn request_body(request: &DecisionRequest, model: &str) -> Value {
    json!({
        "model": model,
        "state": request.state,
        "questions": request.questions,
    })
}

/// Parses a response body. The gateway answers the System One shape,
/// so the mapping is a parse; anything else is a `Parse` error.
pub fn response_from_json(body: &str) -> Result<DecisionResponse, DecisionError> {
    let value: Value =
        serde_json::from_str(body).map_err(|err| DecisionError::Parse(err.to_string()))?;
    DecisionResponse::from_value(value).map_err(|err| DecisionError::Parse(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhema_decision::Question;
    use serde_json::json;

    #[test]
    fn the_body_carries_the_model_then_the_seams_request() {
        let request = DecisionRequest::new("charged twice").with_question(
            "team",
            Question::choice("Which team", [("billing", json!("Payments"))]),
        );
        let body = request_body(&request, "gemini-decision-1");
        assert_eq!(
            body,
            json!({
                "model": "gemini-decision-1",
                "state": "charged twice",
                "questions": {"team": {
                    "type": "choice",
                    "instructions": "Which team",
                    "criteria": {"billing": "Payments"}
                }}
            })
        );
    }

    #[test]
    fn the_documented_response_parses() {
        let body = r#"{
            "model": "jev-1.14",
            "answers": {"q": {"type": "noul", "noul": 0.9}},
            "usage": {"input_tokens": 12, "output_tokens": 0}
        }"#;
        let response = response_from_json(body).unwrap();
        assert_eq!(response.model, "jev-1.14");
        assert_eq!(response.answers.len(), 1);
        assert_eq!(response.usage.input_tokens, 12);
    }

    #[test]
    fn a_body_that_is_not_the_shape_is_a_parse_error() {
        assert!(matches!(
            response_from_json("<html>nope</html>"),
            Err(DecisionError::Parse(_))
        ));
        assert!(matches!(
            response_from_json(r#"{"model": "jev-1.14"}"#),
            Err(DecisionError::Parse(_))
        ));
    }
}
