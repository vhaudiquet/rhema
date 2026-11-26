//! The request's body and the response's serde shape, with the pure
//! mapping onto the seam's results. Always compiled: a host without
//! sockets maps its own HTTP through [`results_from_response`].

use rhema_websearch::SearchResult;
use serde::{Deserialize, Serialize};

/// The provider id the Console gateway expects in every request. The
/// route is Console's hosted search, and one provider is offered
/// under that name.
pub const PROVIDER_ID: &str = "opencode";

/// The request body: the query and the hosted provider's id. There is
/// no count field and no news index; the gateway returns up to eight
/// general-web results, and the adapter trims to the caller's count.
#[derive(Debug, Clone, Serialize)]
pub struct OpenCodeSearchRequest<'a> {
    /// The query text, verbatim.
    pub query: &'a str,
    /// The hosted provider's id, [`PROVIDER_ID`].
    #[serde(rename = "providerID")]
    pub provider_id: &'a str,
}

/// The response the gateway returns.
#[derive(Debug, Clone, Deserialize)]
pub struct OpenCodeSearchResponse {
    /// The provider that answered, expected to be [`PROVIDER_ID`].
    #[serde(rename = "providerID", default)]
    pub provider_id: String,
    /// The hits, in the gateway's ranking order. Absent on a 200:
    /// zero results, an honest answer.
    #[serde(default)]
    pub results: Vec<OpenCodeSearchResult>,
}

/// One hit, as the gateway reports it. Every field is `Option`: the
/// mapping drops a hit missing the url it cannot do without.
#[derive(Debug, Clone, Deserialize)]
pub struct OpenCodeSearchResult {
    /// The page's absolute url.
    pub url: Option<String>,
    /// The page's title.
    pub title: Option<String>,
    /// The page text the gateway carries as the snippet.
    pub content: Option<String>,
    /// The publication time, when the gateway reports one.
    #[serde(default)]
    pub time: Option<OpenCodeSearchTime>,
}

/// A hit's time block. Only `published` is read; it never reaches
/// [`SearchResult`], which has no date field.
#[derive(Debug, Clone, Deserialize)]
pub struct OpenCodeSearchTime {
    /// Publication time in milliseconds since the Unix epoch.
    #[serde(default)]
    pub published: Option<f64>,
}

/// Maps a response onto the seam's results, in gateway order, hits
/// missing a url dropped, missing titles filled with the url, content
/// carried as the snippet verbatim (the renderer trims).
pub fn results_from_response(response: &OpenCodeSearchResponse) -> Vec<SearchResult> {
    response
        .results
        .iter()
        .filter_map(|result| {
            let url = result.url.as_deref()?.to_owned();
            let title = result.title.clone().unwrap_or_else(|| url.clone());
            let snippet = result.content.clone().unwrap_or_default();
            Some(SearchResult::new(title, url, snippet))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_request_body_names_the_hosted_provider() {
        let body = OpenCodeSearchRequest {
            query: "rust async book",
            provider_id: PROVIDER_ID,
        };
        assert_eq!(
            serde_json::to_value(&body).unwrap(),
            serde_json::json!({"query": "rust async book", "providerID": "opencode"})
        );
    }

    #[test]
    fn the_results_map_in_gateway_order() {
        let response: OpenCodeSearchResponse = serde_json::from_str(
            r#"{
                "providerID": "opencode",
                "results": [
                    {"url": "https://a", "title": "First", "content": "one",
                     "time": {"published": 1760000000000}},
                    {"url": "https://b"}
                ]
            }"#,
        )
        .unwrap();
        let results = results_from_response(&response);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "First");
        assert_eq!(results[0].snippet, "one");
        assert_eq!(results[1].title, "https://b");
        assert_eq!(results[1].snippet, "");
    }

    #[test]
    fn a_missing_results_array_is_zero_results() {
        let response: OpenCodeSearchResponse =
            serde_json::from_str(r#"{"providerID": "opencode"}"#).unwrap();
        assert!(results_from_response(&response).is_empty());
    }

    #[test]
    fn a_hit_without_a_url_drops() {
        let response: OpenCodeSearchResponse =
            serde_json::from_str(r#"{"results": [{"title": "no url"}, {"url": "https://kept"}]}"#)
                .unwrap();
        let results = results_from_response(&response);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "https://kept");
    }
}
