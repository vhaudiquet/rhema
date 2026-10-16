//! The agent's presentation identity: the name, mark, tagline, and
//! images the interface renders everywhere — the rail, the chat, the
//! window title, the notifications' author. Stored claw-side so every
//! client reads the same identity over `GET /v1/agent`.
//!
//! Two kinds of facts live here. The text fields are keys in the
//! settings store; the images are files next to the daemon's database
//! with their content type in the store, because the bearer-guarded
//! route serves them back byte for byte. Behaviour — how the agent
//! talks, what it is like — is deliberately not here: that is SOUL.md,
//! the bootstrap store's domain, edited with the agent over onboarding.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rhema_autonomous_agent::{Tool, ToolRegistry, async_trait};
use rhema_core::ToolSpec;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::settings::SettingsStore;

/// Why an identity operation failed.
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    /// The settings store or the image directory failed.
    #[error("identity store failed: {0}")]
    Backend(String),
    /// An image exceeded [`MAX_IMAGE_BYTES`].
    #[error("image too large: {0} bytes, the limit is {1}")]
    TooLarge(usize, usize),
    /// An upload carried a non-image content type.
    #[error("not an image: {0:?}")]
    NotAnImage(String),
}

/// The cap on an uploaded avatar or logo: enough for a crisp square at
/// every size the UI renders, small enough that the route stays cheap.
pub const MAX_IMAGE_BYTES: usize = 2 * 1024 * 1024;

/// The agent's identity as the wire sees it: the optional text fields
/// (each `null` when unset) and whether the images exist — the images
/// themselves live behind their own routes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentIdentity {
    /// The agent's display name: message author, window title,
    /// notifications.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The glyph or emoji in the rail, the favicon, the empty states.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mark: Option<String>,
    /// Used when there is no mark and no avatar.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initials: Option<String>,
    /// One line under the name on the agent pane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tagline: Option<String>,
    /// Appended to messages the agent sends outside the interface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signoff: Option<String>,
    /// Whether an avatar image is set (served at `/v1/agent/avatar`).
    pub avatar: bool,
    /// Whether a logo image is set (served at `/v1/agent/logo`).
    pub logo: bool,
}

/// The five text fields, for a whole-identity save.
#[derive(Debug, Clone, Default)]
pub struct IdentityTexts {
    pub name: Option<String>,
    pub mark: Option<String>,
    pub initials: Option<String>,
    pub tagline: Option<String>,
    pub signoff: Option<String>,
}

/// One of the two identity images.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    /// The square face the chat and rail render.
    Avatar,
    /// The mark the empty states and headers render.
    Logo,
}

impl ImageKind {
    /// The wire path segment and file stem.
    pub fn name(self) -> &'static str {
        match self {
            ImageKind::Avatar => "avatar",
            ImageKind::Logo => "logo",
        }
    }
}

/// The text field's settings key.
fn text_key(field: &str) -> String {
    format!("identity.{field}")
}

/// The identity store: the text fields in the settings table, the
/// images in `dir` with their content types alongside in the store.
pub struct IdentityStore {
    settings: Arc<SettingsStore>,
    dir: PathBuf,
}

impl IdentityStore {
    /// Opens the store over `settings`, keeping images in `dir`
    /// (created on first write).
    pub fn new(settings: Arc<SettingsStore>, dir: impl Into<PathBuf>) -> Self {
        Self {
            settings,
            dir: dir.into(),
        }
    }

    /// The current identity; unset fields read as `None`.
    pub fn get(&self) -> Result<AgentIdentity, IdentityError> {
        Ok(AgentIdentity {
            name: self.text("name")?,
            mark: self.text("mark")?,
            initials: self.text("initials")?,
            tagline: self.text("tagline")?,
            signoff: self.text("signoff")?,
            avatar: self.image_meta(ImageKind::Avatar)?.is_some(),
            logo: self.image_meta(ImageKind::Logo)?.is_some(),
        })
    }

    /// Saves the text fields whole: a `None` clears the field, a value
    /// is trimmed and length-checked (the UI's inputs cap them, the
    /// store enforces it).
    pub fn set_texts(&self, texts: &IdentityTexts) -> Result<AgentIdentity, IdentityError> {
        let fields = [
            ("name", &texts.name, 64usize),
            ("mark", &texts.mark, 8),
            ("initials", &texts.initials, 4),
            ("tagline", &texts.tagline, 120),
            ("signoff", &texts.signoff, 120),
        ];
        for (field, value, cap) in fields {
            let cleaned = value.as_deref().map(str::trim).filter(|v| !v.is_empty());
            if let Some(value) = cleaned {
                if value.chars().count() > cap {
                    return Err(IdentityError::Backend(format!(
                        "{field} is over {cap} characters"
                    )));
                }
            }
            self.settings
                .set(
                    &text_key(field),
                    &serde_json::to_string(&cleaned).expect("an option serializes"),
                )
                .map_err(|err| IdentityError::Backend(err.to_string()))?;
        }
        self.get()
    }

