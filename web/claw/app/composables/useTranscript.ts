// The client-side mirror of a claw session's turns. The daemon owns the
// conversation — this composable only accumulates what the event stream
// delivers, and the stream now opens with the session's recorded turns
// replayed before the live feed. So a reload (and every reconnect)
// rebuilds the transcript from the replay alone: the consumer resets the
// mirror when the stream reopens and folds what arrives, never the other
// way around.
import { ref } from 'vue'
import type { AttachmentMedia, ConsentKind, TurnEvent } from './useClaw'

/** One tool call inside a turn: started, then completed by its result. */
export interface ToolRow {
  name: string
  /** null until the result lands. */
  isError: boolean | null
  summary: string
  /** The process exit code a command-like result carries. */
  code: number | null
  /** The command line a bash result carries. */
  command: string | null
}

/** One stream-ordered thing inside a turn: a tool call, a piece of the
 * thinking trace (adjacent deltas merged into one block), the agent's
 * ask (a consent card), or a handover (an attachment). The turn renders
 * its entries in stream order, so a card sits where the event that
 * raised it happened — an ask appears at its tool call, not at the
 * turn's top, and thinking sits between the calls it precedes. */
export type TurnEntry =
  | { kind: 'tool'; row: ToolRow }
  | { kind: 'thinking'; text: string }
  | { kind: 'consent'; consent: ConsentCard }
  | { kind: 'attachment'; attachment: AttachmentCard }

/** One turn as the UI renders it: the stream-ordered entries and the
 * streamed reply. */
export interface TurnBlock {
  reply: string
  entries: TurnEntry[]
  /** Set by `turn_done`: `handled`, `failed`, or whatever the claw reports. */
  outcome: string | null
}

/** One thing the agent asked its human, as the card renders it. */
export interface ConsentCard {
  id: string
  kind: ConsentKind
  summary: string
  detail: string
  /** null while the human has not answered. */
  granted: boolean | null
}

/** One file the agent handed over, as the card renders it. The bytes
 * arrive as an object URL on request — the route is bearer-guarded. */
export interface AttachmentCard {
  id: string
  name: string
  media: AttachmentMedia
  size: number
  caption: string | null
  /** The object URL once fetched; null until then. */
  url: string | null
}

/** A transcript line: a message the local user sent, or one streamed turn. */
export type TranscriptItem =
  | { kind: 'sent'; id: number; text: string }
  | { kind: 'turn'; id: number; turn: TurnBlock }
  | { kind: 'consent'; id: number; consent: ConsentCard }
  | { kind: 'attachment'; id: number; attachment: AttachmentCard }

// --- embedded pastes --------------------------------------------------
// The wire form a long paste travels in: MARK, payload, MARK — one
// logical unit inside otherwise plain text (rhema-core's paste module
// is the definition; these are its client-side shape).

/** The object-replacement character: opens and closes one embedded
 * paste. Two marks always bracket a payload; a lone mark is literal. */
export const PASTE_MARK = '￼'

/** A paste at or under this many lines joins the input inline; longer
 * ones become chips (the TUI's PASTE_INLINE_LINES). */
export const PASTE_INLINE_LINES = 3

/** Text that travels beside or inside marks: carriage returns to
 * newlines, stray marks dropped — the scan must stay unambiguous
 * (paste::normalize). */
export function normalizePaste(text: string): string {
  return text.replaceAll('\r\n', '\n').replaceAll('\r', '\n').replaceAll(PASTE_MARK, '')
}

/** Rust `str::lines`: \n-split, and a trailing newline ends the last
 * line instead of opening an empty one. */
export function lineCount(text: string): number {
  if (text === '') return 0
  const lines = text.split('\n')
  if (lines[lines.length - 1] === '') lines.pop()
  return lines.length
}

/** A chip's label: `▣1 5L` — the anchor's one-based id and the
 * payload's line count (the TUI's chip_label). */
export function chipLabel(paste: string, index: number): string {
  return `▣${index + 1} ${lineCount(paste)}L`
}

