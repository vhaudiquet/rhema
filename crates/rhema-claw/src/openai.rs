//! The OpenAI-compatible shim: `POST /v1/chat/completions`, so a stock
//! web UI plugs into the resident agent without knowing the claw's own
//! routes.
//!
//! **The incoming history is ignored — deliberately, and this is the
//! load-bearing caveat of the whole shim.** The daemon owns the session:
//! one key, one persistent conversation, restored and stashed by the
//! interactive handler across every turn. A chat UI over this endpoint
//! MIRRORS the replies (streamed as deltas or returned whole); it does
//! not own history and cannot rewrite it. Sending a full transcript
//! changes nothing but which message triggers the turn: only the LAST
//! user message is read. The session key is `interactive:<user>`,
//! defaulting to `interactive:web`.
//!
//! The wire is the familiar subset: `stream: true` answers SSE
//! chat-completion chunks (`delta.reasoning_content` for thinking,
//! `delta.content` for text, a final chunk with `finish_reason: "stop"`,
//! then `data: [DONE]`); `stream: false` answers one completion object
//! once the turn's end marker arrives.
//!
//! The last user message may carry OpenAI's image parts
//! (`{"type": "image_url", "image_url": {"url": ...}}`). Only inline
//! `data:` URIs ride — the crate carries images inline, so a remote
//! http(s) url is skipped — and the same budget as the messages route
//! applies.

use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use rhema_core::ImageMediaType;
use rhema_rsd::{KeyState, ResourceKey};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;
use tokio_stream::wrappers::ReceiverStream;

use crate::hub::TurnEvent;
use crate::interactive::{KEY_PREFIX, WireImage};

/// How long the non-stream path waits for the turn's end marker. Turns
/// can legitimately run minutes of shell work; a web UI's default
/// timeout is the caller's problem, this one is generous.
pub const TURN_WAIT: Duration = Duration::from_secs(600);

/// The session key when the request names no `user`.
pub const DEFAULT_USER: &str = "web";

/// The request body. `messages` is accepted in the OpenAI shape and
/// reduced to the last user message; everything else is ignored.
#[derive(Debug, Deserialize)]
pub struct ChatRequest {
    /// Echoed back when present; the daemon's configured model
    /// otherwise.
    pub model: Option<String>,
    /// The transcript-shaped input. Only the last `user` entry matters.
    pub messages: Vec<ChatMessage>,
    /// Stream chunks (SSE) or wait for the whole reply.
    #[serde(default)]
    pub stream: bool,
    /// The session name: key `interactive:<user>`, `web` when unset.
    pub user: Option<String>,
}

/// One transcript entry; `content` is a string or an array of parts.
#[derive(Debug, Deserialize)]
pub struct ChatMessage {
    /// `user`, `assistant`, `system`, `tool` — only `user` is read.
    pub role: String,
    /// String content, or OpenAI's array-of-parts.
    pub content: ChatContent,
}

/// OpenAI's two content shapes.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum ChatContent {
    /// A plain string.
    Text(String),
    /// An array of typed parts; only the text parts are read.
    Parts(Vec<ChatPart>),
}

impl ChatContent {
    /// The message's text: the string, or the text parts joined. A part
    /// array with no text parts (images only, say) reads as empty.
    pub fn text(&self) -> String {
        match self {
            ChatContent::Text(text) => text.clone(),
            ChatContent::Parts(parts) => parts
                .iter()
                .filter_map(|part| part.text.as_deref())
                .collect::<Vec<_>>()
                .join(""),
        }
    }

    /// The message's inline images, in part order: every part whose
    /// `image_url` is a `data:` URI with a parseable image mime. A
    /// remote url and an unknown mime are skipped, not errors — the
    /// crate carries images inline only.
    pub fn images(&self) -> Vec<WireImage> {
        match self {
            ChatContent::Text(_) => Vec::new(),
            ChatContent::Parts(parts) => parts.iter().filter_map(|part| part.image()).collect(),
        }
    }
}

/// One content part; unknown parts carry no readable text.
#[derive(Debug, Deserialize)]
pub struct ChatPart {
    /// The part's text, for `{"type": "text"}` parts.
    #[serde(default)]
    pub text: Option<String>,
    /// The part's image reference, for `{"type": "image_url"}` parts.
    #[serde(default)]
    pub image_url: Option<ChatImageUrl>,
}

