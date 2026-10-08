//! The built-in tool against a real tempdir workspace: execution, exit
//! codes, output caps, approval gating, and cancellation. No mock needed —
//! the resolver runs directly.

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
        provider_data: serde_json::Value::Null,
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
async fn bash_runs_in_the_workspace_and_returns_output_with_the_code() {
    let root = tempfile::tempdir().unwrap();
    let tools = CodingTools::new(root.path());
    tools.install_approval(Arc::new(AllowAll));
    let ctx = ctx(root.path());

    let result = resolve(
        &tools,
        &ctx,
        "bash",
        json!({ "command": "pwd && echo hello" }),
    )
    .await;
    assert!(!result.is_error);
    assert!(result.content.contains("hello"), "{}", result.content);
    assert!(
        result
            .content
            .contains(root.path().to_string_lossy().as_ref()),
        "runs in the workspace: {}",
        result.content
    );
    assert_eq!(result.code, Some(0));
}

#[tokio::test]
async fn a_failing_command_is_an_in_band_error_with_the_code() {
    let root = tempfile::tempdir().unwrap();
    let tools = CodingTools::new(root.path());
    tools.install_approval(Arc::new(AllowAll));
    let ctx = ctx(root.path());

    let result = resolve(&tools, &ctx, "bash", json!({ "command": "exit 3" })).await;
    assert!(result.is_error);
    assert!(result.content.contains("exit code 3"), "{}", result.content);
    assert_eq!(result.code, Some(3));
}

#[tokio::test]
async fn a_rejected_call_never_executes() {
    let root = tempfile::tempdir().unwrap();
    let tools = CodingTools::new(root.path());
    tools.install_approval(Arc::new(DenyAll));
    let ctx = ctx(root.path());

    let result = resolve(&tools, &ctx, "bash", json!({ "command": "touch marker" })).await;
    assert!(result.is_error);
    assert_eq!(result.content, "rejected by user approval");
    assert_eq!(result.code, None, "nothing ran, no exit code");
    assert!(!root.path().join("marker").exists());
}

#[tokio::test]
async fn the_tool_rejects_without_an_installed_hook() {
    let root = tempfile::tempdir().unwrap();
    let tools = CodingTools::new(root.path());
    let ctx = ctx(root.path());

    let result = resolve(&tools, &ctx, "bash", json!({ "command": "true" })).await;
    assert!(result.is_error);
    assert_eq!(result.content, "rejected by user approval");
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

    let result = resolve(&tools, &ctx, "bash", json!({ "command": "true" })).await;
    assert!(result.is_error);
    assert_eq!(result.content, "cancelled");
    assert!(cancelled.load(Ordering::Relaxed));
}

#[tokio::test]
async fn large_output_is_truncated_with_a_notice() {
    let root = tempfile::tempdir().unwrap();
    let tools = CodingTools::new(root.path());
    tools.install_approval(Arc::new(AllowAll));
    let ctx = ctx(root.path());

    let result = resolve(
        &tools,
        &ctx,
        "bash",
        json!({ "command": "yes | head -c 64k" }),
    )
    .await;
    assert!(!result.is_error);
    assert!(
        result.content.contains("[output truncated"),
        "{}",
        result.content
    );
    assert!(result.content.len() < 64 * 1024);
    // The elided head is counted exactly: the whole 64k minus the kept
    // 50 KiB, aligned back to a line start.
    assert!(
        result
            .content
            .contains("[output truncated — 14336 bytes elided]"),
        "{}",
        result.content
    );
}

#[tokio::test]
async fn a_missing_argument_is_an_in_band_error() {
    let root = tempfile::tempdir().unwrap();
    let tools = CodingTools::new(root.path());
    tools.install_approval(Arc::new(AllowAll));
    let ctx = ctx(root.path());

    let result = resolve(&tools, &ctx, "bash", json!({})).await;
    assert!(result.is_error);
    assert_eq!(result.content, "missing required argument: command");
}
