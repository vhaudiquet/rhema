# AGENTS.md

Conventions for working in this tree. They apply to every commit; the
whole history follows them.

## Before every commit

Run, in order, and make sure they are clean before committing:

```sh
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features   # keep green when touching behavior
```

The crate compiles with `#![forbid(unsafe_code)]` and
`#![deny(missing_docs)]`, so a build fails on any undocumented public
item — do not weaken the attributes to make it pass. `cargo fmt` may
amend files you did not touch — include those changes in the commit
(or in a separate `chore:` commit) rather than leaving the tree dirty.

## Commit messages

Follow Conventional Commits with a component scope:

```
<type>(<scope>): <short summary>
```

Rules:

- Types: `feat`, `fix`, `refactor`, `perf`, `test`, `docs`, `build`,
  `ci`, `chore`.
- Scopes match the component touched — the module name under
  `crates/rhema-core/src/`:
  - `message` — `Role`, `Content`, `Message` (`src/message.rs`)
  - `session` — the caller-owned `Session` (`src/session.rs`)
  - `request` — `ModelRequest`, `ChatParams`, `ToolSpec`
    (`src/request.rs`)
  - `output` — `ModelOutput`, `StopReason`, `Usage`, `Step`
    (`src/output.rs`)
  - `provider` — the `ModelProvider` seam (`src/provider.rs`)
  - `tool` — the `ToolResolver` seam (`src/tool.rs`)
  - `event` — `Event`, `EventSink` (`src/event.rs`)
  - `engine` — the `Agent` step engine and `run_turn`
    (`src/engine.rs`)
  - `mock` — `MockProvider` (`src/mock.rs`, `mock` feature)
  - `tests` — integration suites under `crates/rhema-core/tests/`
  - `google` — the Gemini provider adapter (`crates/rhema-provider-google/`)
  - `config` — stored settings and credentials (`crates/rhema-config/`)
  - `agent` — the coding-agent driver and tools (`crates/rhema-coding-agent/`)
  - `rhema` — the CLI binary and terminal UI (`crates/rhema-cli/`)
  - `tui` — the full-screen terminal UI (`crates/rhema-tui/`)
  - `deps` — dependency additions/bumps (manifests, lockfile)
- Omit the scope entirely for repo-wide changes that do not belong to
  a single component (root config, README.md, SPEC.md).
- Summary: imperative mood ("add", never "added" or "adds"), lowercase
  first letter, no trailing period, max ~72 characters.
- Body (optional): separated by a blank line, wrapped at 72 columns;
  explain why rather than what. Reference issues as `#123`.
- A commit touching several components should be split into one commit
  per component when practical; otherwise use the dominant scope.
- **No merge commits.** Integrate work by rebasing onto the target
  branch (`git rebase`, `git pull --rebase`, `git cherry-pick`); the
  history stays linear. When several work streams run in parallel, land
  them one rebase at a time.

### Examples

```
feat(engine): resolve tool calls inside a single step
fix(engine): append an error result for a tool use with no resolver
test(engine): cover event order for scripted tool calls
feat(mock): script multi-reply providers
docs: document the step semantics and session boundary
deps: bump tokio to 1.47
chore: apply cargo fmt
```

## Code

- The crate denies missing docs (`#![deny(missing_docs)]` in
  `crates/rhema-core/src/lib.rs`): every public item carries a doc
  comment, and the module list there is the layout map — keep it in
  sync when adding a module.
- The step engine has exactly two intended consumers (coding agent,
  RSD agent); the second is a falsification test on the seams, never a
  feature. Anything that smells like dispatch, leases, or persistence
  belongs to a later crate — see SPEC.md §1 and §7 test 5.
- Comments state constraints the code cannot show; no narration.
- Anything user-facing (public API, feature flags) is reflected in
  README.md before commit.
- Design decisions that outlive the session go to `SPEC.md`, not to
  new docs or repos.
