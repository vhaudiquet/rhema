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
import type { ClawSession, ConsentAnswer, StreamStatus } from '~/composables/useClaw'
import type { PastePart, SentImage, StagedImage, TranscriptItem, TurnBlock } from '~/composables/useTranscript'

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

/** One send as the panel hands it up: the text (pastes embedded) and
 * the staged images, previews included for the optimistic row. */
interface SendPayload {
  text: string
  images: StagedImage[]
}

const emit = defineEmits<{
  send: [message: SendPayload]
  openProviders: []
  consentAnswer: [id: string, answer: ConsentAnswer]
  consentDeny: [id: string]
  downloadAttachment: [id: string, name: string]
}>()

const draft = ref('')
const scroller = ref<HTMLElement | null>(null)
/** Pin to the bottom only while the reader is already there. */
const pinned = ref(true)

function onScroll(): void {
  const el = scroller.value
  if (!el) return
  pinned.value = el.scrollHeight - el.scrollTop - el.clientHeight < 48
}

// Thinking grows inside the trailing entry without changing the
// entries length, so its total length joins the watch to keep the
// stream pinned at the bottom.
function liveThinkingLength(): number {
  return (
    props.live?.entries.reduce(
      (count, entry) => (entry.kind === 'thinking' ? count + entry.text.length : count),
      0,
    ) ?? 0
  )
}

watch(
  () => [props.items.length, props.live?.reply.length, liveThinkingLength(), props.live?.entries.length],
  async () => {
    if (!pinned.value) return
    await nextTick()
    const el = scroller.value
    if (el) el.scrollTop = el.scrollHeight
  },
)

