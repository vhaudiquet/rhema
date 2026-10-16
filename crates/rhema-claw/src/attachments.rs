//! The attachment store and the `attach_file` tool: how a turn hands
//! the human a file from its workspace — a screenshot it just took, a
//! diff, a log the transcript's snippets cut.
//!
//! A stored attachment is a copy, not a reference: the bytes move into
//! the attachments directory beside the database under an id, so the
//! turn's later work (or a workspace cleanup) cannot break a link the
//! transcript already served. The transcript carries the metadata —
//! name, size, media kind, caption — and a client fetches bytes
//! through `GET /v1/attachments/{id}`. A size cap keeps one tool call
//! from turning the store into a workspace mirror; a file past the cap
//! is an in-band error, and the model can cut it down first.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rhema_autonomous_agent::{Tool, ToolRegistry, async_trait};
use rhema_core::ToolSpec;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::hub::{EventHub, TurnEvent};
use rhema_rsd::ResourceKey;

/// How large one attachment may be. Generous for screenshots and
/// text; a whole dataset does not belong in a chat.
pub const MAX_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;

/// Why an attachment operation failed.
#[derive(Debug, thiserror::Error)]
pub enum AttachmentError {
    /// The backend failed to read or write.
    #[error("attachment store failed: {0}")]
    Backend(#[source] std::io::Error),
    /// A stored attachment's file went missing.
    #[error("attachment {0} is gone from the store")]
    Missing(String),
}

/// One stored attachment: the metadata the transcript carries.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Stored {
    /// The attachment's id (`att-…`), the fetch path's address.
    pub id: String,
    /// The original file's name, as the tool call named it.
    pub name: String,
    /// The byte size.
    pub size: u64,
    /// The media kind the name implies: `image`, `code`, or `file`.
    pub media: &'static str,
}

/// The attachments directory. Shareable behind an `Arc`; every
/// operation takes `&self`.
pub struct AttachmentStore {
    root: PathBuf,
}

impl AttachmentStore {
    /// Opens (creating if needed) the attachments directory at `root` —
    /// beside the database, the same state volume the transcript lives
    /// on, so a served id never outlives its bytes.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, AttachmentError> {
        let root = root.into();
        std::fs::create_dir_all(&root).map_err(AttachmentError::Backend)?;
        Ok(Self { root })
    }

    /// Stores `bytes` under `name`. The name is sanitized to its file
    /// name and rides the stored file name only for content-type
    /// guesses and human downloads; the id is the address.
    pub fn put(&self, name: &str, bytes: &[u8]) -> Result<Stored, AttachmentError> {
        static ATTACHMENT_SEQ: AtomicU64 = AtomicU64::new(0);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let id = format!(
            "att-{}-{:04x}",
            now,
            ATTACHMENT_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let name = sanitize_name(name);
        let media = media_of(&name);
        std::fs::write(self.root.join(&id), bytes).map_err(AttachmentError::Backend)?;
        // The name rides a sidecar: the transcript carries the original
        // spelling, and the fetch route needs the file name back for
        // its content type and Content-Disposition.
        std::fs::write(self.root.join(format!("{id}.name")), &name)
            .map_err(AttachmentError::Backend)?;
        Ok(Stored {
            id,
            name,
            size: bytes.len() as u64,
            media,
        })
    }

    /// The stored attachment's bytes, when it exists.
    pub fn get(&self, id: &str) -> Result<Option<(Stored, Vec<u8>)>, AttachmentError> {
        if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Ok(None);
        }
        let path = self.root.join(id);
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(AttachmentError::Backend(err)),
        };
        let bytes = std::fs::read(&path).map_err(AttachmentError::Backend)?;
        let name = std::fs::read_to_string(self.root.join(format!("{id}.name")))
            .ok()
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| id.to_owned());
        let stored = Stored {
            id: id.to_owned(),
            name,
            size: metadata.len(),
            media: "file",
        };
        Ok(Some((stored, bytes)))
    }
}

