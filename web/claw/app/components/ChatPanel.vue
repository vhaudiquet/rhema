<script setup lang="ts">
// The transcript: sent lines and streamed turns, the turn in flight at
// the bottom. The claw owns the conversation — this renders what the
// event stream delivered, nothing more. The header names the session
// and hides the dispatch internals behind an info modal; the composer
// chips mirror live claw state; the backend chip jumps to settings.
import {
  PASTE_INLINE_LINES,
  chipLabel,
  embedPastes,
  lineCount,
  normalizePaste,
  pasteParts,
} from '~/composables/useTranscript'
import type { ClawSession, StreamStatus } from '~/composables/useClaw'
import type { PastePart, TranscriptItem, TurnBlock } from '~/composables/useTranscript'

const props = defineProps<{
  items: TranscriptItem[]
  live: TurnBlock | null
  activeKey: string
  /** Only `interactive:*` keys accept messages (the claw 400s the rest). */
  canSend: boolean
  busy: boolean
  session: ClawSession | null
  activeBackend: { provider: string; model: string } | null
  streamStatus: StreamStatus
  identity?: { name: string | null; mark: string | null; initials: string | null } | null
  avatarUrl?: string | null
}>()

const agentName = computed(() => props.identity?.name?.trim() || 'rhema')

const emit = defineEmits<{ send: [text: string]; openProviders: [] }>()

const draft = ref('')
const scroller = ref<HTMLElement | null>(null)
/** Pin to the bottom only while the reader is already there. */
const pinned = ref(true)

function onScroll(): void {
  const el = scroller.value
  if (!el) return
  pinned.value = el.scrollHeight - el.scrollTop - el.clientHeight < 48
}

watch(
  () => [props.items.length, props.live?.reply.length, props.live?.thinking.length, props.live?.tools.length],
  async () => {
    if (!pinned.value) return
    await nextTick()
    const el = scroller.value
    if (el) el.scrollTop = el.scrollHeight
  },
)

function submit(): void {
  const text = draft.value.trim()
  if (!text || !props.canSend || props.busy) return
  emit('send', embedPastes(text, pastes.value))
  draft.value = ''
  pastes.value = []
  pinned.value = true
  void nextTick(resizePrompt)
}

// --- the composer -----------------------------------------------------
const prompt = ref<HTMLTextAreaElement | null>(null)

/** The composer grows to this many rows, then scrolls instead. */
const MAX_PROMPT_ROWS = 10

/** Height follows the content; the CSS max-height clamps the growth. */
function resizePrompt(): void {
  const el = prompt.value
  if (!el) return
  el.style.height = 'auto'
  el.style.height = `${el.scrollHeight}px`
}

/** Enter submits, shift+enter is a newline, ctrl/cmd+enter submits. */
function onPromptKeydown(event: KeyboardEvent): void {
  if (event.key === 'Enter' && !event.shiftKey) {
    event.preventDefault()
    submit()
  }
}

// --- pasted attachments ----------------------------------------------
// A paste over PASTE_INLINE_LINES lines rides as a chip: the payload
// never lands in the box, it embeds into the sent text on submit —
// the same record the TUI keeps.

const pastes = ref<string[]>([])

function onPaste(event: ClipboardEvent): void {
  const transfer = event.clipboardData
  if (!transfer) return
  const text = transfer.getData('text/plain')
  if (text !== '') {
    if (absorbPaste(text)) event.preventDefault()
    return
  }
  // A file paste reads as its text when it claims a textual type; the
  // rest the composer ignores.
  const file = transfer.files[0]
  if (!file || !file.type.startsWith('text/')) return
  event.preventDefault()
  void file.text().then((text) => {
    if (!absorbPaste(text)) insertInline(text)
  })
}

/** A long paste becomes one chip; whether it did is the answer. */
function absorbPaste(text: string): boolean {
  const normalized = normalizePaste(text)
  if (lineCount(normalized) <= PASTE_INLINE_LINES) return false
  pastes.value.push(normalized)
  return true
}

/** A short file paste joins the draft where the caret sits. */
function insertInline(text: string): void {
  const el = prompt.value
  if (el && !el.disabled) {
    el.setRangeText(text, el.selectionStart ?? el.value.length, el.selectionEnd ?? el.value.length, 'end')
    draft.value = el.value
  } else {
    draft.value += text
  }
  resizePrompt()
}

/** One render piece of a user message: literal text or a paste chip. */
type MessageSegment = PastePart | { kind: 'chip'; label: string }

