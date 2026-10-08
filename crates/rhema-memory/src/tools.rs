//! The memory tools: `memory_search`, `memory_save`, `memory_forget`.
//!
//! [`MemoryResolver`] is one [`ToolResolver`] offering all three — the
//! shape the step engine and a resolver chain compose. Its per-tool
//! operations also run individually through [`MemoryResolver::execute`],
//! so the resident tier's registry can shim them into named `Tool`s
//! without re-implementing argument handling. Every failure comes back
//! in-band — a missing argument, an unknown id — because the model reacts
//! to results; nothing here aborts a turn.
//!
//! This crate ships no prompt text: the consumer's system prompt carries
//! the one bootstrap instruction ("you have a memory; search it when
//! context is missing; save what will matter tomorrow"), per framework
//! spec §9.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use serde_json::{Value, json};

use rhema_core::{ToolCall, ToolContext, ToolResolver, ToolResult, ToolSpec};

use crate::record::{MemoryDraft, Scope, format_utc};
use crate::search::MemoryQuery;
use crate::store::MemoryStore;

/// The search tool's name — allowlists reference these.
pub const MEMORY_SEARCH: &str = "memory_search";
/// The save tool's name.
pub const MEMORY_SAVE: &str = "memory_save";
/// The forget tool's name.
pub const MEMORY_FORGET: &str = "memory_forget";

/// The three memory tools over one store, acting in one scope. The scope
/// is fixed at construction — the coding agent's workspace, the claw's
/// watched checkout — and is the only scope saves land in; searches span
/// it plus global.
pub struct MemoryResolver {
    store: Arc<dyn MemoryStore>,
    scope: Scope,
}

impl MemoryResolver {
    /// Tools over `store` acting in `scope`; a `Workspace` path is
    /// canonicalized, falling back to the input when it does not exist
    /// yet — the same handling as the coding agent's workspace.
    pub fn new(store: Arc<dyn MemoryStore>, scope: Scope) -> Self {
        Self {
            store,
            scope: canonicalized(scope),
        }
    }

    /// The active scope saves land in.
    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    /// The scopes a search spans: the active one, plus global — never
    /// another workspace's.
    fn search_scopes(&self) -> Vec<Scope> {
        match &self.scope {
            Scope::Global => vec![Scope::Global],
            active => vec![Scope::Global, active.clone()],
        }
    }

    /// Runs the named tool's arguments; `Err` is the in-band failure
    /// text. The one entry [`ToolResolver::resolve`] and the resident
    /// tier's registry shims both go through.
    pub fn execute(&self, name: &str, args: &Value) -> Result<String, String> {
        match name {
            MEMORY_SEARCH => self.search(args),
            MEMORY_SAVE => self.save(args),
            MEMORY_FORGET => self.forget(args),
            other => Err(format!("unknown tool: {other}")),
        }
    }

    fn search(&self, args: &Value) -> Result<String, String> {
        let mut query = MemoryQuery::new(arg_str(args, "query").unwrap_or_default())
            .with_scopes(self.search_scopes());
        if let Some(tags) = arg_strings(args, "tags") {
            query = query.with_tags(tags);
        }
        if let Some(limit) = args.get("limit").and_then(Value::as_u64) {
            query = query.with_limit(u32::try_from(limit).unwrap_or(u32::MAX));
        }
        let found = self.store.search(query).map_err(|e| e.to_string())?;
        if found.is_empty() {
            return Ok("no records matched".to_owned());
        }
        let rendered: Vec<String> = found.iter().map(render).collect();
        Ok(rendered.join("\n\n"))
    }

    fn save(&self, args: &Value) -> Result<String, String> {
        let body = arg_str(args, "body").ok_or("missing required argument: body")?;
        let tags = arg_strings(args, "tags").unwrap_or_default();
        let draft = match arg_str(args, "id") {
            Some(id) => {
                // Preserve the record's scope: correcting a global
                // memory from a workspace session must not migrate it.
                let existing =
                    self.store
                        .get(&id)
                        .map_err(|e| e.to_string())?
                        .ok_or_else(|| {
                            format!("no record with id {id:?}; save without an id to create one")
                        })?;
                MemoryDraft::new(existing.scope, body)
                    .with_tags(tags)
                    .with_id(id)
            }
            None => MemoryDraft::new(self.scope.clone(), body).with_tags(tags),
        };
        let record = self.store.put(draft).map_err(|e| e.to_string())?;
        Ok(format!("saved {}", record.id))
    }