/// Reduces a caller-supplied name to a safe single file name: its last
/// path component, path separators and control characters folded to
/// `_`, never empty. The original spelling travels in the transcript's
/// metadata; this form only guards the store's content-type guesses.
fn sanitize_name(name: &str) -> String {
    let cleaned: String = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(name)
        .chars()
        .map(|c| if c.is_control() { '_' } else { c })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "attachment".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// The media kind a file name implies, the shape the wire's
/// `attachment` events carry: images render inline where a client can,
/// source files read as code, everything else downloads.
pub fn media_of(name: &str) -> &'static str {
    let extension = Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase());
    match extension.as_deref() {
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "bmp" | "ico" | "avif") => "image",
        Some(
            "rs" | "go" | "c" | "h" | "cpp" | "hpp" | "py" | "rb" | "java" | "kt" | "swift" | "js"
            | "mjs" | "cjs" | "ts" | "tsx" | "jsx" | "vue" | "svelte" | "sh" | "bash" | "zsh"
            | "fish" | "sql" | "toml" | "yaml" | "yml" | "json" | "jsonc" | "xml" | "html" | "css"
            | "scss" | "md" | "markdown" | "txt" | "diff" | "patch" | "lua" | "pl" | "php" | "cs"
            | "scala" | "hs" | "ex" | "exs" | "zig" | "gradle" | "tf",
        ) => "code",
        _ => "file",
    }
}

/// The content type a file name implies, for the fetch route's header.
pub fn content_type(name: &str) -> &'static str {
    let extension = Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase());
    match extension.as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("bmp") => "image/bmp",
        Some("ico") => "image/x-icon",
        Some("avif") => "image/avif",
        Some("json") => "application/json",
        Some("pdf") => "application/pdf",
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("md" | "markdown" | "txt" | "log" | "diff" | "patch") => "text/plain; charset=utf-8",
        Some("xml") => "application/xml",
        Some("wasm") => "application/wasm",
        Some("zip") => "application/zip",
        Some("tar") => "application/x-tar",
        Some("gz") => "application/gzip",
        _ => "application/octet-stream",
    }
}

/// The tool's name on the wire.
pub const ATTACH_FILE: &str = "attach_file";

/// `attach_file`: copy a workspace file into the attachment store and
/// put its card on the stream. Built per dispatch — the event must
/// land on the asking session.
struct AttachFile {
    store: Arc<AttachmentStore>,
    workspace: PathBuf,
    hub: Arc<EventHub>,
    key: ResourceKey,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AttachArgs {
    path: String,
    #[serde(default)]
    caption: Option<String>,
}

#[async_trait]
impl Tool for AttachFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            ATTACH_FILE,
            "Attach a file from this session's workspace to the chat: the \
             human gets a card with the file's name and can view or \
             download it. Use it to hand over artifacts a reply snippet \
             cannot carry — a screenshot, a diff, a generated report, a \
             log. The path is workspace-relative. Large files are \
             refused; attach the relevant cut instead.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string",
                        "description": "The workspace-relative file path, \
                            e.g. 'screenshots/failure.png'." },
                    "caption": { "type": "string",
                        "description": "One line about what the file is \
                            and why it matters." }
                },
                "required": ["path"]
            }),
        )
    }

    async fn invoke(&self, args: Value) -> Result<String, String> {
        let args: AttachArgs =
            serde_json::from_value(args).map_err(|err| format!("bad arguments: {err}"))?;
        let relative = Path::new(&args.path);
        if relative.is_absolute()
            || relative
                .components()
                .any(|c| c == std::path::Component::ParentDir)
        {
            return Err(format!(
                "path must be relative to the workspace, without '..': {:?}",
                args.path
            ));
        }
        let path = self.workspace.join(relative);
        let metadata = std::fs::metadata(&path)
            .map_err(|err| format!("cannot attach {:?}: {err}", args.path))?;
        if metadata.len() > MAX_ATTACHMENT_BYTES as u64 {
            return Err(format!(
                "cannot attach {:?}: {} bytes exceeds the {}-byte limit; attach a smaller cut",
                args.path,
                metadata.len(),
                MAX_ATTACHMENT_BYTES
            ));
        }
        let bytes =
            std::fs::read(&path).map_err(|err| format!("cannot attach {:?}: {err}", args.path))?;
        let stored = self
            .store
            .put(&args.path, &bytes)
            .map_err(|err| err.to_string())?;
        let caption = args
            .caption
            .as_deref()
            .map(str::trim)
            .filter(|caption| !caption.is_empty())
            .map(str::to_owned);
        self.hub.emit(
            &self.key,
            TurnEvent::Attachment {
                id: stored.id.clone(),
                name: stored.name.clone(),
                media: stored.media.to_owned(),
                size: stored.size,
                caption: caption.clone(),
            },
        );
        Ok(json!({
            "attached": true,
            "id": stored.id,
            "name": stored.name,
            "size": stored.size,
            "media": stored.media,
            "caption": caption,
        })
        .to_string())
    }
}

