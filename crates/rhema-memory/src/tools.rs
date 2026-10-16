//! The memory tools: `memory_search`, `memory_save`, `memory_forget`,
//! `memory_retract`, `memory_verify`, `memory_due`.
//!
//! [`MemoryResolver`] is one [`ToolResolver`] offering all six — the
//! shape the step engine and a resolver chain compose. Its per-tool
//! operations also run individually through [`MemoryResolver::execute`],
//! so the resident tier's registry can shim them into named `Tool`s
//! without re-implementing argument handling. Every failure comes back
//! in-band — a missing argument, an unknown id — because the model reacts
//! to results; nothing here aborts a turn.
//!
//! Retrieval carries the doubt to the consumer: every rendered record
//! shows its age, its status, and a `STALE` marker when its decay
//! interval has lapsed. The pipeline never sanitises staleness away.
//!
//! The write path reports conflicts instead of deciding them: a save
//! whose subject or tags overlap an active record succeeds, and the
//! result lists the candidates with a request to declare each relation
//! (`supersedes`, `refines`, `unrelated`). Only a declared supersession
//! retires the older record — the engine never auto-supersedes on a
//! fuzzy match.
//!
//! This crate ships no prompt text: the consumer's system prompt carries
//! the bootstrap instruction, per framework spec §9.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::{Value, json};

use rhema_core::{ToolCall, ToolContext, ToolResolver, ToolResult, ToolSpec};

use crate::conflict::find_conflicts;
use crate::record::{
    Decay, MemoryDraft, MemoryRecord, Scope, Source, Status, VerifyHint, format_utc,
    normalize_subject, normalize_tags,
};
use crate::search::MemoryQuery;
use crate::store::MemoryStore;

/// The search tool's name — allowlists reference these.
pub const MEMORY_SEARCH: &str = "memory_search";
/// The save tool's name.
pub const MEMORY_SAVE: &str = "memory_save";
/// The forget tool's name.
pub const MEMORY_FORGET: &str = "memory_forget";
/// The retract tool's name.
pub const MEMORY_RETRACT: &str = "memory_retract";
/// The verify tool's name.
pub const MEMORY_VERIFY: &str = "memory_verify";
/// The due-for-review tool's name.
pub const MEMORY_DUE: &str = "memory_due";

