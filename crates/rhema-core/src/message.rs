//! Conversation content: roles, content blocks, and messages.
//!
//! A [`Message`] is a role plus an ordered list of [`Content`] blocks, so one
//! assistant turn can mix text and several [`ToolCall`]s. All types are
//! `serde`-friendly: the driver must be able to checkpoint a session
//! containing them (SPEC.md §6).

use serde::{Deserialize, Serialize};

use crate::tool::{ToolCall, ToolResult};

/// Who authored a [`Message`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// System prompt.
    System,
    /// Human turn, or a driver-injected inbox item.
    User,
    /// Model turn.
    Assistant,
    /// Tool result, answering a prior [`ToolCall`].
    Tool,
}

/// One block of a message's content.
///
/// Serialized untagged: `Text` is a bare JSON string, the tool blocks are
/// objects whose field sets distinguish them, and [`Image`] — tried last —
/// is the remaining `{media_type, data}` object shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Content {
    /// Plain text.
    Text(String),
    /// A tool invocation requested by the model.
    ToolCall(ToolCall),
    /// A result answering a prior [`ToolCall`].
    ToolResult(ToolResult),
    /// An inline image, carried as base64.
    Image(Image),
}

/// One turn of the conversation, authored by [`Role`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Author of this turn.
    pub role: Role,
    /// Ordered content blocks; a single turn may mix text and tool blocks.
    pub content: Vec<Content>,
}

impl Message {
    /// A single-text-block message.
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            content: vec![Content::Text(text.into())],
        }
    }

    /// A message whose text is followed by one [`Content::Image`] block per
    /// image; with no images, exactly [`Message::text`].
    pub fn text_with_images(role: Role, text: impl Into<String>, images: Vec<Image>) -> Self {
        let mut content = Vec::with_capacity(images.len() + 1);
        content.push(Content::Text(text.into()));
        content.extend(images.into_iter().map(Content::Image));
        Self { role, content }
    }
}

/// The image formats every target provider API accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageMediaType {
    /// PNG.
    Png,
    /// JPEG.
    Jpeg,
    /// GIF.
    Gif,
    /// WebP.
    Webp,
}

impl ImageMediaType {
    /// The full MIME type, as the provider wire expects.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
        }
    }

    /// Parses a full MIME type, case-insensitive and trimmed; abbreviations
    /// like `jpg` and formats outside the accepted set return `None`.
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "image/png" => Some(Self::Png),
            "image/jpeg" => Some(Self::Jpeg),
            "image/gif" => Some(Self::Gif),
            "image/webp" => Some(Self::Webp),
            _ => None,
        }
    }

    /// Detects the format from the file's magic bytes; `None` for empty,
    /// truncated, or unrecognized input.
    pub fn sniff(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            Some(Self::Png)
        } else if bytes.starts_with(b"\xff\xd8\xff") {
            Some(Self::Jpeg)
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            Some(Self::Gif)
        } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
            Some(Self::Webp)
        } else {
            None
        }
    }
}

/// An image carried inline in a message.
///
/// `data` is the base64 encoding of the file bytes: no `data:` prefix, no
/// MIME header, no newlines. Core only carries the encoding — frontends
/// produce it and provider adapters shape it for their wire format, so the
/// block round-trips through session checkpoints byte-for-byte.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Image {
    /// The format of the decoded bytes.
    pub media_type: ImageMediaType,
    /// Base64-encoded file bytes: no `data:` prefix, no newlines.
    pub data: String,
}