/** Appends each paste as one MARK-payload-MARK embed, in order — the
 * session's record of the prompt (paste::embed; a web draft holds no
 * anchors, so every payload lands at the end). */
export function embedPastes(draft: string, pastes: string[]): string {
  return (
    draft +
    pastes.map((paste) => `${PASTE_MARK}${normalizePaste(paste)}${PASTE_MARK}`).join('')
  )
}

/** One scanned piece of embedded-form text. */
export type PastePart = { kind: 'text'; text: string } | { kind: 'paste'; text: string }

/** Splits embedded-form text into its parts, left to right. A mark
 * with no partner is literal text; malformed input never throws
 * (paste::parts). */
export function pasteParts(text: string): PastePart[] {
  const parts: PastePart[] = []
  let rest = text
  for (;;) {
    const start = rest.indexOf(PASTE_MARK)
    if (start === -1) break
    const after = rest.slice(start + PASTE_MARK.length)
    const end = after.indexOf(PASTE_MARK)
    if (end === -1) {
      parts.push({ kind: 'text', text: rest.slice(0, start + PASTE_MARK.length) })
      rest = after
      continue
    }
    if (start > 0) parts.push({ kind: 'text', text: rest.slice(0, start) })
    parts.push({ kind: 'paste', text: after.slice(0, end) })
    rest = after.slice(end + PASTE_MARK.length)
  }
  if (rest !== '') parts.push({ kind: 'text', text: rest })
  return parts
}

