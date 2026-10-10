<script setup lang="ts">
// The one page: chat over the active session, the session inventory,
// and the watch table — panels over one claw. Everything is
// in-memory; only the token and the active session persist. The claw
// owns the conversation, so this page is a mirror, not a store.
import { ClawError, clawHealth, isInteractive, useClaw } from '~/composables/useClaw'
import type { ClawSession, ModelCard, ProvidersState, StreamStatus } from '~/composables/useClaw'
import { useTranscript } from '~/composables/useTranscript'

const TOKEN_STORE_KEY = 'claw.token'
const ACTIVE_STORE_KEY = 'claw.activeKey'
const DEFAULT_KEY = 'interactive:web'

function stored(key: string): string | null {
  if (!import.meta.client) return null
  try {
    return localStorage.getItem(key)
  } catch {
    return null
  }
}

function persist(key: string, value: string | null): void {
  if (!import.meta.client) return
  try {
    if (value === null) localStorage.removeItem(key)
    else localStorage.setItem(key, value)
  } catch {
    // Storage unavailable (private mode): the session still works, it
    // just forgets.
  }
}

const config = useRuntimeConfig()
const token = ref<string | null>(
  stored(TOKEN_STORE_KEY) ?? (config.public.clawToken || null),
)
const claw = useClaw(token)

const activeKey = ref(stored(ACTIVE_STORE_KEY) ?? DEFAULT_KEY)
watch(activeKey, (key) => persist(ACTIVE_STORE_KEY, key))

const transcript = useTranscript()
const { items: transcriptItems, live: liveTurn } = transcript

// --- surface state -------------------------------------------------
const clawUp = ref<boolean | null>(null)
const sessions = ref<ClawSession[]>([])
const sessionsLoading = ref(false)
const providersState = ref<ProvidersState | null>(null)
const providerModels = ref<ModelCard[]>([])
const providerModelsLoading = ref(false)
const providerModelsError = ref<string | null>(null)
const selectedProvider = ref<string | null>(null)
const streamStatus = ref<StreamStatus>('connecting')
const busy = ref(false)
const authRequired = ref(false)
const tokenDraft = ref('')
const apiError = ref<string | null>(null)
const notices = ref<{ id: number; text: string }[]>([])
let noticeId = 1

function noteApiError(err: unknown): void {
  if (err instanceof ClawError && err.status === 401) {
    authRequired.value = true
    return
  }
  apiError.value = err instanceof Error ? err.message : String(err)
}

function pushNotice(text: string): void {
  notices.value.push({ id: noticeId++, text })
  if (notices.value.length > 4) notices.value.shift()
}

// --- claw calls ----------------------------------------------------
async function checkHealth(): Promise<void> {
  clawUp.value = await clawHealth()
}

async function refreshSessions(): Promise<void> {
  sessionsLoading.value = true
  try {
    sessions.value = await claw.sessions()
    apiError.value = null
  } catch (err) {
    noteApiError(err)
  } finally {
    sessionsLoading.value = false
  }
}

async function refreshProviders(): Promise<void> {
  try {
    providersState.value = await claw.providers()
    if (providersState.value && !selectedProvider.value) {
      selectedProvider.value = providersState.value.active.provider
      await loadProviderModels(selectedProvider.value)
    }
    apiError.value = null
  } catch (err) {
    noteApiError(err)
  }
}

async function loadProviderModels(kind: string): Promise<void> {
  selectedProvider.value = kind
  providerModelsLoading.value = true
  providerModelsError.value = null
  try {
    providerModels.value = await claw.models(kind)
  } catch (err) {
    // A backend without a live listing is normal (404): the operator
    // still sees the switch go through.
    providerModels.value = []
    providerModelsError.value = err instanceof Error ? err.message : String(err)
  } finally {
    providerModelsLoading.value = false
  }
}

async function switchProvider(provider: string, model?: string): Promise<void> {
  try {
    providersState.value = await claw.setActive(provider, model)
    selectedProvider.value = provider
    if (model) {
      providerModelsError.value = null
    }
    pushNotice(`active backend: ${provider} / ${providersState.value.active.model}`)
    apiError.value = null
  } catch (err) {
    noteApiError(err)
  }
}

async function send(text: string): Promise<void> {
  transcript.addSent(text)
  busy.value = true
  try {
    // Fire-and-forget by design: the claw answers 202 and the reply
    // arrives on the event stream.
    await claw.sendMessage(activeKey.value, text)
    apiError.value = null
  } catch (err) {
    noteApiError(err)
  } finally {
    busy.value = false
  }
  void refreshSessions()
}

function saveToken(): void {
  const next = tokenDraft.value.trim()
  token.value = next || null
  persist(TOKEN_STORE_KEY, next || null)
  tokenDraft.value = ''
  authRequired.value = false
  apiError.value = null
  // The stream's retry loop reads the token fresh each attempt, so it
  // recovers on its own; the polls below confirm the token works.
  void checkHealth()
  void refreshProviders()
  void refreshSessions()
}

// --- the one live subscription --------------------------------------
let stream: { close: () => void } | null = null