/// The memory tools over one store, acting in one scope. The scope
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
            MEMORY_RETRACT => self.retract(args),
            MEMORY_VERIFY => self.verify(args),
            MEMORY_DUE => self.due(),
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
        if let Some(at) = u64_arg(args, "as_of")? {
            query = query.as_of(at);
        }
        if flag(args, "include_superseded") {
            query = query.include_superseded();
        }
        if flag(args, "fresh_only") {
            query = query.fresh_only();
        }
        // The instant the annotations are computed at — as_of makes the
        // whole reply a pure function of (query, t).
        let t = query.as_of.unwrap_or_else(now_millis);
        let found = self.store.search(query).map_err(|e| e.to_string())?;
        if found.is_empty() {
            return Ok("no records matched".to_owned());
        }
        let rendered: Vec<String> = found.iter().map(|record| render(record, t)).collect();
        Ok(rendered.join("\n\n"))
    }

    fn save(&self, args: &Value) -> Result<String, String> {
        let body = arg_str(args, "body").ok_or("missing required argument: body")?;
        let tags = arg_strings(args, "tags").unwrap_or_default();
        let supersedes = arg_strings(args, "supersedes").unwrap_or_default();
        let refines = arg_strings(args, "refines").unwrap_or_default();
        let unrelated = arg_strings(args, "unrelated").unwrap_or_default();

        let mut draft = match arg_str(args, "id") {
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
                    .with_tags(tags.clone())
                    .with_id(id)
            }
            None => MemoryDraft::new(self.scope.clone(), body).with_tags(tags.clone()),
        };
        if let Some(subject) = arg_str(args, "subject") {
            draft = draft.with_subject(normalize_subject(&subject).map_err(|e| e.to_string())?);
        }
        if let Some(at) = u64_arg(args, "valid_from")? {
            draft = draft.with_valid_from(at);
        }
        if let Some(at) = u64_arg(args, "valid_until")? {
            draft = draft.with_valid_until(at);
        }
        if let Some(decay) = parse_decay(args)? {
            draft = draft.with_decay(decay);
        }
        if let Some(source) = parse_source(args)? {
            draft = draft.with_source(source);
        }
        if let Some(hint) = parse_hint(args)? {
            draft = draft.with_verify_hint(hint);
        }

        // Overlap detection before the write, over the same scopes a
        // search would span. The engine reports; it never decides.
        let conflicts = find_conflicts(
            &*self.store,
            self.search_scopes(),
            draft.subject.as_deref(),
            &normalize_tags(&tags),
            draft.id.as_deref(),
        )
        .map_err(|e| e.to_string())?;
        let declared: Vec<&str> = supersedes
            .iter()
            .chain(&refines)
            .chain(&unrelated)
            .map(String::as_str)
            .collect();
        let undeclared: Vec<&crate::conflict::Conflict> = conflicts
            .iter()
            .filter(|conflict| !declared.contains(&conflict.record.id.as_str()))
            .collect();

        let record = self.store.put(draft).map_err(|e| e.to_string())?;

        let mut out = format!("saved {}", record.id);
        for id in &supersedes {
            match self.store.supersede(id, &record.id) {
                Ok(_) => out.push_str(&format!("\nsuperseded {id}")),
                Err(e) => out.push_str(&format!("\ncould not supersede {id}: {e}")),
            }
        }
        if !undeclared.is_empty() {
            out.push_str(&conflict_report(&record.id, &undeclared));
        }
        Ok(out)
    }

    fn forget(&self, args: &Value) -> Result<String, String> {
        let id = arg_str(args, "id").ok_or("missing required argument: id")?;
        if self.store.forget(&id).map_err(|e| e.to_string())? {
            Ok(format!("forgot {id}"))
        } else {
            Err(format!("no record with id {id:?}"))
        }
    }

    fn retract(&self, args: &Value) -> Result<String, String> {
        let id = arg_str(args, "id").ok_or("missing required argument: id")?;
        let why = arg_str(args, "why").unwrap_or_else(|| "no reason given".to_owned());
        self.store
            .retract(&id, &why)
            .map(|_| format!("retracted {id}"))
            .map_err(|e| e.to_string())
    }

    fn verify(&self, args: &Value) -> Result<String, String> {
        let id = arg_str(args, "id").ok_or("missing required argument: id")?;
        let record = self.store.verify(&id).map_err(|e| e.to_string())?;
        let at = format_utc(record.verified_at.unwrap_or_else(now_millis));
        match record.stale_at() {
            Some(next) => Ok(format!(
                "verified {id} at {at}; next review due {}",
                format_utc(next)
            )),
            None => Ok(format!("verified {id} at {at}; permanent, never due again")),
        }
    }

    fn due(&self) -> Result<String, String> {
        let items = self
            .store
            .due_for_review(now_millis())
            .map_err(|e| e.to_string())?;
        if items.is_empty() {
            return Ok("nothing is due for review".to_owned());
        }
        let mut out = String::new();
        for item in &items {
            let record = &item.record;
            out.push_str(&format!(
                "[{}] age {}d · tags {}\n{}\ncheck: {}\n\n",
                record.id,
                item.age_days,
                if record.tags.is_empty() {
                    "none".to_owned()
                } else {
                    record.tags.join(", ")
                },
                record.body,
                item.verify_hint
                    .as_ref()
                    .map(VerifyHint::describe)
                    .unwrap_or_else(|| "ask the human".to_owned()),
            ));
        }
        out.push_str(
            "re-check each record; confirm with memory_verify, or supersede \
             or retract it if it no longer holds.",
        );
        Ok(out)
    }
}

/// The warning a save returns when active records may be about the same
/// thing and the caller declared no relation for them. The write already
/// succeeded; the relations stay with the caller.
fn conflict_report(saved: &str, undeclared: &[&crate::conflict::Conflict]) -> String {
    let mut out =
        String::from("\n\npossible conflicts — active records that may be about the same thing:\n");
    for conflict in undeclared {
        let record = &conflict.record;
        let about = record
            .subject
            .clone()
            .unwrap_or_else(|| format!("tags {}", record.tags.join(", ")));
        out.push_str(&format!(
            "  [{}] {}{} · {}\n",
            record.id,
            if conflict.exact { "same subject " } else { "" },
            about,
            first_line(&record.body),
        ));
    }
    out.push_str(&format!(
        "if {saved} replaces one of these, save again with its own id and the \
         conflicting ids in `supersedes`; if it only refines them or is \
         unrelated, say so in `refines` or `unrelated` to clear the warning. \
         Nothing was superseded automatically.\n"
    ));
    out
}