    fn forget(&self, args: &Value) -> Result<String, String> {
        let id = arg_str(args, "id").ok_or("missing required argument: id")?;
        if self.store.forget(&id).map_err(|e| e.to_string())? {
            Ok(format!("forgot {id}"))
        } else {
            Err(format!("no record with id {id:?}"))
        }
    }
}

fn canonicalized(scope: Scope) -> Scope {
    match scope {
        Scope::Workspace(path) => Scope::Workspace(std::fs::canonicalize(&path).unwrap_or(path)),
        other => other,
    }
}

fn arg_str(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn arg_strings(args: &Value, key: &str) -> Option<Vec<String>> {
    args.get(key).and_then(Value::as_array).map(|items| {
        items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()
    })
}

/// One record as the model sees it: header line, then the body — the id
/// is what makes `memory_save`(update) and `memory_forget` usable.
fn render(record: &crate::record::MemoryRecord) -> String {
    let mut header = format!(
        "[{}] {} · updated {}",
        record.id,
        record.scope.label(),
        format_utc(record.updated_at)
    );
    if !record.tags.is_empty() {
        header.push_str(&format!(" · tags {}", record.tags.join(", ")));
    }
    format!("{header}\n{}", record.body)
}

fn search_spec() -> ToolSpec {
    ToolSpec::new(
        MEMORY_SEARCH,
        "Search durable memory that persists across sessions. Returns matching \
         records with their id, scope, tags, date, and body; an empty query \
         returns the most recent records. Search here before saving so \
         corrections update instead of duplicating.",
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Keywords to match against bodies and tags; an empty string lists recent records."
                },
                "tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Tags every result must carry."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum records to return, default 8."
                }
            },
            "required": ["query"]
        }),
    )
}

fn save_spec() -> ToolSpec {
    ToolSpec::new(
        MEMORY_SAVE,
        "Save one durable fact to memory — a preference, a fact about the \
         environment, a decision and its why. One record is one fact. To \
         correct or retag an existing record, pass its id from \
         memory_search instead of saving a near-copy.",
        json!({
            "type": "object",
            "properties": {
                "body": {
                    "type": "string",
                    "description": "The fact, as markdown."
                },
                "tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Freeform keywords; stored lowercased."
                },
                "id": {
                    "type": "string",
                    "description": "An existing record id to replace."
                }
            },
            "required": ["body"]
        }),
    )
}

fn forget_spec() -> ToolSpec {
    ToolSpec::new(
        MEMORY_FORGET,
        "Delete a memory record by its id (from memory_search).",
        json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "The record id to forget."
                }
            },
            "required": ["id"]
        }),
    )
}

