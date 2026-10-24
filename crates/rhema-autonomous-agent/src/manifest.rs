//! Manifest-defined tools: the spec as data, the operation as code.
//!
//! A tool is a folder holding a `TOOL.yaml`: the name the model calls,
//! the description it picks the tool by, the argument schema, the
//! replay classification, and a `run` binding. This module supports one
//! binding, `run: { builtin: <id> }` — the operation is compiled code
//! the consumer registers under that id — so a tool's static data stops
//! living next to it in Rust string literals, and one folder names one
//! tool. The same format takes other `run` kinds later (a command to
//! spawn, for user-written script tools) without a migration: the spec
//! half of [`Tool`] already comes from the file.
//!
//! A manifest that does not parse, or whose `run.builtin` id has no
//! registered operation, is skipped rather than fatal, like an
//! undescribed skill folder; a shipped set pins itself with a test.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use rhema_core::{ReplaySafety, ToolSpec};
use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;

use crate::registry::{Tool, ToolError};

/// The static half of a tool, parsed from a `TOOL.yaml`.
#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    /// Name the model uses to call the tool.
    pub name: String,
    /// What the tool does; the model picks tools from this.
    pub description: String,
    /// JSON schema for the call arguments; the empty-object schema when
    /// the manifest omits it.
    pub parameters: Value,
    /// Whether a resume may re-execute a committed call that lost its
    /// result; [`ReplaySafety::Unsafe`] when the manifest omits it.
    pub replay_safety: ReplaySafety,
    /// The `run.builtin` id naming the compiled operation.
    pub builtin: String,
}

/// Why a `TOOL.yaml` did not parse into a [`Manifest`].
#[derive(Debug, Error)]
#[error("{0}")]
pub struct ManifestError(String);

/// Parses one `TOOL.yaml`. `parameters` defaults to the empty-object
/// schema and `replay_safety` to [`ReplaySafety::Unsafe`], so a minimal
/// manifest is a name, a description, and a `run` binding.
pub fn parse(raw: &str) -> Result<Manifest, ManifestError> {
    let yaml: ManifestYaml =
        serde_yaml::from_str(raw).map_err(|err| ManifestError(err.to_string()))?;
    let name = yaml.name.trim().to_owned();
    if name.is_empty() {
        return Err(ManifestError("no tool name".to_owned()));
    }
    let description = yaml.description.trim().to_owned();
    if description.is_empty() {
        return Err(ManifestError(format!("{name}: no description")));
    }
    let builtin = match yaml.run.kind {
        RunKind::Builtin { builtin } => builtin.trim().to_owned(),
    };
    if builtin.is_empty() {
        return Err(ManifestError(format!(
            "{name}: run.builtin names no operation"
        )));
    }
    let parameters = match yaml.parameters {
        Some(value) => serde_json::to_value(&value)
            .map_err(|err| ManifestError(format!("{name}: parameters are not JSON: {err}")))?,
        None => serde_json::json!({ "type": "object" }),
    };
    let replay_safety = yaml
        .replay_safety
        .map(|replay| match replay {
            ReplayYaml::Unsafe => ReplaySafety::Unsafe,
            ReplayYaml::Safe => ReplaySafety::Safe,
        })
        .unwrap_or_default();
    Ok(Manifest {
        name,
        description,
        parameters,
        replay_safety,
        builtin,
    })
}

/// The operation half of a manifest-defined tool: run the call. The
/// spec half lives in the tool's `TOOL.yaml`, so an operation carries
/// no schema strings of its own.
#[async_trait]
pub trait ToolOp: Send + Sync {
    /// Runs the tool. The registry turns `Err` into an in-band failure
    /// result; `Ok` text is the tool's answer.
    async fn invoke(&self, args: Value) -> Result<String, ToolError>;
}

/// A [`Tool`] cut from a [`Manifest`] plus its registered operation:
/// the model sees the manifest's spec, and the registry dispatches to
/// the operation.
pub struct ManifestTool {
    manifest: Manifest,
    op: Arc<dyn ToolOp>,
}

