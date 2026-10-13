# rhema claw web

A small, hand-rolled web client for the claw (`rhema-claw`): a live
transcript of one session, the session inventory, and a settings area
(model providers, about). Nuxt 4 + Vue 3 + TypeScript, no component
library, no store. It is strictly **one client** of the claw's public
`/v1` HTTP surface.

## Run it against a locally running claw

```sh
rhema-claw                 # the daemon, on 127.0.0.1:7799
cd web/claw
npm install
npm run dev                # http://localhost:3000
```

In dev, the `server/middleware/claw-proxy.ts` middleware forwards
`/v1` and `/healthz` to `http://127.0.0.1:7799` (`NUXT_CLAW_UPSTREAM`
overrides), so the browser sees a same-origin API and CORS never enters
the picture. If the claw is not running, the page still loads: the
down/401/stream banners tell you what is wrong.

### Token

When the claw config sets `token:`, every `/v1` route wants
`Authorization: Bearer <token>`. Give the UI a token either way:

- the environment: `NUXT_PUBLIC_CLAW_TOKEN=... npm run dev` (runtime
  config `clawToken`), or
- the inline field in the 401 banner; it is stored in `localStorage`.

`/healthz` is open either way, which is how the header dot stays honest
when the token is wrong.

## Build

```sh
npm run build      # nuxt generate -> .output/public, fully static
npm run typecheck  # vue-tsc over the whole app
```

`npm run build` produces a static SPA in `.output/public`, the artifact
an ingress serves. The ingress owns routing: same-origin `/v1` and
`/healthz` to the claw. `runtimeConfig.public.clawBase` can point the
client at another base, but the claw sends no CORS headers, so
cross-origin use needs a proxy, not a different origin.

## The one-consumer principle

The claw's `/v1` API is the contract (`crates/rhema-claw/src/http.rs`):
sessions, messages, events, providers. This client uses only public
routes, speaks the wire shapes the Rust side serializes, and adds
nothing server-side. UI needs never justify private endpoints; they
justify claw API changes, argued in the Rust tree.

Two consequences worth knowing before changing this code:

- **The daemon owns the conversation.** `POST .../messages` is
  fire-and-forget (202); the reply streams back on
  `GET /v1/sessions/{key}/events`. The transcript here is a mirror of
  the stream, and the stream opens with the claw's replay of the
  session's recorded turns before the live feed — a reload and every
  reconnect reset the mirror and rebuild from the replay, so the
  transcript survives the browser, not just the claw.
- **SSE goes over fetch, not `EventSource`.** The claw's auth reads
  only the `Authorization: Bearer` header (`http.rs` `require_auth`); it
  has no query-param fallback, and `EventSource` cannot set headers. So
  `useClaw().subscribeEvents` holds a `GET` with the auth header, reads
  the body as a `ReadableStream`, and parses `data:` frames by hand
  (`composables/useClaw.ts`). Drops retry with backoff; the hub's
  `lagged` comments surface as notices.

## Layout

The shell is an icon rail plus two views, switched by a tiny hash
router (`#/chat`, `#/settings/agent/general`, `#/settings/agent/
accounts`, `#/settings/providers`, `#/settings/about`), no
vue-router. The chat view is the session inventory plus the live
transcript; the settings view is a searchable nav plus pages. The
agent's identity (name, mark, avatar) loads
once and renders everywhere — rail, sidebar, chat, window title —
through `AgentAvatar`, which owns the precedence: avatar image, then
mark or initials. Settings pages stage edits into a local draft and
apply them with an explicit Save (`PUT /v1/providers/active` answers
the new pair only; the client carries the provider list forward).
The providers page also manages the roster itself: the add form renders
from the `addable` kinds the claw serves; each kind's `fields` carry
`{name, required}`, `required` marking what the daemon refuses the login
without (llama.cpp needs neither of its fields, it serves keyless on a
default url); the form demands only those, marks the rest optional, and
takes an optional instance name; unnamed, the id is the kind. A login
is `PUT /v1/providers/{id}/credentials`: apply immediately,
credentials persist; roster rows are backend instances (`id`, the kind
unless the login named it, plus the credential's `source`
badge, env-backed kinds return on restart after a removal), and the
remove button (`DELETE /v1/providers/{id}`) refuses the last backend
claw-side.

```
app/
  app.vue                        the shell: rail, banners, view switch
  assets/css/main.css            the Jadeite tokens (matches the TUI's
                                 palette.rs DARK) + card/badge primitives
  components/
    AppRail.vue                  the icon rail: views, claw pulse
    AppBadge.vue                 the status/tag pill
    AppCallout.vue               the toned inline note (info/ok/warn/error)
    AppMarkdown.vue              agent replies as markdown (markdown-it,
                                 raw HTML escaped, styles in main.css .md)
    ClawBanner.vue               the one error/notice banner surface
    ChatPanel.vue                transcript: sent lines, streamed turns,
                                 composer with the backend chip; agent
                                 replies render through AppMarkdown
    SessionsSidebar.vue          every dispatch key, interactive first
    PageHeader.vue               title + subtitle + page actions
    SettingCard.vue              one titled card of rows
    SettingRow.vue               name + description + control
    SettingsNav.vue              searchable settings nav
    StatusDot.vue                one honest state dot
    settings/
      ProvidersPage.vue          the backend switch: draft → Save/Discard
      AboutPage.vue              identity, stack, the routes spoken
  composables/
    useClaw.ts                   the typed API client + fetch-based SSE
    useTranscript.ts             folds TurnEvents into renderable turn blocks
server/
  middleware/claw-proxy.ts dev-only same-origin proxy to a running claw
```
