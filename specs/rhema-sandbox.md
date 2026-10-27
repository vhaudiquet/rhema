# rhema-sandbox — the sandbox seam, its wire protocol, and the execution choice

> **Status:** design, 2026-10-20 (decisions in §9). The rollout in §7 is
> the commit plan; every phase lands green before the next starts.
> **Grounding:** SPEC.md §10–§11 (the cloud frontends; the harness
> extraction), `specs/rhema-cloudflare-code.md` (the shell seam and the
> shim contract this builds on).
> **One-line promise:** one `Sandbox` seam any frontend can aim at any
> machine — the host, a container, a VM, a box over SSH, a cloud sandbox —
> chosen by configuration, never by which crate a frontend was born in.

---

## 0. Scope

The `Sandbox` seam already exists and is already platform-free; what is
wrong is where it lives, what it is welded to, and who cannot reach it.
This spec moves the seam into its own crate, promotes an existing wire
surface to *the* sandbox protocol, and gives `rhema-code` a choice of
where `bash` runs.

**In scope:**

- `rhema-sandbox` — the seam crate of the sandbox family: the `Sandbox`
  trait and its types, the `ShellSandbox` adapter, the wire protocol, the
  HTTP client (`HttpSandbox`), the local sandbox (`LocalSandbox` over a
  host root), `rhema-sandboxd` (the protocol served from a host), the
  shared seeding helper, and the scripted mock.
- Deployment-selected backends in the cloud frontends: a Durable Object
  or a Vercel container may run `bash` on its platform's sandbox *or* on
  any machine speaking the protocol.
- The `rhema-code` execution choice: `local | docker | ssh | http` in v1,
  stored in config, remembered per workspace, pickable in the TUI.

**Out of scope, v1:** live output streaming (SSE) — the trait contract
already blesses completion-buffered delivery; the TLS/mTLS posture of
`sandboxd` on untrusted networks; VM and micro-VM *images* (that is
packaging: an image running `sandboxd` needs no new client code); a
Docker Engine API client (v1 drives the `docker` CLI); per-session
client-supplied sandbox URLs; cross-cloud session resume (a Store
problem, not a sandbox one).

## 1. The problem in this tree

Verified against this tree. Line numbers move; the file names are the
anchor. Re-verify before implementing if the tree has moved.

- **The seam is cloud-scoped.** `Sandbox` — `exec`, `seed`, `read_file`,
  `write_file`; object-safe, async, no platform dependency — lives in
  `crates/rhema-cloud-agent/src/sandbox.rs`, a crate whose lib doc opens
  "the coding-agent harness the cloud frontends share". `rhema-code`
  does not depend on it and has no path to it: `run.rs` builds
  `CodingAgentConfig::new`, whose default tools are `ShellTools::new` —
  the local process executor. The terminal agent can only ever run
  `bash` on the machine it was started on.
- **Frontend and sandbox are 1:1.** The Durable Object constructs
  `SandboxDo` itself (`worker/mod.rs`); the Vercel server constructs
  `VercelSandbox` itself (`server.rs`). Running the DO with `bash` on an
  EC2 box — or the Vercel container against a Cloudflare container —
  means forking a frontend.
- **The wire protocol exists but is unnamed.** `deploy/cloudflare/
  sandbox-shim.mjs` already specifies a complete sandbox surface —
  `exec`, `put`, `get`, `seed`, a token header, tail-capped streams,
  clamped timeouts — but its payload types are private (`ShimExec`,
  `ShimAck`, `ShimFile` in the cloudflare crate) and its only transport
  is a DO binding. It is one promotion away from being the sandbox
  protocol of the whole project.
- **Seeding is copy-pasted.** `ensure_seeded` exists twice — a DO meta
  table and a Postgres marker around the same logic: check the marker,
  git-clone the seed, write the skills.
- **The pieces below the seam are right.** `ShellExecutor`
  (`rhema-agent-tools/src/exec.rs`) is the exec-only seam; `ShellSandbox`
  adapts `Sandbox` onto it, so the `bash` tool — spec, caps, error
  shaping, approval — is unchanged wherever commands run; subagents
  inherit the executor (`ShellTools::executor()`, used by
  `rhema-subagents`). Nothing below the seam moves in this design.

## 2. The seam: what moves, what stays

`rhema-sandbox` mirrors the provider family: the contract and shared
machinery in one crate, one adapter per backend beside it.