fn first_line(body: &str) -> &str {
    body.lines().next().unwrap_or(body)
}

/// One record as the model sees it: header line, then the body. The id
/// is what makes every other tool usable; the annotations — age, status,
/// horizon, subject, provenance, staleness — carry the doubt through, so
/// a stale fact never looks as good as a fresh one.
fn render(record: &MemoryRecord, t: u64) -> String {
    let status = if record.is_active_at(t) {
        "active".to_owned()
    } else {
        match &record.status {
            Status::Superseded { by, at } => {
                format!("superseded by {by} at {}", format_utc(*at))
            }
            Status::Retracted { why, at } => {
                format!("retracted ({why}) at {}", format_utc(*at))
            }
            Status::Active => "active".to_owned(),
        }
    };
    let mut header = format!(
        "[{}] {} · updated {} · age {}d · {status}",
        record.id,
        record.scope.label(),
        format_utc(record.updated_at),
        record.age_days(t),
    );
    if let Some(until) = record.valid_until {
        header.push_str(&format!(" · until {}", format_utc(until)));
    }
    if let Some(subject) = &record.subject {
        header.push_str(&format!(" · subject {subject}"));
    }
    if !record.source.is_unknown() {
        header.push_str(&format!(" · {}", record.source.describe()));
    }
    if !record.tags.is_empty() {
        header.push_str(&format!(" · tags {}", record.tags.join(", ")));
    }
    if record.is_stale(t) {
        header.push_str(&format!(" · STALE {}d unverified", record.age_days(t)));
    }
    format!("{header}\n{}", record.body)
}

fn canonicalized(scope: Scope) -> Scope {
    match scope {
        Scope::Workspace(path) => Scope::Workspace(std::fs::canonicalize(&path).unwrap_or(path)),
        other => other,
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
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

fn flag(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// An optional integer argument, in millis since the Unix epoch.
fn u64_arg(args: &Value, key: &str) -> Result<Option<u64>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("{key} must be an integer of millis since the Unix epoch")),
    }
}

fn parse_decay(args: &Value) -> Result<Option<Decay>, String> {
    match arg_str(args, "decay") {
        None => Ok(None),
        Some(name) => match name.to_lowercase().as_str() {
            "permanent" => Ok(Some(Decay::Permanent)),
            "long" => Ok(Some(Decay::Long)),
            "volatile" => Ok(Some(Decay::Volatile)),
            other => Err(format!(
                "unknown decay {other:?}; use permanent, long, or volatile"
            )),
        },
    }
}

fn parse_source(args: &Value) -> Result<Option<Source>, String> {
    let Some(value) = args.get("source") else {
        return Ok(None);
    };
    let named = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                format!(
                    "source {kind:?} needs `{key}`",
                    kind = { value.get("kind").and_then(Value::as_str).unwrap_or("?") }
                )
            })
    };
    match value.get("kind").and_then(Value::as_str) {
        Some("told") => Ok(Some(Source::Told { by: named("by")? })),
        Some("observed") => Ok(Some(Source::Observed { how: named("how")? })),
        Some("inferred") => Ok(Some(Source::Inferred {
            from: named("from")?,
        })),
        other => Err(format!(
            "source needs a kind: told, observed, or inferred (got {other:?})"
        )),
    }
}

fn parse_hint(args: &Value) -> Result<Option<VerifyHint>, String> {
    let Some(value) = args.get("verify_hint") else {
        return Ok(None);
    };
    let named = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                format!(
                    "verify_hint {kind:?} needs `{key}`",
                    kind = { value.get("kind").and_then(Value::as_str).unwrap_or("?") }
                )
            })
    };
    match value.get("kind").and_then(Value::as_str) {
        Some("command") => Ok(Some(VerifyHint::Command { run: named("run")? })),
        Some("url") => Ok(Some(VerifyHint::Url {
            fetch: named("fetch")?,
        })),
        Some("file") => Ok(Some(VerifyHint::File {
            path: PathBuf::from(named("path")?),
        })),
        Some("manual") => Ok(Some(VerifyHint::Manual)),
        other => Err(format!(
            "verify_hint needs a kind: command, url, file, or manual (got {other:?})"
        )),
    }
}

