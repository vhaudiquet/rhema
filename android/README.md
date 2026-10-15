# Rhema Android

The phone client of the claw (the resident agent daemon): Kotlin + Jetpack
Compose, single module, deliberately small. It chats with the agent, reports
phone status as events, shows the agent's notifications as system
notifications, executes the agent's phone commands (DND, alerts), and
manages the device fleet the claw keeps.

## What it does

- **Settings**: a nav of submenus mirroring the web UI's settings
  sidebar — the "Phone" group (Status reporting, Devices) and the "Claw"
  group (Model providers, Connection), one page per entry behind a back
  arrow. First launch is the exception: it lands straight on the
  connection page until a base URL is saved and validated with
  `GET /healthz`. Stores the claw base URL and bearer token
  (plus a generated, persistent device ID) in app-private DataStore. The
  **Devices** page is the **Phones** card, **Status reporting** and
  **Model providers** their own pages (both described below): the
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
  and one chat screen per session. The applied events persist in a
  per-session cache (DataStore, newest 500 with the full count as the
  cursor), so a screen opens on its cached transcript at once and the
  stream connects with `?since=<count>` to receive only what is new
  instead of replaying the whole history; the cache flushes at every
  `turn_done` and when the screen closes. While a chat screen is open the
  app holds one SSE connection (`GET /v1/sessions/{key}/events`, OkHttp
  streaming GET with the auth header, hand-parsed frames: blank-line
  delimited `data:` lines, `:` comment keep-alives skipped, unknown event
  types skipped). The reader's own messages render as right-anchored
  bubbles (elevated surface, accent bar on the trailing edge); the
  agent's turns keep the avatar on the left. The
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
  is finalized by `turn_done`. The chat renders the claw's presentation identity
  (`GET /v1/agent`, cached in DataStore, refreshed per app open and on the session
  list's refresh): the avatar image when one is set (`GET /v1/agent/avatar`), else
  the initials, the name's first letter, or R; the agent's name labels every turn,
  falling back to "rhema". Replies
  render
  as markdown (`com.mikepenz:multiplatform-markdown-renderer`, GFM flavour:
  raw HTML stays literal, so model output never mounts as markup; the
  streaming cursor rides the source text). The transcript is a reversed
  LazyColumn: the newest turn anchors to the bottom edge, so streamed
  blocks stay on screen and a reader scrolled into history is not yanked
  back.
- **Phone status source**: a WorkManager periodic worker (15 min, the OS
  floor, `NetworkType.CONNECTED`) reads every sensor into one snapshot and
  POSTs **only transitions** vs the last successfully-posted snapshot
  (persisted in DataStore) to `POST /v1/ingest`. Each sensor has its own
  transition rule, tuned to what would otherwise spam:

  | Topic | Posts when | Needs |
  |---|---|---|
  | `battery` | charging flip, or decile bucket change (`level/10`) | nothing |
  | `network` | `wifi`/`metered`/`offline` changes | nothing |
  | `dnd` | on/off changes | nothing |
  | `location` | moved 300 m or more since the last posted fix | location permission |
  | `app` | the foreground package changes | usage access grant |
  | `device` | the identity fingerprint changes (basically once, and on app or OS updates) | nothing |
  | `wifi` | the connected SSID changes | location permission |
  | `alarm` | the next alarm moves | nothing |
  | `heartbeat` | always, when a run produced no other post | nothing |

  A sensor that is ungranted or unreadable produces no posts for its
  topic, never an error. Rich readings carry their own honesty: a
  location payload includes accuracy and fix age, the app payload the
  package, its label and how long it has been current. The location read
  requests a fresh fix (fused on API 31+, else gps then network) with a
  20 s ceiling and falls back to the newest last-known fix under 30 min;
  a plain last-known read would answer with whatever another app left
  cached, which on a stationary phone is where the phone was, not where
  it is. The heartbeat exists for the fleet's last-seen time: the claw
  stamps a device "seen" on whatever post lands, so a run with no
  transitions posts one anyway and the row stays fresh at the worker's
  cadence instead of the last flip.

  Every report is an observation on the claw: it lands in the phone
  tables (the ingest answers `{"ingested": "observed"}`) and a `phone`
  tool status question reads it. No report dispatches a turn, so a
  commute or a session of app-hopping is never a model call per move,
  and a battery flip never becomes a push notification.

  Charging flips additionally kick a one-shot worker from a process-lifetime
  broadcast receiver (`ACTION_POWER_CONNECTED|DISCONNECTED`,
  `ACTION_BATTERY_CHANGED`). Manifest registration for those actions is
  refused by the OS (implicit broadcasts, API 26+), so the receiver is
  registered dynamically in the Application and stays dumb: it only
  enqueues work.
- **Phone fleet**: the Settings screen's **Phones** card is the
  management surface over the claw's registry (`GET /v1/phones`), not a
  private view: every device the claw has heard from, its self-reported
  label (the `device` topic's manufacturer and model), its last-seen
  time, and a one-line state. This install's row is marked; every other
  row carries **Forget**, which `DELETE`s the device's records on the
  claw after a confirm dialog. A device id lives for one install, so a
  reinstall or a cleared app data leaves the old identity behind as a
  stale row with an old last-seen time; that row is what Forget is for.
  A forgotten phone that still reports re-registers on its next run.
  The **Status reporting** card names what this phone cannot send until
  a grant is flipped (location permission, usage access, DND access)
  and walks to the system toggles; the rows re-read on every ON_RESUME.
- **Phone commands**: a third periodic worker polls
  `GET /v1/phone/commands?device=<id>` — the agent's consented control
  queue — every 15 minutes like the other workers (that interval is the
  control latency; opening the app kicks a one-shot KEEP-coalesced run, so
  the queue is fresh while the phone is in hand). Commands execute in
  order, each answered with `POST /v1/phone/commands/{id}/result`, which
  doubles as the ack: a failed post leaves the command queued and the next
  run repeats it. `dnd_on`/`dnd_off` set the interruption filter; they
  need the special **Do Not Disturb access** grant (Settings → Special app
  access — a settings toggle, not a permission), and without it the worker
  reports that back and toggles nothing. `alert` posts a heads-up,
  sound-on notification on the high-importance `rhema-alert` channel with
  the args' `title`/`body` (falling back to "claw" / the action name).
  Unknown actions are reported, never guessed.
- **Notifications**: a second periodic worker polls
  `GET /v1/notifications?device=<id>&since=<cursor>` (cursor in DataStore),
  posts each as a system notification on channel `rhema` (tap opens the app;
  the API carries no session key, so it lands on the session list), then
  `POST`s an ack per id. Acks are best-effort: the cursor means "seen", not
  "acked", so a failed ack does not re-notify.
- **Permissions**: `INTERNET`, `POST_NOTIFICATIONS` (runtime request on
  API 33+, re-asked per cold start; the OS throttles denials),
  `RECEIVE_BOOT_COMPLETED` (boot receiver re-runs the idempotent KEEP
  schedule; WorkManager also re-enqueues its own work),
  `ACCESS_COARSE_LOCATION`/`ACCESS_FINE_LOCATION` (runtime request per
  cold start like notifications; a refusal only silences the `location`
  and `wifi` topics). Two grants are settings toggles, not permissions:
  usage access (`PACKAGE_USAGE_STATS` is declared only so the toggle can
  grant it) for the `app` topic, and Do Not Disturb access
  (`ACCESS_NOTIFICATION_POLICY`, declared only so the app appears on the
  "Do Not Disturb access" screen's list — an undeclared app opens that
  screen and finds nothing to flip) for the dnd commands. The Status
  reporting page in Settings shows all three.

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
| `GET /v1/sessions/{key}/events` | SSE, replay-then-live: `user_message`, `thinking_delta`, `text_delta`, `tool_start`, `tool_result`, `turn_done`, `attachment`, `consent_request`, `consent_resolved`. `?since=N` skips the replay's oldest N events — the transcript cache's cursor |
| `GET /v1/attachments/{id}` | an attachment event's bytes (bearer) |
| `GET /v1/consents` | pending consent questions, oldest first |
| `POST /v1/consents/{id}` | answer one: `{"secret"}` or `{"grant"}` |
| `DELETE /v1/consents/{id}` | deny / cancel a question |
| `GET /v1/phones` | the device fleet, labels and topics included |
| `DELETE /v1/phones/{device}` | retire a device's records |
| `GET /v1/phone/commands?device=` | the control queue for this device |
| `POST /v1/phone/commands/{id}/result` | execution result; doubles as the ack |
| `POST /v1/ingest` | `{"source","key","payload"}` |
| `GET /v1/notifications?device=&since=` | ascending notification list |
| `POST /v1/notifications/{id}/ack` | best-effort ack |

### Ingest payloads emitted

`source` is always `"phone"`, `key` is `phone:<deviceId>:<topic>`.
Every report is an observation on the claw: it feeds the status the
`phone` tool reads, and no turn is run (the ingest answers
`{"ingested": "observed"}`):

```json
{"topic": "battery", "value": "83", "charging": true}
{"topic": "battery", "value": "17", "charging": false}
{"topic": "network", "value": "wifi"}     // or "metered" | "offline"
{"topic": "dnd", "value": "on"}           // or "off"
{"topic": "location", "lat": 48.85, "lon": 2.35, "accuracy_m": 12.0, "provider": "fused", "age_s": 45}
{"topic": "app", "value": "com.google.android.maps", "label": "Maps", "age_s": 900}
{"topic": "device", "value": "Google Pixel 9", "manufacturer": "Google", "model": "Pixel 9", "android": "16", "sdk": 36, "app_version": "0.1.0"}
{"topic": "wifi", "value": "home"}
{"topic": "alarm", "value": 1760000000000, "in_min": 542}
{"topic": "heartbeat", "ts": 1760000000000}
```

`battery.value` is the raw level; the transition rule is charging-flip or
decile bucket change. `location` posts on a 300 m move from the last posted
fix. `app` excludes this app itself. `wifi` needs the location grant
(Android treats an SSID read as location) and is skipped when the system
answers with a placeholder. Unreadable or ungranted sensors produce no
posts for that topic.

## Trade-offs

- **Notification latency**: no push infrastructure; background
  notifications are as fresh as the WorkManager interval (15 min). The chat
  screen is always live via SSE.
- **Transcript cache**: the applied event log persists per session (the
  newest 500 events plus the full count), so a screen opens on its cached
  transcript and the stream resumes past it. The cache is a cursor over
  the claw's record, not a copy of it: a claw database wiped behind the
  phone's back leaves the cache pointing past a history that no longer
  exists, and that session replays nothing until it catches up.
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
ANDROID_HOME=~/Android/Sdk ./gradlew :app:assembleRelease # release APK
```

APK lands at `app/build/outputs/apk/debug/app-debug.apk`. Install and run:

```sh
adb install app/build/outputs/apk/debug/app-debug.apk
```

Debug builds are signed with the committed `debug.keystore` (the standard
Android Debug identity: store/key password `android`, alias
`androiddebugkey`), so any build installs over any other: a CI artifact
updates a locally-built install and vice versa. The key is deliberately
not a secret — debug keys guard nothing — a stable signature across
machines is the whole point. The build never touches
`~/.android/debug.keystore`, so other projects keep their own key.

Release builds are the opposite custody model: the key anchors the
future edge channel — whatever it signs can silently update an install
that trusts the channel — so it never travels with the source. CI holds
it as two repository secrets (`ANDROID_RELEASE_KEYSTORE`, the base64
keystore, and `ANDROID_RELEASE_KEYSTORE_PASSWORD`; alias
`rhema-release`), and the android workflow pins both certificates by
SHA-256 fingerprint before uploading an artifact. Locally, blank
environment leaves `assembleRelease` unsigned: a build check, never a
deliverable.

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
  sensors/             the status readers (location, foreground app, wifi, alarm, identity)
  net/                 ClawApi (one object per endpoint, sealed result type)
  chat/ChatController  transcript state + SSE loop for the open screen
  ui/                  Theme, shared design components, SettingsScreen,
                       SessionsScreen, ChatScreen
  work/                PhoneStatusWorker, NotificationsWorker,
                       PhoneCommandWorker, receivers, scheduling
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
- Distribution: the CI release artifact (`rhema-android-release`) is
  the future edge channel's payload — publishing it somewhere public
  (a Gitea generic package, an always-latest link) is deferred; the
  secret release key is already the channel's trust anchor, so the
  artifact is worth building and pinning now.
