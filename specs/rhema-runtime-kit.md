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
local temp dir, a backend-shared dir, or an uploaded dir;
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
- **The executable is the host's binary.** Every wiring sets its bin
  variable to `current_exe()`. A sandbox can be another OS or
  architecture (a macOS host over ssh to a Linux arm64 box), and the
  host's image cannot run there.
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
in `rhema-sandbox::kit` (§12.1), because every frontend already depends
on that crate and `rhema-sandbox` depends on `rhema-bash`, never the
reverse.

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

For a sharing backend, the root is a directory the host and the
sandbox both see. The backend chooses it, not the workspace:

```rust
pub struct SharedRoot {
    /// The host-side path; both sides read and write it.
    pub host: PathBuf,
    /// The same directory as the sandbox sees it.
    pub sandbox: String,
}

pub enum Root {
    /// A directory mounted into the sandbox (docker, lxd, ecrou).
    Shared(SharedRoot),
    /// No shared directory: the kit uploads (ssh, http).
    Upload,
}
```

The host path is per user: `RHEMA_RUNTIME_DIR`, then
`$XDG_RUNTIME_DIR/rhema`, then `$TMPDIR/rhema-<uid>`, all mode `0700`.
A daemon with no login session falls back to
`$HOME/.cache/rhema/runtime`. The ecrou backend keeps its own scratch
rule when the snap cannot see the chosen root.

Each sharing backend mounts the root at the same path:

- docker: a second `-v` in `docker_run_argv`; `mounts_current` learns
  the runtime mount too, or every assembly would read the set as stale
  and recreate the container.
- lxd: a `runtime` disk device beside `workspace`, through the
  idempotent `lxd_device`.
- ecrou: an extra share on every exec, the way the capture dir already
  takes one; the number of shares the CLI accepts is a spike item.

| placement | host root | sandbox path | cleanup |
|---|---|---|---|
| local | the system temp dir | the same path | `TempDir`/guard on drop |
| shared | the per-user runtime root, mounted | the same path | guard on drop; stale dirs are 1 KB |
| uploaded | host staging | the sandbox's cache dir (`$XDG_CACHE_HOME/rhema/kit/...`) | guard on drop, best-effort remote remove |

A sharing backend that cannot add the mount falls back to
`<workspace>/.rhema/runtime`, and a backend with no shared directory
uploads. The workspace is the exception, not the rule.

### 3.2 The runtime tree

