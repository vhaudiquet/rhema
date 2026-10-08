<script setup lang="ts">
// The watch table and its add form. Validation here only mirrors what
// the claw enforces (config.rs `validate_watch`): owner/name, and a
// host for self-hosted gitea — the server remains the authority, its
// errors surface as banners.
import type { ClawWatch, WatchInput } from '~/composables/useClaw'

defineProps<{
  watches: ClawWatch[]
  loading: boolean
}>()

const emit = defineEmits<{
  add: [input: WatchInput]
  remove: [forge: string, repo: string]
  refresh: []
}>()

const forges = ['github', 'gitea', 'gitlab'] as const

const forge = ref<string>('github')
const repo = ref('')
const host = ref('')
const token = ref('')

/** A client-side pre-flight; the claw's answer decides. */
const formError = computed(() => {
  const name = repo.value.trim()
  if (!name) return null
  const slash = name.indexOf('/')
  if (slash <= 0 || slash === name.length - 1) return 'repo must be owner/name'
  if (forge.value === 'gitea' && !host.value.trim()) return 'gitea is self-hosted: host required'
  return null
})

function submit(): void {
  if (formError.value || !repo.value.trim()) return
  emit('add', { forge: forge.value, repo: repo.value, host: host.value, token: token.value })
  repo.value = ''
  host.value = ''
  token.value = ''
}
</script>

<template>
  <aside class="panel watches">
    <header class="panel-head">
      <span class="label">Watches</span>
      <button class="ghost" title="refresh watches" :disabled="loading" @click="$emit('refresh')">
        ↻
      </button>
    </header>

    <p v-if="watches.length === 0" class="empty">
      {{ loading ? 'loading…' : 'nothing watched' }}
    </p>
    <ul v-else class="list">
      <li v-for="watch in watches" :key="`${watch.forge}/${watch.repo}`">
        <span class="forge">{{ watch.forge }}</span>
        <span class="repo" :title="watch.host ?? undefined">{{ watch.repo }}</span>
        <button class="ghost remove" title="remove watch" @click="$emit('remove', watch.forge, watch.repo)">
          ×
        </button>
      </li>
    </ul>

    <form class="add" @submit.prevent="submit">
      <span class="label">Add watch</span>
      <select v-model="forge">
        <option v-for="kind in forges" :key="kind" :value="kind">{{ kind }}</option>
      </select>
      <input v-model="repo" type="text" placeholder="owner/name" spellcheck="false" />
      <input
        v-if="forge === 'gitea' || forge === 'gitlab'"
        v-model="host"
        type="text"
        placeholder="https://host (optional)"
        spellcheck="false"
      />
      <input v-model="token" type="password" placeholder="forge token (optional)" autocomplete="off" />
      <p v-if="formError" class="form-error">{{ formError }}</p>
      <button type="submit" :disabled="formError !== null || repo.trim() === ''">add</button>
    </form>
  </aside>
</template>

<style scoped>
.panel {
  display: flex;
  flex-direction: column;
  min-height: 0;
  border-left: 1px solid var(--border);
  background: var(--surface);
}

.panel-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 10px 12px 8px;
  border-bottom: 1px solid var(--border);
}

.ghost {
  padding: 0 6px;
  border: none;
  background: none;
  color: var(--text3);
}

.ghost:hover:not(:disabled) {
  color: var(--accent-text);
}

.empty {
  padding: 12px;
  color: var(--text3);
}

.list {
  list-style: none;
  margin: 0;
  padding: 6px 0;
  overflow-y: auto;
}

.list li {
  display: flex;
  align-items: baseline;
  gap: 8px;
  padding: 4px 12px;
}

.forge {
  flex: none;
  font-size: 11px;
  color: var(--accent-text);
}

.repo {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.remove {
  flex: none;
  font-size: 13px;
}

.add {
  display: flex;
  flex-direction: column;
  gap: 8px;
  margin-top: auto;
  padding: 12px;
  border-top: 1px solid var(--border);
}

.form-error {
  margin: 0;
  font-size: 12px;
  color: var(--error);
}
</style>
