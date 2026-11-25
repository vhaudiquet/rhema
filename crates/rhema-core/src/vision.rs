//! Request-time image masking for models that cannot see.
//!
//! A conversation's images live in its [`Session`](crate::Session), not
//! in the model: a model switch only changes what the next request
//! carries. [`ImageMask`] is the provider decorator for a model whose
//! card says it has no vision: every [`Content::Image`] block and every
//! [`crate::tool::ToolResult::images`] entry becomes a short text note naming its
//! format, so no base64 reaches the wire. The caller's session is
//! untouched, and rebuilding the agent over a vision model sends the
//! same images the log kept.

use std::sync::Arc;

use async_trait::async_trait;

use crate::message::{Content, ImageMediaType, Message};
use crate::{EventSink, ModelOutput, ModelProvider, ModelRequest, ProviderError};

/// What one dropped image leaves in its place. The model still learns
/// that something was there, and which format, so it can say it cannot
/// see the image rather than answer as if none had been sent.
fn masked_note(media_type: ImageMediaType) -> String {
    format!(
        "[image omitted: {}; the active model has no vision]",
        media_type.as_str()
    )
}

/// Replaces every image a request carries with a text note.
///
/// Image blocks become text blocks in place, and a tool result's
/// [`images`](crate::tool::ToolResult::images) move into its own `content` before
/// the vector is cleared. Order is preserved, and a message that held
/// only an image still carries the note. The caller's session is not
/// touched: this works on the request's own copy of the history.
pub fn mask_images(messages: &mut [Message]) {
    for message in messages {
        let mut content = Vec::with_capacity(message.content.len());
        for block in std::mem::take(&mut message.content) {
            match block {
                Content::Image(image) => {
                    content.push(Content::Text(masked_note(image.media_type)));
                }
                Content::ToolResult(mut result) => {
                    for image in std::mem::take(&mut result.images) {
                        if !result.content.is_empty() && !result.content.ends_with('\n') {
                            result.content.push('\n');
                        }
                        result.content.push_str(&masked_note(image.media_type));
                    }
                    content.push(Content::ToolResult(result));
                }
                other => content.push(other),
            }
        }
        message.content = content;
    }
}

/// The provider decorator behind [`mask_images`]: a model with no
/// vision never receives pixels.
pub struct ImageMask {
    inner: Arc<dyn ModelProvider>,
}

impl ImageMask {
    /// Wraps `inner` when the model's card says it has no vision.
    ///
    /// `Some(true)` and the unknown `None` return `inner` untouched:
    /// unknown is not "no", and a provider that may accept images must
    /// not have them silently hidden from it.
    pub fn wrap(
        inner: Arc<dyn ModelProvider>,
        supports_vision: Option<bool>,
    ) -> Arc<dyn ModelProvider> {
        match supports_vision {
            Some(false) => Arc::new(Self { inner }),
            _ => inner,
        }
    }
}

impl std::fmt::Debug for ImageMask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The inner provider is a trait object and stays opaque.
        f.debug_struct("ImageMask").finish_non_exhaustive()
    }
}

#[async_trait]
impl ModelProvider for ImageMask {
    async fn complete(
        &self,
        mut req: ModelRequest,
        sink: &mut dyn EventSink,
    ) -> Result<ModelOutput, ProviderError> {
        mask_images(&mut req.messages);
        self.inner.complete(req, sink).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Image, ToolCall, ToolResult};

    /// A provider with no interesting answer: the identity test only
    /// asks whether the decorator was installed, never runs a turn.
    struct Opaque;

    #[async_trait]
    impl ModelProvider for Opaque {
        async fn complete(
            &self,
            _req: ModelRequest,
            _sink: &mut dyn EventSink,
        ) -> Result<ModelOutput, ProviderError> {
            Err(ProviderError::Network("never driven".to_owned()))
        }
    }

    fn note(media_type: ImageMediaType) -> Content {
        Content::Text(masked_note(media_type))
    }

    #[test]
    fn image_blocks_become_notes_and_their_text_stays() {
        let mut message = Message::text_with_images(
            crate::Role::User,
            "what is this?",
            vec![
                Image::new(ImageMediaType::Png, "aGVsbG8="),
                Image::new(ImageMediaType::Jpeg, "d29ybGQ="),
            ],
        );
        mask_images(std::slice::from_mut(&mut message));
        assert_eq!(
            message.content,
            vec![
                Content::Text("what is this?".into()),
                note(ImageMediaType::Png),
                note(ImageMediaType::Jpeg),
            ]
        );
    }

    #[test]
    fn a_tool_results_images_move_into_its_text() {
        let mut message = Message {
            annotations: Default::default(),
            role: crate::Role::Tool,
            content: vec![
                Content::ToolResult(
                    ToolResult::success("c1", "two frames")
                        .with_images(vec![Image::new(ImageMediaType::Gif, "R0lGODlh")]),
                ),
                Content::Text("beside it".into()),
            ],
        };
        mask_images(std::slice::from_mut(&mut message));
        assert_eq!(
            message.content,
            vec![
                Content::ToolResult(ToolResult::success(
                    "c1",
                    format!("two frames\n{}", masked_note(ImageMediaType::Gif))
                )),
                Content::Text("beside it".into()),
            ]
        );
    }

    #[test]
    fn a_result_with_no_text_gets_a_bare_note() {
        let mut message = Message {
            annotations: Default::default(),
            role: crate::Role::Tool,
            content: vec![Content::ToolResult(
                ToolResult::success("c1", "")
                    .with_images(vec![Image::new(ImageMediaType::Webp, "UklGRg==")]),
            )],
        };
        mask_images(std::slice::from_mut(&mut message));
        assert_eq!(
            message.content,
            vec![Content::ToolResult(ToolResult::success(
                "c1",
                masked_note(ImageMediaType::Webp)
            ))]
        );
    }

    #[test]
    fn text_calls_and_other_blocks_pass_through() {
        let mut message = Message {
            annotations: Default::default(),
            role: crate::Role::Assistant,
            content: vec![
                Content::Text("thinking out loud".into()),
                Content::ToolCall(ToolCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    args: serde_json::json!({"command": "ls"}),
                    ..ToolCall::default()
                }),
            ],
        };
        let before = message.content.clone();
        mask_images(std::slice::from_mut(&mut message));
        assert_eq!(message.content, before);
    }

    #[test]
    fn only_a_confirmed_no_installs_the_decorator() {
        let inner: Arc<dyn ModelProvider> = Arc::new(Opaque);
        assert!(Arc::ptr_eq(
            &ImageMask::wrap(inner.clone(), Some(true)),
            &inner
        ));
        assert!(Arc::ptr_eq(&ImageMask::wrap(inner.clone(), None), &inner));
        assert!(!Arc::ptr_eq(
            &ImageMask::wrap(inner.clone(), Some(false)),
            &inner
        ));
    }
}
