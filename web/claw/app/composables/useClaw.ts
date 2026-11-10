// The typed client for the claw's HTTP surface (crates/rhema-claw/src/
// http.rs): sessions inventory, message ingest, turn-event streaming,
// and watch CRUD. One consumer, one contract — everything here mirrors
// the wire shapes the Rust side serializes; nothing else exists.
//
// Why fetch-based SSE: the claw's auth middleware reads only the
// `Authorization: Bearer` header (http.rs `require_auth`); it has no
// query-param fallback, and `EventSource` cannot set headers. So the
// event stream is a plain GET with the auth header, read as a
// ReadableStream and parsed by hand into `data:` frames — the cost of
// keeping the token out of URLs and access logs.
import type { Ref } from 'vue'

/** What a consent asks of the human: a secret string, or an approve/
 * deny on a phone control action. */
export type ConsentKind = 'secret' | 'phone'

/** One consent_request event, and one entry of `GET /v1/consents`. */
export interface ConsentRequest {
  id: string
  kind: ConsentKind
  summary: string
  detail: string
  created_at: number
}

/** The body `POST /v1/consents/{id}` takes: per kind, exactly one of
 * the two shapes. */
export type ConsentAnswer = { secret: string } | { grant: boolean }

/** What an attachment's bytes are: an image to render inline, a code
 * or generic file to download. */
export type AttachmentMedia = 'image' | 'code' | 'file'

/** One attachment event. */
export interface ClawAttachment {
  id: string
  name: string
  media: AttachmentMedia
  size: number
  caption: string | null
}

/** One image a sent message carries, as the `POST .../messages` body
 * takes it: the full MIME type plus bare base64 (http.rs `WireImage`). */
export interface OutboundImage {
  media_type: string
  data: string
}

/** One image a `user_message` event carries: the attachment id the
 * bytes live under (fetch them via `attachmentUrl`) plus the MIME type
 * — megabytes never ride the stream (hub.rs `UserImage`). */
export interface UserImage {
  id: string
  media_type: string
}

/** One turn event, exactly `hub.rs`'s `TurnEvent` (serde type tag, snake case). */
export type TurnEvent =
  | { type: 'user_message'; text: string; images: UserImage[] }
  | { type: 'thinking_delta'; text: string }
  | { type: 'text_delta'; text: string }
  | { type: 'tool_start'; name: string }
  | {
      type: 'tool_result'
      name: string
      is_error: boolean
      summary: string
      /** Process exit code; only command-like tools carry one. */
      code: number | null
      /** The shell command line; only the bash tool carries one. */
      command: string | null
    }
  | ({ type: 'consent_request' } & ConsentRequest)
  | { type: 'consent_resolved'; id: string; granted: boolean }
  | ({ type: 'attachment' } & ClawAttachment)
  | { type: 'turn_done'; reply: string | null; outcome: string }

/** One entry of `GET /v1/sessions` (http.rs `sessions`). */
export interface ClawSession {
  key: string
  state: string
  revision: number
  inbox_depth: number
  last_outcome: string | null
}

/** One watch (config.rs `Watch`, as the watches routes accept and return it). */
/** One row of a provider's live model listing (`ModelCard` server-side). */
export interface ModelCard {
  id: string
  display_name: string
  description: string
  input_token_limit?: number | null
  /** Whether the model takes image input, when the adapter knows:
   * absent means unknown — no verdict, never a `false`. */
  supports_vision?: boolean
}

/** One account of `GET /v1/accounts` (rhema-identity's `Account`):
 * redacted by construction — the wire shape has no secret field. */
export interface ClawAccount {
  id: string
  label: string
  service: string
  url: string | null
  username: string | null
  secret_kind: 'token' | 'password'
  created_at: number
  updated_at: number
}

/** The payload `POST /v1/accounts` takes. The secret is write-only: it
 * is stored and never served back. */
export interface NewAccount {
  label: string
  service: string
  url?: string
  username?: string
  secret: string
  secret_kind?: 'token' | 'password'
  /** Also follow this forge identity's notifications. */
  watch?: boolean
}

/** The `POST /v1/accounts` answer: the save verdict plus whether the
 * forge watch got registered. */
export interface SavedAccount {
  saved: boolean
  id: string
  label: string
  service: string
  secret_kind: 'token' | 'password'
  watched: boolean
  watchable_forge: boolean
}