/// OpenAI's image part payload: `{"url": ...}`.
#[derive(Debug, Deserialize)]
pub struct ChatImageUrl {
    /// A `data:{mime};base64,{body}` URI is the one shape the claw
    /// carries; anything else parses as no image at all.
    pub url: String,
}

impl ChatPart {
    /// The part's inline image, when it is one: an `image_url` part
    /// carrying a `data:{mime};base64,{body}` URI whose mime parses as
    /// an image format. The body rides as bare base64, exactly what
    /// the provider APIs take.
    fn image(&self) -> Option<WireImage> {
        let uri = self.image_url.as_ref()?.url.trim();
        let rest = uri.strip_prefix("data:")?;
        let (meta, body) = rest.split_once(',')?;
        let mime = meta.strip_suffix(";base64")?;
        let media_type = ImageMediaType::parse(mime)?;
        Some(WireImage {
            media_type: media_type.as_str().to_owned(),
            data: body.to_owned(),
        })
    }
}

/// The input one turn runs on, as the shim cut it from the request's
/// last `user` entry.
pub struct UserInput {
    /// The text parts, joined.
    pub text: String,
    /// The inline images, in part order.
    pub images: Vec<WireImage>,
}

/// The last `user` message's input — the only input a turn gets.
pub fn last_user_input(messages: &[ChatMessage]) -> Option<UserInput> {
    messages
        .iter()
        .rev()
        .find(|message| message.role == "user")
        .map(|message| UserInput {
            text: message.content.text(),
            images: message.content.images(),
        })
}

/// The completion/chunk id: `chatcmpl-` plus a process-unique suffix.
fn completion_id() -> String {
    static SHIM_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!(
        "chatcmpl-{}-{:04x}",
        now.as_millis(),
        SHIM_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

/// Seconds since the Unix epoch, the shim's `created`.
fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// The non-stream completion object. `content` is `null` when the turn
/// ended without a reply — OpenAI's shape allows it, and the session
/// records why.
fn completion_body(id: &str, created: u64, model: &str, reply: Option<String>) -> Value {
    json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": reply},
            "finish_reason": "stop",
        }],
    })
}

/// One streaming chunk.
fn chunk_body(id: &str, created: u64, model: &str, delta: Value, finish: Option<&str>) -> Value {
    json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": delta,
            "finish_reason": finish,
        }],
    })
}

/// The shim endpoint. Errors are OpenAI-shaped: `{"error": {"message":
/// ...}}` with a sensible status.
pub async fn chat_completions(
    State(state): State<crate::http::ClawState>,
    Json(request): Json<ChatRequest>,
) -> Response {
    let Some(input) = last_user_input(&request.messages).filter(|i| !i.text.trim().is_empty())
    else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "no usable user message: the shim reads only the last `user` entry",
        );
    };
    // The same image budget the messages route keeps; the message must
    // still carry text, so an images-only request is refused above.
    if let Err(reason) = crate::http::validate_images(&input.images) {
        return error_response(StatusCode::BAD_REQUEST, &reason);
    }
    let user = request
        .user
        .as_deref()
        .map(str::trim)
        .filter(|user| !user.is_empty())
        .unwrap_or(DEFAULT_USER);
    let key = ResourceKey::from(format!("{KEY_PREFIX}{user}"));
    let model = request.model.clone().unwrap_or_else(|| {
        let (_, active_model) = state.backends().active();
        active_model
    });
    let id = completion_id();
    let created = unix_secs();

    // Subscribe first, ingest second, drain third — the same contract
    // the messages route keeps.
    let mut receiver = state.hub().subscribe(&key);
    if let Err(err) = state.dispatcher().ingest(crate::interactive::user_event(
        key.as_str(),
        &input.text,
        &input.images,
    )) {
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string());
    }
    if let Err(err) = crate::http::drain(state.driver(), state.dispatcher(), &key).await {
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string());
    }

    if request.stream {
        return stream_response(receiver, &id, created, &model);
    }

    // The drain ran the turn; its end marker is queued. A queued
    // follow-up behind a foreign turn delays ours — keep waiting through
    // drains until the key settles, bounded by TURN_WAIT.
    let deadline = tokio::time::Instant::now() + TURN_WAIT;
    let reply = loop {
        let done = match tokio::time::timeout_at(deadline, receiver.recv()).await {
            Ok(Ok(TurnEvent::TurnDone { reply, .. })) => reply,
            Ok(Ok(_)) => continue,
            Ok(Err(RecvError::Lagged(_))) => continue,
            Ok(Err(RecvError::Closed)) => {
                return error_response(
                    StatusCode::BAD_GATEWAY,
                    "the event hub closed before the turn finished",
                );
            }
            Err(_elapsed) => {
                return error_response(
                    StatusCode::GATEWAY_TIMEOUT,
                    "the turn did not finish in time; its reply may still arrive on the event stream",
                );
            }
        };
        let settled = state
            .dispatcher()
            .status(&key)
            .map(|status| status.inbox_depth == 0 && status.state != KeyState::Reacquire);
        match settled {
            Ok(true) => break done,
            Ok(false) => {
                // More work is queued behind what just finished — ours,
                // most likely: drain and wait for its end marker.
                if let Err(err) = crate::http::drain(state.driver(), state.dispatcher(), &key).await
                {
                    return error_response(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string());
                }
            }
            Err(err) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
        }
    };
    Json(completion_body(&id, created, &model, reply)).into_response()
}

