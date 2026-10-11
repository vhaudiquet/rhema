<script setup lang="ts">
// The one page: the icon rail, then either the chat view (session
// inventory + live transcript) or the settings view (nav + pages). A
// tiny hash router keeps views deep-linkable without vue-router. All
// state is in-memory; only the token and the active session persist.
// The claw owns the conversation and the config, so this page is a
// mirror, not a store.
import { ClawError, clawHealth, isInteractive, useClaw } from '~/composables/useClaw'
import type {
  AgentIdentity,
  AgentImageKind,
  ClawSession,
  IdentityTexts,
  ModelCard,
  ProviderCredentials,
  ProvidersState,
  StreamStatus,
} from '~/composables/useClaw'
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

// --- the two views ---------------------------------------------------
type Route = { view: 'chat' } | { view: 'settings'; section: string }

const SETTINGS_SECTIONS: { label: string; entries: { id: string; label: string }[] }[] = [
  {
    label: 'Agent',
    entries: [
      { id: 'agent/general', label: 'General' },
      { id: 'agent/accounts', label: 'Accounts' },
    ],
  },
  { label: 'Providers', entries: [{ id: 'providers', label: 'Model providers' }] },
  { label: 'System', entries: [{ id: 'about', label: 'About' }] },
]

function parseHash(): Route {
  if (!import.meta.client) return { view: 'chat' }
  const hash = window.location.hash.replace(/^#\/?/, '')
  if (hash.startsWith('settings')) {
    // Section ids may carry their category ('agent/general'), so the
    // section is everything after 'settings/', not the second segment.
    const section = hash.replace(/^settings\/?/, '') || 'agent/general'
    return {
      view: 'settings',
      section: SETTINGS_SECTIONS.some((s) => s.entries.some((e) => e.id === section))
        ? section
        : 'providers',
    }
  }
  return { view: 'chat' }
}

const route = ref<Route>(parseHash())

function go(next: Route): void {
  window.location.hash = next.view === 'chat' ? '#/chat' : `#/settings/${next.section}`
}

function onHashChange(): void {
  route.value = parseHash()
}

// --- claw state ------------------------------------------------------
const config = useRuntimeConfig()
const token = ref<string | null>(
  stored(TOKEN_STORE_KEY) ?? (config.public.clawToken || null),
)
const claw = useClaw(token)

const activeKey = ref(stored(ACTIVE_STORE_KEY) ?? DEFAULT_KEY)
watch(activeKey, (key) => persist(ACTIVE_STORE_KEY, key))

const transcript = useTranscript()
const { items: transcriptItems, live: liveTurn } = transcript

// --- the agent's identity --------------------------------------------
const identity = ref<AgentIdentity | null>(null)
const identityAvatarUrl = ref<string | null>(null)
const identityLogoUrl = ref<string | null>(null)

async function loadImageUrls(): Promise<void> {
  // A new object URL per fetch; the old one is revoked, not leaked.
  for (const [kind, url] of [
    ['avatar', identityAvatarUrl],
    ['logo', identityLogoUrl],
  ] as const) {
    if (!identity.value?.[kind]) {
      if (url.value) URL.revokeObjectURL(url.value)
      url.value = null
      continue
    }
    if (url.value) URL.revokeObjectURL(url.value)
    try {
      url.value = await claw.agentImage(kind)
    } catch {
      url.value = null
    }
  }
}

async function refreshIdentity(): Promise<void> {
  try {
    identity.value = await claw.agent()
    await loadImageUrls()
  } catch (err) {
    noteApiError(err)
  }
}

async function saveIdentity(texts: IdentityTexts): Promise<void> {
  savingIdentity.value = true
  try {
    identity.value = await claw.setAgent(texts)
    pushNotice('identity saved')
    apiError.value = null
  } catch (err) {
    noteApiError(err)
  } finally {
    savingIdentity.value = false
  }
}

async function uploadIdentityImage(kind: AgentImageKind, file: File): Promise<void> {
  mutatingIdentity.value = true
  try {
    identity.value = await claw.setAgentImage(kind, file)
    await loadImageUrls()
    pushNotice(`${kind} updated`)
    apiError.value = null
  } catch (err) {
    noteApiError(err)
  } finally {
    mutatingIdentity.value = false
  }
}

async function clearIdentityImage(kind: AgentImageKind): Promise<void> {
  mutatingIdentity.value = true
  try {
    identity.value = await claw.clearAgentImage(kind)
    await loadImageUrls()
    pushNotice(`${kind} cleared`)
  } catch (err) {
    noteApiError(err)
  } finally {
    mutatingIdentity.value = false
  }
}

// The window title answers to the identity and the view, as the design
// shows it: "Kosmos — Settings · Model providers".
const viewTitle = computed(() => {
  const current = route.value
  if (current.view === 'chat') return 'Chat'
  const label = SETTINGS_SECTIONS.flatMap((section) => section.entries).find(
    (entry) => entry.id === current.section,
  )?.label
  return `Settings · ${label ?? current.section}`
})
watch(
  [identity, viewTitle],
  () => {
    const name = identity.value?.name?.trim()
    document.title = name ? `${name} — ${viewTitle.value}` : `rhema claw — ${viewTitle.value}`
  },
  { immediate: true },
)

const clawUp = ref<boolean | null>(null)
const sessions = ref<ClawSession[]>([])
const sessionsLoading = ref(false)
const providersState = ref<ProvidersState | null>(null)
const providerModels = ref<ModelCard[]>([])
const providerModelsLoading = ref(false)
const providerModelsError = ref<string | null>(null)
const selectedProvider = ref<string | null>(null)
const savingProviders = ref(false)
const mutatingProviders = ref(false)
const savingIdentity = ref(false)
const mutatingIdentity = ref(false)
const streamStatus = ref<StreamStatus>('connecting')
const busy = ref(false)
const authRequired = ref(false)
const tokenDraft = ref('')
const apiError = ref<string | null>(null)
const notices = ref<{ id: number; text: string }[]>([])
let noticeId = 1

const activeSession = computed(
  () => sessions.value.find((session) => session.key === activeKey.value) ?? null,
)

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

// --- claw calls ------------------------------------------------------
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

async function loadProviderModels(id: string): Promise<void> {
  selectedProvider.value = id
  providerModelsLoading.value = true
  providerModelsError.value = null
  try {
    providerModels.value = await claw.models(id)
  } catch (err) {
    // A backend without a live listing is normal (404): the operator
    // still sees the switch go through on the default model.
    providerModels.value = []
    providerModelsError.value = err instanceof Error ? err.message : String(err)
  } finally {
    providerModelsLoading.value = false
  }
}

async function saveProviders(provider: string, model?: string): Promise<void> {
  savingProviders.value = true
  try {
    // The route answers the new pair only; carry the provider list
    // forward so the state stays whole.
    const { active } = await claw.setActive(provider, model)
    providersState.value = providersState.value
      ? { ...providersState.value, active }
      : { active, providers: [] }
    pushNotice(`active backend: ${provider} / ${active.model}`)
    apiError.value = null
  } catch (err) {
    noteApiError(err)
  } finally {
    savingProviders.value = false
  }
}

async function addProvider(id: string, credentials: ProviderCredentials): Promise<void> {
  mutatingProviders.value = true
  const existed = providersState.value?.providers.some((p) => p.id === id) ?? false
  try {
    // The login answers the whole state: the roster with the new (or
    // replaced) backend instance on it.
    providersState.value = await claw.setCredentials(id, credentials)
    selectedProvider.value = id
    await loadProviderModels(id)
    pushNotice(existed ? `provider ${id} replaced` : `provider ${id} added`)
    apiError.value = null
  } catch (err) {
    noteApiError(err)
  } finally {
    mutatingProviders.value = false
  }
}

async function removeProvider(id: string): Promise<void> {
  mutatingProviders.value = true
  try {
    providersState.value = await claw.removeProvider(id)
    // The removal may have moved the active pair, and the removed
    // instance cannot keep the detail panel: both re-ground on what
    // remains.
    if (selectedProvider.value === id) {
      selectedProvider.value = providersState.value.active.provider
      await loadProviderModels(selectedProvider.value)
    }
    pushNotice(`provider ${id} removed`)
    apiError.value = null
  } catch (err) {
    noteApiError(err)
  } finally {
    mutatingProviders.value = false
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

// --- the one live subscription ---------------------------------------
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
      // A retry reopens the stream with the claw's replay of the
      // recorded turns: drop the mirror and let it rebuild, or the
      // replay would double everything already on screen.
      if (status === 'reconnecting') transcript.reset()
      if (status === 'open') void refreshSessions()
    },
    onNotice: pushNotice,
  })
}

