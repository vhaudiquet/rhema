# rhema-runtime-kit, what a generated bash function needs and how it crosses a sandbox

> **Status:** implemented for jobs, vision, search, and decide.
> `see`, `search`, `decide`, and a sandboxed `job` are shell text;
> `job` needs `gob`, not the rhema helper. `search` and `decide` hand
> their argv to a per-session host endpoint. Local sessions still run
> the host binary's hidden modes. The sandbox installer still installs
> the `rhema` binary as the kit's version carrier, but no sandbox
> feature executes that copy; removing the install is the remaining
> work. MCP is no longer a kit: `mcp-cli` runs inside the sandbox and
> reads its own config there. The bridge and socket sections below
> describe the earlier host-side design and are kept as history.
> **Grounding:** `specs/rhema-sandbox.md` (the seam and its two
> families), `specs/rhema-mcp.md` (§5, §10, §12), `rhema-bash::init`
> (today's provisioner), `app/setup.rs` (the five gates).
> **One-line promise:** a generated bash function is described once,
> as a kit, and placed wherever its session's bash runs: the host, a
> container, a VM, or a remote box.

---

## 0. Scope

Four features reach the model through the same mechanism: a function
defined in a `BASH_ENV` init file. `job` is shell text over `gob` in a
sandbox and the hidden `__jobs` mode on the host. `see` is shell text
that stages an image. `search` and `decide` are shell text over a
per-session host endpoint: the function sends its argv to the host
over a Unix socket, and the host resolves the profile, providers, and
keys in its own memory. `mcp` moved to its own CLI. Each one is placed
wherever the session's bash runs.

**In scope:** one kit description shared by the four; placement into a
local temp dir, a backend-shared dir, or a sandbox-side cache;
the env channel that makes `BASH_ENV` and the bin variable arrive; the
installed binaries' resolution and versioning; the capability matrix
that decides which kit a backend can carry; tests and a rollout.

**Out of scope, v1:** image building, Windows, and the HTTP host route
for `search`/`decide`. The host-side MCP bridge is history; the servers
now live in the sandbox behind `mcp-cli`.

## 1. The problem this work removed

This section records the tree before the kit landed; it is history. The
current state is §11.

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
    /// `jobs`, `vision`, `search`, `decision`.
    pub feature: Feature,
    /// The stubs this feature adds: one per hidden-mode macro.
    pub stubs: Vec<Stub>,
    /// The shell macros this feature adds: `see`.
    pub shells: Vec<Shell>,
    /// Env entries besides `BASH_ENV` and the bin var (token, profile,
    /// media dir, staging dir, session file).
    pub vars: BTreeMap<String, String>,
    /// What placement must supply.
    pub needs: Needs,
}
```

The wiring owns the stubs, shells, and variables. The kit owns the
four things where a mistake is a reviewed failure:

1. **Layout.** Where the directory is and what it is called.
2. **Executable.** Which binary the stubs name, if any, and how it got
   there.
3. **Init.** The generated file, its function set, and its mode.
4. **Env.** `BASH_ENV`, and the bin variable when a stub needs one, in
   sandbox coordinates.

`rhema_bash::runtime::init_text` generates the function text and the
placement writes it. A shared root lets the host write the file
directly; a sandbox with no shared root uploads the same text. The
`__init` mode remains on the helper as a diagnostic, not as the
placement path. The placement that talks to a sandbox lives in
`rhema-sandbox::kit` (§12.1), because every frontend already depends on
that crate and `rhema-sandbox` depends on `rhema-bash`, never the
reverse.

## 3. Placement

A `Kit` is placed. The placer ensures the runtime root (or a
sandbox-side cache when there is no shared root), installs the helper
when a kit needs one (§7), and writes the init file (§3.3). A shared
root gets the file from the host; a sandbox with no shared root receives
the same text through its file route.

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
    /// No shared directory: a sandbox-side cache (ssh, http).
    Sandbox,
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
| sandbox | none | the sandbox's cache dir (`$XDG_CACHE_HOME/rhema/kit/...`) | guard on drop, best-effort remote remove |

A sharing backend that cannot add the mount falls back to
`<workspace>/.rhema/runtime`, and a backend with no shared directory
uses a sandbox-side cache. The workspace is the exception, not the rule.
The root then holds the install, the MCP socket, and the session tree
when it is shared; without a root the tree is the sandbox's own (§3.2).

### 3.2 The runtime tree

```text
<runtime-root>/               # shared, when the backend mounts one
  bin/<version>/              # installed by the sandbox, not copied from the host
    rhema                     # plus gob, which jobs needs
  mcp/<session-key>/
    mcp.sock
  sessions/<session-key>/
    vision/staging/           # images the host reads or pulls
    jobs/session/             # jobs state the host reads or pulls
    mcp/media/                # large payloads the model reads in place

