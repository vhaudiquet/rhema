<script setup lang="ts">
// The Model providers page: the primary backend the claw answers turns
// with, plus the provider roster itself. Pair edits stage into a local
// draft; Save PUTs it (the claw applies the pair on the next turn and
// persists it), Discard reverts to what the claw reports. Logins apply
// immediately: adding an instance constructs its backend claw-side and
// persists its credentials, removing one drops it (env-backed kinds
// re-discover on restart — the source badge says which). The live
// model listing comes from the provider itself; a backend without one
// still switches — its default model applies.
// Presentational: data flows in as props, intent flows up as events.
import type { ModelCard, ProviderCredentials, ProvidersState } from '~/composables/useClaw'

const props = defineProps<{
  state: ProvidersState | null
  models: ModelCard[]
  modelsLoading: boolean
  modelsError: string | null
  /** The instance whose live listing the Model row shows. */
  selectedProvider: string | null
  saving: boolean
  /** A login or a removal is in flight. */
  mutating: boolean
}>()

const emit = defineEmits<{
  /** Load (or reload) the selected instance's live listing. */
  inspect: [id: string]
  save: [provider: string, model?: string]
  refresh: []
  /** Add a backend instance (or replace an id's credentials). */
  add: [id: string, credentials: ProviderCredentials]
  /** Remove a backend instance from the claw's roster. */
  remove: [id: string]
}>()

// The draft pair. An empty model string means "the provider's default" —
// the PUT omits the model and the claw falls back.
const draftProvider = ref<string | null>(null)
const draftModel = ref('')

const defaultModel = computed(() => {
  const id = draftProvider.value
  return props.state?.providers.find((p) => p.id === id)?.default_model ?? ''
})

const effectiveModel = computed(() => draftModel.value || defaultModel.value)

const dirty = computed(() => {
  const active = props.state?.active
  if (!active || draftProvider.value === null) return false
  return (
    draftProvider.value !== active.provider || effectiveModel.value !== active.model
  )
})

/** Re-ground the draft on the claw's state; skipped while dirty. */
function syncDraft(): void {
  const active = props.state?.active
  if (!active) {
    draftProvider.value = null
    draftModel.value = ''
    return
  }
  draftProvider.value = active.provider
  draftModel.value = active.model === defaultModel.value ? '' : active.model
}

watch(
  () => props.state,
  (state) => {
    // A removal can take the staged instance with it: the draft
    // re-grounds even when dirty, because staging a gone provider
    // saves nothing.
    const stagedGone =
      draftProvider.value !== null &&
      !state?.providers.some((p) => p.id === draftProvider.value)
    if (!dirty.value || stagedGone) syncDraft()
  },
  { immediate: true },
)

function pickProvider(id: string): void {
  draftProvider.value = id
  draftModel.value = ''
  emit('inspect', id)
}

function discard(): void {
  syncDraft()
}

function save(): void {
  if (!dirty.value || draftProvider.value === null) return
  emit('save', draftProvider.value, draftModel.value || undefined)
}

// --- the login form ---------------------------------------------------
const KIND_LABELS: Record<string, string> = {
  anthropic: 'Anthropic',
  copilot: 'GitHub Copilot',
  deepseek: 'DeepSeek',
  google: 'Google Gemini',
  'llama-cpp': 'llama.cpp server',
  openai: 'OpenAI',
  openrouter: 'OpenRouter',
  zai: 'Z.ai (GLM Coding Plan)',
}

function kindLabel(kind: string): string {
  return KIND_LABELS[kind] ?? kind
}

/** The key field's voice: copilot logs in with a GitHub token. */
const keyLabel = computed(() =>
  addKind.value === 'copilot' ? 'GitHub token' : 'API key',
)

/** A field the daemon does not demand reads as optional, not hidden. */
function label(name: string, base: string): string {
  return required(name) ? base : `${base} (optional)`
}

const loginOpen = ref(false)
const addKind = ref<string>('')
const addName = ref('')
const addApiKey = ref('')
const addUrl = ref('')
/** Set when a login is posted; the state swap that answers it closes
 * the form (a failure leaves the state, and the form, as they were). */
const pendingAdd = ref<string | null>(null)

const addable = computed(() => props.state?.addable ?? [])

const addFields = computed(() => {
  const entry = addable.value.find((entry) => entry.kind === addKind.value)
  return entry?.fields ?? []
})

/** Whether the selected kind's form declares this field at all. */
function declared(name: string): boolean {
  return addFields.value.some((field) => field.name === name)
}

/** Whether the daemon refuses the login without this field. */
function required(name: string): boolean {
  return addFields.value.find((field) => field.name === name)?.required ?? false
}

function fieldValue(name: string): string {
  return name === 'url' ? addUrl.value : name === 'api_key' ? addApiKey.value : ''
}