/** The message's render pieces: literal text and resolved chips, the
 * chips' one-based ids counted across the message. */
function messageSegments(text: string): MessageSegment[] {
  const segments: MessageSegment[] = []
  let chip = 0
  for (const part of pasteParts(text)) {
    if (part.kind === 'text') {
      if (part.text !== '') segments.push(part)
    } else {
      segments.push({ kind: 'chip', label: chipLabel(part.text, chip++) })
    }
  }
  return segments
}

const displayName = computed(() => props.activeKey.replace(/^interactive:/, ''))

// The dispatch internals live behind the info modal, not in the header.
const infoOpen = ref(false)

const infoRows = computed(() => [
  { label: 'state', value: props.session?.state ?? 'unknown' },
  { label: 'revision', value: props.session ? String(props.session.revision) : '—' },
  { label: 'inbox depth', value: props.session ? String(props.session.inbox_depth) : '—' },
  { label: 'last outcome', value: props.session?.last_outcome ?? '—' },
  { label: 'key', value: props.activeKey },
])

const streamLabel = computed(() =>
  props.streamStatus === 'open' ? 'live' : props.streamStatus,
)

function streamTone(status: StreamStatus): 'ok' | 'warn' | 'error' {
  if (status === 'open') return 'ok'
  if (status === 'reconnecting') return 'error'
  return 'warn'
}

/** Command rows render as shell lines, not glyph rows (the TUI's
 * tool_row); run_command is the bash tool's other name. */
function isShellTool(name: string): boolean {
  return name === 'bash' || name === 'run_command'
}
</script>