function submit(): void {
  const text = draft.value.trim()
  // The draft stays the gate: the claw 422s an empty text even with
  // images aboard.
  if (!text || !props.canSend || props.busy) return
  emit('send', { text: embedPastes(text, pastes.value), images: stagedImages.value })
  stageGeneration++
  draft.value = ''
  pastes.value = []
  stagedImages.value = []
  imageError.value = null
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
  // A file paste: text files read inline or chip, images stage for the
  // next send — the same door the (+) button opens; anything else the
  // composer still ignores.
  let handled = false
  for (const file of Array.from(transfer.files)) {
    if (file.type.startsWith('text/')) {
      handled = true
      void file.text().then((pasted) => {
        if (!absorbPaste(pasted)) insertInline(pasted)
      })
    } else if (mediaTypeOf(file) !== null) {
      stageImage(file)
      handled = true
    }
  }
  if (handled) event.preventDefault()
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

// --- staged images ----------------------------------------------------
// The (+) button and the clipboard both land here: files held for the
// next send, previewed from object URLs, read to base64 at stage time
// so a bad file surfaces before submit, not inside it. The budget is
// the claw's own (http.rs): eight images, 5 MiB decoded each.

const MAX_IMAGES = 8
const MAX_IMAGE_BYTES = 5 * 1024 * 1024
const IMAGE_TYPES: readonly string[] = ['image/png', 'image/jpeg', 'image/gif', 'image/webp']
const IMAGE_ACCEPT = IMAGE_TYPES.join(',')

const stagedImages = ref<StagedImage[]>([])
const imageError = ref<string | null>(null)
const fileInput = ref<HTMLInputElement | null>(null)

/** Bumped on submit: a read still in flight when the draft cleared
 * must not stage into a message that already left. */
let stageGeneration = 0

/** The wire media type a file stages as, or null when it is not one
 * the claw accepts. The type usually rides the file; browsers that
 * cannot name one leave it empty and the extension decides. */
function mediaTypeOf(file: File): string | null {
  if (IMAGE_TYPES.includes(file.type)) return file.type
  const byExt: Record<string, string> = {
    png: 'image/png',
    jpg: 'image/jpeg',
    jpeg: 'image/jpeg',
    gif: 'image/gif',
    webp: 'image/webp',
  }
  return byExt[file.name.split('.').pop()?.toLowerCase() ?? ''] ?? null
}

/** B / KB / MB, one decimal past 1 KB — a chip label, not precision. */
function formatBytes(size: number): string {
  if (size < 1024) return `${size} B`
  if (size < 1024 * 1024) return `${(size / 1024).toFixed(1)} KB`
  return `${(size / (1024 * 1024)).toFixed(1)} MB`
}

function pickImages(): void {
  fileInput.value?.click()
}

function onPick(event: Event): void {
  const input = event.target as HTMLInputElement
  for (const file of Array.from(input.files ?? [])) stageImage(file)
  // Cleared so picking the same file again still fires `change`.
  input.value = ''
}

/** Stages one file, or surfaces why not: never a silent drop. */
function stageImage(file: File): void {
  const label = file.name || 'clipboard image'
  imageError.value = null
  if (stagedImages.value.length >= MAX_IMAGES) {
    imageError.value = `${label}: at most ${MAX_IMAGES} images a message`
    return
  }
  const media_type = mediaTypeOf(file)
  if (!media_type) {
    imageError.value = `${label}: not a png, jpeg, gif, or webp image`
    return
  }
  const generation = stageGeneration
  const reader = new FileReader()
  reader.onload = () => {
    if (generation !== stageGeneration) return
    // The data URL's head carries the mime back; the payload after the
    // first comma is the bare base64 the body wants.
    const dataUrl = typeof reader.result === 'string' ? reader.result : ''
    const comma = dataUrl.indexOf(',')
    const data = comma === -1 ? '' : dataUrl.slice(comma + 1)
    if (data === '') {
      imageError.value = `${label}: could not be read`
      return
    }
    // Four base64 characters decode to three bytes — the same budget
    // check the claw runs, no decode needed.
    if (data.length * 3 / 4 > MAX_IMAGE_BYTES) {
      imageError.value = `${label}: over the 5 MiB decoded budget`
      return
    }
    stagedImages.value.push({ media_type, data, url: URL.createObjectURL(file), name: label })
  }
  reader.onerror = () => {
    if (generation === stageGeneration) imageError.value = `${label}: could not be read`
  }
  reader.readAsDataURL(file)
}

function removeImage(index: number): void {
  const [gone] = stagedImages.value.splice(index, 1)
  if (gone) URL.revokeObjectURL(gone.url)
  imageError.value = null
}

/** A stored image opens the same download path an attachment card
 * uses; a local preview has nothing behind it to download. */
function downloadImage(image: SentImage): void {
  if (image.id === null) return
  emit('downloadAttachment', image.id, storedImageName(image))
}

/** A stored image's download name: the staged file's when the row is
 * local, derived from the id when only the claw knows the bytes. */
function storedImageName(image: SentImage): string {
  if (image.name) return image.name
  const ext = image.media_type.replace('image/', '')
  return `image-${image.id}.${ext === 'jpeg' ? 'jpg' : ext}`
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
            <div v-if="item.images.length > 0" class="sent-images">
              <button
                v-for="(image, index) in item.images"
                :key="index"
                class="sent-image"
                :disabled="image.url === null || image.id === null"
                :title="image.url !== null && image.id !== null ? `download ${storedImageName(image)}` : (image.name ?? 'attached image')"
                @click="downloadImage(image)"
              >
                <img v-if="image.url" :src="image.url" :alt="image.name ?? 'attached image'" />
              </button>
            </div>
            <div class="reply plain">
              <template v-for="(segment, index) in messageSegments(item.text)" :key="index">
                <span v-if="segment.kind === 'chip'" class="badge paste-chip">{{ segment.label }}</span>
                <template v-else>{{ segment.text }}</template>
              </template>
            </div>
          </div>
        </div>
        <div v-else-if="item.kind === 'turn'" class="msg">
          <AgentAvatar :avatar-url="avatarUrl" :mark="identity?.mark" :initials="identity?.initials ?? identity?.name?.slice(0, 1)" :size="32" />
          <div class="msg-body">
            <div class="who">{{ agentName }}</div>
            <TurnEntries
              :entries="item.turn.entries"
              @consent-answer="(id, answer) => emit('consentAnswer', id, answer)"
              @consent-deny="(id) => emit('consentDeny', id)"
              @download-attachment="(id, name) => emit('downloadAttachment', id, name)"
            />
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
        <div v-else class="msg" :class="item.kind">
          <div class="msg-body">
            <TurnEntries
              :entries="[item.kind === 'consent' ? { kind: 'consent', consent: item.consent } : { kind: 'attachment', attachment: item.attachment }]"
              @consent-answer="(id, answer) => emit('consentAnswer', id, answer)"
              @consent-deny="(id) => emit('consentDeny', id)"
              @download-attachment="(id, name) => emit('downloadAttachment', id, name)"
            />
          </div>
        </div>
      </template>

      <div v-if="live" class="msg live">
        <AgentAvatar :avatar-url="avatarUrl" :mark="identity?.mark" :initials="identity?.initials ?? identity?.name?.slice(0, 1)" :size="32" />
        <div class="msg-body">
          <div class="who">{{ agentName }}</div>
          <TurnEntries
            :entries="live.entries"
            :open-thinking="true"
            @consent-answer="(id, answer) => emit('consentAnswer', id, answer)"
            @consent-deny="(id) => emit('consentDeny', id)"
            @download-attachment="(id, name) => emit('downloadAttachment', id, name)"
          />
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
      <div v-if="stagedImages.length > 0" class="paste-chips">
        <span v-for="(image, index) in stagedImages" :key="image.url" class="badge paste-chip">
          ▣{{ index + 1 }} {{ image.name }} {{ formatBytes(Math.round((image.data.length * 3) / 4)) }}
          <button class="chip-x" :title="`drop ${image.name}`" @click="removeImage(index)">
            ×
          </button>
        </span>
      </div>
      <p v-if="imageError" class="image-error" role="alert">
        {{ imageError }}
        <button class="chip-x" title="dismiss" @click="imageError = null">×</button>
      </p>
      <div class="input-row">
        <button class="ghost attach" title="attach images" :disabled="!canSend" @click="pickImages">
          +
        </button>
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
        <input
          ref="fileInput"
          class="file-input"
          type="file"
          :accept="IMAGE_ACCEPT"
          multiple
          @change="onPick"
        />
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

/* A consent card: the agent asking its human something — distinct
   from a tool row, toned by its resolution. */
.consent-card {
  display: flex;
  flex-direction: column;
  gap: 8px;
  padding: 10px 12px;
  background: var(--surface);
  border: 1px solid var(--border);
  border-left: 3px solid var(--warn);
  border-radius: 6px;
}

.consent-card.ok {
  border-left-color: var(--ok);
}

.consent-card.denied {
  border-left-color: var(--error);
}

.consent-head {
  display: flex;
  align-items: center;
  gap: 8px;
}

.consent-summary {
  flex: 1;
  min-width: 0;
  font-size: 13px;
}

.denied .consent-summary {
  color: var(--text3);
}

.consent-state {
  flex: none;
  font-size: 11px;
  letter-spacing: 0.08em;
  text-transform: uppercase;
  color: var(--ok);
}

.denied .consent-state {
  color: var(--error);
}

.consent-detail {
  margin: 0;
  color: var(--text2);
  font-size: 12.5px;
  overflow-wrap: anywhere;
}

.consent-actions {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 8px;
}

.consent-actions input {
  flex: 1;
  min-width: 160px;
}

/* An attachment card: the agent handing a file over — head row names
   it, the image (when there is one) rides under it, capped so a
   screenshot cannot take the stream. */
.attachment-card {
  display: flex;
  flex-direction: column;
  gap: 8px;
  max-width: 480px;
  padding: 10px 12px;
  background: var(--surface);
  border: 1px solid var(--border);
  border-radius: 6px;
}

.attachment-head {
  display: flex;
  align-items: center;
  gap: 8px;
}

.attachment-glyph {
  flex: none;
  color: var(--text3);
}

.attachment-name {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  color: var(--text);
}

.attachment-size {
  flex: none;
  color: var(--text3);
  font-size: 11px;
}

.attachment-image {
  max-width: 100%;
  max-height: 240px;
  align-self: flex-start;
  border: 1px solid var(--border);
  border-radius: 8px;
}

.attachment-caption {
  margin: 0;
  color: var(--text3);
  font-size: 12.5px;
  overflow-wrap: anywhere;
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

.attach {
  width: 38px;
  padding: 0;
  font-size: 16px;
  border-radius: 9px;
  flex: none;
}

.file-input {
  display: none;
}

.image-error {
  display: flex;
  align-items: center;
  gap: 6px;
  margin: 0 0 8px;
  color: var(--error);
  font-size: 12.5px;
}

/* Images a user message carried: the attachment-card visual, sized to
   the sent row. A stored image downloads on click; a local preview has
   nothing behind it. */
.sent-images {
  display: flex;
  flex-wrap: wrap;
  gap: 8px;
  margin-bottom: 6px;
}

.sent-image {
  display: block;
  padding: 0;
  border: none;
  background: none;
  cursor: pointer;
}

.sent-image:disabled {
  cursor: default;
}

.sent-image img {
  display: block;
  max-width: 100%;
  max-height: 240px;
  border: 1px solid var(--border);
  border-radius: 8px;
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