    /// Stores an image: the bytes in `dir`, the content type in the
    /// settings store. Content types outside `image/` are refused, as
    /// are bodies over [`MAX_IMAGE_BYTES`].
    pub fn set_image(
        &self,
        kind: ImageKind,
        bytes: &[u8],
        content_type: &str,
    ) -> Result<AgentIdentity, IdentityError> {
        let content_type = content_type
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if !content_type.starts_with("image/") || content_type == "image/svg+xml" {
            // SVG is refused deliberately: it is the one image format
            // that can carry scripts, and the route serves these bytes
            // back to a browser.
            return Err(IdentityError::NotAnImage(content_type));
        }
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err(IdentityError::TooLarge(bytes.len(), MAX_IMAGE_BYTES));
        }
        std::fs::create_dir_all(&self.dir)
            .map_err(|err| IdentityError::Backend(err.to_string()))?;
        let file = self.image_file(kind);
        std::fs::write(&file, bytes).map_err(|err| IdentityError::Backend(err.to_string()))?;
        self.settings
            .set(&format!("identity.{}.type", kind.name()), &content_type)
            .map_err(|err| IdentityError::Backend(err.to_string()))?;
        self.get()
    }

    /// The image bytes and their content type, when one is set.
    pub fn image(&self, kind: ImageKind) -> Result<Option<(Vec<u8>, String)>, IdentityError> {
        self.image_meta(kind)?
            .map(|content_type| {
                std::fs::read(self.image_file(kind))
                    .map(|bytes| (bytes, content_type))
                    .map_err(|err| IdentityError::Backend(err.to_string()))
            })
            .transpose()
    }

    /// Drops the image and its content type; a no-op when unset.
    pub fn clear_image(&self, kind: ImageKind) -> Result<AgentIdentity, IdentityError> {
        match std::fs::remove_file(self.image_file(kind)) {
            Ok(()) => {}
            // Absent is the cleared state, not a failure.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(IdentityError::Backend(err.to_string())),
        }
        self.settings
            .delete(&format!("identity.{}.type", kind.name()))
            .map_err(|err| IdentityError::Backend(err.to_string()))?;
        self.get()
    }

    /// The stored content type, `None` when no image is set.
    fn image_meta(&self, kind: ImageKind) -> Result<Option<String>, IdentityError> {
        self.settings
            .get(&format!("identity.{}.type", kind.name()))
            .map_err(|err| IdentityError::Backend(err.to_string()))
    }

    fn image_file(&self, kind: ImageKind) -> PathBuf {
        self.dir.join(format!("{}.bin", kind.name()))
    }

    fn text(&self, field: &str) -> Result<Option<String>, IdentityError> {
        Ok(self
            .settings
            .get(&text_key(field))
            .map_err(|err| IdentityError::Backend(err.to_string()))?
            .and_then(|json| serde_json::from_str::<Option<String>>(&json).ok())
            .flatten())
    }
}

/// The tool's name on the wire.
pub const IDENTITY_SET: &str = "identity_set";

/// `identity_set`: the model's surface over its own presentation —
/// the same store the settings pages write, so what it sets here is
/// what every client renders. Personality stays out of reach: that is
/// SOUL.md, and the tool has no path to it.
struct IdentitySet {
    store: Arc<IdentityStore>,
    workspace: PathBuf,
}