```
crates/rhema-sandbox/
  Cargo.toml        # features: `local` (default), `mock` (default),
                    #           `client` (reqwest), `server` (axum)
  src/
    lib.rs          # the seam: `Sandbox`, `ExecRequest`, `ExecOutcome`,
                    #   `WorkspaceSeed`, `SandboxError` — moved verbatim
    adapter.rs      # `ShellSandbox`: `Sandbox` -> `ShellExecutor` — moved
    protocol.rs     # the wire shapes and routes (§3)
    fsroot.rs       # `FsRoot`: one host dir as the sandbox fs; path
                    #   mapping and the traversal guard (§4)
    local.rs        # `LocalSandbox`: `FsRoot` + process exec (`local`)
    http.rs         # `HttpSandbox`: the protocol over HTTPS (`client`)
    server.rs       # `rhema-sandboxd`: the protocol served (`server`)
    seed.rs         # `seed_workspace`: the shared half of `ensure_seeded`
    mock.rs         # `ScriptedSandbox` (`mock`) — moved
```

Dependency order stays clean:
`rhema-core ← rhema-agent-tools ← rhema-sandbox ← {rhema-cloud-agent,
rhema-cloudflare-code, rhema-vercel-agent, rhema-code}`. The default
build needs only serde and async-trait — wasm-clean, the same dep stance
`rhema-core` keeps; `client`, `server`, and `local` are feature-gated so
each consumer compiles only what it drives. House attributes apply:
`#![forbid(unsafe_code)]`, `#![deny(missing_docs)]`.

`rhema-cloud-agent` keeps the assembly, the gate, the config, and the
wire, and re-exports the moved names (`pub use rhema_sandbox::{...}`) so
its public surface does not churn; the re-exports go once callers import
the crate directly.

Three contract clauses move verbatim, because they are load-bearing:

- Output may arrive completion-buffered; the progress-callback shape is
  the seam a streaming transport fills in later.
- Cancellation is checked before the command starts; a command that
  completed remotely reports its true result — the model sees the truth
  whatever the turn decided meanwhile.
- The sandbox side caps streams before they cross the wire; the tool
  re-caps for display.

**The falsification test:** every backend in §4 must land with zero
trait changes. A backend that cannot express itself in
exec/seed/read/write is a trait bug — stop and fix the trait, not the
backend.

## 3. The wire protocol

The contract below is `deploy/cloudflare/sandbox-shim.mjs`'s documented
surface, promoted. `protocol.rs` owns the serde types (replacing the
private `Shim*` trio) and pins the shapes with tests, the way `wire.rs`
pins the client protocol. No version field in v1: a shape drift fails a
test here instead of a debugging session there.

- **Auth:** one shared secret in the `x-rhema-token` header on every
  route; a wrong or missing token is `401`.
- `POST /exec {command, cwd?, timeout_secs} → {exit_code, stdout,
  stderr, timed_out}` — one command, `bash -c` semantics, wall-clock
  budget. `timed_out` is the sandbox's own kill; the client maps it to
  `SandboxError::Timeout`.
- `POST /put {path, content_base64} → {error}` and
  `POST /get {path} → {content_base64 | error}` — single files, base64,
  `error: null` on success. Paths are sandbox-absolute POSIX.
- `POST /seed {git} | {tarball_base64, tarball_name?} → {error}` — the
  `WorkspaceSeed` ingress.

Caps are per-implementation policy, not protocol: the shim tail-caps
streams at 50 KiB and files at 512 KiB; `sandboxd` re-imposes equivalent
caps. What the protocol fixes is the shapes, the header, and the error
convention.

`HttpSandbox` (feature `client`) is this surface over plain HTTPS — the
same request/reply handling `SandboxDo` performs over a DO binding,
minus the binding. `rhema-sandboxd` (feature `server`) serves it from a
host: one root directory per session name under its base, exec through
the `timeout` prefix (§4), files and seeds straight on the fs, streams
capped before the reply, the token checked on every route. A DO pointed
at a `sandboxd` on any EC2 box, home server, or CI runner needs no
container binding at all — the shim, the containers-beta config, and the
wrangler plumbing become optional.

## 4. The backends

Three axes separate every choice on the list: (1) filesystem semantics —
does the runtime *share* the host fs or is its fs *disjoint*; (2) how
exec is wrapped; (3) what transport reaches it. The trait absorbs the
first — it is why the trait carries `seed`/`read_file`/`write_file` at
all, which an exec-only seam cannot express; the protocol absorbs the
third; the second is the implementation's business.

Backends fall into two families:

- **fs-sharing** — `FsRoot` plus an exec prefix: `LocalSandbox`
  (identity prefix), a bubblewrap/nsjail variant (the wrapper as argv
  prefix), a bind-mounted container (`docker exec` prefix).
  `read_file`/`write_file`/`seed` are plain fs under one root.
  `FsRoot` maps sandbox-absolute paths onto that root and refuses `..`
  escapes in both directions — the paths are model-influenced.
