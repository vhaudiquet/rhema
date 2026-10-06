//! The built-in tools against a real tempdir workspace: round-trips,
//! workspace escapes, approval gating, and command execution. No mock
//! needed — the resolver runs directly.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rhema_coding_agent::{AllowAll, Approval, CodingTools};
use rhema_core::{ToolCall, ToolContext, ToolResolver};
use serde_json::json;

fn ctx(root: &Path) -> ToolContext {
    ToolContext {
        cwd: root.to_path_buf(),
        run_id: "test".into(),
        cancelled: Arc::new(AtomicBool::new(false)),
    }
}

fn call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        args,
    }
}

async fn resolve(
    tools: &CodingTools,
    ctx: &ToolContext,
    name: &str,
    args: serde_json::Value,
) -> rhema_core::ToolResult {
    tools.resolve(call("call_1", name, args), ctx).await
}

struct DenyAll;

impl Approval for DenyAll {
    fn approve(&self, _call: &ToolCall) -> bool {
        false
    }
}

#[tokio::test]
async fn read_list_write_round_trip_in_a_workspace() {
    let root = tempfile::tempdir().unwrap();
    let tools = CodingTools::new(root.path());
    tools.install_approval(Arc::new(AllowAll));
    let ctx = ctx(root.path());

    let result = resolve(
        &tools,
        &ctx,
        "write_file",
        json!({ "path": "src/main.rs", "content": "fn main() {}\n" }),
    )
    .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(
        std::fs::read_to_string(root.path().join("src/main.rs")).unwrap(),
        "fn main() {}\n"
    );
    assert!(result.content.contains("13 bytes"), "{}", result.content);

    let result = resolve(&tools, &ctx, "read_file", json!({ "path": "src/main.rs" })).await;
    assert!(!result.is_error);
    assert_eq!(result.content, "fn main() {}\n");

    std::fs::write(root.path().join("README.md"), "# rhema\n").unwrap();
    let result = resolve(&tools, &ctx, "list_dir", json!({})).await;
    assert!(!result.is_error);
    assert_eq!(
        result.content.lines().collect::<Vec<_>>(),
        vec!["dir: src", "file: README.md (8 bytes)"]
    );
}

#[tokio::test]
async fn reading_a_missing_file_is_an_in_band_error() {
    let root = tempfile::tempdir().unwrap();
    let tools = CodingTools::new(root.path());
    let ctx = ctx(root.path());

    let result = resolve(&tools, &ctx, "read_file", json!({ "path": "nope.txt" })).await;
    assert!(result.is_error);
    assert!(result.content.contains("read failed"), "{}", result.content);
}

#[tokio::test]
async fn absolute_paths_outside_the_workspace_are_rejected() {
    let root = tempfile::tempdir().unwrap();
    let tools = CodingTools::new(root.path());
    let ctx = ctx(root.path());

    let result = resolve(
        &tools,
        &ctx,
        "read_file",
        json!({ "path": "/etc/hostname" }),
    )
    .await;
    assert!(result.is_error);
    assert!(
        result.content.contains("workspace escape"),
        "{}",
        result.content
    );
}

#[tokio::test]
async fn traversal_outside_the_workspace_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let tools = CodingTools::new(root.path());
    let ctx = ctx(root.path());

    let result = resolve(
        &tools,
        &ctx,
        "read_file",
        json!({ "path": "../../etc/hostname" }),
    )
    .await;
    assert!(result.is_error);
    assert!(
        result.content.contains("workspace escape"),
        "{}",
        result.content
    );
}

#[tokio::test]
async fn a_writing_escape_is_rejected_without_creating_the_file() {
    let root = tempfile::tempdir().unwrap();
    let tools = CodingTools::new(root.path());
    tools.install_approval(Arc::new(AllowAll));
    let ctx = ctx(root.path());

    let result = resolve(
        &tools,
        &ctx,
        "write_file",
        json!({ "path": "../escape.txt", "content": "x" }),
    )
    .await;
    assert!(result.is_error);
    let escaped = root
        .path()
        .canonicalize()
        .unwrap()
        .parent()
        .unwrap()
        .join("escape.txt");
    assert!(!escaped.exists());
}