/// The `stream: true` answer: an SSE body of completion chunks pumped
/// from the hub. Thinking becomes `delta.reasoning_content`, text
/// becomes `delta.content`, and the turn's end marker becomes the final
/// `finish_reason: "stop"` chunk plus `data: [DONE]` — then the stream
/// closes. Tool traffic is not surfaced: it is the agent's plumbing.
fn stream_response(
    mut receiver: tokio::sync::broadcast::Receiver<TurnEvent>,
    id: &str,
    created: u64,
    model: &str,
) -> Response {
    let id = id.to_owned();
    let model = model.to_owned();
    let (sender, body) =
        tokio::sync::mpsc::channel::<Result<SseEvent, std::convert::Infallible>>(64);
    tokio::spawn(async move {
        loop {
            let frame = match receiver.recv().await {
                Ok(TurnEvent::ThinkingDelta { text }) => Ok(sse_chunk(
                    &id,
                    created,
                    &model,
                    json!({"reasoning_content": text}),
                )),
                Ok(TurnEvent::TextDelta { text }) => {
                    Ok(sse_chunk(&id, created, &model, json!({"content": text})))
                }
                Ok(TurnEvent::TurnDone { .. }) => {
                    // The turn is over regardless of who triggered it or
                    // how it ended: the client gets a clean stop.
                    let final_frame = Ok(sse_chunk_finish(
                        &id,
                        created,
                        &model,
                        json!({}),
                        Some("stop"),
                    ));
                    let done = Ok(SseEvent::default().data("[DONE]"));
                    let _ = sender.send(final_frame).await;
                    let _ = sender.send(done).await;
                    break;
                }
                // The claw's own clients get the tool rows, the
                // consent cards, and the attachments; an OpenAI caller
                // gets text, reasoning, and the end marker.
                Ok(
                    TurnEvent::ToolStart { .. }
                    | TurnEvent::ToolResult { .. }
                    | TurnEvent::ConsentRequest { .. }
                    | TurnEvent::ConsentResolved { .. }
                    | TurnEvent::Attachment { .. },
                ) => continue,
                // The turn's input rides the stream for the claw's own
                // clients; an OpenAI caller named it in the request.
                Ok(TurnEvent::UserMessage { .. }) => continue,
                Err(RecvError::Lagged(dropped)) => Ok(SseEvent::default()
                    .comment(format!("lagged; {dropped} earlier event(s) dropped"))),
                Err(RecvError::Closed) => break,
            };
            if sender.send(frame).await.is_err() {
                break;
            }
        }
    });
    Sse::new(ReceiverStream::new(body))
        .keep_alive(
            KeepAlive::default()
                .interval(crate::http::KEEP_ALIVE_INTERVAL)
                .text("keep-alive"),
        )
        .into_response()
}

/// One delta (or the final) chunk as an SSE frame.
fn sse_chunk(id: &str, created: u64, model: &str, delta: Value) -> SseEvent {
    sse_chunk_finish(id, created, model, delta, None)
}

fn sse_chunk_finish(
    id: &str,
    created: u64,
    model: &str,
    delta: Value,
    finish: Option<&str>,
) -> SseEvent {
    SseEvent::default().data(chunk_body(id, created, model, delta, finish).to_string())
}

