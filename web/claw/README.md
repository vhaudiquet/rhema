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
  the stream, and the stream opens on the newest turn before the live
  feed — a reload paints the latest conversation at once instead of
  replaying a session that may have run for weeks, and the turns above
  it come from `GET /v1/sessions/{key}/history`, one whole turn per
  page, as the reader scrolls up (or right away, while the transcript
  is shorter than the screen). So the transcript survives the browser,
  not just the claw.
- **Every frame names its seq, and a reconnect names it back.** The
  frame's `id:` is the record's seq for the event, and
  `useClaw().subscribeEvents` keeps the last one it delivered: a retry
  sends `?since=<seq>` and receives exactly the gap, so a dropped
  connection costs nothing on screen and nothing on the wire. The mirror
  is not reset on reconnect for the same reason — its items are still
  true. A `reset;` comment means the cursor named events the record no
  longer holds (a wiped database): the mirror drops, and the tail that
  follows rebuilds it.
- **SSE goes over fetch, not `EventSource`.** The claw's auth reads
  only the `Authorization: Bearer` header (`http.rs` `require_auth`); it
  has no query-param fallback, and `EventSource` cannot set headers. So
  `useClaw().subscribeEvents` holds a `GET` with the auth header, reads
  the body as a `ReadableStream`, and parses `data:`/`id:` frames by
  hand (`composables/useClaw.ts`). Drops retry with backoff; the hub's
  `lagged` comments surface as notices.
- **The dev proxy forwards the query string.** `server/middleware/
  claw-proxy.ts` rebuilds the upstream URL from the request's pathname,
  and `since`/`before` are the whole history protocol: dropping them
  silently turns every resume into a full replay and every history page
  into the tail.

## What the transcript does with the stream

- **Long pastes travel as chips.** A paste over three lines (the TUI's
  `PASTE_INLINE_LINES`) never lands in the composer; it stages as a
  droppable chip (`▣1 42L`) and embeds into the sent text as
  U+FFFC-payload-U+FFFC — the same record `rhema-core`'s
  `paste::embed` keeps — so the transcript renders each embed back as
  a chip in place of the payload. An unpaired mark is literal text.
- **The agent can ask.** A `consent_request` event renders as an
  inline card: kind `secret` gets a password input plus submit, kind
  `phone` gets Approve/Deny, and both carry a deny link
  (`DELETE /v1/consents/{id}`). The card flips when the answer lands
  and again on the stream's `consent_resolved` echo; nothing persists
  client-side, so a reload rebuilds the cards from the replay.
- **Attachments render inline.** An `attachment` event becomes a card
  — media glyph, file name, B/KB/MB size, caption. Images fetch the
  bearer-guarded bytes (`GET /v1/attachments/{id}`, blob → object URL,
  like the identity images) and render capped at 240px; code and file
  cards download on click. Object URLs cache per id, so a replay never
  refetches.
- **Your images ride along.** The composer's `+` — and an image on the
  clipboard — stage as removable chips (`▣1 photo.png 1.2 MB`), the
  claw's own budget enforced client-side (8 a message, 5 MiB decoded
  each; the draft stays required, the route 422s an empty text). The
  sent row renders the local previews at once; the stream's
  `user_message` echo carries the stored attachment ids and dedupes
  against that row, so a replayed message renders its own thumbnails
  through the same bearer-guarded fetch as an attachment card.
- **Sessions are created in place.** The sidebar's `+` form posts
  `POST /v1/sessions` with a client-validated name
  (`[A-Za-z0-9._-]{1,64}`), switches the chat to the new key, and
  lets 400/409 land in the error banner. The phone client's status
  keys (`phone:…`) stay out of the inventory; the full list stays on
  the wire.

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
takes an optional instance name; unnamed, the id is the kind. Model
options carry a `◉ vision` mark when the live listing knows the model
takes images (`supports_vision: true`); the key's absence is unknown,
never a no, so it renders nothing. A login
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
    ChatPanel.vue                transcript: sent lines (paste chips),
                                 streamed turns as bash shell lines and
                                 glyph rows, consent + attachment
                                 cards; the growing composer; agent
                                 replies render through AppMarkdown
    SessionsSidebar.vue          conversable dispatch keys (phone status
                                 keys hidden), interactive first; the
                                 + form creates a session
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
    useTranscript.ts             folds TurnEvents into renderable turn
                                 blocks; owns the paste-embed text form
server/
  middleware/claw-proxy.ts dev-only same-origin proxy to a running claw
```