impl ManifestTool {
    /// Binds `manifest` to the operation `op`.
    pub fn new(manifest: Manifest, op: Arc<dyn ToolOp>) -> Self {
        Self { manifest, op }
    }
}

#[async_trait]
impl Tool for ManifestTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            self.manifest.name.clone(),
            self.manifest.description.clone(),
            self.manifest.parameters.clone(),
        )
    }

    fn replay_safety(&self) -> ReplaySafety {
        self.manifest.replay_safety
    }

    async fn invoke(&self, args: Value) -> Result<String, ToolError> {
        self.op.invoke(args).await
    }
}

/// Loads `manifests` — `(source, text)` pairs, `source` naming the
/// manifest in skip reports — against the registered `builtins`. A
/// manifest that does not parse, or whose `run.builtin` id has no
/// operation, is skipped and reported; the survivors are ready to
/// register.
pub fn builtin_tools(
    manifests: &[(&str, &str)],
    builtins: &BTreeMap<String, Arc<dyn ToolOp>>,
) -> (Vec<Arc<dyn Tool>>, Vec<String>) {
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    let mut skipped = Vec::new();
    for (source, raw) in manifests {
        let manifest = match parse(raw) {
            Ok(manifest) => manifest,
            Err(err) => {
                skipped.push(format!("{source}: {err}"));
                continue;
            }
        };
        match builtins.get(&manifest.builtin) {
            Some(op) => tools.push(Arc::new(ManifestTool::new(manifest, op.clone()))),
            None => skipped.push(format!(
                "{source}: no builtin operation for {:?}",
                manifest.builtin
            )),
        }
    }
    (tools, skipped)
}

/// The `TOOL.yaml` body this module reads.
#[derive(Debug, Deserialize)]
struct ManifestYaml {
    name: String,
    description: String,
    #[serde(default)]
    parameters: Option<serde_yaml::Value>,
    #[serde(default)]
    replay_safety: Option<ReplayYaml>,
    run: Run,
}

/// The `run` binding: the kind's field name is the variant, so
/// `builtin` names a compiled operation the consumer registered under
/// that id, and a future kind is another variant.
#[derive(Debug, Deserialize)]
struct Run {
    #[serde(flatten)]
    kind: RunKind,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RunKind {
    Builtin { builtin: String },
}

/// The manifest's `replay_safety`, lowercase to read like the yaml it
/// sits in; maps onto [`ReplaySafety`].
#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ReplayYaml {
    Unsafe,
    Safe,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Allowlist;
    use crate::registry::ToolRegistry;

    struct Echo;

    #[async_trait]
    impl ToolOp for Echo {
        async fn invoke(&self, args: Value) -> Result<String, ToolError> {
            Ok(args.to_string())
        }
    }

    struct Boom;

    #[async_trait]
    impl ToolOp for Boom {
        async fn invoke(&self, _args: Value) -> Result<String, ToolError> {
            Err(ToolError::new("exploded"))
        }
    }

    fn builtins() -> BTreeMap<String, Arc<dyn ToolOp>> {
        BTreeMap::from([
            ("test.echo".to_owned(), Arc::new(Echo) as Arc<dyn ToolOp>),
            ("test.boom".to_owned(), Arc::new(Boom) as Arc<dyn ToolOp>),
        ])
    }