/** The `DELETE /v1/accounts/{id}` answer: whether the account existed,
 * and how many derived watches went with it. */
export interface ForgotAccount {
  forgot: boolean
  watches_removed: number
}

/** One backend instance of the providers roster: named by its `id` —
 * the kind unless a login named it — with the credential's origin. */
export interface ClawProvider {
  /** The instance id: the kind unless the login named it. */
  id: string
  /** The backend kind the instance serves as. */
  kind: string
  default_model: string
  /** Where the credential resolves from: `env` (survives a removal
   * until restart), `stored` (this surface persisted it), `profile`
   * (the stored rhema config), or `keyless`. */
  source?: 'env' | 'stored' | 'profile' | 'keyless'
}

/** One field of a kind's login form: the name the credentials body
 * spells it with, and whether the daemon refuses the login without it. */
export interface ProviderField {
  name: string
  required: boolean
}

/** One kind a runtime login can add, with the fields its form takes. */
export interface AddableKind {
  kind: string
  fields: ProviderField[]
}

export interface ProvidersState {
  /** `provider` is the active instance's id. */
  active: { provider: string; model: string }
  providers: ClawProvider[]
  addable?: AddableKind[]
}

/** The agent's presentation identity as `GET /v1/agent` serves it:
 * unset text fields read as null, the images report presence. */
export interface AgentIdentity {
  name: string | null
  mark: string | null
  initials: string | null
  tagline: string | null
  signoff: string | null
  avatar: boolean
  logo: boolean
}

/** The identity's text fields, as a save carries them: an empty
 * string clears the field. */
export interface IdentityTexts {
  name: string
  mark: string
  initials: string
  tagline: string
  signoff: string
}

/** One of the two identity images. */
export type AgentImageKind = 'avatar' | 'logo'

/** The credentials a provider login carries: the instance name (the
 * kind when omitted), the kind (required on an id's first login), an
 * API key (a GitHub token for copilot, the sign-in JSON for chatgpt),
 * and a server URL for
 * llama.cpp. Empty optionals are omitted, never sent as "" strings. */
export interface ProviderCredentials {
  name?: string
  kind?: string
  api_key?: string
  url?: string
}

/** The `PUT /v1/providers/active` answer: the new pair only — the
 * provider list is not echoed and the caller carries it forward. */
export interface ActiveBackend {
  active: { provider: string; model: string }
}

/** A failed API call: the status plus the claw's `error` string when it sent one. */
export class ClawError extends Error {
  /** HTTP status; 0 when the claw was unreachable at all. */
  readonly status: number

  constructor(status: number, message: string) {
    super(message)
    this.status = status
  }
}

/** Lifecycle of the event stream; anything but `open` is banner-worthy. */
export type StreamStatus = 'connecting' | 'open' | 'reconnecting'

const INTERACTIVE_PREFIX = 'interactive:'

/** How long the stream may go silent before this client gives up on the
 * connection and reconnects: the claw sends a keep-alive comment every
 * fifteen seconds (hub.rs), so three missed ones means the path is
 * black-holed — a half-open TCP after a laptop sleep shows the same
 * 'live' badge a healthy stream does without this check. */
const STREAM_SILENCE_TIMEOUT_MS = 45000

/** Interactive keys are the only ones message ingest accepts (http.rs `messages`). */
export function isInteractive(key: string): boolean {
  return key.startsWith(INTERACTIVE_PREFIX)
}

/** The session name `POST /v1/sessions` accepts; the key becomes
 * `interactive:<name>`. Anything else is a 400. */
export const SESSION_NAME_PATTERN = /^[A-Za-z0-9._-]{1,64}$/

/** The `POST /v1/sessions` answer: the new session's dispatch key. */
export interface NewSession {
  key: string
}

function base(): string {
  const config = useRuntimeConfig()
  return config.public.clawBase.replace(/\/$/, '')
}

function authHeaders(token: string | null): HeadersInit {
  return token ? { Authorization: `Bearer ${token}` } : {}
}

async function unwrap(response: Response): Promise<unknown> {
  if (response.ok) return response.json()
  let detail = `HTTP ${response.status}`
  try {
    const body = (await response.json()) as { error?: string }
    if (typeof body.error === 'string') detail = body.error
  } catch {
    // Non-JSON error body: the status line is the message.
  }
  throw new ClawError(response.status, detail)
}

