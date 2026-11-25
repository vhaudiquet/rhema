# rhema-runtime-kit, what a generated bash function needs and how it crosses a sandbox

> **Status:** design draft, 2026-11-25. Decisions in §12, the rollout
> in §11. Nothing here is implemented yet.
> **Grounding:** `specs/rhema-sandbox.md` (the seam and its two
> families), `specs/rhema-mcp.md` (§5, §10, §12), `rhema-bash::init`
> (today's provisioner), `app/setup.rs` (the five gates).
> **One-line promise:** a generated bash function is described once,
> as a kit, and placed wherever its session's bash runs: the host, a
> container, a VM, or a remote box.

---

## 0. Scope

Five features reach the model through the same mechanism: a function
defined in a `BASH_ENV` init file, exec'ing the rhema binary in a hidden
mode. They are `job`, `see`, `search`, `decide`, and `mcp` (with one
stub per server). Today every one of them is host-only.

**In scope:** one kit description shared by the five; placement into a
local temp dir, a sandbox-visible workspace dir, or an uploaded dir;
the env channel that makes `BASH_ENV` and the bin variable arrive; the
helper executable's resolution and versioning; the capability matrix
that decides which kit a backend can carry; tests and a rollout.

**Out of scope, v1:** moving the MCP servers themselves into the
sandbox (§12.4), a pure-bash bridge client, image building, Windows,
and the provider-secret question for `search`/`decide` beyond naming
the two forms it can take (§8).

## 1. The problem, verified against this tree

- **One gate, five features.** `session_agent_config` builds jobs and
  vision only under `(sandboxed.is_none())`, and `wire_for_session` in
  `search_wire`, `decision_wire` and `mcp_wire` returns `None` as soon
  as `sandboxed.is_some()`. The premise "MCP should work in a sandbox
  like the others" has no others to copy: the family has never crossed
  the boundary.
- **The env map is dropped.** `HostAgentConfig::shell_env` is read only
  when `HostAgent::new` builds the default `ShellTools`. A sandbox puts
  a pre-built `ShellTools` in `config.tools` (`Sandboxed::apply_tools`),
  built with `with_executor`, whose own doc says it "reads no env map";
  `RunRequest` is `{command, cwd, timeout}`. So `BASH_ENV`, the token,
  the bin variable, and the media dir never reach a sandbox command.
- **The files live in host `/tmp`.** `rhema_bash::init::temp_dir` is the
  system temp dir. Mounted backends see only the workspace; ecrou's
  `/tmp` is VM-local; ssh and http are disjoint.
- **The executable is a host path.** Every wiring sets its bin variable
  to `current_exe()`. A sandbox has no such path.
- **The bridge is a host endpoint.** The MCP bridge listens on a UDS in
  the session temp dir. A sandbox cannot dial it.
- **The state is host-side.** Jobs' steering feed, vision's staging
  drain, and search/decide's profile and secret resolution all run in
  the frontend process.

The kit addresses the first five; §8 names the sixth per feature.

## 2. The kit

A kit is a pure description, no filesystem work:

```rust
pub struct Kit {
    /// `jobs`, `vision`, `search`, `decision`, `mcp`.
    pub feature: Feature,
    /// The init file's body: the function definitions, in sandbox
    /// coordinates for paths the body embeds.
    pub body: String,
    /// The bin variable (`RHEMA_MCP_BIN`, …) and the helper it names.
    pub bin_var: &'static str,
    /// Env entries besides `BASH_ENV` and the bin var (token, profile,
    /// media dir, staging dir).
    pub vars: BTreeMap<String, String>,
    /// What placement must supply.
    pub needs: Needs,
    /// The predecessor init file's sandbox path, for the chain.
    pub inherited: Option<String>,
}
```

The wiring owns the body and its variables, as today. The kit owns the
four things where a mistake is a reviewed failure:

1. **Layout.** Where the directory is and what it is called.
2. **Executable.** Which binary the bin variable names and how it got
   there.
3. **Init.** The header, the guarded predecessor source, and the mode
   of the file.
4. **Env.** `BASH_ENV` and the bin variable, in sandbox coordinates.

`rhema_bash::init` already owns 3's text and 4's map. This spec grows it
into a `runtime` module (or renames it): `Kit`, `Needs`, `Capabilities`,
`Layout`, and `KitDir`. The placement that talks to a sandbox lives
above, in `rhema-code` (§12.1), because `rhema-sandbox` depends on
`rhema-bash`, never the reverse.

## 3. Placement

`KitDir` is the one thing a wiring is handed:

```rust
pub enum KitDir {
    /// Local session: the system temp dir, as today.
    Host(PathBuf),
    /// Shared 1:1 with the sandbox (docker, lxd, ecrou).
    Shared(PathBuf),
    /// Host staging, uploaded to `sandbox` after the wiring writes it.
    Upload { host: PathBuf, sandbox: String },
}

impl KitDir {
    /// Where the wiring writes `init.bash` and its state dirs.
    fn write_dir(&self) -> &Path;
    /// The path the sandbox's bash must see for `name`.
    fn sandbox_path(&self, name: &str) -> String;
}
```

The wiring writes to `write_dir()` and writes `sandbox_path()` values
into any path it embeds (the inherited `BASH_ENV`). When the two differ,
the placer uploads `init.bash` and the files the wiring created, and
creates the empty state dirs (`media/`, `staging/`, `session/`, the
socket's parent) on the sandbox side before the session starts.

### 3.1 The runtime root

| placement | root | cleanup |
|---|---|---|
| local | the system temp dir, per-session name | `TempDir`/guard on drop, as today |
| shared | `<workspace>/.rhema/runtime/<feature>/<session>/` | guard on drop; stale dirs are 1 KB |
| uploaded | host staging, then `<sandbox-workspace>/.rhema/runtime/...` | guard on drop, best-effort remote remove |

The workspace is the only directory every non-local backend can share,
and `.rhema` is already the workspace's private convention (ecrou's
git-env adds it to `.git/info/exclude`). The runtime root is a sibling
of the session store, never inside a checkout path the model is
expected to edit.

### 3.2 The runtime tree

```text
<workspace>/.rhema/runtime/
  bin/<sha256>/rhema          # one copy per binary build, shared by all five
  mcp/<session-key>/
    init.bash
    mcp.sock
    media/
  jobs/<session-key>/
    init.bash
    session/                  # the jobs state both sides read
  vision/<session-key>/
    init.bash
    staging/                  # the images the host attaches
  search/<session-key>/init.bash
  decision/<session-key>/init.bash
```

The helper is content-hashed and survives the session; the per-session
dirs do not. A rebuild produces a new hash, so a running session keeps
its copy; the old hash is removed by a sweep once no manifest
names it (v1: leave it; v2: sweep).

### 3.3 The chain

The five init files form one lineage: jobs, vision, search, decision,
mcp. Each kit's `inherited` is the previous kit's **sandbox-side**
`init.bash`, and `BASH_ENV` is the last one's. The placer builds the
chain in order and only then resolves the env map, so an uploaded chain
never writes a host path into a sandbox file.

## 4. Capabilities and the gate

Two small types replace the `sandboxed.is_none()` test:

```rust
pub struct Capabilities {
    /// A directory both sides see at the same path.
    pub shared_fs: bool,
    /// `Sandbox::write_file` can carry a helper and an init file.
    pub upload: bool,
    /// The sandbox can dial a host TCP endpoint.
    pub inbound_net: bool,
}

pub struct Needs {
    /// An executable and an init file (every kit).
    pub exec: bool,
    /// A directory both sides read and write (jobs, vision, mcp media).
    pub shared_state: bool,
    /// A live host endpoint (mcp; search/decide if they take the bridge).
    pub bridge: bool,
    /// Host-side config or keys (search, decide).
    pub secrets: bool,
}
```

`session_agent_config` asks `needs.satisfied_by(caps)` per kit, and a
kit that cannot be placed is skipped with a debug notice, the same
fail-closed behavior as today's gate. The prompt section is set only for
placed kits.

| backend | shared_fs | upload | inbound_net | kits in v1 |
|---|---|---|---|---|
| local | n/a | n/a | n/a | all |
| docker | yes (bind mount) | yes | UDS on the mount; a TCP fallback binds a host interface | jobs, vision, mcp; search/decide after §8 |
| lxd container | yes (disk device) | yes | same as docker | jobs, vision, mcp; search/decide after §8 |
| lxd vm | no | yes (seed) | unproven | search/decide (scoped config); jobs/vision need a state mirror |
| ecrou | yes (virtiofs) | n/a | unproven | jobs, vision, mcp if the UDS crosses |
| ssh | no | yes | yes (remote forward) | mcp, search/decide; jobs/vision need a state mirror |
| http | no | `PUT` (size cap) | no | none in v1; search/decide once the helper fits |

The table is the decision point: a backend is supported by declaring
capabilities, not by adding another `if sandboxed`.

## 5. Transport

`Bridge` is what the MCP kit needs and the other four may grow into:

```rust
pub enum Bridge {
    None,
    Unix { path: String },   // shared_fs
    Tcp { addr: String },    // uploaded, via a tunnel
}
```

- **Unix:** the bridge binds `<runtime>/mcp.sock`, mode `0600`. The
  sandbox connects at the same path. Docker and lxd bind mounts support
  this; ecrou's virtiofs support is the first spike (§11 phase 2).
- **TCP:** for fs-disjoint backends, the bridge listens on an interface
  the sandbox can reach: the docker bridge gateway for a container, a
  loopback port for an ssh session's `-R` forward. The token is the
  auth; the port is allocated per session, never fixed.
- **None:** http has no inbound route in v1; MCP is off there, with a
  notice, and the other kits are unaffected.

The MCP entry owns the bridge and the socket path; the placer creates
the parent dir first. On teardown the entry's guard unlinks the socket
before the kit's guard removes the dir, the order the local path
already uses.

The token always travels in the env map. The media dir
(`RHEMA_MCP_TMP`) is `<runtime>/media`, mode `0700`, visible to both
sides in the shared case; the model reads media in the sandbox, and the
host never needs it.

## 6. Env delivery

`RunRequest` grows `env: BTreeMap<String, String>` (or the bash tool
prefixes `env K=V`; the spec picks `RunRequest`, one field for every
executor). `ShellTools` carries the map independently of its executor:

- `with_env` keeps working for the local executor.
- `with_executor` takes the map too, so `apply_tools` can attach the
  session's map after the wirings are known.
- The local executor applies it to the process; the sandbox adapter
  forwards it to the backend; a backend that cannot apply it fails the
  command rather than silently dropping the map.

Ordering in `session_agent_config` today is `apply_tools` first, env
extend second. It becomes: build the kit chain, place it, collect the
env, then build/attach the tools. The sandbox assembly already runs
before the session config, so the placer has `Capabilities` when it
needs them.

## 7. The helper

Two stages, so the plumbing comes before the packaging:

- **v1, mounted only:** copy the full `rhema` binary into
  `<workspace>/.rhema/runtime/bin/<sha256>/rhema`, once per workspace,
  shared by all five kits. 400 MB, but one copy, not one per session,
  and it needs no new target. This makes docker, lxd and ecrou work.
- **v2, all backends:** a `rhema-cli` target (or a `runtime-kit`
  feature on `rhema-code`) carrying the five hidden modes and their
  dependencies, not the TUI. A few MB, uploadable over ssh and
  plausibly over http once its cap allows it. This also fixes jobs and
  vision on the host, which today pay the full binary for the same
  modes.

The helper must expose `__kit-version`; the init header names the
version it was generated for. A mismatch is a skipped kit with a
notice, never a half-working shell.

For an image that already ships the helper (docker, lxd, a persisted
ecrou), the kit can name it by path instead of copying; the manifest
records which source won, so a debugging session knows whether the
sandbox's copy or the workspace's is in play.

## 8. Per-feature needs

| feature | helper | shared state | bridge | secrets | note |
|---|---|---|---|---|---|
| jobs | yes | yes: the jobs dir, the host's steering feed reads it | no | no | the sandbox needs `gob` too |
| vision | yes | yes: the staging dir the host drains | no | no | fs-disjoint needs an image transfer or a sandbox-side attach |
| search | yes | no | maybe | yes | the hidden mode resolves the profile's providers and keys |
| decide | yes | no | maybe | yes | same |
| mcp | yes | yes: socket + media | yes | no | servers stay host-side in v1 |

The kit unifies the helper and the shared-state placement. The fourth is
an open decision:

- **Scoped config file:** write the profile's provider settings and the
  needed keys into `<runtime>/config` mode `0600`, and let the helper
  read it. Simple, but copies secrets into the sandbox.
- **Host bridge:** search/decide call a host endpoint like MCP does.
  Keeps keys on the host, costs a bridge and a protocol.

Recommendation: reuse the MCP bridge's transport for search and decide,
and do not write secrets into a sandbox on the mounted path unless the
user opts in. This is a user-facing security choice, not a plumbing default.

## 9. Security

- Runtime dirs are `0700`; init files and helper copies `0600`; the UDS
  `0600`; media `0700`, files `0600` (the CLI already enforces the last
  two).
- The token is the bridge's auth. A socket in the workspace is visible
  to the user's own processes; the token, not the path, is the boundary.
- The helper is content-hashed; an installed helper must match
  `__kit-version`, or the kit is skipped.
- No init file carries a secret. The only secret-bearing file considered
  is §8's scoped config, mode `0600`, only on explicit opt-in.
- `.rhema/runtime` joins the workspace's git exclude the same way the
  ecrou git-env already excludes `.rhema/`.
- Uploads never take a model-influenced path: the placer chooses the
  names, the sandbox path is built from the session key and the
  content hash.

## 10. Failure modes

| failure | behavior |
|---|---|
| no helper source, or an incompatible version | that kit is skipped, debug notice, the session runs |
| upload fails | that kit is skipped, the host staging dir is removed |
| UDS cannot connect in the sandbox | MCP is skipped; jobs/vision/search/decide are unaffected |
| ecrou's virtiofs cannot carry a UDS | MCP falls back to TCP if the backend has inbound_net, else skipped |
| a stale session dir | a few KB; the per-session guard removes it on the normal path, a sweep is v2 |
| a stale helper hash | left for the sweep; the session keeps its own hash |

## 11. Rollout

0. **Env channel.** `RunRequest.env` and the `ShellTools` map;
   verify with the scripted sandbox. One day.
1. **Kit types and local placement.** Move the five wirings onto
   `Kit`/`KitDir` with no behavior change; the local path stays a temp
   dir. Two days.
2. **Shared placement.** `<workspace>/.rhema/runtime`; the full-binary
   helper; docker first, then lxd; MCP first, then jobs and vision. The
   ecrou UDS spike runs here. Three days.
3. **Uploads and the small helper.** `rhema-cli`; ssh upload plus a
   remote forward; switch every kit. A week.
4. **search/decide secrets decision and http.** After the §8 decision;
   http waits for a helper that fits its cap or an install path.

Each phase lands green, and the spec's matrix is updated with what the
backend actually did, not what the table predicted.

## 12. Decisions

1. **The kit is pure; the placer is in `rhema-code`.** `rhema-bash`
   owns `Kit`, `Needs`, `Capabilities`, `Layout`, and the init text;
   `rhema-code` owns `place` and the `Sandbox` calls. This keeps the
   dependency order (`rhema-bash ← rhema-sandbox ← rhema-code`) and
   lets the tests drive placement with the mock sandbox.
2. **The shared root is `<workspace>/.rhema/runtime`.** It is the only
   directory a mounted sandbox and the host both see, and `.rhema` is
   already the workspace's private convention.
3. **v1 copies the full binary; v2 ships a small CLI.** The copy is the
   smallest change that makes mounted backends work; the small target is
   the real fix for every sandbox copy.
4. **MCP servers stay host-side in v1.** The alternative is a separate
   design: servers must outlive commands, which ecrou's per-command VM
   and ssh cannot offer without a daemon. Document the host-fs exposure
   in the MCP spec when the sandbox path ships.
5. **Capabilities replace the sandbox gate.** `sandboxed.is_none()`
   disappears from `setup.rs`; the gate is per kit and per backend.
6. **Fail closed.** A kit that cannot be placed is absent from the
   prompt and the shell; it never half-works.

## 13. Alternatives considered

- **Copy the binary per session (today).** The leak that started this work;
  a 400 MB copy per session and a pinned inode per orphan. Rejected.
- **Bind-mount the host's temp dir into every sandbox.** Not every
  backend can (ssh and http have no mount; ecrou's `/tmp` is private),
  and it exposes the whole temp dir, not the one socket. Rejected.
- **A pure-bash client over the socket.** `${RHEMA_BIN} __server` does
  chunking, schema, SSE framing, media, and retries; `nc -U`, `socat`,
  and `jq` are not guaranteed in a sandbox, and a second client would
  drift. Rejected.
- **Run the bridge and the servers inside the sandbox.** The cleanest
  fs story, but ecrou's per-command VM and ssh cannot keep a stateful
  server alive without a daemon. Deferred (§12.4).

## 14. Open questions

- Does a Unix socket on ecrou's virtiofs share accept `connect()` from
  the guest? This is the phase 2 spike; it decides whether ecrou gets
  the shared-FS path or the TCP fallback.
- For ssh, who allocates the forwarded port, and how does a stale
  forward get reclaimed when a session dies?
- Search/decide: scoped config or host bridge? The answer changes
  whether their `Needs.bridge` is `true`.
- Http: can its `PUT` cap rise, or does a helper need an install URL
  the sandbox fetches itself?
- Should the helper cache be per workspace (the proposal) or per user,
  shared across workspaces to save the 400 MB in v1?
- When the small helper ships, does the full `rhema` binary keep the
  hidden modes, or do they leave `rhema-code` entirely?
