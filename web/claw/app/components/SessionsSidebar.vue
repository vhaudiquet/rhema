<script setup lang="ts">
// The session inventory: every dispatch key the claw holds, interactive
// first. Clicking selects the session the chat mirrors. Displayed names
// drop the `interactive:` prefix — the full key stays in the tooltip.
import type { ClawSession } from '~/composables/useClaw'

const props = defineProps<{
  sessions: ClawSession[]
  activeKey: string
  loading: boolean
  identity?: { name: string | null; tagline: string | null; mark: string | null; initials: string | null } | null
  avatarUrl?: string | null
}>()

defineEmits<{ select: [key: string]; refresh: [] }>()

interface Group {
  label: string
  rows: { key: string; name: string; session: ClawSession }[]
}

const groups = computed<Group[]>(() => {
  const byKey = (a: ClawSession, b: ClawSession) => a.key.localeCompare(b.key)
  const interactive = props.sessions
    .filter((session) => session.key.startsWith('interactive:'))
    .sort(byKey)
  const others = props.sessions
    .filter((session) => !session.key.startsWith('interactive:'))
    .sort(byKey)
  const row = (session: ClawSession) => ({
    key: session.key,
    name: session.key.replace(/^interactive:/, ''),
    session,
  })
  const groups: Group[] = []
  if (interactive.length > 0)
    groups.push({ label: 'interactive', rows: interactive.map(row) })
  if (others.length > 0) groups.push({ label: 'other keys', rows: others.map(row) })
  return groups
})

function stateTone(state: string): 'ok' | 'warn' | 'idle' {
  if (state === 'leased') return 'ok'
  if (state === 'reacquire') return 'warn'
  return 'idle'
}
</script>

<template>
  <aside class="sidebar">
    <header class="agent">
      <AgentAvatar
        :avatar-url="avatarUrl"
        :initials="identity?.initials ?? identity?.name?.slice(0, 1)"
        :size="34"
      />
      <div class="agent-text">
        <strong>{{ identity?.name?.trim() || 'rhema' }}</strong>
        <span>{{ identity?.tagline?.trim() || 'the spoken word' }}</span>
      </div>
    </header>
    <header class="head">
      <span class="label">Sessions</span>
      <span class="count">{{ sessions.length }}</span>
      <button class="ghost" title="refresh sessions" :disabled="loading" @click="$emit('refresh')">
        ↻
      </button>
    </header>

    <p v-if="sessions.length === 0" class="empty">
      {{ loading ? 'loading…' : 'no sessions yet; send the first message' }}
    </p>

    <div v-for="group in groups" :key="group.label" class="group">
      <span class="label">{{ group.label }}</span>
      <button
        v-for="row in group.rows"
        :key="row.key"
        class="entry"
        :class="{ active: row.key === activeKey }"
        :title="row.key"
        @click="$emit('select', row.key)"
      >
        <StatusDot :tone="stateTone(row.session.state)" />
        <span class="name">{{ row.name }}</span>
        <span v-if="row.session.inbox_depth > 0" class="depth" title="inbox depth">
          {{ row.session.inbox_depth }}
        </span>
      </button>
    </div>
  </aside>
</template>

<style scoped>
.sidebar {
  display: flex;
  flex-direction: column;
  min-height: 0;
  width: 240px;
  flex: none;
  padding: 14px 10px;
  border-right: 1px solid var(--border);
  background: var(--surface);
  overflow-y: auto;
}

.agent {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 0 8px 12px;
  border-bottom: 1px solid var(--border);
  margin-bottom: 10px;
}

.agent-text {
  display: flex;
  flex-direction: column;
  min-width: 0;
}

.agent-text strong {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.agent-text span {
  color: var(--text3);
  font-size: 11.5px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.head {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 0 8px 10px;
}

.count {
  flex: 1;
  color: var(--text3);
  font-size: 12px;
}

.head .ghost {
  padding: 0 6px;
}

.empty {
  margin: 4px 8px;
  color: var(--text3);
}

.group {
  display: flex;
  flex-direction: column;
  gap: 2px;
  margin-bottom: 14px;
}

.group .label {
  padding: 0 8px 4px;
}

.entry {
  display: flex;
  align-items: center;
  gap: 8px;
  width: 100%;
  text-align: left;
  background: none;
  border: none;
  border-radius: 7px;
  padding: 7px 10px;
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

.name {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.depth {
  flex: none;
  min-width: 18px;
  text-align: center;
  font-size: 11px;
  font-family: var(--mono);
  color: var(--info);
  border: 1px solid var(--border-strong);
  border-radius: 999px;
  padding: 0 5px;
}
</style>
