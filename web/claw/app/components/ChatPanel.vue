<script setup lang="ts">
// The transcript: sent lines and streamed turns, the turn in flight at
// the bottom. The claw owns the conversation — this renders what the
// event stream delivered, nothing more. The header names the session
// and hides the dispatch internals behind an info modal; the composer
// chips mirror live claw state; the backend chip jumps to settings.
import type { ClawSession, StreamStatus } from '~/composables/useClaw'
import type { TranscriptItem, TurnBlock } from '~/composables/useTranscript'

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
  emit('send', text)
  draft.value = ''
  pinned.value = true
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
            <div class="reply plain">{{ item.text }}</div>
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
                <span class="glyph" aria-hidden="true">{{
                  tool.isError === null ? '…' : tool.isError ? '✗' : '✓'
                }}</span>
                <span class="name mono">{{ tool.name }}</span>
                <span v-if="tool.summary" class="summary mono">{{ tool.summary }}</span>
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
              <span class="glyph" aria-hidden="true">{{
                tool.isError === null ? '…' : tool.isError ? '✗' : '✓'
              }}</span>
              <span class="name mono">{{ tool.name }}</span>
              <span v-if="tool.summary" class="summary mono">{{ tool.summary }}</span>
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
      <div class="input-row">
        <input
          v-model="draft"
          type="text"
          :placeholder="canSend ? `Message ${agentName}…` : 'only interactive:* sessions accept messages'"
          :disabled="!canSend"
          @keydown.enter="submit"
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

.input-row {
  display: flex;
  gap: 8px;
}

.input-row input {
  flex: 1;
  min-width: 0;
  padding: 9px 12px;
}

.send {
  width: 38px;
  padding: 0;
  font-size: 16px;
  border-radius: 9px;
  flex: none;
}
</style>