export function useTranscript() {
  const items = ref<TranscriptItem[]>([])
  // A deep ref on purpose: deltas mutate fields in place (reply
  // appends, tool rows completing) and each must re-render.
  const live = ref<TurnBlock | null>(null)
  let nextId = 1

  function ensureLive(): TurnBlock {
    if (!live.value) live.value = { reply: '', entries: [], outcome: null }
    return live.value
  }

  /** A detached copy of one entry: the record of a finished turn must
   * not alias the live slot's objects. */
  function cloneEntry(entry: TurnEntry): TurnEntry {
    if (entry.kind === 'tool') return { kind: 'tool', row: { ...entry.row } }
    if (entry.kind === 'thinking') return { kind: 'thinking', text: entry.text }
    if (entry.kind === 'consent') return { kind: 'consent', consent: { ...entry.consent } }
    return { kind: 'attachment', attachment: { ...entry.attachment } }
  }

  /** Folds one stream event into the mirror. */
  function applyEvent(event: TurnEvent): void {
    switch (event.type) {
      case 'user_message': {
        // The replayed question. A live echo of a message this surface
        // just sent arrives after the optimistic `sent` row: identical
        // text as the last item means the row already stands in for it.
        const last = items.value[items.value.length - 1]
        if (last?.kind === 'sent' && last.text === event.text) return
        items.value.push({ kind: 'sent', id: nextId++, text: event.text })
        break
      }
      case 'thinking_delta': {
        // Thinking joins the entries at its stream position: a delta
        // extends the trailing block, a gap (a tool call, a reply)
        // opens the next one.
        const entries = ensureLive().entries
        const last = entries[entries.length - 1]
        if (last?.kind === 'thinking') last.text += event.text
        else entries.push({ kind: 'thinking', text: event.text })
        break
      }
      case 'text_delta':
        ensureLive().reply += event.text
        break
      case 'tool_start':
        ensureLive().entries.push({
          kind: 'tool',
          row: { name: event.name, isError: null, summary: '', code: null, command: null },
        })
        break
      case 'tool_result': {
        const entries = ensureLive().entries
        // Results pair with the most recent still-running call of the
        // same name; a result with no start still shows, never drops.
        const pending = [...entries]
          .reverse()
          .find((entry) => entry.kind === 'tool' && entry.row.name === event.name && entry.row.isError === null)
        if (pending && pending.kind === 'tool') {
          pending.row.isError = event.is_error
          pending.row.summary = event.summary
          pending.row.code = event.code
          pending.row.command = event.command
        } else {
          entries.push({
            kind: 'tool',
            row: {
              name: event.name,
              isError: event.is_error,
              summary: event.summary,
              code: event.code,
              command: event.command,
            },
          })
        }
        break
      }
      case 'consent_request':
        // One card per consent id, wherever the turn stands: mid-turn
        // the card joins the entries at its tool call; a card with no
        // turn around it (a defensive shape — asks fire mid-turn)
        // falls back to a top-level row.
        if (!findConsent(event.id)) {
          const card: ConsentCard = {
            id: event.id,
            kind: event.kind,
            summary: event.summary,
            detail: event.detail,
            granted: null,
          }
          if (live.value) {
            live.value.entries.push({ kind: 'consent', consent: card })
          } else {
            items.value.push({ kind: 'consent', id: nextId++, consent: card })
          }
        }
        break
      case 'consent_resolved':
        resolveConsent(event.id, event.granted)
        break
      case 'attachment':
        // One card per attachment id; the same dual placement as an
        // ask — mid-turn it rides the entries.
        if (!findAttachment(event.id)) {
          const card: AttachmentCard = {
            id: event.id,
            name: event.name,
            media: event.media,
            size: event.size,
            caption: event.caption,
            url: null,
          }
          if (live.value) {
            live.value.entries.push({ kind: 'attachment', attachment: card })
          } else {
            items.value.push({ kind: 'attachment', id: nextId++, attachment: card })
          }
        }
        break
      case 'turn_done': {
        const turn = ensureLive()
        turn.outcome = event.outcome
        // The clean end marker wins for the reply: deltas may have been
        // dropped (best-effort stream), turn_done carries the final text.
        if (event.reply !== null) turn.reply = event.reply
        // A detached copy: the live slot resets and the record is final.
        items.value.push({
          kind: 'turn',
          id: nextId++,
          turn: { ...turn, entries: turn.entries.map(cloneEntry) },
        })
        live.value = null
        break
      }
    }
  }

  /** A locally sent message, shown immediately; the reply streams after. */
  function addSent(text: string): void {
    items.value.push({ kind: 'sent', id: nextId++, text })
  }

  /** A consent card resolved: flipped in place, or ignored when no
   * card carries the id. The in-flight turn's entries hold the cards a
   * running turn asked for; finished turns keep their own copies. */
  function resolveConsent(id: string, granted: boolean): void {
    if (live.value) {
      for (const entry of live.value.entries) {
        if (entry.kind === 'consent' && entry.consent.id === id) {
          entry.consent.granted = granted
          return
        }
      }
    }
    for (const item of items.value) {
      if (item.kind === 'consent' && item.consent.id === id) {
        item.consent.granted = granted
        return
      }
    }
  }

  /** An attachment's object URL landed; stored on its card. The useClaw
   * cache keeps a reconnect's replay from refetching the bytes. */
  function setAttachmentUrl(id: string, url: string): void {
    if (live.value) {
      for (const entry of live.value.entries) {
        if (entry.kind === 'attachment' && entry.attachment.id === id) {
          entry.attachment.url = url
          return
        }
      }
    }
    for (const item of items.value) {
      if (item.kind === 'attachment' && item.attachment.id === id) {
        item.attachment.url = url
        return
      }
    }
  }

  /** Whether a card for this consent id is already on the mirror — a
   * replayed request never stacks a second card beside the first. */
  function findConsent(id: string): boolean {
    if (live.value?.entries.some((e) => e.kind === 'consent' && e.consent.id === id)) return true
    return items.value.some((item) => item.kind === 'consent' && item.consent.id === id)
  }

  /** Whether a card for this attachment id is already on the mirror. */
  function findAttachment(id: string): boolean {
    if (live.value?.entries.some((e) => e.kind === 'attachment' && e.attachment.id === id)) {
      return true
    }
    return items.value.some((item) => item.kind === 'attachment' && item.attachment.id === id)
  }

  /** Session switched, or the stream reopened: the mirror is per-key,
   * and the replay that follows rebuilds it either way. */
  function reset(): void {
    items.value = []
    live.value = null
  }

  return { items, live, applyEvent, addSent, resolveConsent, setAttachmentUrl, reset }
}
