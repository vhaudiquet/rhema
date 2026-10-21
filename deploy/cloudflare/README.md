# Running the coding agent on Cloudflare

One Durable Object per session: WebSocket chat, streaming model
events, `bash` in a Cloudflare Sandbox container, checkpoints in the
DO's SQLite storage. A restart — of the DO, the container, or the
worker — resumes the session; a command that died with the container
comes back as an "interrupted" tool result, never silently re-run.

## Prerequisites

- A Cloudflare account with **Workers Paid**: Containers require it,
  and the bill includes container runtime (see the pricing caveat
  below).
- Node (for wrangler and worker-build), Rust with
  `wasm32-unknown-unknown` (`rustup target add wasm32-unknown-unknown`).

## Deploy

```sh
# from the repository root
npx worker-build --release     # builds crates/rhema-cloudflare-code into build/worker/
npx wrangler deploy -c deploy/cloudflare/wrangler.toml
```

First deploy creates the `CodingAgent` and `SandboxShim` Durable
Object classes and the container binding.

## Secrets

Two wrangler secrets, never config files, never DO storage:

```sh
npx wrangler secret put ANTHROPIC_API_KEY -c deploy/cloudflare/wrangler.toml
npx wrangler secret put SANDBOX_TOKEN     -c deploy/cloudflare/wrangler.toml
```

`ANTHROPIC_API_KEY` authenticates the model calls. `SANDBOX_TOKEN` is
any long random string; the agent DO sends it to the sandbox shim on
every call, and the shim refuses everything else. Generate one with
`openssl rand -hex 32`.

## Driving a session

Every session lives at `/sessions/<sessionId>/...`:

```sh
# Set up the session once: model, approval policy, seed, skills.
curl -X POST https://<worker>/sessions/demo/config \
  -H 'content-type: application/json' \
  -d '{
    "model": "claude-sonnet-4-5",
    "approval": "ask_client",
    "seed_git": "https://github.com/acme/widgets",
    "skills": [{ "name": "deploy",
                 "description": "Triggers on deploy tasks.",
                 "body": "---\ndescription: Triggers on deploy tasks.\n---\n..." }]
  }'

# Chat over a WebSocket: send {"type":"chat","text":"fix the build"},
# receive streaming events; answer approval requests with
# {"type":"approve","id":"...","approve":true}. Stop with {"type":"stop"}.
curl https://<worker>/sessions/demo/session   # the checkpointed record
```

Approval policies: `deny_all` (default), `allow_list` with
`{"allow_list": ["bash"]}`-style tool names, `allow_all`, and
`ask_client` — requests go over the socket, an unanswered ask denies
after 120 s.

## Limits and pricing

- **Subrequests**: one model fetch plus one or two sandbox calls per
  step. A step costs 2–3 subrequests against the plan's per-invocation
  budget (1000 on paid plans), so a 16-step turn spends well under
  a tenth of it. If turns ever grow past that, the alarm-driven step
  queue (one invocation per step) is the v2 answer, not limit-raising.
- **CPU**: the DO's work is orchestration; the model and the container
  do the heavy lifting off-thread. Wall-clock, not CPU, is the binding
  constraint, and workerd allows long-lived WebSocket requests.
- **Container pricing**: Containers bill for **runtime, not
  invocations** — a container that idles for its inactivity window
  still costs minutes. The shim keeps one warm for 15 minutes after
  the last command (`INACTIVITY_TIMEOUT_MS`); turn that down for
  cost, up for latency.
- **Workspace persistence**: files survive idle sleeps. A full
  container eviction loses them; the next seeded session re-clones.
  Treat the sandbox as a checkout, not a store — push early.

## What is where

| File | What it is |
|---|---|
| `wrangler.toml` | the Worker: DO bindings, SQLite migration, container, vars |
| `entry.mjs` | the deployable entry: re-exports the classes, routes `/sessions/<id>/...` |
| `sandbox-shim.mjs` | the JS DO wrapping `ctx.container` in the JSON surface the agent DO speaks |
| `../../crates/rhema-cloudflare-code/` | the Rust agent: WS protocol, turn loop, checkpoints, fetch transport |