fn search_spec() -> ToolSpec {
    ToolSpec::new(
        MEMORY_SEARCH,
        "Search durable memory that persists across sessions. Returns matching \
         records with their id, scope, tags, age, status, and body — stale \
         records are marked STALE, so weigh them accordingly. An empty query \
         returns the most recent records. Search here before saving so \
         corrections supersede instead of duplicating.",
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
                },
                "as_of": {
                    "type": "integer",
                    "description": "Millis since the Unix epoch: search as the store stood at that instant, for replay and debugging."
                },
                "include_superseded": {
                    "type": "boolean",
                    "description": "Also return superseded and retracted records; they rank below active ones."
                },
                "fresh_only": {
                    "type": "boolean",
                    "description": "Drop records past their review interval."
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
         correct an existing record, save a new one and pass the old ids in \
         `supersedes` — the old records stay, marked replaced. When a save \
         reports possible conflicts, declare each relation on a follow-up \
         save (`supersedes`, `refines`, or `unrelated`). Give `verify_hint` \
         whenever there is a way to re-check the fact without asking a \
         human, and `decay` (permanent: never, long: a year, volatile: a \
         month) when the tags do not already say how soon to doubt it.",
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
                    "description": "Freeform keywords; stored lowercased. They also set the review interval unless `decay` says otherwise."
                },
                "id": {
                    "type": "string",
                    "description": "An existing record id to replace in place."
                },
                "subject": {
                    "type": "string",
                    "description": "What the fact is about, as a namespaced key like home/accommodation; records sharing a subject are flagged as possible conflicts."
                },
                "decay": {
                    "type": "string",
                    "enum": ["permanent", "long", "volatile"],
                    "description": "How soon to re-check the fact. Derived from the tags when omitted."
                },
                "valid_from": {
                    "type": "integer",
                    "description": "Millis since the Unix epoch when the fact starts to hold; default: when saved."
                },
                "valid_until": {
                    "type": "integer",
                    "description": "Millis since the Unix epoch when the fact is planned to stop holding (a quote's expiry, a lease's end). Past it the record is due for review, not deleted."
                },
                "source": {
                    "type": "object",
                    "description": "Where the fact came from: {kind: told, by}, {kind: observed, how}, or {kind: inferred, from}."
                },
                "verify_hint": {
                    "type": "object",
                    "description": "How to re-check without asking a human: {kind: command, run}, {kind: url, fetch}, {kind: file, path}, or {kind: manual}."
                },
                "supersedes": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Ids of records this fact replaces; they are marked superseded, never deleted."
                },
                "refines": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Ids of conflicting records this fact narrows or extends, acknowledging the overlap."
                },
                "unrelated": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Ids of flagged records that only resemble this fact; acknowledges the conflict warning."
                }
            },
            "required": ["body"]
        }),
    )
}

fn forget_spec() -> ToolSpec {
    ToolSpec::new(
        MEMORY_FORGET,
        "Delete a memory record by its id (from memory_search). For secrets \
         and junk only: a fact that stopped being true should be \
         memory_retract'ed or superseded through memory_save, so the \
         history of what was believed survives.",
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

fn retract_spec() -> ToolSpec {
    ToolSpec::new(
        MEMORY_RETRACT,
        "Mark a memory record as no longer true, with the reason. The record \
         stays readable — marked retracted — so the history of what was \
         believed survives. Use this when a fact was simply wrong, not \
         replaced by a newer fact (then memory_save with `supersedes`).",
        json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "The record id to retract."
                },
                "why": {
                    "type": "string",
                    "description": "Why it no longer holds."
                }
            },
            "required": ["id"]
        }),
    )
}

fn verify_spec() -> ToolSpec {
    ToolSpec::new(
        MEMORY_VERIFY,
        "Record that a memory fact was re-checked and still holds: its \
         verified date moves to now and it stops appearing as due for \
         review. Run the check from its verify hint first (a command, a \
         page, a file); when only a human can confirm, ask and verify with \
         their answer.",
        json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "The record id that was re-checked."
                }
            },
            "required": ["id"]
        }),
    )
}

