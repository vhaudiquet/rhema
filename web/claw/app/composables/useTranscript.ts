// The client-side mirror of a claw session's turns. The daemon owns the
// conversation — this composable only accumulates what the event stream
// delivers, and the stream now opens with the session's recorded turns
// replayed before the live feed. So a reload (and every reconnect)
// rebuilds the transcript from the replay alone: the consumer resets the
// mirror when the stream reopens and folds what arrives, never the other
// way around.
import { ref } from 'vue'
import type { TurnEvent } from './useClaw'

/** One tool call inside a turn: started, then completed by its result. */
export interface ToolRow {
  name: string
  /** null until the result lands. */
  isError: boolean | null
  summary: string
}

/** One turn as the UI renders it: thinking trace, tool rows, streamed reply. */
export interface TurnBlock {
  thinking: string
  reply: string
  tools: ToolRow[]
  /** Set by `turn_done`: `handled`, `failed`, or whatever the claw reports. */
  outcome: string | null
}

/** A transcript line: a message the local user sent, or one streamed turn. */
export type TranscriptItem =
  | { kind: 'sent'; id: number; text: string }
  | { kind: 'turn'; id: number; turn: TurnBlock }

export function useTranscript() {
  const items = ref<TranscriptItem[]>([])
  // A deep ref on purpose: deltas mutate fields in place (reply
  // appends, tool rows completing) and each must re-render.
  const live = ref<TurnBlock | null>(null)
  let nextId = 1

  function ensureLive(): TurnBlock {
    if (!live.value) live.value = { thinking: '', reply: '', tools: [], outcome: null }
    return live.value
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
      case 'thinking_delta':
        ensureLive().thinking += event.text
        break
      case 'text_delta':
        ensureLive().reply += event.text
        break
      case 'tool_start':
        ensureLive().tools.push({ name: event.name, isError: null, summary: '' })
        break
      case 'tool_result': {
        const turn = ensureLive()
        // Results pair with the most recent still-running call of the
        // same name; a result with no start still shows, never drops.
        const pending = [...turn.tools].reverse().find(
          (row) => row.name === event.name && row.isError === null,
        )
        if (pending) {
          pending.isError = event.is_error
          pending.summary = event.summary
        } else {
          turn.tools.push({ name: event.name, isError: event.is_error, summary: event.summary })
        }
        break
      }
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
          turn: { ...turn, tools: turn.tools.map((row) => ({ ...row })) },
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

  /** Session switched, or the stream reopened: the mirror is per-key,
   * and the replay that follows rebuilds it either way. */
  function reset(): void {
    items.value = []
    live.value = null
  }

  return { items, live, applyEvent, addSent, reset }
}