/// An OpenAI-shaped error: `{"error": {"message": ...}}`.
fn error_response(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"error": {"message": message}}))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(role: &str, content: ChatContent) -> ChatMessage {
        ChatMessage {
            role: role.to_owned(),
            content,
        }
    }

    #[test]
    fn only_the_last_user_message_is_read() {
        let messages = vec![
            message("system", ChatContent::Text("be brief".into())),
            message("user", ChatContent::Text("first".into())),
            message("assistant", ChatContent::Text("no".into())),
            message("user", ChatContent::Text("second".into())),
        ];
        let read = |slice: &[ChatMessage]| {
            last_user_input(slice)
                .filter(|input| !input.text.trim().is_empty())
                .map(|input| input.text)
        };
        assert_eq!(read(&messages).as_deref(), Some("second"));
        assert_eq!(read(&messages[0..1]), None);
        assert_eq!(read(&[]), None);
    }

    #[test]
    fn part_arrays_join_their_text_parts() {
        let parts = ChatContent::Parts(vec![
            ChatPart {
                text: Some("look ".into()),
                image_url: None,
            },
            ChatPart {
                text: None,
                image_url: Some(ChatImageUrl {
                    url: "data:image/png;base64,cG5n".into(),
                }),
            },
            ChatPart {
                text: Some("here".into()),
                image_url: None,
            },
        ]);
        assert_eq!(parts.text(), "look here");
        assert_eq!(ChatContent::Text("plain".into()).text(), "plain");
    }

    /// An `image_url` part rides only as an inline data URI with a mime
    /// the providers take: a remote url, a non-base64 data URI, and an
    /// unknown mime are all skipped, in part order, with no error.
    #[test]
    fn image_parts_ride_only_as_inline_data_uris() {
        let content = ChatContent::Parts(vec![
            ChatPart {
                text: Some("look".into()),
                image_url: None,
            },
            ChatPart {
                text: None,
                image_url: Some(ChatImageUrl {
                    url: "data:image/png;base64,cG5n".into(),
                }),
            },
            ChatPart {
                text: None,
                image_url: Some(ChatImageUrl {
                    url: "https://example.com/cat.png".into(),
                }),
            },
            ChatPart {
                text: None,
                image_url: Some(ChatImageUrl {
                    url: "data:image/svg+xml;base64,PHN2Zy8+".into(),
                }),
            },
            ChatPart {
                text: None,
                image_url: Some(ChatImageUrl {
                    url: "data:image/png,raw-not-base64".into(),
                }),
            },
        ]);
        let images = content.images();
        assert_eq!(images.len(), 1, "{images:?}");
        assert_eq!(images[0].media_type, "image/png");
        assert_eq!(images[0].data, "cG5n");

        // A plain string carries no images, and the mime is canonical
        // even when the URI spells it with different case.
        assert!(ChatContent::Text("plain".into()).images().is_empty());
        let loud = ChatContent::Parts(vec![ChatPart {
            text: None,
            image_url: Some(ChatImageUrl {
                url: "data:IMAGE/PNG;base64,cG5n".into(),
            }),
        }]);
        assert_eq!(loud.images()[0].media_type, "image/png");
    }

    /// Text-only requests are unchanged: no parts, no images.
    #[test]
    fn text_only_requests_carry_no_images() {
        let input = last_user_input(&[message("user", ChatContent::Text("plain".into()))])
            .expect("a user message");
        assert_eq!(input.text, "plain");
        assert!(input.images.is_empty());
    }

    #[test]
    fn the_completion_shape_is_the_familiar_one() {
        let body = completion_body("chatcmpl-1", 42, "m", Some("hi".into()));
        assert_eq!(body["object"], "chat.completion");
        assert_eq!(body["choices"][0]["message"]["content"], "hi");
        assert_eq!(body["choices"][0]["finish_reason"], "stop");
        let null = completion_body("chatcmpl-1", 42, "m", None);
        assert!(null["choices"][0]["message"]["content"].is_null());

        let chunk = chunk_body("chatcmpl-1", 42, "m", json!({"content": "x"}), None);
        assert_eq!(chunk["object"], "chat.completion.chunk");
        assert_eq!(chunk["choices"][0]["delta"]["content"], "x");
        assert!(chunk["choices"][0]["finish_reason"].is_null());
        let stop = chunk_body("chatcmpl-1", 42, "m", json!({}), Some("stop"));
        assert_eq!(stop["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn ids_are_unique_and_shaped() {
        let a = completion_id();
        let b = completion_id();
        assert_ne!(a, b);
        assert!(a.starts_with("chatcmpl-"), "{a}");
    }
}
