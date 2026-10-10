<script setup lang="ts">
// The Model providers page: the primary backend the claw answers turns
// with. Edits stage into a local draft; Save PUTs it (the claw applies
// the pair on the next turn and persists it), Discard reverts to what
// the claw reports. The live model listing comes from the provider
// itself; a kind without one still switches — its default model applies.
// Presentational: data flows in as props, intent flows up as events.
import type { ModelCard, ProvidersState } from '~/composables/useClaw'

const props = defineProps<{
  state: ProvidersState | null
  models: ModelCard[]
  modelsLoading: boolean
  modelsError: string | null
  /** The kind whose listing is loaded/shown in the detail panel. */
  selectedProvider: string | null
  saving: boolean
}>()

const emit = defineEmits<{
  /** Load (or reload) the selected kind's live listing. */
  inspect: [kind: string]
  save: [provider: string, model?: string]
  refresh: []
}>()

// The draft pair. An empty model string means "the kind's default" —
// the PUT omits the model and the claw falls back.
const draftProvider = ref<string | null>(null)
const draftModel = ref('')

const defaultModel = computed(() => {
  const kind = draftProvider.value
  return props.state?.providers.find((p) => p.kind === kind)?.default_model ?? ''
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
  () => {
    if (!dirty.value) syncDraft()
  },
  { immediate: true },
)

function pickProvider(kind: string): void {
  draftProvider.value = kind
  draftModel.value = ''
  emit('inspect', kind)
}

function discard(): void {
  syncDraft()
}

function save(): void {
  if (!dirty.value || draftProvider.value === null) return
  emit('save', draftProvider.value, draftModel.value || undefined)
}
</script>

<template>
  <div class="page">
    <PageHeader title="Model providers" subtitle="The backend the claw answers with — switching persists and applies from the next turn.">
      <template #actions>
        <button :disabled="!dirty || saving" @click="discard">Discard</button>
        <button class="primary" :disabled="!dirty || saving" @click="save">
          {{ saving ? 'Saving…' : 'Save' }}
        </button>
      </template>
    </PageHeader>

    <div class="scroll">
      <AppCallout v-if="dirty" variant="warn" title="1 change not applied">
        The staged pair takes effect on the claw's next turn — no restart
        involved. Save to apply it, Discard to keep the running backend.
      </AppCallout>
      <p v-if="!state" class="muted">loading providers…</p>

      <template v-else>
        <SettingCard title="Primary model" meta="what the agent answers with">
          <SettingRow
            title="Provider"
            description="The constructible backend kind the claw dispatches turns on."
          >
            <select
              :value="draftProvider ?? undefined"
              :disabled="saving"
              @change="pickProvider(($event.target as HTMLSelectElement).value)"
            >
              <option v-for="p in state.providers" :key="p.kind" :value="p.kind">
                {{ p.kind }}
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
                {{ m.id }}
              </option>
            </select>
          </SettingRow>
        </SettingCard>

        <SettingCard title="Available backends" :meta="`${state.providers.length} constructible`">
          <div
            v-for="p in state.providers"
            :key="p.kind"
            class="backend"
            :class="{ staged: p.kind === draftProvider, active: p.kind === state.active.provider }"
            role="button"
            tabindex="0"
            @click="pickProvider(p.kind)"
            @keydown.enter="pickProvider(p.kind)"
          >
            <span class="kind mono">{{ p.kind }}</span>
            <span class="model mono">{{ p.default_model }}</span>
            <AppBadge v-if="p.kind === state.active.provider" variant="accent">active</AppBadge>
            <AppBadge v-else-if="p.kind === draftProvider && dirty">staged</AppBadge>
          </div>
        </SettingCard>
      </template>
    </div>
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
</style>