#[tokio::test]
async fn a_rejected_write_file_never_touches_the_disk() {
    let root = tempfile::tempdir().unwrap();
    let tools = CodingTools::new(root.path());
    tools.install_approval(Arc::new(DenyAll));
    let ctx = ctx(root.path());

    let result = resolve(
        &tools,
        &ctx,
        "write_file",
        json!({ "path": "notes.txt", "content": "hi" }),
    )
    .await;
    assert!(result.is_error);
    assert_eq!(result.content, "rejected by user approval");
    assert!(!root.path().join("notes.txt").exists());
}

#[tokio::test]
async fn a_rejected_run_command_never_executes() {
    let root = tempfile::tempdir().unwrap();
    let tools = CodingTools::new(root.path());
    tools.install_approval(Arc::new(DenyAll));
    let ctx = ctx(root.path());

    let result = resolve(
        &tools,
        &ctx,
        "run_command",
        json!({ "command": "touch marker" }),
    )
    .await;
    assert!(result.is_error);
    assert_eq!(result.content, "rejected by user approval");
    assert!(!root.path().join("marker").exists());
}

#[tokio::test]
async fn gated_tools_reject_without_an_installed_hook() {
    let root = tempfile::tempdir().unwrap();
    let tools = CodingTools::new(root.path());
    let ctx = ctx(root.path());

    let result = resolve(
        &tools,
        &ctx,
        "write_file",
        json!({ "path": "notes.txt", "content": "hi" }),
    )
    .await;
    assert!(result.is_error);
    assert!(!root.path().join("notes.txt").exists());
}

#[tokio::test]
async fn run_command_returns_output_and_exit_status() {
    let root = tempfile::tempdir().unwrap();
    let tools = CodingTools::new(root.path());
    tools.install_approval(Arc::new(AllowAll));
    let ctx = ctx(root.path());

    let result = resolve(
        &tools,
        &ctx,
        "run_command",
        json!({ "command": "echo hello" }),
    )
    .await;
    assert!(!result.is_error);
    assert!(result.content.contains("hello"), "{}", result.content);
    assert!(result.content.contains("exit code 0"), "{}", result.content);
}

#[tokio::test]
async fn a_failing_command_is_an_in_band_error_with_the_status() {
    let root = tempfile::tempdir().unwrap();
    let tools = CodingTools::new(root.path());
    tools.install_approval(Arc::new(AllowAll));
    let ctx = ctx(root.path());

    let result = resolve(&tools, &ctx, "run_command", json!({ "command": "exit 3" })).await;
    assert!(result.is_error);
    assert!(result.content.contains("exit code 3"), "{}", result.content);
}

#[tokio::test]
async fn an_unknown_tool_name_is_an_in_band_error() {
    let root = tempfile::tempdir().unwrap();
    let tools = CodingTools::new(root.path());
    let ctx = ctx(root.path());

    let result = resolve(&tools, &ctx, "nope", json!({})).await;
    assert!(result.is_error);
    assert_eq!(result.content, "unknown tool: nope");
}

#[tokio::test]
async fn a_cancelled_context_rejects_every_call() {
    let root = tempfile::tempdir().unwrap();
    let tools = CodingTools::new(root.path());
    let cancelled = Arc::new(AtomicBool::new(true));
    let ctx = ToolContext {
        cwd: root.path().to_path_buf(),
        run_id: "test".into(),
        cancelled: cancelled.clone(),
    };

    let result = resolve(&tools, &ctx, "read_file", json!({ "path": "any.txt" })).await;
    assert!(result.is_error);
    assert_eq!(result.content, "cancelled");
    assert!(cancelled.load(Ordering::Relaxed));
}

#[tokio::test]
async fn read_file_truncates_large_content() {
    let root = tempfile::tempdir().unwrap();
    let tools = CodingTools::new(root.path());
    let ctx = ctx(root.path());

    let big = "a".repeat(150 * 1024);
    std::fs::write(root.path().join("big.txt"), &big).unwrap();
    let result = resolve(&tools, &ctx, "read_file", json!({ "path": "big.txt" })).await;
    assert!(!result.is_error);
    assert!(result.content.len() < big.len());
    assert!(result.content.ends_with("[truncated]"));
    assert!(result.content.starts_with(&"a".repeat(1024)));
}
