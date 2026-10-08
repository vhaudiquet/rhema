# rhema claw — web

A small, hand-rolled web client for the claw (`rhema-claw`): a live
transcript of one session, the session inventory, and the watch table.
Nuxt 4 + Vue 3 + TypeScript, no component library, no store — it is
strictly **one client** of the claw's public `/v1` HTTP surface.

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

`npm run build` produces a static SPA in `.output/public` — the artifact
an ingress serves. The ingress owns routing: same-origin `/v1` and
`/healthz` to the claw. `runtimeConfig.public.clawBase` can point the
client at another base, but the claw sends no CORS headers, so
cross-origin use needs a proxy, not a different origin.

## The one-consumer principle

The claw's `/v1` API is the contract (`crates/rhema-claw/src/http.rs`):
sessions, messages, events, watches. This client uses only public
routes, speaks the wire shapes the Rust side serializes, and adds
nothing server-side. UI needs never justify private endpoints — they
justify claw API changes, argued in the Rust tree.

Two consequences worth knowing before changing this code:

- **The daemon owns the conversation.** `POST .../messages` is
  fire-and-forget (202); the reply streams back on
  `GET /v1/sessions/{key}/events`. The transcript here is a mirror of
  the stream: a reload starts empty, and history lives in the claw's
  session, not in the browser.
- **SSE goes over fetch, not `EventSource`.** The claw's auth reads
  only the `Authorization: Bearer` header (`http.rs` `require_auth`); it
  has no query-param fallback, and `EventSource` cannot set headers. So
  `useClaw().subscribeEvents` holds a `GET` with the auth header, reads
  the body as a `ReadableStream`, and parses `data:` frames by hand
  (`composables/useClaw.ts`). Drops retry with backoff; the hub's
  `lagged` comments surface as notices.

## Layout

```
app/
  app.vue                  the one page: header, banners, three panels
  assets/css/main.css      the Jadeite tokens (matches the TUI's palette.rs DARK)
  components/
    ClawBanner.vue         the one inline error/notice surface
    ChatPanel.vue          transcript: sent lines, streamed turns, composer
    SessionsSidebar.vue    every dispatch key, interactive first
    WatchesPanel.vue       the watch table + add/remove
  composables/
    useClaw.ts             the typed API client + fetch-based SSE
    useTranscript.ts       folds TurnEvents into renderable turn blocks
server/
  middleware/claw-proxy.ts dev-only same-origin proxy to a running claw
```