<template>
  <section class="chat">
    <header class="head">
      <AgentAvatar :avatar-url="avatarUrl" :mark="identity?.mark" :initials="identity?.initials ?? identity?.name?.slice(0, 1)" :size="34" />
      <div class="text">
        <h2>{{ displayName }}</h2>
      </div>
      <span class="stream-status" :title="`event stream: ${streamStatus}`">
        <StatusDot :tone="streamTone(streamStatus)" />
        {{ streamLabel }}
      </span>
      <button class="ghost info" title="session info" @click="infoOpen = true">ⓘ</button>
    </header>

    <AppModal :open="infoOpen" title="Session info" @close="infoOpen = false">
      <dl class="info-rows">
        <div v-for="row in infoRows" :key="row.label" class="info-row">
          <dt class="label">{{ row.label }}</dt>
          <dd class="mono">{{ row.value }}</dd>
        </div>
      </dl>
    </AppModal>

    <div ref="scroller" class="stream-area" @scroll.passive="onScroll">
      <p v-if="items.length === 0 && !live" class="empty">
        nothing streamed yet; messages post to the claw and replies arrive here live
      </p>

      <template v-for="item in items" :key="item.id">
        <div v-if="item.kind === 'sent'" class="msg">
          <div class="avatar user" aria-hidden="true">·</div>
          <div class="msg-body">
            <div class="who">you</div>
            <div class="reply plain">
              <template v-for="(segment, index) in messageSegments(item.text)" :key="index">
                <span v-if="segment.kind === 'chip'" class="badge paste-chip">{{ segment.label }}</span>
                <template v-else>{{ segment.text }}</template>
              </template>
            </div>
          </div>
        </div>
        <div v-else class="msg">
          <AgentAvatar :avatar-url="avatarUrl" :mark="identity?.mark" :initials="identity?.initials ?? identity?.name?.slice(0, 1)" :size="32" />
          <div class="msg-body">
            <div class="who">{{ agentName }}</div>
            <details v-if="item.turn.thinking" class="thinking">
              <summary>thinking</summary>
              <pre>{{ item.turn.thinking }}</pre>
            </details>
            <ul v-if="item.turn.tools.length > 0" class="tools">
              <li
                v-for="(tool, index) in item.turn.tools"
                :key="index"
                :class="{ error: tool.isError === true }"
              >
                <template v-if="isShellTool(tool.name)">
                  <span class="shell-mark" aria-hidden="true">$ </span>
                  <span class="shell-command mono">{{ tool.command ?? '…' }}</span>
                  <span
                    v-if="typeof tool.code === 'number'"
                    class="shell-code mono"
                    :class="tool.code === 0 ? 'ok' : 'failed'"
                  >[{{ tool.code }}]</span>
                  <span v-else-if="tool.isError === true" class="shell-code failed">✗</span>
                </template>
                <template v-else>
                  <span class="glyph" aria-hidden="true">{{
                    tool.isError === null ? '…' : tool.isError ? '✗' : '✓'
                  }}</span>
                  <span class="name mono">{{ tool.name }}</span>
                  <span v-if="tool.summary" class="summary mono">{{ tool.summary }}</span>
                </template>
              </li>
            </ul>
            <AppMarkdown v-if="item.turn.reply" :text="item.turn.reply" class="reply" />
            <div
              v-if="item.turn.outcome"
              class="outcome mono"
              :class="{ failed: item.turn.outcome === 'failed' }"
            >
              {{ item.turn.outcome }}
            </div>
          </div>
        </div>
      </template>

      <div v-if="live" class="msg live">
        <AgentAvatar :avatar-url="avatarUrl" :mark="identity?.mark" :initials="identity?.initials ?? identity?.name?.slice(0, 1)" :size="32" />
        <div class="msg-body">
          <div class="who">{{ agentName }}</div>
          <details v-if="live.thinking" class="thinking" open>
            <summary>thinking</summary>
            <pre>{{ live.thinking }}</pre>
          </details>
          <ul v-if="live.tools.length > 0" class="tools">
            <li
              v-for="(tool, index) in live.tools"
              :key="index"
              :class="{ error: tool.isError === true }"
            >
              <template v-if="isShellTool(tool.name)">
                <span class="shell-mark" aria-hidden="true">$ </span>
                <span class="shell-command mono">{{ tool.command ?? '…' }}</span>
                <span
                  v-if="typeof tool.code === 'number'"
                  class="shell-code mono"
                  :class="tool.code === 0 ? 'ok' : 'failed'"
                >[{{ tool.code }}]</span>
                <span v-else-if="tool.isError === true" class="shell-code failed">✗</span>
              </template>
              <template v-else>
                <span class="glyph" aria-hidden="true">{{
                  tool.isError === null ? '…' : tool.isError ? '✗' : '✓'
                }}</span>
                <span class="name mono">{{ tool.name }}</span>
                <span v-if="tool.summary" class="summary mono">{{ tool.summary }}</span>
              </template>
            </li>
          </ul>
          <AppMarkdown v-if="live.reply" :text="live.reply" streaming class="reply" />
        </div>
      </div>
    </div>

    <footer class="composer">
      <div class="chips">
        <span class="badge">{{ activeKey }}</span>
        <button
          class="badge chip-btn"
          title="switch the model backend in settings"
          :disabled="!activeBackend"
          @click="emit('openProviders')"
        >
          {{ activeBackend ? `${activeBackend.provider} · ${activeBackend.model}` : 'no backend' }}
        </button>
      </div>
      <div v-if="pastes.length > 0" class="paste-chips">
        <span v-for="(paste, index) in pastes" :key="index" class="badge paste-chip">
          {{ chipLabel(paste, index) }}
          <button class="chip-x" :title="`drop paste ${index + 1}`" @click="pastes.splice(index, 1)">
            ×
          </button>
        </span>
      </div>
      <div class="input-row">
        <textarea
          ref="prompt"
          v-model="draft"
          rows="1"
          :placeholder="canSend ? `Message ${agentName}…` : 'only interactive:* sessions accept messages'"
          :disabled="!canSend"
          @keydown="onPromptKeydown"
          @paste="onPaste"
          @input="resizePrompt"
        />
        <button class="primary send" :disabled="!canSend || busy || draft.trim() === ''" title="send" @click="submit">
          ↑
        </button>
      </div>
    </footer>
  </section>
</template>

<style scoped>
.chat {
  display: flex;
  flex-direction: column;
  min-height: 0;
  min-width: 0;
  flex: 1;
  background: var(--ground);
}

.head {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 12px 20px;
  border-bottom: 1px solid var(--border);
  background: var(--surface);
}

.avatar {
  display: grid;
  place-items: center;
  width: 34px;
  height: 34px;
  flex: none;
  border-radius: 10px;
  font-weight: 700;
}

.avatar.user {
  background: var(--elevated);
  border: 1px solid var(--border-strong);
  color: var(--text2);
}

.text {
  flex: 1;
  min-width: 0;
}