async function send(path: string, init: RequestInit): Promise<unknown> {
  let response: Response
  try {
    response = await fetch(base() + path, init)
  } catch (cause) {
    throw new ClawError(0, `claw unreachable: ${String(cause)}`)
  }
  return unwrap(response)
}

function get(path: string, token: string | null): Promise<unknown> {
  return send(path, { headers: authHeaders(token) })
}

/** `GET /healthz` — open even when /v1 is tokened. Resolves true when the claw answers. */
export async function clawHealth(): Promise<boolean> {
  try {
    const response = await fetch(`${base()}/healthz`)
    return response.ok
  } catch {
    return false
  }
}

/** Handlers for a turn-event stream. */
export interface StreamHandlers {
  /** One turn event, in stream order, with the record's seq for it — the
   * cursor a reconnect names. Null only for a frame whose `id:` was
   * missing or unparseable. */
  onEvent: (event: TurnEvent, seq: number | null) => void
  /**
   * The cursor named events the record no longer holds (a wiped or
   * reset daemon). The mirror must go: the tail that follows this
   * notice rebuilds it.
   */
  onReset?: () => void
  /** Connection lifecycle; `reconnecting` follows a drop, with backoff. */
  onStatus?: (status: StreamStatus) => void
  /**
   * Best-effort delivery notices from the stream itself — the hub's
   * `lagged` comment (events dropped) and drop/retry chatter. Never
   * turn events, never silent.
   */
  onNotice?: (notice: string) => void
}

/** One page of a session's older turns, as `GET /v1/sessions/{key}/
 * history` answers it: whole turns, oldest first, each event with the
 * seq the record gave it. `more` is false once the page reached the
 * session's first event. */
export interface HistoryPage {
  events: { seq: number; event: TurnEvent }[]
  more: boolean
}

export interface ClawApi {
  /** `GET /v1/sessions` — every dispatch key with its status. */
  sessions(): Promise<ClawSession[]>
  /** `POST /v1/sessions` — create an interactive session from a name;
   * 400 for a name off `SESSION_NAME_PATTERN`, 409 when it exists. */
  createSession(name: string): Promise<NewSession>
  /** `GET /v1/consents` — the pending consents, oldest first. */
  listConsents(): Promise<ConsentRequest[]>
  /** `POST /v1/consents/{id}` — answer one consent; 409 when already
   * resolved, 422 for an empty secret. */
  resolveConsent(id: string, answer: ConsentAnswer): Promise<void>
  /** `DELETE /v1/consents/{id}` — deny or cancel a consent. */
  denyConsent(id: string): Promise<void>
  /** `GET /v1/attachments/{id}` — the attachment's bytes as an object
   * URL for an `<img>` or a download (the route is bearer-guarded, so
   * a plain src will not do). Cached per id: attachments are immutable,
   * so a replayed event never refetches. */
  attachmentUrl(id: string): Promise<string>
  /** `POST /v1/sessions/{key}/messages` — fire-and-forget: the claw answers 202 and the reply arrives on the event stream. Images ride the body only when there are some; without them the body is the old `{"text"}` shape byte-for-byte. The route 422s an empty text even with images aboard. */
  sendMessage(key: string, text: string, images?: OutboundImage[]): Promise<void>
  /** `GET /v1/providers` — the constructible backends and the active pair. */
  providers(): Promise<ProvidersState>
  /** `PUT /v1/providers/active` — switch the backend, persisted claw-side. Answers the new active pair only. */
  setActive(provider: string, model?: string): Promise<ActiveBackend>
  /** `GET /v1/providers/{id}/models` — the live listing; 404 when none. */
  models(id: string): Promise<ModelCard[]>
  /** `PUT /v1/providers/{id}/credentials` — add a backend instance (or
   * replace an id's credentials) at runtime; answers the new providers
   * state. */
  setCredentials(id: string, credentials: ProviderCredentials): Promise<ProvidersState>
  /** `DELETE /v1/providers/{id}` — remove a backend instance; answers
   * the new providers state. The last backend is refused claw-side. */
  removeProvider(id: string): Promise<ProvidersState>
  /** `GET /v1/accounts` — the stored accounts, redacted. */
  accounts(): Promise<ClawAccount[]>
  /** `POST /v1/accounts` — store an account (the secret is write-only),
   * optionally registering its forge notifications watch. */
  addAccount(account: NewAccount): Promise<SavedAccount>
  /** `DELETE /v1/accounts/{id}` — forget an account; its derived
   * watches go with it. */
  removeAccount(id: string): Promise<ForgotAccount>
  /** `GET /v1/agent` — the agent's presentation identity. */
  agent(): Promise<AgentIdentity>
  /** `PUT /v1/agent` — save the named text fields (empty clears);
   * fields left out are untouched claw-side. */
  setAgent(texts: Partial<IdentityTexts>): Promise<AgentIdentity>
  /** `PUT /v1/agent/{kind}` — upload the avatar or logo from a file.
   * The claw refuses non-images and bodies over 2 MiB. */
  setAgentImage(kind: AgentImageKind, file: File): Promise<AgentIdentity>
  /** `DELETE /v1/agent/{kind}` — drop the image. */
  clearAgentImage(kind: AgentImageKind): Promise<AgentIdentity>
  /** `GET /v1/agent/{kind}` — the image as an object URL for an
   * `<img>` (the route is bearer-guarded, so a plain src will not do).
   * The caller revokes the URL when it replaces it. */
  agentImage(kind: AgentImageKind): Promise<string>
  /**
   * Stream one session's turns. Resolves with a handle once the fetch
   * is started; the claw keeps the stream open forever, and a drop is
   * retried with backoff until `close()` is called.
   *
   * Every frame names the record's seq, so a reconnect resumes from the
   * last one delivered and replays only the gap. A first open with no
   * `since` names nothing, and the claw sends the newest turn plus the
   * live feed; older turns come from `history` as the reader scrolls
   * up. A `reset` comment means the cursor named events the record no
   * longer holds: the mirror is dropped and the tail that follows
   * rebuilds it.
   */
  subscribeEvents(
    key: string,
    handlers: StreamHandlers,
    options?: StreamOptions,
  ): { close: () => void }
  /**
   * `GET /v1/sessions/{key}/history` — one page of whole turns ending
   * at `before` (absent: the newest). Pages stack: the next call passes
   * the seq of the oldest event held, and stops when `more` is false.
   */
  history(key: string, before?: number | null, limit?: number): Promise<HistoryPage>
}

