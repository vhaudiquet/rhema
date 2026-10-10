<script setup lang="ts">
// The settings nav: searchable sections of pages, the design's left
// column. The search is a client-side filter over these entries — it
// hides nothing the claw cannot serve.
export interface SettingsEntry {
  id: string
  label: string
}

export interface SettingsSection {
  label: string
  entries: SettingsEntry[]
}

const props = defineProps<{
  sections: SettingsSection[]
  active: string
  /** Health echo for the footer, so the nav answers "am I connected". */
  clawUp: boolean | null
}>()

defineEmits<{ select: [id: string] }>()

const query = ref('')

const filtered = computed<SettingsSection[]>(() => {
  const needle = query.value.trim().toLowerCase()
  if (!needle) return props.sections
  return props.sections
    .map((section) => ({
      label: section.label,
      entries: section.entries.filter((entry) =>
        entry.label.toLowerCase().includes(needle),
      ),
    }))
    .filter((section) => section.entries.length > 0)
})
</script>

<template>
  <aside class="nav">
    <div class="search">
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" aria-hidden="true">
        <circle cx="11" cy="11" r="6.5" />
        <path d="m20 20-3.8-3.8" />
      </svg>
      <input v-model="query" type="text" placeholder="Search settings" />
    </div>

    <p v-if="filtered.length === 0" class="empty">no settings match</p>

    <div v-for="section in filtered" :key="section.label" class="section">
      <span class="label">{{ section.label }}</span>
      <button
        v-for="entry in section.entries"
        :key="entry.id"
        class="entry"
        :class="{ active: entry.id === active }"
        @click="$emit('select', entry.id)"
      >
        {{ entry.label }}
      </button>
    </div>

    <div class="spacer" />

    <footer class="foot">
      <StatusDot :tone="clawUp === null ? 'idle' : clawUp ? 'ok' : 'error'" />
      <span>{{ clawUp === null ? 'checking claw' : clawUp ? 'claw connected' : 'claw unreachable' }}</span>
    </footer>
  </aside>
</template>

<style scoped>
.nav {
  display: flex;
  flex-direction: column;
  gap: 4px;
  width: 232px;
  flex: none;
  min-height: 0;
  padding: 14px 10px 10px;
  border-right: 1px solid var(--border);
  background: var(--surface);
  overflow-y: auto;
}

.search {
  position: relative;
  margin-bottom: 12px;
}

.search svg {
  position: absolute;
  left: 9px;
  top: 50%;
  width: 14px;
  height: 14px;
  transform: translateY(-50%);
  color: var(--text3);
  pointer-events: none;
}

.search input {
  width: 100%;
  padding: 6px 10px 6px 28px;
  font-size: 13px;
}

.empty {
  margin: 4px 6px;
  color: var(--text3);
}

.section {
  display: flex;
  flex-direction: column;
  gap: 2px;
  margin-bottom: 14px;
}

.section .label {
  padding: 0 10px 4px;
}

.entry {
  text-align: left;
  background: none;
  border: none;
  border-radius: 7px;
  padding: 6px 10px;
  color: var(--text2);
}

.entry:hover:not(:disabled) {
  background: var(--elevated);
  color: var(--text);
}

.entry.active {
  background: color-mix(in srgb, var(--accent) 17%, transparent);
  color: var(--accent-text);
  font-weight: 600;
}

.spacer {
  flex: 1;
}

.foot {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 10px;
  border-top: 1px solid var(--border);
  color: var(--text3);
  font-size: 12px;
}
</style>
