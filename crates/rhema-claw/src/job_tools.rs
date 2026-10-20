//! The `coding_agent` tool: how the resident drives a coding agent —
//! named instances over real workspaces, and async jobs against them.
//!
//! One tool, an `action` argument, the [`crate::phone`] tool's shape:
//! `add` pins a name to a workspace directory, `drop` removes it,
//! `run` enqueues a job (a `CodingAgent` turn over the instance's
//! workspace, resumed from the instance's last coding session unless
//! `fresh`), `list` names the instances, and `status` reports jobs.
//! The run is
//! **async by design**: a coding job is minutes of tool rounds, so the
//! call returns the job id in one model round, the jobs dispatcher
//! runs the turn off the chat turn ([`crate::jobs`]), and the final
//! text lands in the job row and the user's notifications. The model
//! answers "it's running" honestly and reads the result later with
//! `status`.

use std::collections::BTreeMap;
use std::sync::Arc;

use rhema_autonomous_agent::{ToolOp, ToolRegistry, async_trait};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::job_store::{CodingJobStore, JobStoreError};

/// The compiled operation behind the manifest, by `run.builtin` id.
fn builtins(store: Arc<CodingJobStore>) -> BTreeMap<String, Arc<dyn ToolOp>> {
    BTreeMap::from([(
        "coding.agent".to_owned(),
        Arc::new(CodingAgent { store }) as Arc<dyn ToolOp>,
    )])
}

/// The coding_agent tool's `TOOL.yaml`, embedded so the daemon ships
/// it with no filesystem layout.
pub const MANIFESTS: &[(&str, &str)] = &[(
    "coding_agent",
    include_str!("../tools/coding_agent/TOOL.yaml"),
)];

/// The coding-agent tool set over `store`: one tool, `coding_agent` —
/// its spec from the embedded manifest, the operation compiled here.
pub fn registry(store: Arc<CodingJobStore>) -> ToolRegistry {
    let (tools, skipped) =
        rhema_autonomous_agent::manifest::builtin_tools(MANIFESTS, &builtins(store));
    assert!(
        skipped.is_empty(),
        "the coding_agent tool manifest must load: {skipped:?}"
    );
    let mut registry = ToolRegistry::new();
    for tool in tools {
        registry.register(tool);
    }
    registry
}

#[derive(Deserialize)]
struct Args {
    action: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    instance: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    fresh: Option<bool>,
    #[serde(default)]
    job: Option<i64>,
}

/// The tool: one `coding_agent` call, `action` picks the operation.
struct CodingAgent {
    store: Arc<CodingJobStore>,
}

impl CodingAgent {
    fn add(&self, name: &str, path: &str) -> Result<String, String> {
        if !std::path::Path::new(path).is_dir() {
            return Err(format!(
                "no directory {path:?}: the workspace must exist; create or clone it first"
            ));
        }
        self.store
            .add_instance(name, path)
            .map(|_| json!({"added": name, "path": path}).to_string())
            .map_err(in_band)
    }

    fn drop(&self, name: &str) -> Result<String, String> {
        let dropped = self.store.drop_instance(name).map_err(in_band)?;
        if dropped {
            Ok(json!({"dropped": name}).to_string())
        } else {
            Err(format!("no instance named {name:?}"))
        }
    }

    fn run(&self, instance: &str, prompt: &str, fresh: bool) -> Result<String, String> {
        if prompt.trim().is_empty() {
            return Err("`prompt` is empty; the coding agent needs a task".to_owned());
        }
        if fresh {
            // The conversation reset lands before the enqueue: a fresh
            // flag on a refused run must not wipe a session it did not
            // replace.
            self.store.clear_session(instance).map_err(in_band)?;
        }
        let job = self.store.enqueue(instance, prompt).map_err(in_band)?;
        Ok(json!({
            "job": job.id,
            "instance": instance,
            "status": "queued",
            "note": "the job runs off this chat; its final text lands in \
                     coding_agent status and the user's notifications"
        })
        .to_string())
    }