/** How `subscribeEvents` opens: `since` is the seq of the last event the
 * caller already holds, or null for "nothing, give me the tail". */
export interface StreamOptions {
  since?: number | null
}

/** Build the client bound to one bearer token (null = unguarded claw). */
export function useClaw(token: Ref<string | null>): ClawApi {
  /** Object URLs already minted for attachments, by id. Attachments are
   * immutable, so the URLs live as long as the page does. */
  const attachmentUrls = new Map<string, string>()

  /** The bytes behind a bearer-guarded route, as a blob. */
  async function fetchBlob(path: string): Promise<Blob> {
    let response: Response
    try {
      response = await fetch(`${base()}${path}`, {
        headers: authHeaders(token.value),
      })
    } catch (cause) {
      throw new ClawError(0, `claw unreachable: ${String(cause)}`)
    }
    if (!response.ok) {
      throw new ClawError(response.status, `HTTP ${response.status}`)
    }
    return response.blob()
  }

  async function sessions(): Promise<ClawSession[]> {
    return (await get('/v1/sessions', token.value)) as ClawSession[]
  }

  async function createSession(name: string): Promise<NewSession> {
    return (await send('/v1/sessions', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', ...authHeaders(token.value) },
      body: JSON.stringify({ name }),
    })) as NewSession
  }

  async function listConsents(): Promise<ConsentRequest[]> {
    const body = (await get('/v1/consents', token.value)) as { consents: ConsentRequest[] }
    return body.consents
  }

  async function resolveConsent(id: string, answer: ConsentAnswer): Promise<void> {
    await send(`/v1/consents/${encodeURIComponent(id)}`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', ...authHeaders(token.value) },
      body: JSON.stringify(answer),
    })
  }

  async function denyConsent(id: string): Promise<void> {
    await send(`/v1/consents/${encodeURIComponent(id)}`, {
      method: 'DELETE',
      headers: authHeaders(token.value),
    })
  }

  async function sendMessage(key: string, text: string, images?: OutboundImage[]): Promise<void> {
    await send(`/v1/sessions/${encodeURIComponent(key)}/messages`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', ...authHeaders(token.value) },
      body: JSON.stringify(images?.length ? { text, images } : { text }),
    })
  }

  async function history(
    key: string,
    before?: number | null,
    limit?: number,
  ): Promise<HistoryPage> {
    const params = new URLSearchParams()
    if (before !== undefined && before !== null) params.set('before', String(before))
    if (limit !== undefined) params.set('limit', String(limit))
    const query = params.size ? `?${params}` : ''
    const body = (await get(
      `/v1/sessions/${encodeURIComponent(key)}/history${query}`,
      token.value,
    )) as { events?: { seq?: unknown; event?: unknown }[]; more?: unknown }
    // The wire shape is trusted only as far as it parses: an event the
    // parser does not know is dropped, and its seq with it — the page
    // still renders what it could read.
    const events = (body.events ?? []).flatMap((entry) => {
      const event = parseTurnEvent(JSON.stringify(entry.event))
      return event && typeof entry.seq === 'number' ? [{ seq: entry.seq, event }] : []
    })
    return { events, more: body.more === true }
  }

  async function providers(): Promise<ProvidersState> {
    return (await get('/v1/providers', token.value)) as ProvidersState
  }

  async function setActive(provider: string, model?: string): Promise<ActiveBackend> {
    return (await send('/v1/providers/active', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json', ...authHeaders(token.value) },
      body: JSON.stringify({ provider, ...(model ? { model } : {}) }),
    })) as ActiveBackend
  }

  async function models(id: string): Promise<ModelCard[]> {
    const body = (await get(`/v1/providers/${encodeURIComponent(id)}/models`, token.value)) as {
      models: ModelCard[]
    }
    return body.models
  }

  async function setCredentials(id: string, credentials: ProviderCredentials): Promise<ProvidersState> {
    return (await send(`/v1/providers/${encodeURIComponent(id)}/credentials`, {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json', ...authHeaders(token.value) },
      body: JSON.stringify(credentials),
    })) as ProvidersState
  }

  async function removeProvider(id: string): Promise<ProvidersState> {
    return (await send(`/v1/providers/${encodeURIComponent(id)}`, {
      method: 'DELETE',
      headers: authHeaders(token.value),
    })) as ProvidersState
  }

  async function accounts(): Promise<ClawAccount[]> {
    const body = (await get('/v1/accounts', token.value)) as { accounts: ClawAccount[] }
    return body.accounts
  }

  async function addAccount(account: NewAccount): Promise<SavedAccount> {
    return (await send('/v1/accounts', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', ...authHeaders(token.value) },
      body: JSON.stringify(account),
    })) as SavedAccount
  }

  async function removeAccount(id: string): Promise<ForgotAccount> {
    return (await send(`/v1/accounts/${encodeURIComponent(id)}`, {
      method: 'DELETE',
      headers: authHeaders(token.value),
    })) as ForgotAccount
  }

  async function agent(): Promise<AgentIdentity> {
    return (await get('/v1/agent', token.value)) as AgentIdentity
  }

  async function setAgent(texts: Partial<IdentityTexts>): Promise<AgentIdentity> {
    return (await send('/v1/agent', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json', ...authHeaders(token.value) },
      body: JSON.stringify(texts),
    })) as AgentIdentity
  }

  async function setAgentImage(kind: AgentImageKind, file: File): Promise<AgentIdentity> {
    return (await send(`/v1/agent/${kind}`, {
      method: 'PUT',
      headers: { 'Content-Type': file.type || 'application/octet-stream', ...authHeaders(token.value) },
      body: file,
    })) as AgentIdentity
  }

  async function clearAgentImage(kind: AgentImageKind): Promise<AgentIdentity> {
    return (await send(`/v1/agent/${kind}`, {
      method: 'DELETE',
      headers: authHeaders(token.value),
    })) as AgentIdentity
  }

  async function agentImage(kind: AgentImageKind): Promise<string> {
    return URL.createObjectURL(await fetchBlob(`/v1/agent/${kind}`))
  }

  async function attachmentUrl(id: string): Promise<string> {
    const cached = attachmentUrls.get(id)
    if (cached) return cached
    const url = URL.createObjectURL(await fetchBlob(`/v1/attachments/${encodeURIComponent(id)}`))
    attachmentUrls.set(id, url)
    return url
  }

  function subscribeEvents(
    key: string,
    handlers: StreamHandlers,
    options?: StreamOptions,
  ): { close: () => void } {
    let closed = false
    let attempt = 0
    /** The seq of the last event this stream delivered: what a retry
     * names as `since`, so a reconnect replays the gap and nothing else.
     * Null until a frame arrives, or after a reset drops the mirror. */
    let cursor: number | null = options?.since ?? null
    /** The attempt in flight; `close` aborts it mid-read. */
    let current: AbortController | null = null
    /** Set when the watchdog, not the server, cut the attempt — the
     * notice says so instead of naming an opaque abort error. */
    let timedOut = false
    let silence: ReturnType<typeof setTimeout> | null = null

    const rearmSilence = (): void => {
      if (silence !== null) clearTimeout(silence)
      silence = setTimeout(() => {
        if (closed) return
        timedOut = true
        current?.abort()
      }, STREAM_SILENCE_TIMEOUT_MS)
    }
    const stopSilence = (): void => {
      if (silence !== null) clearTimeout(silence)
      silence = null
    }

    const status = (next: StreamStatus) => {
      if (!closed) handlers.onStatus?.(next)
    }

    void (async () => {
      while (!closed) {
        status(attempt === 0 ? 'connecting' : 'reconnecting')
        const controller = new AbortController()
        current = controller
        timedOut = false
        rearmSilence()
        try {
          const resume = cursor === null ? '' : `?since=${cursor}`
          const response = await fetch(
            `${base()}/v1/sessions/${encodeURIComponent(key)}/events${resume}`,
            {
              headers: {
                Accept: 'text/event-stream',
                ...authHeaders(token.value),
              },
              signal: controller.signal,
            },
          )
          if (!response.ok || !response.body) {
            throw new ClawError(response.status, `HTTP ${response.status}`)
          }
          attempt = 0
          status('open')
          await readFrames(
            response.body,
            (data, id) => {
              rearmSilence()
              const event = parseTurnEvent(data)
              if (!event) return
              const seq = id !== null && /^\d+$/.test(id) ? Number(id) : null
              if (seq !== null) cursor = seq
              handlers.onEvent(event, seq)
            },
            (comment) => {
              // Keep-alives come through here too; each one proves the
              // path alive.
              rearmSilence()
              // The record lost what this mirror holds: drop the cursor
              // so the next open asks for the tail, and let the consumer
              // drop its items before the tail that already followed
              // this notice folds in.
              if (comment.startsWith('reset')) {
                cursor = null
                handlers.onReset?.()
                return
              }
              // A skipped run of events is not a hole in the cursor:
              // the next frame's id names the seq after it, and a drop
              // before then reopens at the old cursor and replays what
              // was skipped.
              if (comment.startsWith('lagged')) {
                handlers.onNotice?.(`stream lagged — ${comment.replace(/^lagged;\s*/, '')}`)
              }
            },
          )
          if (closed) break
          throw new ClawError(0, 'stream closed by the claw')
        } catch (cause) {
          if (closed) break
          // Backoff caps at 15s: a live stream of a running turn is the
          // UI's whole job, so retries stay eager but not tight-looped.
          const backoff = Math.min(15000, 1000 * 2 ** attempt++)
          handlers.onNotice?.(
            timedOut
              ? `the event stream went quiet (no keep-alive for ${STREAM_SILENCE_TIMEOUT_MS / 1000}s); reconnecting in ${backoff / 1000}s`
              : `event stream dropped (${cause instanceof Error ? cause.message : String(cause)}); retrying in ${backoff / 1000}s`,
          )
          status('reconnecting')
          await new Promise((resolve) => setTimeout(resolve, backoff))
        } finally {
          stopSilence()
          if (current === controller) current = null
        }
      }
    })()

    return {
      close() {
        closed = true
        stopSilence()
        current?.abort()
      },
    }
  }

  return {
    sessions,
    createSession,
    listConsents,
    resolveConsent,
    denyConsent,
    sendMessage,
    history,
    providers,
    setActive,
    models,
    setCredentials,
    removeProvider,
    accounts,
    addAccount,
    removeAccount,
    agent,
    setAgent,
    setAgentImage,
    clearAgentImage,
    agentImage,
    attachmentUrl,
    subscribeEvents,
  }
}