fn due_spec() -> ToolSpec {
    ToolSpec::new(
        MEMORY_DUE,
        "List the memory records due for review: stale past their review \
         interval, or past their validity horizon. Each comes with its age \
         and how to re-check it. Work through the batch: run the check, \
         then memory_verify what still holds, memory_save with `supersedes` \
         what changed, memory_retract what was wrong.",
        json!({ "type": "object", "properties": {} }),
    )
}

#[async_trait]
impl ToolResolver for MemoryResolver {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![
            search_spec(),
            save_spec(),
            forget_spec(),
            retract_spec(),
            verify_spec(),
            due_spec(),
        ]
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

    fn resolver(scope: Scope) -> MemoryResolver {
        MemoryResolver::new(Arc::new(VolatileStore::new()), scope)
    }

    fn resolver_with(store: Arc<VolatileStore>, scope: Scope) -> MemoryResolver {
        MemoryResolver::new(store, scope)
    }

    #[test]
    fn specs_carry_the_six_tools() {
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
                MEMORY_FORGET.to_owned(),
                MEMORY_RETRACT.to_owned(),
                MEMORY_VERIFY.to_owned(),
                MEMORY_DUE.to_owned(),
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
        assert!(found.contains("· age 0d"), "{found}");
        assert!(found.contains("· active"), "{found}");
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
        let resolver = resolver_with(store, Scope::Workspace(PathBuf::from("/w")));

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
        let resolver = resolver_with(store.clone(), Scope::Workspace(PathBuf::from("/w")));
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
        assert_eq!(updated.verified_at, None, "new wording, no verification");
    }

    #[test]
    fn a_conflicting_save_warns_and_a_declaration_supersedes() {
        let resolver = resolver(Scope::Global);
        let first = resolver
            .execute(
                MEMORY_SAVE,
                &json!({
                    "body": "house search: looking around Gragnague",
                    "tags": ["home"],
                    "subject": "home/accommodation",
                }),
            )
            .unwrap();
        let old = first.strip_prefix("saved ").unwrap().to_owned();

        // The newer fact about the same subject saves, and the conflict
        // comes back as a warning — nothing was retired behind the
        // caller's back.
        let second = resolver
            .execute(
                MEMORY_SAVE,
                &json!({
                    "body": "house bought: Toulouse",
                    "tags": ["home"],
                    "subject": "home/accommodation",
                }),
            )
            .unwrap();
        let new = second.split_whitespace().nth(1).unwrap().to_owned();
        assert!(second.contains("possible conflicts"), "{second}");
        assert!(second.contains(&old), "{second}");
        assert!(second.contains("Nothing was superseded automatically"));

        // Declaring the relation on a follow-up save retires the old
        // record without erasing it.
        let declared = resolver
            .execute(
                MEMORY_SAVE,
                &json!({
                    "body": "house bought: Toulouse",
                    "id": new,
                    "supersedes": [old],
                }),
            )
            .unwrap();
        assert!(
            declared.contains(&format!("superseded {old}")),
            "{declared}"
        );
        assert!(!declared.contains("possible conflicts"), "{declared}");

        let found = resolver
            .execute(MEMORY_SEARCH, &json!({"query": "house"}))
            .unwrap();
        assert!(found.contains("house bought: Toulouse"), "{found}");
        assert!(!found.contains("Gragnague"), "dead record hidden: {found}");

        let with_dead = resolver
            .execute(
                MEMORY_SEARCH,
                &json!({"query": "house", "include_superseded": true}),
            )
            .unwrap();
        assert!(with_dead.contains("Gragnague"), "{with_dead}");
        assert!(with_dead.contains("superseded by"), "{with_dead}");
        assert!(
            with_dead.find("house bought").unwrap() < with_dead.find("Gragnague").unwrap(),
            "the live record outranks the dead one: {with_dead}"
        );
    }

