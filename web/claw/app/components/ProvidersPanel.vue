<script setup lang="ts">
// The backend switch: the constructible providers, the active pair, and
// each provider's live model listing. Switching is a PUT the claw
// persists — no restart, and the next turn runs on the new pair.
// Presentational: data flows in as props, intent flows up as events.
import type { ClawProvider, ModelCard, ProvidersState } from '~/composables/useClaw'

defineProps<{
  state: ProvidersState | null
  models: ModelCard[]
  modelsLoading: boolean
  modelsError: string | null
  selectedProvider: string | null
  loading: boolean
}>()

const emit = defineEmits<{
  refresh: []
  select: [kind: string]
  switch: [provider: string, model?: string]
}>()
</script>

<template>
  <section class="panel">
    <header class="panel-head">
      <h2>Providers</h2>
      <button class="ghost" :disabled="loading" @click="emit('refresh')">refresh</button>
    </header>

    <div v-if="!state" class="muted">loading…</div>
    <template v-else>
      <div class="active">
        <span class="muted small">active</span>
        <strong class="kind">{{ state.active.provider }}</strong>
        <span class="model">{{ state.active.model }}</span>
      </div>

      <div class="providers">
        <button
          v-for="p in state.providers as ClawProvider[]"
          :key="p.kind"
          class="provider"
          :class="{ current: p.kind === state.active.provider }"
          :disabled="loading"
          @click="emit('select', p.kind)"
        >
          {{ p.kind }}
        </button>
      </div>

      <div v-if="selectedProvider" class="models">
        <div class="muted small">
          models of {{ selectedProvider }}
          <span v-if="modelsLoading"> · loading…</span>
          <span v-else-if="modelsError" class="dim"> · {{ modelsError }}</span>
        </div>
        <div v-if="!modelsLoading && models.length" class="model-list">
          <button
            v-for="m in models as ModelCard[]"
            :key="m.id"
            class="model-row"
            :class="{ current: m.id === state.active.model }"
            @click="emit('switch', selectedProvider, m.id)"
          >
            <span class="id">{{ m.id }}</span>
            <span v-if="m.display_name && m.display_name !== m.id" class="muted small">
              {{ m.display_name }}
            </span>
          </button>
        </div>
      </div>
    </template>
  </section>
</template>

<style scoped>
.panel {
  border: 1px solid var(--border);
  border-radius: 8px;
  padding: 12px;
  display: flex;
  flex-direction: column;
  gap: 10px;
}

.panel-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
}

.panel-head h2 {
  margin: 0;
  font-size: 12px;
  text-transform: uppercase;
  letter-spacing: 0.08em;
  color: var(--text2);
}

.ghost {
  background: none;
  border: none;
  color: var(--accent-text);
  cursor: pointer;
  font: inherit;
  font-size: 12px;
  padding: 2px 4px;
}

.active {
  display: flex;
  align-items: baseline;
  gap: 8px;
}

.kind {
  font-size: 14px;
}

.model {
  font-family: var(--mono, monospace);
  color: var(--accent-text);
  font-size: 13px;
}

.providers {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
}

.provider {
  background: var(--surface);
  border: 1px solid var(--border);
  border-radius: 6px;
  color: var(--text2);
  cursor: pointer;
  font: inherit;
  font-size: 13px;
  padding: 4px 10px;
}

.provider.current {
  border-color: var(--accent);
  color: var(--accent-text);
}

.models {
  display: flex;
  flex-direction: column;
  gap: 6px;
}

.small {
  font-size: 11px;
}

.model-list {
  display: flex;
  flex-direction: column;
  max-height: 220px;
  overflow-y: auto;
}

.model-row {
  background: none;
  border: none;
  border-left: 2px solid transparent;
  color: var(--text);
  cursor: pointer;
  display: flex;
  flex-direction: column;
  align-items: flex-start;
  font: inherit;
  font-size: 13px;
  padding: 5px 8px;
  text-align: left;
}

.model-row:hover {
  background: var(--elevated);
}

.model-row.current {
  border-left-color: var(--accent);
  color: var(--accent-text);
}

.muted {
  color: var(--text3);
}

.dim {
  color: var(--text3);
  font-style: italic;
}
</style>