function resubscribe(): void {
  stream?.close()
  transcript.reset()
  notices.value = []
  streamStatus.value = 'connecting'
  stream = claw.subscribeEvents(activeKey.value, {
    onEvent: (event) => transcript.applyEvent(event),
    onStatus: (status) => {
      streamStatus.value = status
      if (status === 'open') void refreshSessions()
    },
    onNotice: pushNotice,
  })
}

watch(activeKey, resubscribe)

let healthTimer: ReturnType<typeof setInterval> | undefined
let sessionsTimer: ReturnType<typeof setInterval> | undefined

onMounted(() => {
  resubscribe()
  void checkHealth()
  void refreshProviders()
  void refreshSessions()
  healthTimer = setInterval(checkHealth, 30000)
  sessionsTimer = setInterval(() => void refreshSessions(), 20000)
})

onUnmounted(() => {
  stream?.close()
  clearInterval(healthTimer)
  clearInterval(sessionsTimer)
})
</script>

<template>
  <div class="shell">
    <header class="top">
      <span class="brand"><span class="mark" aria-hidden="true">◆</span> rhema claw</span>
      <span class="indicators">
        <span class="indicator" :title="`event stream: ${streamStatus}`">
          <span class="dot" :class="streamStatus" aria-hidden="true" />
          {{ streamStatus === 'open' ? 'live' : streamStatus }}
        </span>
        <span class="indicator" :title="clawUp === null ? 'health unknown' : clawUp ? '/healthz ok' : '/healthz failing'">
          <span class="dot" :class="{ up: clawUp === true, down: clawUp === false }" aria-hidden="true" />
          {{ clawUp === null ? 'checking' : clawUp ? 'claw up' : 'claw down' }}
        </span>
      </span>
    </header>

    <div class="banners">
      <ClawBanner v-if="authRequired" variant="error">
        the claw requires a bearer token on /v1.
        <template #actions>
          <input
            v-model="tokenDraft"
            type="password"
            placeholder="token"
            autocomplete="off"
            @keydown.enter="saveToken"
          />
          <button @click="saveToken">save</button>
        </template>
      </ClawBanner>
      <ClawBanner v-if="clawUp === false" variant="error">
        the claw is unreachable at {{ config.public.clawBase || 'same origin (dev proxy → 127.0.0.1:7799)' }} —
        start it with <code>rhema-claw</code>
      </ClawBanner>
      <ClawBanner v-if="apiError" variant="error" dismissible @dismiss="apiError = null">
        {{ apiError }}
      </ClawBanner>
      <ClawBanner
        v-for="notice in notices"
        :key="notice.id"
        variant="warn"
        dismissible
        @dismiss="notices = notices.filter((n) => n.id !== notice.id)"
      >
        {{ notice.text }}
      </ClawBanner>
    </div>

    <main class="columns">
      <SessionsSidebar
        :sessions="sessions"
        :active-key="activeKey"
        :loading="sessionsLoading"
        @select="activeKey = $event"
        @refresh="refreshSessions"
      />
      <ChatPanel
        :items="transcriptItems"
        :live="liveTurn"
        :active-key="activeKey"
        :can-send="isInteractive(activeKey)"
        :busy="busy"
        @send="send"
      />
      <ProvidersPanel
        :state="providersState"
        :models="providerModels"
        :models-loading="providerModelsLoading"
        :models-error="providerModelsError"
        :selected-provider="selectedProvider"
        :loading="false"
        @refresh="refreshProviders"
        @select="loadProviderModels"
        @switch="switchProvider"
      />
    </main>
  </div>
</template>

<style scoped>
.right-column {
  display: flex;
  flex-direction: column;
  gap: 12px;
  min-width: 0;
}

.shell {
  display: flex;
  flex-direction: column;
  height: 100vh;
}

.top {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 10px 16px;
  border-bottom: 1px solid var(--border);
  background: var(--surface);
}

.brand {
  letter-spacing: 0.06em;
  color: var(--text);
}

.mark {
  color: var(--accent-text);
}

.indicators {
  display: flex;
  gap: 16px;
  font-size: 11px;
  color: var(--text3);
}

.indicator {
  display: inline-flex;
  align-items: center;
  gap: 6px;
}

.dot {
  width: 7px;
  height: 7px;
  border-radius: 50%;
  background: var(--text3);
}

.dot.open,
.dot.up {
  background: var(--ok);
}

.dot.reconnecting,
.dot.down {
  background: var(--error);
}

.dot.connecting {
  background: var(--warn);
}

.banners {
  display: flex;
  flex-direction: column;
  gap: 6px;
  padding: 8px 16px 0;
}

.columns {
  flex: 1;
  display: grid;
  grid-template-columns: 230px minmax(0, 1fr) 300px;
  min-height: 0;
  padding-top: 8px;
}

@media (max-width: 980px) {
  .shell {
    height: auto;
    min-height: 100vh;
  }

  .columns {
    display: flex;
    flex-direction: column;
  }

  .columns > * {
    height: 60vh;
    border: none;
    border-top: 1px solid var(--border);
  }
}
</style>
