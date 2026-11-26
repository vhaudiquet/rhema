# API notes for 0.5.0

Findings from the 0.4.0 review batch. Items 1-4, 7, and 8 landed on main after the tag; the signatures opened, and these notes keep the outcome on record. Item 5 is deferred past 0.5.0, for the reason in its section. Item 6 is mostly moot, as its heading says. 0.5.0 carries the breaking work that landed.

## 1. One shape for "run a command" (landed)

Was three signatures for one operation:

- `Sandbox::exec(request: ExecRequest, cancelled, progress)` (rhema-sandbox/src/lib.rs:136)
- `ShellExecutor::run(command, cwd, timeout, cancelled, progress)` (rhema-bash/src/exec.rs:165, five positional args)
- the `ExecCall { command, cwd, timeout_secs }` wire DTO (rhema-sandbox/src/protocol.rs:41)

Landed as: `RunRequest { command, cwd: PathBuf, timeout }` / `RunOutcome` in rhema-bash/src/exec.rs, re-exported by rhema-sandbox; both seams take the struct and return the outcome, and both methods are named `run`. `ExecRequest`/`ExecOutcome`/`Executed` are gone; `ExecCall`/`ExecReply` remain as the wire DTOs, and `From<&RunRequest>` bridges to `ExecCall`. The adapter's rebuild-and-copy body reduced to one forwarding call.

## 2. The progress closure's HRTB forced `fn` workarounds (landed)

`impl FnMut(&str) + Send` (or the dyn form `&mut (dyn for<'a> FnMut(&'a str) + Send)`) could not be satisfied by a plain closure; callers wrote dummy `fn silent(_: &str) {}` workarounds (three copies in-tree).

Landed as: the sandbox and executor seams take `&mut dyn ToolProgress` (rhema-core's trait, reused rather than a new `ProgressCallback`), re-exported from rhema-sandbox so the backends keep one import point. The shell tool passes its own channel straight through; the shim closure in shell.rs is gone. A no-op listener is `OutputOnly(|_| {})`.

## 3. One error vocabulary at the sandbox boundary (landed)

Landed as: `SandboxError::OutOfMemory` and `ShellError::OutOfMemory`, so a SIGKILL from outside the budget keeps its class through the adapter instead of collapsing into a generic failure; `exec_util::classify_exit` (the old `exit_status`) is the one exit-code classifier, 137 included, and the backends route through it. `From<std::io::Error> for SandboxError` is the one construction path for bare transport errors; the context-rich `Unavailable(format!(...))` sites stay, they name what failed. `MonitorError`/`ChatError*` no longer existed by the time this landed.

## 4. Cap constants renamed after what they cap (landed)

`MAX_STREAM_BYTES`/`MAX_CAPTURE_BYTES`/`MAX_LINE_BYTES` are now `EXEC_TAIL_BYTES`, `EXEC_CAPTURE_BYTES`, `EXEC_LINE_BYTES`. One correction to the original plan: the single definition of the line cap lives in rhema-bash (its pump uses it, and rhema-sandbox depends on rhema-bash, not the other way), with `exec_util` re-exporting it; the other two are exec_util's own. rhema-mcp's `MAX_LINE_BYTES` (4 MiB, MCP framing) is untouched; it caps a different layer.

## 5. `NONINTERACTIVE_ENV` re-export chain (deferred)

rhema-sandbox's exec_util re-exports `NONINTERACTIVE_ENV`, `OSC7577_TOKEN`, and `setsid_executable` from rhema-bash (`exec_util.rs:27`). rhema-sandbox-local is the only consumer, at `lib.rs:105`, `:120`, and `:125`; the docker and ssh backends do not touch the names.

Deferred past 0.5.0. The re-export is the spawn family's single import point, the way `EXEC_LINE_BYTES` reaches the backends (section 4), and rhema-sandbox already depends on rhema-bash with `default-features = false`, so nothing is gained by moving the definitions. rhema-core is not a home for them: it holds the agent loop and the provider contract and compiles into wasm hosts, while these three names describe how a child process is launched. If the bash tool must leave the backends' import lines, the alternative is a small process crate below both, not core.

## 6. Tougher closure bounds (open, mostly moot)

The `progress` HRTBs item 2 complained about are gone. The retry ladder's callbacks have the same shape; applying `ToolProgress`-style naming there is the remaining piece.

## 7. The resident tool seam deleted (landed)

`rhema-autonomous-agent` shipped `Allowlist`, the `Tool` trait, `ToolRegistry` with its allowlist-filtered resolver, `ResolverChain`, and the `TOOL.yaml` manifest loader, all under the "resident-agent core tier" label. The 0.4.0 consumer review (rhema-claw, the crate's only consumer, all 73 references audited) showed the loop half (`Source`, `Driver`, `SessionContext`) imports none of it; the registry is constructed, filled, and flattened into a resolver in the same breath at every call site; every allowlist in the wild is `permissive()`; and every claw tool arrives as manifest plus `ToolOp`, the `Tool` trait having no direct impls outside the crate's own `ManifestTool`. Packaging for one consumer, not framework API.

Landed as: the four modules deleted. The crate is the loop only (`Source`/`SourceError`, `Driver`/`Handler`/`SessionContext`, the `async_trait` re-export); `serde`/`serde_yaml` leave its dependency list and `serde_json` moves to dev-deps. Tool injection was never this crate's to provide: it is `rhema_core::ToolResolver`, untouched. The claw ports its native families (consents, attachments, phone) to claw-local resolvers, the manifest loader moving with them if the folder convention survives; its CRUD families become `clawctl` verbs over the daemon's HTTP surface, which already serves every verb. Spec §8's TOOL.yaml entry records the new home and the decision not to build `run: { exec }` (user-written tools are MCP's job). No `Chain` lands in rhema-core: the host composes through `Overlay`, and the claw's one chain site folds `Overlay`s or keeps a local struct.

## 8. The resident loop moved into `rhema-rsd` (landed)

After item 7 the crate was `Source`, `SourceError`, `Handler`, `Driver` and `SessionContext`: 313 lines with the tests, no implementation of `Source` or `Handler` anywhere inside it, one consumer, and the spec already naming those types (§3, §5). The reference implementation was shipping the spec's `Handler` in a sibling package.

Landed as: `rhema_rsd::driver`, behind a `driver` feature that brings `tokio` and `async-trait`. The dispatcher keeps its promise, no threads, tasks or timers; the crate docs and the module docs say which half needs the executor. The crate is gone from the workspace; the 0.4.x versions stay on crates.io and want yanking at release time. Both integration suites moved into `rhema-rsd/tests` behind `#![cfg(feature = "driver")]`. The pseudocode loop in `lib.rs` stays, now with a link to the code that runs it, so an embedder that wants the standard shape can take it.

The claw's port is mechanical: `rhema_autonomous_agent::X` becomes `rhema_rsd::X` with `features = ["driver"]`, and the default lease holder prints as `rhema` instead of `autonomous-agent`.

## Verified clean in this batch (no action needed)

The five crate-level READMEs with code (rhema-code, rhema-session-store, -file, -sqlite, rhema-submissions) verified against source: 0 broken commands, 0 non-compiling examples, 0 phantom features. The only doc edits made are the two in rhema-code's README (the `background: native` default and the fluff sentence). The install-block URL is network-form and was not reachable from this host; it matches `DIST_BASE`/update.rs and the release pipeline owns it.

`cargo clippy --workspace --all-targets --all-features -- -D warnings` and `cargo test --workspace --all-features` pass after the 1-4 batch.
