# Rhema Claw

Still Work-In-Progress.

Rhema Claw is an autonomous personal agent ("claw"), built on the Rhema agent framework.

## Features

## The claw: `rhema-claw`

The resident agent. One daemon owns every session in one SQLite store:
yours (`interactive:<name>`, keyed per chat surface) and every watched
forge thread's. Talk to it over HTTP; it watches repos, answers forge
threads with coding-agent turns, and configures its own watches when
asked in chat (`watch_repo`/`unwatch_repo`/`list_watches` are tools the
model calls like any other).

```sh
rhema-claw                     # serve on 127.0.0.1:7799 (config: listen)
rhema-claw --once              # one detect + one dispatch pass, then exit
```

For the debug loop, daemon plus UI against throwaway state and zero
credentials: `scripts/dev.sh --mock` (canned streamed replies; the UI
is at `localhost:3000`, the claw at `127.0.0.1:7799`). Drop `--mock`
and set the provider's key env var (`CLAW_DEV_PROVIDER` picks the
provider) to debug against a real model.

```yaml
# ~/.config/rhema/claw.yaml
provider: anthropic
model: claude-sonnet-4-5
listen: 127.0.0.1:7799
token: change-me                 # optional; when set, /v1/* requires it
poll_interval_secs: 120
max_turn_steps: 2048             # tool rounds one chat turn may run
watches:                         # a seed: imported while the table is empty
  - forge: github
    repo: acme/widgets
```

`max_turn_steps` caps one turn's tool rounds (the coding driver's own
budget shape, and the same default, 2048): the claw is meant to work
unattended for a long time, so a cap this deep is a runaway guard, not
a work timer — a turn that hits it ends with a failed outcome and the
conversation intact; a follow-up message keeps going.

The surface refuses to start on a non-loopback `listen` with no
`token`: anything beyond loopback can reach the port, and the bearer
token is what stands between it and the agent's tools. Set a token, or
bind a loopback address. Resident sessions run the coding agent with
its approval seam auto-approved: tool calls, the shell included,
execute without a prompt. The daemon says so at startup, and the
bearer-gated surface plus the watch list are the trust boundary.

The native surface:

```
GET  /healthz                          GET  /v1/sessions
POST /v1/sessions/{key}/messages       {"text": "..."}   -> 202
GET  /v1/sessions/{key}/events         SSE turn events (see below)
GET  /v1/watches                       POST /v1/watches  DELETE /v1/watches/{forge}/{repo}
GET  /v1/bootstrap                     {"soul": bool, "user": bool}
POST /v1/ingest                         {"source","key","payload"} -> 202
GET  /v1/notifications?device=&since=   POST /v1/notifications/{id}/ack
GET  /v1/providers                      the backend instances + the active pair
PUT  /v1/providers/active               {"provider","model"?} — switch, persisted
PUT  /v1/providers/{id}/credentials     add or replace an instance, persisted
DELETE /v1/providers/{id}               remove one (the last is refused)
GET  /v1/providers/{id}/models          the live model listing
```

### Identity and first-run

The claw's prompt is assembled per turn from an in-binary base plus
three identity files under the state dir's `bootstrap/` (config:
`bootstrap_root`): **SOUL.md**, who the claw is, tone and purpose,
written with you; **USER.md**, the always-visible summary of who you
are; **ENVIRONMENT.md**, host, shell, and tool inventory, generated
deterministically at every boot (detection is code, not conversation;
hand edits are overwritten). The base renders the dispatch's own tool
roster too — every tool's name, description, and call schema, straight
from the resolver chain, so what the prompt lists is what the request
advertises. On first run the claw *offers* a short
setup in chat: who you are, what the machine is for, who it should be.
It writes the first two files itself through the `bootstrap_save`
tool as you answer; decline and it simply keeps offering, never
interrogating. Both files stay hand-editable forever; `/v1/bootstrap`
lets a UI badge setup state.