- **fs-disjoint** — every fs op crosses a boundary: copied-workspace
  containers, VMs, SSH remotes, the protocol family.

**The workspace has two postures, and the seam holds both.** A
workspace is either **copy-in** — born on the machine the frontend
runs on, so the sandbox must receive it — or **native** — born where
the executor lives, so copying is meaningless. `rhema-code` on a
laptop is the copy-in case: `local` and the mount postures make the
copy the mount itself, a copied container takes `docker cp`, and
`sandboxd` copies through the protocol's `put`/`get`. SSH is copy-in
with no mount — nothing yet answers "how does the current folder get
there", which is the mode's open piece (§8). A Durable Object serving
claw is the native case: its workspace storage IS the sandbox fs, exec
and the file ops meet in one place, and the meta-table seed marker is
the whole story. The trait stays silent about provenance — that
silence is the design — but the posture rides the frontend and the
backend together, and no assembly may assume one posture for every
backend.

| choice | the implementation | new code |
|---|---|---|
| local direct | `LocalSandbox`: processes rooted at a host dir | small |
| bwrap / nsjail | the same `FsRoot`, a wrapped argv | ~one wrapper each |
| container, v1 | the `docker` CLI: bind-mount posture = `FsRoot` + `docker exec`; copied posture = `docker cp` | medium |
| container, later | the Engine API over the unix socket, behind its own seam (the `SandboxApi` precedent) | medium |
| LXD / Incus | `rhema-sandbox-lxd`: one driver, two instance types. A container takes the docker bind-mount posture (`lxc exec` prefix, a `disk` device mapping the host root in) or the fs-disjoint one via `lxc file`; a `--vm` instance is the same driver with a launch flag — and the first true isolation tier in the offer | medium |
| VM / microVM | an image running `rhema-sandboxd` — packaging, not code | ~zero |
| SSH | daemonless: exec = `ssh host -- bash -c` (live lines through the existing pump), files via `ssh cat`/`tee`, seed via `ssh git clone`; or `sandboxd` on the remote behind `ssh -L` — then it *is* `HttpSandbox` | medium / zero |
| cloud sandboxes | `SandboxDo` (transport unchanged, types from `protocol.rs`); `VercelSandbox` (the vendor adapter, kept) | done |

Kill semantics: backends without a per-command kill API (docker exec
among them) run every command under `timeout --kill-after=5 N bash -c …`
— the shim's trick — so `ExecRequest::timeout` stays the single knob
everywhere. Live output is a transport property: local and SSH pump real
lines today; HTTP-family backends deliver at completion until a
streaming shape exists (§8).

## 5. `rhema-code`: choosing where `bash` runs

The choice is the `ProviderChoice` pattern applied to machines: a
`SandboxChoice` (`local | docker | ssh | http` in v1) with
`from_stored`/`stored_name`; a `sandbox` section in `rhema-config`
(`kind`, `url`, token reference); per-workspace memory beside the
remembered profile (`last_sandbox_in`/`note_sandbox_in`); a TUI picker
cloned from `ProfileScreen`; and availability probing like
`startup_profile` — the offered list depends on the machine (a docker
socket present, `ssh` configured, a `sandboxd` reachable).
`RHEMA_SANDBOX` overrides the stored kind, the env-wins convention
`RHEMA_PROVIDER` set.

Assembly has two paths, and the split is the one the cloud already drew:

- **`local`** is today's fast path, unchanged: `ShellTools::new` → the
  local process executor. No choice made, no new code runs.
- **every other kind** builds an `Arc<dyn Sandbox>`, seeds it, and wraps
  it in `ShellSandbox` for `CodingAgentConfig.tools`. The workspace
  becomes the sandbox-side path (`/workspace` for http and copied
  containers; the named directory for ssh and mounts); skill *sources*
  are read host-side as today, skill *bodies* are written into the
  sandbox through `seed_workspace`; the base SYSTEM.md follows the
  skills.

What stays host-side in every mode is already host-side: credentials and
profiles (`rhema-config`), session history (`$XDG_CACHE_HOME/rhema/
sessions` — verified, not workspace-relative), the clipboard, the TUI
itself. That is the whole migration for `rhema-code`: no state moves;
only the tool's executor and the prompt's workspace string change.

