<script setup lang="ts">
// The session inventory: every dispatch key the claw holds, interactive
// first. Clicking selects the session the chat mirrors.
import type { ClawSession } from '~/composables/useClaw'

const props = defineProps<{
  sessions: ClawSession[]
  activeKey: string
  loading: boolean
}>()

defineEmits<{ select: [key: string]; refresh: [] }>()

// Interactive sessions are the chat surface; forge keys read after.
const sorted = computed(() => {
  const interactive = props.sessions.filter((session) => session.key.startsWith('interactive:'))
  const forge = props.sessions.filter((session) => !session.key.startsWith('interactive:'))
  const byKey = (a: ClawSession, b: ClawSession) => a.key.localeCompare(b.key)
  return [...interactive.sort(byKey), ...forge.sort(byKey)]
})

function stateClass(state: string): string {
  if (state === 'leased') return 'leased'
  if (state === 'reacquire') return 'reacquire'
  return 'idle'
}
</script>

<template>
  <aside class="panel sessions">
    <header class="panel-head">
      <span class="label">Sessions</span>
      <button class="ghost" title="refresh sessions" :disabled="loading" @click="$emit('refresh')">
        ↻
      </button>
    </header>
    <p v-if="sessions.length === 0" class="empty">
      {{ loading ? 'loading…' : 'no sessions yet — send the first message' }}
    </p>
    <ul v-else class="list">
      <li
        v-for="session in sorted"
        :key="session.key"
        :class="{ active: session.key === activeKey }"
      >
        <button class="row" :title="session.key" @click="$emit('select', session.key)">
          <span class="key">{{ session.key }}</span>
          <span class="meta">
            <span class="state" :class="stateClass(session.state)">{{ session.state }}</span>
            <span v-if="session.inbox_depth > 0" class="inbox" title="inbox depth">
              {{ session.inbox_depth }}
            </span>
          </span>
        </button>
      </li>
    </ul>
  </aside>
</template>

<style scoped>
.panel {
  display: flex;
  flex-direction: column;
  min-height: 0;
  border-right: 1px solid var(--border);
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
  padding: 4px 0;
  overflow-y: auto;
}

.row {
  display: flex;
  flex-direction: column;
  gap: 2px;
  width: 100%;
  text-align: left;
  background: none;
  border: none;
  border-left: 2px solid transparent;
  border-radius: 0;
  padding: 6px 12px;
}

.row:hover:not(:disabled) {
  background: var(--elevated);
  color: var(--text);
}

li.active .row {
  background: var(--elevated);
  border-left-color: var(--accent);
}

.key {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  color: var(--text);
}

li.active .key {
  color: var(--accent-text);
}

.meta {
  display: flex;
  gap: 8px;
  align-items: baseline;
}

.state {
  font-size: 11px;
  color: var(--text3);
}

.state.idle {
  color: var(--text3);
}

.state.leased {
  color: var(--running);
}

.state.reacquire {
  color: var(--warn);
}

.inbox {
  font-size: 11px;
  color: var(--info);
  border: 1px solid var(--border-strong);
  border-radius: 8px;
  padding: 0 6px;
}
</style>