<sandbox-tmp>/rhema-<session>/  # only when there is no shared root
  vision/staging/
  jobs/session/
  mcp/media/
```

With a shared root, the session tree sits under it at mode `0700` and
the host reads it directly. Without one, the same tree is the sandbox's
`mktemp -d` directory and the host pulls it through `read_file`; the
socket is TCP or absent. When the sandbox's `/tmp` does not survive a
command (ecrou with persistence off), the state tree moves under the
shared root, or the kit is skipped.

The helper install survives the session; the per-session dirs do not.
A new version installs beside the old one, and a sweep removes a
version once no session names it (v1: leave it; v2: sweep). Under
`Root::Sandbox`, the same `bin/<version>/` lives in the sandbox cache;
the fallback layout is the same tree under `<workspace>/.rhema/runtime`.

### 3.3 The init file

The features do not chain files. The placement writes one file per
session's kit set:

    <session>/init.sh

The function bodies are static. A helper stub is
`job() { "$RHEMA_JOBS_BIN" __jobs "$@"; }`; a shell macro carries its
body verbatim, so `see` names no binary. The values that differ per
session (bin, token, staging, jobs session, media) are env entries. A
feature whose env is absent exits with "not available in this session".

Under a shared root the host writes the file directly. A sandbox with
no shared root gets the same text through `write_file`, once per kit
set. The `__init` helper mode is still available for a manual
diagnosis; it is no longer how a placement makes the file.

### 3.4 Cleanup

The sandbox temp dir cannot ride a host `TempDir`, so the placer owns
it:

- **Normal end.** The bridge unlinks its socket, then the placer runs
  `rm -rf -- <dir>` in the sandbox, with a short timeout and errors
  ignored. The path is the placer's own record, never model input.
- **Host crash.** Nothing runs. The next session's start sweeps
  `${TMPDIR:-/tmp}/rhema-*` dirs that are older than seven days and
  owned by the sandbox user, before creating a new one. The sweep stays
  one level deep, ignores errors, and follows no symlink.
- **Shared root.** The per-session directory under the root gets the
  same guard on the host side, and the same age sweep at startup:
  `place_shared` returns the `SessionDir` guard, the frontend keeps it
  for the session's life, and `kit::sweep` removes trees older than
  seven days and versions older than 30 days.
- **Install and init.** These are not per-session. A version is removed
  only when it is older than 30 days and is not the version the session
  uses; the running version and its init are never swept.

A stale socket is handled separately: the bridge tries to connect to an
existing path, and unlinks it when the connect is refused (nobody
listening), before binding.

## 4. Capabilities and the gate

Two small types replace the `sandboxed.is_none()` test:

```rust
pub struct Capabilities {
    /// A directory both sides see, when the backend can mount one.
    pub shared_root: Option<SharedRoot>,
    /// `Sandbox::write_file` can carry an init file.
    pub upload: bool,
    /// The host can read the sandbox's files (`read_file`).
    pub pull: bool,
}