SSE frames are JSON, `data: {..}`: `user_message`, `thinking_delta`,
`text_delta`, `tool_start`, `tool_result`, `turn_done`. A subscription
opens with the session's recorded turns replayed first and the live feed
after, cut atomically daemon-side — every streamed event is persisted as
it streams, so both a reload and a daemon restart rebuild the whole
transcript, not a recent window; a session that runs for weeks scrolls
back to its first message. A chat UI subscribes once,
POSTs messages, and resets its mirror whenever the stream (re)opens,
letting the replay rebuild it; the daemon owns the conversation, so the
client mirrors the stream rather than managing history. The OpenAI shim
and the message POST read the live feed only — replay never reaches a
turn that is starting.

Every provider whose credentials resolve (env, the claw's own stored
logins, or the stored rhema config) is constructed at boot as a backend
*instance* named by an id, the kind unless a login named it, so two
llama.cpp servers (or two keys of one cloud) coexist under two names.
One is active, and switching with `PUT /v1/providers/active` is runtime
state persisted in SQLite: the
config's `provider`/`model` pair is only the seed, and when the config
names no provider the first credential discovery actually finds (a
stored login, an env key, or the stored rhema config) is active; the
keyless llama.cpp fallback never wins a seed, so the chart pins
nothing and a switch never needs a restart.

The claw registers RSD sources three ways: watches (forge), scheduled
tasks the agent creates in chat (`schedule_task` with `at` or
`every_secs`; a due task dispatches `task:<id>` through the resident
agent, whose reply auto-notifies the phone), and a generic door,
`POST /v1/ingest`, that any external emitter addresses
(`phone:<device>:<topic>`
keys from the Android app, for example). The resident agent owns
`interactive:`, `task:`, and `phone:` keys; forge threads own the rest.
Its `notify` tool queues a notification for a device; the device polls
and acks.

Stock OpenAI-speaking UIs point at the shim instead:

```sh
curl http://127.0.0.1:7799/v1/chat/completions -H 'Content-Type: application/json' -d '{
  "model": "claw", "user": "web", "stream": true,
  "messages": [{"role": "user", "content": "what are you watching?"}]
}'
```

The shim reads only the last user message (`user` picks the session,
default `web`), maps thinking deltas to `reasoning_content`, and
streams standard chat-completions chunks. There is one notion of
session, the daemon's, behind a wire format every web UI already speaks.

## Deploying the claw

```sh
docker build -f deploy/docker/claw.Dockerfile -t rhema-claw .
helm install claw deploy/helm/rhema-claw \
  --set token=$(openssl rand -hex 24) \
  --set secret.stringData.ANTHROPIC_API_KEY=... \
  --set secret.stringData.GITHUB_TOKEN=... \
  --set config.provider=anthropic --set config.model=claude-sonnet-4-5
```

Expose it with the chart's optional ingress (`ingress.enabled` +
`ingress.host`, TLS strongly recommended, bearer tokens cross it) or
keep it cluster-private behind Tailscale; the claw is bearer-gated
either way. With the chart's UI on (`ui.enabled`, the default), the
same host serves both: the browser gets the web UI on `/`, the daemon
answers `/v1` and `/healthz`; one address for the browser and the
phone app, no CORS anywhere.

The image is the daemon plus `bash` and `git` (the agent's shell is
load-bearing); the chart runs exactly one replica, the claw is
single-node by design: one SQLite store, one dispatcher. The
store and workspaces live on a PVC, `claw.yaml` in a ConfigMap (db and
workspace paths are pinned to the mounted volume), credentials in a
Secret, and the bearer token is the `token` value, required since the
daemon binds all interfaces in-cluster. `persistence.enabled=false`
trades restart survival for disposability.

The UI in `web/claw` is one client of the same surface: `npm run dev`
proxies to a local claw, `npm run build` emits the static bundle an
ingress serves next to `/v1`. The phone is another. `android/` is the
Kotlin/Compose client: chat over the same SSE stream, phone status
(battery, charging, network, DND transitions) posted to `/v1/ingest`
as `phone:<device>:<topic>` events, and agent notifications polled
onto the lock screen (no push infrastructure; a 15-minute WorkManager
floor, live SSE while the app is open). See `android/README.md`.