    #[test]
    fn a_fuzzy_match_does_not_auto_supersede_and_declarations_silence() {
        let resolver = resolver(Scope::Global);
        let first = resolver
            .execute(
                MEMORY_SAVE,
                &json!({"body": "uses the staging cluster", "tags": ["staging", "cluster"]}),
            )
            .unwrap();
        let old = first.strip_prefix("saved ").unwrap().to_owned();

        // Declaring the fuzzy conflict as a refinement clears the warning
        // without touching the older record.
        let refined = resolver
            .execute(
                MEMORY_SAVE,
                &json!({
                    "body": "uses the staging cluster in eu-west",
                    "tags": ["staging", "cluster"],
                    "refines": [old],
                }),
            )
            .unwrap();
        let refined_id = refined.strip_prefix("saved ").unwrap().to_owned();
        assert!(!refined.contains("possible conflicts"), "{refined}");

        // An undeclared fuzzy match warns; the write still succeeds.
        let again = resolver
            .execute(
                MEMORY_SAVE,
                &json!({
                    "body": "uses the staging cluster in eu-west, new creds",
                    "tags": ["staging", "cluster"],
                }),
            )
            .unwrap();
        let third = again.split_whitespace().nth(1).unwrap().to_owned();
        assert!(again.contains("possible conflicts"), "fuzzy match warns");
        assert!(
            again.contains(&old) && again.contains(&refined_id),
            "{again}"
        );
        assert!(again.contains("Nothing was superseded automatically"));

        // Declaring each relation on a follow-up save clears it.
        let acknowledged = resolver
            .execute(
                MEMORY_SAVE,
                &json!({
                    "body": "uses the staging cluster in eu-west, new creds",
                    "id": third,
                    "tags": ["staging", "cluster"],
                    "refines": [old],
                    "unrelated": [refined_id],
                }),
            )
            .unwrap();
        assert!(
            !acknowledged.contains("possible conflicts"),
            "declaring every relation clears the warning: {acknowledged}"
        );
    }

    #[test]
    fn temporal_arguments_flow_through_save() {
        let resolver = resolver(Scope::Global);
        let day = 86_400_000u64;
        let now = now_millis();
        let saved = resolver
            .execute(
                MEMORY_SAVE,
                &json!({
                    "body": "the quote expires end of november",
                    "tags": ["quote"],
                    "decay": "volatile",
                    "valid_until": now + 90 * day,
                    "source": {"kind": "told", "by": "the landlord"},
                    "verify_hint": {"kind": "manual"},
                }),
            )
            .unwrap();
        let id = saved.strip_prefix("saved ").unwrap().to_owned();
        let found = resolver
            .execute(MEMORY_SEARCH, &json!({"query": "quote"}))
            .unwrap();
        assert!(
            found.contains(&format!("until {}", format_utc(now + 90 * day))),
            "{found}"
        );
        assert!(found.contains("told by the landlord"), "{found}");
        assert!(!found.contains("STALE"), "{found}");

        // Travel past the 30-day decay: the annotation carries the doubt.
        let stale = resolver
            .execute(
                MEMORY_SEARCH,
                &json!({"query": "quote", "as_of": now + 40 * day}),
            )
            .unwrap();
        assert!(stale.contains("STALE"), "{stale}");

        let due = resolver.execute(MEMORY_DUE, &json!({})).unwrap();
        assert!(
            due.contains("nothing is due for review"),
            "fresh record, future horizon: {due}"
        );
        assert!(resolver.verify(&json!({"id": id})).is_ok());

        let retracted = resolver
            .execute(
                MEMORY_RETRACT,
                &json!({"id": id, "why": "the quote was withdrawn"}),
            )
            .unwrap();
        assert!(retracted.contains("retracted"), "{retracted}");
        let found = resolver
            .execute(MEMORY_SEARCH, &json!({"query": "quote"}))
            .unwrap();
        assert_eq!(found, "no records matched");
    }