pub struct Needs {
    /// An init file and the helper install probe.
    pub exec: bool,
    /// State the host must read (jobs, vision): a shared root or a
    /// `read_file` pull.
    pub shared_state: bool,
    /// Host-side config or keys (search, decide).
    pub secrets: bool,
}
```

`session_agent_config` asks `needs.satisfied_by(caps)` per kit: a kit's
`shared_state` needs a `shared_root` or `pull`. A kit that cannot be
placed is skipped with a debug notice, the same fail-closed behavior as
the original gate. The prompt section is set only for placed kits.

The four kits are jobs, vision, search, and decide. MCP is not one:
`mcp-cli` is installed beside the helper and reads its own config, so
its availability is the installer's, not a kit's.

| backend | shared root | upload | pull | kits |
|---|---|---|---|---|
| local | n/a | n/a | n/a | all |
| docker | yes, second bind mount | yes | yes | jobs, vision, search/decide |
| lxd container | yes, disk device | yes | yes | jobs, vision, search/decide |
| lxd vm | no | yes (seed) | yes | jobs, vision; search/decide have no host forward |
| ecrou | yes, extra share (virtiofs) | n/a | yes | vision, search/decide if the share carries the socket; jobs stay off (one boot per command) |
| ssh | no | yes | yes | jobs, vision, search/decide |
| http | no | `PUT` (size cap) | yes | jobs, vision; search/decide have no host route |

The table is the decision point: a backend is supported by declaring
capabilities, not by adding another `if sandboxed`. Notifications are
separate from placement: SSH and HTTP poll the sandbox's `gob` for job
finishes, local holds `gob events`, and the mounted backends have no
feed yet.

## 5. Transport

The MCP bridge described in this section is history. The search and
decide host endpoint uses the same Unix socket shape, without a
long-lived server process on the host.

`mcp` is the one kit whose state is live. The bridge is a host process
holding the MCP client sessions (stdio children or HTTP connections),
the approvals, and the media. The sandbox's `mcp` function runs
`rhema __mcp`; that process connects to the bridge over the socket and
prints the JSON-RPC result. The servers outlive the call and cannot be
spawned from the sandbox, which is why this is a socket and not a file:
only the listening process can create it, and it must sit on a path
both sides see. A bind mount on the same Linux kernel carries it; the
docker daemon's own socket is the precedent. A named volume is awkward
because its host path belongs to the daemon. A VM boundary (lxd vm,
ecrou's virtiofs, Docker Desktop) is the spike. No shared path means
TCP (ssh) or no MCP (http).

`Bridge` is what the MCP kit needs and the other four may grow into:

```rust
pub enum Bridge {
    None,
    Unix { path: String },   // shared_root
    Tcp { addr: String },    // no shared root, via a tunnel
}
```

- **Unix:** the bridge binds `<runtime>/mcp.sock`, mode `0600`. The
  sandbox connects at the same path. Docker and lxd bind mounts support
  this; ecrou's virtiofs support is the first spike (§11 phase 2).
- **TCP:** for fs-disjoint backends, the bridge listens on an interface
  the sandbox can reach: the docker bridge gateway for a container, a
  loopback port for an ssh session's `-R` forward. The token is the
  auth; the port is allocated per session, never fixed.
- **None:** http has no inbound route for a host bridge. The current
  sandbox-owned MCP client does not need one.

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
extend second. It becomes: build the kit list, ensure the root and the
init, collect the env, then build/attach the tools. The sandbox assembly already runs
before the session config, so the placer has `Capabilities` when it
needs them.

## 7. The helper

The helper is installed inside the sandbox, never copied from the host.
No sandbox kit executes that copy now. `job`, `see`, `search`, and
`decide` are shell text in a sandbox, and `mcp-cli` is its own binary.
The install remains as the version carrier every kit probes through
`__kit-version`; removing it is tracked in §11. On the host, the same
binary still serves the hidden `__jobs`, `__vision`, `__search`, and
`__decision` modes.
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

A dirty dev build is not published. There is no upload fallback for
now: the installer turns up a release that does not satisfy
`KIT_VERSION`, and the kit is skipped with a notice naming the target.

The small `rhema-cli` target remains v2: a smaller download, fewer
dependencies, and one place to keep `KIT_VERSION`. It is installed the
same way.

## 8. Per-feature needs

| feature | helper | shared state | bridge | secrets | note |
|---|---|---|---|---|---|
| jobs | install probe | yes: the jobs dir, or a pull | no | no | shell macro over `gob` in a sandbox |
| vision | no | yes: the staging dir, or a pull | no | no | shell macro; fs-disjoint pulls the staged files |
| search | install probe | no | yes: host endpoint | yes: host-side chain | shell macro; host resolves and runs the chain |
| decide | install probe | no | yes: host endpoint | yes: host-side instance | shell macro; host resolves and runs the instance |
| mcp | not a kit | config file in the sandbox | no | no | `mcp-cli` reads its own config |

The host-read state is two things: vision's staging dir (the images the
host attaches) and jobs' session dir (the host's steering feed). They
live in the per-session tree: under the shared root when there is one,
otherwise in the sandbox's temp dir, pulled through `read_file`. MCP
media is written and read in the sandbox; the host sees it through
vision only when the model attaches an image. `search` and `decide`
keep no state.

The kit unifies the helper and the shared-state placement. `search`
and `decide` take the host-endpoint route:

- The host binds one Unix socket per session under the shared session
  tree, or a host temp directory for a local session. The shell gets
  `RHEMA_CAP_SOCKET` and a per-session bearer token; no provider key
  enters the sandbox.
- `POST /v1/search` carries `{"argv": [...]}`; `POST /v1/decide`
  carries the argv and any stdin. The host resolves the profile and
  the keychain once at setup, keeps the chain or decision instance in
  process, and answers `{"code", "stdout", "stderr"}`.
- Docker, LXD containers, and ecrou reach the socket through the
  runtime mount. SSH asks the backend for a reverse Unix-socket
  forward. HTTP has no route, and an LXD VM has no forward either, so
  their macros stay off with a notice.
- `sandbox.inject_credentials` no longer wires these macros. The
  `RHEMA_SEARCH_CHAIN` and `RHEMA_DECISION_INSTANCE` env forms remain
  only as the standalone CLI's diagnostic path, not the session route.

## 9. Security

- Runtime dirs are `0700`; init files and the UDS `0600`; the installed
  helper `0755`; media `0700`, files `0600` (the CLI already enforces the
  last two).
- The token is the host endpoint's auth. A socket in a shared root is
  visible to the host user's processes; the token, not the path, is
  the boundary. The token scopes one session to `search` and `decide`,
  not to the keychain.
- The helper is installed by the sandbox's own installer from a
  checksum-verified tarball, and must report a compatible
  `__kit-version`; otherwise the kit is skipped. The install never
  replaces the user's own `rhema`.
- No init file carries a secret. The search and decide keys stay in
  the host process; the sandbox sees only the session socket and its
  token.
- The fallback root `<workspace>/.rhema/runtime` joins the workspace's
  git exclude the same way the ecrou git-env already excludes `.rhema/`.
  The primary root sits outside the checkout, so a checkout never holds
  the helper or a live socket.
- The only host-to-sandbox file is the installer script, fetched and
  checksum-checked by the host, at a path the placer chooses. State
  files are written by the sandbox's own tools, not uploaded.

## 10. Failure modes

| failure | behavior |
|---|---|
| no helper source, a failed install, or an incompatible version | that kit is skipped, debug notice, the session runs |
| the installer script cannot be delivered | that kit is skipped, with the reason |
| the host endpoint cannot be reached | `search`/`decide` exit 4; jobs and vision are unaffected |
| ecrou's virtiofs cannot carry the socket | `search`/`decide` fail their call; the other kits run |
| a stale session dir | the drop guard removes it; a start sweep removes dirs older than seven days |
| a stale helper version | removed by the 30-day sweep unless it is the running version |

### 10.1 Tests

- **Matrix.** For each backend capability set, each kit's `Needs` maps
  to placed or skipped, and the prompt section follows.
- **Init.** `__init` emits every stub, guards a missing env with the
  "not available" message, and is byte-stable for a stub set.
- **Local placement.** The existing wiring tests keep passing with the
  generated init; `BASH_ENV` and the bin vars reach the command.
- **Shared placement.** The mock sandbox sees the root and the socket
  dir; drop removes the dir and unlinks the socket.
- **Sandbox placement.** The mock sees `mktemp -d`, the install probe,
  `__init`, the state pull, and the drop `rm -rf`.
- **Stale sweep.** An old `rhema-*` dir goes, a fresh one stays, and a
  directory owned by another user stays.
- **Bridge.** A real UDS in a temp dir accepts a connection, drop
  unlinks it, and a stale socket is rebound.
- **Failure.** A failed install, an old `__kit-version`, and a missing
  helper each skip the kit with a notice and leave the session running.

## 11. Rollout

0. **Env channel.** `RunRequest.env` and the `ShellTools` map;
   verify with the scripted sandbox. One day.
1. **Kit types, the static init, and local placement.** Move the five
   wirings onto `Kit` and the helper's `__init`; add the cleanup guard
   and the matrix/init tests; no behavior change; the local path stays a
   temp dir. Three days.
2. **Shared placement and the installer.** The per-user root, the three
   mounts, the probe/install/recheck path, and `gob`; docker first, then
   lxd; MCP first, then jobs and vision. The ecrou UDS spike runs here.
   Five to six days.
3. **The small helper.** `rhema-cli`; the ssh install plus a remote
   forward; switch every kit. A week.
4. **search/decide secrets decision and http.** After the §8 decision;
   http waits for a helper that fits its cap or an install path.

Each phase lands green, and the spec's matrix is updated with what the
backend actually did, not what the table predicted.

Status: the env channel, the local placement, the Docker/LXD/ecrou
runtime mounts, the in-sandbox installer, and the SSH/HTTP placement are
in. Every sandbox kit is gated on `__kit-version`, but the `rhema`
binary the installer places is only a version carrier now; no sandbox
feature executes that copy.
SSH and HTTP install into `$HOME/.cache/rhema` and read the init file
and the per-session state from there.

Vision works on every backend. Jobs work everywhere except ecrou, which
boots a machine per command, so the gob daemon dies with it. Finish
notifications work on local, SSH, and HTTP; the mounted backends place
the job macro but have no feed yet, so the model polls `job ls` there.
Search and decide use the per-session host endpoint on local, Docker,
LXD containers, ecrou, and SSH. HTTP has no host route, and an LXD VM
has no forward either. MCP is `mcp-cli` in the sandbox on every backend
except a non-persistent ecrou sandbox.

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
   `<workspace>/.rhema/runtime`, and only sandbox-only backends skip the
   root altogether.
3. **The sandbox installs the helper; nothing copies the host's binary.**
   A copy only works on the same OS and architecture, and a debug build
   is hundreds of megabytes. The published installer already detects
   the sandbox's `os_arch`, honors `RHEMA_INSTALL_DIR`, verifies the
   tarball's sha256, and installs `gob` beside `rhema`. The small CLI
   target makes the install cheaper, not different.
4. **MCP servers moved into the sandbox.** The host bridge and its
   socket are gone; `mcp-cli` runs in the sandbox and reads its own
   config. A server that must outlive a command runs under the
   sandbox's own process model, not a host bridge.
5. **Capabilities replace the sandbox gate.** `sandboxed.is_none()`
   disappears from `setup.rs`; the gate is per kit and per backend.
6. **Fail closed.** A kit that cannot be placed is absent from the
   prompt and the shell; it never half-works.
7. **The init file is generated inside the sandbox and static per kit
   version.** The function bodies are static (`job()` runs `gob` in a
   sandbox, `see()` stages an image, `search()` and `decide()` post to
   the host endpoint), and the per-session values are env. One file, no
   chain, no host write, no shared root.
8. **The shared root is for the install, the session tree, and the host
   endpoint when one exists.** Without it, the session tree is the
   sandbox's temp dir and the host pulls it. `search` and `decide` need
   either that socket or an SSH forward.
9. **Cleanup is a drop guard plus a start sweep.** A sandbox dir cannot
   ride a host `TempDir`, so the placer removes it explicitly, and the
   next start ages out what a crash left. The sweep is one level deep,
   owned by the sandbox user, and older than seven days.

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
- **Write the init per session from the host and chain five files.** The
  bodies are static and the values are env; the chain only composes
  text. Rejected: one generated file per kit version.
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
  mount; the fallback is a sandbox-side cache or the workspace root.
- Where does the root live when no login session exists (a cloud agent,
  a daemon)? `$XDG_RUNTIME_DIR` is absent there, so the cache fallback
  carries the lifecycle.
- For ssh, who allocates the forwarded port, and how does a stale
  forward get reclaimed when a session dies?
- Search/decide: the host endpoint is the answer on local, Docker, LXD
  containers, ecrou, and SSH. HTTP and LXD VMs still need a route.
- Http: can its `PUT` cap rise, or does a helper need an install URL
  the sandbox fetches itself?
- Does the installer need a `--version` flag, or is latest-plus-
  `KIT_VERSION` enough? The channel pointer is one version per channel
  today.
- For an airgapped sandbox, should the host fetch the target's tarball
  and upload it, rather than the sandbox reaching `get.rhema.rs`?
- Does `read_file` carry a multi-megabyte staged image? If the file API
  caps at 512 KiB, vision on an fs-disjoint backend needs a larger
  transfer or a shared root.
- Should the helper install be per sandbox instance or per user path?
  A persisted container keeps it; a recreated one reinstalls unless the
  runtime root's `bin/` survives.
- When the small helper ships, does the full `rhema` binary keep the
  hidden modes, or do they leave `rhema-code` entirely?