/** The id the login lands under: the name, or the kind unnamed. */
const resultingId = computed(() => addName.value.trim() || addKind.value)

const nameValid = computed(() => {
  const name = addName.value.trim()
  return name === '' || (name.length <= 64 && !/\s/.test(name) && !name.includes('/'))
})

const addIdPresent = computed(() =>
  props.state?.providers.some((p) => p.id === resultingId.value) ?? false,
)

const addReady = computed(() => {
  if (!addKind.value || !nameValid.value) return false
  return addFields.value
    .filter((field) => field.required)
    .every((field) => fieldValue(field.name).trim() !== '')
})

function submitAdd(): void {
  if (!addReady.value || props.mutating) return
  // Only declared fields go out, and only filled ones: the claw treats
  // an empty string as the value it was handed, not as "absent".
  const credentials: ProviderCredentials = { kind: addKind.value }
  if (addName.value.trim() !== '') credentials.name = addName.value.trim()
  if (declared('api_key') && addApiKey.value.trim() !== '') {
    credentials.api_key = addApiKey.value.trim()
  }
  if (declared('url') && addUrl.value.trim() !== '') {
    credentials.url = addUrl.value.trim()
  }
  pendingAdd.value = resultingId.value
  emit('add', resultingId.value, credentials)
}

function sourceTitle(source: string): string {
  switch (source) {
    case 'env':
      return 'credential from the environment; a removal lasts until restart'
    case 'stored':
      return 'credential stored claw-side through this surface'
    case 'profile':
      return 'credential from the stored rhema config'
    default:
      return 'serves without a credential'
  }
}

// A login landed: the claw answered with a new state object (a failed
// one leaves the old, and the banner names the error), so the form
// forgets what it held and closes.
watch(
  () => props.state,
  (state, previous) => {
    if (pendingAdd.value !== null && state !== previous) {
      pendingAdd.value = null
      loginOpen.value = false
      addName.value = ''
      addApiKey.value = ''
      addUrl.value = ''
      addKind.value = ''
    }
  },
)

/** The listing's vision verdict, spelled the way the TUI's picker
 * spells it: a mark when the adapter knows the model takes images, a
 * bare "no vision" when it knows it does not, nothing when the key is
 * absent — unknown is no verdict at all, never a `false`. */
function visionLabel(model: ModelCard): string {
  if (model.supports_vision === true) return ' ◉ vision'
  if (model.supports_vision === false) return ' no vision'
  return ''
}
</script>