    #[test]
    fn a_full_manifest_parses() {
        let manifest = parse(
            "name: probe\n\
             description: |\n\
             \x20 Probe the parser.\n\
             parameters:\n\
             \x20 type: object\n\
             \x20 properties:\n\
             \x20   path:\n\
             \x20     type: string\n\
             \x20 required: [path]\n\
             replay_safety: safe\n\
             run:\n\
             \x20 builtin: test.echo\n",
        )
        .expect("parses");
        assert_eq!(manifest.name, "probe");
        assert_eq!(manifest.description, "Probe the parser.");
        assert_eq!(
            manifest.parameters,
            serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            })
        );
        assert_eq!(manifest.replay_safety, ReplaySafety::Safe);
        assert_eq!(manifest.builtin, "test.echo");
    }

    #[test]
    fn omissions_take_defaults() {
        let manifest = parse("name: probe\ndescription: Probe.\nrun:\n  builtin: test.echo\n")
            .expect("parses");
        assert_eq!(manifest.parameters, serde_json::json!({ "type": "object" }));
        assert_eq!(manifest.replay_safety, ReplaySafety::Unsafe);
    }

    #[test]
    fn blank_fields_and_a_missing_run_are_rejected() {
        assert!(parse("description: Probe.\nrun:\n  builtin: test.echo\n").is_err());
        assert!(parse("name: probe\ndescription: \"  \"\nrun:\n  builtin: test.echo\n").is_err());
        assert!(parse("name: probe\ndescription: Probe.\nrun:\n  builtin: \"  \"\n").is_err());
        assert!(parse("name: probe\ndescription: Probe.\n").is_err());
    }

    #[test]
    fn an_unknown_run_kind_is_rejected_not_silently_dropped() {
        let err = parse("name: probe\ndescription: Probe.\nrun:\n  exec: ./run.sh\n")
            .expect_err("exec is not a binding yet");
        assert!(
            err.to_string().contains("did not match any variant"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_manifest_tool_serves_the_manifest_spec_and_the_operation() {
        let manifest = parse("name: probe\ndescription: Probe.\nrun:\n  builtin: test.echo\n")
            .expect("parses");
        let tool = ManifestTool::new(manifest, Arc::new(Echo));
        assert_eq!(tool.spec().name, "probe");
        assert_eq!(tool.spec().description, "Probe.");
        assert_eq!(
            tool.invoke(serde_json::json!({ "a": 1 })).await,
            Ok(r#"{"a":1}"#.to_owned())
        );
    }

    #[tokio::test]
    async fn an_operation_error_stays_an_in_band_tool_error() {
        let manifest = parse("name: probe\ndescription: Probe.\nrun:\n  builtin: test.boom\n")
            .expect("parses");
        let tool = ManifestTool::new(manifest, Arc::new(Boom));
        assert_eq!(
            tool.invoke(Value::Null).await,
            Err(ToolError::new("exploded"))
        );
    }

    #[test]
    fn loading_resolves_builtins_and_reports_skips() {
        let manifests = [
            (
                "probe",
                "name: probe\ndescription: Probe.\nrun:\n  builtin: test.echo\n",
            ),
            ("bad", "name: bad\ndescription: Probe.\n"),
            (
                "orphan",
                "name: orphan\ndescription: Probe.\nrun:\n  builtin: test.nope\n",
            ),
        ];
        let (tools, skipped) = builtin_tools(&manifests, &builtins());
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].spec().name, "probe");
        assert_eq!(skipped.len(), 2, "{skipped:?}");
        assert!(skipped[0].starts_with("bad:"), "{skipped:?}");
        assert!(skipped[1].contains("test.nope"), "{skipped:?}");
    }

    #[test]
    fn a_loaded_tool_registers_under_its_manifest_name_and_classifies() {
        let manifests = [(
            "probe",
            "name: probe\ndescription: Probe.\nreplay_safety: safe\nrun:\n  builtin: test.echo\n",
        )];
        let (tools, skipped) = builtin_tools(&manifests, &builtins());
        assert!(skipped.is_empty(), "{skipped:?}");
        let mut registry = ToolRegistry::new();
        for tool in tools {
            registry.register(tool);
        }
        assert_eq!(registry.len(), 1);
        let resolver = registry.resolver_for(&Allowlist::permissive());
        assert_eq!(resolver.specs()[0].name, "probe");
        assert_eq!(
            resolver.replay_safety("probe"),
            ReplaySafety::Safe,
            "the manifest's classification reaches the resolver"
        );
        assert_eq!(resolver.replay_safety("unknown"), ReplaySafety::Unsafe);
    }
}