.text h2 {
  margin: 0;
  font-size: 15px;
  font-weight: 700;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.head .info {
  flex: none;
  padding: 0 6px;
  font-size: 14px;
}

.info-rows {
  margin: 0;
  display: flex;
  flex-direction: column;
}

.info-row {
  display: flex;
  align-items: baseline;
  justify-content: space-between;
  gap: 16px;
  padding: 7px 0;
}

.info-row + .info-row {
  border-top: 1px solid var(--border);
}

.info-row dd {
  margin: 0;
  min-width: 0;
  overflow-wrap: anywhere;
  text-align: right;
}

.stream-status {
  display: flex;
  align-items: center;
  gap: 6px;
  color: var(--text3);
  font-size: 12px;
  flex: none;
}

.stream-area {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  padding: 20px 24px;
}

.empty {
  color: var(--text3);
  padding: 24px;
}

.msg {
  display: flex;
  gap: 12px;
  margin-bottom: 20px;
}

.msg-body {
  flex: 1;
  min-width: 0;
}

.who {
  font-weight: 700;
  font-size: 13px;
  margin-bottom: 2px;
}

.live .who {
  color: var(--accent-text);
}

.thinking {
  margin-bottom: 8px;
  color: var(--text3);
}

.thinking summary {
  cursor: pointer;
  font-size: 11px;
  letter-spacing: 0.1em;
  text-transform: uppercase;
}

.thinking pre {
  margin: 6px 0 0;
  padding: 8px 10px;
  background: var(--surface);
  border: 1px solid var(--border);
  border-radius: 6px;
  color: var(--text3);
  font-style: italic;
  white-space: pre-wrap;
  overflow-wrap: anywhere;
}

.tools {
  list-style: none;
  margin: 0 0 10px;
  padding: 0;
  display: flex;
  flex-direction: column;
  gap: 4px;
}

.tools li {
  display: flex;
  gap: 8px;
  align-items: baseline;
  padding: 5px 10px;
  background: var(--surface);
  border: 1px solid var(--border);
  border-left: 3px solid var(--ok);
  border-radius: 6px;
}

.tools li.error {
  border-left-color: var(--error);
}

.glyph {
  flex: none;
  color: var(--ok);
}

li.error .glyph {
  color: var(--error);
}

.name {
  color: var(--text);
}

.summary {
  color: var(--text3);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

/* A bash row reads as the shell line it ran: dim $ prefix, the
   command, and the exit code pushed to the row's right end. */
.shell-mark {
  flex: none;
  color: var(--text3);
  white-space: pre;
}

.shell-command {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  color: var(--text);
}

.shell-code {
  flex: none;
  font-size: 11px;
}

.shell-code.ok {
  color: var(--ok);
}

.shell-code.failed {
  color: var(--error);
}

.reply {
  color: var(--text);
  overflow-wrap: anywhere;
}

/* User lines are verbatim input; agent replies render markdown (see
   main.css .md) and wrap by its rules instead. */
.plain {
  white-space: pre-wrap;
}

.outcome {
  margin-top: 6px;
  font-size: 11px;
  letter-spacing: 0.08em;
  text-transform: uppercase;
  color: var(--text3);
}

.outcome.failed {
  color: var(--error);
}

.composer {
  border-top: 1px solid var(--border);
  background: var(--surface);
  padding: 10px 16px 14px;
}

.chips {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-bottom: 8px;
  min-width: 0;
  overflow: hidden;
}

.chip-btn {
  cursor: pointer;
  overflow: hidden;
  text-overflow: ellipsis;
}

.chip-btn:hover:not(:disabled) {
  border-color: var(--accent);
  color: var(--accent-text);
}

.paste-chips {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
  margin-bottom: 8px;
}

/* One embedded paste, standing in for its payload — composer staging
   and transcript rendering share the shape. */
.paste-chip {
  gap: 2px;
  padding-right: 4px;
  background: var(--elevated);
}

.chip-x {
  border: none;
  background: none;
  padding: 0 2px;
  font-size: 12px;
  line-height: 1;
  color: var(--text3);
}

.chip-x:hover:not(:disabled) {
  color: var(--error);
}

.input-row {
  display: flex;
  gap: 8px;
}

.input-row textarea {
  flex: 1;
  min-width: 0;
  padding: 9px 12px;
  resize: none;
  overflow-y: auto;
  /* MAX_PROMPT_ROWS rows, then the box scrolls instead of growing. */
  max-height: calc(10 * 1.55em + 18px);
}

.send {
  width: 38px;
  padding: 0;
  font-size: 16px;
  border-radius: 9px;
  flex: none;
}
</style>