#[async_trait]
impl Tool for IdentitySet {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            IDENTITY_SET,
            "Set your own presentation identity: the name, mark, \
             initials, tagline, and signoff the interfaces render, and \
             your avatar or logo images. This is how you look, not who \
             you are — personality lives in SOUL.md. Leave a field out \
             to keep what is set; an empty string clears it. The avatar \
             and logo take a workspace-relative path to an image file \
             (png, jpeg, gif, or webp, under 2 MiB — svg is refused).",
            json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string",
                        "description": "Display name: message author, \
                            window title, notifications. Up to 64 chars." },
                    "mark": { "type": "string",
                        "description": "Glyph or emoji for the rail and \
                            favicon. Up to 8 chars." },
                    "initials": { "type": "string",
                        "description": "Shown when there is no mark and \
                            no avatar. Up to 4 chars." },
                    "tagline": { "type": "string",
                        "description": "One line under the name. Up to \
                            120 chars." },
                    "signoff": { "type": "string",
                        "description": "Appended to messages sent \
                            outside the interface. Up to 120 chars." },
                    "avatar": { "type": "string",
                        "description": "Workspace-relative path to the \
                            avatar image." },
                    "logo": { "type": "string",
                        "description": "Workspace-relative path to the \
                            logo image." },
                    "clear": { "type": "array",
                        "items": { "type": "string",
                            "enum": ["avatar", "logo"] },
                        "description": "Images to remove." }
                },
            }),
        )
    }

    async fn invoke(&self, args: Value) -> Result<String, String> {
        if !args.is_object() {
            return Err("bad arguments: expected an object".to_owned());
        }
        // The texts save whole, so an argument the call omits must carry
        // the stored value forward — the same absent-means-keep rule the
        // HTTP route applies. An empty string means clear.
        let current = self.store.get().map_err(|err| err.to_string())?;
        let field = |arg: Option<&str>, stored: Option<&str>| {
            arg.map(str::to_owned).or_else(|| stored.map(str::to_owned))
        };
        let texts = IdentityTexts {
            name: field(args["name"].as_str(), current.name.as_deref()),
            mark: field(args["mark"].as_str(), current.mark.as_deref()),
            initials: field(args["initials"].as_str(), current.initials.as_deref()),
            tagline: field(args["tagline"].as_str(), current.tagline.as_deref()),
            signoff: field(args["signoff"].as_str(), current.signoff.as_deref()),
        };
        // Every image is read and validated before anything is written,
        // so a bad path fails the call without half a new identity.
        let mut images = Vec::new();
        for (field, kind) in [("avatar", ImageKind::Avatar), ("logo", ImageKind::Logo)] {
            if let Some(path) = args[field].as_str() {
                let file = workspace_image(&self.workspace, path)?;
                let bytes =
                    std::fs::read(&file).map_err(|err| format!("cannot read {path:?}: {err}"))?;
                images.push((kind, bytes, crate::attachments::content_type(path)));
            }
        }
        let mut clears = Vec::new();
        if let Some(list) = args["clear"].as_array() {
            for name in list {
                let name = name.as_str().ok_or("clear must name avatar or logo")?;
                clears.push(match name {
                    "avatar" => ImageKind::Avatar,
                    "logo" => ImageKind::Logo,
                    other => return Err(format!("unknown image {other:?}")),
                });
            }
        }

        let mut identity = self
            .store
            .set_texts(&texts)
            .map_err(|err| err.to_string())?;
        for (kind, bytes, content_type) in images {
            identity = self
                .store
                .set_image(kind, &bytes, content_type)
                .map_err(|err| err.to_string())?;
        }
        for kind in clears {
            identity = self
                .store
                .clear_image(kind)
                .map_err(|err| err.to_string())?;
        }
        Ok(json!({ "saved": true, "identity": identity }).to_string())
    }
}

/// Resolves a workspace-relative image path — absolute paths and `..`
/// escapes are refused, the same discipline the attachment tool keeps.
fn workspace_image(workspace: &Path, path: &str) -> Result<PathBuf, String> {
    let relative = Path::new(path);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| component == std::path::Component::ParentDir)
    {
        return Err(format!(
            "path must be relative to the workspace, without '..': {path:?}"
        ));
    }
    Ok(workspace.join(relative))
}