#[async_trait]
impl ToolResolver for MemoryResolver {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![search_spec(), save_spec(), forget_spec()]
    }

    async fn resolve(&self, call: ToolCall, ctx: &ToolContext) -> ToolResult {
        if ctx.cancelled.load(Ordering::Relaxed) {
            return ToolResult::failure(call.id, "cancelled");
        }
        match self.execute(&call.name, &call.args) {
            Ok(text) => ToolResult::success(call.id, text),
            Err(text) => ToolResult::failure(call.id, text),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::volatile::VolatileStore;
    use std::path::PathBuf;

    fn resolver(scope: Scope) -> MemoryResolver {
        MemoryResolver::new(Arc::new(VolatileStore::new()), scope)
    }

    #[test]
    fn specs_carry_the_three_tools() {
        let names: Vec<String> = resolver(Scope::Global)
            .specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect();
        assert_eq!(
            names,
            vec![
                MEMORY_SEARCH.to_owned(),
                MEMORY_SAVE.to_owned(),
                MEMORY_FORGET.to_owned()
            ]
        );
    }

    #[test]
    fn a_workspace_path_is_canonicalized() {
        let missing = resolver(Scope::Workspace(PathBuf::from("nonexistent/../rel")));
        match missing.scope() {
            Scope::Workspace(path) => assert_eq!(path, &PathBuf::from("nonexistent/../rel")),
            other => panic!("expected workspace scope, got {other:?}"),
        }
        let existing = resolver(Scope::Workspace(PathBuf::from(".")));
        match existing.scope() {
            Scope::Workspace(path) => assert!(path.is_absolute(), "{path:?}"),
            other => panic!("expected workspace scope, got {other:?}"),
        }
    }

    #[test]
    fn save_then_search_round_trips_through_execute() {
        let resolver = resolver(Scope::Workspace(PathBuf::from("/w")));
        let saved = resolver
            .execute(
                MEMORY_SAVE,
                &json!({"body": "deploys go through just release", "tags": ["Deploy"]}),
            )
            .unwrap();
        let id = saved.strip_prefix("saved ").unwrap().to_owned();

        let found = resolver
            .execute(MEMORY_SEARCH, &json!({"query": "deploys"}))
            .unwrap();
        assert!(found.contains(&id), "{found}");
        assert!(found.contains("deploys go through just release"), "{found}");
        assert!(found.contains("tags deploy"), "{found}");
        assert!(found.contains("/w"), "{found}");
    }

    #[test]
    fn a_search_spans_global_and_active_never_another_workspace() {
        let store = Arc::new(VolatileStore::new());
        store
            .put(MemoryDraft::new(Scope::Global, "travels everywhere"))
            .unwrap();
        store
            .put(MemoryDraft::new(
                Scope::Workspace(PathBuf::from("/other")),
                "stays elsewhere",
            ))
            .unwrap();
        store
            .put(MemoryDraft::new(
                Scope::Workspace(PathBuf::from("/w")),
                "stays home",
            ))
            .unwrap();
        let resolver = MemoryResolver::new(store, Scope::Workspace(PathBuf::from("/w")));

        let found = resolver
            .execute(MEMORY_SEARCH, &json!({"query": ""}))
            .unwrap();
        assert!(found.contains("travels everywhere"), "{found}");
        assert!(found.contains("stays home"), "{found}");
        assert!(!found.contains("stays elsewhere"), "{found}");
    }

    #[test]
    fn an_update_preserves_the_records_scope() {
        let store = Arc::new(VolatileStore::new());
        let record = store
            .put(MemoryDraft::new(
                Scope::Global,
                "release channel is #deploys",
            ))
            .unwrap();
        // A workspace-scoped session corrects the global memory.
        let resolver = MemoryResolver::new(store.clone(), Scope::Workspace(PathBuf::from("/w")));
        resolver
            .execute(
                MEMORY_SAVE,
                &json!({
                    "body": "release channel is #releases",
                    "id": record.id,
                }),
            )
            .unwrap();

        let updated = store.get(&record.id).unwrap().unwrap();
        assert_eq!(updated.body, "release channel is #releases");
        assert_eq!(updated.scope, Scope::Global);
    }

    #[test]
    fn forget_reports_missing_ids_as_failures() {
        let resolver = resolver(Scope::Global);
        assert!(
            resolver
                .forget(&json!({"id": "19700101T000000-00000001"}))
                .is_err()
        );
    }

    #[test]
    fn missing_arguments_fail_in_band() {
        let resolver = resolver(Scope::Global);
        assert_eq!(
            resolver.save(&json!({})).unwrap_err(),
            "missing required argument: body"
        );
        assert_eq!(
            resolver.forget(&json!({})).unwrap_err(),
            "missing required argument: id"
        );
        assert_eq!(
            resolver.execute("memory_teleport", &json!({})).unwrap_err(),
            "unknown tool: memory_teleport"
        );
    }

    #[test]
    fn an_invalid_model_supplied_id_is_an_error_not_a_crash() {
        let resolver = resolver(Scope::Global);
        let err = resolver.forget(&json!({"id": "../escape"})).unwrap_err();
        assert!(err.contains("invalid id"), "{err}");
    }
}
