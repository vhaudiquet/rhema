# Rhema Android

The phone client of the claw (the resident agent daemon): Kotlin + Jetpack
Compose, single module, deliberately small. It chats with the agent, reports
phone status as events, and shows the agent's notifications as system
notifications.

## What it does

- **Settings**: first launch lands here until a base URL is saved and
  validated with `GET /healthz`. Stores the claw base URL and bearer token
  (plus a generated, persistent device ID) in app-private DataStore. Once
  connected, the screen also carries the **Model providers** card: the
  backend instances (`GET /v1/providers`), each one's live model
  listing, and the active pair staged as a draft behind an explicit save
  (`PUT /v1/providers/active`; it applies from the claw's next turn).
  Trade-off: the token is stored in plain DataStore rather than
  EncryptedSharedPreferences; `androidx.security:security-crypto` is an
  alpha-quality, unmaintained artifact, and the OS app sandbox already covers
  the realistic threat (other apps). Backups that could exfiltrate the file
  are disabled in the manifest (`allowBackup=false`,
  `dataExtractionRules`, `fullBackupContent`).
- **Chat**: session list (`GET /v1/sessions`, interactive sessions first,
  the phone's own `phone:*` status keys hidden)
  and one chat screen per session. While a chat screen is open the app holds
  one SSE connection (`GET /v1/sessions/{key}/events`, OkHttp streaming GET
  with the auth header, hand-parsed frames: blank-line delimited `data:`
  lines, `:` comment keep-alives skipped, unknown event types skipped). The
  stream opens with the claw's replay of the session's recorded turns before
  the live feed, so every (re)connection resets the transcript and rebuilds
  it from the replay. The
  composer POSTs `{"text"}`; the claw's documented dance is preserved:
  **subscribe first, then POST**, the reply arrives on the stream. Thinking
  deltas render dim and collapsible, tool rows show name/summary with error
  coloring (bash-like tools render as shell lines instead: dim `$` prefix,
  the command line, and an exit-code badge), pasted payloads embedded in a
  message as paired `U+FFFC` marks render as compact `▣N 5L` chips, and
  `attachment` events render as cards: file name, size, caption, an inline
  preview for small (≤ 2 MB) images, and a download-and-open row (bytes
  from `GET /v1/attachments/{id}` land in the app cache and open through a
  FileProvider `ACTION_VIEW`, the only way another app can read a private
  cache file). The agent's `consent_request` questions (a secret, or an
  approval of a phone control action) render as cards answered in place —
  `POST`/`DELETE /v1/consents/{id}` — and `consent_resolved` settles them
  (a reconnect's replay rebuilds the settled states). The streaming reply
  is finalized by `turn_done`. Replies
  render
  as markdown (`com.mikepenz:multiplatform-markdown-renderer`, GFM flavour:
  raw HTML stays literal, so model output never mounts as markup; the
  streaming cursor rides the source text). The transcript is a reversed
  LazyColumn: the newest turn anchors to the bottom edge, so streamed
  blocks stay on screen and a reader scrolled into history is not yanked
  back.
- **Phone status source**: a WorkManager periodic worker (15 min, the OS
  floor, `NetworkType.CONNECTED`) snapshots battery level, charging state,
  network metering (`wifi`/`metered`/`offline`) and DND, and POSTs **only
  transitions** vs the last successfully-posted snapshot (persisted in
  DataStore) to `POST /v1/ingest`. Battery transitions fire on charging
  flips and decile-bucket changes (`level/10`), so slow drain does not ping.
  Charging flips additionally kick a one-shot worker from a process-lifetime
  broadcast receiver (`ACTION_POWER_CONNECTED|DISCONNECTED`,
  `ACTION_BATTERY_CHANGED`). Manifest registration for those actions is
  refused by the OS (implicit broadcasts, API 26+), so the receiver is
  registered dynamically in the Application and stays dumb: it only
  enqueues work. Location is not touched in v1.
- **Notifications**: a second periodic worker polls
  `GET /v1/notifications?device=<id>&since=<cursor>` (cursor in DataStore),
  posts each as a system notification on channel `rhema` (tap opens the app;
  the API carries no session key, so it lands on the session list), then
  `POST`s an ack per id. Acks are best-effort: the cursor means "seen", not
  "acked", so a failed ack does not re-notify.
- **Permissions**: `INTERNET`, `POST_NOTIFICATIONS` (runtime request on
  API 33+, re-asked per cold start; the OS throttles denials),
  `RECEIVE_BOOT_COMPLETED` (boot receiver re-runs the idempotent KEEP
  schedule; WorkManager also re-enqueues its own work). Nothing else in v1.

## API contract (the claw's `/v1`)

All under one base URL; `Authorization: Bearer <token>` on `/v1`, never on
`/healthz`.

| Endpoint | Used for |
|---|---|
| `GET /healthz` | settings gate (no auth) |
| `GET /v1/providers` | backend instances + active pair |
| `GET /v1/providers/{id}/models` | an instance's live model listing |
| `PUT /v1/providers/active` | switch the backend instance, persisted |
| `GET /v1/sessions` | session list |
| `POST /v1/sessions/{key}/messages` | `{"text"}` → 202; reply via stream |
| `GET /v1/sessions/{key}/events` | SSE, replay-then-live: `user_message`, `thinking_delta`, `text_delta`, `tool_start`, `tool_result`, `turn_done`, `attachment`, `consent_request`, `consent_resolved` |
| `GET /v1/attachments/{id}` | an attachment event's bytes (bearer) |
| `GET /v1/consents` | pending consent questions, oldest first |
| `POST /v1/consents/{id}` | answer one: `{"secret"}` or `{"grant"}` |
| `DELETE /v1/consents/{id}` | deny / cancel a question |
| `POST /v1/ingest` | `{"source","key","payload"}` |
| `GET /v1/notifications?device=&since=` | ascending notification list |
| `POST /v1/notifications/{id}/ack` | best-effort ack |

### Ingest payloads emitted

`source` is always `"phone"`, `key` is `phone:<deviceId>:<topic>`:

```json
{"topic": "battery", "value": "83", "charging": true}
{"topic": "battery", "value": "17", "charging": false}
{"topic": "network", "value": "wifi"}     // or "metered" | "offline"
{"topic": "dnd", "value": "on"}           // or "off"
```

`battery.value` is the raw level; the transition rule is charging-flip or
decile bucket change. Unreadable sensors (e.g. no battery on a desktop boot)
produce no posts for that topic.

## Trade-offs

- **Notification latency**: no push infrastructure; background
  notifications are as fresh as the WorkManager interval (15 min). The chat
  screen is always live via SSE.
- **Transcript lifetime**: the transcript is screen-scoped (compose
  `remember`, SSE restarts on rotation/process death). The claw-side session
  is unaffected.
- **First launch**: nothing works until Settings validates `/healthz`.

## One-consumer principle

The claw's `/v1` is the contract. The app has no private endpoints and no
special privileges; every need goes through the same public API any other
client sees. If a feature seems to need a claw change, that change belongs
in the claw's public API first.

## Build

Requirements: JDK 17+ (Gradle 9.3.1 runs up to the current JDK; the system
default is fine), Android SDK with platform android-36 (set `ANDROID_HOME`
or `local.properties` `sdk.dir`).

```sh
ANDROID_HOME=~/Android/Sdk ./gradlew :app:assembleDebug   # debug APK
ANDROID_HOME=~/Android/Sdk ./gradlew :app:lintDebug       # lint
```

APK lands at `app/build/outputs/apk/debug/app-debug.apk`. Install and run:

```sh
adb install app/build/outputs/apk/debug/app-debug.apk
```

Pinned toolchain: Gradle 9.3.1 (wrapper), AGP 8.13.2, Kotlin 2.2.21
(`org.jetbrains.kotlin.plugin.compose` + `plugin.serialization`), Compose
BOM 2025.12.01, minSdk 26 / target & compileSdk 36.

## Layout

```
app/src/main/java/rs/rhema/phone/
  MainActivity.kt      nav state + POST_NOTIFICATIONS runtime request
  RhemaApp.kt          Application: container, channel, schedule, receiver
  di/AppContainer.kt   manual DI (OkHttp, Json, repos, api, notifier)
  data/                DataStore-backed settings + status repositories
  net/                 ClawApi (one object per endpoint, sealed result type)
  chat/ChatController  transcript state + SSE loop for the open screen
  ui/                  Theme, shared design components, SettingsScreen,
                       SessionsScreen, ChatScreen
  work/                PhoneStatusWorker, NotificationsWorker, receivers, scheduling
  notif/Notifier.kt    channel + system notifications
```

## Cleartext and certificates

The claw is self-hosted, so its address is whatever the operator points
at: a LAN name (`kosmos.lan`), a bare IP, a Tailscale name, where a
publicly-trusted certificate is rare. The app therefore permits
cleartext HTTP app-wide and trusts user-installed CAs (mkcert installs
into the user store, so a mkcert-secured claw works over HTTPS too).
Only the operator's configured claw is ever contacted, and the bearer
token is the auth; prefer HTTPS wherever the deployment has a real
certificate.

## Road notes

- Theming is Jadeite dark, always, the same tokens as the TUI and the
  web UI (`crates/rhema-code/src/palette.rs` DARK). No light scheme, no
  dynamic color: the palette is the brand. The screens mirror the web
  claw's redesign: cards with hairline-divided rows, state dots, badges,
  toned callouts, avatar-row transcripts with accent-bar tool cards, and
  the round accent send button; the shared pieces live in
  `ui/Components.kt`, the Compose counterparts of the web's components.
- Future onboarding: scanning a QR code from the claw (base URL +
  token, rendered by the web UI or printed in the terminal) instead of
  typing both by hand. Deliberately deferred; the Settings screen is
  the manual path today.