    #[test]
    fn a_past_horizon_hides_from_search_but_reports_as_due() {
        let store = Arc::new(VolatileStore::new());
        let day = 86_400_000u64;
        // A second ahead of the store's clock, so both records are
        // squarely "created" before every instant the test reasons about.
        let now = now_millis() + 1_000;
        store
            .put(
                MemoryDraft::new(Scope::Global, "the lease runs out")
                    .with_tags(["home"])
                    .with_decay(Decay::Permanent)
                    .with_valid_until(now + day),
            )
            .unwrap();
        store
            .put(
                MemoryDraft::new(Scope::Global, "the quote has lapsed")
                    .with_tags(["home"])
                    .with_decay(Decay::Permanent)
                    .with_valid_until(now - day),
            )
            .unwrap();
        let resolver = resolver_with(store.clone(), Scope::Global);

        // While the horizon holds the record answers; the lapsed quote
        // already hides.
        let lease = resolver
            .execute(MEMORY_SEARCH, &json!({"query": "lease"}))
            .unwrap();
        assert!(lease.contains("the lease runs out"), "{lease}");
        let quote = resolver
            .execute(MEMORY_SEARCH, &json!({"query": "quote"}))
            .unwrap();
        assert_eq!(quote, "no records matched", "past the horizon");
        // Widening the status rule does not resurrect it: the window
        // rule still applies. Only the sweep reaches it.
        let widened = resolver
            .execute(
                MEMORY_SEARCH,
                &json!({"query": "quote", "include_superseded": true}),
            )
            .unwrap();
        assert_eq!(widened, "no records matched");

        // A future horizon eventually closes too.
        let later = resolver
            .execute(
                MEMORY_SEARCH,
                &json!({"query": "lease", "as_of": now + 2 * day}),
            )
            .unwrap();
        assert_eq!(later, "no records matched");

        // The sweep is the way back: a passed horizon is due, not dead —
        // the lapsed quote at the tool's now, both at a later instant.
        let due_now = resolver.execute(MEMORY_DUE, &json!({})).unwrap();
        assert!(due_now.contains("the quote has lapsed"), "{due_now}");
        assert!(!due_now.contains("the lease runs out"), "{due_now}");
        let due_later = store.due_for_review(now + 2 * day).unwrap();
        let bodies: Vec<&str> = due_later
            .iter()
            .map(|item| item.record.body.as_str())
            .collect();
        assert_eq!(bodies, ["the quote has lapsed", "the lease runs out"]);
    }

    #[test]
    fn the_due_tool_renders_hints() {
        let resolver = resolver(Scope::Global);
        assert!(
            resolver
                .execute(MEMORY_DUE, &json!({}))
                .unwrap()
                .contains("nothing is due")
        );
        resolver
            .execute(
                MEMORY_SAVE,
                &json!({
                    "body": "the backup cron runs on host pin",
                    "tags": ["infrastructure"],
                    "verify_hint": {"kind": "command", "run": "ssh pin systemctl status backup"},
                }),
            )
            .unwrap();
        // Not due yet — travel past the 30-day tag decay.
        let stale_view = resolver
            .execute(
                MEMORY_SEARCH,
                &json!({"query": "backup", "as_of": now_millis() + 40 * 86_400_000u64}),
            )
            .unwrap();
        assert!(stale_view.contains("STALE"), "{stale_view}");
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
            resolver.retract(&json!({})).unwrap_err(),
            "missing required argument: id"
        );
        assert_eq!(
            resolver.verify(&json!({})).unwrap_err(),
            "missing required argument: id"
        );
        assert_eq!(
            resolver.execute("memory_teleport", &json!({})).unwrap_err(),
            "unknown tool: memory_teleport"
        );
    }

    #[test]
    fn malformed_arguments_fail_in_band() {
        let resolver = resolver(Scope::Global);
        assert!(
            resolver
                .save(&json!({"body": "x", "subject": "Has Spaces"}))
                .unwrap_err()
                .contains("invalid subject")
        );
        assert!(
            resolver
                .save(&json!({"body": "x", "decay": "daily"}))
                .unwrap_err()
                .contains("unknown decay")
        );
        assert!(
            resolver
                .save(&json!({"body": "x", "source": {"kind": "told"}}))
                .unwrap_err()
                .contains("needs `by`")
        );
        assert!(
            resolver
                .save(&json!({"body": "x", "verify_hint": {"kind": "smoke"}}))
                .unwrap_err()
                .contains("needs a kind")
        );
        assert!(
            resolver
                .save(&json!({"body": "x", "valid_until": "soon"}))
                .unwrap_err()
                .contains("integer")
        );
    }

    #[test]
    fn an_invalid_model_supplied_id_is_an_error_not_a_crash() {
        let resolver = resolver(Scope::Global);
        let err = resolver.forget(&json!({"id": "../escape"})).unwrap_err();
        assert!(err.contains("invalid id"), "{err}");
    }
}
