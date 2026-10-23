# rhema-tools — the claw's model-facing tools as bash skills + thin clients

*One project, independent of the claw: the resident's data tools become small
executables and the markdown that teaches them, speaking the daemon's existing
HTTP surface through a scoped agent credential.*

## 0. Scope

This spec moves the claw's model-facing data tools out of the daemon binary and
its system prompt. After it, the model's view of tasks, watches, identity,
notifications, and the phone is: `bash`, a handful of short-named executables
on `PATH`, and one markdown skill per domain. The daemon keeps the state, the
schedulers, and the sources; the tools project owns everything the model
touches.

Two consumers change:

- the model, which stops seeing sixteen tool schemas on every step of every
  session and starts composing tool calls in the shell;
- the claw, which stops being the only place a tool can live;
- tool authors — users or the agent itself — who can add a capability without
  touching rhema at all (§8).

Non-goals v1: extracting the server side (stores, schedulers, sources) out of
the claw; extracting memory, attachments, or the bootstrap prompt; any
transport other than the daemon's existing HTTP surface; a daemon-side timer
service (tier-1 tools use system schedulers; §8).

## 1. Problem statement

The claw's tools are native: one `TOOL.yaml` manifest and one Rust builtin per
tool, resolved through the `ClawExtension` chain, rendered as a JSON-schema
bullet into every `SYSTEM.md` render. Sixteen of them today, and the list
grows with every feature.

Four costs, all structural:

1. **Context.** Every session pays the schema of every tool on every step, the
   resident's whole life long, whether or not the turn touches them.
2. **Round trips.** "Unwatch every archived repo" is one native call per repo —
   one model step each. The model cannot loop, filter, or pipeline across tool
   results; only the harness can.
3. **Coupling.** A new tool is a manifest, a builtin, an extension-struct line,
   and a registry test — all inside `rhema-claw`. Nothing tool-shaped can be
   written, versioned, or shipped without the daemon crate.
4. **Reuse.** A second agent (a forge worker, a phone-side helper) cannot share
   the tools without depending on the daemon's binary.

That is the signal that the tool surface is a missing boundary, not a missing
feature list. The boundary is a wire contract.

## 2. Design goals

- The model-facing surface is code: `bash` + executables + skills. Skills are
  read when a domain is relevant; `--help` is always current.
- The tools project compiles with zero `rhema-claw` dependencies. Independence
  is a dependency arrow, not a directory layout, and CI enforces the arrow.
- The daemon's state stays in the daemon. Clients hold no stores, no
  schedulers, no caches; they are request printers.
- One trust boundary, made explicit and enforced: a scoped agent credential
  that cannot answer consents, forge chats, or ingest events.
- Incremental: each domain migrates alone, and its native tool stays until its
  client ships.

## 3. The shape

Three pieces:

1. **Skills.** One markdown file per domain, shipped next to its binary and
   installed into the global skills directory that
   `rhema_agent_tools::skills::discover` already reads. The skill carries the
   examples; the binary carries the behavior; they never drift apart because
   they are versioned and installed together.
2. **Thin clients.** One short-named executable per domain: `notify`, `tasks`,
   `watches`, `identity`, `phone`. Parse arguments, make one or a few HTTP
   calls, print JSON, exit.
3. **The contract.** The daemon's `/v1` HTTP surface — the same routes the web
   UI and the phone client already speak. The contract is the tools project's
   entire knowledge of the claw.

One call, end to end — `schedule_task` becoming `tasks add`:

1. The user: "check CI hourly for me."
2. The model runs `bash`: `tasks add --every 3600 "check CI" --json`.
3. The `bash` tool spawns `tasks` inside the sandbox.
4. `tasks` reads `RHEMA_URL` and `RHEMA_TOKEN` from its environment and does
   `POST /v1/tasks` with the JSON body.
5. The daemon's route writes the same `TaskStore` row the native builtin used
   to write. The scheduler picks it up exactly as before — it never notices
   the row arrived over HTTP.
6. `tasks` prints `{"id":"t-7"}` and exits 0. The model reads stdout and
   replies.

```
model turn
    | bash: tasks add --every 3600 "check CI"
    v
+----------------+  POST /v1/tasks       +----------------------+
| tools project  |  Authorization: agent | claw daemon          |
| tasks, notify, | --------------------> |  route -> TaskStore  |
| watches, ...   | <-------------------- |  (state lives HERE)  |
| (stateless)    |  {"id":"t-7"}         |  scheduler fires     |
+----------------+                       +----------------------+
```

"HTTP client" is the role, not a framework: the client is to the daemon what
`gh` is to GitHub. The state cannot live in the client — it is a different,
short-lived process, possibly inside a sandbox container — so it forwards, and
the independence is precisely that forwarding: no `rhema-claw` import exists
anywhere in the tools project.

