<script setup lang="ts">
// The transcript: sent lines and streamed turns, the turn in flight at
// the bottom. The claw owns the conversation — this renders what the
// event stream delivered, nothing more.
import type { TranscriptItem, TurnBlock } from '~/composables/useTranscript'

const props = defineProps<{
  items: TranscriptItem[]
  live: TurnBlock | null
  activeKey: string
  /** Only `interactive:*` keys accept messages (the claw 400s the rest). */
  canSend: boolean
  busy: boolean
}>()

const emit = defineEmits<{ send: [text: string] }>()

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
  emit('send', text)
  draft.value = ''
  pinned.value = true
}
</script>

<template>
  <section class="panel chat">
    <header class="panel-head">
      <span class="label">Transcript</span>
      <span class="session" :title="activeKey">{{ activeKey }}</span>
    </header>

    <div ref="scroller" class="stream" @scroll.passive="onScroll">
      <p v-if="items.length === 0 && !live" class="empty">
        nothing streamed yet — messages POST to the claw and replies arrive here live
      </p>

      <template v-for="item in items" :key="item.id">
        <div v-if="item.kind === 'sent'" class="sent">
          <span class="who">you ▸</span>
          <span class="text">{{ item.text }}</span>
        </div>
        <div v-else class="turn">
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
              <span class="glyph" aria-hidden="true">{{
                tool.isError === null ? '…' : tool.isError ? '✗' : '✓'
              }}</span>
              <span class="name">{{ tool.name }}</span>
              <span v-if="tool.summary" class="summary">{{ tool.summary }}</span>
            </li>
          </ul>
          <pre v-if="item.turn.reply" class="reply">{{ item.turn.reply }}</pre>
          <div v-if="item.turn.outcome" class="outcome" :class="{ failed: item.turn.outcome === 'failed' }">
            {{ item.turn.outcome }}
          </div>
        </div>
      </template>

      <div v-if="live" class="turn live">
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
            <span class="glyph" aria-hidden="true">{{
              tool.isError === null ? '…' : tool.isError ? '✗' : '✓'
            }}</span>
            <span class="name">{{ tool.name }}</span>
            <span v-if="tool.summary" class="summary">{{ tool.summary }}</span>
          </li>
        </ul>
        <pre v-if="live.reply" class="reply">{{ live.reply }}<span
          class="cursor"
          aria-hidden="true"
        >▊</span></pre>
      </div>
    </div>

    <footer class="composer">
      <input
        v-model="draft"
        type="text"
        :placeholder="canSend ? `message ${activeKey}…` : 'only interactive:* sessions accept messages'"
        :disabled="!canSend"
        @keydown.enter="submit"
      />
      <button :disabled="!canSend || busy || draft.trim() === ''" @click="submit">send</button>
    </footer>
  </section>
</template>

<style scoped>
.panel {
  display: flex;
  flex-direction: column;
  min-height: 0;
  min-width: 0;
  background: var(--ground);
}

.panel-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  padding: 10px 16px;
  border-bottom: 1px solid var(--border);
}

.session {
  color: var(--text3);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.stream {
  flex: 1;
  overflow-y: auto;
  padding: 16px;
}

.empty {
  color: var(--text3);
}

.sent {
  display: flex;
  gap: 8px;
  margin: 0 0 12px;
  padding: 6px 10px;
  border-left: 2px solid var(--border-strong);
}

.who {
  color: var(--accent-text);
  flex: none;
}

.text {
  color: var(--text2);
  overflow-wrap: anywhere;
  white-space: pre-wrap;
}

.turn {
  margin: 0 0 16px;
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
  border-radius: 3px;
  color: var(--text3);
  font-style: italic;
  white-space: pre-wrap;
  overflow-wrap: anywhere;
}

.tools {
  list-style: none;
  margin: 0 0 8px;
  padding: 0;
}

.tools li {
  display: flex;
  gap: 8px;
  align-items: baseline;
  padding: 3px 10px;
  border-left: 2px solid var(--border-strong);
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

.reply {
  margin: 0;
  font: inherit;
  color: var(--text);
  white-space: pre-wrap;
  overflow-wrap: anywhere;
}

.cursor {
  color: var(--accent-text);
  animation: blink 1.1s steps(1) infinite;
}

@keyframes blink {
  50% {
    opacity: 0;
  }
}

.outcome {
  margin-top: 6px;
  font-size: 11px;
  letter-spacing: 0.1em;
  text-transform: uppercase;
  color: var(--text3);
}

.outcome.failed {
  color: var(--error);
}

.composer {
  display: flex;
  gap: 8px;
  padding: 12px 16px;
  border-top: 1px solid var(--border);
  background: var(--surface);
}

.composer input {
  flex: 1;
  min-width: 0;
}

.composer button {
  background: var(--accent);
  border-color: var(--accent);
  color: var(--on-accent);
}

.composer button:hover:not(:disabled) {
  color: var(--on-accent);
  border-color: var(--accent-text);
  background: var(--accent-text);
}
</style>