watch(activeKey, resubscribe)

let healthTimer: ReturnType<typeof setInterval> | undefined
let sessionsTimer: ReturnType<typeof setInterval> | undefined

onMounted(() => {
  window.addEventListener('hashchange', onHashChange)
  resubscribe()
  void checkHealth()
  void refreshIdentity()
  void refreshProviders()
  void refreshSessions()
  healthTimer = setInterval(checkHealth, 30000)
  sessionsTimer = setInterval(() => void refreshSessions(), 20000)
})

onUnmounted(() => {
  window.removeEventListener('hashchange', onHashChange)
  stream?.close()
  clearInterval(healthTimer)
  clearInterval(sessionsTimer)
})
</script>

<template>
  <div class="shell">
    <AppRail
      :view="route.view"
      :claw-up="clawUp"
      :identity="identity"
      :avatar-url="identityAvatarUrl"
      @navigate="go($event === 'chat' ? { view: 'chat' } : { view: 'settings', section: 'providers' })"
    />

    <div class="body">
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
          the claw is unreachable at {{ config.public.clawBase || 'same origin (dev proxy → 127.0.0.1:7799)' }};
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

      <main class="view">
        <template v-if="route.view === 'chat'">
          <SessionsSidebar
            :sessions="sessions"
            :active-key="activeKey"
            :loading="sessionsLoading"
            :identity="identity"
            :avatar-url="identityAvatarUrl"
            @select="activeKey = $event"
            @refresh="refreshSessions"
          />
          <ChatPanel
            :items="transcriptItems"
            :live="liveTurn"
            :active-key="activeKey"
            :can-send="isInteractive(activeKey)"
            :busy="busy"
            :session="activeSession"
            :active-backend="providersState?.active ?? null"
            :stream-status="streamStatus"
            :identity="identity"
            :avatar-url="identityAvatarUrl"
            @send="send"
            @open-providers="go({ view: 'settings', section: 'providers' })"
          />
        </template>

        <template v-else>
          <SettingsNav
            :sections="SETTINGS_SECTIONS"
            :active="route.section"
            :claw-up="clawUp"
            @select="go({ view: 'settings', section: $event })"
          />
          <SettingsAgentGeneralPage
            v-show="route.section === 'agent/general'"
            :identity="identity"
            :avatar-url="identityAvatarUrl"
            :logo-url="identityLogoUrl"
            :saving="savingIdentity"
            :mutating="mutatingIdentity"
            @save="saveIdentity"
            @upload-image="uploadIdentityImage"
            @clear-image="clearIdentityImage"
          />
          <SettingsAgentAccountsPage v-show="route.section === 'agent/accounts'" />
          <SettingsProvidersPage
            v-show="route.section === 'providers'"
            :state="providersState"
            :models="providerModels"
            :models-loading="providerModelsLoading"
            :models-error="providerModelsError"
            :selected-provider="selectedProvider"
            :saving="savingProviders"
            :mutating="mutatingProviders"
            @inspect="loadProviderModels"
            @save="saveProviders"
            @add="addProvider"
            @remove="removeProvider"
          />
          <SettingsAboutPage v-show="route.section === 'about'" :claw-up="clawUp" />
          <DetailPanel v-if="route.section === 'providers'" title="Backend">
            <template v-if="selectedProvider">
              <div class="detail-name">
                <strong class="mono">{{ selectedProvider }}</strong>
                <AppBadge
                  :variant="providerModelsError ? 'warn' : providerModelsLoading ? undefined : 'ok'"
                >
                  {{ providerModelsLoading
                    ? 'listing…'
                    : providerModelsError
                      ? 'no live listing'
                      : `${providerModels.length} models` }}
                </AppBadge>
              </div>
              <p v-if="providerModelsError" class="detail-note">
                {{ providerModelsError }}; the backend still switches and
                its default model applies.
              </p>
              <div v-if="providerModels.length > 0" class="detail-models">
                <span class="label">Live listing</span>
                <div v-for="m in providerModels" :key="m.id" class="detail-model mono">
                  {{ m.id }}
                </div>
              </div>
              <button class="ghost" :disabled="providerModelsLoading" @click="loadProviderModels(selectedProvider)">
                ↻ reload listing
              </button>
            </template>
            <p v-else class="detail-note">loading…</p>
          </DetailPanel>
          <DetailPanel v-else-if="route.section === 'agent/general'" title="Where identity lives">
            <p class="detail-note">
              The name, mark, and images are stored claw-side and served
              over <code>/v1/agent</code> — every client renders the same
              identity. Behaviour is a different file: the agent's
              persona lives in its <code>SOUL.md</code>, written with the
              agent over onboarding, not here.
            </p>
          </DetailPanel>
          <DetailPanel v-else-if="route.section === 'about'" title="Where things live">
            <p class="detail-note">
              The claw's code, including this client, sits in the rhema
              workspace: <code>crates/rhema-claw</code> for the daemon,
              <code>web/claw</code> for this UI. Design decisions live in
              <code>SPEC.md</code>.
            </p>
          </DetailPanel>
        </template>
      </main>
    </div>
  </div>
</template>

<style scoped>
.shell {
  display: flex;
  height: 100vh;
}

.body {
  display: flex;
  flex-direction: column;
  flex: 1;
  min-width: 0;
}

.banners {
  display: flex;
  flex-direction: column;
  gap: 6px;
  padding: 8px 16px 0;
}

.view {
  flex: 1;
  display: flex;
  min-height: 0;
}

.view > * {
  min-height: 0;
}

.detail-name {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
}

.detail-name strong {
  font-size: 14px;
}

.detail-note {
  margin: 0;
  color: var(--text3);
  font-size: 12.5px;
}

.detail-models {
  display: flex;
  flex-direction: column;
  gap: 4px;
}

.detail-model {
  color: var(--text2);
  font-size: 12px;
  padding: 3px 0;
  border-bottom: 1px solid var(--border);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

@media (max-width: 980px) {
  .shell {
    height: auto;
    min-height: 100vh;
  }

  .view {
    flex-direction: column;
  }

  .view > * {
    height: 60vh;
    border-top: 1px solid var(--border);
  }
}
</style>