## 4. The wire contract

Transport: the daemon's existing axum router (`crates/rhema-claw/src/http.rs`)
— loopback plus bearer today, an env-provisioned base URL when a deployment
puts the sandbox on another host.

Routes that already serve clients as-is: `GET/POST /v1/watches`,
`DELETE /v1/watches/{forge}/{repo}`, `GET/POST/DELETE /v1/calendars`
(removal takes `/v1/calendars/{name}`; the reminders source reads the
calendar table fresh every pass, so a route write is live without a
restart), `GET/POST /v1/accounts`, `DELETE /v1/accounts/{id}`,
`GET /v1/notifications`, `POST /v1/notifications/{id}/ack`,
`GET /v1/phones`.

Watches and calendars are the pattern for every data domain the
daemon reconfigures at runtime: the domain lives in a SQLite table —
not in `claw.yaml` — the routes are the one write door, validation
runs at the door (an invalid watch or calendar is refused loudly
wherever it enters), and the config file carries only what boot
needs. A config still carrying the retired `watches:` or
`calendars:` keys is refused at parse time, not silently ignored.

Routes the extraction adds, because the operation exists today only as an
in-process tool: `POST /v1/notifications` (the push), `GET/POST /v1/tasks`,
`DELETE /v1/tasks/{id}`, `POST /v1/phones/{device}/commands`. Promoting a tool
to a route is not new work twice — the web UI and phone gain the same
operations the model does.

Conventions, fixed once:

- Every client prints JSON: one object per call, one object per line for
  lists. `--json` exists as a no-op flag for readability of examples.
- Exit codes: `0` ok, `1` usage (clap), `2` daemon unreachable, `3` denied.
- On failure the last stdout line is a single JSON object
  `{"error": "..."}` — the model parses one shape, always.
- Version: every request carries `X-Rhema-Contract: 1`; a daemon speaking a
  different contract answers `409` with that same error shape. A stale
  installed binary fails loudly on first use, with no discovery round trip.

## 5. The agent credential

Today one `token: Option<String>` guards all of `/v1`, and the model never
holds it — tools run in-process. Bash-accessible clients change who holds a
credential, and the current token is the wrong one to hold:
`POST /v1/consents/{id}` answers a consent, and an agent that can answer
consents can answer its own `ask_secret`.

Two token classes, checked per route in the router:

- the **human token**, unchanged, for the web UI, the phone, and the scripts a
  user runs themselves;
- the **agent token** (`agent_token` in the claw config), provisioned into the
  agent environment as `RHEMA_URL` + `RHEMA_TOKEN`, allowed on exactly the
  data routes the clients speak (§4), denied on consent resolve/deny, session
  and message creation, `phones_forget`, and `/v1/ingest`.

Boot refuses to start extracted-tool mode without `agent_token` configured —
the failure belongs at boot, where the operator is looking, not mid-turn,
where the model is. This is the step that turns the README's "the
bearer-gated surface plus the watch list are the trust boundary" from a
sentence into enforced code.

## 6. What stays native

- `bash` — the substrate the whole design stands on.
- `ask_secret` — the consent round-trip waits inside the tool call, woken by
  the hub, its answer arriving through routes the agent credential must never
  reach (§5). Making it a client would hand the model its own approval path.
- `coding_agent` — spawns a whole coding session, inherits the parent turn's
  approval hook, answers with a job id. Orchestration, not data access.
- `zoom` — reads the in-process transcript mirror; no daemon state behind it.
- memory and `attach_file` — native in v1. Memory is the first later-extraction
  candidate: its store is already behind a `MemoryStore` contract, so a
  `/v1/memory` route family plus a client is a shape, not a research project.

## 7. What moves

| current tool | client | route | wave |
|---|---|---|---|
| `list_watches` / `watch_repo` / `unwatch_repo` | `watches ls/add/rm` | `/v1/watches` (exists) | 1 |
| `notify` | `notify push` | `POST /v1/notifications` (new) | 1 |
| `schedule_task` / `cancel_task` / `list_tasks` | `tasks add/rm/ls` | `/v1/tasks` (new) | 2 |
| `identity_save` / `identity_set` / `identity_list` / `identity_forget` | `identity save/set/ls/forget` | `/v1/accounts` (exists) | 3 |
| `phone` | `phone run` | `POST /v1/phones/{device}/commands` (new) | 3 |
| `bootstrap_save` | — | `PUT /v1/agent` (exists) | later candidate |