The v1 set is `local | docker | lxd | ssh | http` — lxd landed as one
driver and a launch flag, containers and `--vm` instances alike
(§9): `rhema-sandbox-lxd` execs through `lxc exec`, mounts a `disk`
device 1:1 for the container posture, and rides `lxc file` attached
for the VM one. bubblewrap/nsjail and microVM images land later with
zero trait change — that is the test the abstraction has to pass
(§2). SSH ships daemonless first: zero setup on the far
end is the mode's reason to exist; tunnel-plus-`sandboxd` is the
documented fast path. The instance is prepared, not assumed: the
ensure step provisions a standard tool set — git, ripgrep, fd-find,
jq, sd, curl, python3, build-essential — once per instance,
marker-gated and idempotent, so a stock image arrives ready for the
bash tool's sessions. A failed provisioning is a degraded sandbox,
not a dead one: the session runs, and the tools say what exists
when probed; the absent marker retries on the next assembly. Exec
runs as the workspace's owner — the instance's uid map and the
`--user` flag name the pair the tree answers to — and an empty cwd
lands at the workspace, not the instance's default directory.

The choice UI is an instance list, not a kind list: the host's own
`local` row, then the named instances the config stores — a user's
name over a kind and that kind's one target — then the add row. The
add dialog offers the ready kinds only (docker, lxd), ssh and http
out of the picker — not absent from the contract, just honest about
their standing (ssh's working tree has no carry story yet, §8; http
presumes a daemon to reach) — and asks nothing else: no target is
collected, because the concrete name is the workspace's to set,
derived at assembly time and stable across sessions. Pinning a
target — an existing container, an LXD VM, a host — is config.yaml's
edit. What is configured still shows and still runs: a stored ssh
instance lists with its not-offered mark, and `/sandbox <kind>`
takes it when the probe offers it. Enter runs the row's instance —
the section's active pointer names it, so two instances of one kind
switch without touching the kind — and `d` removes the instance
outright, the active pointer going with it.
The bar for a kind joining the add dialog is a story, not a
checkbox.

On safety, plainly: `local` and the mount postures are process hygiene,
not security boundaries; copied containers and VMs are the real tiers.
v1 does not couple the approval policy to the sandbox choice — today
every `rhema-code` mode auto-approves, and changing that is the approval
UX's question (§8) — and the gate stays installed in every mode either
way.

## 6. How we know it works

- `protocol.rs` pins the shapes with literal-JSON tests (the `wire`
  precedent). `SandboxDo` adopts the shared types; its existing offline
  tests catch any drift.
- `http.rs` + `server.rs`: one loopback integration test — spawn
  `sandboxd` on an ephemeral port, drive exec (success, timeout,
  nonzero exit), seed (git and tarball), get/put, a wrong token, and a
  stream cap. The client↔server contract is exercised, not asserted.
- `fsroot.rs`/`local.rs`: unit tests for path mapping, `..` traversal
  refusal in both directions, and seeding into the root.
- The offline e2e keeps its shape: `ScriptedSandbox` + `MockProvider`,
  now from the sandbox crate's `mock` feature — the same suite the
  cloud frontends inherit from.
- `VercelSandbox` keeps its `SandboxApi`-seamed offline tests unchanged.

## 7. Rollout

Per-scope commits (AGENTS.md); each lands green.

1. `feat(sandbox): extract the Sandbox seam into rhema-sandbox` — move
   the trait, the types, `ShellSandbox`, `ScriptedSandbox`; cloud-agent
   re-exports. Zero behavior change.
2. `feat(sandbox): pin the sandbox wire protocol` — `protocol.rs` and
   its shape tests.
3. `feat(sandbox): the HTTP sandbox client` — `http.rs`, feature
   `client`.
4. `feat(sandbox): the local sandbox and the shared seeding` —
   `fsroot.rs`, `local.rs`, `seed.rs`, the traversal tests.
5. `feat(sandbox): rhema-sandboxd` — `server.rs` and the loopback test.
6. `feat(deploy): the rhema-sandboxd image` — a Dockerfile beside
   `deploy/docker/`.
7. `feat(cloudflare-code): select the sandbox backend from the
   environment` — `SANDBOX_BACKEND=cloudflare|http`; `SandboxDo` over
   the shared protocol types; the shim and the containers-beta config
   become optional under `http`.
8. `feat(vercel-agent): select the sandbox backend from the
   environment` — `SANDBOX_BACKEND=vercel|http`.
9. `feat(rhema-code): the sandbox choice` — `SandboxChoice`, the config
   section, per-workspace memory, the picker, seeding on non-local
   kinds, README.
10. `docs: the sandbox seam and the execution choice` — SPEC.md §12,
    AGENTS.md scope entries.