/// The attachment tool set over `store`: `attach_file`.
pub fn registry(
    store: Arc<AttachmentStore>,
    workspace: impl Into<PathBuf>,
    hub: Arc<EventHub>,
    key: ResourceKey,
) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(AttachFile {
        store,
        workspace: workspace.into(),
        hub,
        key,
    }));
    registry
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(dir: &Path) -> Arc<AttachmentStore> {
        Arc::new(AttachmentStore::open(dir.join("attachments")).expect("store opens"))
    }

    #[test]
    fn a_stored_attachment_round_trips_through_its_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store(dir.path());
        let stored = store
            .put("screenshots/failure.png", b"\x89PNG")
            .expect("puts");
        assert_eq!(
            stored.name, "failure.png",
            "the name folds to its file name"
        );
        assert_eq!(stored.media, "image");
        assert_eq!(stored.size, 4);
        let (fetched, bytes) = store.get(&stored.id).expect("reads").expect("exists");
        assert_eq!(bytes, b"\x89PNG");
        assert_eq!(fetched.id, stored.id);
        assert_eq!(
            store.get("../escape").expect("reads"),
            None,
            "a path is not an id"
        );
        assert_eq!(store.get("att-with$igns").expect("reads"), None);
    }

    #[test]
    fn media_kinds_follow_the_extension() {
        assert_eq!(media_of("a.PNG"), "image");
        assert_eq!(media_of("src/main.rs"), "code");
        assert_eq!(media_of("notes.md"), "code");
        assert_eq!(media_of("archive.tar.gz"), "file");
        assert_eq!(media_of("no-extension"), "file");
    }

    #[test]
    fn content_types_follow_the_extension() {
        assert_eq!(content_type("a.png"), "image/png");
        assert_eq!(content_type("run.log"), "text/plain; charset=utf-8");
        assert_eq!(content_type("blob.bin"), "application/octet-stream");
    }

    #[test]
    fn a_name_that_sanitizes_to_nothing_becomes_a_placeholder() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store(dir.path());
        let stored = store.put("/", b"x").expect("puts");
        assert_eq!(stored.name, "attachment");
    }

    /// The tool, end to end: a workspace file in, a stored attachment
    /// and a stream card out, with the caption riding.
    #[tokio::test]
    async fn attach_file_copies_a_workspace_file_and_emits_the_card() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = dir.path().join("interactive");
        std::fs::create_dir_all(workspace.join("out")).expect("workspace");
        std::fs::write(workspace.join("out/report.md"), "# report").expect("file");

        let store = store(dir.path());
        let hub = Arc::new(EventHub::default());
        let key = ResourceKey::from("interactive:web");
        let tool = AttachFile {
            store: store.clone(),
            workspace: workspace.clone(),
            hub: hub.clone(),
            key: key.clone(),
        };
        let answer = tool
            .invoke(json!({"path": "out/report.md", "caption": "the draft"}))
            .await
            .expect("in-band ok");
        let answer: Value = serde_json::from_str(&answer).expect("the answer parses");
        assert_eq!(answer["attached"], true);
        assert_eq!(answer["name"], "report.md");
        assert_eq!(answer["media"], "code");
        let id = answer["id"].as_str().expect("an id").to_owned();
        assert!(
            store.get(&id).expect("reads").is_some(),
            "the bytes are stored"
        );

        let (history, _) = hub.subscribe_with_history(&key);
        assert_eq!(
            history.last(),
            Some(&TurnEvent::Attachment {
                id,
                name: "report.md".into(),
                media: "code".into(),
                size: 8,
                caption: Some("the draft".into()),
            })
        );
    }

    #[tokio::test]
    async fn escapes_oversized_files_and_missing_paths_fail_in_band() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = dir.path().join("interactive");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let tool = AttachFile {
            store: store(dir.path()),
            workspace: workspace.clone(),
            hub: Arc::new(EventHub::default()),
            key: ResourceKey::from("interactive:web"),
        };
        for bad in [
            json!({"path": "../outside.txt"}),
            json!({"path": "/etc/hostname"}),
            json!({"path": "missing.txt"}),
        ] {
            assert!(
                tool.invoke(bad).await.is_err(),
                "an escape or a miss is an in-band error"
            );
        }
    }
}