Wave 1 is watches plus notify: watches has complete routes, notify has the
smallest state, and between them they exercise the whole path — scoped
credential, `PATH` provisioning, skill discovery, contract versioning —
before anything harder follows it.

## 8. Self-contained tools

The client extraction frees the tools from the daemon's binary. A second
question follows: can a tool exist that was never part of rhema at all —
written by a user, or by the agent itself mid-session, without a fork or a
patch? Yes — and it needs no new API, because the generic API is the surface
this spec already builds on. Three primitives, all HTTP, all daemon-side:

- **`POST /v1/ingest`** — wake the agent with an event. The route takes
  `{source, key, payload}`, registers unknown source names lazily on the RSD
  dispatcher, and rejects the daemon's reserved source names. A third-party
  tool's events therefore get the same guarantees the built-in integrations
  get: at most one live handler per resource key, races queued into the live
  inbox, crash recovery by lease.
- **`POST /v1/notifications`** (§4) — reach the user's devices.
- **`GET /v1/sessions/{key}/events`** — read back what the agent said.

Plus two non-daemon primitives: **files** (the tool keeps its own state in its
own directory) and **system schedulers** (cron, launchd, schtasks — the tool
installs its own trigger).

### Anatomy

A self-contained tool is four files and no rhema code:

- an executable, dropped in a bin directory the daemon puts on the agent's
  `PATH`;
- a skill markdown next to it, installed into the global skills directory;
- its own state directory under the state root (one subdir per tool, owned by
  the tool, `0600` for anything credential-shaped);
- optionally one scheduler entry it installed itself.

### The watch table, from outside

Even the case that motivated native integration works through the front door.
A third-party CI watcher: a cron entry runs its poller every five minutes; the
poller calls the forge's API itself, with a token kept in its own state
directory; it triages what is new against its own last-seen file; and for each
new item it does
`POST /v1/ingest {source: "ciwatch", key: "github/acme/widgets#123", payload: {...}}`.
The dispatcher admits the key, the resident gets a turn, and a second event
racing a live handler queues instead of doubling the work. The tool is a
script; nothing in rhema changed.

### The two tiers

- **Tier 1 — self-contained.** Executable + skill + state dir + scheduler.
  Writable by anyone, including the agent. No daemon custody of anything.
- **Tier 2 — native extension.** The `ClawExtension` seam stays for what
  genuinely needs the daemon: secret custody (rhema-identity's use-never-read
  contract — a tier-1 tool cannot receive a secret through the consent
  round-trip, whose answers land in the identity store, not in the tool),
  tick-rate and webhook latency (cron is minute-grained, has no catch-up, and
  the task table's re-arm semantics are the reference for what a daemon-side
  timer would have to do), and deep hub integration (auto-notify on non-chat
  replies, the phone command loop). The forge and calendar integrations live
  here and are not demoted — they are just no longer the only way.

### What this forbids

No plugin SDK, no tool manifest registry, no new IPC protocol. The platform is
HTTP + files + `PATH` — the same surface the model itself uses. The only
daemon additions this spec asks for are the §4 routes and the directory
conventions. A `toolctl` helper in the tools project (`toolctl new`,
`toolctl schedule` wrapping crontab/launchd/schtasks) is convenience, not
contract.

### The agent as tool author

Tier 1 is writable from inside a session: the model writes a script into the
bin dir, a skill into the skills dir, a crontab line — and the resident has
grown itself a capability. That is the codemode endgame, and it costs only the
conventions. One honest line comes with it: such a tool runs with the agent's
privileges and persists past the session through its scheduler entry. The
trust boundary is the shell, exactly as it already is, and `ENVIRONMENT.md`
says what is installed.

## 9. Provisioning and discovery

