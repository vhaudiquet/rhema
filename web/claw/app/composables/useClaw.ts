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

/** One turn event, exactly `hub.rs`'s `TurnEvent` (serde type tag, snake case). */
export type TurnEvent =
  | { type: 'thinking_delta'; text: string }
  | { type: 'text_delta'; text: string }
  | { type: 'tool_start'; name: string }
  | { type: 'tool_result'; name: string; is_error: boolean; summary: string }
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
}

export interface ClawProvider {
  kind: string
  default_model: string
}

export interface ProvidersState {
  active: { provider: string; model: string }
  providers: ClawProvider[]
}

export interface ClawWatch {
  forge: string
  repo: string
  host: string | null
  token: string | null
}

/** Body of `POST /v1/watches` (http.rs `WatchBody`). */
export interface WatchInput {
  forge: string
  repo: string
  host?: string
  token?: string
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

/** Interactive keys are the only ones message ingest accepts (http.rs `messages`). */
export function isInteractive(key: string): boolean {
  return key.startsWith(INTERACTIVE_PREFIX)
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
  /** One turn event, in stream order. */
  onEvent: (event: TurnEvent) => void
  /** Connection lifecycle; `reconnecting` follows a drop, with backoff. */
  onStatus?: (status: StreamStatus) => void
  /**
   * Best-effort delivery notices from the stream itself — the hub's
   * `lagged` comment (events dropped) and drop/retry chatter. Never
   * turn events, never silent.
   */
  onNotice?: (notice: string) => void
}

export interface ClawApi {
  /** `GET /v1/sessions` — every dispatch key with its status. */
  sessions(): Promise<ClawSession[]>
  /** `POST /v1/sessions/{key}/messages` — fire-and-forget: the claw answers 202 and the reply arrives on the event stream. */
  sendMessage(key: string, text: string): Promise<void>
  /** `GET /v1/providers` — the constructible backends and the active pair. */
  providers(): Promise<ProvidersState>
  /** `PUT /v1/providers/active` — switch the backend, persisted claw-side. */
  setActive(provider: string, model?: string): Promise<ProvidersState>
  /** `GET /v1/providers/{kind}/models` — the live listing; 404 when none. */
  models(kind: string): Promise<ModelCard[]>
  /** `GET /v1/watches`. */
  watches(): Promise<ClawWatch[]>
  /** `POST /v1/watches` — 201 when newly added, 200 when it already existed. */
  addWatch(input: WatchInput): Promise<void>
  /** `DELETE /v1/watches/{forge}/{repo}`. */
  removeWatch(forge: string, repo: string): Promise<void>
  /**
   * Stream one session's turns. Resolves with a handle once the fetch
   * is started; the claw keeps the stream open forever, and a drop is
   * retried with backoff until `close()` is called.
   */
  subscribeEvents(key: string, handlers: StreamHandlers): { close: () => void }
}

/** Build the client bound to one bearer token (null = unguarded claw). */
export function useClaw(token: Ref<string | null>): ClawApi {
  async function sessions(): Promise<ClawSession[]> {
    return (await get('/v1/sessions', token.value)) as ClawSession[]
  }

  async function sendMessage(key: string, text: string): Promise<void> {
    await send(`/v1/sessions/${encodeURIComponent(key)}/messages`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', ...authHeaders(token.value) },
      body: JSON.stringify({ text }),
    })
  }

  async function watches(): Promise<ClawWatch[]> {
    const body = (await get('/v1/watches', token.value)) as { watches: ClawWatch[] }
    return body.watches
  }

  async function addWatch(input: WatchInput): Promise<void> {
    await send('/v1/watches', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', ...authHeaders(token.value) },
      body: JSON.stringify({
        forge: input.forge,
        repo: input.repo.trim(),
        host: input.host?.trim() || undefined,
        token: input.token?.trim() || undefined,
      }),
    })
  }

  async function providers(): Promise<ProvidersState> {
    return (await get('/v1/providers', token.value)) as ProvidersState
  }

  async function setActive(provider: string, model?: string): Promise<ProvidersState> {
    return (await send('/v1/providers/active', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json', ...authHeaders(token.value) },
      body: JSON.stringify({ provider, ...(model ? { model } : {}) }),
    })) as ProvidersState
  }

  async function models(kind: string): Promise<ModelCard[]> {
    const body = (await get(`/v1/providers/${encodeURIComponent(kind)}/models`, token.value)) as {
      models: ModelCard[]
    }
    return body.models
  }

  async function removeWatch(forge: string, repo: string): Promise<void> {
    // The repo is a wildcard tail (`{*repo}`); keep its slash a real
    // path separator — only the segments themselves are escaped.
    const repoPath = repo
      .split('/')
      .map(encodeURIComponent)
      .join('/')
    await send(`/v1/watches/${encodeURIComponent(forge)}/${repoPath}`, {
      method: 'DELETE',
      headers: authHeaders(token.value),
    })
  }

  function subscribeEvents(key: string, handlers: StreamHandlers): { close: () => void } {
    const controller = new AbortController()
    let closed = false
    let attempt = 0

    const status = (next: StreamStatus) => {
      if (!closed) handlers.onStatus?.(next)
    }

    void (async () => {
      while (!closed) {
        status(attempt === 0 ? 'connecting' : 'reconnecting')
        try {
          const response = await fetch(
            `${base()}/v1/sessions/${encodeURIComponent(key)}/events`,
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
            (data) => {
              const event = parseTurnEvent(data)
              if (event) handlers.onEvent(event)
            },
            (comment) => {
              // Keep-alives come through here too; only the hub's
              // dropped-events notice is worth raising.
              if (comment.startsWith('lagged')) {
                handlers.onNotice?.(`stream lagged — ${comment.replace(/^lagged;\s*/, '')}`)
              }
            },
          )
          if (closed) break
          throw new ClawError(0, 'stream closed by the claw')
        } catch (cause) {
          if (closed || controller.signal.aborted) break
          // Backoff caps at 15s: a live stream of a running turn is the
          // UI's whole job, so retries stay eager but not tight-looped.
          const backoff = Math.min(15000, 1000 * 2 ** attempt++)
          handlers.onNotice?.(
            `event stream dropped (${cause instanceof Error ? cause.message : String(cause)}); retrying in ${backoff / 1000}s`,
          )
          status('reconnecting')
          await new Promise((resolve) => setTimeout(resolve, backoff))
        }
      }
    })()

    return {
      close() {
        closed = true
        controller.abort()
      },
    }
  }

  return {
    sessions,
    sendMessage,
    providers,
    setActive,
    models,
    watches,
    addWatch,
    removeWatch,
    subscribeEvents,
  }
}

/** One parsed SSE frame: the joined `data:` payload, comments excluded. */
type FrameHandler = (data: string) => void

/**
 * Reads an SSE body into frames. Frames split on a blank line; `data:`
 * lines carry payload (joined with newlines) and `:`-prefixed lines are
 * comments (the claw's keep-alive and its lag notice). `event:`/`id:`
 * lines do not occur on this surface.
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
    const data = frame
      .split('\n')
      .filter((line) => line.startsWith('data:'))
      .map((line) => line.replace(/^data: ?/, ''))
      .join('\n')
    if (data) {
      onFrame(data)
      return
    }
    const comment = frame
      .split('\n')
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

/** Parses one `data:` payload into a `TurnEvent`; unknown shapes are dropped, not guessed. */
function parseTurnEvent(data: string): TurnEvent | null {
  try {
    const raw = JSON.parse(data) as Record<string, unknown>
    switch (raw.type) {
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