/// The identity tool set over `store`: `identity_set`.
pub fn registry(store: Arc<IdentityStore>, workspace: impl Into<PathBuf>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(IdentitySet {
        store,
        workspace: workspace.into(),
    }));
    registry
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The store over a tempdir, with the guard returned so the
    /// directory outlives the calls under test.
    fn store() -> (IdentityStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let settings = Arc::new(SettingsStore::in_memory().expect("the store opens"));
        (
            IdentityStore::new(settings, dir.path().join("identity")),
            dir,
        )
    }

    #[test]
    fn text_fields_round_trip_and_clear() {
        let (store, _dir) = store();
        let saved = store
            .set_texts(&IdentityTexts {
                name: Some("  Kosmos  ".to_owned()),
                mark: Some("✦".to_owned()),
                initials: None,
                tagline: Some("order".to_owned()),
                signoff: Some(String::new()),
            })
            .expect("the texts save");
        assert_eq!(saved.name.as_deref(), Some("Kosmos"), "trimmed");
        assert_eq!(saved.mark.as_deref(), Some("✦"));
        assert_eq!(saved.initials, None);
        assert_eq!(saved.signoff, None, "an empty string clears");
        assert!(!saved.avatar);

        let cleared = store
            .set_texts(&IdentityTexts::default())
            .expect("the clear saves");
        assert_eq!(cleared.name, None);
    }

    #[test]
    fn an_overlong_field_is_refused() {
        let (store, _dir) = store();
        let long = "x".repeat(65);
        let err = store
            .set_texts(&IdentityTexts {
                name: Some(long),
                ..IdentityTexts::default()
            })
            .expect_err("over the cap");
        assert!(err.to_string().contains("over 64"));
    }

    #[test]
    fn images_round_trip_and_clear() {
        let (store, _dir) = store();
        let saved = store
            .set_image(ImageKind::Avatar, &[1, 2, 3], "image/png; charset=binary")
            .expect("the avatar saves");
        assert!(saved.avatar);
        let (bytes, content_type) = store
            .image(ImageKind::Avatar)
            .expect("the image reads")
            .expect("an image is set");
        assert_eq!(bytes, vec![1, 2, 3]);
        assert_eq!(content_type, "image/png", "parameters stripped");

        let cleared = store
            .clear_image(ImageKind::Avatar)
            .expect("the clear saves");
        assert!(!cleared.avatar);
        assert!(store.image(ImageKind::Avatar).expect("reads").is_none());
        // Clearing an unset image is a no-op, not an error.
        store
            .clear_image(ImageKind::Avatar)
            .expect("the second clear is a no-op");
    }

    #[test]
    fn non_images_and_oversized_bodies_are_refused() {
        let (store, _dir) = store();
        assert!(
            store
                .set_image(ImageKind::Logo, b"<svg/>", "image/svg+xml")
                .is_err(),
            "svg carries scripts"
        );
        assert!(
            store
                .set_image(ImageKind::Logo, b"plain", "text/plain")
                .is_err()
        );
        let big = vec![0u8; MAX_IMAGE_BYTES + 1];
        assert!(store.set_image(ImageKind::Logo, &big, "image/png").is_err());
    }

    /// The tool, over a real workspace file: the texts save, the image
    /// lands with its extension's content type, and the answer carries
    /// the resulting identity.
    #[tokio::test]
    async fn identity_set_saves_texts_and_a_workspace_avatar() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = dir.path().join("interactive");
        std::fs::create_dir_all(&workspace).expect("workspace");
        std::fs::write(workspace.join("face.png"), b"png-bytes").expect("the image writes");
        let settings = Arc::new(SettingsStore::in_memory().expect("the store opens"));
        let store = Arc::new(IdentityStore::new(settings, dir.path().join("identity")));
        let tool = IdentitySet {
            store: store.clone(),
            workspace: workspace.clone(),
        };
        let answer = tool
            .invoke(json!({
                "name": "Kosmos",
                "mark": "✦",
                "avatar": "face.png",
            }))
            .await
            .expect("in-band ok");
        let answer: Value = serde_json::from_str(&answer).expect("parses");
        assert_eq!(answer["saved"], true);
        assert_eq!(answer["identity"]["name"], "Kosmos");
        assert_eq!(answer["identity"]["avatar"], true);
        let (bytes, content_type) = store
            .image(ImageKind::Avatar)
            .expect("reads")
            .expect("the avatar is set");
        assert_eq!(bytes, b"png-bytes");
        assert_eq!(content_type, "image/png");
        // An absent field carried the stored value forward.
        tool.invoke(json!({ "tagline": "order" }))
            .await
            .expect("saves");
        let identity = store.get().expect("reads");
        assert_eq!(identity.name.as_deref(), Some("Kosmos"), "kept");
        assert_eq!(identity.tagline.as_deref(), Some("order"), "added");
        assert!(identity.avatar, "untouched by a text-only save");
    }

    #[tokio::test]
    async fn identity_set_clears_fields_and_images_and_refuses_escapes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = dir.path().join("interactive");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let settings = Arc::new(SettingsStore::in_memory().expect("the store opens"));
        let store = Arc::new(IdentityStore::new(settings, dir.path().join("identity")));
        store
            .set_image(ImageKind::Logo, b"bytes", "image/png")
            .expect("the logo saves");
        let tool = IdentitySet {
            store: store.clone(),
            workspace: workspace.clone(),
        };
        let answer = tool
            .invoke(json!({
                "name": "",
                "clear": ["logo"],
            }))
            .await
            .expect("in-band ok");
        let answer: Value = serde_json::from_str(&answer).expect("parses");
        assert_eq!(
            answer["identity"]["name"],
            Value::Null,
            "an empty string clears"
        );
        assert_eq!(answer["identity"]["logo"], false);

        for bad in [
            json!({ "avatar": "../outside.png" }),
            json!({ "avatar": "/etc/hostname" }),
            json!({ "avatar": "missing.png" }),
            json!({ "clear": ["soul"] }),
            json!({ "name": "x".repeat(65) }),
        ] {
            assert!(
                tool.invoke(bad.clone()).await.is_err(),
                "refused in band: {bad}"
            );
        }
    }
}