    /// The instances: the audit surface for which checkouts a coding
    /// agent may drive, each with its live job at a glance.
    fn list(&self) -> Result<String, String> {
        let instances = self.store.instances().map_err(in_band)?;
        let listed = instances
            .iter()
            .map(|instance| {
                let live = self
                    .store
                    .live_job(&instance.name)
                    .ok()
                    .flatten()
                    .map(|job| json!({"id": job.id, "status": job.status.as_str()}));
                json!({
                    "name": instance.name,
                    "path": instance.path,
                    "has_session": instance.session.is_some(),
                    "live_job": live,
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({ "instances": listed }).to_string())
    }

    /// The jobs: every recent one, one instance's, or one job's result.
    fn status(&self, instance: Option<&str>, job: Option<i64>) -> Result<String, String> {
        if let Some(id) = job {
            let record = self
                .store
                .job(id)
                .map_err(in_band)?
                .ok_or_else(|| format!("no job {id}"))?;
            return Ok(job_json(&record).to_string());
        }
        match instance {
            Some(name) => {
                if self.store.instance(name).map_err(in_band)?.is_none() {
                    return Err(format!("no instance named {name:?}"));
                }
                let jobs = self.store.jobs_for(name, 20).map_err(in_band)?;
                Ok(json!({
                    "instance": name,
                    "jobs": jobs.iter().map(job_json).collect::<Vec<_>>(),
                })
                .to_string())
            }
            None => {
                let jobs = self.store.recent(20).map_err(in_band)?;
                Ok(json!({
                    "jobs": jobs.iter().map(job_json).collect::<Vec<_>>(),
                })
                .to_string())
            }
        }
    }
}

fn job_json(job: &crate::job_store::JobRecord) -> Value {
    json!({
        "job": job.id,
        "instance": job.instance,
        "status": job.status.as_str(),
        "result": job.result,
        "prompt": if job.status.is_live() { Some(job.prompt.as_str()) } else { None },
    })
}

/// The store's typed refusals are the model's in-band answers.
fn in_band(err: JobStoreError) -> String {
    err.to_string()
}

#[async_trait]
impl ToolOp for CodingAgent {
    async fn invoke(&self, args: Value) -> Result<String, String> {
        let args: Args =
            serde_json::from_value(args).map_err(|err| format!("bad arguments: {err}"))?;
        match args.action.as_str() {
            "add" => self.add(
                required(&args.name, "add needs `name`")?,
                required(&args.path, "add needs `path`")?,
            ),
            "drop" => self.drop(required(&args.name, "drop needs `name`")?),
            "run" => self.run(
                required(&args.instance, "run needs `instance`")?,
                required(&args.prompt, "run needs `prompt`")?,
                args.fresh.unwrap_or(false),
            ),
            "list" => self.list(),
            "status" => self.status(args.instance.as_deref(), args.job),
            other => Err(format!(
                "unknown action {other:?}; one of add, drop, run, list, status"
            )),
        }
    }
}

fn required<'a>(value: &'a Option<String>, complaint: &str) -> Result<&'a str, String> {
    value
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| complaint.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhema_autonomous_agent::Allowlist;

    #[test]
    fn the_manifest_loads_against_its_builtin_and_serves_the_registry() {
        let store = StdArc::new(CodingJobStore::in_memory().unwrap());
        let resolver = registry(store).resolver_for(&Allowlist::permissive());
        assert_eq!(
            resolver.replay_safety("coding_agent"),
            rhema_core::ReplaySafety::Unsafe,
            "a resumed run must not re-enqueue a coding job"
        );
    }
    use rhema_core::{ToolCall, ToolContext, ToolResolver};
    use serde_json::json;
    use std::sync::Arc as StdArc;

    fn resolver(store: StdArc<CodingJobStore>) -> StdArc<dyn ToolResolver> {
        registry(store).resolver_for(&Allowlist::permissive())
    }

    fn tempdir() -> std::path::PathBuf {
        let dir = tempfile::tempdir().unwrap().keep();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn call(resolver: &StdArc<dyn ToolResolver>, args: Value) -> Result<String, String> {
        let result = resolver
            .resolve(
                ToolCall {
                    id: "call_1".into(),
                    name: "coding_agent".into(),
                    args,
                    provider_data: Value::Null,
                },
                &ToolContext::default(),
            )
            .await;
        if result.is_error {
            Err(result.content)
        } else {
            Ok(result.content)
        }
    }

    #[tokio::test]
    async fn add_run_and_status_round_trip() {
        let store = StdArc::new(CodingJobStore::in_memory().unwrap());
        let resolver = resolver(store.clone());
        let workspace = tempdir();
        let added = call(
            &resolver,
            json!({"action": "add", "name": "rhema",
                   "path": workspace.to_str().unwrap()}),
        )
        .await
        .unwrap();
        assert!(added.contains("\"added\":\"rhema\""), "{added}");

        let run = call(
            &resolver,
            json!({"action": "run", "instance": "rhema", "prompt": "fix the lint"}),
        )
        .await
        .unwrap();
        assert!(run.contains("\"status\":\"queued\""), "{run}");

        let listed = call(&resolver, json!({"action": "list"})).await.unwrap();
        assert!(listed.contains("rhema"), "{listed}");
        assert!(listed.contains("live_job"), "{listed}");

        let recent = call(&resolver, json!({"action": "status"})).await.unwrap();
        assert!(recent.contains("fix the lint"), "{recent}");

        let status = call(&resolver, json!({"action": "status", "instance": "rhema"}))
            .await
            .unwrap();
        assert!(status.contains("fix the lint"), "{status}");
    }

    #[tokio::test]
    async fn the_refusals_are_in_band_and_named() {
        let store = StdArc::new(CodingJobStore::in_memory().unwrap());
        let resolver = resolver(store.clone());
        let workspace = tempdir();

        let err = call(&resolver, json!({"action": "explode"}))
            .await
            .unwrap_err();
        assert!(err.contains("unknown action"), "{err}");
        let err = call(&resolver, json!({"action": "add", "name": "x"}))
            .await
            .unwrap_err();
        assert!(err.contains("add needs `path`"), "{err}");
        let err = call(
            &resolver,
            json!({"action": "add", "name": "x", "path": "/nowhere-at-all"}),
        )
        .await
        .unwrap_err();
        assert!(err.contains("no directory"), "{err}");

        call(
            &resolver,
            json!({"action": "add", "name": "rhema", "path": workspace.to_str().unwrap()}),
        )
        .await
        .unwrap();
        let err = call(
            &resolver,
            json!({"action": "run", "instance": "ghost", "prompt": "x"}),
        )
        .await
        .unwrap_err();
        assert!(err.contains("no instance named"), "{err}");

        call(
            &resolver,
            json!({"action": "run", "instance": "rhema", "prompt": "long work"}),
        )
        .await
        .unwrap();
        let err = call(
            &resolver,
            json!({"action": "run", "instance": "rhema", "prompt": "more"}),
        )
        .await
        .unwrap_err();
        assert!(err.contains("job in flight"), "{err}");
        let err = call(&resolver, json!({"action": "drop", "name": "rhema"}))
            .await
            .unwrap_err();
        assert!(err.contains("job in flight"), "{err}");

        let err = call(&resolver, json!({"action": "status", "job": 999}))
            .await
            .unwrap_err();
        assert!(err.contains("no job 999"), "{err}");
    }

    #[tokio::test]
    async fn fresh_clears_the_session_only_on_a_valid_run() {
        let store = StdArc::new(CodingJobStore::in_memory().unwrap());
        store.add_instance("rhema", "/tmp/rhema").unwrap();
        store.save_session("rhema", r#"{"history":[1]}"#).unwrap();
        let resolver = resolver(store.clone());
        // The refused run wipes nothing.
        let err = call(
            &resolver,
            json!({"action": "run", "instance": "ghost", "prompt": "x", "fresh": true}),
        )
        .await
        .unwrap_err();
        assert!(err.contains("no instance named"), "{err}");
        assert!(store.instance("rhema").unwrap().unwrap().session.is_some());
        // The accepted run clears before enqueueing.
        call(
            &resolver,
            json!({"action": "run", "instance": "rhema", "prompt": "x", "fresh": true}),
        )
        .await
        .unwrap();
        assert!(store.instance("rhema").unwrap().unwrap().session.is_none());
    }
}