/** One parsed SSE frame: the joined `data:` payload, comments excluded. */
type FrameHandler = (data: string, id: string | null) => void

/**
 * Reads an SSE body into frames. Frames split on a blank line; `data:`
 * lines carry payload (joined with newlines), `id:` names the event's
 * seq, and `:`-prefixed lines are comments (the claw's keep-alive, its
 * lag notice, and its reset). `event:` lines do not occur here.
 */
async function readFrames(
  body: ReadableStream<Uint8Array>,
  onFrame: FrameHandler,
  onComment: (comment: string) => void,
): Promise<void> {
  const reader = body.getReader()
  const decoder = new TextDecoder()
  let buffer = ''
  const step = (frame: string) => {
    const lines = frame.split('\n')
    const data = lines
      .filter((line) => line.startsWith('data:'))
      .map((line) => line.replace(/^data: ?/, ''))
      .join('\n')
    if (data) {
      const id = lines
        .find((line) => line.startsWith('id:'))
        ?.slice(3)
        .trim()
      onFrame(data, id ?? null)
      return
    }
    const comment = lines
      .filter((line) => line.startsWith(':'))
      .map((line) => line.slice(1).trimStart())
      .join('\n')
    if (comment) onComment(comment)
  }
  for (;;) {
    const { done, value } = await reader.read()
    if (done) break
    buffer += decoder.decode(value, { stream: true })
    let boundary = buffer.indexOf('\n\n')
    while (boundary !== -1) {
      step(buffer.slice(0, boundary))
      buffer = buffer.slice(boundary + 2)
      boundary = buffer.indexOf('\n\n')
    }
  }
}