Phases 1–2 are pure motion plus naming and de-risk everything after;
phase 7 delivers the stated goal (a DO with `bash` on arbitrary
infrastructure); phase 9 is the one users see.

## 8. Explicitly not decided here

- **Streaming:** the shape (SSE, chunked replies, a ws upgrade) and
  whether it rides the trait (a second exec entry point) or the
  transport. The callback contract already anticipates it.
- **`sandboxd` lifecycle:** workspace GC/TTL, concurrency limits, the
  `--no-gc` posture for dedicated boxes. The shim inherits eviction from
  Cloudflare; a standalone daemon needs its own answer.
- **Transport security:** TLS fronting vs mTLS for a `sandboxd` on an
  untrusted network; the token is a bearer secret and nothing more.
- **Protocol versioning:** shapes are pinned by tests; a version route
  or header waits for a second incompatible consumer.
- **Approval defaults per isolation tier:** today local auto-approves
  while the cloud default is deny-all; tiering that is the approval
  UX's decision, not this spec's.
- **Cross-cloud resume:** a session checkpointed in DO storage resuming
  against Vercel's Postgres is Store portability, not sandboxing.
- **The copy-in story for ssh:** what carries the working tree to the
  remote, and when — a git-remote sync, an rsync/tar push at startup
  and on demand, or `sandboxd` behind `ssh -L` (then the protocol's
  `put`/`get` IS the story). Until one lands, ssh stays daemonless
  exec with seeded files, honest about not mirroring the working tree.

## 9. Decisions

- **An own crate, not `rhema-core` or `rhema-agent-tools`** — SPEC.md §0
  commits the engine to "does not sandbox"; and `agent-tools` rides into
  every consumer (memory, acp, subagents), where axum/reqwest would
  follow or drown the crate in feature gates. The provider family is
  the shape to copy: a contract crate, per-backend adapters beside it.
- **The wire protocol is the shim's surface, promoted** — it is already
  specified, already spoken by one client and one server, and already
  handles the hard parts (token, caps, timeouts). A second protocol
  would be a worse version of adopting this one.
- **Backend selection rides deployment env, not `SessionConfig`** — the
  sandbox is a deployment decision; and a per-session client-chosen URL
  is an SSRF on a server that executes commands.
- **`rhema-code` keeps the direct local fast path** — the zero-flag
  invocation must not grow a sandbox abstraction in its hot path; the
  choice machinery engages only non-local kinds.
- **`VercelSandbox` stays** — it is a legitimate per-vendor adapter (the
  `rhema-provider-anthropic` of sandboxes); it just stops being the
  only choice its frontend can make.
- **v1 output may arrive buffered** — the trait contract already allows
  it; live lines are a transport property, and the callback is the seam
  that receives them when a streaming shape lands.
- **SSH ships daemonless first** — zero remote setup is the mode's
  reason to exist; tunnel-plus-`sandboxd` is documented as the fast
  path, not required plumbing.
- **The `timeout` prefix, not kill APIs** — uniform `ExecRequest::
  timeout` semantics across backends that lack a per-command kill; the
  shim has run the pattern in production shape.
- **No approval coupling in v1** — the gate stays installed everywhere;
  tiering defaults per isolation would change two variables where this
  design changes exactly one (where `bash` runs).
- **VMs are packaging** — if an isolation tier costs no client code,
  the abstraction is right; the success test is that §4's "later" rows
  need no trait change.
- **Both workspace postures stay first-class** — copy-in (the laptop
  folder carried to the sandbox: the mount, `docker cp`, the protocol's
  `put`/`get`, ssh's story still to land) and native (the DO's storage
  IS the workspace; copying there is meaningless). The seam serves
  both; an assembly that assumes one posture everywhere is wrong.
- **LXD is one driver and a launch flag, not two kinds** — `lxc exec`
  and `lxc file` speak to containers and `--vm` instances alike, so
  the instance type lives in the lxd config section, not in the choice
  vocabulary. The VM type is the point: local and mounts are process
  hygiene, and an LXD VM is the strongest isolation tier a
  `rhema-code` host can offer without a cloud.
- **The settings page is an instance list** — the host's `local`
  row, the stored instances, the add row last. The add dialog
  advertises the ready set only (docker, lxd) while ssh and http
  instances stay addable through `config.yaml`, where they list with
  their marks like any other. The UI's silence is a feature-status
  statement, not a capability one: `/sandbox <kind>` still runs a
  configured kind the probe offers, enter runs a stored instance
  through the section's active pointer, and `d` removes the instance
  outright — record and pointer, never the stored kind.