<template>
  <div class="page">
    <PageHeader title="Model providers" subtitle="The backends the claw can answer with. Logins apply immediately; the pair switch persists.">
      <template #actions>
        <button :disabled="mutating" @click="loginOpen = true">Add provider</button>
        <button :disabled="!dirty || saving" @click="discard">Discard</button>
        <button class="primary" :disabled="!dirty || saving" @click="save">
          {{ saving ? 'Saving…' : 'Save' }}
        </button>
      </template>
    </PageHeader>

    <div class="scroll">
      <AppCallout v-if="dirty" variant="warn" title="1 change not applied">
        The staged pair takes effect on the claw's next turn, no restart
        involved. Save to apply it, Discard to keep the running backend.
      </AppCallout>
      <p v-if="!state" class="muted">loading providers…</p>

      <template v-else>
        <SettingCard title="Primary model" meta="what the agent answers with">
          <SettingRow
            title="Provider"
            description="The backend instance the claw dispatches turns on."
          >
            <select
              :value="draftProvider ?? undefined"
              :disabled="saving"
              @change="pickProvider(($event.target as HTMLSelectElement).value)"
            >
              <option v-for="p in state.providers" :key="p.id" :value="p.id">
                {{ p.id }}
              </option>
            </select>
          </SettingRow>
          <SettingRow
            title="Model"
            description="Left on the default, the provider's own model answers."
          >
            <select v-model="draftModel" :disabled="saving || !draftProvider">
              <option value="">(provider default)</option>
              <option v-for="m in models" :key="m.id" :value="m.id">
                {{ m.id }}{{ visionLabel(m) }}
              </option>
            </select>
          </SettingRow>
        </SettingCard>

        <SettingCard
          title="Available backends"
          :meta="`${state.providers.length} constructed · env-backed kinds return on restart`"
        >
          <div
            v-for="p in state.providers"
            :key="p.id"
            class="backend"
            :class="{ staged: p.id === draftProvider, active: p.id === state.active.provider }"
            role="button"
            tabindex="0"
            @click="pickProvider(p.id)"
            @keydown.enter="pickProvider(p.id)"
          >
            <span class="kind mono">{{ p.id }}</span>
            <AppBadge
              v-if="p.kind !== p.id"
              variant="dim"
              :title="`a ${p.kind} backend`"
            >{{ p.kind }}</AppBadge>
            <span class="model mono">{{ p.default_model }}</span>
            <AppBadge
              v-if="p.source"
              variant="dim"
              :title="sourceTitle(p.source)"
            >{{ p.source }}</AppBadge>
            <AppBadge v-if="p.id === state.active.provider" variant="accent">active</AppBadge>
            <AppBadge v-else-if="p.id === draftProvider && dirty">staged</AppBadge>
            <button
              class="ghost remove"
              :title="`remove ${p.id}`"
              :disabled="mutating || state.providers.length <= 1"
              @click.stop="emit('remove', p.id)"
            >
              ×
            </button>
          </div>
        </SettingCard>

      </template>
    </div>

    <AppModal :open="loginOpen" title="Add a provider" @close="loginOpen = false">
      <div v-if="state" class="add-form">
        <div class="add-row">
          <label class="add-label" for="add-kind">Kind</label>
          <select id="add-kind" v-model="addKind" :disabled="mutating">
            <option value="" disabled>choose a provider…</option>
            <option v-for="entry in addable" :key="entry.kind" :value="entry.kind">
              {{ kindLabel(entry.kind) }}{{ state.providers.some((p) => p.id === entry.kind) ? '; replaces its credentials' : '' }}
            </option>
          </select>
        </div>
        <div class="add-row">
          <label class="add-label" for="add-name">Name</label>
          <input
            id="add-name"
            v-model="addName"
            type="text"
            placeholder="name (optional, defaults to the provider kind)"
            autocomplete="off"
            :disabled="mutating"
            class="mono"
          />
        </div>
        <div v-if="declared('url')" class="add-row">
          <label class="add-label" for="add-url">{{ label('url', 'Server URL') }}</label>
          <input
            id="add-url"
            v-model="addUrl"
            type="text"
            placeholder="http://127.0.0.1:8080"
            autocomplete="off"
            :disabled="mutating"
            class="mono"
          />
        </div>
        <div v-if="declared('api_key')" class="add-row">
          <label class="add-label" for="add-key">{{ label('api_key', keyLabel) }}</label>
          <input
            id="add-key"
            v-model="addApiKey"
            type="password"
            placeholder="stored claw-side, never shown again"
            autocomplete="off"
            :disabled="mutating"
          />
        </div>
        <p v-if="addName.trim() && !nameValid" class="add-note">
          a name carries no whitespace or '/' and stays under 64 characters.
        </p>
        <p v-else-if="addIdPresent" class="add-note">
          {{ resultingId }} is already serving. saving replaces its
          credentials and restarts nothing; the active pair is untouched.
        </p>
        <p v-else-if="addKind" class="add-note">
          saving adds a new {{ kindLabel(addKind) }} backend{{ addName.trim() ? ` named ${resultingId}` : '' }}.
        </p>
      </div>
      <template #actions>
        <button @click="loginOpen = false">Cancel</button>
        <button class="primary" :disabled="!addReady || mutating" @click="submitAdd">
          {{ mutating ? 'Working…' : addIdPresent ? 'Replace credentials' : 'Add provider' }}
        </button>
      </template>
    </AppModal>
  </div>
</template>

<style scoped>
.page {
  display: flex;
  flex-direction: column;
  min-height: 0;
  min-width: 0;
  flex: 1;
}

.scroll {
  flex: 1;
  overflow-y: auto;
  padding: 0 24px 24px;
  display: flex;
  flex-direction: column;
  gap: 16px;
  max-width: 860px;
}

.muted {
  color: var(--text3);
  padding: 8px 2px;
}

.backend {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 11px 16px;
  cursor: pointer;
}

.backend:hover {
  background: var(--elevated);
}

.backend.staged {
  background: color-mix(in srgb, var(--accent) 8%, var(--surface));
}

.backend .kind {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  color: var(--text);
}

.backend .model {
  color: var(--text3);
  font-size: 12px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.backend .remove {
  flex: none;
  padding: 0 6px;
  font-size: 14px;
}

.add-form {
  display: flex;
  flex-direction: column;
  gap: 12px;
  padding: 16px;
}

.add-row {
  display: flex;
  align-items: center;
  gap: 12px;
}

.add-label {
  width: 110px;
  flex: none;
  color: var(--text3);
  font-size: 12.5px;
}

.add-row select,
.add-row input {
  flex: 1;
  min-width: 0;
}

.add-note {
  margin: 0;
  color: var(--text3);
  font-size: 12.5px;
}

.add-actions {
  display: flex;
  align-items: center;
  gap: 12px;
}

.add-actions .callout {
  flex: 1;
}

@media (max-width: 760px) {
  .add-row {
    flex-direction: column;
    align-items: stretch;
  }

  .add-label {
    width: auto;
  }
}
</style>