impl Image {
    /// An image from its format and base64-encoded data.
    pub fn new(media_type: ImageMediaType, data: impl Into<String>) -> Self {
        Self {
            media_type,
            data: data.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolCall;

    #[test]
    fn message_round_trips_through_serde() {
        let message = Message {
            role: Role::Assistant,
            content: vec![
                Content::Text("running the check".into()),
                Content::ToolCall(ToolCall {
                    id: "call_1".into(),
                    name: "cargo_test".into(),
                    args: serde_json::json!({ "package": "rhema-core" }),

                    provider_data: serde_json::Value::Null,
                }),
            ],
        };
        let json = serde_json::to_string(&message).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(back, message);
    }

    #[test]
    fn image_block_round_trips_through_serde() {
        let message = Message::text_with_images(
            Role::User,
            "what is in this picture?",
            vec![Image::new(ImageMediaType::Png, "aGVsbG8=")],
        );
        let json = serde_json::to_string(&message).unwrap();
        assert!(json.contains("\"media_type\":\"png\""), "{json}");
        assert!(json.contains("aGVsbG8="), "{json}");

        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(back, message);
    }

    #[test]
    fn untagged_content_keeps_the_variants_disjoint() {
        let image: Content =
            serde_json::from_str(r#"{"media_type":"png","data":"aGVsbG8="}"#).unwrap();
        assert_eq!(
            image,
            Content::Image(Image::new(ImageMediaType::Png, "aGVsbG8="))
        );

        let call: Content = serde_json::from_str(
            r#"{"id":"call_1","name":"cargo_test","args":{},"provider_data":null}"#,
        )
        .unwrap();
        assert_eq!(
            call,
            Content::ToolCall(ToolCall {
                id: "call_1".into(),
                name: "cargo_test".into(),
                args: serde_json::json!({}),
                provider_data: serde_json::Value::Null,
            })
        );

        assert!(serde_json::from_str::<Content>(r#"{"foo":1}"#).is_err());
    }

    #[test]
    fn media_type_parse_accepts_only_full_mime_types() {
        assert_eq!(
            ImageMediaType::parse(" image/PNG "),
            Some(ImageMediaType::Png)
        );
        assert_eq!(
            ImageMediaType::parse("Image/JPEG"),
            Some(ImageMediaType::Jpeg)
        );
        assert_eq!(
            ImageMediaType::parse("image/gif"),
            Some(ImageMediaType::Gif)
        );
        assert_eq!(
            ImageMediaType::parse("IMAGE/WEBP"),
            Some(ImageMediaType::Webp)
        );
        assert_eq!(ImageMediaType::parse("jpg"), None);
        assert_eq!(ImageMediaType::parse("image/svg+xml"), None);
        assert_eq!(ImageMediaType::parse(""), None);
    }

    #[test]
    fn media_type_sniff_detects_file_magic() {
        assert_eq!(
            ImageMediaType::sniff(b"\x89PNG\r\n\x1a\n\x00\x00"),
            Some(ImageMediaType::Png)
        );
        assert_eq!(
            ImageMediaType::sniff(b"\xff\xd8\xff\xe0rest"),
            Some(ImageMediaType::Jpeg)
        );
        assert_eq!(
            ImageMediaType::sniff(b"GIF89a...."),
            Some(ImageMediaType::Gif)
        );
        assert_eq!(
            ImageMediaType::sniff(b"GIF87a...."),
            Some(ImageMediaType::Gif)
        );
        assert_eq!(
            ImageMediaType::sniff(b"RIFF\x24\x00\x00\x00WEBPVP8 "),
            Some(ImageMediaType::Webp)
        );
        // Truncated past any magic, and garbage.
        assert_eq!(ImageMediaType::sniff(b"\x89PNG"), None);
        assert_eq!(ImageMediaType::sniff(b"RIFF\x24\x00\x00\x00WEB"), None);
        assert_eq!(ImageMediaType::sniff(b"not an image"), None);
        assert_eq!(ImageMediaType::sniff(b""), None);
    }

    #[test]
    fn text_with_images_lays_out_text_then_images() {
        let images = vec![
            Image::new(ImageMediaType::Png, "first"),
            Image::new(ImageMediaType::Jpeg, "second"),
        ];
        let message = Message::text_with_images(Role::User, "look", images);
        assert_eq!(
            message.content,
            vec![
                Content::Text("look".into()),
                Content::Image(Image::new(ImageMediaType::Png, "first")),
                Content::Image(Image::new(ImageMediaType::Jpeg, "second")),
            ]
        );

        assert_eq!(
            Message::text_with_images(Role::User, "look", Vec::new()),
            Message::text(Role::User, "look")
        );
    }
}