The daemon installs the clients (a `bin` directory it controls, added to the
agent's `PATH`) and their skills (the global skills directory). Boot-time
`ENVIRONMENT.md` already inventories what is on `PATH`, so a missing binary is
visible before the first model call. A client that cannot reach the daemon
prints which environment variable is missing or wrong — the model reports it
instead of retrying.

## 10. Approvals, replay, crash

A `bash` call that runs three clients is one approval unit and one
commit-before-execute unit — coarser than three native calls, and already true
of any compound shell command. Resident sessions auto-approve the shell
anyway (the daemon says so at startup), so nothing is lost in practice. In
exchange, mutating verbs are upsert-shaped (`add`, `rm`, `save`), so a replay
after a mid-command crash converges instead of duplicating. The manifests'
`replay_safety: safe` moves into the contract: `GET` verbs are safe,
`POST`/`DELETE` are not, and the exit code says which one ran.

## 11. Layout and the dependency arrow

New project — `rhema-tools`:

- `rhema-tools-contract` — route constants, JSON payload types, a mock router,
  and the conformance suite. Depends on nothing.
- one crate per domain — the binary and its skills directory.
- one thin shared client crate — base-URL/token env handling, the HTTP calls,
  the error printing every binary shares.

Dependency arrows, both enforced in CI: the claw depends on
`rhema-tools-contract` (to run the conformance suite against its real router)
and never on the domain crates; the tools project depends on nothing of
rhema-claw.

`TOOL.yaml` is retired for extracted domains. The manifest existed to make a
Rust builtin model-facing; a clap surface plus a skill markdown is that now.
The old guarantee — every manifest loads against its builtin — becomes two:
every skill's examples parse against the binary (`--help`/dry-run), and every
route the contract names is exercised by the conformance suite.

## 12. Migration

1. The scoped agent credential lands in the claw: token classes, route
   middleware, boot check. No model-facing change.
2. The contract crate lands; the claw grows the new routes
   (`POST /v1/notifications`, the `/v1/tasks` family) and passes conformance.
3. Wave 1: `watches` and `notify` clients + skills; the agent environment gets
   `PATH`, skills, `RHEMA_URL`/`RHEMA_TOKEN`, and the tier-1 state root; the
   two domains' manifests and builtins stop rendering and are deleted.
4. Waves 2–3 repeat step 3 per domain: routes if new, client + skill, delete
   manifest + builtin + extension line.
5. When the last wave lands, the claw's `SYSTEM.md` tools section lists
   `bash`, `zoom`, `ask_secret`, `coding_agent`, memory — plus a two-line
   pointer to the installed skills.

## 13. How we know it works

- Conformance: the contract's suite runs against the claw's real router and
  against the contract's mock router, with the same results.
- Arrow test: the tools workspace builds with `rhema-claw` absent.
- End to end: a mock-provider turn whose scripted model runs `tasks add`
  through `bash`; the daemon's scheduler fires the task; the phone poll sees
  the notification; the agent token's `POST /v1/consents/{id}` answers `403`.
- Skill lint: every fenced shell example in every skill parses (usage check /
  dry-run), so doc rot fails a test instead of fooling a model.
- Tier-1 end to end: a test drops a two-line shell script and a skill into the
  provisioned dirs, runs it, and its `POST /v1/ingest` lands as a turn;
  `POST /v1/ingest` under a reserved source name answers `400`.

## 14. Explicitly not decided here

- Binary naming: short domain names (`tasks`) versus a prefix
  (`rhema-tasks`). Short names read best in skills; switch if a collision
  appears on a real system.
- Whether the project starts as a repo-local directory (the `integrations/`
  shape) and extracts to its own repo later, or is born separate.
- Extraction waves for memory, `attach_file`, and `bootstrap_save`.
- Transports beyond bearer-over-HTTP: a unix-socket listener, mTLS for a
  remote daemon, a quota on client calls per turn.
- Whether `toolctl` (scaffolder + scheduler wrapper) ships in v1 or stays
  documentation.
- A daemon-side timer service (`POST /v1/timers`) for cronless hosts — the
  task table's fire-guards are the reference semantics. Not v1.
- Whether tier-1 tools ever get a consent-shaped way to receive secrets. V1
  answer: no — users place credentials in the tool's state dir themselves.

## 15. Decisions

- Model-facing data tools become bash skills + thin executables; the schema
  list leaves `SYSTEM.md`.
- Wire contract, not crate dependency; the arrows claw → contract ← tools are
  enforced.
- The scoped agent credential precedes every client; consents, sessions, and
  ingest are unreachable by it.
- Native keeps: `bash`, `ask_secret`, `coding_agent`, `zoom`, memory,
  `attach_file` (v1).
- Waves: watches + notify, then tasks, then identity + phone.
- Self-contained tools are files + system schedulers + the three HTTP
  primitives; no plugin SDK, no manifest registry, no new IPC.
- Two tiers: tier 1 writable by anyone, including the agent mid-session;
  tier 2 (`ClawExtension`) keeps secret custody, tick/webhook latency, and
  hub depth.

---

## Status

Wave 1 is built: `tools/contract` (allowlist, header, conformance
suite), `tools/notify` (the first client tool), the claw's contract
enforcement (the scoped agent token, the allowlist middleware, the
push and schedule routes, the JSON error shape everywhere), and the
provisioning seam (`agent_token` + `tools_bin` → the daemon's tool
environment → `ShellTools::with_env` → the coding agent's commands).
The conformance suite runs against the daemon's real router in
`crates/rhema-claw/tests/http.rs`; `tools/notify/tests` drives the
`notify` binary against a mock. Waves 2–4 (skills mounts, packaging,
more tools) are still future work.
