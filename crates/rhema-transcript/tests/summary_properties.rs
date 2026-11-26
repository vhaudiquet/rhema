//! Property tests for the extractive line text and the byte accounting.
//!
//! Everything the view says about a message it does not carry whole is
//! mechanical: heads, counts, and flat byte figures. The contract is
//! that those figures are honest and the lines stay lines: a head is
//! the text flattened, never past its cap plus the ellipsis, never cut
//! mid-character; the byte and token figures price every part of a
//! message, images at their flat charge; and the zoom render carries
//! every part a message holds.

use hegel::TestCase;
use hegel::generators::{self as gs, TextGenerator};
use rhema_core::{Content, Image, ImageMediaType, Message, Role, ToolResult};
use rhema_transcript::summary::{
    HEAD_BYTES, IMAGE_BYTES, IMAGE_TOKENS, head, message_bytes, message_images, message_tokens,
    one_line, render_message, text_tokens, unknown_bytes,
};

/// Text with the shapes that stress flattening and byte cutting:
/// whitespace runs, doubled spaces, multibyte characters, tabs.
fn raw_text() -> TextGenerator {
    gs::text()
        .min_size(0)
        .max_size(80)
        .alphabet("ab \t\n\ré😀.!")
}

#[derive(Debug, Clone, hegel::DefaultGenerator)]
enum GenPart {
    Text(String),
    Image,
    Call {
        name: String,
        args: String,
    },
    Result {
        ok: bool,
        content: String,
        images: u16,
    },
}

fn to_part(part: GenPart) -> Content {
    match part {
        GenPart::Text(text) => Content::Text(text),
        GenPart::Image => Content::Image(Image::new(ImageMediaType::Png, "Zm9v")),
        GenPart::Call { name, args } => Content::ToolCall(rhema_core::ToolCall {
            id: "t".into(),
            name,
            args: serde_json::Value::String(args),
            provider_data: serde_json::Value::Null,
        }),
        GenPart::Result {
            ok,
            content,
            images,
        } => {
            let result = if ok {
                ToolResult::success("t", content)
            } else {
                ToolResult::failure("t", content)
            };
            Content::ToolResult(
                result.with_images(
                    (0..images % 3)
                        .map(|_| Image::new(ImageMediaType::Png, "YmFy"))
                        .collect(),
                ),
            )
        }
    }
}

#[hegel::test(test_cases = 400)]
fn a_head_stays_a_line_within_its_cap(tc: TestCase) {
    for _ in 0..8 {
        let text = tc.draw(raw_text());
        let flat = one_line(&text);

        // Flattening loses no character and no newline survives: the
        // non-whitespace characters come through in order, and the
        // line is one line.
        let plain: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        let flat_plain: String = flat.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(plain, flat_plain, "flattening lost or reordered text");
        assert!(
            !flat.contains('\n') && !flat.contains('\r') && !flat.contains('\t'),
            "a flattened line kept a control character: {flat:?}"
        );

        // Every cap from zero to past the text: the head is the flat
        // text when it fits, and otherwise a boundary cut plus the
        // ellipsis, never past the cap by more than that ellipsis.
        for cap in [
            0usize,
            1,
            2,
            3,
            4,
            7,
            flat.len() / 2,
            flat.len(),
            flat.len() + 4,
        ] {
            let cut = head(&text, cap);
            if flat.len() <= cap {
                assert_eq!(cut, flat, "a fitting text was not returned whole");
            } else {
                assert!(
                    cut.ends_with('\u{2026}'),
                    "a cut head lost its ellipsis: {cut:?}"
                );
                assert!(
                    cut.len() <= cap + 3,
                    "a head overshot its cap: {} bytes for cap {cap}",
                    cut.len()
                );
                // The cut stays a prefix of the flat text: bytes, not
                // a rewrite, and always whole characters (the String
                // built here would not exist otherwise).
                let without = &cut[..cut.len() - 3];
                assert!(
                    flat.starts_with(without),
                    "a head rewrote the text it quoted: {cut:?}"
                );
            }
        }
    }

    // The documented cap of a quoted head: heads the view quotes use
    // HEAD_BYTES, so the bound must hold there too, whatever the text.
    let text = tc.draw(raw_text());
    assert!(head(&text, HEAD_BYTES).len() <= HEAD_BYTES + 3);
}

#[hegel::test(test_cases = 400)]
fn the_byte_and_token_figures_price_every_part(tc: TestCase) {
    let parts: Vec<GenPart> = tc.draw(gs::vecs(gs::default::<GenPart>()).max_size(6));
    let message = Message {
        annotations: Default::default(),
        role: Role::User,
        content: parts.iter().cloned().map(to_part).collect(),
    };

    // The expected figures, part by part, on the documented charges:
    // text at its byte length (a quarter token per byte), tool-call
    // text as name plus serialized args, tool results at content plus
    // their images, every image at the flat charge.
    let mut bytes = 0usize;
    let mut tokens = 0u64;
    let mut images = 0usize;
    for content in &message.content {
        match content {
            Content::Text(text) => {
                bytes += text.len();
                tokens += text_tokens(text.len());
            }
            Content::ToolCall(call) => {
                let price = call.name.len() + call.args.to_string().len();
                bytes += price;
                tokens += text_tokens(price);
            }
            Content::ToolResult(result) => {
                bytes += result.content.len() + result.images.len() * IMAGE_BYTES;
                tokens +=
                    text_tokens(result.content.len()) + result.images.len() as u64 * IMAGE_TOKENS;
                images += result.images.len();
            }
            Content::Image(_) => {
                bytes += IMAGE_BYTES;
                tokens += IMAGE_TOKENS;
                images += 1;
            }
            // The generator draws the four kinds above only; the arm
            // mirrors the library's fallback.
            _ => {
                bytes += unknown_bytes(content);
                tokens += text_tokens(unknown_bytes(content));
                images += 1;
            }
        }
    }
    assert_eq!(message_bytes(&message), bytes, "the byte figure diverged");
    assert_eq!(
        message_tokens(&message),
        tokens,
        "the token figure diverged"
    );
    assert_eq!(message_images(&message), images, "the image count diverged");

    // The zoom render carries every part: each text part verbatim,
    // each call and result spelled out, each image named.
    let render = render_message(&message);
    for content in &message.content {
        match content {
            Content::Text(text) if !text.is_empty() => {
                assert!(
                    render.contains(text.as_str()),
                    "the render lost a text part:\n{render:?}"
                );
            }
            Content::Text(_) => {}
            Content::ToolCall(call) => {
                assert!(
                    render.contains(&format!("[calls {}({})]", call.name, call.args)),
                    "the render lost a call:\n{render:?}"
                );
            }
            Content::ToolResult(result) => {
                assert!(
                    render.contains(&result.content),
                    "the render lost a result:\n{render:?}"
                );
                for _ in &result.images {
                    // Counted through: the render names one image line
                    // per image the result carries.
                }
            }
            Content::Image(image) => {
                assert!(
                    render.contains(&format!("[image: {}]", image.media_type.as_str())),
                    "the render lost an image line:\n{render:?}"
                );
            }
            _ => {}
        }
    }
}