/** The images a `user_message` carries: rows recorded before images
 * rode the transcript carry no field at all, and an entry without an
 * id and an image media type is not one — both fold to fewer (or no)
 * images rather than a guessed shape. */
function parseUserImages(raw: unknown): UserImage[] {
  if (!Array.isArray(raw)) return []
  const images: UserImage[] = []
  for (const entry of raw) {
    const image = entry as Record<string, unknown>
    if (typeof image.id !== 'string' || typeof image.media_type !== 'string') continue
    if (!image.media_type.startsWith('image/')) continue
    images.push({ id: image.id, media_type: image.media_type })
  }
  return images
}

/** Parses one `data:` payload into a `TurnEvent`; unknown shapes are dropped, not guessed. */
function parseTurnEvent(data: string): TurnEvent | null {
  try {
    const raw = JSON.parse(data) as Record<string, unknown>
    switch (raw.type) {
      case 'user_message':
        return {
          type: 'user_message',
          text: String(raw.text ?? ''),
          images: parseUserImages(raw.images),
        }
      case 'thinking_delta':
      case 'text_delta':
        return { type: raw.type, text: String(raw.text ?? '') }
      case 'tool_start':
        return { type: 'tool_start', name: String(raw.name ?? '') }
      case 'tool_result':
        return {
          type: 'tool_result',
          name: String(raw.name ?? ''),
          is_error: Boolean(raw.is_error),
          summary: String(raw.summary ?? ''),
          code: typeof raw.code === 'number' ? raw.code : null,
          command: typeof raw.command === 'string' ? raw.command : null,
        }
      case 'consent_request': {
        // A consent of an unknown kind is an unknown shape: dropped,
        // not guessed.
        if (raw.kind !== 'secret' && raw.kind !== 'phone') return null
        return {
          type: 'consent_request',
          id: String(raw.id ?? ''),
          kind: raw.kind,
          summary: String(raw.summary ?? ''),
          detail: String(raw.detail ?? ''),
          created_at: typeof raw.created_at === 'number' ? raw.created_at : 0,
        }
      }
      case 'consent_resolved':
        return {
          type: 'consent_resolved',
          id: String(raw.id ?? ''),
          granted: Boolean(raw.granted),
        }
      case 'attachment': {
        // An attachment of an unknown media kind is an unknown shape:
        // dropped, not guessed.
        if (raw.media !== 'image' && raw.media !== 'code' && raw.media !== 'file') return null
        return {
          type: 'attachment',
          id: String(raw.id ?? ''),
          name: String(raw.name ?? ''),
          media: raw.media,
          size: typeof raw.size === 'number' ? raw.size : 0,
          caption: typeof raw.caption === 'string' ? raw.caption : null,
        }
      }
      case 'turn_done':
        return {
          type: 'turn_done',
          reply: typeof raw.reply === 'string' ? raw.reply : null,
          outcome: String(raw.outcome ?? ''),
        }
      default:
        return null
    }
  } catch {
    return null
  }
}