```text
<runtime-root>/
  bin/<version>/              # installed by the sandbox, not copied from the host
    rhema                     # plus gob, which jobs needs
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

The helper install survives the session; the per-session dirs do not.
A new version installs beside the old one, and a sweep removes a
version once no session names it (v1: leave it; v2: sweep). The
fallback layout is the same tree under `<workspace>/.rhema/runtime`.

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
    /// A directory both sides see, when the backend can mount one.
    pub shared_root: Option<SharedRoot>,
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

| backend | shared root | upload | inbound_net | kits in v1 |
|---|---|---|---|---|
| local | n/a | n/a | n/a | all |
| docker | yes, second bind mount | yes | UDS on the mount; a TCP fallback binds a host interface | jobs, vision, mcp; search/decide after §8 |
| lxd container | yes, disk device | yes | same as docker | jobs, vision, mcp; search/decide after §8 |
| lxd vm | no | yes (seed) | unproven | search/decide (scoped config); jobs/vision need a state mirror |
| ecrou | yes, extra share (virtiofs) | n/a | unproven | jobs, vision, mcp if the UDS crosses |
| ssh | no | yes | yes (remote forward) | mcp, search/decide; jobs/vision need a state mirror |
| http | no | `PUT` (size cap) | no | none in v1; search/decide once the helper fits |

The table is the decision point: a backend is supported by declaring
capabilities, not by adding another `if sandboxed`.

## 5. Transport

`Bridge` is what the MCP kit needs and the other four may grow into:

```rust
pub enum Bridge {
    None,
    Unix { path: String },   // shared_root
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

The helper is installed inside the sandbox, never copied from the host.
The host's binary is the wrong OS and architecture for an ssh box or a
container on another machine, and a copy is tens of megabytes at best.

Placement runs one probe and one install:

1. **Probe.** Run `uname -s`/`uname -m` and, if a helper exists at the
   kit's path or on `PATH`, `<helper> __kit-version`. A helper whose
   version is at least the kit's is used as is.
2. **Install.** Otherwise run the published installer with
   `RHEMA_INSTALL_DIR=<private dir>`. It resolves the sandbox's
   `os_arch`, downloads that target's tarball, verifies the manifest's
   sha256, and installs `rhema` there, with `gob` beside it when the
   platform carries one. The private dir is
   `<runtime-root>/bin/<version>/` when the root is shared and
   persistent, and a sandbox-side cache
   (`$XDG_CACHE_HOME/rhema/kit/<version>` or the backend's scratch) when
   it is not. It never replaces the user's own `rhema`.
3. **Recheck.** `__kit-version` must now satisfy the kit, or the kit is
   skipped with a notice.

The installer is the `get.rhema.rs/code` script the update path already
runs. It needs `curl` or `wget`, `tar`, and a sha256 tool; the
provision's package set gains them. The host fetches the script and its
`<url>.sha256` sidecar, verifies the sidecar, and uploads the script to
the sandbox before running it, so the sandbox executes the script the
host checked. The channel is `stable` for a release host and `edge` for
a dev host; the version check decides whether the result is usable.

`KIT_VERSION` is a small integer, bumped when the init contract or a
hidden mode changes. It is separate from the package version: a newer
helper that reports a compatible `KIT_VERSION` is fine, so the host does
not need its exact build. A helper that is too old gets the current
release installed beside it, and the host's own binary is untouched.

A dev build that is not published is the one case the installer cannot
cover. The fallbacks, in order:

- same OS and architecture: upload `current_exe`;
- `RHEMA_KIT_HELPER` names a target-matched artifact to upload;
- otherwise the kit is skipped, with a notice naming the target.

The small `rhema-cli` target remains v2: a smaller download, fewer
dependencies, and one place to keep `KIT_VERSION`. It is installed the
same way.

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

- Runtime dirs are `0700`; init files and the UDS `0600`; the installed
  helper `0755`; media `0700`, files `0600` (the CLI already enforces the
  last two).
- The token is the bridge's auth. A socket in a shared root is visible
  to the host user's processes; the token, not the path, is the boundary.
- The helper is installed by the sandbox's own installer from a
  checksum-verified tarball, and must report a compatible
  `__kit-version`; otherwise the kit is skipped. The install never
  replaces the user's own `rhema`.
- No init file carries a secret. The only secret-bearing file considered
  is §8's scoped config, mode `0600`, only on explicit opt-in.
- The fallback root `<workspace>/.rhema/runtime` joins the workspace's
  git exclude the same way the ecrou git-env already excludes `.rhema/`.
  The primary root sits outside the checkout, so a checkout never holds
  the helper or a live socket.
- Uploads never take a model-influenced path: the placer chooses the
  names, the sandbox path is built from the session key and the
  kit version.

## 10. Failure modes

| failure | behavior |
|---|---|
| no helper source, a failed install, or an incompatible version | that kit is skipped, debug notice, the session runs |
| upload fails | that kit is skipped, the host staging dir is removed |
| UDS cannot connect in the sandbox | MCP is skipped; jobs/vision/search/decide are unaffected |
| ecrou's virtiofs cannot carry a UDS | MCP falls back to TCP if the backend has inbound_net, else skipped |
| a stale session dir | a few KB; the per-session guard removes it on the normal path, a sweep is v2 |
| a stale helper version | left for the sweep; the session keeps its own path |

## 11. Rollout

0. **Env channel.** `RunRequest.env` and the `ShellTools` map;
   verify with the scripted sandbox. One day.
1. **Kit types and local placement.** Move the five wirings onto
   `Kit`/`KitDir` with no behavior change; the local path stays a temp
   dir. Two days.
2. **Shared placement and the installer.** The per-user root, the three
   mounts, the probe/install/recheck path, and `gob`; docker first, then
   lxd; MCP first, then jobs and vision. The ecrou UDS spike runs here.
   Five to six days.
3. **Uploads and the small helper.** `rhema-cli`; ssh upload plus a
   remote forward; switch every kit. A week.
4. **search/decide secrets decision and http.** After the §8 decision;
   http waits for a helper that fits its cap or an install path.

Each phase lands green, and the spec's matrix is updated with what the
backend actually did, not what the table predicted.

## 12. Decisions

1. **The kit and the placer are library code, not frontend code.**
   `rhema-bash` owns the pure `Kit`, `Needs`, `Capabilities`, `Layout`,
   and the init text. `rhema-sandbox::kit`, behind a `kit` feature, owns
   `place` and the `Sandbox` calls. `rhema-code`, `rhema-cloud-agent`,
   and the external claw call the same function; none of them owns it.
   The cloud agent adds the sandbox handle its frontend already drives.
   `rhema-code`'s own doc calls the crate a leaf consumer, and a
   non-terminal frontend must not pull ratatui and crossterm to place a
   file. `Sandboxed` keeps the `Arc<dyn Sandbox>` it builds today (it is
   dropped after the executor wraps it); placement needs the handle for
   `write_file`.
2. **The runtime root is per user and backend-mounted; the workspace is
   the fallback.** A checkout is where the user's code lives, not where
   a helper and live sockets belong. The backend adds a mount for
   a dedicated root; only a backend that cannot mount falls back to
   `<workspace>/.rhema/runtime`, and only upload-only backends skip the
   root altogether.
3. **The sandbox installs the helper; nothing copies the host's binary.**
   A copy only works on the same OS and architecture, and a debug build
   is hundreds of megabytes. The published installer already detects
   the sandbox's `os_arch`, honors `RHEMA_INSTALL_DIR`, verifies the
   tarball's sha256, and installs `gob` beside `rhema`. The small CLI
   target makes the install cheaper, not different.
4. **MCP servers stay host-side in v1.** The alternative is a separate
   design: servers must outlive commands, which ecrou's per-command VM
   and ssh cannot offer without a daemon. Document the host-fs exposure
   in the MCP spec when the sandbox path ships.
5. **Capabilities replace the sandbox gate.** `sandboxed.is_none()`
   disappears from `setup.rs`; the gate is per kit and per backend.
6. **Fail closed.** A kit that cannot be placed is absent from the
   prompt and the shell; it never half-works.

## 13. Alternatives considered

- **Copy the host's binary (today, and the first draft).** The leak that
  started this work; it also cannot run across OS or architecture, and
  a debug build is 500 MB. Rejected.
- **Share one prebuilt helper from the runtime root.** Same architecture
  problem in a different place. Rejected in favor of an install per
  sandbox and target.
- **Mount the host's whole temp dir into every sandbox.** Not every
  backend can (ssh and http have no mount; ecrou's `/tmp` is private),
  and it exposes every user's temp files. Rejected in favor of a
  dedicated per-user root, mounted by the backends that can.
- **Put the runtime under `<workspace>/.rhema/runtime`.** It needs no
  new mount, but it puts a helper and live sockets in the user's
  checkout. Kept as the fallback.
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
  the shared-root path or the TCP fallback.
- Can every backend add the runtime mount to an instance a stored target
  names? A user-owned container is not ours to recreate just to add a
  mount; the fallback is upload or the workspace root.
- Where does the root live when no login session exists (a cloud agent,
  a daemon)? `$XDG_RUNTIME_DIR` is absent there, so the cache fallback
  carries the lifecycle.
- For ssh, who allocates the forwarded port, and how does a stale
  forward get reclaimed when a session dies?
- Search/decide: scoped config or host bridge? The answer changes
  whether their `Needs.bridge` is `true`.
- Http: can its `PUT` cap rise, or does a helper need an install URL
  the sandbox fetches itself?
- Does the installer need a `--version` flag, or is latest-plus-
  `KIT_VERSION` enough? The channel pointer is one version per channel
  today.
- For an airgapped sandbox, should the host fetch the target's tarball
  and upload it, rather than the sandbox reaching `get.rhema.rs`?
- Should the helper install be per sandbox instance or per user path?
  A persisted container keeps it; a recreated one reinstalls unless the
  runtime root's `bin/` survives.
- When the small helper ships, does the full `rhema` binary keep the
  hidden modes, or do they leave `rhema-code` entirely?
